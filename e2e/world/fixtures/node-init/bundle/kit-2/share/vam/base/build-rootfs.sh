#!/bin/sh
# Fixture stub of the kit builder: needs no docker, no sudo. Writes only VAM_ROOTFS_OUT and the
# manifest beside it, which names the sha256 of the --runtime it was given.
set -eu
runtime=""
while [ "$#" -gt 0 ]; do
  case "$1" in
    --runtime) runtime="$2"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done
[ -n "$runtime" ] && [ -n "${VAM_ROOTFS_OUT:-}" ] || { echo "needs --runtime and VAM_ROOTFS_OUT" >&2; exit 2; }
sha=$(sha256sum "$runtime" | cut -d' ' -f1)
printf 'fixture image for %s\n' "$sha" > "$VAM_ROOTFS_OUT"
img=$(sha256sum "$VAM_ROOTFS_OUT" | cut -d' ' -f1)
printf '{\n  "name": "vam/base:2",\n  "version": "fixture",\n  "digest": "sha256:%s",\n  "runtime_sha256": "%s",\n  "runtimes": { "python": "3.13.5", "bun": "1.2.0" }\n}\n' "$img" "$sha" > "$(dirname "$VAM_ROOTFS_OUT")/rootfs.base.json"
