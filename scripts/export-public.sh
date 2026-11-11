#!/usr/bin/env bash
# Exports a clean tree of this repository for the public repository: no git history, only
# tracked files, minus the exclusions below. Then scans the result for secrets and refuses to
# finish on any finding.
#
#   scripts/export-public.sh <commit> <target-dir>
#
#   <commit>      any commit-ish in this clone (for example origin/main)
#   <target-dir>  must not exist, or be an empty directory
#
# Needs gitleaks on PATH (or GITLEAKS=/path/to/gitleaks). It uses .gitleaks.toml from the
# exported tree, so the allowlist is reviewed with the code.
set -euo pipefail

usage() { sed -n '2,13p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
[ $# -eq 2 ] || usage
commit="$1"
target="$2"

repo="$(git -C "$(dirname "$0")/.." rev-parse --show-toplevel)"
gitleaks="${GITLEAKS:-gitleaks}"
command -v "$gitleaks" >/dev/null 2>&1 || {
  echo "error: gitleaks not found; install it or set GITLEAKS=/path/to/gitleaks" >&2
  exit 1
}

# Paths left out of the export. Each is a git pathspec relative to the repository root.
EXCLUDES=(
  # Maintainer tooling that polls the private repositories' pull requests.
  ".cursor/tools"
  # Local editor hooks and MCP config for `graft`, a maintainer-only tool: they run a binary
  # that public contributors do not have on every file read. The rules, skills and workstream
  # trackers under .cursor/ stay.
  ".cursor/hooks"
  ".cursor/hooks.json"
  ".cursor/mcp.json"
)
# Never tracked (see .gitignore), so `git archive` already leaves them out; the check below
# proves it: build output (target/, docs/dist/, docs/node_modules/), guest images (*.ext4,
# vmlinux*), Firecracker binaries, .env files, logs, graft caches, .claude/ settings.
MUST_BE_ABSENT=(
  ".git" "target" "docs/node_modules" "docs/dist" "rootfs.ext4" ".env" ".claude" "graft"
)

sha="$(git -C "$repo" rev-parse --verify "$commit^{commit}")"

if [ -e "$target" ] && [ -n "$(ls -A "$target" 2>/dev/null)" ]; then
  echo "error: $target exists and is not empty" >&2
  exit 1
fi
mkdir -p "$target"
target="$(cd "$target" && pwd)"

echo "exporting $sha to $target"
pathspec=(".")
for e in "${EXCLUDES[@]}"; do pathspec+=(":(exclude)$e"); done
git -C "$repo" archive --format=tar "$sha" -- "${pathspec[@]}" | tar -x -C "$target"

fail=0
for e in "${EXCLUDES[@]}" "${MUST_BE_ABSENT[@]}"; do
  if [ -e "$target/$e" ]; then
    echo "error: excluded path present in export: $e" >&2
    fail=1
  fi
done
# e2e/world/fixtures/ holds tiny stand-ins with these names (for example an empty rootfs.ext4
# for the node-init scenarios); they are tracked on purpose.
stray="$(find "$target" -path "$target/e2e/world/fixtures" -prune -o \
  \( -name '*.ext4' -o -name 'vmlinux*' -o -name '.env' -o -name '*.log' \) -print)"
if [ -n "$stray" ]; then
  echo "error: build or local files present in export:" >&2
  echo "$stray" >&2
  fail=1
fi
[ "$fail" -eq 0 ] || exit 1

echo "scanning the export for secrets (gitleaks)"
config="$target/.gitleaks.toml"
[ -f "$config" ] || { echo "error: $config missing from the export" >&2; exit 1; }
if ! "$gitleaks" dir "$target" --no-banner --redact -c "$config"; then
  echo "error: gitleaks reported findings; nothing may be published until they are triaged" >&2
  exit 1
fi

files="$(find "$target" -type f | wc -l | tr -d ' ')"
size="$(du -sh "$target" | cut -f1)"
echo "export of $sha: $files files, $size, in $target"
