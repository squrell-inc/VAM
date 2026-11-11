#!/usr/bin/env bash
# Proves the fake Slack transport is absent from a normal vam-notify build and present only with
# the `fake-transport` feature, and that the broker never depends on vam-orchestrator.
# Builds the default flavour, inspects it, then the feature flavour (cargo reuses
# target/debug/vam-notify-broker, so each is inspected right after its build).
set -euo pipefail

cd "$(dirname "$0")/../.."
target_dir="${CARGO_TARGET_DIR:-target}"
bin="$target_dir/debug/vam-notify-broker"
fail=0
say() { printf '%s\n' "$*"; }
bad() { say "FAIL: $*"; fail=1; }
ok() { say "ok:   $*"; }

# Strings only a fake-transport binary may contain.
markers=(VAM_NOTIFY_FAKE_TRANSPORT sent.jsonl)
has_string() { strings -a "$bin" | grep -F -- "$1" >/dev/null; }

say "== default build =="
cargo build -q -p vam-notify
for m in "${markers[@]}"; do
  if has_string "$m"; then bad "default binary contains fake-transport marker: $m"; else ok "default binary has no '$m'"; fi
done
if "$bin" --help | grep -i fake >/dev/null; then bad "default --help names fake"; else ok "default --help names no fake flag or subcommand"; fi
if "$bin" --fake-transport >/dev/null 2>&1; then bad "default binary accepts --fake-transport"; else ok "default binary refuses --fake-transport"; fi
if "$bin" fake click >/dev/null 2>&1; then bad "default binary accepts the fake subcommand"; else ok "default binary refuses the fake subcommand"; fi
if cargo tree -q -p vam-notify -e normal,features | grep -F 'fake-transport' >/dev/null; then
  bad "default dependency tree enables fake-transport"
else
  ok "default dependency tree does not enable fake-transport"
fi
if cargo tree -q -p vam-notify -e normal | grep -F 'vam-orchestrator' >/dev/null; then
  bad "vam-notify depends on vam-orchestrator"
else
  ok "vam-notify does not depend on vam-orchestrator"
fi

say "== fake-transport build =="
cargo build -q -p vam-notify --features fake-transport
for m in "${markers[@]}"; do
  if has_string "$m"; then ok "feature binary has '$m' (the check can fail)"; else bad "feature binary lacks marker: $m"; fi
done

say "== manifests =="
# Only vam-notify's [features] and dev-dependencies may name the feature (check-manifests.sh
# guards process-guest the same way).
hits="$(grep -rn --include=Cargo.toml 'fake-transport' . --exclude-dir=target --exclude-dir=.git \
  | grep -v '^./brokers/vam-notify/Cargo.toml' || true)"
if [ -z "$hits" ]; then ok "no other Cargo.toml names fake-transport"; else bad "fake-transport named outside vam-notify: $hits"; fi
if awk '/^\[/{s=$0} /fake-transport/ && !/^[[:space:]]*#/ {print s}' brokers/vam-notify/Cargo.toml \
  | grep -vE '^\[(features|dev-dependencies)\]$' | grep . >/dev/null; then
  bad "vam-notify/Cargo.toml names fake-transport outside [features] and [dev-dependencies]"
else
  ok "vam-notify/Cargo.toml names fake-transport only in [features] and [dev-dependencies]"
fi

if [ "$fail" -ne 0 ]; then say "check-notify-build: FAILED"; exit 1; fi
say "check-notify-build: PASS"
