#!/usr/bin/env bash
# guards.sh against a fixture repository (real git, a local bare "remote") and fixture API answers.
# No network, no gh, no cargo-deny: GUARDS_API_FIXTURES and GUARDS_CARGO_DENY stand in for them.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"

guards="$release_dir/guards.sh"
base="$t_tmp/base"

# --- the base fixture, built once --------------------------------------------------------------------
#   C1 (0.1.0, tag v0.1.0-rc.1) ── M1 (merge of feat/x, 0.1.0) ── M2 (merge of release/prepare, 0.2.0-rc.1) = main
#                  └─ S (feat/x only: a side commit, reachable from main only through M1's second parent)
build_base() {
  mkdir -p "$base" && cd "$base" || exit 1
  git_quiet init -q --bare remote.git
  git_quiet clone -q remote.git work 2>/dev/null
  cd work || exit 1
  git_quiet checkout -q -b main
  printf '[workspace.package]\nversion = "0.1.0"\nedition = "2021"\n' > Cargo.toml
  git_quiet add -A && git_quiet commit -q -m "C1"
  git_quiet tag v0.1.0-rc.1
  git_quiet checkout -q -b feat/x && echo side > side.txt && git_quiet add -A && git_quiet commit -q -m "S"
  S="$(git rev-parse HEAD)"
  git_quiet checkout -q main && git_quiet merge -q --no-ff feat/x -m "M1: merge feat/x"
  M1="$(git rev-parse HEAD)"
  git_quiet checkout -q -b release/prepare
  sed -i 's/^version = .*/version = "0.2.0-rc.1"/' Cargo.toml && git_quiet commit -qam "release: prepare v0.2.0-rc.1"
  git_quiet checkout -q main && git_quiet merge -q --no-ff release/prepare -m "M2: release: prepare v0.2.0-rc.1 (#1)"
  M2="$(git rev-parse HEAD)"
  git_quiet push -q origin main feat/x release/prepare v0.1.0-rc.1 2>/dev/null
  git_quiet fetch -q origin
  printf '%s %s %s\n' "$S" "$M1" "$M2" > "$base/shas"
  cd "$base" || exit 1
  cat > deny-stub <<'STUB'
#!/usr/bin/env bash
# Stands in for `cargo deny`: logs where it ran and answers per STUB_DENY_RC / STUB_DENY_OUT.
echo "ran in $(git rev-parse HEAD) with: $*" >> "${STUB_LOG:-/dev/null}"
[ -n "${STUB_DENY_OUT:-}" ] && echo "$STUB_DENY_OUT"
exit "${STUB_DENY_RC:-0}"
STUB
  chmod +x deny-stub
}
build_base
read -r S M1 M2 < "$base/shas"

# --- per-case helpers -------------------------------------------------------------------------------
case_dir=""
fixtures=""
new_case() {
  rm -rf "$t_tmp/case"; mkdir -p "$t_tmp/case"
  cp -a "$base/remote.git" "$base/work" "$t_tmp/case/"
  ( cd "$t_tmp/case/work" && git remote set-url origin "$t_tmp/case/remote.git" )
  case_dir="$t_tmp/case"; fixtures="$case_dir/api"; mkdir -p "$fixtures"
  : > "$case_dir/deny.log"
  runs_json "$M2" push main completed success > "$fixtures/rust-runs.json"
  echo '[]' > "$fixtures/releases.json"
}
# runs_json <sha> <event> <branch> <status> <conclusion>
runs_json() {
  jq -n --arg s "$1" --arg e "$2" --arg b "$3" --arg st "$4" --arg c "$5" \
    '{workflow_runs: [{head_sha: $s, event: $e, head_branch: $b, status: $st, conclusion: (if $c == "null" then null else $c end), html_url: ("https://example.invalid/runs/" + $e + "-" + $c)}]}'
}
# guards [VAR=value ...]: runs guards.sh in the case checkout with good defaults, overrides last.
guards_run() {
  ( cd "$case_dir/work" && env VERSION=0.2.0-rc.1 COMMIT="$M2" DRY_RUN=false REF=refs/heads/main REPO=squrell-inc/VAM \
      GUARDS_API_FIXTURES="$fixtures" GUARDS_CARGO_DENY="$base/deny-stub" STUB_LOG="$case_dir/deny.log" "$@" "$guards" ) 2>&1
}
# failed_guards <output>: the numbers of the guards that failed, space separated
failed_guards() {
  printf '%s\n' "$1" | grep -E '^guard [0-9]+ \(' | grep -vE '\): (ok|skipped),' | sed -E 's/^guard ([0-9]+) .*/\1/' | sort -un | tr '\n' ' ' | sed 's/ $//'
}
# case_fails <name> <expected failing guards> <reason regex> [VAR=value ...]
case_fails() {
  local name="$1" want="$2" pat="$3"; shift 3
  T_OUT="$(guards_run "$@")"; T_RC=$?
  local got; got="$(failed_guards "$T_OUT")"
  if [ "$T_RC" = 0 ]; then bad "$name (expected a refusal, got exit 0)" "$T_OUT"
  elif [ "$got" != "$want" ]; then bad "$name (failing guards: '$got', expected '$want')" "$T_OUT"
  elif ! grep -Eq -- "$pat" <<< "$T_OUT"; then bad "$name (no /$pat/ in output)" "$T_OUT"
  else ok "$name"; fi
}
case_passes() {
  local name="$1"; shift
  T_OUT="$(guards_run "$@")"; T_RC=$?
  if [ "$T_RC" = 0 ] && [ -z "$(failed_guards "$T_OUT")" ]; then ok "$name"; else bad "$name (exit $T_RC)" "$T_OUT"; fi
}

