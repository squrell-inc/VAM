---
title: macOS + Lima (Local Linux Node)
description: Run the full VAM stack on Apple Silicon — nested-KVM Lima VM, aarch64 rootfs, guest kernel, vamd, and Mac CLI on port 8080.
---

Apple Silicon Macs cannot run Firecracker or KVM natively on macOS. This page is the **primary local development path** on Mac: a **Lima** aarch64 Linux VM with nested KVM runs `vamd`, Firecracker, and microVM sessions; the Mac `vam` CLI talks to it over HTTP on port **8080**.

For a remote-only workflow (no Lima), see [Installation & Setup](/getting-started/installation/).

---

## Requirements

| Requirement | Notes |
|---|---|
| **Hardware** | Apple Silicon **M3 or newer** (nested virtualization via Apple Virtualization.framework) |
| **macOS** | **15+** (Sequoia or later recommended) |
| **Homebrew** | For installing Lima |
| **RAM** | Lima reserves **6 GiB** while the `vam` instance is running; memory is released after `limactl stop vam` |
| **Disk** | ~40 GiB for the Lima VM disk image; ~256 MiB for the guest rootfs at `/tmp/lima/rootfs.ext4` |
| **Time (first run)** | Rootfs build inside Lima typically takes **10–20 minutes** |

---

## Architecture

```
┌─────────────────────────────┐       ┌──────────────────────────────┐
│  macOS (your laptop)        │       │  Lima VM (aarch64 Linux)     │
│                             │       │                              │
│  • vam CLI (HTTP client)    │──────▶│  • vamd orchestrator         │
│  • Repo checkout (ro mount) │ :8080 │  • Firecracker 1.16.1        │
│                             │       │  • /dev/kvm (nested)         │
│                             │       │  • rootfs.ext4 + vmlinux     │
└─────────────────────────────┘       └──────────────────────────────┘
                                              │
                                              ▼
                                    ┌──────────────────────────────┐
                                    │  microVM (Firecracker guest) │
                                    │  • vam-runtime as /sbin/init │
                                    │  • Bun at /usr/local/bin/bun │
                                    └──────────────────────────────┘
```

The repo is mounted **read-only** under your home directory. Writable artifacts go to `/tmp/lima/` (forwarded to the Mac at `/tmp/lima/`).

---

## 1. Install and start Lima

```bash
brew install lima
limactl --version

# From the repository root
limactl start --name=vam ./scripts/lima-macos.yaml
```

First boot downloads the Ubuntu 24.04 ARM cloud image and provisions a 40 GiB disk. Port **8080** on the guest is forwarded to **8080** on the host.

If the instance is **Broken**, repair or recreate:

```bash
limactl stop vam
limactl delete vam
limactl start --name=vam ./scripts/lima-macos.yaml
```

---

## 2. Doctor checks

```bash
limactl list
limactl shell vam -- uname -m          # expect aarch64
limactl shell vam -- ls -l /dev/kvm    # expect crw-rw---- root kvm
```

---

## 3. Build guest stack (run in order)

From the repository root on macOS:

```bash
# 1. Guest kernel (Firecracker CI aarch64 vmlinux → /var/lib/vam/kernels/vmlinux-6.18)
./scripts/fetch-guest-kernel-lima.sh

# 2. Guest rootfs (aarch64 musl + Bun; output /tmp/lima/rootfs.ext4) — 10–20 min first time
./scripts/build-rootfs-lima.sh

# 3. Start vamd inside Lima (builds aarch64 vamd if needed, installs Firecracker 1.16.1)
./scripts/start-vamd-lima.sh
```

`start-vamd-lima.sh` exports `VAM_ROOTFS_PATH` (the Lima-built rootfs), `VAMD_FIRECRACKER_BIN` and `VAMD_JAILER_BIN`; a VM node has no built-in paths. It fails with a clear message if the rootfs has not been built yet.

Optional first org admin: export `VAMD_BOOTSTRAP_ADMIN` and `VAMD_BOOTSTRAP_ADMIN_PASSWORD` **before** that script. The script forwards both into Lima. See step 4 and [Users and roles](/getting-started/users-and-roles/).

The start script sets `VAMD_TELEMETRY_CAPTURE` to `redacted-full` by default so run events include truncated stdout/stderr previews (needed for journey log views in the VAM app). Override with `VAMD_TELEMETRY_CAPTURE=digest` (hex only) or `none` before running the script. Metrics scrape (`VAMD_TELEMETRY_LISTEN`, default `127.0.0.1:9090` inside Lima) stays inside the VM; the Mac CLI and app talk to vamd on port 8080 only.

`vamd` also exports OTLP/HTTP to loopback `:4318` inside Lima (`VAMD_OTLP_ENABLED`, default on). Run `otelcol --config scripts/otel-collector-config.yaml` **inside the Lima VM** if you want the debug exporter to show spans. Do not forward 4317 or 4318 to the Mac. If `otelcol` / `otelcol-contrib` is already on the Lima `PATH`, `start-vamd-lima.sh` starts it; otherwise it is a no-op and export fail-opens.

---

## 4. Configure the Mac CLI

Build the CLI on macOS if you have not already:

```bash
cargo build -p vam-cli --release
# or: cp target/release/vam ~/.local/bin/
```

Point the CLI at the forwarded port and set the bootstrap key with **`--from-env`** (there is no `--token` flag on `set-key`):

