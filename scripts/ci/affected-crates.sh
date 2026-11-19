#!/usr/bin/env bash
# Which crates' tests a change needs: the fast tier's `cargo test` scope (rust.yml `changes` job,
# scripts/ci/local.sh --fast).
#
#   scripts/ci/affected-crates.sh [--base REF] [--files FILE|-]
#
# Changed paths come from FILE (one per line, `-` for stdin) or from `git diff --name-only
# REF...HEAD` (REF defaults to origin/main). Prints three lines:
#   scope=workspace|affected|none
#   crates=<space-separated workspace packages>   (empty unless scope=affected)
#   reason=<why>
#
# - A file under a member crate (crates/<x>/, brokers/<x>/) selects that crate and every workspace
#   crate that depends on it, transitively (normal, build and dev edges, from `cargo metadata`).
# - A data directory a crate's tests read (examples/, e2e/, docs/api/, ...) selects that crate only.
# - Forced workspace: root Cargo.toml, Cargo.lock, rust-toolchain*, .cargo/, proto/, third_party/,
#   .github/workflows/, or any file of a core crate (CORE_CRATES below).
# - Anything else (design/, .cursor/, other scripts) selects nothing.
#
# Core crates: vam-spec (the data contracts; every member but vam-firecracker depends on it) and
# vam-security (every member but vam-spec, vam-firecracker and vam-schedule depends on it). Their
# reverse closure is already most of the workspace; forcing it keeps the rule plain.
# Env: AFFECTED_METADATA=<file> reads `cargo metadata --no-deps` JSON from a file (tests).
set -euo pipefail

CORE_CRATES="vam-spec vam-security"

base="origin/main"
files=""
while [ $# -gt 0 ]; do
  case "$1" in
    --base) shift; base="${1:?--base needs a ref}" ;;
    --files) shift; files="${1:?--files needs a path}" ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) echo "affected-crates: unknown argument: $1" >&2; exit 2 ;;
  esac
  shift
done

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo"
command -v jq >/dev/null || { echo "affected-crates: jq is required" >&2; exit 2; }

if [ -n "$files" ]; then
  if [ "$files" = - ]; then changed="$(cat)"; else changed="$(cat "$files")"; fi
else
  changed="$(git diff --name-only "$base...HEAD")"
fi

if [ -n "${AFFECTED_METADATA:-}" ]; then
  meta="$(cat "$AFFECTED_METADATA")"
else
  meta="$(cargo metadata --format-version 1 --no-deps --offline 2>/dev/null || cargo metadata --format-version 1 --no-deps)"
fi

# "<dir relative to the repo>\t<package>" for every workspace member.
members="$(jq -r --arg root "$repo/" '.packages[] | [(.manifest_path | ltrimstr($root) | sub("/Cargo.toml$"; "")), .name] | @tsv' <<< "$meta")"
# "<package>\t<workspace package it depends on>" edges.
edges="$(jq -r '[.packages[].name] as $m | .packages[] | .name as $n | .dependencies[] | select(.name as $d | $m | index($d)) | select(.name != $n) | [$n, .name] | @tsv' <<< "$meta")"

emit() { printf 'scope=%s\ncrates=%s\nreason=%s\n' "$1" "$2" "$3"; }

declare -A seed=() direct=()
forced=""
while IFS= read -r f; do
  [ -n "$f" ] || continue
  case "$f" in
    Cargo.toml|Cargo.lock|rust-toolchain|rust-toolchain.toml|.cargo/*|proto/*|third_party/*|.github/workflows/*)
      forced="${forced:-$f}"; continue ;;
  esac
  pkg=""
  while IFS=$'\t' read -r dir name; do
    [ -n "$dir" ] || continue
    case "$f" in "$dir"/*) pkg="$name"; break ;; esac
  done <<< "$members"
  if [ -n "$pkg" ]; then
    case " $CORE_CRATES " in *" $pkg "*) forced="${forced:-$f (core crate $pkg)}"; continue ;; esac
    seed[$pkg]=1
    continue
  fi
  # Data a crate's tests read: those crates only, not their dependents.
  case "$f" in
    examples/*)                 for p in vam-spec vam-security vam-orchestrator vam-cli; do direct[$p]=1; done ;;
    spec/*)                     direct[vam-spec]=1 ;;
    e2e/*)                      for p in vam-e2e vam-spec vam-orchestrator vam-cli vam-schedule; do direct[$p]=1; done ;;
    bench/*)                    direct[vam-e2e]=1 ;;
    docs/api/*)                 direct[vam-spec]=1; direct[vam-orchestrator]=1 ;;
    docs/*)                     direct[vam-e2e]=1 ;;
    scripts/build-rootfs*|scripts/base/*|scripts/assets.pins|scripts/release/build-guest.sh)
                                direct[vam-cli]=1; direct[vam-runtime]=1 ;;
    scripts/bench/*|scripts/web-broker/*) direct[vam-orchestrator]=1 ;;
  esac
done <<< "$changed"

if [ -n "$forced" ]; then
  emit workspace "" "forced by $forced"
  exit 0
fi

# Reverse closure of the seeds over the dependency edges.
declare -A sel=()
queue=("${!seed[@]}")
for p in "${queue[@]}"; do sel[$p]=1; done
while [ ${#queue[@]} -gt 0 ]; do
  cur="${queue[0]}"; queue=("${queue[@]:1}")
  while IFS=$'\t' read -r from to; do
    [ "$to" = "$cur" ] || continue
    if [ -z "${sel[$from]:-}" ]; then sel[$from]=1; queue+=("$from"); fi
  done <<< "$edges"
done
for p in "${!direct[@]}"; do sel[$p]=1; done

if [ ${#sel[@]} -eq 0 ]; then
  emit none "" "no crate or crate test data changed"
  exit 0
fi
crates="$(printf '%s\n' "${!sel[@]}" | sort | tr '\n' ' ' | sed 's/ $//')"
emit affected "$crates" "changed: $(printf '%s\n' "${!seed[@]}" "${!direct[@]}" | grep -v '^$' | sort -u | tr '\n' ' ' | sed 's/ $//')"
