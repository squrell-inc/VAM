#!/usr/bin/env bash
# Parses the `CI:` line of a pull request body (rust.yml `changes` job). The body is untrusted:
# it arrives in the PR_BODY environment variable, is never evaluated, and only allowlisted values
# come out.
#
#   PR_BODY="$body" scripts/ci/parse-ci-line.sh
#
# The line starts at the beginning of a line, first one wins:
#   CI: scenarios=a,b tests=workspace lane=yes release-build=yes
#   CI: full
# Keys: scenarios (ids matching ^[a-z0-9-]+$ with a file in e2e/scenarios/, at most 20),
# tests (workspace), lane (yes|no), release-build (yes|no). Every key is optional.
# Prints five lines on stdout: full=, scenarios= (comma-separated), tests= (workspace or empty),
# lane= and release_build= (yes|no). Everything ignored is logged on stderr as `ci-line: ignored ...`.
# Env: SCENARIO_DIR overrides e2e/scenarios (tests).
set -euo pipefail

MAX_SCENARIOS=20
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
scenario_dir="${SCENARIO_DIR:-$repo/e2e/scenarios}"

# A value fit for a log line: printable ASCII only, at most 60 characters.
show() { printf '%s' "$1" | LC_ALL=C tr -cd '[:alnum:][:punct:] ' | cut -c1-60; }
ignored() { printf 'ci-line: ignored %s: %s\n' "$1" "$(show "$2")" >&2; }

full=no tests="" lane=no release_build=no
scenarios=()

line=""
n=0
while IFS= read -r l || [ -n "$l" ]; do
  l="${l%$'\r'}"
  case "$l" in
    CI:*)
      n=$((n + 1))
      if [ "$n" = 1 ]; then line="${l#CI:}"; else ignored "extra CI: line" "$l"; fi ;;
  esac
done <<< "${PR_BODY:-}"

# `read -a` splits on whitespace and never expands globs or substitutions.
read -r -a toks <<< "$line" || true
for tok in "${toks[@]+"${toks[@]}"}"; do
  if ! [[ "$tok" =~ ^[A-Za-z0-9=,_-]+$ ]]; then ignored "token (characters outside [A-Za-z0-9=,_-])" "$tok"; continue; fi
  if [ "$tok" = full ]; then full=yes; continue; fi
  key="${tok%%=*}"
  if [ "$key" = "$tok" ]; then ignored "token (not key=value)" "$tok"; continue; fi
  val="${tok#*=}"
  case "$key" in
    scenarios)
      IFS=, read -r -a ids <<< "$val" || true
      for id in "${ids[@]+"${ids[@]}"}"; do
        [ -n "$id" ] || continue
        if ! [[ "$id" =~ ^[a-z0-9-]+$ ]]; then ignored "scenario id (not ^[a-z0-9-]+\$)" "$id"; continue; fi
        if [ ! -f "$scenario_dir/$id.yaml" ]; then ignored "scenario id (no such scenario)" "$id"; continue; fi
        case " ${scenarios[*]-} " in *" $id "*) continue ;; esac
        if [ "${#scenarios[@]}" -ge "$MAX_SCENARIOS" ]; then ignored "scenario id (over $MAX_SCENARIOS)" "$id"; continue; fi
        scenarios+=("$id")
      done ;;
    tests)
      if [ "$val" = workspace ]; then tests=workspace; else ignored "tests value (only workspace)" "$val"; fi ;;
    lane|release-build)
      case "$val" in
        yes|no) if [ "$key" = lane ]; then lane="$val"; else release_build="$val"; fi ;;
        *) ignored "$key value (yes|no)" "$val" ;;
      esac ;;
    *) ignored "key" "$key" ;;
  esac
done

printf 'full=%s\nscenarios=%s\ntests=%s\nlane=%s\nrelease_build=%s\n' \
  "$full" "$(IFS=,; echo "${scenarios[*]-}")" "$tests" "$lane" "$release_build"
