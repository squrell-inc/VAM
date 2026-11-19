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

# release_assets <version>: the file names a release of this version holds besides SHA256SUMS and
# RELEASE_NOTES.md, one per line: the one list SHA256SUMS, the notes table, `publish` and `verify`
# are built from, so a missing or stray file fails loudly. install.sh is a source file that
# `assemble` copies from the checked-out commit; the build jobs upload `built_assets`.
release_assets() {
  local v="$1"
  printf '%s\n' \
    "vam-node-$v-x86_64-unknown-linux-gnu.tar.gz" \
    "vam-$v-x86_64-unknown-linux-gnu.tar.gz" \
    "vam-$v-aarch64-unknown-linux-gnu.tar.gz" \
    "vam-$v-aarch64-apple-darwin.tar.gz" \
    "vam-$v-x86_64-apple-darwin.tar.gz" \
    "vam-runtime-$v-x86_64-unknown-linux-musl" \
    "install.sh"
}

# built_assets <version>: what the build jobs upload (asset-* artifacts): release_assets without
# install.sh. `assemble` checks its --assets directory against exactly this list.
built_assets() { release_assets "$1" | grep -vx 'install.sh'; }

# sha256_of <file>: the hex digest (sha256sum, or shasum on macOS).
sha256_of() { if command -v sha256sum >/dev/null; then sha256sum "$1" | awk '{print $1}'; else shasum -a 256 "$1" | awk '{print $1}'; fi; }

# file_size <file>: bytes.
file_size() { wc -c < "$1" | tr -d ' '; }

# release_files <version>: every file a release holds: the assets, SHA256SUMS and RELEASE_NOTES.md.
release_files() { release_assets "$1"; printf '%s\n' SHA256SUMS RELEASE_NOTES.md; }

# --- GitHub release helpers (publish.sh, verify.sh); they use `gh` on PATH and $REPO ----------------

# retry <tries> <seconds> <command...>: runs the command until it succeeds, at most <tries> times.
# RETRY_SLEEP overrides the wait (tests set 0). The last failure's output stays visible.
retry() {
  local tries="$1" wait="${RETRY_SLEEP:-$2}" n=1; shift 2
  until "$@"; do
    [ "$n" -lt "$tries" ] || return 1
    echo "retry: attempt $n of $tries failed; waiting ${wait}s" >&2
    sleep "$wait"; n=$((n + 1))
  done
}

# gh_releases: every release of $REPO, drafts included (the token needs contents: write), as one JSON array.
gh_releases() {
  local out
  out="$(gh api -H 'Accept: application/vnd.github+json' --paginate "repos/$REPO/releases?per_page=100")" || return 1
  printf '%s' "$out" | jq -cs 'add // []'
}

# gh_tag_refs <tag>: the refs named exactly refs/tags/<tag>, as a JSON array (matching-refs is a prefix match).
gh_tag_refs() {
  local out
  out="$(gh api -H 'Accept: application/vnd.github+json' --paginate "repos/$REPO/git/matching-refs/tags/$1")" || return 1
  printf '%s' "$out" | jq -cs --arg r "refs/tags/$1" 'add // [] | map(select(.ref == $r))'
}

# gh_release_by_id <id>: the release JSON; exit 3 when it does not exist (404), 1 on any other failure.
gh_release_by_id() {
  local out err rc
  err="$(mktemp "${TMPDIR:-/tmp}/gh-err.XXXXXX")"
  out="$(gh api -H 'Accept: application/vnd.github+json' "repos/$REPO/releases/$1" 2>"$err")"; rc=$?
  if [ "$rc" != 0 ]; then
    if grep -q 'HTTP 404' "$err"; then rm -f "$err"; return 3; fi
    cat "$err" >&2; rm -f "$err"; return 1
  fi
  rm -f "$err"; printf '%s' "$out"
}
