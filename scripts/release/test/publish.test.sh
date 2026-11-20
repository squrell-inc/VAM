#!/usr/bin/env bash
# publish.sh against the fake gh (test/fake-gh.sh): what `check` refuses, the draft, the publish and
# the abort, including that nothing is ever created after a refused check, that a failure before
# publish deletes only this run's draft, and that a failure after publish never deletes anything.
# No network, no real gh.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"
. "$(dirname "$0")/publish-world.sh"

tag_of() { echo "v$VERSION"; }

echo "the happy path, an rc"
new_world
pub check
expect_eq "check passes" 0 "$T_RC"
expect_match "check says ok for the tag and commit" "ok for v0\.1\.0-rc\.1 at $C1" "$T_OUT"
pub draft
expect_eq "draft passes" 0 "$T_RC"
expect_match "the draft is created at the commit, as a draft, titled, a pre-release and not latest" \
  "release create v0\.1\.0-rc\.1 --repo acme/vam --target $C1 --draft --title v0\.1\.0-rc\.1 --notes-file .*RELEASE_NOTES\.md --prerelease --latest=false" "$(calls)"
expect_eq "the draft's id is recorded" "100" "$(cat "$W/state/release-id")"
expect_eq "every file is uploaded (assets, SHA256SUMS, notes)" "9" "$(jq '.[0].assets | length' "$W/gh/releases.json")"
expect_eq "the release is a draft" "true" "$(jq -r '.[0].draft' "$W/gh/releases.json")"
expect_eq "no tag exists yet" "0" "$(jq length "$W/gh/tags.json")"
ver draft
expect_eq "the draft verifies" 0 "$T_RC"
expect_eq "nine attestation checks" "9" "$(calls | grep -c 'attestation verify /')"
expect_match "each names the repository, the signer workflow and main" "attestation verify .* -R acme/vam --signer-workflow acme/vam/\.github/workflows/release\.yml --source-ref refs/heads/main" "$(calls)"
expect_nomatch "no source digest is demanded (the dispatch commit is not the released commit)" "--source-digest" "$(calls)"
pub publish
expect_eq "publish passes" 0 "$T_RC"
expect_match "the release is patched out of draft, not latest" "api -X PATCH repos/acme/vam/releases/100 -F draft=false -f make_latest=false" "$(calls)"
expect_eq "the release is public" "false" "$(jq -r '.[0].draft' "$W/gh/releases.json")"
expect_eq "GitHub created the tag at the commit" "$C1" "$(jq -r '.[0].object.sha' "$W/gh/tags.json")"
run_cmd env "$verify" release --dir "$DIST"
expect_eq "the published release verifies" 0 "$T_RC"
expect_eq "gh release verify-asset runs once per file" "9" "$(calls | grep -c 'release verify-asset v[^ ]* /')"
expect_match "gh release verify runs on the tag" "release verify v0\.1\.0-rc\.1 -R acme/vam" "$(calls)"

echo "the happy path, a final"
new_world 0.1.0
pub check; pub draft
expect_match "a final is created with --latest and no --prerelease" "release create v0\.1\.0 .* --latest\$" "$(calls)"
expect_nomatch "and not as a pre-release" "--prerelease" "$(calls)"
pub publish
expect_match "a final is made latest when published" "-F draft=false -f make_latest=true" "$(calls)"

echo "an annotated tag is dereferenced"
new_world
pub check; pub draft
pub publish FAKE_ANNOTATED=1
expect_eq "an annotated tag that points at the commit passes" 0 "$T_RC"

echo "check refuses, and nothing is created"
refused() { # <name> <regex> [VAR=value ...]
  local name="$1" pat="$2"; shift 2
  new_world; pub check "$@"
  if [ "$T_RC" = 0 ]; then bad "$name (check passed)" "$T_OUT"
  elif ! grep -Eq -- "$pat" <<< "$T_OUT"; then bad "$name (refused, but not with /$pat/)" "$T_OUT"
  elif calls | grep -q 'release create\|-X PATCH\|-X DELETE'; then bad "$name (something was created or changed)" "$(calls)"
  else ok "$name"; fi
  pub draft "$@"
  expect_fail_rc "$name: draft refuses without a passed check" "$T_RC" "$T_OUT" "has not passed"
}
expect_fail_rc() { # <name> <rc> <out> <regex>
  if [ "$2" != 0 ] && grep -Eq -- "$4" <<< "$3"; then ok "$1"; else bad "$1 (rc $2)" "$3"; fi
}
refused "validate and build disagree on the commit" "validate checked $C2 but build built $C1" VALIDATED_COMMIT="$C2"
refused "an empty validate commit fails closed" "validate produced no valid commit" VALIDATED_COMMIT=
refused "a short validate commit" "validate produced no valid commit" VALIDATED_COMMIT=abc123
refused "an empty build commit" "build produced no valid commit" COMMIT=
refused "validate approved another version" "validate approved version '0\.1\.0' but build built" VALIDATED_VERSION=0.1.0
refused "the dispatch asked for another version" "the dispatch asked for '0\.2\.0'" INPUT_VERSION=0.2.0
refused "a malformed version" "not a release version" VERSION=banana
refused "an empty run id" "GITHUB_RUN_ID is empty" GITHUB_RUN_ID=
refused "a gh without release verify" "no 'release verify'" FAKE_NO_RELEASE_VERIFY=1
refused "a gh without --source-ref" "no --source-ref" FAKE_NO_SOURCE_REF=1
refused "the release directory is missing" "does not exist" DIST="$t_tmp/nowhere"

