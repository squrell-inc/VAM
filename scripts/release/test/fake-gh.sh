#!/usr/bin/env bash
# A stateful stand-in for `gh` for publish.test.sh and verify.test.sh (the tests copy it to bin/gh).
# It models just what publish.sh and verify.sh call: releases (drafts included), their assets, tags
# (GitHub creates the tag when a draft is published), `gh release create|upload|verify|verify-asset|
# download`, `gh attestation verify` and the `gh api` paths below. Every call is appended to
# $FAKE_GH_DIR/calls.log. State lives in $FAKE_GH_DIR: releases.json, tags.json, assets/<id>.
#
# Failure and fault injection (env): FAKE_FAIL=a,b,... makes these operations fail:
#   tags list create upload patch delete release_verify verify_asset attest get_release
# FAKE_NO_RELEASE_VERIFY=1 and FAKE_NO_SOURCE_REF=1 hide a command or flag from --help.
# FAKE_CORRUPT=<name> stores that uploaded file with an extra byte; FAKE_PENDING=<name> leaves it
# in state "starter"; FAKE_NO_TAG=1 publishes without creating the tag; FAKE_TAG_SHA=<sha> creates
# it elsewhere; FAKE_ANNOTATED=1 creates an annotated tag object; FAKE_ATTEST_FAIL_NAME=<basename>
# fails the attestation of that file; FAKE_ATTEST_FLAKY=<n> fails the first n attestation checks.
set -uo pipefail
d="${FAKE_GH_DIR:?FAKE_GH_DIR is not set}"
mkdir -p "$d/assets"
echo "gh $*" >> "$d/calls.log"
[ -f "$d/releases.json" ] || echo '[]' > "$d/releases.json"
[ -f "$d/tags.json" ] || echo '[]' > "$d/tags.json"
[ -f "$d/next-id" ] || echo 100 > "$d/next-id"
failing() { case ",${FAKE_FAIL:-}," in *",$1,"*) return 0 ;; esac; return 1; }
next_id() { local n; n="$(cat "$d/next-id")"; echo $((n + 1)) > "$d/next-id"; echo "$n"; }
upd() { local f="$1"; shift; jq "$@" "$f" > "$f.new" && mv "$f.new" "$f"; }
notfound() { echo "gh: Not Found (HTTP 404)" >&2; exit 1; }
sha_of() { sha256sum "$1" | awk '{print $1}'; }

add_tag() { # <tag> <sha>
  local type=commit sha="$2"
  if [ "${FAKE_ANNOTATED:-0}" = 1 ]; then type=tag; echo "$sha" > "$d/annotated-target"; sha="$(printf 'a%.0s' $(seq 40))"; fi
  upd "$d/tags.json" --arg r "refs/tags/$1" --arg s "$sha" --arg t "$type" '. + [{ref: $r, object: {sha: $s, type: $t}}]'
}

case "${1:-} ${2:-} ${3:-}" in
  "--version  ") echo "gh version 2.89.0 (fake)"; exit 0 ;;
  "release verify --help") [ "${FAKE_NO_RELEASE_VERIFY:-0}" = 1 ] && { echo "unknown command" >&2; exit 1; }; echo "gh release verify"; exit 0 ;;
  "release verify-asset --help") echo "gh release verify-asset"; exit 0 ;;
  "attestation verify --help")
    echo "gh attestation verify --repo --signer-workflow"
    [ "${FAKE_NO_SOURCE_REF:-0}" = 1 ] || echo "--source-ref"
    exit 0 ;;
esac

