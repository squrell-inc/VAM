#!/usr/bin/env bash
# prepare.sh on throwaway workspaces (real cargo, no dependencies, no network).
set -uo pipefail
. "$(dirname "$0")/helpers.sh"
. "$release_dir/lib.sh"

prepare="$release_dir/prepare.sh"

# --- lib.sh: ordering ----------------------------------------------------------------------------
echo "lib.sh"
cmp_case() { expect_eq "semver_cmp $1 $2" "$3" "$(semver_cmp "$1" "$2")"; }
cmp_case 0.1.0 0.1.0 0
cmp_case 0.1.0-rc.1 0.1.0 -1
cmp_case 0.1.0 0.1.0-rc.9 1
cmp_case 0.1.0-rc.2 0.1.0-rc.10 -1
cmp_case 0.2.0-rc.1 0.1.9 1
cmp_case 1.0.0 0.99.99 1
cmp_case 0.10.0 0.9.0 1
for v in 1.0 v1.0.0 1.0.0-beta.1 01.0.0 1.0.0+dev 1.0.0-rc.01 ""; do
  if semver_valid "$v"; then bad "semver_valid accepts '$v'"; else ok "semver_valid refuses '$v'"; fi
done

# --- fixture workspace ---------------------------------------------------------------------------
make_repo() { # make_repo <dir>
  local d="$1"
  mkdir -p "$d/crates/a/src" "$d/crates/b/src"
  cat > "$d/Cargo.toml" <<'T'
[workspace]
resolver = "2"
members = ["crates/a", "crates/b"]

[workspace.package]
version = "0.1.0"
edition = "2021"
T
  printf '[package]\nname = "fx-a"\nversion.workspace = true\nedition.workspace = true\n' > "$d/crates/a/Cargo.toml"
  printf '[package]\nname = "fx-b"\nversion.workspace = true\nedition.workspace = true\n\n[dependencies]\nfx-a = { path = "../a" }\n' > "$d/crates/b/Cargo.toml"
  : > "$d/crates/a/src/lib.rs"; : > "$d/crates/b/src/lib.rs"
  printf '/target/\n' > "$d/.gitignore"
  ( cd "$d" && git_quiet init -q && cargo generate-lockfile --offline -q && git_quiet add -A && git_quiet commit -q -m init )
}
fresh() { rm -rf "$t_tmp/r"; make_repo "$t_tmp/r"; }
in_repo() { ( cd "$t_tmp/r" && "$@" ); }
prep() { in_repo "$prepare" --no-fetch "$@"; }

echo "prepare.sh"
fresh
for v in 1.0 v1.0.0 1.0.0-beta.1 01.0.0 1.0.0+dev ""; do
  expect_fail "refuses malformed '$v'" "not a release version|usage" prep $v
done
expect_fail "refuses no version" "usage" prep
expect_eq "nothing changed after refusals" "" "$(in_repo git status --porcelain)"

# The first prepare sets 0.1.0-rc.1 below the never-released 0.1.0 in Cargo.toml: accepted.
expect_ok "no tags: 0.1.0-rc.1 accepted (below the current Cargo.toml version)" prep 0.1.0-rc.1
expect_eq "only Cargo.toml and Cargo.lock changed" "Cargo.lock Cargo.toml" "$(in_repo git diff --name-only | tr '\n' ' ' | sed 's/ $//')"
expect_eq "Cargo.toml: one line changed" "2" "$(in_repo git diff -U0 Cargo.toml | grep -c '^[-+][^-+]')"
expect_match "Cargo.toml has the new version" '^version = "0.1.0-rc.1"$' "$(in_repo grep '^version' Cargo.toml)"
lock_diff="$(in_repo git diff -U0 Cargo.lock | grep '^[-+][^-+]')"
expect_eq "Cargo.lock: only the two workspace versions changed" "4" "$(printf '%s\n' "$lock_diff" | wc -l | tr -d ' ')"
expect_nomatch "Cargo.lock diff touches nothing but version lines" '^[-+](name|source|checksum|dependencies)' "$lock_diff"
expect_match "prints the PR title" 'release: prepare v0.1.0-rc.1' "$T_OUT"
expect_match "prints the gh pr create line" 'gh pr create --base main --title "release: prepare v0.1.0-rc.1"' "$T_OUT"

