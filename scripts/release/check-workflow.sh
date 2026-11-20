#!/usr/bin/env bash
# Lints the release workflows against the rules of the release plan (locked decisions 4, 5, 8, 9).
#
#   scripts/release/check-workflow.sh [workflow.yml ...]
#
# With no arguments: .github/workflows/release*.yml, and rust.yml's concurrency rule. A file's rules
# follow its name: release.yml (the dispatch workflow), release-build.yml (reusable and build-only),
# rust.yml (guard 3 needs a complete push run on every merge into main). Every violation is
# reported; exit 1 if there is any. Needs jq, and python3 with PyYAML or mikefarah's yq.
set -uo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
errors=0
err() { printf 'check-workflow: %s: %s\n' "$cur" "$1" >&2; errors=$((errors + 1)); }

command -v jq >/dev/null || { echo "check-workflow: jq is required" >&2; exit 2; }

yaml2json() {
  if python3 -c 'import yaml' 2>/dev/null; then
    python3 -I -c '
import json, sys, yaml
def norm(x):
    if isinstance(x, dict):
        return {("on" if k is True else str(k)): norm(v) for k, v in x.items()}
    if isinstance(x, list):
        return [norm(v) for v in x]
    return x
json.dump(norm(yaml.safe_load(sys.stdin)), sys.stdout)' < "$1"
  elif command -v yq >/dev/null && yq --version 2>&1 | grep -qi mikefarah; then
    yq -o=json '.' "$1"
  else
    echo "check-workflow: need python3 with PyYAML, or mikefarah's yq, to read YAML" >&2; return 1
  fi
}

# The only expressions a `run:` script may contain; everything else goes through `env:`.
allowed_run_exprs='github.sha github.run_id github.repository runner.temp'

check_common() { # <json> <file>
  local j="$1" f="$2" jobs job
  jq -e '.permissions == {}' <<< "$j" >/dev/null || err "top-level permissions must be exactly {} (each job asks for its own)"
  # uses: pinned by full SHA, no docker://, no caches
  local u
  while IFS= read -r u; do
    [ -n "$u" ] || continue
    case "$u" in
      ./*) continue ;;
      docker://*) err "uses: $u (docker:// references are not allowed)"; continue ;;
    esac
    [[ "$u" =~ ^[A-Za-z0-9._/-]+@[0-9a-f]{40}$ ]] || err "uses: $u is not pinned by a full 40-hex commit SHA"
    case "$u" in actions/cache*|*rust-cache*) err "uses: $u (no caches in release workflows: a PR-written cache would reach the release)" ;; esac
  done < <(jq -r '.jobs[] | (.uses? // empty), (.steps[]? | .uses? // empty)' <<< "$j")
  # every pinned `uses:` line carries its version in a comment
  local line
  while IFS= read -r line; do
    case "$line" in *"uses: ./"*) continue ;; esac
    [[ "$line" =~ @[0-9a-f]{40}[[:space:]]+\#[[:space:]]*v?[0-9] ]] || err "line without a version comment after the SHA: ${line#"${line%%[![:space:]]*}"}"
  done < <(grep -E '^[[:space:]]*-?[[:space:]]*uses:' "$f" || true)
  # checkouts do not keep the token
  local n
  n="$(jq -r '[.jobs[] | .steps[]? | select((.uses? // "") | startswith("actions/checkout@")) | select((.with["persist-credentials"]? | tostring) != "false")] | length' <<< "$j")"
  [ "$n" = 0 ] || err "$n actions/checkout step(s) without persist-credentials: false"
  # run: scripts only see allowed expressions
  local job_id step_name expr trimmed
  while IFS=$'\t' read -r job_id step_name expr; do
    [ -n "$expr" ] || continue
    trimmed="$(sed -E 's/^\$\{\{[[:space:]]*//; s/[[:space:]]*\}\}$//' <<< "$expr")"
    case " $allowed_run_exprs " in *" $trimmed "*) continue ;; esac
    err "job '$job_id' step '$step_name': \${{ $trimmed }} inside run: ; pass it through env: (inputs and event data must never be interpolated into shell)"
  done < <(jq -r '.jobs | to_entries[] | .key as $k | .value.steps[]? | select(.run != null) | (.name // .id // "run") as $n | .run | [match("\\$\\{\\{[^}]*\\}\\}"; "g").string][] | [$k, $n, .] | @tsv' <<< "$j")
  # secrets, write permissions and continue-on-error stay out of every job but publish
  jobs="$(jq -r '.jobs | keys[]' <<< "$j")"
  for job in $jobs; do
    if jq -e --arg k "$job" '.jobs[$k] | .. | strings | select(test("secrets\\."))' <<< "$j" >/dev/null 2>&1 && [ "$job" != publish ]; then
      err "job '$job' references secrets.*; only publish may (and it needs none beyond GITHUB_TOKEN)"; fi
    if jq -e --arg k "$job" '.jobs[$k].secrets? != null' <<< "$j" >/dev/null && [ "$job" != publish ]; then
      err "job '$job' passes secrets; build and validate jobs hold none"; fi
    if [ "$job" != publish ] && jq -e --arg k "$job" '.jobs[$k].permissions? | (type == "object") and (to_entries | any(.value == "write")) or (. == "write-all")' <<< "$j" >/dev/null; then
      err "job '$job' has write permissions; only publish may"; fi
    if jq -e --arg k "$job" '(.jobs[$k].permissions? // null) == null' <<< "$j" >/dev/null; then
      err "job '$job' has no permissions: block (it would inherit; say what it needs)"; fi
    if jq -e --arg k "$job" '.jobs[$k] | (has("uses") | not) and ((has("timeout-minutes")) | not)' <<< "$j" >/dev/null; then
      err "job '$job' has no timeout-minutes"; fi
    if jq -e --arg k "$job" '.jobs[$k] | [.. | objects | select(has("continue-on-error"))] | length > 0' <<< "$j" >/dev/null; then
      err "job '$job' uses continue-on-error; a release check must fail the run"; fi
  done
  if jq -e '.jobs | has("publish")' <<< "$j" >/dev/null; then
    jq -e '.jobs.publish.environment == "release" or .jobs.publish.environment.name == "release"' <<< "$j" >/dev/null || err "publish must run in environment: release (the owner approves there)"
    jq -e '.jobs.publish.needs | (if type == "string" then [.] else . end) | index("validate") != null' <<< "$j" >/dev/null 2>&1 || err "publish must need validate"
  fi
}