cmd="${1:-}"; sub="${2:-}"
case "$cmd $sub" in
  "api "*)
    shift
    method=GET; path=""; accept=""; declare -A fields=()
    while [ $# -gt 0 ]; do
      case "$1" in
        -X) method="$2"; shift 2 ;;
        -H) case "$2" in Accept:*) accept="${2#Accept: }" ;; esac; shift 2 ;;
        -F|-f) fields["${2%%=*}"]="${2#*=}"; shift 2 ;;
        --paginate) shift ;;
        -*) shift ;;
        *) path="$1"; shift ;;
      esac
    done
    path="${path%%\?*}"
    case "$path" in
      repos/*/git/matching-refs/tags/*)
        failing tags && { echo "gh: server error (HTTP 500)" >&2; exit 1; }
        jq -c --arg p "refs/tags/${path##*/matching-refs/tags/}" 'map(select(.ref | startswith($p)))' "$d/tags.json" ;;
      repos/*/git/ref/tags/*)
        jq -ec --arg r "refs/tags/${path##*/git/ref/tags/}" '.[] | select(.ref == $r)' "$d/tags.json" || notfound ;;
      repos/*/git/tags/*)
        jq -nc --arg s "$(cat "$d/annotated-target" 2>/dev/null)" '{object: {sha: $s, type: "commit"}}' ;;
      repos/*/releases/assets/*)
        cat "$d/assets/${path##*/}" || notfound ;;
      repos/*/releases/[0-9]*)
        id="${path##*/}"
        failing get_release && { echo "gh: server error (HTTP 500)" >&2; exit 1; }
        jq -e --argjson id "$id" '.[] | select(.id == $id)' "$d/releases.json" >/dev/null || notfound
        case "$method" in
          GET) jq -c --argjson id "$id" '.[] | select(.id == $id)' "$d/releases.json" ;;
          PATCH)
            failing patch && { echo "gh: server error (HTTP 500)" >&2; exit 1; }
            if [ "${fields[draft]:-}" = false ]; then
              upd "$d/releases.json" --argjson id "$id" 'map(if .id == $id then .draft = false else . end)'
              if [ "${FAKE_NO_TAG:-0}" != 1 ]; then
                add_tag "$(jq -r --argjson id "$id" '.[] | select(.id == $id) | .tag_name' "$d/releases.json")" \
                  "${FAKE_TAG_SHA:-$(jq -r --argjson id "$id" '.[] | select(.id == $id) | .target_commitish' "$d/releases.json")}"
              fi
            fi
            jq -c --argjson id "$id" '.[] | select(.id == $id)' "$d/releases.json" ;;
          DELETE)
            failing delete && { echo "gh: server error (HTTP 500)" >&2; exit 1; }
            upd "$d/releases.json" --argjson id "$id" 'map(select(.id != $id))' ;;
        esac ;;
      repos/*/releases)
        failing list && { echo "gh: server error (HTTP 500)" >&2; exit 1; }
        cat "$d/releases.json" ;;
      *) echo "fake gh: unhandled api path $path" >&2; exit 1 ;;
    esac ;;

  "release create")
    shift 2
    failing create && { echo "gh: create failed" >&2; exit 1; }
    tagname="$1"; shift
    target=""; draft=false; pre=false; latest=auto; title=""
    while [ $# -gt 0 ]; do
      case "$1" in
        --target) target="$2"; shift 2 ;; --repo|-R) shift 2 ;; --title|-t) title="$2"; shift 2 ;;
        --notes-file|-F) [ -f "$2" ] || { echo "no notes file $2" >&2; exit 1; }; shift 2 ;;
        --draft|-d) draft=true; shift ;; --prerelease|-p) pre=true; shift ;;
        --latest) latest=true; shift ;; --latest=false) latest=false; shift ;;
        *) echo "fake gh: release create: unexpected $1" >&2; exit 1 ;;
      esac
    done
    id="$(next_id)"
    upd "$d/releases.json" --argjson id "$id" --arg t "$tagname" --arg c "$target" --argjson dr "$draft" --argjson pr "$pre" --arg la "$latest" \
      '. + [{id: $id, tag_name: $t, target_commitish: $c, draft: $dr, prerelease: $pr, latest: $la, html_url: ("https://github.com/fake/releases/" + $t), assets: []}]'
    [ "$draft" = true ] || add_tag "$tagname" "$target"
    echo "https://github.com/fake/releases/$tagname" ;;

  "release upload")
    shift 2
    failing upload && { echo "gh: upload failed" >&2; exit 1; }
    tagname="$1"; shift
    files=()
    while [ $# -gt 0 ]; do case "$1" in --repo|-R) shift 2 ;; *) files+=("$1"); shift ;; esac; done
    id="$(jq -r --arg t "$tagname" '[.[] | select(.tag_name == $t)] | .[0].id // empty' "$d/releases.json")"
    [ -n "$id" ] || notfound
    for f in "${files[@]}"; do
      aid="$(next_id)"; name="$(basename "$f")"
      cp "$f" "$d/assets/$aid"
      [ "${FAKE_CORRUPT:-}" != "$name" ] || printf 'x' >> "$d/assets/$aid"
      st=uploaded; [ "${FAKE_PENDING:-}" != "$name" ] || st=starter
      upd "$d/releases.json" --argjson id "$id" --argjson aid "$aid" --arg n "$name" --arg st "$st" \
        'map(if .id == $id then .assets += [{id: $aid, name: $n, state: $st}] else . end)'
    done ;;

  "release verify")
    failing release_verify && { echo "gh: no attestation" >&2; exit 1; }
    tagname="$3"
    jq -e --arg t "$tagname" '.[] | select(.tag_name == $t and .draft == false)' "$d/releases.json" >/dev/null || { echo "gh: release $tagname is not published" >&2; exit 1; }
    echo "verified $tagname" ;;

  "release verify-asset")
    failing verify_asset && { echo "gh: asset does not match" >&2; exit 1; }
    tagname="$3"; file="$4"; name="$(basename "$file")"
    aid="$(jq -r --arg t "$tagname" --arg n "$name" '.[] | select(.tag_name == $t and .draft == false) | .assets[] | select(.name == $n) | .id' "$d/releases.json")"
    [ -n "$aid" ] || { echo "gh: $name is not an asset of $tagname" >&2; exit 1; }
    [ "$(sha_of "$d/assets/$aid")" = "$(sha_of "$file")" ] || { echo "gh: $name does not match the release" >&2; exit 1; }
    echo "verified $name" ;;

  "release download")
    shift 2; tagname="$1"; shift; out=.
    while [ $# -gt 0 ]; do case "$1" in -D) out="$2"; shift 2 ;; *) shift 2 ;; esac; done
    mkdir -p "$out"
    jq -r --arg t "$tagname" '.[] | select(.tag_name == $t) | .assets[] | "\(.id) \(.name)"' "$d/releases.json" | while read -r aid name; do cp "$d/assets/$aid" "$out/$name"; done ;;

  "attestation verify")
    shift 2; file="$1"
    n=0; [ -f "$d/attest-count" ] && n="$(cat "$d/attest-count")"
    echo $((n + 1)) > "$d/attest-count"
    failing attest && { echo "gh: no attestation found" >&2; exit 1; }
    [ "$(basename "$file")" != "${FAKE_ATTEST_FAIL_NAME:-}" ] || { echo "gh: no attestation found for $file" >&2; exit 1; }
    [ "$n" -ge "${FAKE_ATTEST_FLAKY:-0}" ] || { echo "gh: attestation not found yet" >&2; exit 1; }
    echo "Loaded digest sha256:$(sha_of "$file") for file://$file" ;;

  *) echo "fake gh: unhandled: $*" >&2; exit 1 ;;
esac