fresh
in_repo git_quiet tag v0.1.0-rc.1
expect_fail "equal to a tag is refused" "not greater than the existing tag v0.1.0-rc.1" prep 0.1.0-rc.1
expect_fail "lower than a tag is refused" "not greater than the existing tag v0.1.0-rc.1" prep 0.0.9
expect_ok "a final after its rc is accepted" prep 0.1.0

fresh
in_repo git_quiet tag v0.1.0
expect_fail "an rc after its final is refused" "not greater than the existing tag v0.1.0" prep 0.1.0-rc.2
expect_fail "lower than the highest of several tags" "not greater than the existing tag v0.1.0" prep 0.0.5
in_repo git_quiet tag v0.3.0
expect_fail "greater than one tag but not the highest" "not greater than the existing tag v0.3.0" prep 0.2.0
expect_ok "greater than every tag" prep 0.3.1

fresh
in_repo git_quiet tag vnext
expect_fail "a non-semver v* tag is refused, naming it" "tag 'vnext' is not a release version" prep 0.2.0

fresh
echo "// edit" >> "$t_tmp/r/crates/a/src/lib.rs"
expect_fail "a dirty tree (tracked change) is refused" "not clean" prep 0.2.0
fresh
: > "$t_tmp/r/untracked.txt"
expect_fail "a dirty tree (untracked file) is refused" "not clean" prep 0.2.0

# A member that does not inherit the version is refused and everything is restored.
fresh
sed -i 's/^version.workspace = true/version = "0.1.0"/' "$t_tmp/r/crates/b/Cargo.toml"
( cd "$t_tmp/r" && cargo generate-lockfile --offline -q && git_quiet commit -qam "b pins its version" )
expect_fail "a member with its own version is refused" "not at 0.2.0.*fx-b" prep 0.2.0
expect_eq "refusal restores Cargo.toml and Cargo.lock" "" "$(in_repo git status --porcelain)"

# A Cargo.lock change beyond the workspace crates is refused: a cargo shim that also adds a registry crate.
fresh
mkdir -p "$t_tmp/shim"
real_cargo="$(command -v cargo)"
cat > "$t_tmp/shim/cargo" <<SHIM
#!/usr/bin/env bash
if [ "\${1:-}" = update ]; then
  "$real_cargo" "\$@" || exit \$?
  printf '\n[[package]]\nname = "intruder"\nversion = "9.9.9"\nsource = "registry+https://github.com/rust-lang/crates.io-index"\n' >> Cargo.lock
  exit 0
fi
exec "$real_cargo" "\$@"
SHIM
chmod +x "$t_tmp/shim/cargo"
run_cmd env PATH="$t_tmp/shim:$PATH" bash -c "cd '$t_tmp/r' && '$prepare' --no-fetch 0.2.0"
if [ "$T_RC" != 0 ] && printf '%s' "$T_OUT" | grep -q 'gained registry entry intruder'; then ok "a lock change beyond the workspace crates is refused"; else bad "lock check (exit $T_RC)" "$T_OUT"; fi
expect_eq "…and Cargo.lock is restored" "" "$(in_repo git status --porcelain)"

# Fetching: an unreachable origin is a refusal, not a guess.
fresh
in_repo git remote add origin "$t_tmp/does-not-exist.git"
expect_fail "an unreadable remote is refused (no --no-fetch)" "cannot fetch tags" in_repo "$prepare" 0.2.0
expect_ok "…and --no-fetch compares with the local tags" prep 0.2.0

finish
