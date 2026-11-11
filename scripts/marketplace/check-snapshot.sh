#!/usr/bin/env bash
# The e2e fixture e2e/world/fixtures/marketplace-squrell/ is a vendored copy of the public
# squrell-inc/vam-marketplace repository. Its SOURCE file names the marketplace commit and tag it
# was copied from and the git tree hash of the copy (SOURCE itself excluded). This check fails when
# the fixture no longer hashes to that stamp, so the copy cannot drift from the marketplace by hand.
#
#   scripts/marketplace/check-snapshot.sh            check (runs in scripts/ci/local.sh --fast)
#   scripts/marketplace/check-snapshot.sh --print    print the tree hash of the fixture
#
# To update: change the marketplace first (PR, merge, tag), copy its tree over the fixture, then
# set `commit`, `tag` and `tree` in SOURCE (use --print for the tree hash).
set -euo pipefail

repo="$(cd "$(dirname "$0")/../.." && pwd)"
dir="$repo/e2e/world/fixtures/marketplace-squrell"
source_file="$dir/SOURCE"

# Git tree hash of the directory, computed in a throwaway repository so the surrounding
# checkout's ignore rules, attributes and index are never involved.
tree_hash() {
  local tmp
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' RETURN
  git init -q --bare "$tmp/git"
  GIT_DIR="$tmp/git" GIT_WORK_TREE="$dir" git \
    -c core.autocrlf=false -c core.safecrlf=false -c core.excludesFile=/dev/null -c core.fileMode=true \
    add -A -f -- . ':(exclude)SOURCE'
  GIT_DIR="$tmp/git" git write-tree
}

if [ "${1:-}" = "--print" ]; then
  tree_hash
  exit 0
fi

if [ ! -f "$source_file" ]; then
  echo "marketplace snapshot: $source_file is missing" >&2
  exit 1
fi

stamped="$(sed -n 's/^tree: *//p' "$source_file" | head -1)"
if [ -z "$stamped" ]; then
  echo "marketplace snapshot: SOURCE has no 'tree:' line" >&2
  exit 1
fi

actual="$(tree_hash)"
if [ "$stamped" != "$actual" ]; then
  commit="$(sed -n 's/^commit: *//p' "$source_file" | head -1)"
  cat >&2 <<EOF
marketplace snapshot: e2e/world/fixtures/marketplace-squrell has drifted from its stamp.
  stamped tree: $stamped (marketplace commit $commit)
  actual tree:  $actual
The fixture is a vendored copy of squrell-inc/vam-marketplace. Change the marketplace first,
copy it over the fixture, then update SOURCE (scripts/marketplace/check-snapshot.sh --print).
EOF
  exit 1
fi
echo "marketplace snapshot: fixture matches $(sed -n 's/^tag: *//p' "$source_file" | head -1) (tree $actual)"
