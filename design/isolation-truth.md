# Isolation truth: the claims match the node

Status: **proposed** (2026-10-21). Standing track **IT**, run as hotfix
waves beside the build order in `agent-experience.md`, not as a numbered
epic. Opens `.cursor/workstreams/isolation-truth.md` when Phase 1 is
planned. Serves P1, P2, P9, P10.

## What we are building

The gap between what the docs and the receipt say about isolation and what
the node actually does, closed from both sides: make the node do it, or
make the docs stop saying it. The code audit of 2026-10-19 found the
following on `main`:

| # | Finding | Where |
|---|---|---|
| 1 | CI has never booted a microVM. All nine real-VM tests are `#[ignore]` with hard-coded `/home/ec2-user` paths; CI runs guests as host processes | `crates/vam-core` tests, `process_vmm.rs` |
| 2 | Jailer is off by default | `crates/vam-orchestrator/src/daemon.rs:145` |
| 3 | eBPF egress filter is a stub with no dependency behind it; `network/overview.md` and the README call the network "fully implemented" | `crates/vam-network/src/ebpf.rs:18-26` |
| 4 | `vamd` HTTP API is plain HTTP; `VAMD_TLS_*` is parsed and unused | `daemon.rs` |
| 5 | Secret redaction is string matching on known secret values; encoded, partial or derived forms pass | `host_tools.rs:203` `redact_secrets` |
| 6 | Generic host-tool path allows 1 MiB output against a 64 KiB vsock frame | E10 Phase 0 wave 2 note |
| 7 | Guest egress proxy falls back to `0.0.0.0:3128` when the loopback port is taken | `vam-runtime/src/egress.rs` (`agent-experience.md` §V.5 q4, slated for E8 Phase 2) |
| 8 | No rate limiting on the API; list endpoints return everything; nothing is cleaned up by TTL | `vam-subjects.md` §2 |
| 9 | `expect("connected")` in the daemon's HSP client can panic `vamd` | `hsp.rs:184-248` |
| 10 | Hard-coded EC2 defaults | `daemon.rs:143-144`, `manager.rs:191`, `golden.rs:84` |
| 11 | `cargo clippy -D warnings` fails with about 200 warnings; CI does not run clippy | `.github/workflows/rust.yml` |
| 12 | 21 `docs/build/` pages describe an unbuilt pipeline; storage docs claim "transactional POSIX"; `project-status.md` overstates `vam-build` | `docs/src/content/docs/` |
| 13 | Vendored, unmaintained Firecracker SDK fork; `tonic 0.10` pinned; 55 duplicated dependency versions | `crates/vam-firecracker`, `Cargo.lock` |

None of these is a feature. Together they are the first page of any
security review, and the receipt (`run-receipt.md` decision 1) prints the
isolation line from the node's real configuration, so a node with the
jailer off says so on every receipt.

## Why

- The buyer VAM can win (sovereign, regulated, governed business process
  work) opens with a security review. The claims that sell the product
  (hardware isolation, kernel-level egress filtering, no secrets in the
  guest) must be true on the default install or not be made.
- The playbook design already rules: "isolation claims in docs and
  marketing must be backed by a test or a doc page". This track makes
  that rule enforceable.
- GitHub-hosted Linux runners have had KVM enabled since early 2024, so a
  Firecracker lane in CI is work, not hardware.

## Locked decisions (proposed)

1. **A claim is a scenario or it is removed.** Every isolation statement
   in `docs/` and the README names the scenario or test that proves it on
   the default configuration, or it is rewritten as planned.
2. **Secure by default.** Jailer on, TLS on, loopback-only guest proxy,
   rate limits on, on a fresh install. Turning any of them off is an
   explicit operator setting that the receipt reports.
3. **The receipt tells the truth about the node.** The isolation line is
   derived from runtime configuration, never from a constant.
4. **Build or remove, never describe.** eBPF is either built with a
   scenario behind it or removed from the docs. The 21 build pages move
   under a "planned" banner or out of the site until `vam-build` has code.
5. **Clippy in CI** at `-D warnings` once the existing warnings are
   cleared; new warnings fail the PR.
6. **No panics in the daemon's request paths.** `expect` and `unwrap`
   outside tests in `vamd` are tracked and removed.

## Phases

