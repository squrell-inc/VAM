#!/usr/bin/env bash
# A fixture world for publish.test.sh and verify.test.sh (sourced after helpers.sh): a release
# directory as `assemble` writes it, the fake gh (fake-gh.sh) on PATH, and the environment the
# publish job passes. Not a test itself.
# shellcheck source=scripts/release/lib.sh
. "$release_dir/lib.sh"

publish="$release_dir/publish.sh"
verify="$release_dir/verify.sh"
C1="$(printf '1%.0s' $(seq 40))"
C2="$(printf '2%.0s' $(seq 40))"

# new_world [version]: a fresh release directory, state and fake gh; exports the job's environment
new_world() {
  local v="${1:-0.1.0-rc.1}" a
  W="$t_tmp/world"; rm -rf "$W"; mkdir -p "$W/bin" "$W/dist" "$W/state" "$W/gh"
  cp "$release_dir/test/fake-gh.sh" "$W/bin/gh"; chmod +x "$W/bin/gh"
  while IFS= read -r a; do printf 'content of %s\n' "$a" > "$W/dist/$a"; done < <(release_assets "$v")
  (cd "$W/dist" && release_assets "$v" | LC_ALL=C sort | xargs sha256sum > SHA256SUMS)
  printf '# VAM v%s\n\nnotes\n' "$v" > "$W/dist/RELEASE_NOTES.md"
  jq -n --arg v "$v" --arg c "$C1" '{version: $v, commit: $c, channel: "release", run_id: "777", assets: []}' > "$W/dist/build-info.json"
  export PATH="$W/bin:$PATH" FAKE_GH_DIR="$W/gh" RETRY_SLEEP=0 PUBLISH_FIND_SLEEP=0
  export REPO=acme/vam VERSION="$v" COMMIT="$C1" VALIDATED_COMMIT="$C1" VALIDATED_VERSION="$v" INPUT_VERSION="$v"
  export DIST="$W/dist" GITHUB_RUN_ID=777 PUBLISH_STATE="$W/state"
  unset FAKE_FAIL FAKE_NO_RELEASE_VERIFY FAKE_NO_SOURCE_REF FAKE_CORRUPT FAKE_PENDING FAKE_NO_TAG FAKE_TAG_SHA FAKE_ANNOTATED FAKE_ATTEST_FAIL_NAME FAKE_ATTEST_FLAKY FAKE_LIST_LAG
  : > "$W/gh/calls.log"
}
# pub <step> [VAR=value ...]: runs publish.sh <step>, with extra environment, capturing $T_OUT and $T_RC
pub() { local step="$1"; shift; run_cmd env "$@" "$publish" "$step"; }
ver() { local step="$1"; shift; run_cmd env "$@" "$verify" "$step"; }
calls() { cat "$W/gh/calls.log"; }
# releases_json: the fake's releases
releases_json() { cat "$W/gh/releases.json"; }
# set_release <jq filter>: edits the fake's releases
set_release() { jq "$1" "$W/gh/releases.json" > "$W/gh/r.new" && mv "$W/gh/r.new" "$W/gh/releases.json"; }
