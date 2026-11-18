#!/usr/bin/env bash
# Prepares a release: sets the workspace version and refreshes the workspace entries in Cargo.lock.
#
#   scripts/release/prepare.sh [--no-fetch] <version>      (X.Y.Z or X.Y.Z-rc.N)
#
# Run from a clean checkout of `main`. It edits Cargo.toml and Cargo.lock only, commits nothing, and
# prints the commands that open the `release: prepare v<version>` PR. The merge of that PR is the
# commit that gets tagged (locked decision 2). Refuses, changing nothing:
#   - a malformed version, or one not greater than every `v*` tag (an rc sorts below its final);
#   - a dirty working tree;
#   - tags it cannot read (it fetches them from origin; --no-fetch uses the local tags only);
#   - a Cargo.lock change other than the workspace crates' own entries;
#   - a workspace member that does not report the new version (every crate inherits it).
set -euo pipefail

die() { echo "prepare: $*" >&2; exit 1; }

fetch=1
version=""
while [ $# -gt 0 ]; do
  case "$1" in
    --no-fetch) fetch=0 ;;
    -h|--help) sed -n '2,13p' "$0"; exit 0 ;;
    -*) die "unknown flag: $1" ;;
    *) [ -z "$version" ] || die "one version only"; version="$1" ;;
  esac
  shift
done
[ -n "$version" ] || die "usage: scripts/release/prepare.sh [--no-fetch] <version>"

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release/lib.sh
. "$here/lib.sh"

semver_valid "$version" || die "'$version' is not a release version (X.Y.Z or X.Y.Z-rc.N, no leading zeros)"

root="$(git rev-parse --show-toplevel 2>/dev/null)" || die "not inside a git checkout"
cd "$root"
[ -f Cargo.toml ] && [ -f Cargo.lock ] || die "no Cargo.toml and Cargo.lock at $root"
[ -z "$(git status --porcelain)" ] || die "the working tree is not clean; commit or stash first"

# Tags: refuse to guess from a partial list.
if [ "$fetch" = 1 ] && git remote get-url origin >/dev/null 2>&1; then
  git fetch --tags --quiet origin || die "cannot fetch tags from origin (use --no-fetch to compare with the local tags only)"
  tags="$(version_tags)" || die "cannot read the remote's tags"
else
  tags="$(REMOTE="" version_tags)"
fi
while IFS= read -r t; do
  [ -n "$t" ] || continue
  semver_valid "$t" || die "tag 'v$t' is not a release version; fix or remove it before releasing"
  semver_gt "$version" "$t" || die "$version is not greater than the existing tag v$t"
done <<< "$tags"

# --- edit ---------------------------------------------------------------------------------------
restore() { git checkout -q -- Cargo.toml Cargo.lock; }
trap 'rc=$?; if [ $rc -ne 0 ]; then restore; echo "prepare: Cargo.toml and Cargo.lock restored" >&2; fi' EXIT

current="$(workspace_version < Cargo.toml)"
[ -n "$current" ] || die "no version under [workspace.package] in Cargo.toml"
awk -v v="$version" '
  /^[[:space:]]*\[/ { s = $0; gsub(/[[:space:]]/, "", s); insec = (s == "[workspace.package]") }
  insec && !done && /^[[:space:]]*version[[:space:]]*=/ { print "version = \"" v "\""; done = 1; next }
  { print }
' Cargo.toml > Cargo.toml.new && mv Cargo.toml.new Cargo.toml
[ "$(workspace_version < Cargo.toml)" = "$version" ] || die "could not set the workspace version"

# lock_entries <Cargo.lock>: one `name version source` line per package; "-" when it has no source
# (workspace and path crates).
lock_entries() {
  awk '
    function flush() { if (name != "") print name, version, (source == "" ? "-" : source); name = ""; version = ""; source = "" }
    /^\[\[package\]\]/ { flush(); next }
    /^name = / { name = $3; gsub(/"/, "", name) }
    /^version = / { version = $3; gsub(/"/, "", version) }
    /^source = / { source = $3; gsub(/"/, "", source) }
    END { flush() }
  ' "$1" | sort
}
before="$(mktemp)"; after="$(mktemp)"
trap 'rc=$?; rm -f "$before" "$after"; if [ $rc -ne 0 ]; then restore; echo "prepare: Cargo.toml and Cargo.lock restored" >&2; fi' EXIT
lock_entries Cargo.lock > "$before"

cargo update --workspace --offline --quiet || die "cargo update --workspace --offline failed (is the registry cache warm?)"

lock_entries Cargo.lock > "$after"
removed="$(comm -23 "$before" "$after")"
added="$(comm -13 "$before" "$after")"
bad=0
while IFS= read -r line; do
  [ -n "$line" ] || continue
  read -r n v s <<< "$line"
  if [ "$s" != "-" ]; then echo "prepare: Cargo.lock lost registry entry $n $v" >&2; bad=1; fi
done <<< "$removed"
while IFS= read -r line; do
  [ -n "$line" ] || continue
  read -r n v s <<< "$line"
  if [ "$s" != "-" ]; then echo "prepare: Cargo.lock gained registry entry $n $v" >&2; bad=1
  elif [ "$v" != "$version" ]; then echo "prepare: Cargo.lock entry $n changed to $v, not $version" >&2; bad=1; fi
done <<< "$added"
[ "$(printf '%s\n' "$removed" | awk 'NF {print $1}' | sort | tr '\n' ' ')" = "$(printf '%s\n' "$added" | awk 'NF {print $1}' | sort | tr '\n' ' ')" ] \
  || { echo "prepare: the set of crates in Cargo.lock changed" >&2; bad=1; }
[ "$bad" = 0 ] || die "Cargo.lock changed beyond the workspace crates' own versions; not touching it"

# Every workspace member inherits the one version (locked decision 1).
off="$(cargo metadata --no-deps --offline --format-version 1 | jq -r --arg v "$version" '.packages[] | select(.version != $v) | "\(.name) \(.version)"')"
[ -z "$off" ] || die "workspace member(s) not at $version (they must use version.workspace = true): $(echo "$off" | tr '\n' ',')"

changed="$(git diff --name-only | tr '\n' ' ')"
echo "prepare: version $current -> $version; changed: ${changed:-nothing}"
git --no-pager diff --stat
trap - EXIT
rm -f "$before" "$after"

cat <<MSG

Open the prepare PR (only Cargo.toml and Cargo.lock; no other PR into main should be open):

  git switch -c release/prepare-v$version
  git add Cargo.toml Cargo.lock
  git commit -s -m "release: prepare v$version"
  git push -u origin release/prepare-v$version
  gh pr create --base main --title "release: prepare v$version" --body "Sets the workspace version to $version. Merge with a merge commit once no other PR into main is open; the merge is the commit that is tagged."
MSG