# artifact problems: edit the world, then run check
art() { # <name> <regex> <shell snippet run in $W/dist>
  local name="$1" pat="$2" snippet="$3"
  new_world; (cd "$W/dist" && eval "$snippet")
  pub check
  if [ "$T_RC" = 0 ]; then bad "$name (check passed)" "$T_OUT"
  elif ! grep -Eq -- "$pat" <<< "$T_OUT"; then bad "$name (refused, but not with /$pat/)" "$T_OUT"
  elif calls | grep -q 'release create'; then bad "$name (a release was created)"
  else ok "$name"; fi
}
bi() { printf 'jq --arg x "%s" ".%s = \\$x" build-info.json > b.new && mv b.new build-info.json' "$1" "$2"; }
art "a dev-channel build" "channel 'dev'" "$(bi dev channel)"
art "another run's artifact" "names run '123', not this run \(777\)" "$(bi 123 run_id)"
art "build-info naming another commit" "build-info.json names commit" "$(bi "$C2" commit)"
art "build-info naming another version" "build-info.json names version" "$(bi 0.1.0 version)"
art "a missing build-info.json" "build-info.json is missing" "rm build-info.json"
art "a tampered asset" "SHA256SUMS does not verify" "echo tamper >> vam-0.1.0-rc.1-aarch64-apple-darwin.tar.gz"
art "a missing asset" "does not hold exactly the expected files" "rm vam-runtime-0.1.0-rc.1-x86_64-unknown-linux-musl"
art "an extra file" "does not hold exactly the expected files.*extra\.bin" "echo x > extra.bin"
art "an empty asset" "missing, empty or not a regular file" ": > vam-runtime-0.1.0-rc.1-x86_64-unknown-linux-musl; sha256sum vam-runtime-0.1.0-rc.1-x86_64-unknown-linux-musl >/dev/null"
art "SHA256SUMS missing an entry" "does not list exactly the release assets" "sed -i '1d' SHA256SUMS"
art "notes of another tag" "not the notes of v0\.1\.0-rc\.1" "echo '# VAM v9.9.9' > RELEASE_NOTES.md"

echo "tag and release must be absent (drafts included)"
new_world
jq -n --arg r "refs/tags/v0.1.0-rc.1" --arg s "$C2" '[{ref: $r, object: {sha: $s, type: "commit"}}]' > "$W/gh/tags.json"
pub check
expect_eq "an existing tag refuses" 1 "$T_RC"; expect_match "and says the number is burned" "tag v0\.1\.0-rc\.1 already exists" "$T_OUT"
new_world
jq -n --arg r "refs/tags/v0.1.0-rc.10" --arg s "$C2" '[{ref: $r, object: {sha: $s, type: "commit"}}]' > "$W/gh/tags.json"
pub check
expect_eq "a longer tag that only shares the prefix does not refuse" 0 "$T_RC"
new_world
jq -n '[{id: 5, tag_name: "v0.1.0-rc.1", draft: false, html_url: "https://x/5", assets: []}]' > "$W/gh/releases.json"
pub check
expect_eq "a published release refuses" 1 "$T_RC"; expect_match "naming it" "a release for v0\.1\.0-rc\.1 already exists" "$T_OUT"
new_world
jq -n '[{id: 5, tag_name: "v0.1.0-rc.1", draft: true, html_url: "https://x/5", assets: []}]' > "$W/gh/releases.json"
pub check
expect_eq "a draft someone left refuses" 1 "$T_RC"; expect_match "saying it is a draft" "draft: true" "$T_OUT"
pub abort
expect_eq "abort leaves a draft this run did not create alone" "1" "$(jq length "$W/gh/releases.json")"
expect_nomatch "and never calls DELETE" "-X DELETE" "$(calls)"
new_world; FAKE_FAIL=tags pub check FAKE_FAIL=tags
expect_match "an unreadable tag list refuses (closed)" "cannot read the tags" "$T_OUT"
new_world; pub check FAKE_FAIL=list
expect_match "an unreadable release list refuses (closed)" "cannot read the releases" "$T_OUT"

