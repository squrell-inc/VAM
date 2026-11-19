#!/usr/bin/env bash
# Tests the CI tier scripts: parse-ci-line.sh, affected-crates.sh and plan.sh. Offline, no cargo
# (a fixture `cargo metadata`), no network. Run by rust.yml's `checks` job.
#   scripts/ci/test/run.sh
set -uo pipefail
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ci="$here/.."
repo="$(cd "$ci/../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
pass=0 fail=0
ok() { pass=$((pass + 1)); echo "  ok   $1"; }
bad() { fail=$((fail + 1)); echo "  FAIL $1"; [ -z "${2:-}" ] || printf '       %s\n' "$2"; }
# has <name> <haystack> <needle>: haystack contains needle as a whole line
has() { if grep -qxF -- "$3" <<< "$2"; then ok "$1"; else bad "$1" "want line '$3' in: $(tr '\n' ';' <<< "$2")"; fi; }

# Scenario fixtures and a cargo metadata fixture shaped like the real workspace.
mkdir -p "$tmp/scen"
for id in alpha beta-two gamma; do : > "$tmp/scen/$id.yaml"; done
for i in $(seq 1 25); do : > "$tmp/scen/s$i.yaml"; done
export SCENARIO_DIR="$tmp/scen"
cat > "$tmp/meta.json" <<JSON
{"packages":[
 {"name":"vam-spec","manifest_path":"$repo/crates/vam-spec/Cargo.toml","dependencies":[]},
 {"name":"vam-security","manifest_path":"$repo/crates/vam-security/Cargo.toml","dependencies":[{"name":"vam-spec"}]},
 {"name":"vam-telemetry","manifest_path":"$repo/crates/vam-telemetry/Cargo.toml","dependencies":[{"name":"vam-spec"},{"name":"vam-security"},{"name":"serde"}]},
 {"name":"vam-storage","manifest_path":"$repo/crates/vam-storage/Cargo.toml","dependencies":[{"name":"vam-telemetry"}]},
 {"name":"vam-orchestrator","manifest_path":"$repo/crates/vam-orchestrator/Cargo.toml","dependencies":[{"name":"vam-storage"},{"name":"vam-web","kind":"dev"},{"name":"vam-orchestrator","kind":"dev"}]},
 {"name":"vam-web","manifest_path":"$repo/brokers/vam-web/Cargo.toml","dependencies":[{"name":"vam-spec"}]},
 {"name":"vam-firecracker","manifest_path":"$repo/crates/vam-firecracker/Cargo.toml","dependencies":[]}
]}
JSON
export AFFECTED_METADATA="$tmp/meta.json"

parse() { PR_BODY="$1" "$ci/parse-ci-line.sh" 2>"$tmp/err"; }

echo "parse-ci-line.sh"
out="$(parse "")"
has "empty body: fast defaults" "$out" "full=no"
has "empty body: no scenarios" "$out" "scenarios="
has "empty body: lane no" "$out" "lane=no"
out="$(parse $'## What\n\nCI: full\n')"
has "CI: full" "$out" "full=yes"
out="$(parse $'CI: scenarios=alpha,beta-two tests=workspace lane=yes release-build=yes\r\n')"
has "all keys: scenarios" "$out" "scenarios=alpha,beta-two"
has "all keys: tests" "$out" "tests=workspace"
has "all keys: lane" "$out" "lane=yes"
has "all keys: release-build" "$out" "release_build=yes"
out="$(parse ' CI: full')"
has "indented line is not a CI: line" "$out" "full=no"
out="$(parse '<!-- CI: full -->')"
has "the template's commented line is not a CI: line" "$out" "full=no"
out="$(parse 'ci: full')"
has "lower-case ci: is not a CI: line" "$out" "full=no"
out="$(parse $'CI: lane=yes\nCI: full')"
has "first CI: line wins" "$out" "full=no"
grep -q "ignored extra CI: line" "$tmp/err" && ok "second CI: line logged" || bad "second CI: line logged" "$(cat "$tmp/err")"
out="$(parse 'CI: colour=blue tests=affected lane=maybe release-build=1')"
has "bad key dropped" "$out" "full=no"
has "bad tests value dropped" "$out" "tests="
has "bad lane value dropped" "$out" "lane=no"
has "bad release-build value dropped" "$out" "release_build=no"
grep -q "ignored key: colour" "$tmp/err" && ok "bad key logged" || bad "bad key logged" "$(cat "$tmp/err")"
grep -q "ignored lane value" "$tmp/err" && ok "bad value logged" || bad "bad value logged" "$(cat "$tmp/err")"
out="$(parse 'CI: scenarios=alpha,nope,Beta,,gamma,alpha')"
has "unknown and upper-case ids dropped, duplicates once" "$out" "scenarios=alpha,gamma"
grep -q "no such scenario): nope" "$tmp/err" && ok "unknown id logged" || bad "unknown id logged" "$(cat "$tmp/err")"

