#!/usr/bin/env bash
# scan.sh on fixture archives: a clean set passes; each planted secret, refused name, build path and
# unsafe archive member fails with its own reason. gitleaks is a stub (GITLEAKS) except for the one
# case that runs the real binary when it is installed (REQUIRE_GITLEAKS=1 makes its absence a
# failure; the assemble job sets it). Also: export-public.sh reads the same refused-names list.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"

scan="$release_dir/scan.sh"
v=0.1.0-rc.1
key="vam_sk_dev_$(printf 'ab%.0s' $(seq 32))"   # 64 hex digits

stub="$t_tmp/gitleaks-stub"
cat > "$stub" <<'STUB'
#!/usr/bin/env bash
echo "gitleaks $*" >> "${STUB_LOG:-/dev/null}"
exit "${STUB_RC:-0}"
STUB
chmod +x "$stub"
export GITLEAKS="$stub"
export STUB_LOG="$t_tmp/stub.log"

# new_set <name>: an assets directory holding one cli archive and the standalone runtime
new_set() {
  S="$t_tmp/$1"; rm -rf "$S"; mkdir -p "$S/assets" "$S/pkg/vam-$v-x"
  printf '#!/bin/sh\necho vam\n' > "$S/pkg/vam-$v-x/vam"; chmod +x "$S/pkg/vam-$v-x/vam"
  printf 'MIT\n' > "$S/pkg/vam-$v-x/LICENSE-MIT"
  printf 'rt\n' > "$S/assets/vam-runtime-$v-x86_64-unknown-linux-musl"
  pack
}
# pack: (re)builds the archive from $S/pkg
pack() { tar -czf "$S/assets/vam-$v-x.tar.gz" -C "$S/pkg" "vam-$v-x"; }

echo "a clean set"
new_set clean
expect_ok "a clean set passes" "$scan" "$S/assets"
expect_match "gitleaks ran over the unpacked tree with the repo config" "gitleaks dir .* -c .*\.gitleaks\.toml" "$(cat "$STUB_LOG")"

echo "a bare vam_sk_ prefix is a literal in the binaries, not a finding"
new_set prefix
printf 'Authorization: Bearer vam_sk_... required\nvam_sk_dev_keys\nvam_sk_948fbc102e3b4a\n\\bvam_sk_[a-zA-Z0-9_-]{16,}\\b\n' > "$S/pkg/vam-$v-x/vam"; pack
expect_ok "the prefix, the placeholder and the redaction regex pass" "$scan" "$S/assets"

echo "a whole minted key"
new_set key_text
printf 'token=%s\n' "$key" > "$S/pkg/vam-$v-x/notes.txt"; pack
expect_fail "a planted key in a text file" "minted API key.*vam-$v-x/notes\.txt" "$scan" "$S/assets"
new_set key_bin
printf 'xx\0\0%s\0yy' "$key" > "$S/pkg/vam-$v-x/vam"; pack
expect_fail "a planted key in a binary" "minted API key.*vam-$v-x/vam" "$scan" "$S/assets"
new_set key_standalone
printf 'ELF\0%s\0' "${key/vam_sk_dev_/vam_sk_}" > "$S/assets/vam-runtime-$v-x86_64-unknown-linux-musl"
expect_fail "a key without the dev segment in the standalone runtime" "minted API key.*vam-runtime-" "$scan" "$S/assets"

echo "refused names"
for n in .env x.ext4 vmlinux-6.1 run.log; do
  new_set "name_$n"
  echo x > "$S/pkg/vam-$v-x/$n"; pack
  expect_fail "$n in an archive" "refused file name.*$(printf '%s' "$n" | sed 's/\./\\./g')" "$scan" "$S/assets"
done
new_set name_standalone
echo x > "$S/assets/rootfs.ext4"
expect_fail "a standalone rootfs.ext4 asset" "refused file name.*rootfs\.ext4" "$scan" "$S/assets"

