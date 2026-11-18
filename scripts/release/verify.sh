#!/usr/bin/env bash
# Verification for the publish job and for people (design/release-artifacts.md §5.4, §9).
#
#   verify.sh draft
#       The publish job, after the attestation steps and before publishing: download every file of
#       the draft this run created, compare each with the release directory and with SHA256SUMS, and
#       run `gh attestation verify` on each. Env: REPO, VERSION, DIST, PUBLISH_STATE (publish.sh's
#       state directory, which holds the draft's id).
#   verify.sh release [VERSION] [-R OWNER/NAME] [--dir DIR] [--attest]
#       A published release: `gh release verify v<VERSION>` and `gh release verify-asset` for every
#       file. The files come from --dir (the publish job passes the release directory) or are
#       downloaded. --attest also runs `gh attestation verify` on each (the owner's checklist).
#       VERSION and the repository default to $VERSION and $REPO.
#         scripts/release/verify.sh release 0.1.0-rc.1 -R OWNER/NAME --attest
#
# Attestations are checked against the repository, the signer workflow
# <repo>/.github/workflows/release.yml and the source ref refs/heads/main (guard 1: releases run
# only from main). The source digest is not checked: it is the dispatch commit (the head of main),
# which is not the commit being released when `commit` is given. The released commit is bound by
# publish.sh instead (build-info.json, the draft's target and the tag).
# Both a draft's attestations and a release's attestation can take a moment to appear, so each check
# retries 3 times, 10 s apart (RETRY_SLEEP overrides the wait).
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release/lib.sh
. "$here/lib.sh"
die() { echo "verify: $*" >&2; exit 1; }
command -v jq >/dev/null || die "jq is required"
command -v gh >/dev/null || die "gh is required"

mode="${1:-}"; [ $# -eq 0 ] || shift
REPO="${REPO:-}"; VERSION="${VERSION:-}"; DIST="${DIST:-}"; dir=""; attest=0
if [ "$mode" = release ]; then
  while [ $# -gt 0 ]; do
    case "$1" in
      -R|--repo) [ $# -ge 2 ] || die "$1 needs a value"; REPO="$2"; shift 2 ;;
      --dir) [ $# -ge 2 ] || die "--dir needs a value"; dir="$2"; shift 2 ;;
      --attest) attest=1; shift ;;
      -*) die "unknown option $1" ;;
      *) VERSION="$1"; shift ;;
    esac
  done