```bash
export VAMD_BOOTSTRAP_API_KEY="vam_sk_dev_18cd74a2a97ff4d1"   # default from start script, or your pinned key
vam config set-host http://127.0.0.1:8080   # start-vamd-lima.sh runs vamd with --insecure-dev (plain HTTP), so the host is http://
vam config set-key --from-env VAMD_BOOTSTRAP_API_KEY
vam health
vam node status
```

That key is the **operator**. It is not a tenant password. For a loginable org admin, export the bootstrap-admin env **before** the start script (the script forwards both vars into Lima). If vamd is already running without those vars, export them and run the start script again — it recreates the Lima dev DB:

```bash
export VAMD_BOOTSTRAP_ADMIN=admin
export VAMD_BOOTSTRAP_ADMIN_PASSWORD='…'
./scripts/start-vamd-lima.sh
vam login --username admin
```

Which user to use next (operator vs org admin, publisher vs member): [Users and roles](/getting-started/users-and-roles/).

---

## 5. Run a session

```bash
vam run --file examples/bun-assistant/run.json --prompt "List workspace files" --follow
```

For LLM inference, add `OPENROUTER_API_KEY` (or similar) to a repo-root `.env` file. `start-vamd-lima.sh` sources it and sets `VAMD_IMPORT_ENV_SECRETS=1` automatically.

To use the [Vercel AI Gateway](https://vercel.com/docs/ai-gateway), put `AI_GATEWAY_API_KEY=...` in the same `.env`; it is imported into the vault as `vercel/api-key`. Then declare the provider on the agent: `"preferredModels": [{"provider": "vercel", "modelName": "anthropic/claude-haiku-5.5"}]`. The declared provider is honoured, so the slash model id goes to `https://ai-gateway.vercel.sh/v1/chat/completions` instead of OpenRouter. Rebuild the guest rootfs (`./scripts/build-rootfs-lima.sh`) after updating `vam-runtime`.

---

## Stop and clean up

```bash
limactl shell vam -- pkill vamd
limactl stop vam          # release 6 GiB RAM
limactl delete vam        # remove VM (optional)
```

---

## Future: prebuilt downloads

A later release may ship a zip of aarch64 **`vamd`**, **`rootfs.ext4`**, **Firecracker 1.16.1**, and **`vmlinux`** so developers can skip the Lima compile and kernel fetch. That hosting is **not implemented** in this pass — use the three scripts above.

---

## Remote x86_64 Linux host (alternative)

If you already have an **x86_64** Linux box (EC2, bare metal) with prebuilt artifacts:

```bash
# From Mac — copy prebuilt x86_64 artifacts
scp vamd rootfs.ext4 ec2-user@your-host:~/vam/target/
# on the host, point vamd at them: VAM_ROOTFS_PATH, VAMD_FIRECRACKER_BIN, VAMD_JAILER_BIN
```

On the Linux host: install Firecracker/Jailer v1.16.1, place an uncompressed `vmlinux` under `/var/lib/vam/kernels/`, start `vamd`, then on Mac:

```bash
vam config set-host https://your-host:8080   # a real node serves TLS by default (operator certificate, or set-ca for its self-signed one)
vam config set-key --from-env VAM_API_KEY
vam health
```

See [Host Prerequisites](/core/prerequisites/) for kernel and rootfs requirements.

---

## E2E validation (M3 Pro, Aug 2026)

Tested on Apple Silicon M3 Pro, macOS, Lima instance `vam`:

| Step | Result |
|---|---|
| `limactl list` | `vam` Running, aarch64, 6 GiB |
| `fetch-guest-kernel-lima.sh` | OK — `vmlinux-6.18.41` from Firecracker CI → `/var/lib/vam/kernels/vmlinux-6.18` |
| `build-rootfs-lima.sh` | OK — ~2.5 min (cached deps); output `/tmp/lima/rootfs.ext4` (256 MiB, aarch64 musl + Bun) |
| `start-vamd-lima.sh` | OK — `{"status":"SERVING"}` on `http://127.0.0.1:8080` |
| `vam config set-host` + `set-key --from-env` | OK |
| `vam health` | OK — `{"status":"SERVING"}` |
| `vam node status` | OK — `ready: true` |
| `vam run --file examples/bun-assistant/run.json --follow` | OK — `RUN_STATE_SUCCEEDED`; guest lists workspace files via `glob` tool |

**Fixes applied during validation:** Firecracker SDK `Empty` response parsing (accept `{}` bodies); `/dev/kvm` ACL for the Lima user; repo `.env` import via `VAMD_IMPORT_ENV_SECRETS=1`; dev DB reset on `start-vamd-lima.sh` restart to clear stale `guest_cid` rows.

**Note:** `start-vamd-lima.sh` no longer links anything under `/home/ec2-user/`; it exports the paths vamd needs.

---

## Next Steps

- [Users and roles](/getting-started/users-and-roles/) — first admin, operator key, publisher vs member
- [Publish a Skill Package](/authoring/publish-skill/) — local draft/release workflow on Mac + Lima
- [Quickstart (5 Min)](/getting-started/quickstart/) — secrets, HITL, and example runs
- [CLI Reference](/getting-started/cli/) — all `vam` subcommands
- [Installation & Setup](/getting-started/installation/) — remote-only Mac workflow without Lima
