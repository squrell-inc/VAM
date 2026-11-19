#!/usr/bin/env bash
# notes.sh against a fixture repository (real git) and fixture pull request answers: the range, the
# release: filter, the deploy note extraction, the asset table and the verify commands.
# No network, no gh.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"
# shellcheck source=scripts/release/lib.sh
. "$release_dir/lib.sh"

notes="$release_dir/notes.sh"
repo_name=acme/vam
base="$t_tmp/base"
api="$t_tmp/api"; mkdir -p "$api"

# --- history -----------------------------------------------------------------------------------------
#   C0 (tag v0.0.1) ─ M1 #1 release: first ─ (tag v0.1.0-rc.1) ─ M2 #2 release: second ─ M3 #3 not a release
#     ─ M4 #4 release: prepare v0.1.0-rc.2 ─ M5 #5 (404) ─ M6 (no number) ─ M7 #7 release: seventh (tag v0.1.0-rc.2 at M7)
mkdir -p "$base" && cd "$base" || exit 1
git_quiet init -q . && git_quiet checkout -q -b main
echo 0 > f && git_quiet add -A && git_quiet commit -q -m C0 && git_quiet tag v0.0.1
merge() { # <n> <subject>
  git_quiet checkout -q -b "b$1" && echo "$1" > "f$1" && git_quiet add -A && git_quiet commit -q -m "work $1"
  git_quiet checkout -q main && git_quiet merge -q --no-ff "b$1" -m "$2"
}
merge 1 "Merge pull request #1: release: first"
git_quiet tag v0.1.0-rc.1
merge 2 "release: second (#2)"
merge 3 "Merge pull request #3: wave"
merge 4 "release: prepare v0.1.0-rc.2 (#4)"
merge 5 "Merge pull request #5: gone"
merge 6 "Merge branch 'x'"
merge 7 "release: seventh (#7)"
git_quiet tag v0.1.0-rc.2
head="$(git rev-parse HEAD)"

pr() { # <n> <title> <body>
  jq -n --arg t "$2" --arg b "$3" --arg u "https://github.com/$repo_name/pull/$1" '{title: $t, body: $b, html_url: $u}' > "$api/pull-$1.json"
}
pr 1 "release: first" $'## What\nthe first\n\n## Deploy note\nrebuild the guest rootfs\n'
pr 2 "release: second" $'## What\nx\r\n\r\n## Deploy note\r\n\r\nrun `vam node init` again\r\n\r\n```sh\r\n# a comment, not a heading\r\nvam node status\r\n```\r\n\r\n- last line\r\n\r\n## Proof\r\nnot part of the note\r\n'
pr 3 "RA-A W5: not a release" $'## Deploy note\nmust not appear\n'
pr 4 "release: prepare v0.1.0-rc.2" $'## Deploy note\nmust not appear either\n'
touch "$api/pull-5.404"
pr 7 "release: seventh" $'**Deploy note:**\n\nrestart vamd before the app\n\n**Next**\nnot a heading\n\n### Proof\nignored\n'

assets="$t_tmp/assets"
mk_assets() { # <version>
  rm -rf "$assets"; mkdir -p "$assets"
  while IFS= read -r a; do printf 'content of %s\n' "$a" > "$assets/$a"; done < <(release_assets "$1")
}
export NOTES_API_FIXTURES="$api"

echo "an rc lists the merges since the previous tag of any kind"
mk_assets 0.1.0-rc.3
T_OUT="$(cd "$base" && "$notes" --version 0.1.0-rc.3 --commit "$head" --assets "$assets" --repo "$repo_name" 2>"$t_tmp/notes.err")"; T_RC=$?
expect_eq "exits 0" 0 "$T_RC"
expect_match "starts with the tag" "^# VAM v0\.1\.0-rc\.3" "$T_OUT"
expect_match "says it is a release candidate" "release candidate" "$T_OUT"
expect_match "names the range start" "Changes since v0\.1\.0-rc\.2" "$T_OUT"
expect_match "no release PR in the range" "No .release:. pull requests in this range" "$T_OUT"

mk_assets 0.1.0-rc.2
T_OUT="$(cd "$base" && "$notes" --version 0.1.0-rc.2 --commit "$head" --assets "$assets" --repo "$repo_name" 2>"$t_tmp/notes.err")"; T_RC=$?
expect_eq "rc.2 exits 0" 0 "$T_RC"
expect_match "rc.2 starts after rc.1" "Changes since v0\.1\.0-rc\.1" "$T_OUT"
expect_match "lists #2 with its link" "\[#2\]\(https://github.com/acme/vam/pull/2\)" "$T_OUT"
expect_match "lists #7" "- release: seventh \(\[#7\]" "$T_OUT"
expect_nomatch "leaves out #1 (before rc.1)" "release: first" "$T_OUT"
expect_nomatch "leaves out a PR that is not a release: title" "not a release" "$T_OUT"
expect_nomatch "leaves out release: prepare" "release: prepare" "$T_OUT"
expect_match "says a PR was not found" "1 merge\(s\) refer to a pull request that could not be found" "$T_OUT"
expect_match "the 404 is also on stderr" "pull request #5 was not found" "$(cat "$t_tmp/notes.err")"
expect_nomatch "leaves out the deploy note of a non-release PR" "must not appear" "$T_OUT"

