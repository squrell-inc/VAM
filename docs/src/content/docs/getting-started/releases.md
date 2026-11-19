---
title: Releases
description: Install the vam CLI with one command, which release archive fits which machine, how to check a download, how to upgrade, and how to install a node from the bundle with no checkout.
status: shipped
proves:
  - cli-prints-its-version-and-commit
  - node-init-from-a-release-bundle-needs-no-checkout
  - node-init-from-a-newer-bundle-keeps-the-state
  - dock-puts-vam-on-the-path-once
  - upgrade-replaces-vam-with-the-newer-release
  - upgrade-refuses-a-download-whose-hash-does-not-match
---

A release gives you ready-built binaries. You do not need a checkout or a Rust toolchain. Every release lives at `https://github.com/squrell-inc/VAM/releases`.

Release candidates (`v0.1.0-rc.1` and so on) are pre-releases, so GitHub shows no "latest" release yet. Use the tag's own URL: `https://github.com/squrell-inc/VAM/releases/tag/<tag>`. Each file is at `https://github.com/squrell-inc/VAM/releases/download/<tag>/<asset>`.

---

## Install the CLI with one command

`install.sh` installs the `vam` CLI on Linux or macOS. It needs `curl` (or `wget`), `tar` and one of `sha256sum`, `shasum` or `openssl`. It never uses `sudo`.

Until `v0.1.0` there is no "latest" release, so use the tag's own URL:

```bash
curl -fsSL https://github.com/squrell-inc/VAM/releases/download/<tag>/install.sh | sh
```

To read the script before you run it:

```bash
curl -fsSLO https://github.com/squrell-inc/VAM/releases/download/<tag>/install.sh
less install.sh
sh install.sh
```

| Flag | Meaning |
|---|---|
| `--version vX` | The release to install, for example `v0.1.0-rc.2`. A leading `v` is optional. Without it the script takes the latest final release. Before the first final release it stops with `no final release yet; pass --version v0.1.0-rc.N`. |
| `--dir DIR` | Where `vam` goes. Passed to `vam dock`. The default is `~/.local/bin`. |
| `--no-modify-path` | Print the `PATH` line instead of editing your profile files. Passed to `vam dock`. |
| `--from DIR` | Use the archive and `SHA256SUMS` in `DIR`. No network, and no attestation check. |

The script prints each step before it runs it:

1. It picks the CLI archive for your OS and CPU.
2. It downloads the archive and `SHA256SUMS`, and checks the archive's hash. A mismatch, or a missing line, stops here and nothing is installed.
3. If `gh` is on your `PATH`, it runs `gh attestation verify`. A refusal stops here. Without `gh`, it says the attestation was not checked and prints the command to run.
4. It extracts `vam`, checks that it starts and reports the right version, and runs `vam dock`.