echo "parse-ci-line.sh: injection"
out="$(parse 'CI: scenarios=../alpha lane=yes')"
has "a path in scenarios drops the token" "$out" "scenarios="
marker="$tmp/pwned"
out="$(parse "CI: scenarios=a;touch\$IFS$marker lane=yes")"
has "semicolon token dropped, the rest kept" "$out" "lane=yes"
has "semicolon token yields no scenario" "$out" "scenarios="
out="$(parse 'CI: scenarios=alpha;rm -rf / lane=yes')"
has "scenarios=a;rm -rf / dropped whole" "$out" "scenarios="
grep -q "ignored token" "$tmp/err" && ok "injection token logged" || bad "injection token logged" "$(cat "$tmp/err")"
out="$(parse "CI: scenarios=\$(touch $marker) \`touch $marker\` lane=\$(echo yes) *")"
[ ! -e "$marker" ] && ok "\$(...) and backticks never run" || bad "\$(...) and backticks never run"
has "\$(...) value dropped" "$out" "lane=no"
has "glob not expanded into scenarios" "$out" "scenarios="
out="$(parse $'CI: lane=yes\n::error::forged')"
! grep -q '^::' "$tmp/err" && ok "no log line starts a workflow command" || bad "no log line starts a workflow command" "$(cat "$tmp/err")"
ids="$(printf 's%s,' $(seq 1 25))"
out="$(parse "CI: scenarios=${ids%,}")"
n="$(sed -n 's/^scenarios=//p' <<< "$out" | tr ',' '\n' | grep -c .)"
[ "$n" = 20 ] && ok "at most 20 scenario ids" || bad "at most 20 scenario ids" "got $n"

echo "affected-crates.sh"
ac() { printf '%s\n' "$@" | "$ci/affected-crates.sh" --files -; }
out="$(ac crates/vam-telemetry/src/lib.rs)"
has "telemetry: affected" "$out" "scope=affected"
has "telemetry: itself and its dependents, transitively (dev edges too)" "$out" "crates=vam-orchestrator vam-storage vam-telemetry"
out="$(ac brokers/vam-web/src/lib.rs)"
has "a dev-dependency edge counts" "$out" "crates=vam-orchestrator vam-web"
out="$(ac crates/vam-firecracker/src/a.rs crates/vam-storage/b.rs)"
has "two crates: the union" "$out" "crates=vam-firecracker vam-orchestrator vam-storage"
out="$(ac examples/agent.json)"
has "test data: the reading crates only" "$out" "crates=vam-cli vam-orchestrator vam-security vam-spec"
out="$(ac scripts/ci/local.sh design/x.md)"
has "no crate: none" "$out" "scope=none"
for f in Cargo.toml Cargo.lock rust-toolchain.toml proto/vam/v1/a.proto .github/workflows/rust.yml third_party/ext4-view/src/lib.rs crates/vam-spec/src/lib.rs crates/vam-security/src/lib.rs; do
  out="$(ac crates/vam-telemetry/src/lib.rs "$f")"
  has "forced workspace: $f" "$out" "scope=workspace"
done
out="$(ac crates/vam-telemetry/Cargo.toml)"
has "a crate's own Cargo.toml is that crate, not forced" "$out" "scope=affected"

echo "plan.sh"
plan() { # <changed paths, newline-separated> then env assignments via the caller
  printf "%s\n" "$@" > "$tmp/changed"
  CHANGED_FILES="$tmp/changed" PLAN_SUMMARY="$tmp/summary" "$ci/plan.sh" 2>/dev/null
}
fb="HEAD_REPO=o/r BASE_REPO=o/r EVENT_NAME=pull_request BASE_REF=feat/x"
: > "$tmp/summary"
out="$(env $fb PR_BODY="" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-telemetry/src/lib.rs")"
has "feature PR, no CI: line: fast" "$out" "tier=fast"
has "feature PR: affected tests" "$out" "tests_scope=affected"
has "feature PR: affected crates" "$out" "affected=vam-orchestrator vam-storage vam-telemetry"
has "feature PR: no scenarios" "$out" "scenarios="
has "feature PR: no lane" "$out" "lane=false"
has "feature PR: no release build" "$out" "release_build=false"
grep -q "CI plan: fast tier" "$tmp/summary" && ok "plan written to the summary" || bad "plan written to the summary"
out="$(env $fb PR_BODY="CI: scenarios=alpha lane=yes release-build=yes" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-telemetry/src/lib.rs")"
has "CI: line: named scenarios" "$out" "scenarios=alpha"
has "CI: line: lane" "$out" "lane=true"
has "CI: line: release build" "$out" "release_build=true"
out="$(env $fb PR_BODY="CI: tests=workspace" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-telemetry/src/lib.rs")"
has "CI: tests=workspace" "$out" "tests_scope=workspace"
out="$(env $fb PR_BODY="" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan Cargo.lock")"
has "safety net: Cargo.lock forces workspace with no CI: line" "$out" "tests_scope=workspace"
has "safety net keeps the fast tier otherwise" "$out" "tier=fast"
out="$(env $fb PR_BODY="CI: full" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-telemetry/src/lib.rs")"
has "CI: full on a feature PR: full tier" "$out" "tier=full"
has "CI: full on a feature PR: full tests" "$out" "tests_scope=workspace"
has "CI: full: smoke" "$out" "scenarios=smoke"
has "CI: full: lane follows the VM paths (telemetry is not one)" "$out" "lane=false"
out="$(env $fb PR_BODY="CI: full" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-core/src/lib.rs scripts/release/build.sh")"
has "CI: full: VM path runs the lane" "$out" "lane=true"
has "CI: full: release path runs the release build" "$out" "release_build=true"
out="$(env HEAD_REPO=o/r BASE_REPO=o/r EVENT_NAME=pull_request BASE_REF=main PR_BODY="CI: scenarios=alpha" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-telemetry/src/lib.rs")"
has "PR into main ignores the CI: line: full" "$out" "tier=full"
has "PR into main: smoke" "$out" "scenarios=smoke"
has "PR into main: workspace tests" "$out" "tests_scope=workspace"
has "PR into main: release builds stay with release-build.yml" "$out" "release_build=false"
out="$(env HEAD_REPO=o/r BASE_REPO=o/r EVENT_NAME=pull_request BASE_REF=main RELEASE=true PR_LABELS=skip-vm-lane PR_BODY="" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan scripts/ci/local.sh")"
has "release PR into main: the lane always runs" "$out" "lane=true"
out="$(env HEAD_REPO=o/r BASE_REPO=o/r EVENT_NAME=pull_request BASE_REF=main PR_LABELS=a,skip-vm-lane PR_BODY="" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-core/src/lib.rs")"
has "skip-vm-lane label on a non-release PR into main" "$out" "lane=false"
out="$(env HEAD_REPO=fork/r BASE_REPO=o/r EVENT_NAME=pull_request BASE_REF=feat/x PR_BODY="" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-telemetry/src/lib.rs")"
has "fork PR: full" "$out" "tier=full"
has "fork PR: workspace tests" "$out" "tests_scope=workspace"
out="$(env HEAD_REPO= BASE_REPO=o/r EVENT_NAME=pull_request BASE_REF=feat/x PR_BODY="" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-telemetry/src/lib.rs")"
has "deleted fork (no head repo): full" "$out" "tier=full"
for ev in push workflow_dispatch; do
  out="$(env EVENT_NAME=$ev BASE_REF= PR_BODY="CI: scenarios=alpha" bash -c "$(declare -f plan); ci=$ci tmp=$tmp; plan crates/vam-telemetry/src/lib.rs")"
  has "$ev: full" "$out" "tier=full"
  has "$ev: lane" "$out" "lane=true"
done

echo
if [ "$fail" -ne 0 ]; then echo "ci tests: FAILED ($fail failed, $pass passed)"; exit 1; fi
echo "ci tests: PASS ($pass)"