check_release() { # <json>: the dispatch workflow
  local j="$1"
  jq -e '.on | keys == ["workflow_dispatch"]' <<< "$j" >/dev/null || err "release.yml must trigger on workflow_dispatch only (no push, tag, schedule or pull_request trigger)"
  jq -e '.on.workflow_dispatch.inputs | (.version.required == true) and (.version.type == "string")' <<< "$j" >/dev/null || err "input version must be a required string"
  jq -e '.on.workflow_dispatch.inputs | has("commit")' <<< "$j" >/dev/null || err "input commit is missing"
  jq -e '.on.workflow_dispatch.inputs.dry_run | (.type == "boolean") and (.default == true)' <<< "$j" >/dev/null || err "input dry_run must be a boolean defaulting to true"
  jq -e '.concurrency.group == "release" and .concurrency["cancel-in-progress"] == false' <<< "$j" >/dev/null || err "concurrency must be group: release with cancel-in-progress: false"
  jq -e '.jobs | has("validate")' <<< "$j" >/dev/null || { err "no validate job"; return; }
  jq -e '.jobs.validate.permissions == {"contents": "read", "actions": "read"}' <<< "$j" >/dev/null || err "validate permissions must be exactly contents: read, actions: read"
  jq -e '[.jobs.validate.steps[] | select((.uses? // "") | startswith("actions/checkout@")) | .with["fetch-depth"]] | (length > 0) and all(. == 0)' <<< "$j" >/dev/null || err "validate must check out with fetch-depth: 0 (guards read the full history)"
  jq -e '[.jobs.validate.steps[] | select(.run? != null) | .run | contains("guards.sh")] | any' <<< "$j" >/dev/null || err "validate does not run scripts/release/guards.sh"
  # the builds are the reusable workflow, called with the release channel and nothing secret
  jq -e '.jobs | has("build")' <<< "$j" >/dev/null || { err "no build job (it calls release-build.yml)"; return; }
  jq -e '.jobs.build.uses == "./.github/workflows/release-build.yml"' <<< "$j" >/dev/null || err "build must call ./.github/workflows/release-build.yml"
  jq -e '.jobs.build.with.channel == "release"' <<< "$j" >/dev/null || err "build must pass channel: release (only the release workflow builds a release)"
  # pull-requests: read is the cap for release-build.yml's `assemble` job (it reads PR bodies for the notes)
  jq -e '.jobs.build.permissions == {"contents": "read", "pull-requests": "read"}' <<< "$j" >/dev/null || err "build permissions must be exactly contents: read, pull-requests: read (the called workflow's assemble job reads pull requests; nothing else)"
  jq -e '(.jobs.build | has("secrets") | not) and (.jobs.build | has("environment") | not)' <<< "$j" >/dev/null || err "build passes no secrets (no secrets: inherit) and has no environment"
  check_publish "$j"
}

