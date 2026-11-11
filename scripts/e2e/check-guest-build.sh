#!/usr/bin/env bash
# Proves the guest process mode is absent from the default (rootfs) build and
# confined in the `process-guest` build. Container-runnable, no root needed.
# Builds both flavours of the vam-runtime binary one after the other and
# inspects each right after its build (cargo reuses target/debug/vam-runtime).
set -euo pipefail

cd "$(dirname "$0")/../.."
target_dir="${CARGO_TARGET_DIR:-target}"
bin="$target_dir/debug/vam-runtime"
fail=0
say() { printf '%s\n' "$*"; }
bad() { say "FAIL: $*"; fail=1; }
ok() { say "ok:   $*"; }

# Strings only the process-guest build may contain.
markers=(VAM_GUEST_VSOCK_UDS VAM_EGRESS_PROXY_SOCK VAM_EGRESS_PROXY_ADDR "uds-mux" "process-guest mode")
# libc symbols that act on the whole machine; none may be imported by process-guest.
forbidden_syms=(reboot umount umount2 mount clock_settime settimeofday sethostname pivot_root)

# No grep -q here: it would SIGPIPE strings, and pipefail would report a match as a miss.
has_string() { strings -a "$bin" | grep -F -- "$1" >/dev/null; }
imported_syms() { nm -D --undefined-only "$bin" | awk '{print $NF}' | sed 's/@.*//' | sort -u; }

say "== default build (the rootfs guest) =="
cargo build -q -p vam-runtime
for m in "${markers[@]}"; do
  if has_string "$m"; then bad "default binary contains process-mode marker: $m"; else ok "default binary has no '$m'"; fi
done
if has_string "0.0.0.0:3128"; then bad "default binary still names the wildcard proxy address 0.0.0.0:3128"; else ok "default binary has no wildcard proxy address"; fi
for s in reboot umount mount clock_settime; do
  if imported_syms | grep -x "$s" >/dev/null; then ok "default binary imports $s (the symbol check can fail)"; else bad "default binary lacks $s: symbol check is vacuous"; fi
done
if cargo tree -q -p vam-runtime -e normal,features | grep -E 'uds-transport|process-guest' >/dev/null; then
  bad "default dependency tree enables uds-transport/process-guest"
else
  ok "default dependency tree enables neither uds-transport nor process-guest"
fi

say "== process-guest build =="
cargo build -q -p vam-runtime --features process-guest
for m in "${markers[@]}"; do
  if has_string "$m"; then ok "feature binary has '$m' (the check can fail)"; else bad "feature binary lacks marker: $m"; fi
done
syms="$(imported_syms)"
for s in "${forbidden_syms[@]}"; do
  if printf '%s\n' "$syms" | grep -x "$s" >/dev/null; then bad "feature binary imports $s"; else ok "feature binary does not import $s"; fi
done
if has_string "0.0.0.0"; then bad "feature binary contains a 0.0.0.0 literal"; else ok "feature binary has no 0.0.0.0 literal"; fi
if has_string "/dev/urandom"; then bad "feature binary references /dev/urandom"; else ok "feature binary does not reference /dev/urandom"; fi

say "== sources =="
hits="$(grep -rln 'libc::kill(-1' crates/vam-runtime/src || true)"
if [ "$hits" = "crates/vam-runtime/src/bin/guest_init/pid1.rs" ]; then ok "libc::kill(-1) only in guest_init/pid1.rs"; else bad "libc::kill(-1) found in: $hits"; fi
if grep -nE 'process-guest|--features|--all-features' scripts/build-rootfs*.sh; then bad "a rootfs script enables a feature"; else ok "scripts/build-rootfs*.sh enable no feature"; fi

if [ "$fail" -ne 0 ]; then say "check-guest-build: FAILED"; exit 1; fi
say "check-guest-build: PASS"
