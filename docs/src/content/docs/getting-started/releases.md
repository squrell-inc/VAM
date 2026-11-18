---
title: Releases
description: Which release archive fits which machine, how to check a download, and how to install a node from the bundle with no checkout.
status: shipped
proves:
  - cli-prints-its-version-and-commit
  - node-init-from-a-release-bundle-needs-no-checkout
  - node-init-from-a-newer-bundle-keeps-the-state
---

A release gives you ready-built binaries. You do not need a checkout or a Rust toolchain. Every release lives at `https://github.com/squrell-inc/VAM/releases`.

Release candidates (`v0.1.0-rc.1` and so on) are pre-releases, so GitHub shows no "latest" release yet. Use the tag's own URL: `https://github.com/squrell-inc/VAM/releases/tag/<tag>`. Each file is at `https://github.com/squrell-inc/VAM/releases/download/<tag>/<asset>`.

---

## Which archive for which machine

| You want | Asset | Machine |
|---|---|---|
| A node (`vamd`, `vam`, the three brokers, the guest init, the base recipe) | `vam-node-<V>-x86_64-unknown-linux-gnu.tar.gz` | x86_64 Linux with KVM, glibc 2.34 or newer |
| The `vam` CLI alone | `vam-<V>-<target>.tar.gz` | Linux x86_64 or aarch64, macOS arm64 or x86_64 |
| The guest init alone | `vam-runtime-<V>-x86_64-unknown-linux-musl` | Rebuilding a base by hand |
| Checksums | `SHA256SUMS` | Every asset above is listed in it |

`<V>` is the version without the `v`, for example `0.1.0-rc.1`. The `<target>` of the CLI archives is `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin` or `x86_64-apple-darwin`.

Not in this release: a Windows `vam.exe`, an aarch64 node bundle, and signature or provenance checks. On Windows, run the Linux bundle inside WSL2 ([Windows + WSL](/getting-started/windows-wsl/)).

---

## Check a download

Put `SHA256SUMS` beside the files you downloaded.

```bash
# Linux
sha256sum -c --ignore-missing SHA256SUMS

# macOS
shasum -a 256 -c --ignore-missing SHA256SUMS
```

Each file you have must print `OK`. Delete a file that prints `FAILED` and download it again.

---

## Check what you run

```bash
vam --version     # vam 0.1.0-rc.1 (93eb949), commit date on release builds
vamd --version    # one line; starts nothing
```

A release build prints its exact version. Any other build prints the nearest tag plus `+dev`, for example `0.1.0+dev.14 (93eb949)`, and never claims a version that is not released. `+dirty` after the commit means the tree had uncommitted changes. `vamd --version` does not read your config and works beside a broken `.env`.

---

## Install a node from the bundle

```bash
tar xzf vam-node-<V>-x86_64-unknown-linux-gnu.tar.gz
cd vam-node-<V>-x86_64-unknown-linux-gnu

# Read-only: names what the host lacks, with the fix.
sudo ./vam node init --check

# Install.
sudo ./vam node init
```

The bundle holds `vamd`, `vam`, `vam-web-broker`, `vam-schedule-broker`, `vam-notify-broker`, `vam-runtime`, the licences, `NOTICE`, and `share/vam/base/` (the recipe for the standard base image). `vam node init` finds `vamd` and the base recipe beside `vam`. It builds the base from the bundle's recipe and puts the bundle's `vam-runtime` in as the guest init. It never looks for a checkout.

The host still needs what [Installation & Setup](/getting-started/installation/#requirements) lists: KVM, cgroup v2, root and Docker.

A base you name yourself wins. The order is `--base`, then `--kit DIR`, then `--checkout DIR`, then the bundle beside `vam`, then a checkout found from the current directory.

`vam node init` is safe to run twice. A second run reports `unchanged` and prints no new admin key.

### Upgrade a node

Extract the newer bundle and run `sudo ./vam node init` from it. The base is rebuilt because the bundle changed. The state database and the admin key stay as they are, so `vam node init` prints no new key.

---

## Use the CLI alone

To talk to a node on another machine, unpack the `vam` archive and put `vam` on your `PATH`.

```bash
tar xzf vam-<V>-<target>.tar.gz
sudo install vam-<V>-<target>/vam /usr/local/bin/vam
vam config set-host https://<node>:8080
```

See [Remote developer workflow](/getting-started/installation/#remote-developer-workflow-macos--windows).

---

## Does the node match the CLI

```bash
vam node status
```

prints a `node:` line and a `cli:` line:

```text
node: vamd 0.1.0-rc.1 (93eb949) release
cli: vam 0.1.0-rc.1 (93eb949)
```

When the major or minor number differs, a warning follows: `warning: the node runs 0.2, this vam is 0.1: minor versions differ`. Before 1.0 a minor release may break the API, so install the matching `vam`. Patch, rc and `+dev` differences print no warning. An older node prints `node: version unknown (older vamd)`.