echo "build machine paths"
new_set path_host
printf 'panic at /home/runner/work/VAM/VAM/crates/vam-cli/src/main.rs\0' > "$S/pkg/vam-$v-x/vam"; pack
expect_fail "a runner checkout path in a host binary" "build machine path.*/home/runner/work" "$scan" "$S/assets"
new_set path_mac
printf '/Users/runner/work/VAM\0' > "$S/pkg/vam-$v-x/vam"; pack
expect_fail "a macOS runner path" "build machine path.*/Users/runner" "$scan" "$S/assets"
new_set path_container
printf '/usr/local/cargo/registry/src/x\0' > "$S/pkg/vam-$v-x/vam"; pack
expect_fail "a container cargo home path" "build machine path.*/usr/local/cargo" "$scan" "$S/assets"
new_set path_registry_host
printf '/home/runner/.cargo/registry/src/index.crates.io-1/a-1.0/src/lib.rs\0' > "$S/pkg/vam-$v-x/vam"; pack
expect_fail "a cargo registry path in a host binary" "build machine path.*/home/runner/.cargo/registry" "$scan" "$S/assets"
new_set path_guest_registry
printf '/home/runner/.cargo/registry/src/index.crates.io-1/a-1.0/src/lib.rs\0' > "$S/assets/vam-runtime-$v-x86_64-unknown-linux-musl"
expect_ok "a cargo registry path in the unremapped guest init is tolerated" "$scan" "$S/assets"
expect_match "and reported as a notice" "note .*vam-runtime.*cargo registry path" "$T_OUT"
new_set path_guest_outdir
printf '/home/runner/work/VAM/VAM/target/x86_64-unknown-linux-musl/release/build/vam-spec-d416f795f3876ce0/out/vam.v1.rs\0' > "$S/assets/vam-runtime-$v-x86_64-unknown-linux-musl"
expect_ok "a build-script output path (target/.../build/*/out/) in the guest init is tolerated" "$scan" "$S/assets"
expect_match "and reported as a notice" "note .*vam-runtime.*build-script output path" "$T_OUT"
new_set path_host_outdir
printf '/home/runner/work/VAM/VAM/target/x86_64-unknown-linux-gnu/release/build/vam-spec-1/out/vam.v1.rs\0' > "$S/pkg/vam-$v-x/vam"; pack
expect_fail "the same build-script output path in a host binary fails" "build machine path.*/home/runner/work" "$scan" "$S/assets"
new_set path_guest_target_other
printf '/home/runner/work/VAM/VAM/target/x86_64-unknown-linux-musl/release/deps/x.rlib\0' > "$S/assets/vam-runtime-$v-x86_64-unknown-linux-musl"
expect_fail "another target/ path in the guest init still fails" "build machine path.*/home/runner/work" "$scan" "$S/assets"
new_set path_guest_checkout
printf '/home/runner/.cargo/registry/src/a/lib.rs\0/home/runner/work/VAM/VAM/crates/vam-runtime/src/main.rs\0' > "$S/assets/vam-runtime-$v-x86_64-unknown-linux-musl"
expect_fail "the checkout path in the guest init still fails" "build machine path.*/home/runner/work" "$scan" "$S/assets"
new_set path_guest_in_bundle
mkdir -p "$S/pkg/vam-$v-x"; printf '/home/runner/.cargo/git/checkouts/x\0' > "$S/pkg/vam-$v-x/vam-runtime"; pack
expect_ok "the same tolerance for vam-runtime inside a bundle" "$scan" "$S/assets"

echo "archive hygiene"
new_set sym
ln -s /etc/passwd "$S/pkg/vam-$v-x/link"; pack
expect_fail "a symlink member" "not a regular file or directory" "$scan" "$S/assets"
new_set dotdot
python3 -I - "$S" "$v" <<'PY'
import sys, tarfile, io
s, v = sys.argv[1:3]
with tarfile.open(f"{s}/assets/vam-{v}-x.tar.gz", "w:gz") as t:
    for name in (f"vam-{v}-x/../escape.txt", "/abs.txt"):
        i = tarfile.TarInfo(name); i.size = 1
        t.addfile(i, io.BytesIO(b"x"))