| Phase | Outcome | Contents | Depends on | Exit |
|---|---|---|---|---|
| **1 Prove the VM** | CI boots a microVM | A `firecracker` lane in CI on a KVM runner running the scenarios marked `needs: firecracker` (boot, snapshot restore, vsock registration, one chat run, one gate run); un-ignore the nine VM tests with runner-relative paths; jailer on by default with a scenario that proves the jailed process tree; TLS on `vamd` with a scenario that refuses plain HTTP unless `--insecure-dev` | E9 Phase 1 (done) | Every PR boots a VM; the receipt's isolation line reads `firecracker microVM, jailer on` on the CI node |
| **2 Close the leaks** | The known gaps are shut | Redaction of encoded and partial secret forms with a test corpus; host-tool output capped to the frame with chunking; guest proxy fails closed to loopback (folds E8 Phase 2 q4 forward); API rate limits and pagination defaults; TTL cleanup for sessions and approvals; `expect` removal in `hsp.rs`; EC2 defaults replaced by a config requirement | 1 | Scenarios: `secret-never-reaches-guest-encoded`, `host-tool-output-fits-frame`, `guest-proxy-never-binds-wildcard`, `api-rate-limits-a-flood` |
| **3 Honest docs and clean build** | Docs and build match the code | Docs pass: every page marked shipped, planned or stale with the proving scenario named; `build/` pages behind a planned banner; `network/overview.md` and storage pages rewritten; eBPF decision taken (build or remove); clippy clean and gated; dependency pass on the vendored SDK and pins | 1, 2 | `vam-e2e check` fails on a doc page claiming an unproven isolation property; clippy gate green |

Phase 1 can be planned now and runs beside E10 Phase 2 and E9 Phase 2; it
touches CI, `vam-core` tests, `daemon.rs` defaults and new scenarios only.

## Scenarios

- `vm-boots-and-registers-over-vsock` (`needs: firecracker`)
- `jailer-confines-the-vmm-process` (`needs: firecracker`)
- `vamd-refuses-plain-http-by-default`
- `secret-never-reaches-guest-encoded`
- `host-tool-output-fits-frame`
- `guest-proxy-never-binds-wildcard`
- `api-rate-limits-a-flood`
- `receipt-isolation-line-matches-node-config`

## Open questions

1. KVM runner: GitHub-hosted `ubuntu-latest` (KVM available, slower
   boot) or a self-hosted KVM runner? Proposal: hosted first, measure.
2. eBPF: **decided by the owner 2026-10-28: remove.** The stub
   (`crates/vam-network/src/ebpf.rs`) and every doc claim go in Phase 3.
   Reason: every session runs the brokered network mode, where the guest
   has no route and the host proxy makes every connection, so there is no
   packet path to filter; the honest claim is stronger than the eBPF one.
   Reopen as its own design when one of three triggers appears: a
   direct-route network mode for traffic the proxy cannot terminate (E8
   CONNECT passthrough keeps the host in the path, so not that); the
   userspace proxy becoming the bottleneck at many VMs per host; or a
   buyer requiring kernel-enforced egress as an attestable control. The
   first answer then is nftables with an address set the DNS proxy fills;
   eBPF after that, for scale.
3. TLS: self-signed on first start with the CA printed for the app, or
   require an operator certificate? Proposal: self-signed by default,
   operator certificate by config.
4. Which of the 21 build pages survive as "planned" and which move to
   `design/`?

## What we are not building

- No new isolation model. Firecracker, jailer, host brokers, Cedar.
- No eBPF work before the decision in question 2.
- No security certification work (SOC 2, ISO 42001); this track makes
  the claims true, not audited.

## Dependency pass (Phase 3)

Written by IT-3.9 on 2026-10-30 from `origin/feat/isolation-truth`
. Design finding 13 counted 55 duplicated crates; Phase 1 W3
dropped openssl and native-tls, and the count today is below.

**Counts.** `cargo tree -d | wc -l`: **1183 lines before, 1171 after**.
Duplicated crates counted on normal and build edges (what ships): **34
before, 33 after**. Counting dev edges as well gives 42 before.

**Advisories.** `cargo audit` is not installed here, and the advisory
databases (RustSec on GitHub, OSV) were blocked by the sandbox proxy, so
no advisory check ran. No bump below is justified by an advisory. Run
`cargo audit` on a networked machine before the launch cut.

### Safe bumps made

- `crates/vam-security/Cargo.toml`: removed the unused
  `prost-types = "0.13"` (nothing in the crate uses it). Removes `prost`,
  `prost-derive` and `prost-types` 0.13.5 from the lock; no code change.
- No lockfile patch bump removes a duplicate: every duplicate is a
  different minor or major, so none is made.

### Duplicates by root cause (normal and build edges)

