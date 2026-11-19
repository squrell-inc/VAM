#!/usr/bin/env bash
# assemble.sh on a fixture repository and dummy assets: SHA256SUMS, the asset-set check, the scan
# gate, build-info.json and the summary. gitleaks is a stub; no network.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"
# shellcheck source=scripts/release/lib.sh
. "$release_dir/lib.sh"

assemble="$release_dir/assemble.sh"
v=0.1.0-rc.1
repo_name=acme/vam
fix="$t_tmp/repo"; mkdir -p "$fix" && cd "$fix" || exit 1
git_quiet init -q . && git_quiet checkout -q -b main && echo 0 > f && git_quiet add -A && git_quiet commit -q -m C0
head="$(git rev-parse HEAD)"
export NOTES_API_FIXTURES="$t_tmp/api"; mkdir -p "$NOTES_API_FIXTURES"
stub="$t_tmp/gitleaks-stub"; printf '#!/bin/sh\nexit "${STUB_RC:-0}"\n' > "$stub"; chmod +x "$stub"; export GITLEAKS="$stub"

new_assets() {
  A="$t_tmp/assets"; rm -rf "$A" "$t_tmp/out"; mkdir -p "$A"
  while IFS= read -r a; do
    case "$a" in
      *.tar.gz) d="$t_tmp/pkg/${a%.tar.gz}"; rm -rf "$t_tmp/pkg"; mkdir -p "$d"; printf 'bin %s\n' "$a" > "$d/vam"; tar -czf "$A/$a" -C "$t_tmp/pkg" "${a%.tar.gz}" ;;
      *) printf 'runtime\n' > "$A/$a" ;;
    esac
  done < <(built_assets "$v")
}
run_assemble() { (cd "$fix" && "$assemble" --version "$v" --commit "$head" --channel "${CHANNEL:-dev}" --assets "$A" --out "$t_tmp/out" --repo "$repo_name" "$@"); }

echo "a good set"
new_assets
export GITHUB_STEP_SUMMARY="$t_tmp/summary.md"; : > "$GITHUB_STEP_SUMMARY"; export GITHUB_RUN_ID=4242
run_cmd run_assemble
expect_eq "exits 0" 0 "$T_RC"
out="$t_tmp/out"
expect_eq "the release directory holds the assets, SHA256SUMS, the notes and build-info.json" \
  "$( { release_assets "$v"; printf '%s\n' SHA256SUMS RELEASE_NOTES.md build-info.json; } | LC_ALL=C sort | tr '\n' ' ')" \
  "$(ls "$out" | LC_ALL=C sort | tr '\n' ' ')"
expect_ok "install.sh is copied from the checkout, byte for byte, executable" bash -c "cmp '$release_dir/install.sh' '$out/install.sh' && [ -x '$out/install.sh' ]"
expect_match "SHA256SUMS lists install.sh" "^[0-9a-f]{64}  install\.sh\$" "$(cat "$out/SHA256SUMS")"
expect_ok "sha256sum -c SHA256SUMS passes" bash -c "cd '$out' && sha256sum -c SHA256SUMS"
expect_eq "SHA256SUMS lists every asset once" "$(release_assets "$v" | wc -l | tr -d ' ')" "$(wc -l < "$out/SHA256SUMS" | tr -d ' ')"
expect_eq "SHA256SUMS is sorted by name" "$(awk '{print $2}' "$out/SHA256SUMS")" "$(awk '{print $2}' "$out/SHA256SUMS" | LC_ALL=C sort)"
expect_nomatch "SHA256SUMS does not list itself, the notes or build-info" "SHA256SUMS$|RELEASE_NOTES|build-info" "$(awk '{print $2}' "$out/SHA256SUMS")"
expect_match "SHA256SUMS uses the two-space sha256sum format" "^[0-9a-f]{64}  vam" "$(grep "  vam" "$out/SHA256SUMS" | head -n 1)"
expect_eq "build-info version" "$v" "$(jq -r .version "$out/build-info.json")"
expect_eq "build-info commit" "$head" "$(jq -r .commit "$out/build-info.json")"
expect_eq "build-info channel" "dev" "$(jq -r .channel "$out/build-info.json")"
expect_eq "build-info run id" "4242" "$(jq -r .run_id "$out/build-info.json")"
expect_eq "build-info lists the assets" "$(release_assets "$v" | wc -l | tr -d ' ')" "$(jq '.assets | length' "$out/build-info.json")"
expect_match "the notes carry the asset table" "\| Asset \| Size \(bytes\) \| SHA-256 \|" "$(cat "$out/RELEASE_NOTES.md")"
expect_match "the notes name the repository in the verify commands" "-R acme/vam" "$(cat "$out/RELEASE_NOTES.md")"
expect_match "the summary has the heading" "### Release $v \(dev build\) at" "$(cat "$GITHUB_STEP_SUMMARY")"
expect_match "the summary has the table" "\| Asset \| Size" "$(cat "$GITHUB_STEP_SUMMARY")"
expect_match "the summary says the scan was clean" "Scan clean" "$(cat "$GITHUB_STEP_SUMMARY")"
CHANNEL=release; new_assets; run_cmd run_assemble
expect_eq "the release channel is recorded" "release" "$(jq -r .channel "$t_tmp/out/build-info.json")"
CHANNEL=dev

echo "the asset set must be exact"
new_assets; rm "$A/vam-runtime-$v-x86_64-unknown-linux-musl"
expect_fail "a missing asset" "missing asset.*vam-runtime-$v" run_assemble
new_assets; echo x > "$A/install.sh"
expect_fail "install.sh among the built assets is stray (it comes from the checkout)" "unexpected file.*install\.sh" run_assemble
new_assets; echo x > "$A/extra.bin"
expect_fail "another stray file" "unexpected file.*extra\.bin" run_assemble
new_assets; : > "$A/vam-runtime-$v-x86_64-unknown-linux-musl"
expect_fail "an empty asset" "not a non-empty regular file" run_assemble

echo "the scan gates everything"
new_assets
d="$t_tmp/pkg/vam-$v-x86_64-unknown-linux-gnu"; mkdir -p "$d"; echo secret > "$d/.env"
tar -czf "$A/vam-$v-x86_64-unknown-linux-gnu.tar.gz" -C "$t_tmp/pkg" "vam-$v-x86_64-unknown-linux-gnu"
expect_fail "a planted .env stops the assembly" "refused file name" run_assemble
[ ! -e "$t_tmp/out" ] && ok "nothing is written when the scan fails" || bad "the out directory exists after a failed scan"
new_assets
STUB_RC=1 expect_fail "a gitleaks finding stops the assembly" "gitleaks reported findings" run_assemble

echo "the output directory"
new_assets; mkdir -p "$t_tmp/out"; echo x > "$t_tmp/out/old"
expect_fail "a non-empty --out is refused" "exists and is not empty" run_assemble
new_assets
expect_fail "a bad channel" "channel must be release or dev" bash -c "cd '$fix' && '$assemble' --version $v --commit $head --channel nightly --assets '$A' --out '$t_tmp/out' --repo $repo_name"
expect_fail "a bad version" "not a release version" bash -c "cd '$fix' && '$assemble' --version 1.0 --commit $head --channel dev --assets '$A' --out '$t_tmp/out' --repo $repo_name"
finish