`vam dock` copies `vam` into the directory and adds that directory to your `PATH`. See [Put vam on your PATH](#put-vam-on-your-path).

The script refuses Windows, musl (Alpine) and 32-bit systems. A Windows `vam.exe` and an aarch64 node bundle come in v0.2.0. On Windows, use [WSL2](/getting-started/windows-wsl/).

`install.sh` installs the CLI only. For a node, see [Install a node from the bundle](#install-a-node-from-the-bundle).

---

## Which archive for which machine

| You want | Asset | Machine |
|---|---|---|
| A node (`vamd`, `vam`, the three brokers, the guest init, the base recipe) | `vam-node-<V>-x86_64-unknown-linux-gnu.tar.gz` | x86_64 Linux with KVM, glibc 2.34 or newer |
| The `vam` CLI alone | `vam-<V>-<target>.tar.gz` | Linux x86_64 or aarch64, macOS arm64 or x86_64 |
| The guest init alone | `vam-runtime-<V>-x86_64-unknown-linux-musl` | Rebuilding a base by hand |
| The installer | `install.sh` | Linux and macOS. See above |
| Checksums | `SHA256SUMS` | Every archive above is listed in it |

A release holds seven assets (the node bundle, four CLI archives, the guest init, and `install.sh`), plus `SHA256SUMS` and `RELEASE_NOTES.md`.

`<V>` is the version without the `v`, for example `0.1.0-rc.1`. The `<target>` of the CLI archives is `x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin` or `x86_64-apple-darwin`.

Not in this release: a Windows `vam.exe` and an aarch64 node bundle. On Windows, run the Linux bundle inside WSL2 ([Windows + WSL](/getting-started/windows-wsl/)).

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

### Check where a download came from

The release workflow attests every asset. If you have the [GitHub CLI](https://cli.github.com/), run:

```bash
gh attestation verify <file> -R squrell-inc/VAM \
  --signer-workflow squrell-inc/VAM/.github/workflows/release.yml
```

It must print that the file verified. Attestations are published from `v0.1.0-rc.1` on; `rc.1` is the first release that has them. The hash check stays the check that always runs.

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

Extract the newer bundle and run `sudo ./vam node init` from it (add the same `--broker` list you used before). `vam upgrade` does not upgrade a node. The base is rebuilt because the bundle changed. The state database and the admin key stay as they are, so `vam node init` prints no new key.

---

## Use the CLI alone

To talk to a node on another machine, run `install.sh` (see above), then:

```bash
vam config set-host https://<node>:8080
```

By hand instead: unpack the `vam` archive, check it, and run `vam dock` from the unpacked file.

```bash
tar xzf vam-<V>-<target>.tar.gz
./vam-<V>-<target>/vam dock
```

Or put it where you like: `sudo install vam-<V>-<target>/vam /usr/local/bin/vam`.

See [Remote developer workflow](/getting-started/installation/#remote-developer-workflow-macos--windows).

---

## Put vam on your PATH

`vam dock` copies the `vam` you run into `~/.local/bin` and adds that directory to your `PATH`.

```bash
./vam dock                       # default: ~/.local/bin
./vam dock --dir ~/bin           # another directory
./vam dock --no-modify-path      # copy only; print the PATH line
vam undock                       # undo it
```

It adds one line, ending in `# added by vam dock`, to your login profile files: `~/.profile` (made if missing), `~/.bashrc` and `~/.zshrc` if they exist on Linux; `~/.zprofile` (made if missing) and `~/.bash_profile` if it exists on macOS. It skips a file that has the line already, and adds nothing when the directory is on your `PATH`. Run it twice and the second run changes nothing. Open a new terminal to pick up the change.

`vam undock` removes the copy of `vam` and the marked lines. It touches nothing else. It takes `--dir` too, when you docked into another directory.

On Windows, both commands exit with an error until v0.2.0.

---

## Upgrade the CLI

```bash
vam upgrade --check              # report only; change nothing
vam upgrade                      # replace this vam with the latest final release
vam upgrade --pre                # also consider release candidates
vam upgrade --version v0.1.0-rc.2   # a named release, newer or older
```

`vam upgrade` runs every check before it touches the file you are running: the version is a release, the release has an archive for your build, `SHA256SUMS` lists it and the download matches, `gh` (when present) verifies the attestation, and the new binary starts and reports the release's version. Only then does it replace `vam`. A hash mismatch exits with code 9 and leaves the old `vam` in place.

* `--check` changes nothing. It prints whether an upgrade, or a downgrade, is available and the command to run.
* It never downgrades by itself. If your `vam` is newer than the latest release it says so and stops; pass `--version` to go back.
* It refuses inside a node bundle (when `vamd` sits beside `vam`): upgrade the node from its bundle. `--check` still reports there. It never touches `vamd`.
* Before the first final release, plain `vam upgrade` says `no final release yet`. Use `--pre` or `--version`.
* It only runs when you run it. No other command goes to the network on its own. Set `GH_TOKEN` to avoid GitHub's rate limit.

---

## macOS: a browser download is quarantined

macOS marks files you download in a browser, and the binaries are not signed yet. Opening `vam` then fails with a warning. Clear the mark once:

```bash
xattr -d com.apple.quarantine vam
```

A file fetched with `curl`, and so `install.sh`, carries no mark. Code signing comes later.

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
