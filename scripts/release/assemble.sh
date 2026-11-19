#!/usr/bin/env bash
# Assembles the release directory from the built assets (design/release-artifacts.md §4.1, §5.4,
# §6.4). Runs in the `assemble` job of .github/workflows/release-build.yml, so a pull request's
# build-only run proves it and release.yml's `publish` job (a later wave) only uploads the result.
#
#   scripts/release/assemble.sh --version V --commit SHA --channel release|dev \
#                               --assets DIR --out DIR [--repo OWNER/NAME]
#
#   --assets  the downloaded asset-* artifacts, flat; exactly `built_assets V` (lib.sh: the release
#             assets without install.sh) and nothing else: a missing or stray file fails here, not
#             at publish. scripts/release/install.sh is copied in from this checkout and scanned
#   --out     must not exist or be empty; receives the assets, SHA256SUMS, RELEASE_NOTES.md and
#             build-info.json (version, commit, channel, run id: publish checks it against the
#             commit `validate` approved; it is not a release asset)
#   --repo    default $REPO
# Order: asset check, scan.sh (a finding stops everything and nothing is written), SHA256SUMS
# (`sha256sum` format, sorted by name, every asset, not itself and not the notes), notes.sh,
# build-info.json, then the job summary ($GITHUB_STEP_SUMMARY): the notes, which hold the asset
# table with sizes and sha256.
# Env: GH_TOKEN and NOTES_API_FIXTURES (notes.sh), GITLEAKS (scan.sh), GITHUB_RUN_ID,
#      GITHUB_STEP_SUMMARY.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release/lib.sh
. "$here/lib.sh"
die() { echo "assemble: $*" >&2; exit 1; }

version=""; commit=""; channel=""; assets=""; out=""; repo_name="${REPO:-}"
while [ $# -gt 0 ]; do
  [ $# -ge 2 ] || die "$1 needs a value"
  case "$1" in
    --version) version="$2" ;; --commit) commit="$2" ;; --channel) channel="$2" ;;
    --assets) assets="$2" ;; --out) out="$2" ;; --repo) repo_name="$2" ;;
    *) die "unknown option $1" ;;
  esac
  shift 2
done
semver_valid "$version" || die "--version '$version' is not a release version"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || die "--commit '$commit' is not a full 40-hex SHA"
case "$channel" in release|dev) ;; *) die "--channel must be release or dev, got '$channel'" ;; esac
[ -d "$assets" ] || die "--assets: no such directory '$assets'"
[ -n "$out" ] || die "--out is required"
[ -n "$repo_name" ] || die "--repo (or REPO) is required"
command -v jq >/dev/null || die "jq is required"
if [ -e "$out" ] && [ -n "$(ls -A "$out" 2>/dev/null)" ]; then die "--out '$out' exists and is not empty"; fi

# --- the asset set ------------------------------------------------------------------------------------
installer="$here/install.sh"
[ -f "$installer" ] && [ -s "$installer" ] || die "$installer is missing or empty"
expected="$(release_assets "$version" | LC_ALL=C sort)"
built="$(built_assets "$version" | LC_ALL=C sort)"
have="$(ls -A "$assets" | LC_ALL=C sort)"
missing="$(comm -23 <(printf '%s\n' "$built") <(printf '%s\n' "$have"))"
stray="$(comm -13 <(printf '%s\n' "$built") <(printf '%s\n' "$have") | awk 'NF')"
[ -z "$missing" ] || die "missing asset(s) for $version: $(printf '%s' "$missing" | tr '\n' ' ')"
[ -z "$stray" ] || die "unexpected file(s) in $assets: $(printf '%s' "$stray" | tr '\n' ' ') (the release holds exactly the listed assets)"
while IFS= read -r a; do
  [ -f "$assets/$a" ] && [ -s "$assets/$a" ] || die "asset $a is not a non-empty regular file"
done <<< "$built"

# --- the scan (nothing is written when it finds something) ----------------------------------------------
# the built assets plus the script that will ship beside them
scan_dir="$(mktemp -d "${TMPDIR:-/tmp}/assemble-scan.XXXXXX")" || die "no temporary directory"
trap 'rm -rf "$scan_dir"' EXIT
while IFS= read -r a; do cp "$assets/$a" "$scan_dir/$a"; done <<< "$built"
cp "$installer" "$scan_dir/install.sh"
"$here/scan.sh" "$scan_dir"

# --- the release directory --------------------------------------------------------------------------------
mkdir -p "$out"
out="$(cd "$out" && pwd)"
while IFS= read -r a; do cp "$assets/$a" "$out/$a"; done <<< "$built"
install -m 755 "$installer" "$out/install.sh"
: > "$out/SHA256SUMS"
while IFS= read -r a; do printf '%s  %s\n' "$(sha256_of "$out/$a")" "$a" >> "$out/SHA256SUMS"; done <<< "$expected"
if command -v sha256sum >/dev/null; then (cd "$out" && sha256sum -c SHA256SUMS >/dev/null) || die "SHA256SUMS does not verify its own assets"; fi

"$here/notes.sh" --version "$version" --commit "$commit" --assets "$out" --repo "$repo_name" --out "$out/RELEASE_NOTES.md"

jq -S -n --arg version "$version" --arg commit "$commit" --arg channel "$channel" --arg run_id "${GITHUB_RUN_ID:-}" \
  --argjson assets "$(printf '%s\n' "$expected" | jq -R . | jq -sc .)" \
  '{version: $version, commit: $commit, channel: $channel, run_id: $run_id, assets: $assets}' > "$out/build-info.json"

# --- the summary ------------------------------------------------------------------------------------------
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    printf '### Release %s (%s build) at `%s`\n\n' "$version" "$channel" "$commit"
    printf 'Scan clean. `SHA256SUMS` lists %s assets.\n\n' "$(printf '%s\n' "$expected" | wc -l | tr -d ' ')"
    cat "$out/RELEASE_NOTES.md"
    printf '\n'
  } >> "$GITHUB_STEP_SUMMARY"
fi
echo "assemble: $out holds $(printf '%s\n' "$expected" | wc -l | tr -d ' ') assets, SHA256SUMS, RELEASE_NOTES.md and build-info.json"
