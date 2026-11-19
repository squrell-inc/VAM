#!/bin/sh
# Installs the `vam` CLI from a GitHub release of squrell-inc/VAM and docks it (`vam dock`).
#
#   install.sh [--version vX.Y.Z[-rc.N]] [--dir DIR] [--no-modify-path] [--from DIR] [--help]
#
# What it does, in order (each step is printed first):
#   1. picks the archive for this OS and CPU: Linux x86_64 or aarch64 (glibc), macOS arm64 or x86_64;
#      anything else is refused (Windows and musl are not supported yet)
#   2. picks the version: --version, else the latest final release (a release candidate only when
#      named with --version, as `vam upgrade` does)
#   3. downloads the archive and SHA256SUMS into a temporary directory and checks the archive's
#      sha256 against the line SHA256SUMS holds for it; a mismatch or a missing line stops here and
#      nothing is installed
#   4. runs `gh attestation verify` when `gh` is on the PATH (a refusal stops here); otherwise
#      prints how to check it
#   5. extracts the one `vam` binary, checks that it starts and names the version, and hands over
#      to `vam dock`, which copies it to a per-user directory and puts that on the PATH
#
# It never pipes anything to a shell, never uses sudo, never reads standard input and never edits
# a shell profile itself (`vam dock` does, and `vam undock` undoes it).
#
#   --version vX   the release to install (a leading v is optional): 0.1.0, v0.1.0-rc.2
#   --dir DIR      passed to `vam dock`: the directory to put vam in (default: dock's own)
#   --no-modify-path  passed to `vam dock`: print the PATH line instead of editing profiles
#   --from DIR     use the archive and SHA256SUMS in DIR instead of downloading (no network; the
#                  attestation is not checked); with no --version, the version is read from
#                  SHA256SUMS
#
# Environment: VAM_INSTALL_BASE_URL points the download at a local release server for tests; only
# file://, http://127.0.0.1 and http://localhost are accepted, anything else is refused.
# Compatible with dash and the bash 3.2 that macOS ships as /bin/sh.
set -eu

REPO="squrell-inc/VAM"
RELEASE_WORKFLOW=".github/workflows/release.yml"
SEMVER_RE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rc\.(0|[1-9][0-9]*))?$'
MAX_SUMS=1048576
MAX_ARCHIVE=209715200

say() { printf 'install: %s\n' "$*"; }
die() { printf 'install: %s\n' "$*" >&2; exit 1; }

usage() {
  cat <<'EOF'
usage: install.sh [--version vX.Y.Z[-rc.N]] [--dir DIR] [--no-modify-path] [--from DIR]

Installs the vam CLI from a GitHub release of squrell-inc/VAM and runs `vam dock`.
  --version vX      the release to install; default is the latest final release
                    (a release candidate needs --version v0.1.0-rc.N)
  --dir DIR         where `vam dock` puts vam
  --no-modify-path  print the PATH line instead of editing your profile files
  --from DIR        use the archive and SHA256SUMS found in DIR (no network)
EOF
}

version=""; version_given=0; dock_dir=""; no_modify=0; from=""
while [ $# -gt 0 ]; do
  case "$1" in
    -h|--help) usage; exit 0 ;;
    --no-modify-path) no_modify=1 ;;
    --version|--dir|--from)
      [ $# -ge 2 ] || die "$1 needs a value"
      case "$1" in --version) version="$2"; version_given=1 ;; --dir) dock_dir="$2" ;; --from) from="$2" ;; esac
      shift ;;
    *) usage >&2; die "unknown argument: $1" ;;
  esac
  shift
done

# --- the version, validated -------------------------------------------------------------------------
if [ "$version_given" = 1 ]; then
  version="${version#v}"
  printf '%s\n' "$version" | grep -Eq "$SEMVER_RE" \
    || die "--version must look like v0.1.0 or v0.1.0-rc.2, got '$version'"
fi

# --- the platform -----------------------------------------------------------------------------------
os_name="$(uname -s)"
cpu="$(uname -m)"
case "$os_name" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  MINGW*|MSYS*|CYGWIN*) die "Windows is not supported yet: vam.exe and install.ps1 come in v0.2.0" ;;
  *) die "unsupported operating system '$os_name'; releases ship vam for Linux and macOS" ;;
esac
case "$cpu" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) die "unsupported CPU '$cpu'; releases ship vam for x86_64 and aarch64 (arm64)" ;;
esac
if [ "$os" = darwin ] && [ "$arch" = x86_64 ]; then
  # a shell started under Rosetta reports x86_64 on an Apple silicon Mac
  if [ "$(sysctl -n hw.optional.arm64 2>/dev/null || true)" = 1 ]; then arch=aarch64; fi
fi
if [ "$os" = linux ]; then
  # the Linux binaries are built against glibc
  if ldd --version 2>&1 | grep -qi musl; then
    die "this system uses musl libc (Alpine); the Linux release binaries need glibc"
  fi
  triple="$arch-unknown-linux-gnu"
