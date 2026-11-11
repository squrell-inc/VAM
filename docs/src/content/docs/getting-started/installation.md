---
title: Installation & Setup
description: Install a VAM node with one command, `vam node init`, and sign in. What the standard base image holds, and how to fix a failed check.
status: shipped
proves:
  - node-init-check-names-what-is-missing
  - node-init-run-twice-changes-nothing-and-shows-the-key-once
  - skill-needing-a-runtime-the-base-lacks-is-refused-at-install
  - skill-uses-python-pandas-and-git-on-the-standard-base
  - node-reports-the-standard-base-it-boots
  - plain-chat-gets-a-reply
  - vamd-refuses-plain-http-by-default
  - vm-restores-from-the-golden-snapshot
---

A VAM node is `vamd` running on a Linux host with KVM, plus the `vam` CLI to talk to it. **`vam node init`** takes a bare host to a running, signed-in-ready node in one command.

---

## Requirements

| Needed | Why |
|---|---|
| x86_64 Linux (Ubuntu 22.04+, Debian 12+, Amazon Linux 2023) | The guest kernel and Firecracker are x86_64 builds |
| KVM (`/dev/kvm`) | Each session runs in a Firecracker microVM. On a cloud VM, enable nested virtualization |
| cgroup v2 | The jailer places each microVM in a cgroup |
| root (`sudo`) | The jailer needs mounts and namespaces; `vamd` runs as root under systemd |
| Docker | Builds the base image at install time. Nothing needs Docker at run time |
| A Rust toolchain | Builds `vam` and `vamd` |

