#!/usr/bin/env bash
# Scans the built release assets before they are assembled (design/release-artifacts.md §6.4,
# locked decision 10). Every archive is unpacked into its own temporary directory and every file
# in it (and each standalone asset) is checked; every check runs, each failure names its file, and
# the exit is non-zero on any failure.
#
#   scripts/release/scan.sh [--config PATH] [--names PATH] ASSETS_DIR
#
#   --config PATH  gitleaks config, default <repo>/.gitleaks.toml (reviewed with the code)
#   --names PATH   refused file names, default <repo>/scripts/refused-names.txt (the list
#                  scripts/export-public.sh uses too)
# Env: GITLEAKS  the gitleaks command (default `gitleaks`); tests point it at a stub.
#
# Checks:
#   1. archive hygiene: no absolute or `..` member names, and only files and directories (a
#      symlink, hardlink or device inside a release archive is refused, so unpacking cannot reach
#      outside the temporary directory).
#   2. refused names (.env, *.ext4, vmlinux*, *.log ...).
#   3. a minted API key: `vam_sk_dev_` or `vam_sk_` followed by 64 hex digits (what vamd mints,
#      keys.rs). The bare prefix is a literal in the binaries (key parsing, redaction, error
#      messages), so it is not a finding; a whole key is.
#   4. build machine paths: /home/runner, /Users/runner, /github/workspace, /github/home, /__w/,
#      /usr/local/cargo, /usr/local/rustup. The host binaries are built with the paths remapped
#      (build.sh checks each one too); this covers the whole archive. The guest init is built
#      exactly as build-rootfs.sh builds it, without remapping (decision 11), so the registry
#      sources cargo records for dependencies (/home/runner/.cargo/registry/..., a public crates.io
#      path) are tolerated in `vam-runtime` files and listed as a notice; any other build path in
#      them, such as the checkout, is a failure.
#   5. gitleaks dir over the unpacked tree with the repository's config (it skips binaries; checks
#      3 and 4 are what cover those).
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="$(cd "$here/../.." && pwd)"
config="$repo/.gitleaks.toml"
names="$repo/scripts/refused-names.txt"
gitleaks="${GITLEAKS:-gitleaks}"

die() { echo "scan: $*" >&2; exit 2; }
while [ $# -gt 0 ]; do
  case "$1" in
    --config) [ $# -ge 2 ] || die "--config needs a value"; config="$2"; shift 2 ;;
    --names) [ $# -ge 2 ] || die "--names needs a value"; names="$2"; shift 2 ;;
    -*) die "unknown option $1" ;;
    *) break ;;
  esac
