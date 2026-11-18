#!/usr/bin/env bash
# check-workflow.sh on the real workflows and on mutated copies (one forbidden line each), plus
# actionlint over the release workflows (release*.yml) when the binary is available (CI installs
# it and sets REQUIRE_ACTIONLINT=1; a session without it prints a skip line). The other workflows
# are left alone (owner, 2026-11-18): their shellcheck findings predate this workstream.
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
    inputs:
      version: {type: string}
      commit: {type: string}
      channel: {type: string}
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
  assemble:
    runs-on: ubuntu-24.04
    timeout-minutes: 30
    needs: [meta, guest, node-x86_64, cli-linux, cli-macos]
    permissions:
      contents: read
      pull-requests: read
    steps:
      - uses: actions/download-artifact@$SHA # v7.0.0
        with:
          pattern: asset-*
          path: assets
      - run: scripts/release/assemble.sh
      - uses: actions/upload-artifact@$SHA # v6.0.0
        with:
          name: release-1
          path: release/*
          if-no-files-found: error
          retention-days: 7
YAML
python3 -I - "$t_tmp/m" <<'PY'
import sys
p = sys.argv[1] + "/release-build.yml"
s = open(p).read()
for j in ("meta", "guest", "node-x86_64", "cli-linux", "cli-macos"):
    s = s.replace("  assemble:\n", "  %s:\n    runs-on: ubuntu-24.04\n    timeout-minutes: 5\n    permissions:\n      contents: read\n    steps:\n      - run: echo %s\n  assemble:\n" % (j, j), 1)
open(p, "w").write(s)
PY
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

echo "release.yml calls the reusable build"
rel_fails "no build job" "no build job" "  build:
    name: build" "  buildx:
    name: build"
rel_fails "a build calling another workflow" "must call ./.github/workflows/release-build.yml" "    uses: ./.github/workflows/release-build.yml" "    uses: ./.github/workflows/rust.yml"
rel_fails "a build on the dev channel" "must pass channel: release" "      channel: release" "      channel: dev"
rel_fails "a build with write permissions" "write permissions; only publish may" "      pull-requests: read
    uses:" "      pull-requests: write
    uses:"
rel_fails "a build without the pull-requests read the assemble job needs" "build permissions must be exactly contents: read, pull-requests: read" "      pull-requests: read
    uses:" "    uses:"
rel_fails "a build with more than contents and pull-requests read" "build permissions must be exactly contents: read, pull-requests: read" "      pull-requests: read
    uses:" "      pull-requests: read
      actions: read
    uses:"
rel_fails "a build with contents write" "write permissions; only publish may" "      contents: read
      pull-requests: read
    uses:" "      contents: write
      pull-requests: read
    uses:"
rel_fails "a build passing secrets" "passes no secrets" "      channel: release" "      channel: release
    secrets: inherit"

echo "release-build.yml (the real file)"
expect_ok "release-build.yml passes" "$check" "$wf/release-build.yml"
rb_fails() {
  local name="$1" pat="$2"; shift 2
  if ! out="$(mutate release-build.yml "$wf/release-build.yml" "$@" 2>&1)"; then bad "$name (test is stale)" "$out"; return; fi
  expect_fail "$name" "$pat" "$check" "$t_tmp/m/release-build.yml"
}
rb_fails "a push trigger" "may trigger only on workflow_call" "  schedule:
    - cron:" "  push:
    branches: [main]
  schedule:
    - cron:"
rb_fails "pull_request_target" "may trigger only on workflow_call" "  schedule:
    - cron:" "  pull_request_target:
  schedule:
    - cron:"
rb_fails "a pull_request trigger without paths" "needs a paths filter" "    paths:
      - \"scripts/release/**\"" "    branches: [main]
    x-paths:
      - \"scripts/release/**\""
rb_fails "a missing channel input" "string inputs version, commit and channel" "      channel:
        description: \"release or dev (dev when blank)\"
        type: string" "      channel_x:
        description: \"release or dev (dev when blank)\"
        type: string"
rb_fails "write permission in the node job" "write permissions; only publish may" "    timeout-minutes: 90
    permissions:
      contents: read" "    timeout-minutes: 90
    permissions:
      contents: write"
rb_fails "an id-token in a build job" "permissions must be exactly contents: read" "    timeout-minutes: 10
    permissions:
      contents: read" "    timeout-minutes: 10
    permissions:
      contents: read
      id-token: write"
rb_fails "a build job in an environment" "no job in an environment" "    name: meta
    runs-on: ubuntu-24.04" "    name: meta
    environment: release
    runs-on: ubuntu-24.04"
rb_fails "an artifact without retention-days" "retention-days, 30 at most" "          name: asset-node-x86_64-unknown-linux-gnu
          path: dist/*
          if-no-files-found: error
          retention-days: 7" "          name: asset-node-x86_64-unknown-linux-gnu
          path: dist/*
          if-no-files-found: error"
rb_fails "an artifact kept for 90 days" "retention-days, 30 at most" "          name: asset-node-x86_64-unknown-linux-gnu
          path: dist/*
          if-no-files-found: error
          retention-days: 7" "          name: asset-node-x86_64-unknown-linux-gnu
          path: dist/*
          if-no-files-found: error
          retention-days: 90"
rb_fails "an artifact that tolerates missing files" "if-no-files-found: error" "          name: asset-runtime-x86_64-unknown-linux-musl
          path: dist/*
          if-no-files-found: error" "          name: asset-runtime-x86_64-unknown-linux-musl
          path: dist/*
          if-no-files-found: warn"
rb_fails "a cache action" "no caches" "      - name: Install musl-tools" "      - uses: actions/cache@$SHA # v4.2.0
        with:
          path: ~/.cargo
          key: k
      - name: Install musl-tools"
rb_fails "an unpinned upload-artifact" "not pinned by a full 40-hex" "actions/upload-artifact@b7c566a772e6b6bfb58ed0dc250532a479d7789f # v6.0.0
        with:
          name: asset-node" "actions/upload-artifact@v6 # v6.0.0
        with:
          name: asset-node"
rb_fails "the version input inside run:" "inputs.version.*inside run:" "        run: sudo apt-get update -qq && sudo apt-get install -y -qq musl-tools" "        run: echo \${{ inputs.version }}"
rb_fails "a container image pinned by tag" "pinned by digest" "    container:
      image: almalinux@sha256:02df05719ef287ebd0df0a1eedb9172a00bd70703aed7b25660e426b8e0d6047
    timeout-minutes: 90" "    container:
      image: almalinux:9
    timeout-minutes: 90"
rb_fails "a container image with a short digest" "pinned by digest" "    container:
      image: almalinux@sha256:02df05719ef287ebd0df0a1eedb9172a00bd70703aed7b25660e426b8e0d6047
    timeout-minutes: 90" "    container:
      image: almalinux@sha256:02df0571
    timeout-minutes: 90"
rb_fails "a container with options" "takes no credentials, env or options" "    container:
      image: almalinux@sha256:02df05719ef287ebd0df0a1eedb9172a00bd70703aed7b25660e426b8e0d6047
    timeout-minutes: 90" "    container:
      image: almalinux@sha256:02df05719ef287ebd0df0a1eedb9172a00bd70703aed7b25660e426b8e0d6047
      options: --privileged
    timeout-minutes: 90"
rb_fails "a matrix value inside run:" "matrix.target.*inside run:" "      - name: Build vam (native gcc, glibc 2.34)
        run: scripts/release/build.sh \"\$TARGET\" --set cli --out \"\$RUNNER_TEMP/bin\"" "      - name: Build vam (native gcc, glibc 2.34)
        run: scripts/release/build.sh \${{ matrix.target }} --set cli --out \"\$RUNNER_TEMP/bin\""
rb_fails "a job without a timeout" "no timeout-minutes" "    timeout-minutes: 90
" ""

echo "release-build.yml: the assemble job"
rb_fails "assemble with write permissions" "write permissions; only publish may" "      contents: read
      pull-requests: read
    env:" "      contents: read
      pull-requests: write
    env:"
rb_fails "assemble with more than it needs" "assemble permissions must be exactly" "      contents: read
      pull-requests: read
    env:" "      contents: read
      pull-requests: read
      packages: read
    env:"
rb_fails "assemble without pull-requests read" "assemble permissions must be exactly" "      contents: read
      pull-requests: read
    env:" "      contents: read
    env:"
rb_fails "assemble with id-token" "assemble permissions must be exactly" "      pull-requests: read
    env:" "      pull-requests: read
      id-token: write
    env:"
rb_fails "pull-requests read on another job" "every build job's permissions must be exactly contents: read" "    timeout-minutes: 10
    permissions:
      contents: read" "    timeout-minutes: 10
    permissions:
      contents: read
      pull-requests: read"
rb_fails "no assemble job" "no assemble job" "  assemble:
    name: assemble" "  assemblex:
    name: assemble"
rb_fails "assemble that does not wait for a build job" "assemble must need meta and every build job" "    needs: [meta, guest, node-x86_64, cli-linux, cli-macos]" "    needs: [meta, guest, node-x86_64, cli-linux]"
rb_fails "assemble that does not run assemble.sh" "does not run scripts/release/assemble.sh" "          scripts/release/assemble.sh --version" "          echo scripts/release/skip.sh --version"
rb_fails "assemble that downloads a narrower set" "asset-.* artifacts" "          pattern: asset-*" "          pattern: asset-node*"
rb_fails "assemble uploading something else" "release-<version> artifact" "          name: release-\${{ needs.meta.outputs.version }}" "          name: assembled"
rb_fails "a token in a build job" "only assemble gets github.token" "      VAM_BUILD_CHANNEL: \${{ needs.meta.outputs.channel }}
      COMMIT: \${{ needs.meta.outputs.commit }}
    steps:
      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd # v6.0.2
        with:
          ref: \${{ needs.meta.outputs.commit }}
          fetch-depth: 0
          fetch-tags: true
          persist-credentials: false
      - name: Install musl-tools" "      VAM_BUILD_CHANNEL: \${{ needs.meta.outputs.channel }}
      COMMIT: \${{ needs.meta.outputs.commit }}
      GH_TOKEN: \${{ github.token }}
    steps:
      - uses: actions/checkout@de0fac2e4500dabe0009e67214ff5f5447ce83dd # v6.0.2
        with:
          ref: \${{ needs.meta.outputs.commit }}
          fetch-depth: 0
          fetch-tags: true
          persist-credentials: false
      - name: Install musl-tools"
rb_fails "a secret in assemble" "references secrets" "      GH_TOKEN: \${{ github.token }}
    steps:
      # full history" "      GH_TOKEN: \${{ secrets.PAT }}
    steps:
      # full history"
rb_fails "the version input inside run: in assemble" "inputs.version.*inside run:" "          ls -l assets
" "          echo \${{ inputs.version }}
"
rb_fails "an unpinned download-artifact" "not pinned by a full 40-hex" "actions/download-artifact@37930b1c2abaa49bbe596cd826c3c89aef350131 # v7.0.0
        with:
          pattern" "actions/download-artifact@v7 # v7.0.0
        with:
          pattern"
rb_fails "assemble uploading without if-no-files-found: error" "if-no-files-found: error" "          name: release-\${{ needs.meta.outputs.version }}
          path: release/*
          if-no-files-found: error" "          name: release-\${{ needs.meta.outputs.version }}
          path: release/*
          if-no-files-found: ignore"

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
  expect_ok "actionlint is clean on the release workflows" "$actionlint_bin" "$wf"/release*.yml
elif [ "${REQUIRE_ACTIONLINT:-0}" = 1 ]; then
  bad "actionlint is required here (REQUIRE_ACTIONLINT=1) but not installed: scripts/release/install-tool.sh actionlint"
else
  echo "  skip actionlint is not installed (scripts/release/install-tool.sh actionlint); CI requires it"
fi
finish