# the publish job: the only writer, behind the owner's approval, steps in a fixed order
check_publish() { # <json>
  local j="$1" i
  jq -e '.jobs | has("publish")' <<< "$j" >/dev/null || { err "no publish job"; return; }
  jq -e '.jobs.publish.needs | (if type == "string" then [.] else . end) | sort == ["build", "validate"]' <<< "$j" >/dev/null 2>&1 || err "publish must need exactly validate and build"
  jq -e '.jobs.publish.if? | strings | contains("inputs.dry_run")' <<< "$j" >/dev/null 2>&1 || err "publish must run only when dry_run is false (an if: on inputs.dry_run)"
  jq -e '.jobs.publish | .. | strings | select(test("secrets\\."))' <<< "$j" >/dev/null 2>&1 && err "publish references secrets.*; it needs only the job's GITHUB_TOKEN"
  jq -e '.jobs.publish.permissions == {"contents": "write", "id-token": "write", "attestations": "write"}' <<< "$j" >/dev/null || err "publish permissions must be exactly contents: write, id-token: write, attestations: write"
  # the two commits it compares come from two different jobs, and the build's own outputs
  jq -e '.jobs.publish.env.COMMIT == "${{ needs.build.outputs.commit }}" and .jobs.publish.env.VERSION == "${{ needs.build.outputs.version }}"' <<< "$j" >/dev/null || err "publish env COMMIT and VERSION must be needs.build.outputs.commit and .version"
  jq -e '.jobs.publish.env.VALIDATED_COMMIT == "${{ needs.validate.outputs.commit }}" and .jobs.publish.env.VALIDATED_VERSION == "${{ needs.validate.outputs.version }}"' <<< "$j" >/dev/null || err "publish env VALIDATED_COMMIT and VALIDATED_VERSION must be needs.validate.outputs.commit and .version (the check compares them with the build's)"
  jq -e '[.jobs.publish.steps[] | select((.uses? // "") | startswith("actions/download-artifact@")) | .with.name | tostring | startswith("release-")] | any' <<< "$j" >/dev/null || err "publish must download the release-<version> artifact"
  # every release operation lives in scripts/release/publish.sh and verify.sh, where the fixture tests are
  while IFS= read -r i; do
    [ -n "$i" ] || continue
    err "publish runs '$i' in the workflow; release operations belong in scripts/release/publish.sh and verify.sh (tested against fixtures)"
  done < <(jq -r '.jobs.publish.steps[] | .run? // empty | split("\n")[] | select(test("gh[[:space:]]+release[[:space:]]+(create|edit|delete|upload)|draft=false|-X[[:space:]]+(PATCH|DELETE|POST|PUT)|gh[[:space:]]+api")) | gsub("^[[:space:]]+"; "")' <<< "$j")
  # the order: check, draft, attest..., verify the draft, publish, verify the release
  jq -e '
    [.jobs.publish.steps | to_entries[] | .key as $k | .value
      | {k: $k, run: (.run? // ""), uses: (.uses? // "")}] as $s
    | def first(f): [$s[] | select(f) | .k] | first;
      def last(f): [$s[] | select(f) | .k] | last;
      (first(.run | contains("publish.sh check"))) as $check
    | (first(.run | contains("publish.sh draft"))) as $draft
    | (first(.uses | startswith("actions/attest@"))) as $attest_first
    | (last(.uses | startswith("actions/attest@"))) as $attest_last
    | (first(.run | contains("verify.sh draft"))) as $vdraft
    | (first(.run | contains("publish.sh publish"))) as $publish
    | (first(.run | contains("verify.sh release"))) as $vrelease
    | ([$check, $draft, $attest_first, $attest_last, $vdraft, $publish, $vrelease] | all(. != null))
      and $check < $draft and $draft < $attest_first and $attest_last < $vdraft and $vdraft < $publish and $publish < $vrelease' <<< "$j" >/dev/null 2>&1 \
    || err "publish steps must run in this order: publish.sh check, publish.sh draft, actions/attest (every one), verify.sh draft, publish.sh publish, verify.sh release"
  jq -e '[.jobs.publish.steps[] | select((.run? // "") | contains("publish.sh abort")) | select((.if? // "") | (contains("failure()") and contains("cancelled()")))] | length == 1' <<< "$j" >/dev/null 2>&1 \
    || err "publish needs one cleanup step running publish.sh abort with if: failure() || cancelled()"
  jq -e '(.jobs.publish.steps | last | .run? // "") | contains("publish.sh abort")' <<< "$j" >/dev/null 2>&1 || err "the cleanup step (publish.sh abort) must be the last step of publish"
}

check_release_build() { # <json>: the reusable build workflow
  local j="$1"
  jq -e '.on | has("workflow_call") and ((keys - ["workflow_call", "pull_request", "schedule"]) == [])' <<< "$j" >/dev/null || err "release-build.yml may trigger only on workflow_call, pull_request and schedule"
  jq -e '[.jobs[] | select(has("environment"))] | length == 0' <<< "$j" >/dev/null || err "release-build.yml has no job in an environment (publishing is release.yml's publish job)"
  jq -e '.on.workflow_call.inputs | (.version.type == "string") and (.commit.type == "string") and (.channel.type == "string")' <<< "$j" >/dev/null || err "workflow_call must take string inputs version, commit and channel"
  jq -e '(.on | has("pull_request") | not) or ((.on.pull_request.paths? // []) | (type == "array") and length > 0)' <<< "$j" >/dev/null || err "the pull_request trigger needs a paths filter (build only when the release path changes)"
  jq -e '[.jobs | to_entries[] | select(.key != "assemble") | select(.value | has("uses") | not) | .value.permissions] | all(. == {"contents": "read"})' <<< "$j" >/dev/null || err "every build job's permissions must be exactly contents: read"
  # assemble: the one job that also reads pull requests (release notes); it never writes
  jq -e '.jobs | has("assemble")' <<< "$j" >/dev/null || { err "no assemble job (SHA256SUMS, notes and scans)"; return; }
  jq -e '.jobs.assemble.permissions == {"contents": "read", "pull-requests": "read"}' <<< "$j" >/dev/null || err "assemble permissions must be exactly contents: read, pull-requests: read"
  jq -e '.jobs.assemble.needs | (if type == "string" then [.] else . end) | (index("meta") != null) and (index("guest") != null) and (index("node-x86_64") != null) and (index("cli-linux") != null) and (index("cli-macos") != null)' <<< "$j" >/dev/null 2>&1 || err "assemble must need meta and every build job (guest, node-x86_64, cli-linux, cli-macos)"
  jq -e '[.jobs.assemble.steps[] | select(.run? != null) | .run | contains("assemble.sh")] | any' <<< "$j" >/dev/null || err "assemble does not run scripts/release/assemble.sh"
  jq -e '[.jobs.assemble.steps[] | select((.uses? // "") | startswith("actions/download-artifact@")) | .with.pattern] | any(. == "asset-*")' <<< "$j" >/dev/null || err "assemble must download the asset-* artifacts (download-artifact with pattern: asset-*)"
  jq -e '[.jobs.assemble.steps[] | select((.uses? // "") | startswith("actions/upload-artifact@")) | .with.name | tostring | startswith("release-")] | any' <<< "$j" >/dev/null || err "assemble must upload the release-<version> artifact"
  # only assemble holds a token for pull requests; the other jobs never see pull-requests
  jq -e '[.jobs | to_entries[] | select(.key != "assemble") | .value | .. | strings | select(test("github\\.token"))] | length == 0' <<< "$j" >/dev/null || err "only assemble gets github.token in this workflow (the build jobs take no token at all)"
  jq -e '[.jobs[] | .container? // empty | (if type == "string" then . else .image end) | select(test("@sha256:[0-9a-f]{64}$") | not)] | length == 0' <<< "$j" >/dev/null || err "a job container image must be pinned by digest (image@sha256:<64 hex>), never by tag"
  jq -e '[.jobs[] | .container? // empty | (if type == "object" then (.credentials? != null or .env? != null or .options? != null) else false end)] | any | not' <<< "$j" >/dev/null || err "a job container takes no credentials, env or options (no secrets in builds, no privileged flags)"
  jq -e '[.jobs[] | .steps[]? | select((.uses? // "") | startswith("actions/upload-artifact@")) | select((.with["retention-days"]? == null) or (.with["retention-days"] > 30))] | length == 0' <<< "$j" >/dev/null || err "upload-artifact needs retention-days, 30 at most (build-only runs leave short-lived artifacts)"
  jq -e '[.jobs[] | .steps[]? | select((.uses? // "") | startswith("actions/upload-artifact@")) | select(.with["if-no-files-found"]? != "error")] | length == 0' <<< "$j" >/dev/null || err "upload-artifact must set if-no-files-found: error (a missing asset fails the run)"
  # install-smoke: the shipped install.sh, from the release-<version> artifact, on Linux and macOS
  jq -e '.jobs | has("install-smoke")' <<< "$j" >/dev/null || { err "no install-smoke job (it runs install.sh on Linux and macOS against this run's assets)"; return; }
  jq -e '.jobs["install-smoke"].needs | (if type == "string" then [.] else . end) | index("assemble") != null' <<< "$j" >/dev/null 2>&1 || err "install-smoke must need assemble (it installs from the release-<version> artifact)"
  jq -e '.jobs["install-smoke"].strategy.matrix.os | sort == ["macos-15", "ubuntu-24.04"]' <<< "$j" >/dev/null 2>&1 || err "install-smoke must run on exactly ubuntu-24.04 and macos-15"
  jq -e '.jobs["install-smoke"]["runs-on"] == "${{ matrix.os }}"' <<< "$j" >/dev/null 2>&1 || err "install-smoke must run on matrix.os"
  jq -e '[.jobs["install-smoke"].steps[] | select((.uses? // "") | startswith("actions/download-artifact@")) | .with.name | tostring | startswith("release-")] | any' <<< "$j" >/dev/null || err "install-smoke must download the release-<version> artifact (the installer it tests is the one that ships)"
  jq -e '[.jobs["install-smoke"].steps[] | select(.run? != null) | .run | select(contains("/bin/sh release/install.sh")) | contains("/bin/sh release/install.sh --from")] | (length > 0) and all' <<< "$j" >/dev/null || err "install-smoke must run every /bin/sh release/install.sh with --from <the downloaded release> (no download from GitHub releases)"
  jq -e '[.jobs["install-smoke"].steps[] | select(.run? != null) | .run | contains("undock")] | any' <<< "$j" >/dev/null || err "install-smoke must run vam undock"
  jq -e '[.jobs["install-smoke"].steps[] | select((.uses? // "") | startswith("actions/checkout@"))] | length == 0' <<< "$j" >/dev/null || err "install-smoke must not check out the repository (it tests the release's own install.sh)"
}

check_rust() { # <json>: guard 3 needs a complete push run on every merge into main
  local j="$1"
  jq -e '.concurrency["cancel-in-progress"] == "${{ github.event_name == '"'"'pull_request'"'"' }}"' <<< "$j" >/dev/null || err "concurrency cancel-in-progress must be true for pull_request runs only, or a later merge cancels the push run guard 3 reads"
  jq -e '.concurrency.group | contains("github.run_id")' <<< "$j" >/dev/null || err "concurrency group must be unique per push run (github.run_id) outside pull requests: GitHub replaces a pending run in a shared group even without cancel-in-progress"
}

files=("$@")
if [ ${#files[@]} -eq 0 ]; then
  files=("$repo"/.github/workflows/release*.yml "$repo"/.github/workflows/rust.yml)
fi
for f in "${files[@]}"; do
  cur="$(basename "$f")"
  [ -f "$f" ] || { err "no such file: $f"; continue; }
  json="$(yaml2json "$f")" || { err "cannot parse $f"; continue; }
  case "$cur" in
    rust.yml) check_rust "$json" ;;
    release-build.yml) check_common "$json" "$f"; check_release_build "$json" ;;
    release*.yml) check_common "$json" "$f"; check_release "$json" ;;
    *) err "no rules for $cur" ;;
  esac
done
if [ "$errors" -ne 0 ]; then echo "check-workflow: $errors violation(s)" >&2; exit 1; fi
echo "check-workflow: ok (${#files[@]} file(s))"
