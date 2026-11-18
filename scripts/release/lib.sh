#!/usr/bin/env bash
# Shared helpers for the release scripts (prepare.sh, guards.sh). Source it; it sets no options.
#
# Versions are `X.Y.Z` or `X.Y.Z-rc.N` (locked decision 1): no leading zeros, no build metadata.
# An rc sorts below its own final: 0.1.0-rc.1 < 0.1.0.

SEMVER_RE='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-rc\.(0|[1-9][0-9]*))?$'

# semver_valid <v>: exit 0 when <v> is a release version.
semver_valid() { [[ "${1:-}" =~ $SEMVER_RE ]]; }

# semver_cmp <a> <b>: prints -1, 0 or 1 (a<b, a=b, a>b). Both must be valid.
semver_cmp() {
  local a="$1" b="$2" i x y
  semver_valid "$a" && semver_valid "$b" || { echo "semver_cmp: invalid version '$a' or '$b'" >&2; return 2; }
  local a_core="${a%%-*}" b_core="${b%%-*}"
  local a_rc=-1 b_rc=-1
  [[ "$a" == *-rc.* ]] && a_rc="${a##*-rc.}"
  [[ "$b" == *-rc.* ]] && b_rc="${b##*-rc.}"
  local -a pa pb
  IFS=. read -r -a pa <<< "$a_core"
  IFS=. read -r -a pb <<< "$b_core"
  for i in 0 1 2; do
    x="${pa[$i]}"; y="${pb[$i]}"
    if (( 10#$x < 10#$y )); then echo -1; return 0; fi
    if (( 10#$x > 10#$y )); then echo 1; return 0; fi
  done
  # Same X.Y.Z: a final (no rc, marked -1 here) is above any rc.
  if [ "$a_rc" = -1 ] && [ "$b_rc" = -1 ]; then echo 0; return 0; fi
  if [ "$a_rc" = -1 ]; then echo 1; return 0; fi
  if [ "$b_rc" = -1 ]; then echo -1; return 0; fi
  if (( 10#$a_rc < 10#$b_rc )); then echo -1; elif (( 10#$a_rc > 10#$b_rc )); then echo 1; else echo 0; fi
}

# semver_gt <a> <b>: exit 0 when a > b.
semver_gt() { [ "$(semver_cmp "$1" "$2")" = 1 ]; }

# version_tags: every `v*` tag name (without the leading v), one per line, from the local
# repository and, when REMOTE is set (default origin), from that remote too. A remote that cannot
# be read is an error (exit 1) so a caller never compares against a partial list.
version_tags() {
  local remote="${REMOTE-origin}" local_tags remote_tags="" t
  local_tags="$(git tag -l 'v*')" || return 1
  if [ -n "$remote" ] && git remote get-url "$remote" >/dev/null 2>&1; then
    remote_tags="$(git ls-remote --tags "$remote" 'refs/tags/v*')" || return 1
    remote_tags="$(printf '%s\n' "$remote_tags" | awk 'NF {print $2}' | sed -e 's#^refs/tags/##' -e 's#\^{}$##')"
  fi
  printf '%s\n%s\n' "$local_tags" "$remote_tags" | awk 'NF' | sort -u | while IFS= read -r t; do printf '%s\n' "${t#v}"; done
}

# workspace_version <Cargo.toml text on stdin>: the `version` under [workspace.package].
workspace_version() {
  awk '
    /^[[:space:]]*\[/ { s = $0; gsub(/[[:space:]]/, "", s); insec = (s == "[workspace.package]"); next }
    insec && /^[[:space:]]*version[[:space:]]*=/ {
      v = $0; sub(/^[^=]*=[[:space:]]*"/, "", v); sub(/".*$/, "", v); print v; exit
    }'
}
