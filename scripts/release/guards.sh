#!/usr/bin/env bash
# The release guards (design/release-artifacts.md §4.2, locked decision 5). Runs in the `validate`
# job of .github/workflows/release.yml, from a checkout of `main` with full history and tags.
# Needs bash, git and jq, plus `gh` and a token with `actions: read` and `contents: read` unless
# GUARDS_API_FIXTURES is set (tests).
#
# Inputs come from the environment only:
#   VERSION  X.Y.Z or X.Y.Z-rc.N
#   COMMIT   full 40-hex SHA to release; empty means the head of $MAIN_REF (printed)
#   DRY_RUN  true | false (every guard runs on a dry run too; nothing here depends on it)
#   REF      the ref the workflow was dispatched from (github.ref)
#   REPO     owner/name
# Test and CI knobs:
#   MAIN_REF            default origin/main
#   REMOTE              default origin (the remote whose tags are read)
#   GUARDS_API_FIXTURES a directory with rust-runs.json and releases.json instead of `gh api`
#   GUARDS_CARGO_DENY   the cargo-deny command, default `cargo deny`
#
# Every guard runs (no early abort) and each failure says why. Guard 8 (inputs reach shell only
# through `env:`) is enforced by scripts/release/check-workflow.sh. Exit 0 only if all guards pass.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release/lib.sh
. "$here/lib.sh"

MAIN_REF="${MAIN_REF:-origin/main}"
export REMOTE="${REMOTE-origin}"
VERSION="${VERSION:-}"; COMMIT="${COMMIT:-}"; DRY_RUN="${DRY_RUN:-}"; REF="${REF:-}"; REPO="${REPO:-}"

command -v git >/dev/null && command -v jq >/dev/null || { echo "guards: git and jq are required" >&2; exit 2; }

rows=()      # "N|name|status|detail" for the summary table
failures=0

# record <n> <name> <pass|FAIL|skip> <detail>
record() {
  rows+=("$1|$2|$3|$4")
  case "$3" in
    FAIL)
      failures=$((failures + 1))
      printf 'guard %s (%s): %s\n' "$1" "$2" "$4" >&2
      [ -n "${GITHUB_ACTIONS:-}" ] && printf '::error title=guard %s (%s)::%s\n' "$1" "$2" "$(printf '%s' "$4" | head -n 1)"
      ;;
    pass) printf 'guard %s (%s): ok, %s\n' "$1" "$2" "$4" ;;
    skip) printf 'guard %s (%s): skipped, %s\n' "$1" "$2" "$4" ;;
  esac
  return 0
}

# api <fixture-name> <path>: JSON on stdout; fails (closed) when it cannot be read.
api() {
  if [ -n "${GUARDS_API_FIXTURES:-}" ]; then
    [ -f "$GUARDS_API_FIXTURES/$1.json" ] || { echo "no fixture $1.json" >&2; return 1; }
    cat "$GUARDS_API_FIXTURES/$1.json"
  else
    command -v gh >/dev/null || { echo "gh is not installed" >&2; return 1; }
    gh api -H 'Accept: application/vnd.github+json' --paginate "$2"
  fi
}

# --- inputs --------------------------------------------------------------------------------------
inputs_ok=1
version_ok=1
commit_ok=1
if ! semver_valid "$VERSION"; then
  record 0 "input version" FAIL "VERSION '$VERSION' is not X.Y.Z or X.Y.Z-rc.N (no leading zeros, no leading v)"
  version_ok=0
