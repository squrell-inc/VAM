#!/usr/bin/env bash
# check-workflow.sh on the real workflows and on mutated copies (one forbidden line each), plus
# actionlint over every workflow when the binary is available (CI installs it and sets
# REQUIRE_ACTIONLINT=1; a session without it prints a skip line).
set -uo pipefail
. "$(dirname "$0")/helpers.sh"

check="$release_dir/check-workflow.sh"
wf="$repo_root/.github/workflows"
mkdir -p "$t_tmp/m"

echo "the real workflows pass"
expect_ok "check-workflow.sh on release*.yml and rust.yml" "$check"
expect_ok "check-workflow.sh on release.yml" "$check" "$wf/release.yml"

# mutate <file-name> <source> <old> <new>: writes $t_tmp/m/<file-name> = source with old replaced by new
# (old must occur exactly once); returns 1 when it does not, so a stale test fails loudly.
mutate() {
  python3 -I - "$1" "$2" "$3" "$4" "$t_tmp/m" <<'PY'
import sys
name, src, old, new, out = sys.argv[1:6]
s = open(src).read()
if s.count(old) != 1:
    sys.exit("mutation anchor found %d times: %r" % (s.count(old), old))
open(out + "/" + name, "w").write(s.replace(old, new))
PY
}
# rel_fails <name> <regex> <old> <new>: the mutated release.yml must be refused with /regex/
rel_fails() {
  local name="$1" pat="$2"; shift 2
  if ! out="$(mutate release.yml "$wf/release.yml" "$@" 2>&1)"; then bad "$name (test is stale)" "$out"; return; fi
  expect_fail "$name" "$pat" "$check" "$t_tmp/m/release.yml"
}

SHA=de0fac2e4500dabe0009e67214ff5f5447ce83dd
echo "release.yml: triggers, inputs, concurrency"
rel_fails "a push trigger" "workflow_dispatch only" "  workflow_dispatch:" "  push:
    branches: [main]
  workflow_dispatch:"
rel_fails "a schedule trigger" "workflow_dispatch only" "  workflow_dispatch:" "  schedule:
    - cron: '0 3 * * 1'
  workflow_dispatch:"
rel_fails "a pull_request_target trigger" "workflow_dispatch only" "  workflow_dispatch:" "  pull_request_target:
  workflow_dispatch:"
rel_fails "dry_run defaulting to false" "dry_run must be a boolean defaulting to true" "        default: true" "        default: false"
rel_fails "cancel-in-progress true" "cancel-in-progress: false" "  cancel-in-progress: false" "  cancel-in-progress: true"

echo "release.yml: permissions"
rel_fails "no top-level permissions" "top-level permissions must be exactly" "permissions: {}
" ""
rel_fails "top-level write permissions" "top-level permissions must be exactly" "permissions: {}
" "permissions:
  contents: write
"
rel_fails "write permission in validate" "write permissions; only publish may" "      actions: read
    outputs:" "      actions: read
      contents: write
    outputs:"
rel_fails "validate without a permissions block" "no permissions: block" "    permissions:
      contents: read
      actions: read
" ""
rel_fails "validate with extra read permissions" "exactly contents: read, actions: read" "      actions: read
    outputs:" "      actions: read
      packages: read
    outputs:"
rel_fails "a job without timeout-minutes" "no timeout-minutes" "    timeout-minutes: 20
" ""
rel_fails "secrets in a job" "references secrets" "          GH_TOKEN: \${{ github.token }}" "          GH_TOKEN: \${{ secrets.RELEASE_TOKEN }}"
rel_fails "continue-on-error" "continue-on-error" "        id: guards
" "        id: guards
        continue-on-error: true
"

echo "release.yml: actions"
rel_fails "an action pinned by tag" "not pinned by a full 40-hex" "actions/checkout@$SHA # v6.0.2" "actions/checkout@v6 # v6.0.2"
rel_fails "an action pinned by a short SHA" "not pinned by a full 40-hex" "actions/checkout@$SHA # v6.0.2" "actions/checkout@${SHA:0:12} # v6.0.2"
rel_fails "a pin without its version comment" "without a version comment" "actions/checkout@$SHA # v6.0.2" "actions/checkout@$SHA"
rel_fails "a docker:// action" "docker:// references" "      - name: Install cargo-deny" "      - uses: docker://alpine:3.20
      - name: Install cargo-deny"
rel_fails "actions/cache" "no caches" "      - name: Install cargo-deny" "      - uses: actions/cache@$SHA # v4.2.0
        with:
          path: ~/.cargo
          key: k
      - name: Install cargo-deny"
rel_fails "rust-cache" "no caches" "      - name: Install cargo-deny" "      - uses: Swatinem/rust-cache@$SHA # v2.7.0
      - name: Install cargo-deny"
rel_fails "checkout keeping credentials" "without persist-credentials: false" "          persist-credentials: false" "          persist-credentials: true"
rel_fails "checkout without persist-credentials" "without persist-credentials: false" "          persist-credentials: false
" ""
rel_fails "a shallow checkout in validate" "fetch-depth: 0" "          fetch-depth: 0
" "          fetch-depth: 1
"