else
  if [ "$arch" = aarch64 ]; then triple="aarch64-apple-darwin"; else triple="x86_64-apple-darwin"; fi
fi
say "detected $os_name $cpu: the release asset target is $triple"

# --- tools ------------------------------------------------------------------------------------------
# fetch URL OUT MAX_BYTES: curl, or wget for https downloads when curl is missing.
fetch() {
  if command -v curl >/dev/null 2>&1; then
    if [ -n "${base_override:-}" ]; then
      curl -fsSL --max-filesize "$3" -o "$2" "$1" </dev/null
    else
      curl --proto '=https' --tlsv1.2 -fsSL --max-filesize "$3" -o "$2" "$1" </dev/null
    fi
  elif [ -z "${base_override:-}" ] && command -v wget >/dev/null 2>&1; then
    wget -q -O "$2" "$1" </dev/null
  else
    die "curl or wget is required to download"
  fi
}

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | awk '{print $1}'
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | awk '{print $1}'
  elif command -v openssl >/dev/null 2>&1; then openssl dgst -sha256 "$1" | awk '{print $NF}'
  else die "sha256sum, shasum or openssl is required to check the download"
  fi
}

# --- where the files come from ----------------------------------------------------------------------
base_override=""
if [ -n "${VAM_INSTALL_BASE_URL:-}" ]; then
  case "$VAM_INSTALL_BASE_URL" in
    file://*|http://127.0.0.1|http://127.0.0.1[:/]*|http://localhost|http://localhost[:/]*) ;;
    *) die "VAM_INSTALL_BASE_URL must be a file:// or loopback http:// URL, got '$VAM_INSTALL_BASE_URL'" ;;
  esac
  base_override="${VAM_INSTALL_BASE_URL%/}"
  command -v curl >/dev/null 2>&1 || die "VAM_INSTALL_BASE_URL needs curl"
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/vam-install.XXXXXX")" || die "cannot create a temporary directory"
cleanup() { rm -rf "$tmp"; }
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if [ -n "$from" ]; then
  [ -d "$from" ] || die "--from: no such directory '$from'"
  [ -f "$from/SHA256SUMS" ] || die "--from: $from has no SHA256SUMS"
  if [ -z "$version" ]; then
    # the one CLI archive this platform has in SHA256SUMS
    found="$(awk -v t="$triple" '
      NF == 2 { f = $2; sub(/^\*/, "", f)
        if (f ~ /^vam-.*\.tar\.gz$/ && f !~ /^vam-node-/ && index(f, "-" t ".tar.gz") > 0) print f }' "$from/SHA256SUMS")"
    [ -n "$found" ] || die "--from: SHA256SUMS lists no vam archive for $triple; pass --version"
    [ "$(printf '%s\n' "$found" | wc -l | tr -d ' ')" = 1 ] || die "--from: SHA256SUMS lists several vam archives for $triple; pass --version"
    version="${found#vam-}"; version="${version%-"$triple".tar.gz}"
    printf '%s\n' "$version" | grep -Eq "$SEMVER_RE" || die "--from: cannot read a version from '$found'; pass --version"
  fi
elif [ -z "$version" ]; then
  say "looking up the latest final release"
  if [ -n "$base_override" ]; then
    fetch "$base_override/latest" "$tmp/latest" 4096 || die "no final release yet; pass --version v0.1.0-rc.N to install a given release"
    tag="$(head -n 1 "$tmp/latest" | tr -d ' \r')"
  else
    latest_url="https://github.com/$REPO/releases/latest"
    if command -v curl >/dev/null 2>&1; then
      final_url="$(curl --proto '=https' --tlsv1.2 -sSL -o /dev/null -w '%{url_effective}' "$latest_url" </dev/null)" \
        || die "cannot reach github.com to find the latest release; pass --version vX"
    elif command -v wget >/dev/null 2>&1; then
      final_url="$(wget -q -S --spider --max-redirect=0 "$latest_url" </dev/null 2>&1 | awk 'tolower($1) == "location:" {print $2; exit}' | tr -d '\r')" || true
    else
      die "curl or wget is required to download"
    fi
    case "$final_url" in
      */releases/tag/*) tag="${final_url##*/releases/tag/}" ;;
      *) tag="" ;;
    esac
  fi
  [ -n "$tag" ] || die "no final release yet; pass --version v0.1.0-rc.N to install a given release"
  version="${tag#v}"
  printf '%s\n' "$version" | grep -Eq "$SEMVER_RE" || die "the latest release is tagged '$tag', which is not a release version; pass --version vX"
fi

tag="v$version"
archive="vam-$version-$triple.tar.gz"
top="vam-$version-$triple"
say "installing vam $version ($archive)"

