#!/usr/bin/env bash
# The publish steps of .github/workflows/release.yml's `publish` job (design/release-artifacts.md
# §4.1, §4.3, §5.4; locked decisions 6, 7, 9, 10). The workflow runs them in this order, with the
# attestation steps between `draft` and `publish`:
#
#   publish.sh check     refuse unless everything agrees and the tag and release are absent
#   publish.sh draft     create the draft release at the commit, upload every file, record its id
#   publish.sh publish   re-check the draft, publish it (GitHub creates the tag now), check the tag
#   publish.sh abort     on failure: delete the draft this run created, or say the number is burned
#
# Inputs come from the environment only (the workflow passes them through `env:`):
#   REPO                owner/name
#   VERSION             the version `build` built (needs.build.outputs.version)
#   COMMIT              the commit `build` built (needs.build.outputs.commit)
#   VALIDATED_COMMIT    the commit `validate` approved (needs.validate.outputs.commit)
#   VALIDATED_VERSION   the version `validate` approved
#   INPUT_VERSION       the version the dispatch asked for
#   DIST                the downloaded release-<version> artifact (assets, SHA256SUMS,
#                       RELEASE_NOTES.md, build-info.json)
#   GITHUB_RUN_ID       this run (build-info.json must name it)
#   GH_TOKEN            the job's GITHUB_TOKEN (contents: write)
#   PUBLISH_STATE       a directory for this run's state, default $RUNNER_TEMP/publish-state
# Also GITHUB_STEP_SUMMARY and GITHUB_OUTPUT when set. Needs bash, jq, sha256sum and `gh`.
#
# `check` runs every check and reports each failure; it refuses (exit 1) on any, and an empty
# `validate` commit fails closed. `draft` refuses to run unless `check` passed in this job. The id
# of the draft is written to $PUBLISH_STATE/release-id before the first upload; `abort` deletes only
# the release with that id, only while it is still a draft, so it can never touch a release another
# run or a person made, or a published one.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release/lib.sh
. "$here/lib.sh"

die() { echo "publish: $*" >&2; exit 1; }
REPO="${REPO:-}"; VERSION="${VERSION:-}"; COMMIT="${COMMIT:-}"
VALIDATED_COMMIT="${VALIDATED_COMMIT:-}"; VALIDATED_VERSION="${VALIDATED_VERSION:-}"; INPUT_VERSION="${INPUT_VERSION:-}"
DIST="${DIST:-}"; RUN_ID="${GITHUB_RUN_ID:-}"
state="${PUBLISH_STATE:-${RUNNER_TEMP:-/tmp}/publish-state}"
tag="v$VERSION"