PY
expect_fail "a .. member" "climbs out of the archive" "$scan" "$S/assets"
expect_match "an absolute member" "absolute path" "$T_OUT"
new_set notar
echo "not a tar" > "$S/assets/vam-$v-x.tar.gz"
expect_fail "a corrupt archive" "not a readable tar.gz" "$scan" "$S/assets"
new_set empty
rm -f "$S/assets"/*
expect_fail "no assets" "no assets" "$scan" "$S/assets"

echo "every check runs"
new_set many
echo x > "$S/pkg/vam-$v-x/.env"; printf '%s\0/home/runner/work\0' "$key" > "$S/pkg/vam-$v-x/vam"; pack
run_cmd "$scan" "$S/assets"
expect_match "the refused name is reported" "refused file name" "$T_OUT"
expect_match "the key is reported" "minted API key" "$T_OUT"
expect_match "the path is reported" "build machine path" "$T_OUT"
expect_match "the count is reported" "3 finding" "$T_OUT"

echo "gitleaks"
new_set gl
STUB_RC=1 expect_fail "a gitleaks finding fails the scan" "gitleaks reported findings" "$scan" "$S/assets"
expect_fail "a missing gitleaks fails the scan" "gitleaks is not installed" env GITLEAKS=/nonexistent/gitleaks "$scan" "$S/assets"
real="$(command -v gitleaks || true)"
[ -n "$real" ] || { [ -x "$HOME/.local/bin/gitleaks" ] && real="$HOME/.local/bin/gitleaks"; }
if [ -n "$real" ]; then
  new_set gl_real_clean
  expect_ok "the real gitleaks passes a clean set" env GITLEAKS="$real" "$scan" "$S/assets"
  new_set gl_real_planted
  printf 'GITHUB_TOKEN=ghp_R2d9KxQ7mZ4vB8nL1cY6tW3sH5jF0aE2pU9q\n' > "$S/pkg/vam-$v-x/config.env.txt"; pack
  expect_fail "the real gitleaks catches a planted token" "gitleaks reported findings" env GITLEAKS="$real" "$scan" "$S/assets"
elif [ "${REQUIRE_GITLEAKS:-0}" = 1 ]; then
  bad "gitleaks is required here (REQUIRE_GITLEAKS=1) but not installed: scripts/release/install-tool.sh gitleaks"
else
  echo "  skip gitleaks is not installed (scripts/release/install-tool.sh gitleaks); the assemble job requires it"
fi

echo "export-public.sh reads the same refused-names list"
x="$t_tmp/export"; mkdir -p "$x"
(
  cd "$x" || exit 1
  git_quiet init -q . && mkdir -p scripts e2e/world/fixtures
  cp "$repo_root/scripts/export-public.sh" scripts/
  cp "$repo_root/.gitleaks.toml" .
  printf '*.ext4\nvmlinux*\n.env\n*.log\n*.secretdump\n' > scripts/refused-names.txt
  echo ok > README.md; echo x > data.secretdump; echo x > e2e/world/fixtures/rootfs.ext4
  git_quiet add -A -f && git_quiet commit -q -m base
)
gl_ok="$t_tmp/gl-ok"; printf '#!/bin/sh\nexit 0\n' > "$gl_ok"; chmod +x "$gl_ok"
expect_fail "a name added to the list is refused by the export" "data\.secretdump" env GITLEAKS="$gl_ok" bash "$x/scripts/export-public.sh" HEAD "$t_tmp/export-out"
expect_nomatch "the fixtures directory stays exempt" "rootfs\.ext4" "$T_OUT"
(cd "$x" && git_quiet rm -q data.secretdump && git_quiet commit -q -m drop)
expect_ok "without the planted file the export passes" env GITLEAKS="$gl_ok" bash "$x/scripts/export-public.sh" HEAD "$t_tmp/export-out2"
finish
