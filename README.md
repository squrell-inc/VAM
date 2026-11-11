# VAM — Virtual Agentic Machine

> **Status: pre-release (0.1.0-dev).** APIs, wire formats and on-disk state may change without notice. No production-readiness or security guarantees are made yet.

VAM is a Rust workspace that runs autonomous AI agents inside hardware-virtualized sandboxes.

Rather than executing untrusted agent scripts or raw model-generated commands in shared process spaces or lightweight containers, VAM boots every agent session inside an isolated **Firecracker microVM**, utilizing host-brokered services (inference, credentials, networks, and storage) to maintain strict security boundaries.

## What ships today

Each item below is covered by tests or e2e scenarios in this repository (see `e2e/README.md`):

* **`vamd`**, the node daemon: sessions and runs, durable state, restart reconciliation, an HTTP/JSON API served over TLS by default, API keys and users.
* **`vam`**, the CLI for that API.
* **Isolation**: each session in a Firecracker microVM under the jailer, with the guest init (`vam-runtime`) talking to the host over `AF_VSOCK`. A process mode runs guests as host processes for development and CI.
* **Host-brokered services**: model inference (the guest never holds a provider key), secret injection on egress, per-agent tool and internet permissions evaluated with Cedar, human approvals.
* **Run receipts**: a signed record of what the host observed during a run, verifiable offline.
* **Hosted-service brokers** (`brokers/`): web search and fetch, schedules, notifications.

Not here yet: multi-node fleet control, cloud storage backends, and an image builder. The docs site marks every page `shipped`, `planned` or `stale`; trust the `shipped` ones.

## Prerequisites

- [rustup](https://rustup.rs/) — Rust toolchain manager
- [pnpm](https://pnpm.io/) — Package manager for the documentation site
- Node.js 22+ — Required by Astro / Starlight

### Musl cross linker (for `build-rootfs.sh`)

`scripts/build-rootfs.sh` compiles `vam-runtime` for `x86_64-unknown-linux-musl`. Cargo expects a `x86_64-linux-musl-gcc` linker on `PATH`.

Amazon Linux 2023 (and RHEL-family distros) do **not** ship a `musl-gcc` / `musl-devel` package in `dnf`. Install a prebuilt cross toolchain from [musl.cc](https://musl.cc/) instead:

```bash
rustup target add x86_64-unknown-linux-musl
sudo mkdir -p /opt
curl -L -o /tmp/x86_64-linux-musl-cross.tgz https://musl.cc/x86_64-linux-musl-cross.tgz
sudo tar xf /tmp/x86_64-linux-musl-cross.tgz -C /opt
export PATH="/opt/x86_64-linux-musl-cross/bin:$PATH"
```

On Debian/Ubuntu you can use `sudo apt install musl-tools` (provides `musl-gcc`; Rust still looks for `x86_64-linux-musl-gcc`, so symlink or set `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=musl-gcc`).

`build-rootfs.sh` also requires Docker and `e2fsprogs` (`mke2fs`, `e2fsck`, `debugfs`).


## Rust Workspace Verification

From the repository root, you can check that the current code compiles:

```bash
cargo check
cargo build
```

Guest runtime changes require rebuilding the microVM rootfs image (host `cargo build` alone does not update the guest PID 1 binary inside Firecracker VMs):

```bash
./scripts/build-rootfs.sh
```

## Running the Documentation Local Server

The full architectural guide, specifications, and dev rules are served locally via Astro Starlight:

```bash
cd docs
pnpm install
pnpm dev      # Starts local server at http://localhost:4321
pnpm build    # Validates routing and generates static output
```

## Agent / IDE Orientation

If you are an AI assistant, coding agent, or Agentic IDE exploring this workspace, **read [`AGENTS.md`](AGENTS.md) first**. It is the canonical entry point, defining the system rules, coding style, and strict workspace invariants you must follow.

## Contributing

See [`CONTRIBUTING.md`](CONTRIBUTING.md). Every commit needs a DCO `Signed-off-by` line. Report security issues privately as described in [`SECURITY.md`](SECURITY.md), never in a public issue.

## License

Licensed under either of

* Apache License, Version 2.0 ([`LICENSE-APACHE`](LICENSE-APACHE))
* MIT license ([`LICENSE-MIT`](LICENSE-MIT))

at your option. Vendored third-party code keeps its own licence; see [`NOTICE`](NOTICE).

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion in this work by you, as defined in the Apache-2.0 license, shall be dual licensed as above, without any additional terms or conditions.
