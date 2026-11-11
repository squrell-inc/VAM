#!/usr/bin/env bash
# Resolves (or checks) the pins in scripts/base/pins against the registry and the Bun
# release host. Needs network: run it on a host that has it (the CI lane does).
#
#   scripts/base/resolve-pins.sh --check            verify what is pinned; change nothing
#   scripts/base/resolve-pins.sh --write            fill UNRESOLVED values (Bun), keep the rest
#   scripts/base/resolve-pins.sh --write --refresh  also re-read the Debian digest and
#                                                   set the snapshot timestamp to now (UTC)
#   BUN_VERSION=bun-v1.3.5 scripts/base/resolve-pins.sh --write   choose the Bun release
#
# Nothing here is a secret; every request is anonymous.
set -euo pipefail
cd "$(dirname "${BASH_SOURCE[0]}")/../.."

PINS=scripts/base/pins
mode=check refresh=0
for a in "$@"; do
  case "$a" in
    --check) mode=check ;;
    --write) mode=write ;;
    --refresh) refresh=1 ;;
    *) echo "usage: $0 [--check|--write] [--refresh]" >&2; exit 2 ;;
  esac
done

WANTED_BUN="${BUN_VERSION:-}"
# shellcheck disable=SC1090
. "$PINS"

registry_digest() {
  local tag="$1" token
  token=$(curl -fsSL "https://auth.docker.io/token?service=registry.docker.io&scope=repository:library/debian:pull" \
    | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
  curl -fsSI \
    -H "Authorization: Bearer $token" \
    -H "Accept: application/vnd.oci.image.index.v1+json, application/vnd.docker.distribution.manifest.list.v2+json" \
    "https://registry-1.docker.io/v2/library/debian/manifests/$tag" \
    | tr -d '\r' | awk 'tolower($1)=="docker-content-digest:" {print $2}'
}

set_pin() { # key value
  sed -i "s|^$1=.*|$1=$2|" "$PINS"
  echo "pinned $1=$2"
}

fail=0
if [ "$refresh" = 1 ] || [ "$mode" = check ]; then
  live=$(registry_digest "$DEBIAN_TAG" || true)
  if [ -z "$live" ]; then
    echo "WARN: could not read the registry digest for debian:$DEBIAN_TAG" >&2
  elif [ "$mode" = write ] && [ "$refresh" = 1 ]; then
    set_pin DEBIAN_DIGEST "$live"
  elif [ "$live" = "$DEBIAN_DIGEST" ]; then
    echo "ok: debian:$DEBIAN_TAG is $live"
  else
    echo "NOTE: pinned $DEBIAN_DIGEST, registry now says $live (a newer image; the pin still builds)" >&2
  fi
fi
if [ "$mode" = write ] && [ "$refresh" = 1 ]; then
  set_pin SNAPSHOT_TIMESTAMP "$(date -u +%Y%m%dT%H%M%SZ)"
fi

# Bun: the release tag and the sha256 of the glibc baseline zip from the release's SHASUMS256.txt.
case "$BUN_VERSION$BUN_SHA256" in
  *UNRESOLVED*)
    if [ "$mode" = check ]; then echo "UNRESOLVED: Bun pin" >&2; fail=1; else
      tag="$WANTED_BUN"
      if [ -z "$tag" ]; then
        tag=$(curl -fsSI -o /dev/null -w '%{redirect_url}' "https://github.com/oven-sh/bun/releases/latest" | sed 's|.*/tag/||')
      fi
      case "$tag" in bun-v*) ;; *) echo "cannot resolve a Bun release tag (got '$tag')" >&2; exit 1 ;; esac
      sums=$(curl -fsSL "https://github.com/oven-sh/bun/releases/download/$tag/SHASUMS256.txt")
      sha=$(printf '%s\n' "$sums" | awk '$2=="bun-linux-x64-baseline.zip" {print $1}')
      [ -n "$sha" ] || { echo "no bun-linux-x64-baseline.zip in SHASUMS256.txt for $tag" >&2; exit 1; }
      # Cross-check: download and hash it ourselves.
      tmp=$(mktemp -d)
      curl -fsSL -o "$tmp/bun.zip" "https://github.com/oven-sh/bun/releases/download/$tag/bun-linux-x64-baseline.zip"
      [ "$(sha256sum "$tmp/bun.zip" | cut -d' ' -f1)" = "$sha" ] || { echo "sha256 mismatch for $tag" >&2; rm -rf "$tmp"; exit 1; }
      rm -rf "$tmp"
      set_pin BUN_VERSION "$tag"
      set_pin BUN_SHA256 "$sha"
    fi ;;
  *) echo "ok: Bun $BUN_VERSION $BUN_SHA256" ;;
esac

exit "$fail"