command -v jq >/dev/null || die "jq is required"
command -v gh >/dev/null || die "gh is required"
[[ "$REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "REPO '$REPO' is not owner/name"
mkdir -p "$state"

summary() { if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then printf '%s\n' "$1" >> "$GITHUB_STEP_SUMMARY"; fi; }
gh_error() { [ -n "${GITHUB_ACTIONS:-}" ] && printf '::error title=publish::%s\n' "$1"; return 0; }

# prerelease and latest handling by version: an rc is a pre-release and never "latest" (decision 1)
is_rc=false; case "$VERSION" in *-rc.*) is_rc=true ;; esac

# expected_files: the release's files (assets, SHA256SUMS, RELEASE_NOTES.md), one per line
expected_files() { release_files "$VERSION"; }

# check_draft_state <release json>: the draft is ours, at the commit, complete. Prints each problem.
draft_problems() {
  local json="$1" want have
  jq -r --arg tag "$tag" --arg commit "$COMMIT" --argjson rc "$is_rc" '
    (if .draft != true then "the release is no longer a draft" else empty end),
    (if .tag_name != $tag then "the release is for tag \(.tag_name), not \($tag)" else empty end),
    (if .target_commitish != $commit then "the draft targets \(.target_commitish), not \($commit)" else empty end),
    (if (.prerelease // false) != $rc then "the draft has prerelease=\(.prerelease), expected \($rc)" else empty end),
    ([.assets[]? | select(.state != "uploaded") | .name] | if length > 0 then "assets not uploaded: \(join(", "))" else empty end)' <<< "$json"
  want="$(expected_files | LC_ALL=C sort)"
  have="$(jq -r '.assets[]?.name' <<< "$json" | LC_ALL=C sort)"
  [ "$want" = "$have" ] || echo "the draft's assets are not exactly the expected files (have: $(printf '%s' "$have" | tr '\n' ' '); want: $(printf '%s' "$want" | tr '\n' ' '))"
}

# ------------------------------------------------------------------------------------------------
cmd_check() {
  local failures=0 json
  fail() { failures=$((failures + 1)); printf 'publish check: FAIL %s\n' "$1" >&2; gh_error "$1"; }

  # the gh on this runner must know the commands the publish relies on, before anything is created
  gh release verify --help >/dev/null 2>&1 || fail "this gh has no 'release verify' (immutable release verification); a newer gh is needed ($(gh --version 2>&1 | head -n 1))"
  gh release verify-asset --help >/dev/null 2>&1 || fail "this gh has no 'release verify-asset'"
  gh attestation verify --help 2>&1 | grep -q -- '--source-ref' || fail "this gh's 'attestation verify' has no --source-ref"

  # one commit and one version through validate, build and the artifact
  [[ "$VALIDATED_COMMIT" =~ ^[0-9a-f]{40}$ ]] || fail "validate produced no valid commit ('${VALIDATED_COMMIT:-<empty>}'); nothing is published without the guards' commit"
  [[ "$COMMIT" =~ ^[0-9a-f]{40}$ ]] || fail "build produced no valid commit ('${COMMIT:-<empty>}')"
  if [ "$VALIDATED_COMMIT" != "$COMMIT" ]; then
    fail "validate checked $VALIDATED_COMMIT but build built $COMMIT (main may have moved during the run: dispatch again, or pass the commit)"
  fi
  semver_valid "$VERSION" || fail "build's version '$VERSION' is not a release version"
  [ "$VALIDATED_VERSION" = "$VERSION" ] || fail "validate approved version '$VALIDATED_VERSION' but build built '$VERSION'"
  [ "$INPUT_VERSION" = "$VERSION" ] || fail "the dispatch asked for '$INPUT_VERSION' but build built '$VERSION'"
  [ -n "$RUN_ID" ] || fail "GITHUB_RUN_ID is empty"

  # the artifact
  if [ ! -d "$DIST" ]; then
    fail "the release directory '$DIST' does not exist"
  else
    if [ -f "$DIST/build-info.json" ]; then
      json="$(cat "$DIST/build-info.json")"
      jq -e . >/dev/null 2>&1 <<< "$json" || fail "build-info.json is not JSON"
      [ "$(jq -r '.version // ""' <<< "$json" 2>/dev/null)" = "$VERSION" ] || fail "build-info.json names version '$(jq -r '.version // ""' <<< "$json" 2>/dev/null)', not $VERSION"
      [ "$(jq -r '.commit // ""' <<< "$json" 2>/dev/null)" = "$COMMIT" ] || fail "build-info.json names commit '$(jq -r '.commit // ""' <<< "$json" 2>/dev/null)', not $COMMIT"
      [ "$(jq -r '.channel // ""' <<< "$json" 2>/dev/null)" = release ] || fail "build-info.json says channel '$(jq -r '.channel // ""' <<< "$json" 2>/dev/null)': only a release-channel build is published"
      [ "$(jq -r '.run_id // ""' <<< "$json" 2>/dev/null)" = "$RUN_ID" ] && [ -n "$RUN_ID" ] || fail "build-info.json names run '$(jq -r '.run_id // ""' <<< "$json" 2>/dev/null)', not this run ($RUN_ID)"
    else
      fail "build-info.json is missing from $DIST"
    fi
    local want have f
    want="$( { expected_files; echo build-info.json; } | LC_ALL=C sort)"
    have="$(ls -A "$DIST" | LC_ALL=C sort)"
    if [ "$want" != "$have" ]; then
      fail "$DIST does not hold exactly the expected files (missing: $(comm -23 <(printf '%s\n' "$want") <(printf '%s\n' "$have") | tr '\n' ' '); unexpected: $(comm -13 <(printf '%s\n' "$want") <(printf '%s\n' "$have") | tr '\n' ' '))"
    fi
    while IFS= read -r f; do
      [ -f "$DIST/$f" ] && [ -s "$DIST/$f" ] || fail "$f is missing, empty or not a regular file"
    done < <(expected_files)
    if [ -f "$DIST/SHA256SUMS" ]; then
      (cd "$DIST" && sha256sum -c SHA256SUMS >/dev/null 2>&1) || fail "SHA256SUMS does not verify the files in $DIST"
      [ "$(awk '{print $2}' "$DIST/SHA256SUMS" | LC_ALL=C sort)" = "$(release_assets "$VERSION" | LC_ALL=C sort)" ] || fail "SHA256SUMS does not list exactly the release assets"
    fi
    if [ -f "$DIST/RELEASE_NOTES.md" ]; then
      grep -q "^# VAM $tag\$" "$DIST/RELEASE_NOTES.md" || fail "RELEASE_NOTES.md is not the notes of $tag"
    fi
  fi

  # guard 5 again, with a token that can see drafts: no tag, no release of any kind
  if json="$(gh_tag_refs "$tag" 2>&1)"; then
    [ "$(jq 'length' <<< "$json")" = 0 ] || fail "tag $tag already exists; an immutable release burns its tag name, take the next number"
  else
    fail "cannot read the tags from the API: $(printf '%s' "$json" | head -n 3 | tr '\n' ' ')"
  fi
  if json="$(gh_releases 2>&1)"; then
    jq -e --arg t "$tag" 'map(select(.tag_name == $t)) | length == 0' >/dev/null 2>&1 <<< "$json" \
      || fail "a release for $tag already exists ($(jq -r --arg t "$tag" 'map(select(.tag_name == $t)) | .[0] | "draft: \(.draft), \(.html_url)"' <<< "$json" 2>/dev/null)); take the next number"
  else
    fail "cannot read the releases from the API: $(printf '%s' "$json" | head -n 3 | tr '\n' ' ')"
  fi

  if [ "$failures" -ne 0 ]; then
    summary "### publish: checks failed for \`$tag\`"
    summary "$failures check(s) failed; nothing was created. See the log."
    echo "publish check: $failures failure(s); nothing was created" >&2
    exit 1
  fi
  rm -f "$state/release-id" "$state/published"
  printf '%s %s\n' "$VERSION" "$COMMIT" > "$state/checked"
  echo "publish check: ok for $tag at $COMMIT"
  summary "### publish: \`$tag\` at \`$COMMIT\`"
  summary "Checks passed: one commit through validate and build, release-channel build of this run, SHA256SUMS verifies, exact asset set, no tag and no release (drafts included)."
}

# ------------------------------------------------------------------------------------------------
cmd_draft() {
  [ -f "$state/checked" ] && [ "$(cat "$state/checked")" = "$VERSION $COMMIT" ] || die "'publish.sh check' has not passed in this job for $VERSION at $COMMIT; refusing to create anything"
  [ ! -e "$state/release-id" ] || die "a draft was already created by this run ($(cat "$state/release-id"))"
  local flags=(--latest) f json id n
  if [ "$is_rc" = true ]; then flags=(--prerelease --latest=false); fi
  gh release create "$tag" --repo "$REPO" --target "$COMMIT" --draft --title "$tag" --notes-file "$DIST/RELEASE_NOTES.md" "${flags[@]}" \
    || die "gh release create failed"
  # The draft has no tag, so it is found by listing. The check proved no release of this tag existed and
  # the `release` concurrency group runs one release at a time, so exactly one draft is ours.
  # The release list can lag a just-created draft (seen on the first rc.1 run: found 0), so look again
  # for up to PUBLISH_FIND_TRIES x PUBLISH_FIND_SLEEP seconds before giving up.
  local tries="${PUBLISH_FIND_TRIES:-12}" pause="${PUBLISH_FIND_SLEEP:-5}" i=1
  while :; do
    json="$(gh_releases)" || die "cannot list the releases to find the draft just created"
    n="$(jq --arg t "$tag" '[.[] | select(.tag_name == $t)] | length' <<< "$json")"
    [ "$n" = 0 ] && [ "$i" -lt "$tries" ] || break
    echo "publish: the new draft is not listed yet (attempt $i of $tries); waiting ${pause}s" >&2
    sleep "$pause"; i=$((i + 1))
  done
  [ "$n" = 1 ] || die "expected exactly one release for $tag after creating the draft, found $n; nothing is recorded, inspect the draft by hand"
  id="$(jq -r --arg t "$tag" '.[] | select(.tag_name == $t) | .id' <<< "$json")"
  [[ "$id" =~ ^[0-9]+$ ]] || die "the draft has no numeric id"
  printf '%s\n' "$id" > "$state/release-id"
  [ -z "${GITHUB_OUTPUT:-}" ] || printf 'release_id=%s\n' "$id" >> "$GITHUB_OUTPUT"
  echo "publish draft: release id $id for $tag"
  json="$(gh_release_by_id "$id")" || die "cannot read the draft just created"
  [ "$(jq -r .draft <<< "$json")" = true ] || die "the release just created is not a draft; it must not be public yet"
  [ "$(jq -r .target_commitish <<< "$json")" = "$COMMIT" ] || die "the draft targets '$(jq -r .target_commitish <<< "$json")', not $COMMIT"
  local files=()
  while IFS= read -r f; do files+=("$DIST/$f"); done < <(expected_files)
  gh release upload "$tag" --repo "$REPO" "${files[@]}" || die "gh release upload failed"
  json="$(gh_release_by_id "$id")" || die "cannot read the draft after the upload"
  json_problems="$(draft_problems "$json")"
  [ -z "$json_problems" ] || die "the draft is not complete after the upload: $json_problems"
  summary "Draft release $id created at the commit with $(expected_files | wc -l | tr -d ' ') files uploaded."
}

# ------------------------------------------------------------------------------------------------
cmd_publish() {
  [ -f "$state/release-id" ] || die "no draft was recorded by this run; nothing to publish"
  local id json problems refs now
  id="$(cat "$state/release-id")"
  [[ "$id" =~ ^[0-9]+$ ]] || die "the recorded release id '$id' is not a number"
  # Still our draft, at the commit, complete, and the tag still absent: the last look before it is public.
  json="$(gh_release_by_id "$id")" || die "cannot read draft $id before publishing"
  problems="$(draft_problems "$json")"
  [ -z "$problems" ] || die "refusing to publish: $problems"
  json="$(gh_releases)" || die "cannot list the releases before publishing"
  [ "$(jq --arg t "$tag" '[.[] | select(.tag_name == $t)] | length' <<< "$json")" = 1 ] || die "refusing to publish: more than one release uses $tag"
  refs="$(gh_tag_refs "$tag")" || die "cannot read the tags before publishing"
  [ "$(jq 'length' <<< "$refs")" = 0 ] || die "refusing to publish: tag $tag appeared since the checks (a stray push would carry the release)"
  echo "publish: draft $id is ours, at $COMMIT, complete; no tag $tag; publishing"
  printf 'publishing\n' > "$state/published"
  local latest=true; [ "$is_rc" = true ] && latest=false
  gh api -X PATCH "repos/$REPO/releases/$id" -F draft=false -f "make_latest=$latest" >/dev/null || die "publishing release $id failed (it may or may not be public: do not reuse $tag until you have looked)"
  printf 'published\n' > "$state/published"
  # GitHub creates the tag now; it must point at the commit
  now="$(gh_release_by_id "$id")" || die "published, but cannot read the release back: do not reuse $tag"
  [ "$(jq -r .draft <<< "$now")" = false ] || die "the publish call returned but the release is still a draft"
  refs="$(gh_tag_refs "$tag")" || die "published, but cannot read the tag: do not reuse $tag"
  [ "$(jq 'length' <<< "$refs")" = 1 ] || die "published, but tag $tag does not exist: do not reuse $tag; inspect the release"
  local type sha
  type="$(jq -r '.[0].object.type' <<< "$refs")"; sha="$(jq -r '.[0].object.sha' <<< "$refs")"
  if [ "$type" = tag ]; then
    sha="$(gh api "repos/$REPO/git/tags/$sha" | jq -r '.object.sha')" || die "published, but cannot dereference the annotated tag: do not reuse $tag"
  fi
  [ "$sha" = "$COMMIT" ] || die "published, but tag $tag points at $sha, not $COMMIT: do not reuse $tag; prepare the next number"
  echo "publish: $tag is public and points at $COMMIT"
  summary "### Published \`$tag\`"
  summary "$(jq -r '.html_url' <<< "$now"): tag \`$tag\` created by GitHub at \`$COMMIT\`."
}

# ------------------------------------------------------------------------------------------------
cmd_abort() {
  local id json rc
  if [ ! -f "$state/release-id" ]; then
    echo "publish abort: this run created no draft; nothing to clean up (no tag was created, the same number may run again once the cause is fixed)"
    summary "### Release failed before a draft existed: the same number may run again after the fix."
    return 0
  fi
  id="$(cat "$state/release-id")"
  [[ "$id" =~ ^[0-9]+$ ]] || { echo "publish abort: the recorded release id '$id' is not a number; deleting nothing"; return 0; }
  json="$(gh_release_by_id "$id")"; rc=$?
  if [ "$rc" = 3 ]; then
    echo "publish abort: release $id no longer exists; nothing to delete"
    return 0
  elif [ "$rc" != 0 ]; then
    echo "publish abort: cannot read release $id ($json); deleting nothing. Look at the releases page before reusing $tag." >&2
    gh_error "cannot determine whether release $id is a draft; check by hand before reusing $tag"
    return 0
  fi
  if [ "$(jq -r .draft <<< "$json")" = true ]; then
    if gh api -X DELETE "repos/$REPO/releases/$id" >/dev/null; then
      echo "publish abort: deleted draft $id; no tag $tag was created, so the same number may run again after the fix"
      summary "### Release failed before publish: draft $id deleted, no tag created. The same number may run again after the fix."
    else
      echo "publish abort: could not delete draft $id; delete it by hand before running $tag again" >&2
      gh_error "draft $id could not be deleted; delete it by hand before dispatching $tag again"
    fi
  else
    echo "publish abort: release $id is already published. Do not reuse $tag and do not delete or move the tag; prepare the next number." >&2
    gh_error "$tag is published but a later step failed: do not reuse this number; prepare the next"
    summary "### Release failed after publish: do not reuse \`$tag\`; prepare the next number (decision 6)."
  fi
  return 0
}

case "${1:-}" in
  check) cmd_check ;;
  draft) cmd_draft ;;
  publish) cmd_publish ;;
  abort) cmd_abort ;;
  *) die "usage: publish.sh check|draft|publish|abort (inputs in the environment)" ;;
esac
