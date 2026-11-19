#!/usr/bin/env bash
# scripts/release/install.sh against local fixtures (no network): platform choice from a stub
# `uname`, version selection, the sha256 and attestation checks, archive safety and the hand-off to
# `vam dock` (a stub vam records its arguments). Every case runs under dash and bash, because the
# script is POSIX sh (macOS's /bin/sh is bash 3.2). shellcheck runs when it is installed
# (REQUIRE_SHELLCHECK=1 makes its absence a failure; the install-smoke job lints on Linux).
set -uo pipefail
. "$(dirname "$0")/helpers.sh"

inst="$release_dir/install.sh"
v=0.1.0-rc.2
repo=squrell-inc/VAM

echo "static checks"
if command -v shellcheck >/dev/null; then
  expect_ok "shellcheck -s sh" shellcheck -s sh "$inst"
elif [ -n "${REQUIRE_SHELLCHECK:-}" ]; then
  bad "shellcheck is required here (REQUIRE_SHELLCHECK=1) but not installed"
else
  echo "  skip shellcheck is not installed (the install-smoke job lints install.sh on Linux)"
fi
src="$(grep -v '^[[:space:]]*#' "$inst")"
nolines() { expect_eq "$1" "" "$(grep -nE -- "$2" <<< "$src")"; }
nolines "never uses sudo" '(^|[^a-z])sudo( |$)'
nolines "never pipes to a shell" '\| *(ba|da)?sh( |$)'
nolines "never uses eval" '(^|[^a-z])eval( |$)'
nolines "never reads standard input" '(^|[;&|(]|then |do )[[:space:]]*read( |$)'
expect_match "is POSIX sh" '^#!/bin/sh$' "$(head -n 1 "$inst")"
nolines "uses no bashisms (local, [[ ]], arrays)" '^[[:space:]]*local |\[\[|<<<|<\('