echo "release.yml: inputs reach shell only through env:"
rel_fails "inputs.version inside run:" "inputs.version.*inside run:" "        run: scripts/release/install-tool.sh cargo-deny" "        run: scripts/release/install-tool.sh cargo-deny \${{ inputs.version }}"
rel_fails "github.event.inputs inside run:" "github.event.inputs.version.*inside run:" "        run: scripts/release/install-tool.sh cargo-deny" "        run: echo \${{ github.event.inputs.version }}"
rel_fails "github.head_ref inside run:" "github.head_ref.*inside run:" "        run: scripts/release/install-tool.sh cargo-deny" "        run: echo \${{ github.head_ref }}"
rel_fails "a step output inside run:" "steps.guards.outputs.commit.*inside run:" "        run: scripts/release/install-tool.sh cargo-deny" "        run: echo \${{ steps.guards.outputs.commit }}"
if mutate release.yml "$wf/release.yml" "        run: scripts/release/install-tool.sh cargo-deny" "        run: echo \${{ github.sha }} \${{ runner.temp }}" >/dev/null 2>&1; then
  expect_ok "an allowed expression (github.sha, runner.temp) inside run:" "$check" "$t_tmp/m/release.yml"
fi

echo "release.yml: the publish job (when a later wave adds it)"
publish_good="
  publish:
    name: publish
    runs-on: ubuntu-24.04
    needs: [validate]
    timeout-minutes: 30
    environment: release
    permissions:
      contents: write
      id-token: write
      attestations: write
    steps:
      - uses: actions/checkout@$SHA # v6.0.2
        with:
          persist-credentials: false
"
cp "$wf/release.yml" "$t_tmp/m/release.yml"; printf '%s' "$publish_good" >> "$t_tmp/m/release.yml"
expect_ok "a well-formed publish job (write permissions, environment: release) passes" "$check" "$t_tmp/m/release.yml"
cp "$wf/release.yml" "$t_tmp/m/release.yml"; printf '%s' "${publish_good/    environment: release
/}" >> "$t_tmp/m/release.yml"
expect_fail "publish without environment: release" "environment: release" "$check" "$t_tmp/m/release.yml"
cp "$wf/release.yml" "$t_tmp/m/release.yml"; printf '%s' "${publish_good/needs: \[validate\]/needs: []}" >> "$t_tmp/m/release.yml"
expect_fail "publish that does not need validate" "publish must need validate" "$check" "$t_tmp/m/release.yml"
cp "$wf/release.yml" "$t_tmp/m/release.yml"; printf '%s' "${publish_good/publish:/build:}" >> "$t_tmp/m/release.yml"
expect_fail "write permissions in a job that is not publish" "write permissions; only publish may" "$check" "$t_tmp/m/release.yml"

echo "release-build.yml"
cat > "$t_tmp/m/release-build.yml" <<YAML
name: Release build
on:
  workflow_call:
  pull_request:
    paths: ["scripts/release/**"]
  schedule:
    - cron: "0 4 * * 1"
permissions: {}
jobs:
  build:
    runs-on: ubuntu-24.04
    timeout-minutes: 30
    permissions:
      contents: read
    steps:
      - uses: actions/checkout@$SHA # v6.0.2
        with:
          persist-credentials: false
      - run: echo build
YAML
expect_ok "a build-only workflow passes" "$check" "$t_tmp/m/release-build.yml"
cp "$t_tmp/m/release-build.yml" "$t_tmp/m/rb.yml"
python3 -I - "$t_tmp/m" <<'PY'
import sys
d = sys.argv[1]
s = open(d + "/rb.yml").read()
open(d + "/release-build.yml", "w").write(s.replace("  workflow_call:\n", "  workflow_call:\n  push:\n"))
PY
expect_fail "release-build.yml with a push trigger" "may trigger only on workflow_call" "$check" "$t_tmp/m/release-build.yml"
python3 -I - "$t_tmp/m" <<'PY'
import sys
d = sys.argv[1]
s = open(d + "/rb.yml").read()
open(d + "/release-build.yml", "w").write(s.replace("      contents: read\n", "      contents: write\n"))
PY
expect_fail "a build job with write permissions" "write permissions; only publish may" "$check" "$t_tmp/m/release-build.yml"
python3 -I - "$t_tmp/m" <<'PY'
import sys
d = sys.argv[1]
s = open(d + "/rb.yml").read()
open(d + "/release-build.yml", "w").write(s.replace("      - run: echo build\n", "      - run: echo build\n        env:\n          K: ${{ secrets.K }}\n"))
PY
expect_fail "a build job reading a secret" "references secrets" "$check" "$t_tmp/m/release-build.yml"

echo "rust.yml keeps a complete push run on main"
rust_fails() {
  local name="$1" pat="$2"; shift 2
  if ! out="$(mutate rust.yml "$wf/rust.yml" "$@" 2>&1)"; then bad "$name (test is stale)" "$out"; return; fi
  expect_fail "$name" "$pat" "$check" "$t_tmp/m/rust.yml"
}
rust_fails "cancel-in-progress true for every event" "cancel-in-progress must be true for pull_request runs only" "  cancel-in-progress: \${{ github.event_name == 'pull_request' }}" "  cancel-in-progress: true"
rust_fails "a concurrency group shared by push runs" "unique per push run" "github.event_name == 'pull_request' && github.ref || github.run_id" "github.ref"

echo "actionlint"
actionlint_bin="${ACTIONLINT:-$(command -v actionlint || true)}"
[ -n "$actionlint_bin" ] || { [ -x "$HOME/.local/bin/actionlint" ] && actionlint_bin="$HOME/.local/bin/actionlint"; }
if [ -n "$actionlint_bin" ]; then
  expect_ok "actionlint is clean on every workflow" "$actionlint_bin" "$wf"/*.yml
elif [ "${REQUIRE_ACTIONLINT:-0}" = 1 ]; then
  bad "actionlint is required here (REQUIRE_ACTIONLINT=1) but not installed: scripts/release/install-tool.sh actionlint"
else
  echo "  skip actionlint is not installed (scripts/release/install-tool.sh actionlint); CI requires it"
fi
finish