SECONDS=0
echo "good inputs"
new_case; case_passes "a good input passes"
expect_match "…and names the CI run" 'rust.yml succeeded on .*runs/push-success' "$T_OUT"
grep -q "ran in $M2 with: check advisories" "$case_dir/deny.log" && ok "…cargo deny ran in a checkout of the commit" || bad "cargo deny did not run at the commit" "$(cat "$case_dir/deny.log")"
new_case; runs_json "$M2" workflow_dispatch main completed success > "$fixtures/rust-runs.json"; case_passes "a workflow_dispatch run of rust.yml on main is accepted"
new_case; case_passes "an empty commit means the head of main" COMMIT=
new_case; case_passes "a dry run runs the same guards" DRY_RUN=true
new_case
jq -n --arg s "$M2" '{workflow_runs: [
  {head_sha:$s, event:"push", head_branch:"main", status:"completed", conclusion:"failure", html_url:"u1"},
  {head_sha:$s, event:"workflow_dispatch", head_branch:"main", status:"completed", conclusion:"success", html_url:"u2"}]}' > "$fixtures/rust-runs.json"
case_passes "a red push run followed by a green dispatch run passes"
new_case; git_quiet -C "$case_dir/work" tag v0.1.9 "$M1"; case_passes "a lower existing tag is fine"
# summary and outputs
new_case
sum="$case_dir/summary.md"; out="$case_dir/output.txt"
guards_run GITHUB_STEP_SUMMARY="$sum" GITHUB_OUTPUT="$out" >/dev/null
expect_match "the summary table lists the guards" '\| 3 \| ci \| pass \|' "$(cat "$sum")"
expect_match "the output carries the resolved commit" "^commit=$M2\$" "$(cat "$out")"

echo "guard 0: inputs"
new_case; case_fails "a short commit" 0 "not a full 40-character" COMMIT="${M2:0:12}"
new_case; case_fails "an uppercase commit" 0 "not a full 40-character" COMMIT="$(printf '%s' "$M2" | tr a-f A-F)"
for v in 1.0 v0.2.0-rc.1 0.2.0-beta.1 0.2.0+dev 02.0.0 ""; do
  new_case; case_fails "malformed version '$v'" 0 "is not X.Y.Z or X.Y.Z-rc.N" VERSION="$v"
done
new_case; case_fails "a bad dry_run" 0 "not true or false" DRY_RUN=maybe
new_case; case_fails "a bad repo" 0 "not owner/name" REPO=nope

echo "guard 1: ref"
new_case; case_fails "dispatched from a feature branch" 1 "releases run only from refs/heads/main" REF=refs/heads/feat/x
new_case; case_fails "dispatched from a tag" 1 "releases run only from refs/heads/main" REF=refs/tags/v0.2.0-rc.1
new_case; case_fails "an empty ref" 1 "releases run only from refs/heads/main" REF=

echo "guard 2: first-parent history"
new_case; runs_json "$S" push main completed success > "$fixtures/rust-runs.json"
case_fails "a side-branch commit (on main only through a merge's second parent)" 2 "only through a side branch" COMMIT="$S" VERSION=0.1.0
new_case; case_fails "a commit that is not in the checkout" "2 3 4" "not a commit in this checkout" COMMIT=0123456789abcdef0123456789abcdef01234567
new_case
( cd "$case_dir/work" && git_quiet checkout -q --orphan other && git_quiet commit -q --allow-empty -m "unrelated" && git rev-parse HEAD > "$case_dir/unrelated" )
U="$(cat "$case_dir/unrelated")"; runs_json "$U" push main completed success > "$fixtures/rust-runs.json"
case_fails "a commit not on main at all" 2 "is not on origin/main" COMMIT="$U"

