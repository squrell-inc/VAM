#!/usr/bin/env bash
# Fails when a process guest left files behind.
#   check-leftovers.sh --list                      print the guest files in /tmp (one per line)
#   check-leftovers.sh --after "<list>" <out-dir>  fail if the process-guest state dir under
#                                                  <out-dir>/vamd/state is not empty, or /tmp has
#                                                  guest files that <list> did not have
# Guest files: /tmp/vam-vsock-* (base and _<port> sockets) and /tmp/vam-egress-*.
set -euo pipefail

list() { ls -d /tmp/vam-vsock-* /tmp/vam-egress-* 2>/dev/null | sort || true; }

if [ "${1:-}" = "--list" ]; then list; exit 0; fi
if [ "${1:-}" != "--after" ] || [ "$#" -ne 3 ]; then
  echo "usage: check-leftovers.sh --list | --after \"<list>\" <out-dir>" >&2
  exit 2
fi
before="$2" out="$3"
fail=0

state="$out/vamd/state/process-guest"
if [ -d "$state" ] && [ -n "$(ls -A "$state")" ]; then
  echo "LEFTOVER: process-guest state dir is not empty: $state"
  ls -la "$state"
  fail=1
fi

after="$(list)"
new="$(comm -13 <(printf '%s\n' "$before" | sort) <(printf '%s\n' "$after" | sort))"
if [ -n "$new" ]; then
  echo "LEFTOVER: guest files left in /tmp:"
  printf '%s\n' "$new"
  fail=1
fi

if [ "$fail" -ne 0 ]; then echo "check-leftovers: FAILED"; exit 1; fi
echo "check-leftovers: ok (no process-guest state, no new /tmp/vam-vsock-* or /tmp/vam-egress-* files)"
