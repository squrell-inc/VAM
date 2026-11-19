#!/usr/bin/env bash
# verify.sh against the fake gh: the public `release` mode (what the owner runs after a publish) and
# the usage errors. The draft mode is exercised with the publish flow in publish.test.sh.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"
. "$(dirname "$0")/publish-world.sh"

published_world() { new_world "${1:-0.1.0-rc.1}"; pub check >/dev/null; pub draft >/dev/null; pub publish >/dev/null; }

echo "release mode, files downloaded"
published_world
run_cmd env GITHUB_STEP_SUMMARY="$W/summary.md" "$verify" release 0.1.0-rc.1 -R acme/vam --attest
expect_eq "a published release verifies" 0 "$T_RC"
expect_match "it says so" "all checks passed for v0\.1\.0-rc\.1" "$T_OUT"
expect_match "the assets were downloaded" "release download v0\.1\.0-rc\.1 -R acme/vam -D" "$(calls)"
expect_eq "verify-asset ran for all eight files" "8" "$(calls | grep -c 'release verify-asset v[^ ]* /')"
expect_eq "attestation verify ran for all eight files" "8" "$(calls | grep -c 'attestation verify /')"
expect_match "with the repository's release workflow as signer" "--signer-workflow acme/vam/\.github/workflows/release\.yml --source-ref refs/heads/main" "$(calls)"
expect_match "the summary has a row per file" "\| \`SHA256SUMS\` \| ok, attestation verified \|" "$(cat "$W/summary.md")"

echo "without --attest no attestation is checked"
published_world
: > "$W/gh/calls.log"
run_cmd env FAKE_FAIL=attest "$verify" release 0.1.0-rc.1 -R acme/vam
expect_eq "passes even where attestations would fail" 0 "$T_RC"
expect_eq "and ran none" "0" "$(calls | grep -c 'attestation verify /')"
run_cmd env FAKE_FAIL=attest "$verify" release 0.1.0-rc.1 -R acme/vam --attest
expect_eq "--attest fails when an attestation is missing" 1 "$T_RC"
expect_match "naming the check" "gh attestation verify failed" "$T_OUT"

echo "release mode refuses"
new_world; pub check >/dev/null; pub draft >/dev/null
run_cmd env "$verify" release 0.1.0-rc.1 -R acme/vam --dir "$DIST"
expect_eq "a draft is not a verified release" 1 "$T_RC"
published_world
echo tamper >> "$DIST/vam-runtime-0.1.0-rc.1-x86_64-unknown-linux-musl"
run_cmd env "$verify" release 0.1.0-rc.1 -R acme/vam --dir "$DIST"
expect_eq "a local file that is not the released one fails verify-asset" 1 "$T_RC"
expect_match "naming it" "verify-asset v0\.1\.0-rc\.1 vam-runtime-0\.1\.0-rc\.1.* failed" "$T_OUT"
published_world
rm "$DIST/vam-0.1.0-rc.1-x86_64-apple-darwin.tar.gz"
run_cmd env "$verify" release 0.1.0-rc.1 -R acme/vam --dir "$DIST"
expect_match "a file missing from --dir is reported" "vam-0\.1\.0-rc\.1-x86_64-apple-darwin\.tar\.gz is not in" "$T_OUT"
published_world
run_cmd env FAKE_FAIL=release_verify "$verify" release 0.1.0-rc.1 -R acme/vam --dir "$DIST"
expect_match "a failing gh release verify is reported" "gh release verify v0\.1\.0-rc\.1 failed" "$T_OUT"
expect_eq "the three tries were made" "3" "$(calls | grep -c 'release verify v0.1.0-rc.1 -R')"

echo "usage"
new_world
run_cmd "$verify" release 0.1.0-rc.1 -R nope
expect_match "a bad repository" "not owner/name" "$T_OUT"
run_cmd env -u VERSION "$verify" release -R acme/vam
expect_match "no version" "not a release version" "$T_OUT"
run_cmd "$verify" release 1.0 -R acme/vam
expect_match "a malformed version" "not a release version" "$T_OUT"
run_cmd "$verify" release 0.1.0 -R acme/vam --nope
expect_match "an unknown option" "unknown option" "$T_OUT"
run_cmd "$verify" banana
expect_match "an unknown mode" "usage: verify.sh" "$T_OUT"
run_cmd env DIST="$W/dist" PUBLISH_STATE="$W/empty" "$verify" draft
expect_match "draft mode without a recorded draft" "no draft was recorded" "$T_OUT"
finish
