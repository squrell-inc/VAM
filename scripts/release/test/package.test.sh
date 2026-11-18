#!/usr/bin/env bash
# package.sh on dummy binaries: archive names and layout, the kit manifest, reproducibility, and
# what it refuses. No build, no network.
set -uo pipefail
. "$(dirname "$0")/helpers.sh"

pkg="$release_dir/package.sh"
v=0.1.0-rc.1
tgt=x86_64-unknown-linux-gnu
export SOURCE_DATE_EPOCH=1700000000
bins="$t_tmp/bins"; mkdir -p "$bins"
for b in vamd vam vam-web-broker vam-schedule-broker vam-notify-broker; do printf '#!/bin/sh\necho %s\n' "$b" > "$bins/$b"; chmod +x "$bins/$b"; done
printf 'dummy runtime\n' > "$t_tmp/vam-runtime"

echo "node bundle"
out="$t_tmp/out"
expect_ok "builds the node bundle" "$pkg" --kind node --target "$tgt" --version "$v" --bins "$bins" --runtime "$t_tmp/vam-runtime" --out "$out"
asset="$out/vam-node-$v-$tgt.tar.gz"
[ -f "$asset" ] && ok "named vam-node-<v>-<target>.tar.gz" || bad "no $asset"
listing="$(tar -tzf "$asset" | sed "s#^vam-node-$v-$tgt/##" | sort)"
for want in vamd vam vam-web-broker vam-schedule-broker vam-notify-broker vam-runtime scripts/assets.pins LICENSE-APACHE LICENSE-MIT NOTICE README.md \
  share/vam/base/build-rootfs.sh share/vam/base/Dockerfile share/vam/base/pins share/vam/base/resolve-pins.sh share/vam/base/manifest.json; do
  expect_match "holds $want" "^$(printf '%s' "$want" | sed 's/\./\\./g')\$" "$listing"
done
expect_nomatch "has one top directory" "^(\.\.|/)" "$(tar -tzf "$asset")"
expect_eq "every path is under vam-node-<v>-<target>/" "0" "$(tar -tzf "$asset" | grep -vc "^vam-node-$v-$tgt/")"
x="$t_tmp/x"; mkdir -p "$x"; tar -xzf "$asset" -C "$x"
d="$x/vam-node-$v-$tgt"
kit="$d/share/vam/base"
expect_eq "the manifest names the base" "vam/base:2" "$(jq -r .name "$kit/manifest.json")"
expect_eq "the manifest names the version" "$v" "$(jq -r .version "$kit/manifest.json")"
expect_eq "the manifest lists exactly the kit files" "Dockerfile build-rootfs.sh pins resolve-pins.sh" "$(jq -r '.files | keys | join(" ")' "$kit/manifest.json")"
expect_ok "every manifest sha256 matches its file" bash -c "cd '$kit' && jq -r '.files | to_entries[] | \"\\(.value)  \\(.key)\"' manifest.json | sha256sum -c -"
[ -x "$d/vamd" ] && [ -x "$kit/build-rootfs.sh" ] && ok "binaries and build-rootfs.sh are executable" || bad "mode lost"
cmp -s "$d/vam-runtime" "$t_tmp/vam-runtime" && ok "vam-runtime is the file given" || bad "vam-runtime differs"
echo tampered >> "$kit/Dockerfile"
expect_fail "a changed kit file no longer matches the manifest" "FAILED|did NOT match" bash -c "cd '$kit' && jq -r '.files | to_entries[] | \"\\(.value)  \\(.key)\"' manifest.json | sha256sum -c -"
# The kit finds its recipe beside itself (no base/ subdirectory): the script gets past that and
# stops at the runtime check, before Docker is needed.
tar -xzf "$asset" -C "$x" --overwrite
expect_fail "build-rootfs.sh in the kit finds the recipe beside itself" "not an ELF file" bash "$kit/build-rootfs.sh" --runtime "$d/vam-runtime"

echo "reproducible"
touch -d '2020-01-01' "$bins"/* "$t_tmp/vam-runtime"
"$pkg" --kind node --target "$tgt" --version "$v" --bins "$bins" --runtime "$t_tmp/vam-runtime" --out "$t_tmp/out2" >/dev/null 2>&1
expect_eq "the same inputs give the same archive" "$(sha256sum "$asset" | awk '{print $1}')" "$(sha256sum "$t_tmp/out2/vam-node-$v-$tgt.tar.gz" | awk '{print $1}')"

echo "cli archive and the guest init"
expect_ok "builds the cli archive" "$pkg" --kind cli --target aarch64-apple-darwin --version "$v" --bins "$bins" --out "$out"
cli_list="$(tar -tzf "$out/vam-$v-aarch64-apple-darwin.tar.gz" | sort | tr '\n' ' ')"
expect_eq "the cli archive holds vam, the licences and NOTICE" "vam-$v-aarch64-apple-darwin/ vam-$v-aarch64-apple-darwin/LICENSE-APACHE vam-$v-aarch64-apple-darwin/LICENSE-MIT vam-$v-aarch64-apple-darwin/NOTICE vam-$v-aarch64-apple-darwin/vam " "$cli_list"
expect_ok "copies the guest init" "$pkg" --kind runtime --target x86_64-unknown-linux-musl --version "$v" --runtime "$t_tmp/vam-runtime" --out "$out"
[ -x "$out/vam-runtime-$v-x86_64-unknown-linux-musl" ] && ok "named vam-runtime-<v>-<target>, executable" || bad "no guest asset"

echo "refusals"
rm "$bins/vam-notify-broker"
expect_fail "a missing broker" "missing or empty: .*vam-notify-broker" "$pkg" --kind node --target "$tgt" --version "$v" --bins "$bins" --runtime "$t_tmp/vam-runtime" --out "$t_tmp/o3"
expect_fail "an invalid version" "not a release version" "$pkg" --kind cli --target "$tgt" --version 0.1 --bins "$bins" --out "$t_tmp/o3"
expect_fail "an unknown kind" "--kind must be" "$pkg" --kind zip --target "$tgt" --version "$v" --out "$t_tmp/o3"
: > "$t_tmp/empty"
expect_fail "an empty runtime" "missing or empty" "$pkg" --kind runtime --target x86_64-unknown-linux-musl --version "$v" --runtime "$t_tmp/empty" --out "$t_tmp/o3"
finish
