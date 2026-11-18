#!/usr/bin/env bash
# Writes the release notes (design/release-artifacts.md §3.4, locked decision 12): no CHANGELOG
# file, the notes come from the first-parent merges into main since the previous release.
#
#   scripts/release/notes.sh --version V --commit SHA --assets DIR [--repo OWNER/NAME] [--out FILE]
#
#   --repo   default $REPO; the repository the verify commands name (never hardcoded here)
#   --out    default stdout
# Run from a checkout with full history and tags. Env:
#   GH_TOKEN             a token that can read pull requests (`pull-requests: read`)
#   NOTES_API_FIXTURES   tests: a directory with pull-<N>.json (the API answer), pull-<N>.404 (not
#                        found) instead of `gh api`; any other number is an API failure
#
# The range is previous..SHA. The previous release is the greatest v* tag below V (by semver) that
# is an ancestor of SHA: for an rc any tag, for a final the finals only (tracker open question 11);
# with none, the whole history. Sections:
#   Releases           the PRs titled `release: ...` (title from the API) except `release: prepare v...`
#   Upgrade notes      the "Deploy note" section of each of their bodies, verbatim
#   Artifacts and verification   the asset table (size, sha256) and the verify commands
# A PR that cannot be found (404) is counted and said so at the end of Releases; any other API
# failure stops the script, so a note is never dropped silently. The notes must stay under
# 100,000 characters (GitHub's limit is 125,000): they are cut by nobody, they fail.
set -uo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=scripts/release/lib.sh
. "$here/lib.sh"
die() { echo "notes: $*" >&2; exit 1; }

version=""; commit=""; assets=""; repo_name="${REPO:-}"; out=""
while [ $# -gt 0 ]; do
  [ $# -ge 2 ] || die "$1 needs a value"
  case "$1" in
    --version) version="$2" ;; --commit) commit="$2" ;; --assets) assets="$2" ;;
    --repo) repo_name="$2" ;; --out) out="$2" ;;
    *) die "unknown option $1" ;;
  esac
  shift 2
done
semver_valid "$version" || die "--version '$version' is not a release version"
[[ "$commit" =~ ^[0-9a-f]{40}$ ]] || die "--commit '$commit' is not a full 40-hex SHA"
[ -d "$assets" ] || die "--assets: no such directory '$assets'"
[[ "$repo_name" =~ ^[A-Za-z0-9._-]+/[A-Za-z0-9._-]+$ ]] || die "the repository '$repo_name' is not owner/name (--repo or REPO)"
command -v jq >/dev/null || die "jq is required"
git cat-file -e "$commit^{commit}" 2>/dev/null || die "$commit is not a commit in this checkout"

# --- the previous release -------------------------------------------------------------------------
# Local tags only: the checkout fetched them, and notes need no network beyond the API.
prev=""
while IFS= read -r t; do
  [ -n "$t" ] || continue
  semver_valid "$t" || continue
  semver_gt "$version" "$t" || continue
  case "$version" in *-rc.*) ;; *) case "$t" in *-rc.*) continue ;; esac ;; esac
  git merge-base --is-ancestor "v$t" "$commit" 2>/dev/null || continue
  if [ -z "$prev" ] || semver_gt "$t" "$prev"; then prev="$t"; fi
done < <(REMOTE="" version_tags)
if [ -n "$prev" ]; then range="v$prev..$commit"; since="v$prev"; else range="$commit"; since="the start of the history"; fi

# --- pull requests --------------------------------------------------------------------------------
# pr_json <number>: the PR's JSON on stdout. Exit 0 found, 3 not found, 1 any other failure.
pr_json() {
  local n="$1" err rc
  if [ -n "${NOTES_API_FIXTURES:-}" ]; then
    if [ -f "$NOTES_API_FIXTURES/pull-$n.json" ]; then cat "$NOTES_API_FIXTURES/pull-$n.json"; return 0; fi
    if [ -f "$NOTES_API_FIXTURES/pull-$n.404" ]; then return 3; fi
    echo "no fixture for pull $n" >&2; return 1
  fi
  command -v gh >/dev/null || { echo "gh is not installed" >&2; return 1; }
  err="$(mktemp "${TMPDIR:-/tmp}/notes-err.XXXXXX")"
  gh api -H 'Accept: application/vnd.github+json' "repos/$repo_name/pulls/$n" 2>"$err"; rc=$?
  if [ "$rc" != 0 ]; then
    if grep -q 'HTTP 404' "$err"; then rm -f "$err"; return 3; fi
    cat "$err" >&2; rm -f "$err"; return 1
  fi
  rm -f "$err"
}