fi
[[ "$REPO" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "the repository '$REPO' is not owner/name (-R or REPO)"
semver_valid "$VERSION" || die "the version '$VERSION' is not a release version"
tag="v$VERSION"
failures=0
rows=()
fail() { failures=$((failures + 1)); printf 'verify: FAIL %s\n' "$1" >&2; [ -n "${GITHUB_ACTIONS:-}" ] && printf '::error title=verify::%s\n' "$1"; return 0; }
summary() { if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then printf '%s\n' "$1" >> "$GITHUB_STEP_SUMMARY"; fi; }

# attest_verify <file>
attest_verify() {
  retry 3 10 gh attestation verify "$1" -R "$REPO" --signer-workflow "$REPO/.github/workflows/release.yml" --source-ref refs/heads/main >/dev/null
}

finish() {
  local title="$1" r
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    summary "### $title"
    summary ""
    summary "| File | Result |"
    summary "|---|---|"
    for r in "${rows[@]:-}"; do [ -n "$r" ] && summary "| ${r%%|*} | ${r#*|} |"; done
  fi
  if [ "$failures" -ne 0 ]; then echo "verify: $failures failure(s)" >&2; exit 1; fi
  echo "verify: all checks passed for $tag"
}

case "$mode" in
  draft)
    DIST="${DIST:-}"; state="${PUBLISH_STATE:-${RUNNER_TEMP:-/tmp}/publish-state}"
    [ -d "$DIST" ] || die "DIST '$DIST' is not a directory"
    [ -f "$state/release-id" ] || die "no draft was recorded by this run"
    id="$(cat "$state/release-id")"; [[ "$id" =~ ^[0-9]+$ ]] || die "the recorded release id '$id' is not a number"
    json="$(gh_release_by_id "$id")" || die "cannot read draft $id"
    [ "$(jq -r .draft <<< "$json")" = true ] || die "release $id is not a draft"
    work="$(mktemp -d "${TMPDIR:-/tmp}/verify-draft.XXXXXX")" || die "no temporary directory"
    trap 'rm -rf "$work"' EXIT
    want="$(release_files "$VERSION" | LC_ALL=C sort)"
    have="$(jq -r '.assets[]?.name' <<< "$json" | LC_ALL=C sort)"
    [ "$want" = "$have" ] || fail "the draft's files are not exactly the expected ones (have: $(printf '%s' "$have" | tr '\n' ' '))"
    while IFS= read -r f; do
      aid="$(jq -r --arg n "$f" '.assets[]? | select(.name == $n) | .id' <<< "$json")"
      if [ -z "$aid" ]; then rows+=("\`$f\`|missing from the draft"); fail "$f is missing from the draft"; continue; fi
      if ! gh api -H 'Accept: application/octet-stream' "repos/$REPO/releases/assets/$aid" > "$work/$f" 2>"$work/err"; then
        rows+=("\`$f\`|download failed"); fail "$f: cannot download the draft's copy: $(head -n 2 "$work/err" | tr '\n' ' ')"; continue
      fi
      got="$(sha256_of "$work/$f")"
      if [ "$got" != "$(sha256_of "$DIST/$f")" ]; then rows+=("\`$f\`|differs from the build"); fail "$f: the draft's copy ($got) differs from the built file"; continue; fi
      # the assets must also match the line SHA256SUMS holds for them
      if line="$(grep -F "  $f" "$DIST/SHA256SUMS" | head -n 1)" && [ -n "$line" ] && [ "${line%% *}" != "$got" ]; then
        rows+=("\`$f\`|differs from SHA256SUMS"); fail "$f: the draft's copy does not match SHA256SUMS"; continue
      fi
      if ! attest_verify "$work/$f"; then rows+=("\`$f\`|attestation did not verify"); fail "$f: gh attestation verify failed"; continue; fi
      rows+=("\`$f\`|ok: bytes match, attestation verified")
    done < <(release_files "$VERSION")
    finish "Draft $tag: downloaded and verified"
    ;;
  release)
    if [ -n "$dir" ]; then
      [ -d "$dir" ] || die "--dir '$dir' is not a directory"
    else
      dir="$(mktemp -d "${TMPDIR:-/tmp}/verify-release.XXXXXX")" || die "no temporary directory"
      trap 'rm -rf "$dir"' EXIT
      gh release download "$tag" -R "$REPO" -D "$dir" >/dev/null || die "cannot download the assets of $tag"
    fi
    if retry 3 10 gh release verify "$tag" -R "$REPO" >/dev/null; then rows+=("release \`$tag\`|ok: immutable release attestation verified")
    else rows+=("release \`$tag\`|gh release verify failed"); fail "gh release verify $tag failed"; fi
    while IFS= read -r f; do
      if [ ! -f "$dir/$f" ]; then rows+=("\`$f\`|not found in $dir"); fail "$f is not in $dir"; continue; fi
      if ! retry 3 10 gh release verify-asset "$tag" "$dir/$f" -R "$REPO" >/dev/null; then rows+=("\`$f\`|verify-asset failed"); fail "gh release verify-asset $tag $f failed"; continue; fi
      if [ "$attest" = 1 ] && ! attest_verify "$dir/$f"; then rows+=("\`$f\`|attestation did not verify"); fail "$f: gh attestation verify failed"; continue; fi
      rows+=("\`$f\`|ok$([ "$attest" = 1 ] && echo ', attestation verified')")
    done < <(release_files "$VERSION")
    if [ -f "$dir/SHA256SUMS" ]; then
      (cd "$dir" && sha256sum -c SHA256SUMS --ignore-missing >/dev/null 2>&1) || fail "sha256sum -c SHA256SUMS failed in $dir"
    fi
    finish "Release $tag: verified"
    ;;
  *) die "usage: verify.sh draft | verify.sh release [VERSION] [-R OWNER/NAME] [--dir DIR] [--attest]" ;;
esac
