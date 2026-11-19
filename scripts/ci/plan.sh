#!/usr/bin/env bash
# Plans one rust.yml run: which tier, which tests, which scenarios, whether the microVM lane and the
# (ci-tiers probe: this line only exercises a CI: scenarios= line and is never merged.)
# release builds run. Called by the `changes` job; every input is an environment variable, so
# nothing from the event is interpolated into shell.
#
#   EVENT_NAME=pull_request BASE_REF=feat/x PR_BODY=... scripts/ci/plan.sh >> "$GITHUB_OUTPUT"
#
# Inputs: EVENT_NAME, BASE_REF, HEAD_REPO, BASE_REPO, PR_BODY, PR_LABELS (comma-separated),
# RELEASE (true for a release PR into main), CHANGED_FILES (a file of changed paths; default
# `git diff --name-only origin/$BASE_REF...HEAD`), PLAN_SUMMARY (a file the markdown plan is
# appended to, the job summary in CI).
# Outputs, one key=value per line:
#   tier=full|fast   tests_scope=workspace|affected|none   affected=<crates>
#   scenarios=smoke|<id,id>|(empty)   lane=true|false   vm=<same as lane>   release_build=true|false
#
# Tiers (owner, 2026-11-19; working-agreement.md, CI rule):
#   push, workflow_dispatch, PR into main, fork PR   full: workspace tests, smoke, the lane when VM
#                                                    paths change (always on a release PR)
#   PR into any other branch                         fast: affected-crate tests; the PR body's `CI:`
#                                                    line asks for more (parse-ci-line.sh)
# release_build is true only when rust.yml itself must call release-build.yml: a PR into main gets
# the release builds from release-build.yml's own pull_request trigger.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$here/../.."

event="${EVENT_NAME:-}"
base_ref="${BASE_REF:-}"
summary="${PLAN_SUMMARY:-/dev/null}"

changed_list() {
  if [ -n "${CHANGED_FILES:-}" ]; then cat "$CHANGED_FILES"; else git diff --name-only "origin/$base_ref...HEAD"; fi
}

# The microVM lane's path filter (unchanged from the old `changes` job).
vm_paths() {
  grep -qE '^(crates/(vam-core|vam-firecracker|vam-runtime|vam-orchestrator|vam-vsock|vam-storage|vam-e2e)/|scripts/e2e/|scripts/build-rootfs[^/]*\.sh$|scripts/release/build-guest\.sh$|scripts/base/|e2e/(scenarios|world)/|proto/|Cargo\.lock$|\.github/workflows/rust\.yml$)' <<< "$changed"
}
# release-build.yml's pull_request paths.
release_paths() {
  grep -qE '^(scripts/release/|\.github/workflows/release[^/]*\.yml$|scripts/build-rootfs\.sh$|scripts/base/|scripts/assets\.pins$|scripts/refused-names\.txt$|\.gitleaks\.toml$|Cargo\.lock$)' <<< "$changed"
}
# The full tier's lane decision for a pull request.
full_lane() {
  # A release PR always runs the lane: no path filter, no label (launch audit S4). The
  # `release lane (microVM)` job fails it unless the lane is green.
  if [ "${RELEASE:-false}" = "true" ]; then echo true; return; fi
  case ",${PR_LABELS:-}," in *,skip-vm-lane,*) echo false; return ;; esac
  if vm_paths; then echo true; else echo false; fi
}

tier=full tests_scope=workspace affected="" scenarios=smoke lane=true release_build=false
reason="" tests_reason="" ignored_log=""
if [ "$event" != pull_request ]; then
  reason="$event: always full"
else
  changed="$(changed_list)"
  if [ "$base_ref" = main ]; then
    reason="pull request into main: always full (a CI: line is ignored)"
    lane="$(full_lane)"
  elif [ "${HEAD_REPO:-}" != "${BASE_REPO:-}" ]; then
    reason="fork pull request: always full"
    lane="$(full_lane)"
    if release_paths; then release_build=true; fi
  else
    tier=fast
    ignored_log="$(mktemp)"
    parsed="$(PR_BODY="${PR_BODY:-}" "$here/parse-ci-line.sh" 2>"$ignored_log")"
    get() { sed -n "s/^$1=//p" <<< "$parsed"; }
    if [ "$(get full)" = yes ]; then
      tier=full reason="pull request into $base_ref with CI: full"
      lane="$(full_lane)"
      [ "$(get lane)" = yes ] && lane=true
      if release_paths || [ "$(get release_build)" = yes ]; then release_build=true; fi
    else
      reason="pull request into $base_ref: fast tier"
      scenarios="$(get scenarios)"
      lane=false; [ "$(get lane)" = yes ] && lane=true
      [ "$(get release_build)" = yes ] && release_build=true
      ac="$(printf '%s\n' "$changed" | scripts/ci/affected-crates.sh --files -)"
      tests_scope="$(sed -n 's/^scope=//p' <<< "$ac")"
      affected="$(sed -n 's/^crates=//p' <<< "$ac")"
      tests_reason="$(sed -n 's/^reason=//p' <<< "$ac")"
      if [ "$(get tests)" = workspace ] && [ "$tests_scope" != workspace ]; then
        tests_scope=workspace affected="" tests_reason="CI: tests=workspace"
      fi
    fi
  fi
fi

printf 'tier=%s\ntests_scope=%s\naffected=%s\nscenarios=%s\nlane=%s\nvm=%s\nrelease_build=%s\n' \
  "$tier" "$tests_scope" "$affected" "$scenarios" "$lane" "$lane" "$release_build"

{
  echo "### CI plan: $tier tier"
  echo
  echo "$reason"
  echo
  echo "| part | plan |"
  echo "|---|---|"
  echo "| checks, clippy | run |"
  case "$tests_scope" in
    workspace) echo "| cargo test | workspace${tests_reason:+ ($tests_reason)} |" ;;
    affected)  echo "| cargo test | $affected ($tests_reason) |" ;;
    *)         echo "| cargo test | skipped ($tests_reason) |" ;;
  esac
  case "$scenarios" in
    smoke) echo "| scenarios | smoke set |" ;;
    "")    echo "| scenarios | skipped (none named) |" ;;
    *)     echo "| scenarios | ${scenarios//,/, } |" ;;
  esac
  echo "| microVM lane | $([ "$lane" = true ] && echo run || echo skipped) |"
  echo "| release builds | $([ "$release_build" = true ] && echo "run (called from this workflow)" || echo "not from this workflow (release-build.yml runs itself on PRs into main that touch the release path)") |"
  if [ -n "$ignored_log" ] && [ -s "$ignored_log" ]; then
    echo
    echo "Ignored in the CI: line:"
    echo
    # Already printable ASCII (parse-ci-line.sh); drop what could break out of the list item.
    tr -d '`|<>[]' < "$ignored_log" | sed 's/^/- /'
  fi
} >> "$summary"
if [ -n "$ignored_log" ]; then cat "$ignored_log" >&2; rm -f "$ignored_log"; fi
