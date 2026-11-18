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

echo "platforms, .tar.xz and the cargo fallback"
command -v xz >/dev/null && {
  tar -cJf "$t_tmp/fakex.tar.xz" -C "$t_tmp/src" fake-1.0
  xz_sha="$(sha256sum "$t_tmp/fakex.tar.xz" | awk '{print $1}')"
  printf 'fakex 1.0 file://%s %s fake-1.0/fake\n' "$t_tmp/fakex.tar.xz" "$xz_sha" >> "$pins"
  rm -f "$t_tmp/bin/fake"
  expect_ok "a .tar.xz archive installs" inst fakex
}
printf 'plat 1.0 file://%s %s fake-1.0/fake linux-aarch64\n' "$t_tmp/fake.tar.gz" "$good" >> "$pins"
expect_fail "a line for another platform is not used (and there is no cargo fallback)" "not in .* for linux-x86_64" env TOOLS_CARGO_PINS=/nonexistent TOOLS_PLATFORM=linux-x86_64 TOOLS_PINS="$pins" TOOLS_BIN="$t_tmp/bin" "$install" plat
expect_ok "the platform's own line installs" env TOOLS_PLATFORM=linux-aarch64 TOOLS_PINS="$pins" TOOLS_BIN="$t_tmp/bin" "$install" plat
mkdir -p "$t_tmp/stubcargo"
cat > "$t_tmp/stubcargo/cargo" <<'STUB'
#!/usr/bin/env bash
# cargo install --locked --version V --root R NAME: makes R/bin/NAME and records the call
echo "cargo $*" >> "$STUB_LOG"
root=""; name=""; prev=""
for a in "$@"; do [ "$prev" = --root ] && root="$a"; prev="$a"; name="$a"; done
mkdir -p "$root/bin"; printf '#!/bin/sh\necho built\n' > "$root/bin/$name"; chmod +x "$root/bin/$name"
STUB
chmod +x "$t_tmp/stubcargo/cargo"
printf 'cfake 2.3.4\nbadver latest\n' > "$t_tmp/cargo.pins"
cinst() { env PATH="$t_tmp/stubcargo:$PATH" STUB_LOG="$t_tmp/cargo.log" TOOLS_CARGO_PINS="$t_tmp/cargo.pins" TOOLS_PLATFORM=darwin-arm64 TOOLS_PINS="$pins" TOOLS_BIN="$t_tmp/bin" "$install" "$@"; }
rm -f "$t_tmp/bin/cfake"
expect_ok "a tool with no binary for the platform is built with cargo install --locked" cinst cfake
expect_match "…at the pinned version" "^cargo install --locked --version 2\.3\.4 --root .* cfake\$" "$(cat "$t_tmp/cargo.log")"
[ -x "$t_tmp/bin/cfake" ] && ok "…and installed" || bad "cfake not installed"
expect_fail "a cargo.pins entry that is not a version" "is not a version" cinst badver
# macOS's BSD wc pads its count ("       1"): a single pin must still be found with such a wc.
mkdir -p "$t_tmp/padwc"
printf '#!/bin/sh\nn=$(command -p wc "$@" | tr -d " ")\nprintf "%%8s\\n" "$n"\n' > "$t_tmp/padwc/wc"
chmod +x "$t_tmp/padwc/wc"
rm -f "$t_tmp/bin/cfake"
expect_ok "a cargo.pins tool installs when wc pads its count (macOS)" env PATH="$t_tmp/padwc:$t_tmp/stubcargo:$PATH" STUB_LOG="$t_tmp/cargo.log" TOOLS_CARGO_PINS="$t_tmp/cargo.pins" TOOLS_PLATFORM=darwin-arm64 TOOLS_PINS="$pins" TOOLS_BIN="$t_tmp/bin" "$install" cfake
expect_ok "a tools.pins tool installs when wc pads its count (macOS)" env PATH="$t_tmp/padwc:$PATH" TOOLS_PINS="$pins" TOOLS_BIN="$t_tmp/bin" "$install" fake

# The real pins file: one line per tool, https from the tool's own release page, 64-hex hashes.
real="$release_dir/tools.pins"
for tool in cargo-deny actionlint cargo-auditable cargo-audit; do
  line="$(awk -v t="$tool" '$1 == t' "$real")"
  expect_match "tools.pins pins $tool with an https release URL and a sha256" "^$tool [0-9]+\.[0-9]+\.[0-9]+ https://github\.com/[^ ]+/releases/download/[^ ]+ [0-9a-f]{64} [^ ]+\$" "$line"
done
# every tool the build workflows install is pinned for linux-x86_64 or in cargo.pins
for tool in cargo-auditable cargo-audit; do
  expect_match "cargo.pins has $tool for the platforms with no prebuilt binary" "^$tool [0-9]+\.[0-9]+\.[0-9]+\$" "$(grep "^$tool " "$release_dir/cargo.pins")"
  expect_eq "$tool: cargo.pins and tools.pins name the same version" "$(awk -v t="$tool" '$1==t {print $2}' "$release_dir/cargo.pins")" "$(awk -v t="$tool" '$1==t {print $2}' "$real")"
done
finish