Developer laptops (macOS, Windows) need only the `vam` CLI. See [Remote developer workflow](#remote-developer-workflow-macos--windows) below.

---

## Install a node

```bash
git clone <repository-url> vam && cd vam
cargo build --release -p vam-cli -p vam-orchestrator   # target/release/vam and vamd

# 1. Read-only: names everything the host lacks, with the fix. Changes nothing.
sudo target/release/vam node init --check

# 2. Install.
sudo target/release/vam node init
```

`vam node init` does these steps, each reported as `done`, `unchanged` or an error:

1. Checks the host (the same checks as `--check`).
2. Creates the state directory `/var/lib/vam` (mode 0700).
3. Fetches Firecracker, the jailer and the guest kernel, each verified against a pinned sha256.
4. Builds the standard base image from the checkout (or takes `--base PATH|URL --base-sha256 SHA`).
5. Writes `/etc/vam/vamd.env` (mode 0600, no secrets) and a systemd unit, then starts `vamd`. The unit never turns the jailer off and never turns TLS off.
6. Runs `vamd --init`, which mints the first admin key.

Useful flags: `--no-systemd` (install the files but do not manage a unit), `--base PATH|URL` with `--base-sha256`, `--checkout DIR` (where to build the base from), `--root DIR` (put every host path under DIR, for tests).

### The admin key is shown once

The first run prints the admin key **once, on your terminal**, and tells you to save it. `vamd` stores only its hash and never logs the key; a node that starts without a key mints none and prints nothing. `vamd --init` on a node that already has an admin key prints none. If you lose the key, sign in some other way you set up (for example the headless admin below) and create another with `vam keys create`.

### Re-running is safe

Run `sudo vam node init` again at any time (after an upgrade, or to repair a file). Every step compares first and writes only what differs: a second run says `unchanged` throughout, changes no file and shows no key.

---

## Sign in

The command ends with the lines to use. From this machine or another:

```bash
vam config set-host https://<host>:8080
vam config set-ca /var/lib/vam/tls/ca.pem      # copy ca.pem first when you sign in from another machine
vam config set-key --from-env VAM_API_KEY      # the admin key printed by the first run
vam node status                                # base: vam/base:2 (..., verified), runtimes
```

Or open the app at `https://<host>:8080/` and sign in with the key. `vamd` serves TLS by default with a certificate it makes itself; `set-ca` makes your CLI trust it. To add a password-based admin afterwards, see [Users and roles](/getting-started/users-and-roles/).

---

## The standard base: `vam/base:2`

Every microVM boots a root filesystem built at install time from the standard base. It holds:

| Part | Contents |
|---|---|
| Python 3.13 | `requests`, `pydantic`, `pandas`, `numpy`, `python-dateutil`, `openpyxl` |
| Tools | `git`, `curl`, `jq` |
| JavaScript | Bun |
| Shell | busybox, plus the guest init (`vam-runtime`) |

**Nothing installs at run time.** The guest has no package index, and `pip install` is refused. A skill that needs more than this declares it (below) and the node refuses it at install if the base cannot supply it. To change what the base holds, rebuild it with `scripts/build-rootfs.sh` and run `vam node init` again.

The node reports its base at `GET /v1/node` and `vam node status`: the name, whether its manifest matches the image the node actually boots (`verified`), and the runtimes with versions. The agent is told the same facts in its prompt.

### `requiresRuntime`

A skill whose scripts need a runtime or version the base may lack sets `requiresRuntime` in its `manifest.json`, for example `{ "python": ">=3.12", "git": "*" }`. The node checks it when the skill package is installed and again at release, and refuses a skill the base cannot run, with a message naming the base and the runtime (for example, a skill needing `java >=21` is refused by `vam/base:2`). So a skill fails at install, not halfway through a run. See [Skills: `requiresRuntime`](/authoring/skill-manifest/#requiresruntime).

### The minimal fallback

For this release only, the old busybox image (`vam/base:1`, Bun only) is still available: `scripts/build-rootfs.sh --minimal` or `VAM_BASE=minimal`. It is deprecated and removed in the next release; skills that need Python or git are refused on it.

---

## Troubleshooting

`vam node init --check` prints one line per check with `ok`, `missing`, `wrong` or `skipped`, and the fix.

| Check | If it fails |
|---|---|
| `kvm` | No `/dev/kvm`: use a host with hardware virtualization, load `kvm_intel` or `kvm_amd`, or enable nested virtualization on a cloud VM. Not openable: run as root or join the `kvm` group |
| `cgroup-v2` | `/sys/fs/cgroup/cgroup.controllers` is absent: boot with cgroup v2 (`systemd.unified_cgroup_hierarchy=1`) |
| `firecracker` | Binary missing or not executable, or its sha256 differs from the pin: run `vam node init`, which fetches and verifies it |
| `jailer` | Same as `firecracker` |
| `kernel` | Guest kernel missing or wrong: run `vam node init` |
| `base-image` | No base, or it does not match its manifest: run `vam node init` (builds it, or pass `--base` with `--base-sha256`). Needs Docker |
| `vamd-preflight` | `vamd --preflight` judges the jailer rules (the jailer's user, chroot base and cgroup, and TLS) once the assets exist. Read its detail line; fix it as named and re-run |

Never work around a failed check with `--no-jailer` or `--insecure-dev` on a real node: they exist for local development only and remove the isolation this system is for.

---

## Configuration & authentication

The `vam` CLI resolves settings in this order (highest first):

1. **CLI flags**: `--host <URL>` and `--token <API_KEY>`
2. **Environment**: `VAM_HOST` and `VAM_API_KEY` (CI keeps using `VAM_API_KEY` as a Bearer override)
3. **Config file**: `~/.config/vam/config.toml` (mode 0600)

```bash
vam config show    # keys are redacted
```

### Headless first admin (no browser)

To create a loginable admin without SMTP or a UI:

```bash
export VAMD_BOOTSTRAP_ADMIN_PASSWORD='...'
vamd --bootstrap-admin admin
```

Then `vam login --username admin`. Who that user is and when to use publisher or member: [Users and roles](/getting-started/users-and-roles/).

---

## Remote Developer Workflow (macOS / Windows)

You do **not** need a Linux machine or KVM on your local workstation to develop agents and interact with VAM.

```
┌─────────────────────────────────────────┐          ┌─────────────────────────────────────────┐
│     Local Laptop (macOS / Windows)      │          │       Remote Linux Host / Cloud EC2     │
│                                         │          │                                         │
│  [vam CLI] ─── HTTP / REST ─────────────┼──────────┼──▶ [vamd Orchestrator]                  │
│    • vam run                            │  (TLS)   │      • Firecracker MicroVMs             │
│    • vam approve decide                 │          │      • Drive9 Storage Volumes           │
│    • vam secrets set                    │          │      • Host Zero-Trust Egress Proxy     │
└─────────────────────────────────────────┘          └─────────────────────────────────────────┘
```

1. **Host Setup**: Run `vamd` on a Linux host (e.g. EC2 `c6i.metal` or bare-metal KVM instance).
2. **Generate API Key**: On the host, create a key for the laptop with the admin key:
   ```bash
   vam keys create --name "developer-laptop" --scope node:read --scope runs:write
   ```
3. **Configure Laptop**: On macOS/Windows, point your local `vam` binary to the remote host:
   ```bash
   export VAM_HOST="https://vam.example.com:8080"
   export VAM_API_KEY="vam_sec_..."
   vam health
   ```

For a **full local stack** on Apple Silicon (nested KVM, aarch64 rootfs, `vamd`, and microVM runs), see [macOS + Lima](/getting-started/macos-lima/). For remote-only Mac development against EC2 or bare metal, continue below.

---

## Health & Diagnostics

Run the health check command to verify connectivity and daemon status:

```bash
vam health
```

It prints the JSON body of `GET /v1/health`.

---

## Optional performance tuning

For sub-second session create on repeated runs, enable Tier 1 golden snapshot restore:

```bash
# Pin guest artifacts (included in golden snapshot key)
./scripts/fetch-guest-kernels.sh
export VAM_KERNEL_PATH=/var/lib/vam/kernels/vmlinux-6.18.41

# Optional: per-step timing on stderr
export VAM_TIMING=1
mkdir -p ~/.cache/vam
cargo run -p vam-orchestrator --bin vamd 2>>~/.cache/vam/vamd.log
```

Wait for `Golden snapshot ready: {key}` before benchmarking. See [Cold-start & Golden Snapshot](/architecture/cold-start/) for host prep, caveats, and measured numbers.

---

## Next Steps

- Read [Users and roles](/getting-started/users-and-roles/) for the first admin and which login to use.
- Check the [CLI Command Reference](/getting-started/cli/) for all subcommands.
- Read [Quickstart (5 Min)](/getting-started/quickstart/) to run your first workflow.
