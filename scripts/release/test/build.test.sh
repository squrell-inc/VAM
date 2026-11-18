#!/usr/bin/env bash
# build.sh against stub cargo, rustup and objdump on a fixture repository: the flags it passes, the
# glibc floor, the build-path leak check, the version check, and what it refuses. No compile.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"

# the release toolchain is the clippy toolchain (one Rust bump, one PR)
clippy="$(sed -n 's/^ *CLIPPY_TOOLCHAIN="\([^"]*\)".*/\1/p' "$repo_root/scripts/ci/local.sh" | head -n 1)"
default_tc="$(sed -n 's/.*RUST_TOOLCHAIN:-\([0-9.]*\)}.*/\1/p' "$release_dir/build.sh" | head -n 1)"
expect_eq "build.sh builds with CLIPPY_TOOLCHAIN" "$clippy" "$default_tc"

fx="$t_tmp/fx"; stubs="$t_tmp/stubs"; mkdir -p "$fx/scripts/release" "$stubs"
cp "$release_dir/build.sh" "$release_dir/build-guest.sh" "$release_dir/lib.sh" "$fx/scripts/release/"
printf '[advisories]\nignore = [\n  { id = "RUSTSEC-2026-0001", reason = "x" },\n]\n' > "$fx/deny.toml"
printf '[workspace.package]\nversion = "0.1.0"\n' > "$fx/Cargo.toml"
git_quiet -C "$fx" init -q
git_quiet -C "$fx" add -A
GIT_COMMITTER_DATE="2026-11-18T12:00:00Z" GIT_AUTHOR_DATE="2026-11-18T12:00:00Z" git_quiet -C "$fx" commit -q -m fixture
commit="$(git -C "$fx" rev-parse HEAD)"
short="$(git -C "$fx" rev-parse --short=7 HEAD)"
epoch="$(git -C "$fx" log -1 --format=%ct)"

cat > "$stubs/rustup" <<'STUB'
#!/usr/bin/env bash
echo "rustup $*" >> "$STUB_LOG"
STUB
cat > "$stubs/cargo" <<'STUB'
#!/usr/bin/env bash
{ echo "cargo $*"; echo "  RUSTFLAGS=${RUSTFLAGS:-}"; echo "  STRIP=${CARGO_PROFILE_RELEASE_STRIP:-}"; echo "  EPOCH=${SOURCE_DATE_EPOCH:-}"
  echo "  SHA=${VAM_GIT_SHA:-} DATE=${VAM_COMMIT_DATE:-} CHANNEL=${VAM_BUILD_CHANNEL:-} INC=${CARGO_INCREMENTAL:-}"; } >> "$STUB_LOG"
case " $* " in
  *" audit "*) exit "${STUB_AUDIT_RC:-0}" ;;
esac
t=""; prev=""
for a in "$@"; do [ "$prev" = --target ] && t="${a%.2.34}"; prev="$a"; done
rel="$CARGO_TARGET_DIR/$t/release"; mkdir -p "$rel"
for b in vamd vam vam-web-broker vam-schedule-broker vam-notify-broker vam-runtime; do
  if [ "$b" = vam ]; then
    printf '#!/bin/sh\necho "vam %s (%s %s)"\n' "$STUB_VERSION_LINE" "$VAM_GIT_SHA" "$VAM_COMMIT_DATE" > "$rel/$b"
  else printf '#!/bin/sh\necho %s\n' "$b" > "$rel/$b"; fi
  [ -z "${STUB_LEAK:-}" ] || [ "$b" != vamd ] || printf '%s/crates/x.rs\n' "$STUB_LEAK" >> "$rel/$b"
  chmod +x "$rel/$b"
done
STUB
cat > "$stubs/objdump" <<'STUB'
#!/usr/bin/env bash
printf 'DYNAMIC SYMBOL TABLE:\n0000 g DF .text 0000 GLIBC_2.17 memcpy\n0000 g DF .text 0000 GLIBC_%s getrandom\n' "${STUB_GLIBC:-2.34}"
STUB
chmod +x "$stubs"/*

log="$t_tmp/log"
# bld <extra env...> -- args: runs build.sh from the fixture with the stubs first on PATH
bld() {
  local envs=()
  while [ "$1" != -- ]; do envs+=("$1"); shift; done; shift
  : > "$log"
  env PATH="$stubs:$PATH" STUB_LOG="$log" CARGO_TARGET_DIR="$t_tmp/target" STUB_VERSION_LINE="0.1.0+dev.4" \
    CARGO_HOME="$t_tmp/cargo-home" RUSTUP_HOME="$t_tmp/rustup-home" "${envs[@]}" "$fx/scripts/release/build.sh" "$@"
}

echo "the node set for x86_64 linux gnu"
expect_ok "builds" bld -- x86_64-unknown-linux-gnu --set node --out "$t_tmp/o1"
for b in vamd vam vam-web-broker vam-schedule-broker vam-notify-broker; do [ -x "$t_tmp/o1/$b" ] || bad "$b not in --out"; done; ok "all five binaries are in --out"
[ ! -e "$t_tmp/o1/vam-runtime" ] && ok "the guest init is not part of the host set" || bad "vam-runtime in the host set"
build_line="$(grep 'auditable build' "$log")"
expect_match "cargo auditable build with the pinned toolchain, locked, for the host triple" "^cargo \+$clippy auditable build --release --locked --target x86_64-unknown-linux-gnu -p " "$build_line"
expect_nomatch "no zig and no glibc suffix on the target" "zig|\.2\.34" "$(cat "$log")"
expect_match "…of every shipped binary only" "-p vam-cli --bin vam -p vam-orchestrator --bin vamd -p vam-web --bin vam-web-broker -p vam-schedule --bin vam-schedule-broker -p vam-notify --bin vam-notify-broker\$" "$build_line"
expect_match "the checkout, CARGO_HOME and RUSTUP_HOME are remapped" "remap-path-prefix=$fx=/vam-src.*remap-path-prefix=$t_tmp/cargo-home=/cargo.*remap-path-prefix=$t_tmp/rustup-home=/rustup" "$(cat "$log")"
expect_match "host binaries are stripped" "STRIP=true" "$(cat "$log")"
expect_match "SOURCE_DATE_EPOCH is the commit time" "EPOCH=$epoch\$" "$(grep EPOCH= "$log" | head -n 1)"
expect_match "the commit, date and channel are passed" "SHA=$short DATE=2026-11-18 CHANNEL=dev INC=0" "$(cat "$log")"
expect_match "cargo audit bin runs with the advisories deny.toml ignores (before bin: --ignore is an audit option)" "audit --ignore RUSTSEC-2026-0001 .*bin " "$(cat "$log")"

echo "refusals"
expect_fail "a binary needing glibc 2.35" "needs GLIBC_2\.35, above the floor 2\.34" bld STUB_GLIBC=2.35 -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o2"
expect_ok "glibc 2.17 and 2.34 pass" bld STUB_GLIBC=2.34 -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o2"
expect_fail "a binary that still holds the checkout path" "still contains the build path" bld STUB_LEAK="$fx" -- x86_64-unknown-linux-gnu --set node --out "$t_tmp/o3"
expect_fail "cargo audit finding a vulnerability" "cargo audit bin failed" bld STUB_AUDIT_RC=1 -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o4"
expect_fail "a release build reporting another version" "not 'vam 0\.1\.0 \(\.\.\.\)'" bld VAM_BUILD_CHANNEL=release VERSION=0.1.0 STUB_VERSION_LINE="0.1.0+dev.4" -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o5"
expect_ok "a release build reporting the version" bld VAM_BUILD_CHANNEL=release VERSION=0.1.0 STUB_VERSION_LINE="0.1.0" -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o5"
expect_fail "a dev build reporting no +dev" "must report \+dev" bld STUB_VERSION_LINE="0.1.0" -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o6"
expect_fail "the wrong commit checked out" "not the commit to build" bld COMMIT=0000000000000000000000000000000000000000 -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o6"
expect_fail "an unknown target" "unknown target" bld -- riscv64-unknown-linux-gnu --set cli --out "$t_tmp/o6"
expect_fail "--set guest for a host target" "guest builds for x86_64-unknown-linux-musl only" bld -- x86_64-unknown-linux-gnu --set guest --out "$t_tmp/o6"
expect_fail "--set node for musl" "is not built for musl" bld -- x86_64-unknown-linux-musl --set node --out "$t_tmp/o6"
expect_fail "a bad channel" "must be release or dev" bld VAM_BUILD_CHANNEL=stable -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o6"
expect_fail "no --out" "--out DIR is required" bld -- x86_64-unknown-linux-gnu --set cli
echo dirty >> "$fx/deny.toml"
expect_fail "uncommitted changes" "uncommitted changes" bld -- x86_64-unknown-linux-gnu --set cli --out "$t_tmp/o6"
git -C "$fx" checkout -q -- deny.toml

echo "macOS and the guest init"
expect_ok "macOS builds natively (no objdump)" env PATH="$stubs:/usr/bin:/bin" STUB_LOG="$log" CARGO_TARGET_DIR="$t_tmp/target" STUB_VERSION_LINE="0.1.0+dev.4" "$fx/scripts/release/build.sh" aarch64-apple-darwin --set cli --out "$t_tmp/o7"
expect_match "…with cargo auditable build" "^cargo \+$clippy auditable build --release --locked --target aarch64-apple-darwin" "$(grep 'auditable build' "$log")"
expect_ok "the guest init goes through build-guest.sh" bld -- x86_64-unknown-linux-musl --set guest --out "$t_tmp/o8"
[ -x "$t_tmp/o8/vam-runtime" ] && ok "vam-runtime is in --out" || bad "no vam-runtime"
expect_match "…built for musl with the pinned toolchain" "rustup target add --toolchain $clippy x86_64-unknown-linux-musl" "$(cat "$log")"
finish