# --- get the archive and SHA256SUMS ------------------------------------------------------------------
if [ -n "$from" ]; then
  say "using $from (no download)"
  [ -f "$from/$archive" ] || die "--from: $from has no $archive"
  cp "$from/$archive" "$tmp/$archive"
  cp "$from/SHA256SUMS" "$tmp/SHA256SUMS"
else
  if [ -n "$base_override" ]; then url_base="$base_override/$tag"; else url_base="https://github.com/$REPO/releases/download/$tag"; fi
  say "downloading SHA256SUMS from $url_base"
  fetch "$url_base/SHA256SUMS" "$tmp/SHA256SUMS" "$MAX_SUMS" || die "cannot download SHA256SUMS for $tag"
  say "downloading $archive"
  fetch "$url_base/$archive" "$tmp/$archive" "$MAX_ARCHIVE" || die "cannot download $archive for $tag"
fi

# --- the hash ---------------------------------------------------------------------------------------
say "checking the sha256 of $archive against SHA256SUMS"
# the hash SHA256SUMS lists for the archive: `<hash>  <name>` or `<hash> *<name>`, as vam upgrade reads it
listed="$(awk -v n="$archive" 'NF == 2 { f = $2; sub(/^\*/, "", f); if (f == n) { print tolower($1); exit } }' "$tmp/SHA256SUMS")"
[ -n "$listed" ] || die "SHA256SUMS does not list $archive; nothing was installed"
printf '%s\n' "$listed" | grep -Eq '^[0-9a-f]{64}$' || die "SHA256SUMS holds a malformed hash for $archive; nothing was installed"
got="$(sha256_of "$tmp/$archive")"
[ "$got" = "$listed" ] || die "hash mismatch for $archive: SHA256SUMS lists $listed, the download is $got; nothing was installed"
say "hash ok (sha256 $got)"

# --- the attestation --------------------------------------------------------------------------------
attest_args="attestation verify $archive -R $REPO --signer-workflow $REPO/$RELEASE_WORKFLOW"
if [ -n "$from" ]; then
  say "attestation not checked: --from uses local files that carry no attestation"
elif command -v gh >/dev/null 2>&1; then
  say "verifying the attestation with gh"
  if ! (cd "$tmp" && gh attestation verify "$archive" -R "$REPO" --signer-workflow "$REPO/$RELEASE_WORKFLOW" </dev/null); then
    die '`gh attestation verify` refused the download; nothing was installed. If gh is not signed in, run `gh auth login`.'
  fi
  say "attestation verified with gh"
else
  say "attestation not checked: gh is not on the PATH. To check it, install gh, download $archive from the release, and run: gh $attest_args"
fi

# --- extract ----------------------------------------------------------------------------------------
say "extracting vam"
listing="$tmp/listing"
tar -tzf "$tmp/$archive" > "$listing" 2>/dev/null || die "$archive is not a readable tar.gz archive; nothing was installed"
if grep -Eq '^/|(^|/)\.\.(/|$)' "$listing"; then
  die "$archive holds an absolute or parent-relative path; nothing was installed"
fi
grep -Fxq "$top/vam" "$listing" || die "$archive does not hold $top/vam; nothing was installed"
mkdir "$tmp/x"
tar -xzf "$tmp/$archive" -C "$tmp/x" "$top/vam" || die "cannot extract $top/vam; nothing was installed"
vam="$tmp/x/$top/vam"
{ [ -f "$vam" ] && [ ! -L "$vam" ]; } || die "$top/vam is not a regular file; nothing was installed"
chmod 755 "$vam"

say "checking that the new vam starts and reports $version"
line="$("$vam" --version </dev/null 2>/dev/null | head -n 1)" || line=""
line="$(printf '%s' "$line" | tr -d '\r')"
# A release build prints `vam X.Y.Z (commit date)`. With --from, a development build of that
# version (`vam X.Y.Z+dev.N ...`, what a pull request's build-only run makes) is accepted too.
dev_ok=""
[ -z "$from" ] || dev_ok="vam $version+dev"
case "$line" in
  "vam $version"|"vam $version "*|"vam $version	"*) ;;
  *)
    if [ -n "$dev_ok" ]; then
      case "$line" in "$dev_ok"|"$dev_ok "*|"$dev_ok".*) dev_ok=ok ;; *) dev_ok="" ;; esac
    fi
    [ "$dev_ok" = ok ] || die "the downloaded vam does not report vam $version (printed '$line'); nothing was installed"
    ;;
esac

# --- dock -------------------------------------------------------------------------------------------
set -- dock
[ -z "$dock_dir" ] || set -- "$@" --dir "$dock_dir"
[ "$no_modify" = 0 ] || set -- "$@" --no-modify-path
say "running vam $*"
rc=0
"$vam" "$@" </dev/null || rc=$?
[ "$rc" = 0 ] || die "vam dock failed (exit $rc)"
say "done: vam $version is installed; run 'vam undock' to remove it"