fi
case "$DRY_RUN" in true|false) ;; *) record 0 "input dry_run" FAIL "DRY_RUN '$DRY_RUN' is not true or false"; inputs_ok=0 ;; esac
[[ "$REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || { record 0 "input repo" FAIL "REPO '$REPO' is not owner/name"; inputs_ok=0; }
if ! git rev-parse -q --verify "$MAIN_REF^{commit}" >/dev/null; then
  record 0 "input main" FAIL "$MAIN_REF does not exist in this checkout (fetch full history: actions/checkout with fetch-depth 0)"
  commit_ok=0
elif [ -z "$COMMIT" ]; then
  COMMIT="$(git rev-parse "$MAIN_REF^{commit}")"
  echo "guards: commit not given; using the head of $MAIN_REF: $COMMIT"
elif [[ ! "$COMMIT" =~ ^[0-9a-f]{40}$ ]]; then
  record 0 "input commit" FAIL "COMMIT '$COMMIT' is not a full 40-character lowercase hex SHA"
  commit_ok=0
fi
[ "$inputs_ok" = 1 ] || commit_ok=0   # nothing below can run without a repo and a mode

# --- 1: the dispatch ref ---------------------------------------------------------------------------
guard_1() {
  if [ "$REF" = refs/heads/main ]; then record 1 "ref" pass "dispatched from refs/heads/main"
  else record 1 "ref" FAIL "dispatched from '${REF:-<empty>}'; releases run only from refs/heads/main so the workflow file that runs is the reviewed one (Run workflow, Use workflow from: main)"; fi
}

# --- 2: first-parent history of main -----------------------------------------------------------------
guard_2() {
  if ! git cat-file -e "$COMMIT^{commit}" 2>/dev/null; then
    record 2 "first-parent" FAIL "$COMMIT is not a commit in this checkout (wrong SHA, or not fetched)"; return
  fi
  if ! git merge-base --is-ancestor "$COMMIT" "$MAIN_REF"; then
    record 2 "first-parent" FAIL "$COMMIT is not on $MAIN_REF (a commit from a feature branch or another repository)"; return
  fi
  # Into a variable first: `grep -q` closing the pipe early would fail the pipeline under pipefail.
  local first_parent
  first_parent="$(git rev-list --first-parent "$MAIN_REF")" || { record 2 "first-parent" FAIL "cannot list the history of $MAIN_REF"; return; }
  if ! grep -qxF "$COMMIT" <<< "$first_parent"; then
    record 2 "first-parent" FAIL "$COMMIT reaches $MAIN_REF only through a side branch; release a merge into main (its first-parent history), not a commit from inside a feature branch"; return
  fi
  record 2 "first-parent" pass "$COMMIT is on the first-parent history of $MAIN_REF"
}

# --- 3: CI green on that exact SHA ---------------------------------------------------------------------
guard_3() {
  local runs summary good
  if ! runs="$(api rust-runs "repos/$REPO/actions/workflows/rust.yml/runs?head_sha=$COMMIT&per_page=100" 2>&1)"; then
    record 3 "ci" FAIL "cannot read the rust.yml runs for $COMMIT from the Actions API (needs actions: read): $(printf '%s' "$runs" | head -n 3 | tr '\n' ' ')"; return
  fi
  # Runs of rust.yml on main at exactly this SHA: the push run, or a workflow_dispatch run.
  runs="$(printf '%s' "$runs" | jq -cs --arg c "$COMMIT" '[ .[] | (.workflow_runs // [])[]
      | select(.head_sha == $c and .head_branch == "main" and (.event == "push" or .event == "workflow_dispatch")) ]' 2>&1)" \
    || { record 3 "ci" FAIL "the Actions API answer is not the expected JSON: $runs"; return; }
  good="$(printf '%s' "$runs" | jq -r '[ .[] | select(.status == "completed" and .conclusion == "success") ][0] | if . then .html_url else empty end')"
  if [ -n "$good" ]; then record 3 "ci" pass "rust.yml succeeded on $COMMIT: $good"; return; fi
  summary="$(printf '%s' "$runs" | jq -r '.[] | "  \(.html_url) (event \(.event), status \(.status), conclusion \(.conclusion // "none"))"')"
  if [ -n "$summary" ]; then
    record 3 "ci" FAIL "no successful rust.yml run on main at $COMMIT; the runs are not green:
$summary
Re-run rust.yml for that SHA (never the release) and dispatch again once it is green."
  else
    record 3 "ci" FAIL "no rust.yml run on main at $COMMIT (a docs-only merge has no push run, and a cancelled or superseded run does not count). Run rust.yml for it: gh workflow run rust.yml --ref main (only possible while $COMMIT is the head of main), then dispatch again once it is green."
  fi
}

# --- 4: Cargo.toml version ---------------------------------------------------------------------------------
guard_4() {
  local have
  if ! have="$(git show "$COMMIT:Cargo.toml" 2>/dev/null | workspace_version)" || [ -z "$have" ]; then
    record 4 "cargo-version" FAIL "cannot read [workspace.package] version from Cargo.toml at $COMMIT"; return
  fi
  if [ "$have" = "$VERSION" ]; then record 4 "cargo-version" pass "Cargo.toml at $COMMIT has version $have"
  else record 4 "cargo-version" FAIL "Cargo.toml at $COMMIT has version '$have' but the input is '$VERSION'; merge the prepare PR (scripts/release/prepare.sh $VERSION) first"; fi
}

# --- 5: tag and release absent ------------------------------------------------------------------------------
# Published releases are checked here. A draft release is invisible to this job's read-only token;
# the publish job (contents: write) checks for drafts before it creates its own.
guard_5() {
  local tag="v$VERSION" releases found
  if git rev-parse -q --verify "refs/tags/$tag" >/dev/null; then
    record 5 "tag-absent" FAIL "tag $tag already exists locally; an immutable release burns its tag name, so a failed publish takes the next number"; return
  fi
  if [ -n "$REMOTE" ] && git remote get-url "$REMOTE" >/dev/null 2>&1; then
    if ! found="$(git ls-remote --tags "$REMOTE" "refs/tags/$tag")"; then
      record 5 "tag-absent" FAIL "cannot read the tags of remote '$REMOTE'"; return
    fi
    if [ -n "$found" ]; then record 5 "tag-absent" FAIL "tag $tag already exists on remote '$REMOTE'; take the next number"; return; fi
  fi
  if ! releases="$(api releases "repos/$REPO/releases?per_page=100" 2>&1)"; then
    record 5 "tag-absent" FAIL "cannot read the releases from the API: $(printf '%s' "$releases" | head -n 3 | tr '\n' ' ')"; return
  fi
  if ! found="$(printf '%s' "$releases" | jq -rs --arg t "$tag" 'add // [] | map(select(.tag_name == $t)) | .[0] | if . then .html_url // .tag_name else empty end' 2>&1)"; then
    record 5 "tag-absent" FAIL "the releases answer is not the expected JSON: $found"; return
  fi
  if [ -n "$found" ]; then record 5 "tag-absent" FAIL "a release for $tag already exists ($found); take the next number"; return; fi
  record 5 "tag-absent" pass "no tag or published release $tag"
}

# --- 6: version greater than every v* tag ----------------------------------------------------------------------
guard_6() {
  local tags t bad=""
  if ! tags="$(version_tags)"; then record 6 "version-order" FAIL "cannot read the existing tags"; return; fi
  while IFS= read -r t; do
    [ -n "$t" ] || continue
    if ! semver_valid "$t"; then bad+="tag 'v$t' is not a release version; "; continue; fi
    semver_gt "$VERSION" "$t" || bad+="$VERSION is not greater than the existing tag v$t; "
  done <<< "$tags"
  if [ -z "$bad" ]; then record 6 "version-order" pass "$VERSION is greater than every v* tag ($(printf '%s\n' "$tags" | awk 'NF' | wc -l | tr -d ' ') tags)"
  else record 6 "version-order" FAIL "${bad%; }"; fi
}

# --- 7: no known advisory at the commit ---------------------------------------------------------------------------
guard_7() {
  local wt out rc deny
  wt="$(mktemp -d "${TMPDIR:-/tmp}/guards-wt.XXXXXX")" || { record 7 "advisories" FAIL "cannot create a temporary directory"; return; }
  if ! git worktree add --detach -q "$wt/tree" "$COMMIT" 2>/dev/null; then
    rm -rf "$wt"; record 7 "advisories" FAIL "cannot check out $COMMIT to scan it"; return
  fi
  read -r -a deny <<< "${GUARDS_CARGO_DENY:-cargo deny}"
  out="$(cd "$wt/tree" && "${deny[@]}" check advisories 2>&1)"; rc=$?
  git worktree remove --force "$wt/tree" >/dev/null 2>&1; rm -rf "$wt"
  if [ "$rc" = 0 ]; then record 7 "advisories" pass "cargo deny check advisories is clean at $COMMIT"
  else record 7 "advisories" FAIL "cargo deny check advisories failed at $COMMIT (exit $rc):
$(printf '%s' "$out" | tail -n 40)"; fi
}

guard_1
if [ "$commit_ok" = 1 ]; then guard_2; else record 2 "first-parent" skip "no valid commit"; fi
if [ "$commit_ok" = 1 ]; then guard_3; else record 3 "ci" skip "no valid commit"; fi
if [ "$commit_ok" = 1 ] && [ "$version_ok" = 1 ]; then guard_4; else record 4 "cargo-version" skip "no valid commit or version"; fi
if [ "$version_ok" = 1 ] && [ "$inputs_ok" = 1 ]; then guard_5; else record 5 "tag-absent" skip "no valid version"; fi
if [ "$version_ok" = 1 ] && [ "$inputs_ok" = 1 ]; then guard_6; else record 6 "version-order" skip "no valid version"; fi
if [ "$commit_ok" = 1 ] && git cat-file -e "$COMMIT^{commit}" 2>/dev/null; then guard_7; else record 7 "advisories" skip "no valid commit in this checkout"; fi

# --- outputs ------------------------------------------------------------------------------------------------------------
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    printf '### Release guards for `%s` at `%s` (dry run: %s)\n\n| # | Guard | Result | Detail |\n|---|---|---|---|\n' "$VERSION" "${COMMIT:-?}" "${DRY_RUN:-?}"
    for r in "${rows[@]}"; do
      IFS='|' read -r n name status detail <<< "$r"
      printf '| %s | %s | %s | %s |\n' "$n" "$name" "$status" "$(printf '%s' "$detail" | head -n 1 | sed 's/|/\\|/g')"
    done
  } >> "$GITHUB_STEP_SUMMARY"
fi
if [ -n "${GITHUB_OUTPUT:-}" ] && [ "$commit_ok" = 1 ]; then printf 'commit=%s\nversion=%s\n' "$COMMIT" "$VERSION" >> "$GITHUB_OUTPUT"; fi

if [ "$failures" -ne 0 ]; then echo "guards: $failures guard(s) failed; nothing is released" >&2; exit 1; fi
echo "guards: all passed for $VERSION at $COMMIT"
