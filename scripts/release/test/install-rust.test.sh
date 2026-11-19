#!/usr/bin/env bash
# install-rust.sh against a local fake rustup-init (no network): a good pin runs it with the right
# arguments, a wrong hash does not run it, and the real rustup.pins is well formed.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"

inst="$release_dir/install-rust.sh"
fake="$t_tmp/rustup-init"
printf '#!/bin/sh\necho "rustup-init $*" >> "$STUB_LOG"\nmkdir -p "$CARGO_HOME/bin"\nprintf "#!/bin/sh\\necho rustc 1.97.0\\n" > "$CARGO_HOME/bin/rustc"; chmod +x "$CARGO_HOME/bin/rustc"\n' > "$fake"
good="$(sha256sum "$fake" | awk '{print $1}')"
pins="$t_tmp/pins"
printf 'linux-x86_64 1.0.0 file://%s %s\nlinux-aarch64 1.0.0 file://%s %s\nlinux-riscv 1.0.0 file://%s abc\n' "$fake" "$good" "$fake" "$(printf '%064d' 0)" "$fake" > "$pins"
run() { env STUB_LOG="$t_tmp/log" CARGO_HOME="$t_tmp/cargo" RUSTUP_PINS="$pins" "$@"; }

: > "$t_tmp/log"
expect_ok "a pinned rustup-init runs" run RUSTUP_PLATFORM=linux-x86_64 "$inst" 1.97.0
expect_match "…with the minimal profile, the toolchain and no PATH edit" "^rustup-init -y --profile minimal --default-toolchain 1\.97\.0 --no-modify-path\$" "$(cat "$t_tmp/log")"
gp="$t_tmp/github_path"; : > "$gp"
expect_ok "adds cargo's bin to GITHUB_PATH" run RUSTUP_PLATFORM=linux-x86_64 GITHUB_PATH="$gp" "$inst"
expect_eq "…the bin directory" "$t_tmp/cargo/bin" "$(cat "$gp")"
: > "$t_tmp/log"
expect_fail "a wrong hash is refused" "sha256 mismatch" run RUSTUP_PLATFORM=linux-aarch64 "$inst"
[ ! -s "$t_tmp/log" ] && ok "…and rustup-init was not run" || bad "ran despite the mismatch"
expect_fail "a malformed hash is refused" "not 64 hex digits" run RUSTUP_PLATFORM=linux-riscv "$inst"
expect_fail "an unpinned platform is refused" "no rustup-init pinned for darwin-arm64" run RUSTUP_PLATFORM=darwin-arm64 "$inst"

real="$release_dir/rustup.pins"
for p in linux-x86_64 linux-aarch64; do
  expect_match "rustup.pins pins $p with an https static.rust-lang.org URL and a sha256" "^$p [0-9]+\.[0-9]+\.[0-9]+ https://static\.rust-lang\.org/rustup/archive/[^ ]+/rustup-init [0-9a-f]{64}\$" "$(grep "^$p " "$real")"
done
# the toolchain default is the clippy toolchain
clippy="$(sed -n 's/^ *CLIPPY_TOOLCHAIN="\([^"]*\)".*/\1/p' "$repo_root/scripts/ci/local.sh" | head -n 1)"
expect_match "install-rust.sh defaults to CLIPPY_TOOLCHAIN ($clippy)" "toolchain=\"\\\${1:-$clippy}\"" "$(grep '^toolchain=' "$inst")"
finish