# deploy_note <body on stdin>: the text under a "Deploy note" heading (# .. ###### or a bold line,
# optional s and colon) up to the next heading outside a code fence, blank edges trimmed, CR removed.
deploy_note() {
  tr -d '\r' | awk '
    function lower_trim(x) { x = tolower(x); gsub(/^[[:space:]]+|[[:space:]]+$/, "", x); return x }
    function is_title(x) { return lower_trim(x) ~ /^deploy notes?[[:space:]]*:?$/ }
    BEGIN { insec = 0; fence = 0; n = 0; finished = 0 }
    {
      if (finished) next
      if ($0 ~ /^[[:space:]]*(```|~~~)/) { fence = !fence; if (insec) buf[++n] = $0; next }
      heading = 0
      if (!fence && match($0, /^#+[[:space:]]/) && RLENGTH <= 7) { heading = 1; h = substr($0, RLENGTH + 1) }
      if (heading) {
        if (insec) { finished = 1; next }
        if (is_title(h)) insec = 1
        next
      }
      if (!insec && !fence && $0 ~ /^[[:space:]]*\*\*/) {
        b = $0; gsub(/\*\*/, "", b)
        if (is_title(b)) { insec = 1; next }
      }
      if (insec) buf[++n] = $0
    }
    END {
      i = 1; while (i <= n && buf[i] ~ /^[[:space:]]*$/) i++
      j = n; while (j >= i && buf[j] ~ /^[[:space:]]*$/) j--
      for (; i <= j; i++) print buf[i]
    }'
}

releases_md=""; upgrades_md=""; skipped=0; listed=0; no_pr_number=0
while IFS=$'\t' read -r sha subject; do
  [ -n "$sha" ] || continue
  if [[ "$subject" =~ \#([0-9]+) ]]; then num="${BASH_REMATCH[1]}"; else no_pr_number=$((no_pr_number + 1)); continue; fi
  json="$(pr_json "$num")"; rc=$?
  if [ "$rc" = 3 ]; then skipped=$((skipped + 1)); echo "notes: pull request #$num was not found; not listed" >&2; continue; fi
  [ "$rc" = 0 ] || die "cannot read pull request #$num (merge $sha): $json"
  title="$(jq -r '.title // empty' <<< "$json")" || die "pull request #$num: the answer is not JSON"
  [ -n "$title" ] || die "pull request #$num has no title"
  case "$title" in release:\ prepare\ v*) continue ;; release:*) ;; *) continue ;; esac
  url="$(jq -r '.html_url // empty' <<< "$json")"
  [ -n "$url" ] || url="https://github.com/$repo_name/pull/$num"
  listed=$((listed + 1))
  releases_md+="- ${title} ([#$num]($url))"$'\n'
  note="$(jq -r '.body // ""' <<< "$json" | deploy_note)"
  if [ -n "$note" ]; then
    upgrades_md+="#### ${title} (#$num)"$'\n\n'"$note"$'\n\n'
  fi
done < <(git log --first-parent --merges --format='%H%x09%s' "$range")

# --- assets -----------------------------------------------------------------------------------------
table="| Asset | Size (bytes) | SHA-256 |"$'\n'"|---|---|---|"$'\n'
first_asset=""
while IFS= read -r a; do
  [ -f "$assets/$a" ] || die "asset $a is missing from $assets"
  table+="| \`$a\` | $(file_size "$assets/$a") | \`$(sha256_of "$assets/$a")\` |"$'\n'
  [ -n "$first_asset" ] || first_asset="$a"
done < <(release_assets "$version")
tag="v$version"

{
  printf '# VAM %s\n\n' "$tag"
  case "$version" in *-rc.*) printf 'A release candidate (pre-release). Commit `%s`.\n\n' "$commit" ;; *) printf 'Commit `%s`.\n\n' "$commit" ;; esac
  printf '## Releases\n\nChanges since %s.\n\n' "$since"
  if [ "$listed" -gt 0 ]; then printf '%s\n' "$releases_md"; else printf 'No `release:` pull requests in this range.\n\n'; fi
  [ "$skipped" -eq 0 ] || printf '_%s merge(s) refer to a pull request that could not be found and are not listed._\n\n' "$skipped"
  printf '## Upgrade notes\n\n'
  if [ -n "$upgrades_md" ]; then printf '%s' "$upgrades_md"; else printf 'No upgrade notes.\n\n'; fi
  printf '## Artifacts and verification\n\n%s\n' "$table"
  cat <<EOF
Check a release first, then a download (replace \`<asset>\` with a file name from the table):

\`\`\`sh
gh release verify $tag -R $repo_name
gh release verify-asset $tag <asset> -R $repo_name
sha256sum -c SHA256SUMS --ignore-missing
gh attestation verify <asset> -R $repo_name --signer-workflow $repo_name/.github/workflows/release.yml
\`\`\`
EOF
} > "${out:-/dev/stdout}"

if [ -n "$out" ]; then
  size="$(wc -c < "$out" | tr -d ' ')"
  [ "$size" -le 100000 ] || die "the notes are $size characters; GitHub's limit is 125000 and nothing is cut (shorten a deploy note)"
  echo "notes: $listed release PR(s) since $since, $no_pr_number merge(s) without a PR number, $skipped not found; $size characters" >&2
fi