echo "the draft"
new_world; pub check; pub draft FAKE_FAIL=create
expect_eq "a failed create fails the step" 1 "$T_RC"; [ ! -e "$W/state/release-id" ] && ok "no id is recorded" || bad "an id was recorded"
pub abort
expect_match "abort says nothing was created and the number may run again" "created no draft.*may run again" "$T_OUT"
expect_nomatch "and deletes nothing" "-X DELETE" "$(calls)"
new_world; pub check; pub draft FAKE_FAIL=upload
expect_eq "a failed upload fails the step" 1 "$T_RC"; expect_eq "but the id was recorded first" "100" "$(cat "$W/state/release-id")"
pub abort
expect_match "abort deletes the draft" "deleted draft 100.*same number may run again" "$T_OUT"
expect_eq "no release is left" "0" "$(jq length "$W/gh/releases.json")"
expect_eq "no tag was created" "0" "$(jq length "$W/gh/tags.json")"
new_world; pub check; pub draft FAKE_PENDING=SHA256SUMS
expect_match "an asset that is not uploaded stops the draft" "not complete after the upload.*not uploaded" "$T_OUT"
new_world; pub check
jq -n '[{id: 5, tag_name: "v0.1.0-rc.1", draft: true, html_url: "https://x/5", assets: []}]' > "$W/gh/releases.json"
pub draft
expect_match "two releases for the tag after the create: nothing is recorded" "found 2" "$T_OUT"
[ ! -e "$W/state/release-id" ] && ok "so abort cannot touch either" || bad "an id was recorded"
new_world; pub check; pub draft
pub draft
expect_match "a second draft in one run is refused" "already created by this run" "$T_OUT"

echo "publish re-checks before it publishes"
prepare_draft() { new_world; pub check; pub draft; }
prepare_draft; set_release 'map(.draft = false)'
pub publish
expect_match "a release that is no longer a draft is not published again" "no longer a draft" "$T_OUT"
expect_nomatch "no PATCH" "-X PATCH" "$(calls)"
pub abort
expect_match "abort calls a published release burned" "already published.*Do not reuse" "$T_OUT"
expect_nomatch "and deletes nothing" "-X DELETE" "$(calls)"
prepare_draft; set_release 'map(.target_commitish = "'"$C2"'")'
pub publish
expect_match "a draft that now targets another commit is refused" "targets $C2, not $C1" "$T_OUT"
expect_nomatch "no PATCH (target)" "-X PATCH" "$(calls)"
prepare_draft; set_release 'map(.assets |= map(select(.name != "SHA256SUMS")))'
pub publish
expect_match "a draft missing an asset is refused" "not exactly the expected files" "$T_OUT"
prepare_draft; set_release 'map(.assets[0].state = "starter")'
pub publish
expect_match "a draft with an unfinished asset is refused" "assets not uploaded" "$T_OUT"
prepare_draft; jq -n --arg r "refs/tags/v0.1.0-rc.1" --arg s "$C2" '[{ref: $r, object: {sha: $s, type: "commit"}}]' > "$W/gh/tags.json"
pub publish
expect_match "a tag that appeared since the checks stops the publish" "tag v0\.1\.0-rc\.1 appeared" "$T_OUT"
expect_nomatch "no PATCH (tag)" "-X PATCH" "$(calls)"
pub abort
expect_match "and abort deletes the draft (the tag is not ours)" "deleted draft 100" "$T_OUT"
prepare_draft; set_release '. + [{id: 9, tag_name: "v0.1.0-rc.1", draft: true, assets: []}]'
pub publish
expect_match "a second release for the tag stops the publish" "more than one release uses v0\.1\.0-rc\.1" "$T_OUT"
new_world; pub publish
expect_match "publish without a recorded draft refuses" "no draft was recorded" "$T_OUT"

