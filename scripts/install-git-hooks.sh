#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

chmod +x scripts/git-hooks/pre-commit
git config core.hooksPath scripts/git-hooks

echo "Installed git hooks (core.hooksPath=scripts/git-hooks)."
echo "Pre-commit runs: cargo fmt --all --check"