| Root cause | Duplicated crates | Pinned by |
|---|---|---|
| tonic 0.10, axum 0.6, hyper 0.14 stack | hyper, h2, http, http-body, tower, tower-http, socket2, bitflags, sync_wrapper, indexmap, hashbrown, base64 0.21 | `vam-spec`, `vam-orchestrator`, `brokers/vam-web` (tonic 0.10); `vam-orchestrator` (axum 0.6, tower 0.4, tower-http 0.4, hyper 0.14, http 0.2) |
| rustls 0.21 and reqwest 0.11 | rustls, rustls-webpki, tokio-rustls, hyper-rustls, webpki-roots 0.25 | `axum-server` 0.5 and `reqwest` 0.11 (direct in `vam-cli`, `vam-e2e`, `vam-inference`, `vam-network`, `vam-security`, `vam-orchestrator`, `vam-web`); `vam-telemetry` uses reqwest 0.13, `vam-network` rustls 0.23 |
| prost 0.12 against 0.14 | prost, prost-derive, itertools, petgraph, fixedbitset | `prost` 0.12 in most crates and `tonic-build`/`prost-build` 0.12; opentelemetry-proto brings 0.14; cedar brings itertools 0.15 |
| rand 0.8 against 0.9 | rand, rand_core, rand_chacha, getrandom 0.2/0.3/0.4 | `vam-orchestrator`, `vam-telemetry`, `vam-vsock` and `tower` 0.4 on 0.8; `tempfile` and `uuid` on getrandom 0.4 |
| thiserror 1 against 2 | thiserror, thiserror-impl | workspace pin `1.0` (all `vam-*` crates); dependencies use 2 |
| webpki-roots | webpki-roots 0.25, 0.26, 1.0 | `vam-network` pins `0.26` (a shim over 1.0); bumping to `1` removes no duplicate (0.25 and 1.0 remain) |
| Tooling | syn 2/3, string_cache, phf_shared, unicode-width | lalrpop and miette (transitive) |

### The vendored Firecracker SDK (`crates/vam-firecracker`)

- **What VAM uses.** `vam-core` uses `models` (boot_source, drive, logger,
  machine_configuration, metrics, vsock), `jailer::JailerOption`,
  `firecracker::FirecrackerOption`, `events` and `Instance` with the
  `_rt-std` feature (`spawn_vmm`, `pause`, `resume`, `describe_instance`,
  the `put_guest_*` calls). Only `vmm.rs` and `config.rs` touch it.
- **Divergence.** Upstream `firecracker-rs-sdk` 0.1.0 is 3,938 lines; the
  fork is 4,222. 15 files differ, about 544 changed lines, plus
  `models/partial_vsock.rs` added and `fstack.rs` removed. The fork fixed
  the HTTP-status bug (a refused `load_snapshot` returned Ok until W4b).
  Three `/home/ec2-user` defaults remain in `agent/rt_*.rs` (the
  `FIRECRACKER` env fallback).
- **Is upstream alive.** One crates.io release (0.1.0, 2025-02-09, about
  2,000 downloads), 11 commits, no releases. Treat it as dormant: there
  is nothing to re-sync to.
- **Options.** (a) Keep the fork. (b) Re-sync to upstream: not possible,
  nothing newer exists. (c) Replace with a thin in-tree client of the
  Firecracker API socket (the dozen calls above): about 1,000 lines,
  touches `vam-core`, the vsock and API paths, needs jailed re-proof.
- **Recommendation.** Keep the fork. Trim the unused async runtimes and
  the `/home/ec2-user` defaults when a wave next edits the crate.
  `Decide: BACKLOG — proof 1 (finding 13), cost 3 (security-relevant API and vsock paths, jailed re-proof)`

### Upgrades not made

| Upgrade | Size and files | Risk | Decide |
|---|---|---|---|
| tonic 0.12 / prost 0.13 / hyper 1 / axum 0.7 / axum-server / rustls 0.23 / tower 0.5, as one move | Large. `vam-spec` codegen (`tonic-build`), `vam-orchestrator` (axum routes, TLS listener, `tower-http`), `brokers/vam-web`, `vam-cli`; every `prost` user moves with it. The TypeScript HSP brokers stay wire-compatible (protobuf wire format does not change) | High: the TLS listener, client auth and the vsock and API paths are security-relevant; needs HSP wire tests and a TLS review | `Decide: BACKLOG — proof 1, cost 3` |
| reqwest 0.11 to 0.13 | Medium. 7 crates (`vam-cli`, `vam-e2e`, `vam-inference`, `vam-network`, `vam-security`, `vam-orchestrator`, `vam-web`); feature names change (`rustls-tls` to `rustls`). `vam-inference` belongs to H1 | Medium: TLS client defaults change | `Decide: BACKLOG — proof 0, cost 2, wait until H1 is clear of vam-inference` |
| thiserror 1 to 2 | Small. Workspace pin plus `vam-firecracker`; removes 2 duplicates | Low: derive attributes are source compatible | `Decide: BACKLOG — proof 0, cost 1, carry into a wave that already edits workspace dependencies` |
| rand 0.8 to 0.9 | Small to medium. `vam-orchestrator`, `vam-telemetry`, `vam-vsock`; API renames (`gen` to `random`); `tower` 0.4 keeps 0.8 until the stack moves | Low to medium: `vam-vsock` uses it for IDs and nonces | `Decide: BACKLOG — proof 0, cost 1, do after the tower move` |
| webpki-roots 0.26 to 1 | One line in `vam-network`; removes no duplicate (0.25 and 1.0 remain) | Medium for its size: it is the TLS root store of the egress proxy | `Decide: BACKLOG — proof 0, cost 0, take it with the reqwest or rustls move` |
| Vendored SDK replacement | See above | See above | `Decide: BACKLOG — proof 1, cost 3` |