echo "guard 3: CI on that exact SHA"
new_case; echo '{"workflow_runs": []}' > "$fixtures/rust-runs.json"
case_fails "no rust.yml run" 3 "no rust.yml run on main at $M2.*gh workflow run rust.yml --ref main"
new_case; runs_json "$M2" push main completed failure > "$fixtures/rust-runs.json"
case_fails "a red run, named" 3 "runs/push-failure.*conclusion failure"
new_case; runs_json "$M2" push main completed cancelled > "$fixtures/rust-runs.json"
case_fails "a cancelled run" 3 "runs/push-cancelled.*conclusion cancelled"
new_case; runs_json "$M2" push main in_progress null > "$fixtures/rust-runs.json"
case_fails "a run still in progress" 3 "status in_progress"
new_case; runs_json "$M1" push main completed success > "$fixtures/rust-runs.json"
case_fails "a green run for another SHA does not count" 3 "no rust.yml run on main at $M2"
new_case; runs_json "$M2" push feat/x completed success > "$fixtures/rust-runs.json"
case_fails "a green run on another branch does not count" 3 "no rust.yml run on main at $M2"
new_case; runs_json "$M2" pull_request main completed success > "$fixtures/rust-runs.json"
case_fails "a green pull_request run does not count" 3 "no rust.yml run on main at $M2"
new_case; rm "$fixtures/rust-runs.json"
case_fails "an unreadable API fails closed" 3 "cannot read the rust.yml runs"
new_case; echo 'not json' > "$fixtures/rust-runs.json"
case_fails "a garbled API answer fails closed" 3 "not the expected JSON"

echo "guard 4: Cargo.toml version"
new_case; case_fails "Cargo.toml version is not the input" 4 "has version '0.2.0-rc.1' but the input is '0.2.0'.*prepare" VERSION=0.2.0
new_case; runs_json "$M1" push main completed success > "$fixtures/rust-runs.json"
case_fails "the commit predates the prepare merge" 4 "has version '0.1.0' but the input is '0.2.0-rc.1'" COMMIT="$M1"

echo "guard 5: tag and release absent"
new_case; git_quiet -C "$case_dir/work" tag v0.2.0-rc.1 "$M2"
case_fails "the tag exists locally" "5 6" "tag v0.2.0-rc.1 already exists locally"
new_case; git_quiet -C "$case_dir/remote.git" tag v0.2.0-rc.1 "$M2"
case_fails "the tag exists only on the remote" "5 6" "already exists on remote 'origin'"
new_case; echo '[{"tag_name":"v0.2.0-rc.1","draft":false,"html_url":"https://example.invalid/releases/1"}]' > "$fixtures/releases.json"
case_fails "a release uses the tag" 5 "a release for v0.2.0-rc.1 already exists"
new_case; echo '[{"tag_name":"v0.2.0-rc.1","draft":true,"html_url":"https://example.invalid/releases/2"}]' > "$fixtures/releases.json"
case_fails "a draft the API lists counts too" 5 "a release for v0.2.0-rc.1 already exists"
new_case; echo '[{"tag_name":"v0.1.0-rc.1"}]' > "$fixtures/releases.json"; case_passes "a release for another tag is fine"
new_case; rm "$fixtures/releases.json"
case_fails "an unreadable releases list fails closed" 5 "cannot read the releases"

echo "guard 6: version order"
new_case; git_quiet -C "$case_dir/work" tag v0.3.0 "$M1"
case_fails "lower than an existing tag" 6 "0.2.0-rc.1 is not greater than the existing tag v0.3.0"
new_case; git_quiet -C "$case_dir/work" tag v0.2.0 "$M1"
case_fails "an rc after its own final" 6 "0.2.0-rc.1 is not greater than the existing tag v0.2.0"
new_case; git_quiet -C "$case_dir/work" tag vnext "$M1"
case_fails "a stray non-semver v* tag" 6 "tag 'vnext' is not a release version"
new_case; git_quiet -C "$case_dir/remote.git" tag v0.9.0 "$M1"
case_fails "a higher tag that exists only on the remote" 6 "existing tag v0.9.0"

echo "guard 7: advisories"
new_case; case_fails "an advisory is reported" 7 "RUSTSEC-2099-0001" STUB_DENY_RC=1 STUB_DENY_OUT="error[vulnerability]: RUSTSEC-2099-0001 in example 1.0.0"
grep -q "ran in $M2" "$case_dir/deny.log" && ok "…scanned at the commit being released" || bad "advisory scan not at the commit" "$(cat "$case_dir/deny.log")"
new_case; case_fails "a missing cargo-deny fails closed" 7 "cargo deny check advisories failed" GUARDS_CARGO_DENY=/nonexistent/cargo-deny

echo "several at once"
new_case; case_fails "every failing guard is reported" "1 7" "releases run only from refs/heads/main" REF=refs/heads/feat/x STUB_DENY_RC=1

echo "tree left clean"
new_case
before="$(git -C "$case_dir/work" worktree list | wc -l)"
guards_run STUB_DENY_RC=1 >/dev/null
expect_eq "no worktree is left behind" "$before" "$(git -C "$case_dir/work" worktree list | wc -l)"

[ "$SECONDS" -le 10 ] && ok "the suite runs in ${SECONDS} s (limit 10)" || bad "the suite took ${SECONDS} s (limit 10)"
finish
