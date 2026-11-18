#!/usr/bin/env bash
# Packs built binaries into the release assets of design §5.1.
#
#   scripts/release/package.sh --kind node|cli|runtime --target TRIPLE --version V --bins DIR --out DIR
#                              [--runtime PATH]
#
#   node     vam-node-<V>-<TRIPLE>.tar.gz   top directory vam-node-<V>-<TRIPLE>/ holding vamd, vam,
#            vam-web-broker, vam-schedule-broker, vam-notify-broker, vam-runtime (from --runtime),
#            scripts/assets.pins, LICENSE-APACHE, LICENSE-MIT, NOTICE, README.md and
#            share/vam/base/ (build-rootfs.sh, Dockerfile, pins, resolve-pins.sh, manifest.json)
#   cli      vam-<V>-<TRIPLE>.tar.gz        top directory vam-<V>-<TRIPLE>/ holding vam, the licences, NOTICE
#   runtime  vam-runtime-<V>-<TRIPLE>       the guest init alone (a copy of --runtime, mode 755)
#
# share/vam/base/manifest.json is {"name", "version", "files": {path relative to share/vam/base:
# sha256}}; vam node init reads exactly this (tracker Wave 4 ruling). Dockerfile and pins sit beside
# build-rootfs.sh, which looks for the recipe next to itself when there is no base/ subdirectory.
#
# Archives are reproducible for the same inputs: sorted names, mtime = SOURCE_DATE_EPOCH (default:
# the HEAD commit time), owner 0, numeric ids, `gzip -n`. Needs GNU tar or bsdtar, jq, sha256sum or
# shasum. The last stdout line is the asset path.
set -euo pipefail

die() { echo "package.sh: $*" >&2; exit 1; }
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo="${PACKAGE_REPO:-$(cd "$here/../.." && pwd)}"

kind=""; target=""; version=""; bins=""; out=""; runtime=""
while [ $# -gt 0 ]; do
  [ $# -ge 2 ] || die "$1 needs a value"
  case "$1" in
    --kind) kind="$2" ;; --target) target="$2" ;; --version) version="$2" ;;
    --bins) bins="$2" ;; --out) out="$2" ;; --runtime) runtime="$2" ;;
    *) die "unknown option $1" ;;
  esac
  shift 2
done
case "$kind" in node|cli|runtime) ;; *) die "--kind must be node, cli or runtime" ;; esac
[ -n "$target" ] && [ -n "$version" ] && [ -n "$out" ] || die "--target, --version and --out are required"
# shellcheck source=scripts/release/lib.sh
. "$here/lib.sh"
semver_valid "$version" || die "'$version' is not a release version"
command -v jq >/dev/null || die "jq is required"
mkdir -p "$out"; out="$(cd "$out" && pwd)"

sha256_of() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }
epoch="${SOURCE_DATE_EPOCH:-$(git -C "$repo" log -1 --format=%ct)}"
[[ "$epoch" =~ ^[0-9]+$ ]] || die "SOURCE_DATE_EPOCH '$epoch' is not a number"

# need_file <path>: a regular file that is not empty
need_file() { [ -f "$1" ] && [ -s "$1" ] || die "missing or empty: $1"; }

# make_tar <staging parent> <top directory> <archive>: deterministic tar.gz of one directory
make_tar() {
  local parent="$1" top="$2" archive="$3" tmp tarbin=tar
  command -v gtar >/dev/null && tarbin=gtar # GNU tar on macOS runners
  tmp="$(mktemp "${TMPDIR:-/tmp}/package-tar.XXXXXX")"
  if "$tarbin" --version 2>/dev/null | grep -q 'GNU tar'; then
    "$tarbin" -C "$parent" --sort=name --mtime="@$epoch" --owner=0 --group=0 --numeric-owner -cf "$tmp" "$top"
  else
    # bsdtar (macOS): no --sort; feed a sorted list. -n keeps tar from recursing a second time.
    (cd "$parent" && find "$top" -print | LC_ALL=C sort | tar -n -cf "$tmp" --uid 0 --gid 0 --numeric-owner -T -)
    touch -d "@$epoch" "$tmp" 2>/dev/null || true
  fi
  gzip -n -9 < "$tmp" > "$archive"
  rm -f "$tmp"
}

stage="$(mktemp -d "${TMPDIR:-/tmp}/package.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

case "$kind" in
  runtime)
    [ -n "$runtime" ] || die "--runtime PATH is required for --kind runtime"
    need_file "$runtime"
    asset="$out/vam-runtime-$version-$target"
    install -m 755 "$runtime" "$asset"
    printf '%s\n' "$asset"
    exit 0 ;;
  cli)
    [ -n "$bins" ] || die "--bins DIR is required"
    top="vam-$version-$target"; asset="$out/$top.tar.gz"
    mkdir -p "$stage/$top"
    need_file "$bins/vam"; install -m 755 "$bins/vam" "$stage/$top/vam"
    for f in LICENSE-APACHE LICENSE-MIT NOTICE; do need_file "$repo/$f"; install -m 644 "$repo/$f" "$stage/$top/$f"; done ;;
  node)
    [ -n "$bins" ] && [ -n "$runtime" ] || die "--bins DIR and --runtime PATH are required"
    top="vam-node-$version-$target"; asset="$out/$top.tar.gz"
    d="$stage/$top"; base="$d/share/vam/base"
    mkdir -p "$d/scripts" "$base"
    for b in vamd vam vam-web-broker vam-schedule-broker vam-notify-broker; do
      need_file "$bins/$b"; install -m 755 "$bins/$b" "$d/$b"
    done
    need_file "$runtime"; install -m 755 "$runtime" "$d/vam-runtime"
    for f in LICENSE-APACHE LICENSE-MIT NOTICE README.md; do need_file "$repo/$f"; install -m 644 "$repo/$f" "$d/$f"; done
    need_file "$repo/scripts/assets.pins"; install -m 644 "$repo/scripts/assets.pins" "$d/scripts/assets.pins"
    need_file "$repo/scripts/build-rootfs.sh"; install -m 755 "$repo/scripts/build-rootfs.sh" "$base/build-rootfs.sh"
    need_file "$repo/scripts/base/Dockerfile"; install -m 644 "$repo/scripts/base/Dockerfile" "$base/Dockerfile"
    need_file "$repo/scripts/base/pins"; install -m 644 "$repo/scripts/base/pins" "$base/pins"
    need_file "$repo/scripts/base/resolve-pins.sh"; install -m 755 "$repo/scripts/base/resolve-pins.sh" "$base/resolve-pins.sh"
    # build-rootfs.sh calls build-guest.sh only without --runtime; a kit always has the runtime.
    base_name="$(sed -n 's/^BASE_NAME=//p' "$repo/scripts/base/pins" | head -n 1 | tr -d "\"'")"
    [ -n "$base_name" ] || die "scripts/base/pins has no BASE_NAME"
    files_json="{}"
    for f in Dockerfile build-rootfs.sh pins resolve-pins.sh; do
      files_json="$(jq -c --arg k "$f" --arg v "$(sha256_of "$base/$f")" '. + {($k): $v}' <<< "$files_json")"
    done
    jq -S -n --arg name "$base_name" --arg version "$version" --argjson files "$files_json" \
      '{name: $name, version: $version, files: $files}' > "$base/manifest.json" ;;
esac

# mtimes inside the archive come from tar; normalise the staged tree too, so bsdtar agrees
find "$stage" -exec touch -d "@$epoch" {} + 2>/dev/null || true
make_tar "$stage" "$top" "$asset"
printf '%s\n' "$asset"