echo "publish fails"
prepare_draft; pub publish FAKE_FAIL=patch
expect_eq "a failed publish call fails the step" 1 "$T_RC"
expect_match "and says do not reuse until looked at" "do not reuse" "$T_OUT"
prepare_draft; pub publish FAKE_NO_TAG=1
expect_match "a publish without a tag is called burned" "tag v0\.1\.0-rc\.1 does not exist: do not reuse" "$T_OUT"
pub abort
expect_nomatch "abort never deletes a published release (no tag)" "-X DELETE" "$(calls)"
prepare_draft; pub publish FAKE_TAG_SHA="$C2"
expect_match "a tag at another commit is called burned" "points at $C2, not $C1: do not reuse" "$T_OUT"
pub abort
expect_match "abort says the number is burned" "already published" "$T_OUT"
expect_eq "the release is still there" "1" "$(jq length "$W/gh/releases.json")"

echo "a failure while verifying the draft deletes it"
# a file corrupted on upload: the fault is active during draft
new_world; pub check; pub draft FAKE_CORRUPT=SHA256SUMS
ver draft
expect_eq "a corrupted upload fails the draft verification" 1 "$T_RC"
expect_match "naming the file" "SHA256SUMS: the draft's copy .* differs from the built file" "$T_OUT"
pub abort
expect_match "abort deletes the draft: the same number may run again" "deleted draft 100.*same number may run again" "$T_OUT"
new_world; pub check; pub draft
ver draft FAKE_ATTEST_FAIL_NAME=vam-runtime-0.1.0-rc.1-x86_64-unknown-linux-musl
expect_eq "a missing attestation fails the draft verification" 1 "$T_RC"
expect_match "naming the file" "vam-runtime-0\.1\.0-rc\.1.*gh attestation verify failed" "$T_OUT"
new_world; pub check; pub draft FAKE_LIST_LAG=3
expect_eq "a draft the release list shows only on the fourth look is found" 0 "$T_RC"
expect_match "…after saying it is not listed yet" "not listed yet \(attempt 3 of 12\)" "$T_OUT"
[ -s "$W/state/release-id" ] && ok "…and its id is recorded" || bad "no release id recorded after the lagging list"
new_world; pub check; pub draft FAKE_LIST_LAG=99 PUBLISH_FIND_TRIES=4
expect_eq "a draft the list never shows fails after the tries" 1 "$T_RC"
expect_match "…saying found 0 and to inspect the draft" "found 0; nothing is recorded, inspect the draft by hand" "$T_OUT"
expect_eq "…after four listings of the releases following the create" "4" "$(calls | sed -n '/release create/,$p' | grep -c 'releases?per_page')"
new_world; pub check; pub draft
ver draft FAKE_ATTEST_FLAKY=2
expect_eq "an attestation that appears on the third try passes" 0 "$T_RC"
new_world; pub check; pub draft
ver draft FAKE_ATTEST_FLAKY=99
expect_eq "one that never appears fails after three tries" 1 "$T_RC"
expect_eq "three tries for each of the nine files" "27" "$(calls | grep -c 'attestation verify /')"

echo "a failure after publish never deletes"
prepare_draft; pub publish
run_cmd env FAKE_FAIL=release_verify "$verify" release --dir "$DIST"
expect_eq "a failing gh release verify fails the step" 1 "$T_RC"
pub abort
expect_match "abort says the number is burned" "already published.*Do not reuse" "$T_OUT"
expect_nomatch "and deletes nothing" "-X DELETE" "$(calls)"
expect_eq "the release is still public" "false" "$(jq -r '.[0].draft' "$W/gh/releases.json")"
prepare_draft; pub publish
run_cmd env FAKE_FAIL=verify_asset "$verify" release --dir "$DIST"
expect_eq "a failing verify-asset fails the step" 1 "$T_RC"

echo "abort is careful"
new_world; pub check
jq -n '[{id: 5, tag_name: "v0.0.9", draft: false, html_url: "https://x/5", assets: []}]' > "$W/gh/releases.json"
echo 5 > "$W/state/release-id"
pub abort
expect_nomatch "abort never deletes a published release, even by a recorded id" "-X DELETE" "$(calls)"
echo 101 > "$W/state/release-id"
pub abort
expect_match "a recorded release that is gone is left alone" "no longer exists" "$T_OUT"
echo banana > "$W/state/release-id"
pub abort
expect_match "a corrupt id deletes nothing" "not a number; deleting nothing" "$T_OUT"
expect_eq "abort exits 0 (a cleanup step must not mask the real failure)" 0 "$T_RC"
prepare_draft; pub abort FAKE_FAIL=get_release
expect_match "when the state cannot be read nothing is deleted" "deleting nothing" "$T_OUT"
expect_nomatch "no DELETE then" "-X DELETE" "$(calls)"
prepare_draft; pub abort FAKE_FAIL=delete
expect_match "a failed delete says to delete by hand" "could not delete draft 100" "$T_OUT"
finish