echo "upgrade notes: verbatim, up to the next heading, fences respected"
expect_match "the note of #2 (CRs gone)" "run .vam node init. again" "$T_OUT"
expect_match "a comment line inside a fence stays in the note" "# a comment, not a heading" "$T_OUT"
expect_match "the line after the fence stays" "- last line" "$T_OUT"
expect_nomatch "the next heading ends the note" "not part of the note" "$T_OUT"
expect_nomatch "no carriage returns" $'\r' "$T_OUT"
expect_match "the bold form of the heading works (#7)" "restart vamd before the app" "$T_OUT"
expect_match "a bold line that is not the title stays in the note" "\*\*Next\*\*" "$T_OUT"
expect_nomatch "a heading ends the bold-form note" "ignored" "$T_OUT"
expect_match "each note is under its PR's title" "#### release: second \(#2\)" "$T_OUT"

echo "a final lists the merges since the previous final"
mk_assets 0.1.0
T_OUT="$(cd "$base" && "$notes" --version 0.1.0 --commit "$head" --assets "$assets" --repo "$repo_name" 2>/dev/null)"
expect_match "starts at v0.0.1" "Changes since v0\.0\.1" "$T_OUT"
expect_match "includes the first release PR (before rc.1)" "release: first" "$T_OUT"
expect_match "and its deploy note" "rebuild the guest rootfs" "$T_OUT"
expect_nomatch "no pre-release line for a final" "release candidate" "$T_OUT"

echo "no previous tag: the whole history"
git -C "$base" tag -d v0.0.1 v0.1.0-rc.1 v0.1.0-rc.2 >/dev/null
mk_assets 0.1.0-rc.1
T_OUT="$(cd "$base" && "$notes" --version 0.1.0-rc.1 --commit "$head" --assets "$assets" --repo "$repo_name" 2>/dev/null)"
expect_match "says the start of the history" "Changes since the start of the history" "$T_OUT"
expect_match "lists the first PR" "release: first" "$T_OUT"
git -C "$base" tag v0.0.1 "$(git -C "$base" rev-list --max-parents=0 HEAD)"
git -C "$base" tag v0.1.0-rc.1 "$(git -C "$base" log --first-parent --format=%H --grep='#1: release' | head -n 1)"
git -C "$base" tag v0.1.0-rc.2 "$head"

echo "the asset table and the verify commands"
mk_assets 0.1.0-rc.2
T_OUT="$(cd "$base" && "$notes" --version 0.1.0-rc.2 --commit "$head" --assets "$assets" --repo "$repo_name" 2>/dev/null)"
a="vam-0.1.0-rc.2-aarch64-apple-darwin.tar.gz"
expect_match "a row has name, size and sha256" "\| \`$a\` \| $(file_size "$assets/$a") \| \`$(sha256_of "$assets/$a")\` \|" "$T_OUT"
expect_eq "one row per asset" "$(release_assets 0.1.0-rc.2 | wc -l | tr -d ' ')" "$(grep -cE "^\| \`(vam|install)" <<< "$T_OUT")"
expect_match "gh release verify names the tag and repository" "gh release verify v0\.1\.0-rc\.2 -R acme/vam" "$T_OUT"
expect_match "gh release verify-asset" "gh release verify-asset v0\.1\.0-rc\.2 <asset> -R acme/vam" "$T_OUT"
expect_match "the attestation check names the signer workflow of the repository" "gh attestation verify <asset> -R acme/vam --signer-workflow acme/vam/.github/workflows/release\.yml" "$T_OUT"
expect_match "sha256sum -c" "sha256sum -c SHA256SUMS --ignore-missing" "$T_OUT"
expect_nomatch "no hardcoded repository" "squrell" "$T_OUT"

echo "determinism, --out and limits"
o1="$t_tmp/n1.md"; o2="$t_tmp/n2.md"
(cd "$base" && "$notes" --version 0.1.0-rc.2 --commit "$head" --assets "$assets" --repo "$repo_name" --out "$o1" 2>/dev/null)
(cd "$base" && "$notes" --version 0.1.0-rc.2 --commit "$head" --assets "$assets" --repo "$repo_name" --out "$o2" 2>/dev/null)
cmp -s "$o1" "$o2" && ok "the same inputs give the same notes" || bad "notes differ between runs"
pr 2 "release: second" "$(printf '## Deploy note\n%s\n' "$(head -c 101000 /dev/zero | tr '\0' x)")"
expect_fail "a note over 100000 characters fails, nothing is cut" "100000|limit" bash -c "cd '$base' && '$notes' --version 0.1.0-rc.2 --commit '$head' --assets '$assets' --repo '$repo_name' --out '$t_tmp/big.md'"
pr 2 "release: second" "x"

echo "what it refuses"
rm -f "$api/pull-7.json"
expect_fail "an API failure other than 404 stops the script" "cannot read pull request #7" bash -c "cd '$base' && '$notes' --version 0.1.0-rc.2 --commit '$head' --assets '$assets' --repo '$repo_name'"
pr 7 "release: seventh" "x"
expect_fail "a malformed version" "not a release version" "$notes" --version 1.0 --commit "$head" --assets "$assets" --repo "$repo_name"
expect_fail "a short commit" "full 40-hex" "$notes" --version 0.1.0 --commit abc123 --assets "$assets" --repo "$repo_name"
expect_fail "a bad repository" "not owner/name" bash -c "cd '$base' && '$notes' --version 0.1.0 --commit '$head' --assets '$assets' --repo 'nope'"
rm -f "$assets/vam-runtime-0.1.0-rc.2-x86_64-unknown-linux-musl"
expect_fail "a missing asset" "asset vam-runtime-0.1.0-rc.2.* is missing" bash -c "cd '$base' && '$notes' --version 0.1.0-rc.2 --commit '$head' --assets '$assets' --repo '$repo_name'"
finish