done
[ $# -eq 1 ] || die "usage: scan.sh [--config PATH] [--names PATH] ASSETS_DIR"
assets="$1"
[ -d "$assets" ] || die "no such directory: $assets"
[ -f "$config" ] || die "no gitleaks config: $config"
[ -f "$names" ] || die "no refused-names file: $names"
tarbin=tar; command -v gtar >/dev/null && tarbin=gtar

failures=0
fail() { failures=$((failures + 1)); printf 'scan: FAIL %s\n' "$1" >&2; [ -n "${GITHUB_ACTIONS:-}" ] && printf '::error title=release scan::%s\n' "$1"; return 0; }
notice() { printf 'scan: note %s\n' "$1"; }

work="$(mktemp -d "${TMPDIR:-/tmp}/release-scan.XXXXXX")" || die "cannot create a temporary directory"
trap 'rm -rf "$work"' EXIT
tree="$work/tree"
mkdir -p "$tree"

# --- unpack (check 1) ---------------------------------------------------------------------------
n_assets=0
shopt -s nullglob
for f in "$assets"/*; do
  [ -f "$f" ] || continue
  n_assets=$((n_assets + 1))
  base="$(basename "$f")"
  dest="$tree/$base"
  mkdir -p "$dest"
  case "$base" in
    *.tar.gz|*.tgz)
      bad=0
      # member names
      if ! members="$("$tarbin" -tzf "$f" 2>&1)"; then fail "$base: not a readable tar.gz archive: $(printf '%s' "$members" | head -n 1)"; continue; fi
      while IFS= read -r m; do
        [ -n "$m" ] || continue
        case "/$m/" in */../*) fail "$base: member '$m' climbs out of the archive (..)"; bad=1 ;; esac
        case "$m" in /*) fail "$base: member '$m' has an absolute path"; bad=1 ;; esac
      done <<< "$members"
      # member types: the first character of the long listing (- regular, d directory)
      while IFS= read -r line; do
        [ -n "$line" ] || continue
        case "${line:0:1}" in -|d) ;; *) fail "$base: member is not a regular file or directory: ${line}"; bad=1 ;; esac
      done < <("$tarbin" -tvzf "$f" 2>/dev/null)
      [ "$bad" = 0 ] || continue
      "$tarbin" -xzf "$f" -C "$dest" --no-same-owner 2>/dev/null || { fail "$base: cannot be unpacked"; continue; }
      ;;
    *) cp "$f" "$dest/$base" ;;
  esac
done
shopt -u nullglob
[ "$n_assets" -gt 0 ] || { fail "no assets in $assets"; }
echo "scan: unpacked $n_assets asset(s)"

# --- 2: refused names ------------------------------------------------------------------------------
name_args=()
while IFS= read -r glob; do
  [ -n "$glob" ] || continue
  [ "${#name_args[@]}" -eq 0 ] || name_args+=(-o)
  name_args+=(-name "$glob")
done < <(sed -e 's/[[:space:]]*#.*$//' -e 's/^[[:space:]]*//' -e 's/[[:space:]]*$//' "$names" | awk 'NF')
[ "${#name_args[@]}" -gt 0 ] || die "$names lists no names"
while IFS= read -r p; do
  [ -n "$p" ] || continue
  fail "refused file name: ${p#"$tree"/} (a build or local file; see $(basename "$names"))"
done < <(find "$tree" -type f \( "${name_args[@]}" \) -print | LC_ALL=C sort)

# --- 3: a whole minted API key ------------------------------------------------------------------------
while IFS= read -r p; do
  [ -n "$p" ] || continue
  fail "a minted API key (vam_sk_ followed by 64 hex digits) in ${p#"$tree"/}"
done < <(grep -arlE 'vam_sk_(dev_)?[0-9a-f]{64}' "$tree" 2>/dev/null | LC_ALL=C sort)

# --- 4: build machine paths -----------------------------------------------------------------------------
path_re='(/home/runner|/Users/runner|/github/workspace|/github/home|/__w/|/usr/local/cargo|/usr/local/rustup)[^[:cntrl:] "]*'
# a runner's cargo registry and git sources: public crates.io paths, tolerated in the unremapped guest init only
registry_re='^/(home|Users)/runner/\.cargo/(registry|git)/'
# build-script output the guest includes (prost's generated vam.v1.rs via include!): a target/ path, not source
outdir_re='^/(home|Users)/runner/work/[^/]+/[^/]+/target/[^/]+/release/build/[^/]+/out/'
while IFS= read -r p; do
  [ -n "$p" ] || continue
  rel="${p#"$tree"/}"
  hits="$(grep -aoE "$path_re" "$p" 2>/dev/null | LC_ALL=C sort -u)"
  case "$(basename "$p")" in
    vam-runtime|vam-runtime-*)
      tolerated="$(printf '%s\n' "$hits" | grep -E "$registry_re" || true)"
      outdirs="$(printf '%s\n' "$hits" | grep -E "$outdir_re" || true)"
      hits="$(printf '%s\n' "$hits" | grep -vE "$registry_re" | grep -vE "$outdir_re" || true)"
      [ -z "$tolerated" ] || notice "$rel holds $(printf '%s\n' "$tolerated" | wc -l | tr -d ' ') cargo registry path(s), expected in the unremapped guest init (decision 11)"
      [ -z "$outdirs" ] || notice "$rel holds $(printf '%s\n' "$outdirs" | wc -l | tr -d ' ') build-script output path(s) under target/, expected in the unremapped guest init (decision 11)" ;;
  esac
  [ -n "$(printf '%s' "$hits" | tr -d '[:space:]')" ] || continue
  fail "a build machine path in $rel: $(printf '%s\n' "$hits" | head -n 3 | tr '\n' ' ')"
done < <(grep -arlE "$path_re" "$tree" 2>/dev/null | LC_ALL=C sort)

# --- 5: gitleaks ------------------------------------------------------------------------------------------
if command -v "$gitleaks" >/dev/null 2>&1; then
  if "$gitleaks" dir "$tree" --no-banner --redact -c "$config"; then
    echo "scan: gitleaks found nothing"
  else
    fail "gitleaks reported findings in the unpacked assets (see its output above)"
  fi
else
  fail "gitleaks is not installed (scripts/release/install-tool.sh gitleaks, or GITLEAKS=/path)"
fi

if [ "$failures" -ne 0 ]; then echo "scan: $failures finding(s); nothing is assembled" >&2; exit 1; fi
echo "scan: clean ($n_assets asset(s))"
