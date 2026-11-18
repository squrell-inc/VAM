#!/usr/bin/env bash
# install-tool.sh against local file:// archives (no network): a good pin installs, a wrong hash
# and a missing member do not, and the real tools.pins is well formed.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"

install="$release_dir/install-tool.sh"
mkdir -p "$t_tmp/src/fake-1.0" "$t_tmp/bin"
printf '#!/bin/sh\necho fake\n' > "$t_tmp/src/fake-1.0/fake"
tar -czf "$t_tmp/fake.tar.gz" -C "$t_tmp/src" fake-1.0
good="$(sha256sum "$t_tmp/fake.tar.gz" | awk '{print $1}')"
bad_hash="$(printf '%064d' 0)"
pins="$t_tmp/pins"
printf 'fake 1.0 file://%s %s fake-1.0/fake\n' "$t_tmp/fake.tar.gz" "$good" > "$pins"
printf 'tampered 1.0 file://%s %s fake-1.0/fake\n' "$t_tmp/fake.tar.gz" "$bad_hash" >> "$pins"
printf 'nomember 1.0 file://%s %s fake-1.0/other\n' "$t_tmp/fake.tar.gz" "$good" >> "$pins"
printf 'short 1.0 file://%s abc fake-1.0/fake\n' "$t_tmp/fake.tar.gz" >> "$pins"

inst() { env TOOLS_PINS="$pins" TOOLS_BIN="$t_tmp/bin" "$install" "$@"; }

expect_ok "a pinned tool installs" inst fake
[ -x "$t_tmp/bin/fake" ] && ok "…as an executable named after the archive member" || bad "binary not installed"
rm -f "$t_tmp/bin/fake"
expect_fail "a wrong hash is refused" "sha256 mismatch" inst tampered
[ ! -e "$t_tmp/bin/fake" ] && ok "…and nothing is installed" || bad "installed despite the mismatch"
expect_fail "a member missing from the archive is refused" "is not in the archive" inst nomember
expect_fail "a malformed hash is refused" "not 64 hex digits" inst short
expect_fail "an unknown tool is refused" "not in" inst nosuchtool
expect_fail "no tool named" "usage" inst
gp="$t_tmp/github_path"; : > "$gp"
expect_ok "adds the directory to GITHUB_PATH" env GITHUB_PATH="$gp" TOOLS_PINS="$pins" TOOLS_BIN="$t_tmp/bin" "$install" fake
expect_eq "…the install directory" "$t_tmp/bin" "$(cat "$gp")"

# The real pins file: one line per tool, https from the tool's own release page, 64-hex hashes.
real="$release_dir/tools.pins"
for tool in cargo-deny actionlint; do
  line="$(awk -v t="$tool" '$1 == t' "$real")"
  expect_match "tools.pins pins $tool with an https release URL and a sha256" "^$tool [0-9]+\.[0-9]+\.[0-9]+ https://github\.com/[^ ]+/releases/download/[^ ]+ [0-9a-f]{64} [^ ]+\$" "$line"
done
finish
