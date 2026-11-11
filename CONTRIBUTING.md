# Contributing to VAM

Thanks for helping. This page covers how to build, how to test, how work is
planned, and the sign-off every commit needs.

Read [`AGENTS.md`](AGENTS.md) too. It is the operating manual for coding agents,
and it is the shortest accurate map of the workspace for people as well.

## Build

You need [rustup](https://rustup.rs/); the toolchain is pinned in
`rust-toolchain.toml`. The docs site needs Node.js 22+ and
[pnpm](https://pnpm.io/).

```bash
cargo check --workspace
cargo build --workspace
```

Running guests in Firecracker microVMs needs a Linux host with KVM and a guest
rootfs (`./scripts/build-rootfs.sh`, or `./scripts/build-rootfs-lima.sh` on
macOS). Most work does not: process mode runs guests as host processes, and the
scenario suite uses it.

## Test

`scripts/ci/local.sh` runs the same checks as CI, with the same settings:

```bash
scripts/ci/local.sh --fast   # fmt, protos, manifests, cargo check, smoke scenarios
scripts/ci/local.sh --full   # --fast plus clippy (-D warnings) and cargo test --workspace
```

Run `--full` before you open a pull request. For a single crate,
`cargo test -p <crate>` is fine while you iterate.

Docs changes: `cargo run -p vam-e2e -- check` lints every page's `status` and
`proves` frontmatter, and `cd docs && pnpm install && pnpm build` builds the
site.

## Scenarios first

Behaviour is specified by scenarios in `e2e/scenarios/`. A change in behaviour
starts with the scenario: write it (or change it) first, see it fail for the
right reason, then make it pass. Never edit a scenario's `then` to make it
pass. Run them with `scripts/e2e/run.sh`; no KVM needed. The how-to is
[`e2e/README.md`](e2e/README.md), and the rationale is
[`design/scenarios.md`](design/scenarios.md).

Larger work is planned as a workstream: a design doc in `design/`, a phase
tracker in `.cursor/workstreams/`, then one phase at a time. Open an issue to
discuss anything beyond a small fix before you build it.

## Sign your commits (DCO)

Every commit must carry a `Signed-off-by` line, certifying the
[Developer Certificate of Origin](https://developercertificate.org/): that you
wrote the change, or otherwise have the right to submit it under the project's
licence.

```bash
git commit -s -m "Fix the thing"
```

This adds `Signed-off-by: Your Name <you@example.com>` using your git identity.
A pull request with an unsigned commit fails the DCO check; fix it with
`git rebase --signoff <base>` and force-push your branch.

By contributing you agree that your contribution is licensed under
`MIT OR Apache-2.0`, the project's licence (see `LICENSE-MIT`,
`LICENSE-APACHE`).

## Pull requests

- Keep each pull request to one change, and fill in the template.
- `cargo fmt --all` and clippy must be clean.
- Library crates use `thiserror` and never `unwrap` or `expect`; binaries use
  `anyhow`. The guest is untrusted: nothing may give it host paths, host
  sockets or credentials. See `AGENTS.md` section 4.
- Never commit secrets, `.env` files or real credentials. Tests use mock
  providers.

### How VAM is built

The maintainers use AI coding tools. Every change still goes through the same
gates: scenarios and tests first, CI, and review by a maintainer who signs off
the commit and answers for it.

### Agent-assisted pull requests

Pull requests written with coding agents are welcome. They are reviewed exactly
like any other: a human submits them, understands the change, signs off every
commit, and answers for it. Say in the description that an agent helped. The
agent should follow `AGENTS.md`. Low-effort generated pull requests are closed.

## Security issues

Do not open a public issue for a vulnerability. See [`SECURITY.md`](SECURITY.md).

## Code of conduct

This project follows the [Contributor Covenant](CODE_OF_CONDUCT.md).