# --- fixtures -------------------------------------------------------------------------------------------
stubs="$t_tmp/stubs"; mkdir -p "$stubs"
cat > "$stubs/uname" <<'S'
#!/bin/sh
case "$1" in -s) echo "${STUB_S:-Linux}" ;; -m) echo "${STUB_M:-x86_64}" ;; *) echo "${STUB_S:-Linux}" ;; esac
S
cat > "$stubs/ldd" <<'S'
#!/bin/sh
echo "${STUB_LDD:-ldd (GNU libc) 2.39}"
S
cat > "$stubs/sysctl" <<'S'
#!/bin/sh
echo "${STUB_ARM64:-0}"
S
chmod +x "$stubs"/*

# a stub vam: prints its version; `dock` records its arguments and exits $DOCK_RC
mk_archive() { # <dir> <version> <triple> [vam-version-line]
  local d="$1" ver="$2" triple="$3" line="${4:-vam $2}" top
  top="vam-$ver-$triple"
  rm -rf "$t_tmp/pkg"; mkdir -p "$t_tmp/pkg/$top"
  cat > "$t_tmp/pkg/$top/vam" <<VAM
#!/bin/sh
if [ "\${1:-}" = "--version" ]; then echo "$line"; exit 0; fi
printf '%s\n' "\$*" >> "\${DOCK_LOG:-/dev/null}"
exit "\${DOCK_RC:-0}"
VAM
  chmod 755 "$t_tmp/pkg/$top/vam"
  tar -czf "$d/$top.tar.gz" -C "$t_tmp/pkg" "$top"
}
mk_sums() { # <dir>: SHA256SUMS over every file in the directory except itself
  (cd "$1" && for f in $(ls | grep -v '^SHA256SUMS$'); do printf '%s  %s\n' "$(sha256sum "$f" | awk '{print $1}')" "$f"; done > SHA256SUMS)
}
mk_release() { # <dir> <version>: every CLI triple, a node bundle and the runtime, like a real release
  local d="$1" ver="$2" t
  rm -rf "$d"; mkdir -p "$d"
  for t in x86_64-unknown-linux-gnu aarch64-unknown-linux-gnu aarch64-apple-darwin x86_64-apple-darwin; do mk_archive "$d" "$ver" "$t"; done
  echo node > "$d/vam-node-$ver-x86_64-unknown-linux-gnu.tar.gz"
  echo runtime > "$d/vam-runtime-$ver-x86_64-unknown-linux-musl"
  mk_sums "$d"
}

rel="$t_tmp/rel"
tmpd="$t_tmp/tmpd"; mkdir -p "$tmpd"
export DOCK_LOG="$t_tmp/dock.log"

# run_inst <sh> <args...>: the installer in a fresh HOME, stubs first on PATH, TMPDIR a private directory
run_inst() {
  local sh="$1"; shift
  env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" "$@"
}
fresh() { rm -rf "$t_tmp/home" "$DOCK_LOG"; mkdir -p "$t_tmp/home"; unset DOCK_RC STUB_S STUB_M STUB_LDD STUB_ARM64; }
leftover() { ls -A "$tmpd" | tr '\n' ' '; }

mk_release "$rel" "$v"

for sh in dash bash; do
  echo "== under $sh"
  command -v "$sh" >/dev/null || { bad "$sh is not installed"; continue; }

  echo "platform"
  for case in "Linux x86_64 x86_64-unknown-linux-gnu" "Linux aarch64 aarch64-unknown-linux-gnu" \
              "Darwin arm64 aarch64-apple-darwin" "Darwin x86_64 x86_64-apple-darwin"; do
    set -- $case
    fresh; STUB_S="$1" STUB_M="$2" run_cmd env STUB_S="$1" STUB_M="$2" PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$rel" --version "v$v"
    if [ "$T_RC" = 0 ]; then expect_match "$1 $2 takes the $3 archive" "vam-$v-$3.tar.gz" "$T_OUT"
    else bad "$1 $2 failed (exit $T_RC)" "$T_OUT"; fi
  done
  fresh; run_cmd env STUB_S=Darwin STUB_M=x86_64 STUB_ARM64=1 PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$rel" --version "v$v"
  expect_match "a Rosetta shell on Apple silicon takes the arm64 archive" "vam-$v-aarch64-apple-darwin.tar.gz" "$T_OUT"
  fresh; STUB_S=Linux STUB_M=armv7l expect_fail "an unsupported CPU is refused" "unsupported CPU 'armv7l'" env STUB_M=armv7l PATH="$stubs:$PATH" "$sh" "$inst" --from "$rel" --version "v$v"
  fresh; expect_fail "i686 is refused" "unsupported CPU 'i686'" env STUB_M=i686 PATH="$stubs:$PATH" "$sh" "$inst" --from "$rel" --version "v$v"
  expect_fail "Windows is refused (v0.2.0)" "Windows is not supported yet.*v0\.2\.0" env STUB_S=MINGW64_NT-10.0 PATH="$stubs:$PATH" "$sh" "$inst" --from "$rel" --version "v$v"
  expect_fail "another OS is refused" "unsupported operating system 'FreeBSD'" env STUB_S=FreeBSD PATH="$stubs:$PATH" "$sh" "$inst" --from "$rel" --version "v$v"
  expect_fail "musl is refused" "musl" env STUB_LDD="musl libc (x86_64)" PATH="$stubs:$PATH" "$sh" "$inst" --from "$rel" --version "v$v"

  echo "a good install from local files"
  fresh; run_cmd run_inst "$sh" --from "$rel" --version "v$v"
  expect_eq "exits 0" 0 "$T_RC"
  expect_match "prints the detected target first" "detected Linux x86_64" "$T_OUT"
  expect_match "prints the hash check" "hash ok \(sha256 [0-9a-f]{64}\)" "$T_OUT"
  expect_match "says the attestation was not checked for local files" "attestation not checked" "$T_OUT"
  expect_eq "runs vam dock with no other argument" "dock" "$(cat "$DOCK_LOG")"
  expect_eq "leaves no temporary files" "" "$(leftover)"
  fresh; run_cmd run_inst "$sh" --from "$rel"
  expect_eq "the version is read from SHA256SUMS without --version" 0 "$T_RC"
  expect_match "…and named" "installing vam $v " "$T_OUT"
  fresh; run_cmd run_inst "$sh" --from "$rel" --version "$v" --dir "$t_tmp/bin dir" --no-modify-path
  expect_eq "--dir and --no-modify-path (and a bare version) reach vam dock" "dock --dir $t_tmp/bin dir --no-modify-path" "$(cat "$DOCK_LOG")"
  fresh; DOCK_RC=3 expect_fail "a failing vam dock fails the install" "vam dock failed \(exit 3\)" env DOCK_RC=3 PATH="$stubs:$PATH" HOME="$t_tmp/home" "$sh" "$inst" --from "$rel" --version "v$v"
  fresh; run_cmd env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$rel" --version "v$v" < /dev/null
  expect_eq "works with standard input closed (curl | sh)" 0 "$T_RC"

  echo "no network with --from"
  netstub="$t_tmp/net"; mkdir -p "$netstub"
  for n in curl wget; do printf '#!/bin/sh\necho "%s called: $*" >> "%s/calls"\nexit 99\n' "$n" "$netstub" > "$netstub/$n"; chmod +x "$netstub/$n"; done
  fresh; rm -f "$netstub/calls"
  run_cmd env PATH="$netstub:$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$rel" --version "v$v"
  expect_eq "--from installs without touching curl or wget" "0 " "$T_RC $([ -e "$netstub/calls" ] && cat "$netstub/calls")"

  echo "integrity: nothing is installed"
  bad_rel="$t_tmp/badrel"
  mk_release "$bad_rel" "$v"; : > "$bad_rel/SHA256SUMS"
  fresh; expect_fail "an empty SHA256SUMS" "SHA256SUMS does not list vam-$v-x86_64-unknown-linux-gnu.tar.gz" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  expect_eq "…and vam dock never ran" "" "$(cat "$DOCK_LOG" 2>/dev/null)"
  mk_release "$bad_rel" "$v"; grep -v 'x86_64-unknown-linux-gnu.tar.gz$' "$bad_rel/SHA256SUMS" | grep -v node > "$bad_rel/s"; mv "$bad_rel/s" "$bad_rel/SHA256SUMS"
  fresh; expect_fail "a missing line for this archive" "does not list vam-$v-x86_64-unknown-linux-gnu" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  mk_release "$bad_rel" "$v"; echo tamper >> "$bad_rel/vam-$v-x86_64-unknown-linux-gnu.tar.gz"
  fresh; expect_fail "a tampered archive" "hash mismatch for vam-$v-x86_64-unknown-linux-gnu.tar.gz: SHA256SUMS lists [0-9a-f]{64}, the download is [0-9a-f]{64}; nothing was installed" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  expect_eq "…vam dock never ran" "" "$(cat "$DOCK_LOG" 2>/dev/null)"
  expect_eq "…and the temporary files are gone" "" "$(leftover)"
  expect_eq "…and HOME is untouched" "" "$(ls -A "$t_tmp/home")"
  mk_release "$bad_rel" "$v"; sed -i.bak 's/^\([0-9a-f]\{4\}\)[0-9a-f]*  /\1  /' "$bad_rel/SHA256SUMS"
  fresh; expect_fail "a malformed hash" "malformed hash" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  mk_release "$bad_rel" "$v"; sed -i.bak 's/  vam-/ *vam-/' "$bad_rel/SHA256SUMS"
  fresh; run_cmd env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  expect_eq "the '<hash> *<name>' form is read too" 0 "$T_RC"
  mk_release "$bad_rel" "$v"; sed -i.bak 's/^\([0-9a-f]*\)  \(vam-.*\)$/\1  \2  extra/' "$bad_rel/SHA256SUMS"
  fresh; expect_fail "a line with a third field is not a match" "does not list" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  mk_release "$bad_rel" "$v"; rm "$bad_rel/vam-$v-aarch64-unknown-linux-gnu.tar.gz"
  fresh; expect_fail "a missing archive" "has no vam-$v-aarch64-unknown-linux-gnu.tar.gz" env STUB_M=aarch64 PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"

  echo "archive safety and the smoke run"
  mk_release "$bad_rel" "$v"
  top="vam-$v-x86_64-unknown-linux-gnu"
  rm -rf "$t_tmp/evil"; mkdir -p "$t_tmp/evil/$top"; cp "$t_tmp/pkg/$top/vam" "$t_tmp/evil/$top/vam" 2>/dev/null || true
  mk_archive "$bad_rel" "$v" x86_64-unknown-linux-gnu
  (cd "$t_tmp/pkg" && echo x > evil && tar -czf "$bad_rel/$top.tar.gz" --transform "s,^evil,$top/../evil," "$top" evil 2>/dev/null) || true
  mk_sums "$bad_rel"
  fresh; expect_fail "a parent-relative member name" "parent-relative path" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  mk_release "$bad_rel" "$v"; mk_archive "$bad_rel" "$v" x86_64-unknown-linux-gnu "vam 9.9.9"; mk_sums "$bad_rel"
  fresh; expect_fail "a vam that reports another version" "does not report vam $v \(printed 'vam 9.9.9'\)" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  mk_release "$bad_rel" "$v"; mk_archive "$bad_rel" "$v" x86_64-unknown-linux-gnu "vam $v (abc1234 2026-11-19)"; mk_sums "$bad_rel"
  fresh; run_cmd env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  expect_eq "extra text after the version is fine (the build line)" "0" "$T_RC"
  mk_release "$bad_rel" "$v"; mk_archive "$bad_rel" "$v" x86_64-unknown-linux-gnu "vam $v+dev.4 (abc1234 2026-11-19)"; mk_sums "$bad_rel"
  fresh; run_cmd env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  expect_eq "--from accepts a development build of the version (a pull request's build-only run)" "0" "$T_RC"
  mk_archive "$bad_rel" "$v" x86_64-unknown-linux-gnu "vam $v+devious (abc1234)"; mk_sums "$bad_rel"
  fresh; expect_fail "…but not another suffix" "does not report vam $v" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"
  mk_release "$bad_rel" "$v"; echo notatar > "$bad_rel/vam-$v-x86_64-unknown-linux-gnu.tar.gz"; mk_sums "$bad_rel"
  fresh; expect_fail "a file that is not an archive" "not a readable tar.gz" env PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --from "$bad_rel" --version "v$v"

  echo "attestation"
  gh="$t_tmp/gh"; mkdir -p "$gh"
  printf '#!/bin/sh\nprintf "%%s\\n" "$*" >> "%s/log"\nexit "${GH_RC:-0}"\n' "$gh" > "$gh/gh"; chmod +x "$gh/gh"
  # a release served from a local directory, so the attestation step runs (--from skips it)
  srv="$t_tmp/srv"; rm -rf "$srv"; mkdir -p "$srv/v$v"; cp "$rel"/* "$srv/v$v/"; echo "v$v" > "$srv/latest"
  if command -v curl >/dev/null; then
    base="file://$srv"
    fresh; rm -f "$gh/log"
    run_cmd env VAM_INSTALL_BASE_URL="$base" PATH="$gh:$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --version "v$v"
    expect_eq "a served release installs" 0 "$T_RC"
    expect_match "gh attestation verify names the repository and signer workflow" "^attestation verify vam-$v-x86_64-unknown-linux-gnu.tar.gz -R $repo --signer-workflow $repo/.github/workflows/release.yml\$" "$(cat "$gh/log")"
    expect_match "…and success is reported" "attestation verified with gh" "$T_OUT"
    fresh; rm -f "$gh/log"
    run_cmd env GH_RC=1 VAM_INSTALL_BASE_URL="$base" PATH="$gh:$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --version "v$v"
    expect_eq "a refused attestation fails" 1 "$T_RC"
    expect_match "…with the vam upgrade wording" '`gh attestation verify` refused the download; nothing was installed\. If gh is not signed in, run `gh auth login`\.' "$T_OUT"
    expect_eq "…and vam dock never ran" "" "$(cat "$DOCK_LOG" 2>/dev/null)"
    fresh
    nogh="$t_tmp/nogh"; mkdir -p "$nogh"; for n in sh dash bash env dirname basename mktemp rm cp mkdir cat awk grep sed tr head wc tar chmod ls curl gzip sha256sum cut printf uname ldd sysctl; do p="$(command -v "$n" 2>/dev/null)" && ln -sf "$p" "$nogh/$n"; done
    ln -sf "$stubs/uname" "$nogh/uname"; ln -sf "$stubs/ldd" "$nogh/ldd"; ln -sf "$stubs/sysctl" "$nogh/sysctl"
    run_cmd env VAM_INSTALL_BASE_URL="$base" PATH="$nogh" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --version "v$v"
    expect_eq "without gh the install goes ahead" 0 "$T_RC$([ "$T_RC" = 0 ] || echo " $T_OUT")"
    expect_match "…saying how to check, as vam upgrade does" "attestation not checked: gh is not on the PATH\. To check it, install gh, download vam-$v-x86_64-unknown-linux-gnu\.tar\.gz from the release, and run: gh attestation verify vam-$v-x86_64-unknown-linux-gnu\.tar\.gz -R $repo --signer-workflow $repo/\.github/workflows/release\.yml" "$T_OUT"

    mk_archive "$srv/v$v" "$v" x86_64-unknown-linux-gnu "vam $v+dev.4 (abc1234)"; mk_sums "$srv/v$v"
    fresh; expect_fail "a downloaded release must be a release build, not a +dev one" "does not report vam $v \(printed 'vam $v\+dev\.4" env VAM_INSTALL_BASE_URL="$base" PATH="$gh:$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --version "v$v"
    mk_archive "$srv/v$v" "$v" x86_64-unknown-linux-gnu; mk_sums "$srv/v$v"

    echo "version selection through a local release server"
    fresh; run_cmd env VAM_INSTALL_BASE_URL="$base" PATH="$gh:$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst"
    expect_eq "the latest release is read from \$BASE/latest" 0 "$T_RC"
    expect_match "…and named" "installing vam $v " "$T_OUT"
    : > "$srv/latest"
    fresh; expect_fail "no final release yet" "no final release yet; pass --version v0\.1\.0-rc\.N" env VAM_INSTALL_BASE_URL="$base" PATH="$gh:$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst"
    rm -f "$srv/latest"
    fresh; expect_fail "…also when there is no latest at all" "no final release yet" env VAM_INSTALL_BASE_URL="$base" PATH="$gh:$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst"
    fresh; expect_fail "an unpublished version" "cannot download SHA256SUMS for v9\.9\.9" env VAM_INSTALL_BASE_URL="$base" PATH="$gh:$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --version v9.9.9
  else
    echo "  skip curl is not installed (the served-release cases need it)"
  fi

  echo "the test seam is local only"
  for u in "https://example.com/x" "http://example.com" "http://127.0.0.1.evil.example/x" "http://localhostile/x" "ftp://127.0.0.1/x" "/tmp/x"; do
    fresh; expect_fail "VAM_INSTALL_BASE_URL=$u is refused" "must be a file:// or loopback http:// URL" env VAM_INSTALL_BASE_URL="$u" PATH="$stubs:$PATH" HOME="$t_tmp/home" "$sh" "$inst" --version "v$v"
  done
  fresh; expect_fail "a loopback URL that is not listening fails to download" "cannot download SHA256SUMS" env VAM_INSTALL_BASE_URL="http://127.0.0.1:9" PATH="$stubs:$PATH" HOME="$t_tmp/home" TMPDIR="$tmpd" "$sh" "$inst" --version "v$v"

  echo "arguments"
  for ver in "1" "0.1" "01.2.3" "0.1.0-rc" "0.1.0-rc.01" "0.1.0+build" "0.1.0-beta.1" "latest" ""; do
    fresh; expect_fail "--version '$ver' is refused" "--version must look like" env PATH="$stubs:$PATH" "$sh" "$inst" --from "$rel" --version "$ver"
  done
  fresh; run_cmd run_inst "$sh" --from "$rel" --version "0.1.0-rc.2"
  expect_eq "an rc is installed when named" 0 "$T_RC"
  fresh; expect_fail "an unknown flag" "unknown argument: --pre" env PATH="$stubs:$PATH" "$sh" "$inst" --pre
  fresh; expect_fail "a flag without its value" "--version needs a value" env PATH="$stubs:$PATH" "$sh" "$inst" --version
  fresh; expect_fail "a stray positional argument" "unknown argument: foo" env PATH="$stubs:$PATH" "$sh" "$inst" foo
  fresh; expect_fail "--from a missing directory" "no such directory" env PATH="$stubs:$PATH" "$sh" "$inst" --from "$t_tmp/nope" --version "v$v"
  run_cmd "$sh" "$inst" --help
  expect_eq "--help exits 0" 0 "$T_RC"
  expect_match "…and prints the usage" "usage: install.sh" "$T_OUT"
  expect_eq "no step ever left the temporary directory behind" "" "$(leftover)"
done

finish
