#!/usr/bin/env bash
# Tiny assertion helpers for the release script tests (sourced; not itself a test).
# Each test file sets `set -uo pipefail`, sources this, runs cases, and ends with `finish`.

T_PASS=0
T_FAIL=0
t_tmp="$(mktemp -d "${TMPDIR:-/tmp}/release-test.XXXXXX")"
trap 'rm -rf "$t_tmp"' EXIT

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
release_dir="$repo_root/scripts/release"

ok() { T_PASS=$((T_PASS + 1)); printf '  ok   %s\n' "$1"; }
bad() { T_FAIL=$((T_FAIL + 1)); printf '  FAIL %s\n' "$1"; [ -n "${2:-}" ] && printf '%s\n' "$2" | sed 's/^/       | /'; return 0; }

# run_case <out-var-prefix> -- command...: runs a command, captures combined output and status in
# $T_OUT and $T_RC.
run_cmd() { T_OUT="$("$@" 2>&1)"; T_RC=$?; }

# expect_ok <name> <command...>
expect_ok() {
  local name="$1"; shift
  run_cmd "$@"
  if [ "$T_RC" = 0 ]; then ok "$name"; else bad "$name (exit $T_RC)" "$T_OUT"; fi
}

# expect_fail <name> <regex the output must match> <command...>
expect_fail() {
  local name="$1" pat="$2"; shift 2
  run_cmd "$@"
  if [ "$T_RC" = 0 ]; then bad "$name (expected a refusal, got exit 0)" "$T_OUT"
  elif grep -Eq -- "$pat" <<< "$T_OUT"; then ok "$name"
  else bad "$name (refused, but not with /$pat/)" "$T_OUT"; fi
}

# expect_match <name> <regex> <text>
expect_match() {
  if grep -Eq -- "$2" <<< "$3"; then ok "$1"; else bad "$1 (no /$2/ in output)" "$3"; fi
}
expect_nomatch() {
  if grep -Eq -- "$2" <<< "$3"; then bad "$1 (found /$2/)" "$3"; else ok "$1"; fi
}
expect_eq() {
  if [ "$2" = "$3" ]; then ok "$1"; else bad "$1" "expected: $2
actual:   $3"; fi
}

finish() {
  printf '%s: %d passed, %d failed\n' "$(basename "$0")" "$T_PASS" "$T_FAIL"
  [ "$T_FAIL" = 0 ]
}

# git_quiet: git with a fixed identity, no hooks, no signing, for fixture repositories.
git_quiet() {
  git -c user.name=Test -c user.email=test@example.invalid -c commit.gpgsign=false -c core.hooksPath=/dev/null \
    -c init.defaultBranch=main "$@"
}
