# Workstream: Isolation truth (E13)

```yaml
status: complete (Phase 4 held)
slug: isolation-truth
current_phase: complete (Phase 3 released to main 2026-11-09, merge commit; checklist out). Phase 4 (developer docs) waits on the owner's review of the outline. Phases 1 and 2 released to main, checklists out
design_docs:
  - design/isolation-truth.md
  - design/launch-0.1.0.md
base: vam feat/isolation-truth (cut from main 2026-10-21)
waves: it-1/wave-N, PRs into feat/isolation-truth
orchestrator: cloud session (owner, 2026-10-21)
```

`Decide: NOW — proof 3 (security page), cost 2 (one phase; jailer/TLS are security floor)`

## Goal

What the docs and the receipt say about isolation is what the node does on a
default install. Every isolation claim names the scenario or test that proves
it, or it is rewritten as planned. Done when the three phases in
`design/isolation-truth.md` are closed: CI boots a microVM on every PR with the
jailer and TLS on by default (Phase 1), the known leaks are shut (Phase 2), and
the docs and the build match the code (Phase 3).

## Out of scope

- A new isolation model (Firecracker, jailer, host brokers, Cedar stay).
- eBPF work before open question 2 of the design is decided.
- Security certification (SOC 2, ISO 42001).
- E10 runtime behaviour. This track touches CI (`.github/workflows/rust.yml`),
  `vam-core` tests, daemon defaults and new scenarios only.

## Locked decisions

Do not re-litigate without editing this list. From
`design/isolation-truth.md`; locked by the owner 2026-10-21 with the Phase 1 breakdown.

1. **A claim is a scenario or it is removed.**
2. **Secure by default.** Jailer on, TLS on, loopback-only guest proxy, rate
   limits on, on a fresh install. Turning any off is an explicit operator
   setting the receipt reports.
3. **The receipt tells the truth about the node.** The isolation line is derived
   from runtime configuration, never from a constant.
4. **Build or remove, never describe.**
5. **Clippy in CI** at `-D warnings` once the existing warnings are cleared.
6. **No panics in the daemon's request paths.**

## Coordination with E10 (agent-runtime, `feat/agent-runtime`)

- The TLS default must not break process-mode scenarios: the e2e runner passes
  `--insecure-dev` or speaks TLS.
- Once the firecracker lane is green it also runs E10's
  `workspace-matches-conversation-after-vm-stop` (on `feat/agent-runtime` until
  E10 releases). That unblocks E10 Wave 6.
- Files both epics may touch are listed per wave in the breakdown.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases (archived)

Full sections: not kept; see the progress log and git history.

| Phase | Name | Done |
|---|---|---|

### Phase 1 — Prove the VM

**Goal:** CI boots a microVM on every PR, with the jailer and TLS on by default.
**Depends on:** E9 Phase 1 (done)
**Plan:** approved 2026-10-21 (owner: all open-question defaults; TLS deploy note accepted)
**Exit:** every PR boots a VM; the receipt's isolation line reads
`firecracker microVM, jailer on` on the CI node. Until E12 builds the receipt
(not on `main`), the line is the node's own (`GET /v1/node`, `vam node`),
from the one function the receipt will call (open question 6).

- [x] `firecracker` CI lane on a KVM runner (hosted ubuntu first, measured)
  running the `needs: firecracker` scenarios: boot, snapshot restore, vsock
  registration, one chat run, one gate run
- [x] The nine `#[ignore]` VM tests un-ignored with runner-relative paths
  (in fact 4 in `vam-core` and 5 in `vam-orchestrator`; 4 of those need a live
  key: open question 7)
- [x] Jailer on by default, with a scenario proving the jailed process tree
- [x] TLS on `vamd`, refusing plain HTTP unless `--insecure-dev`

**What the code map found** (2026-10-21, `feat/isolation-truth`)

- The runner has no VM mode. `vam-e2e run` always starts a `process-guest`
  vamd and prints `SKIP (needs firecracker)` for every other scenario
  (`crates/vam-e2e/src/run_cmd.rs:121`); `--smoke` is every `needs: process`
  scenario. No `needs: firecracker` scenario exists on this base.
- The "nine VM tests in `vam-core`" are 4 in `crates/vam-core/tests/integration.rs`
  and 5 in `crates/vam-orchestrator/tests/e2e_tests.rs`. Four of the five need
  `VAM_LIVE_INFERENCE` and `OPENROUTER_API_KEY`. The `vam-core` ones also use a
  stale guest (`/var/lib/vam/rootfs/ubuntu-22.04.ext4`,
  `/usr/bin/vam-guest-agent` as init).
- The jailer path has never run through vamd: `FirecrackerVmm` hard-codes
  uid/gid 1000 and gives the chrooted Firecracker host paths for its sockets
  (`/tmp/vam-fc-*.socket`, `/tmp/vam-vsock-*.socket`, `vmm.rs:305-306`).
  Adopted VMMs are torn down as unjailed (`manager.rs:47`, `vmm.rs:796`). The
  vendored jailer defaults to cgroup v1 (`vam-firecracker/src/jailer.rs:39`);
  hosted Ubuntu is cgroup v2.
- TLS: `VAMD_TLS_CERT`/`VAMD_TLS_KEY` are parsed (`daemon.rs:218-223`) and
  only logged (`bin/vamd.rs:418`); `api::serve` is plain axum 0.6. `rcgen` is
  already in the tree (`vam-network`, for the egress CA). Every client speaks
  plain HTTP today: the CLI default `http://127.0.0.1:8080`
  (`vam-cli/src/config.rs:153`), the runner (`vam-e2e/src/vamd.rs` `base_url`,
  `reqwest` without a TLS feature), `start-vamd-lima.sh`, `bench-coldstart.sh`,
  and the VAM app.
- No receipt on `main` (E12 is proposed). `NodeStatus` has `guest_mode` only
  (`orchestrator.proto:85`).
- `rootfs.ext4` is not in the repo (gitignored; AGENTS.md says it is).
  `scripts/build-rootfs.sh` hard-codes `/home/ec2-user`, needs docker and sudo,
  and downloads the latest Bun; `fetch-guest-kernels.sh` takes the latest
  Firecracker CI kernel. The Mac's Lima VM is aarch64 (Firecracker 1.16.1); the
  hosted lane is x86_64.
- `session.booted` carries `bootPath` (`claim`, `restore`, `cold`) but
  `boot_duration_ms` is always 0 (`manager.rs:1581`, `:1816`). Lane boot times
  come from the VM tests' printed latencies.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `vm-boots-and-registers-over-vsock` | new | firecracker | `org-admin`, `agent-with-builtins` | A chat on a microVM node: the guest boots and registers over vsock, the mock reply comes back, `write_file` lands in the workspace (Drive9 as an ext4 device), `session.booted` is on the run. The exit "every PR boots a VM" |
| `vm-restores-from-the-golden-snapshot` | new | firecracker | `org-admin`, `agent-plain` | With snapshot restore on, a new session boots with `session.booted` `bootPath: restore` and its chat replies; a second new-thread chat restores too. Guards `golden.rs`, `manager.rs` boot path |
| `vm-run-waits-at-a-gate-and-finishes` | new | firecracker | `org-admin`, `agent-plain`, `playbook-with-gate` | A playbook run on a microVM parks at its gate and finishes after the decision: park and resume on a real VM |
| `jailer-confines-the-vmm-process` | new | firecracker | `org-admin`, `agent-with-guarded-tool` | On a default node, while a run's microVM is up (parked on an approval), its Firecracker process runs under the jailer: chrooted under the jailer's directory, a non-root uid, its own PID and mount namespaces. Seen from the host's process table |
| `vamd-refuses-plain-http-by-default` | new | process | `org-admin` | A default vamd answers HTTPS with its certificate; a plain-HTTP request to the same address gets no API answer (no 2xx, no body), and no key is sent on it |
| `node-reports-the-isolation-it-runs` | new | firecracker | `org-admin` | `GET /v1/node` on the lane's default node reads `firecracker microVM, jailer on` with TLS on, from the running config (locked decision 3). Stands in for the design's `receipt-isolation-line-matches-node-config`, which stays for E12 |

Unchanged, and must stay green under the TLS default: all 11 process
scenarios (explicit acceptance check of IT-1.6). Arrives with E10, not copied
here: `workspace-matches-conversation-after-vm-stop` (`needs: firecracker`,
`pending`, on `feat/agent-runtime`); the lane runs every `needs: firecracker`
scenario, so it joins the lane when E10 releases and the bases take `main`. It
needs E10's runner forms (`chat {wait: false}`, the new `stop-session` wait).

**Benchmark delta**

No benchmark line moves (`design/benchmark.md` §1): no model, tool or
procedure behaviour changes, so no baseline run and no paid endpoint. Launch
proof moved: **the security page** (`launch-0.1.0.md` §1: CI boots a VM;
jailer and TLS on by default). Baseline on this base : 0 microVM
boots in CI, 0 of 9 VM tests run, jailer off, vamd plain HTTP. The phase
close records after: VM boots per PR, lane minutes (cold and warm cache),
cold-boot and registration latency on the lane, jailer on, TLS on.

**Wave 0 "scenarios"** (Sonnet low)

Writes the six scenarios as `pending` with `expectFail`, plus the three
expectation forms they need, harness only (`crates/vam-e2e`: `scenario.rs`,
`check.rs`, `engine.rs`, self-tests, `e2e/README.md` lines): a node
expectation (`GET /v1/node` fields, the existing `fields` matcher), a
plain-HTTP probe expectation (refused: no 2xx and no API body), and a VMM
process expectation (jailed: read from `/proc` on the host, as `file` reads the
workspace). Red for the right reason, honestly:

- `vamd-refuses-plain-http-by-default` is red now on the process layer:
  `scripts/e2e/run.sh --scenarios vamd-refuses-plain-http-by-default` fails at
  the plain-HTTP probe, class `product` (vamd answers 200).
- The five `needs: firecracker` scenarios cannot be run until the lane
  exists; `vam-e2e check` is their only gate in wave 0. They land `pending`:
  `jailer-confines-the-vmm-process` with `expectFail` at the jailed step and
  `node-reports-the-isolation-it-runs` at the node step (class `product`: the
  jailer is off, the field is absent); the three boot, restore and gate
  scenarios at `when.0` class `env` ("no VM node"). Wave 2 runs them on the
  lane for the first time: each then passes (flip to `active`) or fails at the
  step the phase adds. Re-pointing `expectFail` is not a `then` change.

**Items**

- [x] **IT-1.1 Lane assets.** One script installs pinned Firecracker and
  jailer (x86_64 release, sha256-checked, same minor as Lima's 1.16.1), a
  pinned guest kernel (exact Firecracker CI object key and sha256, not
  "latest"), and builds the rootfs from the tree's `vam-runtime` with
  `scripts/build-rootfs.sh` made runner-relative (no `/home/ec2-user`, chown to
  the caller, Bun pinned or skipped by env; the default dev-host behaviour
  stays). *Files:* `scripts/e2e/firecracker-assets.sh` (new),
  `scripts/build-rootfs.sh`. *Accept:* on a hosted runner it exits 0 and prints
  the four paths; a warm cache downloads nothing. *Test:* `bash -n`, the lane
  run. *Turns green:* none.
  `Decide: NOW — proof 3, cost 1, line security page (CI boots a VM)`
- [x] **IT-1.2 Runner VM mode.** `scripts/e2e/run.sh --firecracker` builds the
  default vamd (not `process-guest`) and `vam-e2e run --firecracker` starts it
  with the IT-1.1 assets (`VAMD_FIRECRACKER_BIN`, `VAMD_JAILER_BIN`,
  `VAM_KERNEL_PATH`, `VAM_ROOTFS_PATH`), snapshot restore on and the mock
  scripts as today. It selects every non-retired `needs: firecracker` scenario,
  preflights `guestMode: vm` (else class `env`), and puts Firecracker
  stdout/stderr in the briefs. `--smoke` unchanged. *Files:* `crates/vam-e2e`
  (`run_cmd.rs`, `vamd.rs`, `engine.rs` preflight, `main.rs`, tests),
  `scripts/e2e/run.sh`, `e2e/README.md`, the lane step in `rust.yml`.
  *Accept:* the lane runs the five firecracker scenarios; boot, restore and
  gate pass and flip to `active`; jailer and node stay red at their step;
  `--smoke` green; self-tests for selection and preflight. *Test:*
  `cargo test -p vam-e2e`, `cargo run -p vam-e2e -- check`,
  `scripts/e2e/run.sh --smoke`, the lane. *Turns green:*
  `vm-boots-and-registers-over-vsock`, `vm-restores-from-the-golden-snapshot`,
  `vm-run-waits-at-a-gate-and-finishes`.
  `Decide: NOW — proof 3, cost 1, line security page (CI boots a VM)`
- [x] **IT-1.3 CI `firecracker` job.** A new job in `rust.yml` on
  `ubuntu-latest`: free disk (as E10's `test` job does), `/dev/kvm` access
  (udev rule), musl target and `musl-tools`, `rust-cache`, `actions/cache` for
  the assets keyed on pinned versions and the `crates/vam-runtime` hash; runs
  the VM tests (IT-1.4), then (after IT-1.2) `scripts/e2e/run.sh --firecracker`;
  uploads briefs and Firecracker logs on failure; writes boot latencies and
  job minutes to the job summary. `timeout-minutes: 35`. Advisory at first
  (open question 4). *Files:* `.github/workflows/rust.yml` (new job only).
  *Accept:* green on the wave PR; the summary shows cold-boot and registration
  latency and total minutes, cold and warm. *Turns green:* none.
  `Decide: NOW — proof 3, cost 1, line security page (CI boots a VM)`
- [x] **IT-1.4 VM tests on the lane.** The 4 `vam-core` integration tests and
  `test_vamd_real_firecracker_e2e` read binaries, kernel and rootfs from env
  (`VAM_FIRECRACKER_BIN`, `VAM_JAILER_BIN`, `VAM_KERNEL_PATH`,
  `VAM_ROOTFS_PATH`), with no EC2 or stale-guest default; they boot the built
  rootfs (`/sbin/init` is `vam-runtime`); `#[ignore = "needs KVM: firecracker
  lane"]` keeps them out of the `test` job and the lane runs them with
  `--ignored`. A test that is stale against today's guest is fixed if small,
  else deleted with the covering scenario named in the PR. The 4 live-model
  tests stay ignored with that reason (open question 7). *Files:*
  `crates/vam-core/tests/integration.rs`,
  `crates/vam-orchestrator/tests/e2e_tests.rs`. *Accept:* 5 VM tests green on
  the lane; `cargo test -p vam-core` unchanged locally. *Test:*
  `cargo test -p vam-core --test integration -- --ignored` (lane).
  *Turns green:* none (tests, not scenarios).
  `Decide: NOW — proof 3, cost 1, line security page (claims backed by a test)`
- [x] **IT-1.5 The node reports its isolation.** `NodeStatus` gains an additive
  `isolation` message: `vmm` (`firecracker` | `process`), `jailer`, `api_tls`,
  and `summary`, the receipt's line (`firecracker microVM, jailer on`,
  `firecracker microVM, jailer off`, `process guest (no isolation)`), built by
  one function from the running config that E12's receipt will call; `vam
  node` prints the line. No baseline descriptor change (additive). *Files:*
  `proto/vam/v1/orchestrator.proto`, `crates/vam-orchestrator/src/api/mod.rs`
  (`get_node_status`), `crates/vam-orchestrator/src/lib.rs`,
  `crates/vam-cli/src/cmd/node.rs`, unit tests. *Accept:* unit tests for the
  three lines; on the lane `node-reports-the-isolation-it-runs` fails at its
  node step reading `jailer off` (still `pending`, right reason). *Test:*
  `cargo test -p vam-orchestrator --lib`, `cargo test -p vam-cli`,
  `cargo test -p vam-spec`. *Turns green:* none until IT-1.7.
  `Decide: SLICE — proof 3 (exit line), cost 2 (proto field) → slice: one additive NodeStatus field and the vam node line; the receipt line stays E12`
- [x] **IT-1.6 TLS on vamd.** vamd serves the API over TLS by default: an
  operator certificate from `VAMD_TLS_CERT`/`VAMD_TLS_KEY`, else a self-signed
  one made on first start, kept in the state dir and reused across restarts,
  its fingerprint and CA path logged. `--insecure-dev` (and an env twin) serves
  plain HTTP with a warning and the node reports TLS off. A bad or half-set
  operator pair stops vamd. The CLI defaults to `https://`, trusts a CA file
  (`vam config` setting and env), and explains an `http://` host against a TLS
  node. The runner speaks TLS and trusts the node CA from its state dir (open
  question 2). `start-vamd-lima.sh` and `bench-coldstart.sh` follow (open
  question 8). The telemetry gRPC listener (loopback) is out of scope.
  *Files:* `crates/vam-orchestrator` (`daemon.rs`, `bin/vamd.rs`,
  `api/mod.rs` `serve`, `Cargo.toml`), `crates/vam-cli` (`config.rs`, client),
  `crates/vam-e2e` (`api.rs`, `vamd.rs`, `Cargo.toml`), the two scripts.
  *Accept:* `vamd-refuses-plain-http-by-default` green; **`scripts/e2e/run.sh
  --smoke` green with the TLS default (no scenario changed)**; the lane green;
  unit tests for self-signed reuse after `restart-vamd`, operator cert,
  `--insecure-dev`, bad pair. *Test:* `cargo test -p vam-orchestrator --lib`,
  `cargo test -p vam-cli`, `cargo test -p vam-e2e`, `scripts/e2e/run.sh
  --smoke`, the lane. *Turns green:* `vamd-refuses-plain-http-by-default`.
  `Decide: NOW — security floor (the API key travels in clear), proof 3, cost 2, line security page (TLS on by default)`
- [x] **IT-1.7 Jailer on by default.** `use_jailer` defaults to true;
  `--no-jailer` / `VAMD_USE_JAILER=0` is the explicit opt-out, logged and
  reported (`jailer off`). A VM node that cannot jail (not root, no jailer
  binary, no cgroup v2) refuses to start unless opted out; process-guest nodes
  are unaffected. Make the jailed path work end to end: host-side paths of the
  API and vsock sockets, kernel, rootfs, Drive9 image and snapshot files inside
  the chroot; uid/gid from config, not 1000; `--cgroup-version 2`; teardown
  (adopted VMMs too) removes the jail directory. The lane runs vamd as root.
  *Files:* `crates/vam-orchestrator/src/daemon.rs`, `bin/vamd.rs`,
  `manager.rs` (`adopted_vmm`, teardown), `golden.rs` (snapshot paths),
  `crates/vam-core/src/vmm.rs`, `crates/vam-firecracker/src/jailer.rs` if
  needed, the lane step. *Accept:* `jailer-confines-the-vmm-process` and
  `node-reports-the-isolation-it-runs` green on the lane; boot, restore and
  gate stay green with the jailer on (open question 9); one VM test each with
  the jailer on and off; unit tests for the default, the opt-out and the
  refusal. *Test:* `cargo test -p vam-core`, `cargo test -p vam-orchestrator
  --lib`, the lane. *Turns green:* `jailer-confines-the-vmm-process`,
  `node-reports-the-isolation-it-runs`.
  `Decide: NOW — security floor (VMM unconfined on the default install), proof 3, cost 3, line security page (jailer on by default)`

- [x] **IT-1.8 CI minutes.** Keep paid Actions minutes near constant
  (owner, 2026-10-24: GitHub Pro, 3,000 min/month shared by every epic; about
  15-20 min per push today). In `.github/workflows/rust.yml`: the `firecracker`
  job runs only when the VM path changes (a `changes` job or path filter over
  `crates/vam-core`, `crates/vam-firecracker`, `crates/vam-runtime`,
  `crates/vam-orchestrator`, `crates/vam-vsock`, `crates/vam-storage`,
  `scripts/e2e`, `scripts/build-rootfs.sh`, `e2e/scenarios`, `e2e/world`,
  `proto`, `Cargo.lock`, `rust.yml`), plus `workflow_dispatch`; the
  firecracker job's free-disk step trimmed to what the lane needs (measured
  before/after); `rustfmt`, `example JSON protos` and `release gate` merged into
  one job (per-job minute rounding) with the same check names kept as steps;
  no job runs on draft PRs (`if: github.event.pull_request.draft == false` or
  `types: [opened, synchronize, reopened, ready_for_review]`). *Accept:* a
  docs-only and a non-VM PR skip the lane; a VM-path PR runs it; minutes per
  push recorded before and after in the PR. *Files:* `rust.yml` only.
  `Decide: NOW — proof 1, cost 0, line CI minutes per push`

- [x] **IT-1.9 Golden restore: timeouts, timing and evidence.** (owner, option B: reliability moved to IT-1.10) On the x86_64 lane the
  restore is intermittent: on some runs every restored guest never answers
  `ResumeFromSnapshotRequest` (reseed timeout), and `vamd` falls back to a cold
  boot (`bootPath: cold`, about 19 s per session including the timeout). Find
  why (reseed in `golden.rs` / `manager.rs`, or the guest's resume handler in
  `vam-runtime`) and fix it; the fallback stays cold, never jailer off (open
  question 9). *Accept (owner, 2026-10-24, option a):* `vm-restores-from-the-golden-snapshot` (already `active`) green
  on three consecutive lane runs, and the lane summary shows restore latency and
  its per-phase split; making restore fast is IT-1.10; restore latency recorded in the lane summary.
  *Files:* to be planned (`golden.rs`, `manager.rs`, `vam-runtime`); note the
  E10 overlap in `manager.rs`. *Turns green:* `vm-restores-from-the-golden-snapshot`.
  `Decide: NOW — proof 2 (launch demo assumes ~115 ms restore; confirmed defect), cost 1, line restore latency`

- [x] **IT-1.10 Deterministic bake and restore latency.** (owner, 2026-10-24,
  option B: after W3, before W4; Sonnet high, the bake is a host-guest timing
  problem and medium used its two cycles in W2c.) Two findings from W2c: (1)
  restore is all-or-nothing per bake: some golden bakes produce a snapshot
  whose restored guest never answers `ResumeFromSnapshotRequest`, even in 30 s;
  suspected cause, the snapshot is taken at a non-deterministic point after
  registration (the guest is still starting its flush loop and egress dial);
  fix by a guest "ready to snapshot" signal in a quiescent state, confirmed
  first by the guest console and checkpoints W2c added; (2) when restore works
  the reseed takes under 120 ms, and the 8.6 s session start is host-side
  (after the reseed). *Accept:* `vm-restores-from-the-golden-snapshot` green on
  three consecutive lane runs; restore latency and the session-start split in
  the lane summary; the launch claim "~115 ms restore" proven or rewritten.
  `Decide: NOW — proof 2 (launch demo claim, confirmed defect), cost 2, line restore latency`

**Waves**

Each wave: one session in its own worktree of vam, base `feat/isolation-truth`,
branch `it-1/wave-N`, plan-only first turn, PR titled `IT-1 WN: …`. Cloud
sessions have no `/dev/kvm`: waves 1, 2 and 4 iterate on the lane through
their PR's CI runs (about 15 to 25 minutes a cycle), which the budgets include.
Known local failure not to chase: `hsp_uds::uds_socket_acl_deny_fails_fast` as
root. No local `cargo test --workspace`.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Status | PR |
|---|---|---|---|---|---|---|---|---|
| 0 | scenarios (+ three expectation forms) | — (red, right reason: plain-HTTP on process; the rest `check` only) | vam | Sonnet low | 75 min | — | [x] | — |
| 1 | IT-1.1, IT-1.3, IT-1.4 | — (the lane boots VMs: 5 VM tests) | vam | Sonnet medium: crosses scripts, CI and two crates' tests; the first VM boot in CI, debugged only through CI runs | 120 min | 0 | [x] | — |
| 2 | IT-1.2, IT-1.5 | `vm-boots-and-registers-over-vsock`, `vm-restores-from-the-golden-snapshot`, `vm-run-waits-at-a-gate-and-finishes` | vam | Sonnet medium: a new runner mode plus a proto field and API | 120 min | 1 | [x] | — |
| 2c | IT-1.9 | `vm-restores-from-the-golden-snapshot` reliable | vam | Sonnet medium: crosses host (golden reseed) and guest (resume handler) | 120 min | 2 | [x] | — |
| 2b | IT-1.8 | — (minutes per push) | vam | Sonnet low | 45 min | 2c | [x] | — |
| 3 | IT-1.6 | `vamd-refuses-plain-http-by-default` | vam | Sonnet high: the API transport and key handling (security boundary), across vamd, CLI, runner and scripts | 120 min | 2 | [x] | — |
| 3b | IT-1.10 | `vm-restores-from-the-golden-snapshot` reliable (three consecutive lane runs) | vam | Sonnet high: host-guest timing at the golden bake; medium used its two cycles in 2c | 150 min | 3 | [x] | — |
| 4a | IT-1.7 (jailed cold boot) | `jailer-confines-the-vmm-process`, `node-reports-the-isolation-it-runs` | vam | Sonnet high: VMM confinement (security boundary) | 110 min | 3b | [x] | — |
| 4b | IT-1.7 (jailed bake and restore) | `vm-restores-from-the-golden-snapshot` back to `active` | vam | Sonnet high | 100 min | 4a | [x] | — |

Wave 1 lands the lane first so every later wave is proven on it. Wave 3 goes
before wave 4 so the jailer is proven on a TLS node; they do not share files
beyond `daemon.rs` and `bin/vamd.rs` (different functions) and could swap if
wave 3 stalls. Wave decision lines: W0
`Decide: NOW — proof 3, cost 1, line security page`; W1, W2
`Decide: NOW — proof 3, cost 1, line security page (CI boots a VM)`; W3, W4
as their items.

**Open questions** (defaults in bold)

1. KVM runner: **GitHub-hosted `ubuntu-latest`**, measured in wave 1. Move to
   a self-hosted KVM runner only if a cold lane run passes 30 minutes or KVM
   flakes twice.
2. Process scenarios under the TLS default: **the runner speaks TLS**, trusting
   the node's CA from its state dir, so every PR exercises the default;
   `--insecure-dev` is covered by a vamd test. If that does not fit wave 3,
   the process runner passes `--insecure-dev` and
   `vamd-refuses-plain-http-by-default` moves to the firecracker layer (spec
   change, owner).
3. TLS certificate: **self-signed on first start, operator certificate by
   config** (design q3). The team instance runs an operator certificate the
   app's TLS stack already trusts, so **The VAM app needs no change in Phase 1**;
   CA pinning in the app goes with the install slice (`vam node init`).
4. The lane on PRs: **advisory first** (runs on every PR, not a required
   check) until green on 5 consecutive PRs, then required, at the latest at
   the phase close. The owner sets branch protection.
5. Assets in CI: **pinned Firecracker release and kernel object (sha256),
   rootfs built in the lane from the PR's `vam-runtime`, all cached with
   `actions/cache`** (versions plus the `crates/vam-runtime` hash). No rootfs
   in git or in releases. Estimate: about 20 to 25 minutes cold, 10 to 12 warm
   (musl release build 3 to 5, default vamd 4 to 6, rootfs 1 to 2, scenarios
   and tests 5); wave 1 records the real numbers.
6. Receipt line: E12 is not on `main`. **Phase 1 proves the node's line
   (`GET /v1/node` `isolation.summary`, `vam node`) from the function the
   receipt will call**; the exit reads "the node's isolation line" until E12.
   `receipt-isolation-line-matches-node-config` stays in the design for E12.
7. The four live-model VM tests (`e2e_tests.rs`, OpenRouter): **stay ignored
   with the reason** (mock only; a real model on the owner's word). E10's
   firecracker scenario covers the follow-up and checkpoint flows they probe.
8. Dev hosts: **`start-vamd-lima.sh` (aarch64, non-root) passes `--no-jailer`
   and `--insecure-dev` explicitly and prints it**; the node then reports
   `jailer off`. The owner may ask for a root vamd in Lima instead.
9. Golden restore under the jailer: if it cannot work inside the chroot in
   wave 4's budget, **a jailed node falls back to cold boot** (`bootPath:
   cold`), never jailer off; `vm-restores-from-the-golden-snapshot` goes back
   to `pending` with a known issue and the owner decides.
10. E10 Wave 6 wants the lane: **release Phase 1 to `main` at its close**; if
    E10 Wave 6 is ready first, the owner may release waves 1 and 2 early (the
    lane only, defaults unchanged).
11. How the jailer scenario catches the VM alive: **while its run is parked on
    an approval** (`agent-with-guarded-tool`, the VM still warm); else right
    after the run, inside keep-warm. Wave 0 picks and records it.

**Coordination with E10** (`origin/main...origin/feat/agent-runtime`, 52
commits, for the paths this phase touches)

- `.github/workflows/rust.yml`: E10 adds "Free runner disk" and "Disk after
  tests" to the `test` job. This phase adds a new `firecracker` job only and
  copies the disk step into it. **Low.**
- `crates/vam-e2e` (`scenario.rs`, `engine.rs`, `check.rs`, `events.rs`,
  `tests/engine_selftests.rs`, `tests/support/mod.rs`): E10 adds `chat {wait:
  false}`, a `thread` expect family, the last-marker `label_of` and the
  `stop-session` wait. Waves 0, 2 and 3 add three expect forms, the VM mode and
  the TLS client. **Medium:** the same `Expect` struct, `families()` and
  `check` family list. Add new fields at the end and new engine functions in
  their own blocks; resolve when the bases take `main`.
- `crates/vam-orchestrator/src/bin/vamd.rs`: E10 adds `settle_dead_trees`
  before the golden warmup (~:358); waves 3 and 4 change the VMM factory
  (~:332) and the listener (~:410-425). **Low** (adjacent hunks).
- `crates/vam-orchestrator/src/manager.rs`: E10 changes create, terminate and
  keep-warm (+392 lines); wave 4 touches `adopted_vmm` and teardown. **Medium:**
  keep jailer work in `vam-core/src/vmm.rs` where possible.
- `crates/vam-orchestrator/src/api/mod.rs`: E10 +1332 lines, none in
  `serve` (~:210) or `get_node_status` (~:420-450), which waves 2 and 3 touch.
  **Low.**
- `crates/vam-orchestrator/src/lib.rs`, `Cargo.toml`, `e2e/README.md`: both add
  lines (modules, a dependency, README sections). **Low.**
- No E10 change in `crates/vam-core`, `daemon.rs`, `orchestrator.proto`,
  `crates/vam-cli/src/config.rs` or `cmd/node.rs`, `scripts/`, or any scenario
  or world piece this phase adds.
- The TLS default: every E10 process scenario keeps running because the runner
  speaks TLS (or passes `--insecure-dev`, open question 2); E10's
  `workspace-matches-conversation-after-vm-stop` joins the lane with E10's
  release, as above.

**Phase close** (2026-10-24, all waves merged into `feat/isolation-truth`)

- Scenarios: all six `active` and green. The five firecracker ones run on the
  lane on every VM-path PR, push to `main` and dispatch (owner, W2b: the lane is
  skipped on non-VM PRs); the plain-HTTP one runs in `scenarios (smoke)`. The 11
  process scenarios green with the TLS default. VM tests on the lane: 5
  unjailed + 3 jailed (vam-core 2, vamd 1) + the jailed restore staging test.
- Exit: `GET /v1/node` on the lane's default node reads `firecracker microVM,
  jailer on` with `apiTls: true` (`node-reports-the-isolation-it-runs`). The
  lane is still advisory: making it required is the owner's branch-protection
  call (open question 4).
- Scoreboard (lane, hosted ubuntu-latest): lane job 6-15 min (cold about 14,
  warm with `skip_vm_tests` about 6); cold boot to registered 165-246 ms
  (jailed 165-217 ms); session start with golden restore median 224 ms
  unjailed (W3b, n=15) and 172-233 ms jailed (W4b); golden bake soak 20/20
  (W3b) and 10/10 jailed (W4b). Benchmark: no line moved; the security-page
  launch proof moved (CI boots a VM; TLS and jailer on by default).
- Billed CI minutes for the phase: about 600 (W0-W4b, including about 75
  lost to the billing stop, a Docker Hub 502, runner surprises and the
  `vam-storage` EDQUOT reruns).
- Known issues carried: `vam-storage` `test_vam_timeline_survives_commit_cycle`
  EDQUOT intermittent on the `test` runner (root-disk pressure); restore SLO
  50 ms not met (about 180 ms left in materialize, reseed and broker setup).
- Follow-ups (not planned): per-VM jail uid; a root-only run dir for unjailed
  nodes; cgroup limits; dropping the soak from every lane run; rustls 0.21 /
  axum 0.6 upgrade (Phase 3).
- No Mac run needed: the lane replaces it for this phase's firecracker
  scenarios.

**Team checklist — Phase 1** (team instance, CLI + `curl`, about 10 minutes)

Setup: deploy `vamd` from `main` after the release, run as root on the KVM
host (the jailer needs root), with the operator certificate in
`VAMD_TLS_CERT` / `VAMD_TLS_KEY` and the Firecracker and jailer binaries in
`VAMD_FIRECRACKER_BIN` / `VAMD_JAILER_BIN`. The jail base `/srv/jailer`, the
kernel, the rootfs and the Drive9 root must be on one filesystem (vamd refuses
to start otherwise and says why). Point the CLI and the app at
`https://<host>:8080`.

1. `curl http://<host>:8080/v1/health`: no health answer.
2. `curl https://<host>:8080/v1/health`: healthy.
3. `vam node`: the isolation line reads `firecracker microVM, jailer on`.
4. In the app, chat with an agent and ask it to write a file: the reply comes
   back and the file is in the workspace.
5. Start a playbook with a gate, decide it: the run finishes.
6. Restart vamd without the certificate variables: the log names a
   self-signed certificate and its fingerprint; `curl -k https://…/v1/health`
   is healthy and plain `http://` still gets no answer.
7. Restart vamd with `VAMD_USE_JAILER=0`: `vam node` reads `firecracker
   microVM, jailer off`. Restore the setup.

Report: pass, or the step number with the error text or a screenshot.

### Phase 2 — Close the leaks

**Goal:** the known gaps are shut: no encoded or partial secret reaches the
guest, no host-tool result outgrows the vsock frame, the guest proxy never
listens beyond loopback, the API limits a flood and pages its lists, idle
sessions end, the daemon's request paths cannot panic, and no default path
points into an EC2 home directory.
**Depends on:** Phase 1 (done, on `main` since 2026-10-24)
**Plan:** approved 2026-10-26 (owner: all open-question defaults; W3 carries `skip-vm-lane`; the receipt `rateLimits` field stays (locked decision 2), W3 runs after H0's receipt changes land or holds; W0 on Sonnet medium with test-only fixture adapters accepted)
**Exit:** scenarios `secret-never-reaches-guest-encoded`,
`host-tool-output-fits-frame`, `guest-proxy-never-binds-wildcard` and
`api-rate-limits-a-flood` `active` and green (the first, second and fourth in
`scenarios (smoke)`, the third on the microVM lane). The design's
`receipt-isolation-line-matches-node-config` is covered by E12 and not added
(Scenario delta).

- [ ] Redaction of encoded and partial secret forms, with a test corpus
- [ ] Host-tool output capped to the vsock frame
- [ ] Guest egress proxy fails closed to loopback
- [ ] API rate limits and pagination defaults
- [ ] TTL cleanup for idle sessions (approvals already expire)
- [ ] No `expect`/`unwrap` in the daemon's request paths
- [ ] EC2 defaults replaced by required config

**What the code map found** (2026-10-26, `feat/isolation-truth` = `main` + tracker)

- *Redaction.* `redact_secrets` (`host_tools.rs:203-209`) is
  `text.replace(value, "***")` over the exact secret values; `redact_bytes`
  (`:212-216`) round-trips bytes through `String::from_utf8_lossy`, so a
  binary artifact is also corrupted. Base64, base64url, hex, percent-encoded,
  split or partial forms pass. Four host-tool paths each call it their own
  way: the generic adapters (`:691-720`, values from `gather_secret_values`
  `:758`), the hosted (HSP) path (`:3889`), MCP (`redact_vendor_text`
  `:2067-2081`, the only one that also redacts the JSON-escaped form, longest
  first) and MCP multi-part files (`:1946-1950`, only `text/*` and
  `application/json` parts are redacted; other parts pass raw). A second,
  separate redactor sits on the egress path: `vam-network/src/secrets.rs:418`
  `scrub_response` (called from `vam-network/src/broker.rs:329`) replaces raw
  secret bytes in vendor response headers and bodies with the same weakness.
  The production registry is `HostToolRegistry::with_test_adapters()`
  (`brokers.rs:596`), so `secret_echo` (`host_tools.rs:123-148`, prints a vault
  value; only redaction stands between it and the guest) is live on every
  node. `vam-security` is a dependency of both `vam-orchestrator` and
  `vam-network`, so one shared redactor can live there.
- *Frame.* `vam_vsock::MAX_PAYLOAD_SIZE` is 64 KiB (`vam-vsock/src/frame.rs:6`),
  enforced on read (`:69`) and write (`:92`). The generic host-tool path caps
  at the capability token's `max_output_bytes` (`host_tools.rs:493-497`,
  default 1 MiB), and the envelopes mint 1, 2 or 5 MiB
  (`security.rs:1577-1597`). The hosted path already caps at
  `HSP_FRAME_BUDGET_BYTES` = 60 KiB (`host_tools.rs:2722`, used `:891`,
  `:3130`); generic and MCP do not. An oversized reply makes
  `write_control_envelope` fail (`brokers.rs:343-355`) and the Port 52 loop
  `break`s (`:291-338`): the guest's control connection drops instead of the
  tool failing. The process guest uses the same frame codec over its socket,
  so the process layer shows it. A chunking pattern exists for checkpoints
  (`vam-runtime/src/pep.rs:21-22`, 32 KiB chunks, upload and fetch loops).
- *Guest proxy.* The wildcard fallback is only in the microVM build:
  `start_guest_proxy` (`vam-runtime/src/egress.rs:403-420`,
  `#[cfg(not(feature = "process-guest"))]`) retries `0.0.0.0:3128` when the
  loopback bind fails; PID 1 calls it with `127.0.0.1:3128`
  (`bin/guest_init/pid1.rs:529-539`, after a loopback bring-up whose failure is
  only logged). The process-guest entry `start_guest_proxy_with` (`:422-437`)
  has no fallback, `EgressEndpoint::per_guest` (`:356-381`) refuses
  non-loopback, and a unit test proves a busy port yields no wildcard
  (`:707-724`). `advertise_proxy_url` (`:391`) already exists for a
  non-default port. So **the process layer cannot show this leak** (agent-experience.md §V.5 q4: "E9's process-guest build already compiles the fallback out").
- *API.* No rate limiting anywhere on the HTTP API (only an MCP-client 429
  reader, `mcp_client.rs:472`, and a guest-stream token bucket,
  `guest_stream.rs:30`); `tower` 0.4 and `tower-http` 0.4 are in, no limiter
  crate. The router is one chain in `api/mod.rs` `router()` (`:85-208`), the
  bearer-auth layer last (`:204-207`); `serve` (`:210-221`) and
  `tls::serve_app` (`tls.rs:564`) use `into_make_service()` (no peer address).
  Lists with `pagination: None`: `list_sessions` (`:914-927`, store query has
  no `ORDER BY`, `session.rs:260`), `list_runs` (`:1608-1617`, newest first,
  `catalog.rs:2079`, and one `build_run_status` per run), `list_approvals`
  (`:1736-1749`, pending only, oldest first, `workflow.rs:1007`), plus keys,
  agents, policies, playbooks, skill packages, hosted brokers and user keys.
  `PaginationRequest` (`page_size` 1..1000, 0 = server default 50) and
  `PaginationResponse` (`next_page_token`) already exist (`common.proto:140-147`)
  and are already fields of those responses: paging is additive on the wire.
  Thread messages already page (`api/threads.rs:35-38`, `limit` plus cursor);
  run events take an `after` cursor with no cap (`api/mod.rs:1654-1663`, E10's
  outbox).
- *TTL.* The 60 s sweeper loop in `bin/vamd.rs:589-627` runs gate timeouts,
  `sweep_expired_approvals` (`manager.rs:492`), active-duration and keep-warm.
  Pending tool approvals already expire after `VAMD_TOOL_HITL_TIMEOUT_SECS`
  (default 3600, `allowed_models.rs:22`): the row goes `EXPIRED`, the session
  is terminated, the lock released, the receipt issued. Only rows with
  `expires_at` set expire (`workflow.rs:1450-1462`); gates without a timeout
  wait forever (by design: the P7 three-day gate). Run sessions end after
  keep-warm. **Gap:** nothing ends a session that has no run (a bare
  `POST /v1/sessions`) and nothing ever deletes session or approval rows (no
  `DELETE FROM sessions|approvals` outside tests); no idle or age TTL exists
  (`vam-subjects.md` §2 Fact 3).
- *Panics* (script over non-test code; `#[cfg(test)]` and
  `#[cfg(all(test, unix))]` modules excluded). `vam-orchestrator`: 23 sites
  (21 `expect`, 2 `unwrap`) plus 2 `panic!`. **Request paths, 14:** `hsp.rs`
  7× `expect("connected")` (`:184, :200, :216, :232, :248, :264, :280`, one per
  `try_register`, `try_invoke`, `try_list_tools`, `try_on_request`,
  `try_on_complete`, `try_on_response`, `try_on_paused`); guarded but fragile:
  `api/mod.rs:1315` (`inline_playbook.as_ref().unwrap()`), `:1828`
  (`request_id.unwrap()`), `security.rs:927`, `run_checkpoint.rs:817`,
  `hsp_registry.rs:105`, `api/composition.rs:276`; infallible:
  `signing.rs:166` (HMAC accepts any key length). **Startup, 11:**
  `bin/vamd.rs:212, :348, :416, :535, :540, :558, :671, :672` and the two
  `panic!` at `:344`, `:493`; `daemon.rs:231` (a constant parse). The `hsp.rs`
  `unwrap`s at `:369-414` are test code. Dependencies `vamd` links
  (approximate, for Phase 3's clippy gate, not this phase): `vam-inference`
  about 19 (`budget.rs` 13 `lock().unwrap()`, `broker.rs` 2 locks and 3
  infallible `encode`), `vam-telemetry` 9 regex compiles, `vam-network` and
  `vam-storage` mostly mutex locks and statics.
- *EC2 paths.* `daemon.rs:215-216` (`/home/ec2-user/bin/firecracker`,
  `/home/ec2-user/bin/jailer`, overridden by `VAMD_FIRECRACKER_BIN`,
  `VAMD_JAILER_BIN` `:268-273` or flags `:363-367`); `manager.rs:191` (seed
  examples `/home/ec2-user/vam/examples` after `./examples` and
  `CARGO_MANIFEST_DIR`); `golden.rs:105` (rootfs
  `/home/ec2-user/vam/target/rootfs.ext4` unless `VAM_ROOTFS_PATH`; the
  design's `golden.rs:84` is now the kernel search list `:77-95` under
  `/var/lib/vam/kernels`, a system path, not EC2); `scripts/build-rootfs.sh:56`
  (Bun cache); `scripts/start-vamd-lima.sh:83-97` symlinks into
  `/home/ec2-user` to satisfy those defaults; the vendored SDK's agent
  runtimes (`vam-firecracker/src/agent/rt_std.rs:103`, `rt_tokio.rs:88`,
  `rt_async_std.rs:86`, Phase 3 dependency pass). **Fail-open:** the jailer
  preflight checks that both binaries are executable files, not who owns
  them (`vam-core/src/jail.rs:211-224`), so a root `vamd` with
  `VAMD_JAILER_BIN` unset runs `/home/ec2-user/bin/jailer` as root if anyone
  who can write that home directory put a file there.
- *Runner.* World pieces are `org`, `agent`, `playbook` only
  (`vam-e2e/src/world.rs:7-11`): no secret, no skill with a host tool. No
  expect form says "this text is absent", bounds a message's size, or sends a
  burst. The runner polls at 100 ms backing off to 250 ms
  (`engine.rs:36`, `:546-575`), at most about 10 requests per second per key;
  one vamd per run; `listed_approvals` reads one page (`engine.rs:945-960`).
  `guest-proxy-never-binds-wildcard` is expressible with existing forms
  (`agent-with-shell`, a `run` call, `thread` contains); the other three need
  new forms (wave 0). The lane filter (`rust.yml:63`) matches all of
  `crates/vam-orchestrator/`, `crates/vam-e2e/`, `crates/vam-runtime/`,
  `crates/vam-vsock/`, `e2e/scenarios|world/`, `proto/` and `Cargo.lock`, so
  **every wave of this phase triggers the lane** unless it carries the
  `skip-vm-lane` label; file placement cannot avoid it.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `secret-never-reaches-guest-encoded` | new | process | `org-admin`, `api-token` (new `secret` piece), `skill-host-fixtures` (new `skill` piece), `agent-with-host-fixtures` (new) | A host tool whose output carries the secret raw, base64, base64url, hex, percent-encoded, JSON-escaped and split: the run succeeds and no form of the secret (nor any long run of its bytes) is in the thread, the run's events, the receipt or the workspace. Redaction is host code and the guest gets the same bytes on both layers, so process is honest. The exit; benchmark P6 |
| `host-tool-output-fits-frame` | new | process | `org-admin`, `skill-host-fixtures`, `agent-with-host-fixtures` | A host tool that returns 200 KB: the run succeeds, the tool result the model sees ends with the truncation note and is at most 64 KiB, and the next tool call in the run works. Same Port 52 codec on both layers. The exit; P9 (fail safely) |
| `guest-proxy-never-binds-wildcard` | new | firecracker | `org-admin`, `agent-with-shell` | On a microVM whose loopback proxy port is already taken at boot, the guest lists no listener on `0.0.0.0` or `[::]` port 3128 (read from `/proc/net/tcp*` by a `run` call), and the egress proxy is still reachable on loopback or its unix socket. Firecracker only: the process build has no fallback to remove (code map), so a process scenario would pass on today's base, which is wrong. The exit; locked decision 2 |
| `api-rate-limits-a-flood` | new | process | `org-admin`, `org-member` as `flooder` | 300 quick `GET /v1/node` with the flooder's key: some answer 429 with `Retry-After` and a reason header; the admin's key is answered 200 meanwhile; after `Retry-After` the flooder is answered again. Uses a second identity so no other scenario's key is ever throttled. The exit; locked decision 2 |
| `receipt-isolation-line-matches-node-config` | not added | — | — | Covered by `receipt-names-the-isolation-the-run-had` (firecracker, `active`: `receipt.isolation.summary` `firecracker microVM, jailer on`, `jailer: true`) and `receipt-separates-host-facts-from-model-text` (process, `active`: `process guest (no isolation)`), with `node-reports-the-isolation-it-runs` for the node. No gap found |

Unchanged and must stay green: every existing scenario. None guards
`host_tools.rs` or `egress.rs`. 17 guard `api/mod.rs`; rate limits and paging
must not trip them, and **no `then` changes**:

- Rate limits are per key. Defaults sit at least 3x above the runner's peak
  (about 10 requests per second per key; IT-2.4 measures `--smoke` and
  `--all` and records it). The runner does **not** retry a 429, so a default
  that is too tight for a busy client turns the smoke set red: the smoke set is
  the canary. The flood uses its own identity.
- Paging: at most 50 pending approvals per page; the runner follows
  `nextPageToken` (wave 0), so `--all` never misses one.
- `NodeIsolation` and `ReceiptIsolation` gain an additive `rateLimits` field;
  the `summary` string does not change (the two receipt scenarios and
  `node-reports-the-isolation-it-runs` match it exactly).
- The idle-session TTL default (24 h) never fires inside a run.

Red for the right reason on today's base (wave 0 confirms each):
`secret-never-reaches-guest-encoded` passes `run: succeeded` and fails at the
`secret-absent` step, class `product` (only the raw form is replaced by
`***`); `host-tool-output-fits-frame` fails at the `thread` step, class
`product` (the tool result is a transport error after the 200 KB reply broke
Port 52, not a truncated result; if the run itself fails, at `then.0`);
`api-rate-limits-a-flood` fails at the `flood` step, class `product` (0 of
300 answered 429); `guest-proxy-never-binds-wildcard` fails on the lane at the
`thread` step, class `product` (the advertised proxy does not answer: with
`127.0.0.1:3128` held the old `0.0.0.0:3128` fallback fails with EADDRINUSE
too, so no wildcard listener appears on the base), once the busy-port boot
flag exists (wave 1, below).

**Benchmark delta**

Lines to prove (`design/benchmark.md` §1, both present):

- **P6** "tasks completed through brokered connections with **0 secret bytes
  in guest output**" (`benchmark.md:94`; metric "secret bytes in guest output
  (must be 0)", `:139`, "from the redaction check"). Proven by
  `secret-never-reaches-guest-encoded` and IT-2.1's corpus tests over every
  host-tool path and the egress scrub. The runner's `secret-absent` check is
  the "secret-leak check on every run's transcript" the line names; B1's scorer
  can call the same function.
- **P9** "malicious-skill tasks that fail safely (100%)" (`:97`), task
  `hostile-skill` (`:119`: read a secret, call an undeclared tool, exfiltrate;
  scheduled in B1, `:176`). This phase proves two of its three legs at the
  scenario layer: a skill that tries to push a secret out in encoded form gets
  nothing (`secret-never-reaches-guest-encoded`), a skill that floods its
  result fails safely instead of dropping the control channel
  (`host-tool-output-fits-frame`), and the guest proxy cannot be reached from
  off-guest (`guest-proxy-never-binds-wildcard`). "Call an undeclared tool" is
  already refused by the host catalog (`host_tools.rs:460-475`) and is not this
  phase's.

No baseline run and no paid endpoint: both are must-be-zero / must-be-100%
safety lines, decided by host code, not by a model; the mock model drives every
scenario deterministically, and the `hostile-skill` task does not exist until
B1. Baseline on this base, from the code: any adapter or vendor that echoes a
credential encoded puts nonzero secret bytes in guest output (P6 > 0); a skill
returning more than 64 KiB breaks Port 52 instead of failing safely. The phase
close records after: the four scenarios green, the corpus size and forms
covered, the frame cap on every path. Launch proof also moved: **the security
page** (no secret in the guest, loopback-only proxy, rate limits on).

**Wave 0 "scenarios"** (Sonnet medium: runner plus one orchestrator fixture file)

Writes `secret-never-reaches-guest-encoded`, `host-tool-output-fits-frame` and
`api-rate-limits-a-flood` as `pending` with `expectFail` at the steps above,
and the world pieces `api-token`, `skill-host-fixtures`,
`agent-with-host-fixtures`. Runner forms (`crates/vam-e2e`: `world.rs`,
`scenario.rs`, `engine.rs`, `check.rs`, `api.rs`, self-tests, `e2e/README.md`):

1. World kinds `secret` (puts an org secret with a runner-made random value,
   kept like an identity's key and never printed) and `skill` (`PUT
   /v1/skills` with the `{{name}}` URN; an agent piece names it in
   `skillRefs`). Wave 0 first confirms that a `PUT /v1/skills` spec with a
   `hostExec` tool reaches the session's catalog; if only a published package
   does, the piece publishes a package instead.
2. `expect: { secret-absent: { secret: <piece> } }`: the runner computes every
   form of the value (raw; base64 and base64url, padded and not, at the three
   byte offsets; hex lower and upper; percent-encoded; JSON-escaped; any run of
   12 or more of its bytes) and searches the latest run's thread messages, run
   events, receipt and model appendix, and workspace files. A hit names the
   form and the place, never the bytes.
3. `thread` gains an optional `maxBytes`: the matched message's text is at most
   that size.
4. Step `flood: { as, path, count }` (GET, sequential, keep-alive) and
   `expect: { flood: { rateLimited: { atLeast: N }, retryAfter: present,
   recovers: true } }` (`recovers`: after sleeping `Retry-After`, one request
   is 2xx). Counts by status go in the brief.
5. `listed_approvals` follows `pagination.nextPageToken` (no-op today).
6. **Fixture adapters** (the exception to "harness only": no runner form can
   make a host tool emit an encoded secret or 200 KB): a new
   `crates/vam-orchestrator/src/host_tools_fixtures.rs` with `secret_forms`
   (the first required secret in the forms its `config.forms` lists) and
   `bulk_output` (`config.bytes` of text), registered only on a node started
   with `VAMD_E2E_MOCK_SCRIPTS` (the switch that already gates `mock-model`),
   one registration line where the registry is built (`brokers.rs:596`). Unit
   test: an ordinary node has neither adapter.

Proven red: `scripts/e2e/run.sh --scenarios
secret-never-reaches-guest-encoded,host-tool-output-fits-frame,api-rate-limits-a-flood`
fails each at its step, class `product`; `--smoke` otherwise unchanged; `vam-e2e
check` clean. `guest-proxy-never-binds-wildcard` is **not** written in wave 0:
it can be red only once the lane can occupy the guest's loopback port at boot
(guest code), and a pending firecracker scenario that passes would turn the lane
red for the wrong reason on every later VM-path PR. Wave 1 writes it as its
first push.

**Items**

- [x] **IT-2.1 Redaction of encoded and partial forms.** One redactor in a new
  `crates/vam-security/src/secret_forms.rs`: given the secret values, it
  removes every form listed in wave 0 item 2 (longest first, overlapping runs
  merged), works on bytes (no lossy UTF-8 round trip, so binary artifacts stay
  intact apart from the redacted runs), and leaves secrets shorter than the
  minimum length (open question 4) to whole-value replacement. Used by all
  four host-tool paths (generic, hosted, MCP text and every MCP part regardless
  of MIME) and by `vam-network` `scrub_response`; `redact_vendor_text`,
  `redact_secrets` and `redact_bytes` become thin calls to it.
  `secret_echo` is registered only on a test node (open question 6). A
  corpus file of cases (each form, a secret inside a larger base64 blob such
  as `user:secret` Basic auth, split across a line break, inside JSON, 4-byte
  secret, binary artifact) drives one table test per path. *Files:*
  `crates/vam-security/src/secret_forms.rs` (new), `lib.rs`;
  `crates/vam-orchestrator/src/host_tools.rs` (`:203-216`, `:691-720`,
  `:1946-1950`, `:2067-2081`, `:3889`, registry `:167-179`);
  `crates/vam-network/src/secrets.rs` (`:418-460`); corpus under
  `crates/vam-security/tests/`. *Accept:* every corpus case redacted on every
  path; no plain-text regression in the existing redaction tests;
  `secret-never-reaches-guest-encoded` green. *Test:* `cargo test -p
  vam-security`, `cargo test -p vam-network --lib`, `cargo test -p
  vam-orchestrator --lib host_tools`, `scripts/e2e/run.sh --scenarios
  secret-never-reaches-guest-encoded`, `--smoke`. *Turns green:*
  `secret-never-reaches-guest-encoded`.
  `Decide: NOW — security floor (secret path), proof 3, cost 1, line P6 secret bytes in guest output = 0; P9 hostile-skill`
- [x] **IT-2.2 Host-tool output fits the frame.** One budget for everything a
  host-tool reply carries (text, artifact, extra artifacts), derived from
  `vam_vsock::MAX_PAYLOAD_SIZE` minus envelope room (today's 60 KiB
  `HSP_FRAME_BUDGET_BYTES`, moved to one shared constant), applied after
  redaction on the generic and MCP paths as on the hosted path, with the
  existing "[truncated: kept N of M bytes]" notes; `min(envelope, budget)`, so
  a smaller envelope still wins. Defence in depth in `brokers.rs`: a reply that
  still does not fit becomes an error envelope ("result exceeds the frame")
  and the connection stays open. Chunked delivery of larger artifacts is not
  built (open question 7). *Files:* `host_tools.rs` (`:493-497`, `:699-720`,
  MCP `:620-660`, `:2722`), `brokers.rs` (`:291-355`), unit tests. *Accept:*
  a 5 MiB envelope and a 1 MiB adapter result give one reply under the frame
  with the note; an oversize envelope never closes Port 52 (test with a duplex
  stream as `brokers.rs:1543`); `host-tool-output-fits-frame` green. *Test:*
  `cargo test -p vam-orchestrator --lib host_tools brokers`, `cargo test -p
  vam-vsock`, `scripts/e2e/run.sh --scenarios host-tool-output-fits-frame`,
  `--smoke`. *Turns green:* `host-tool-output-fits-frame`.
  `Decide: NOW — security floor (any tool result over 64 KiB drops the guest's control channel), proof 3, cost 1, line P9 hostile-skill (fails safely)`
- [x] **IT-2.3 Guest proxy fails closed to loopback.** `start_guest_proxy`
  (microVM) never binds a non-loopback address: it tries `127.0.0.1:3128`, then
  `127.0.0.1:0` and advertises the port it got (`advertise_proxy_url`), and
  always serves the unix socket; with no loopback at all, the unix socket only.
  The VM entry delegates to `start_guest_proxy_with` so there is one bind path.
  Test hook for the lane (open question 8): a kernel command-line flag the host
  adds only on a node started with `VAMD_E2E_MOCK_SCRIPTS`; PID 1 then holds
  `127.0.0.1:3128` before the proxy starts. The wave writes
  `guest-proxy-never-binds-wildcard` first (flag plus scenario, red on the lane
  at the `thread` step), then the fix. Unit test: the VM entry with a busy
  loopback port binds no wildcard (the `:707-724` test, for the VM build).
  *Files:* `crates/vam-runtime/src/egress.rs` (`:391-437`),
  `bin/guest_init/pid1.rs` (`:529-539`), the host command line
  (`crates/vam-core/src/vmm.rs` boot args or `golden.rs`, whichever builds
  them for cold and bake), `e2e/scenarios/guest-proxy-never-binds-wildcard.yaml`.
  *Accept:* the scenario green on the lane, every other lane scenario green
  with the flag on (tools reach the proxy on the advertised port); the
  wildcard string `0.0.0.0:3128` gone from `vam-runtime`. *Test:* `cargo test
  -p vam-runtime`, `cargo run -p vam-e2e -- check`, the lane. *Turns green:*
  `guest-proxy-never-binds-wildcard`. **Deploy:** rootfs rebuild (below).
  `Decide: NOW — security floor (proxy exposed on the guest's TAP), locked decision 2 made true, proof 3, cost 1, line P9 hostile-skill (exfiltrate)`
- [x] **IT-2.4 API rate limits.** A new `crates/vam-orchestrator/src/api/rate_limit.rs`:
  in-memory token buckets, no new crate. Inner layer, after auth: one bucket
  per API key (all requests) and one per key for `POST /v1/runs` and
  `POST /v1/sessions`. Outer layer: one bucket per peer IP charged only by
  requests without a valid key (login, bad keys), so a whole CI suite from
  `127.0.0.1` with valid keys never shares it. `/v1/health` exempt. A refusal
  is 429 with the API error JSON (`RESOURCE_EXHAUSTED`), `Retry-After`
  (seconds) and `X-VAM-Rate-Limited-Reason` (`key-rate`, `key-create-rate`,
  `ip-auth-rate`; vam-subjects.md §8). Config in `daemon.rs`
  (`VAMD_RATE_LIMIT_RPS`, `_BURST`, `_CREATE_RPS`, `_CREATE_BURST`); off only
  with `VAMD_RATE_LIMIT=off`, logged WARN and reported: additive
  `rate_limits` on `NodeIsolation` (field 5) and `ReceiptIsolation` (field 5),
  `summary` unchanged (locked decisions 2 and 3). In `api/mod.rs`: two
  `.layer(...)` lines in `router()` and `into_make_service_with_connect_info`
  in `serve` (and `tls::serve_app`); no handler edits. *Files:*
  `api/rate_limit.rs` (new), `api/mod.rs` (`router` `:85-208`, `serve`
  `:210-221`), `tls.rs` (`:564`), `daemon.rs`, `bin/vamd.rs` (pass config),
  `isolation.rs`, `receipt.rs`, `proto/vam/v1/orchestrator.proto`,
  `receipt.proto`, `docs/api/openapi.yaml`. *Accept:* `api-rate-limits-a-flood`
  green; `--smoke` green with defaults (runner peak recorded in the PR);
  unit tests for refill, per-key isolation, the IP bucket ignoring valid keys,
  the opt-out reported. *Test:* `cargo test -p vam-orchestrator --lib`,
  `cargo test -p vam-spec`, `scripts/e2e/run.sh --scenarios
  api-rate-limits-a-flood`, `--smoke`. *Turns green:* `api-rate-limits-a-flood`.
  `Decide: SLICE — proof 3 (locked decision 2, security page), cost 2 (middleware and an additive proto field) → slice: per-key and per-IP buckets with 429 and the node/receipt report, one wave; per-App concurrency, spend ceilings and per-Subject limits (vam-subjects §7.5, §8) backlog, trigger "Apps land"`
- [x] **IT-2.5 Pagination defaults.** `GET /v1/runs`, `/v1/sessions` and
  `/v1/approvals` take `pageSize` (default 50, max 1000, as `common.proto:141`
  says) and `pageToken` (opaque cursor over the store's row order, not an
  offset) and return `pagination.nextPageToken`; sessions get a stable
  newest-first order; runs build status only for the page. The CLI's list
  commands print the first page and a hint, `--all` follows the token. Other
  lists unchanged (open question 2). *Files:* `api/mod.rs` (the three
  handlers only), `catalog.rs:2079`, `session.rs:260`, `workflow.rs:1007`
  (limit and cursor queries), `crates/vam-cli/src/cmd/{run,session,approve}.rs`,
  `docs/api/openapi.yaml`. *Accept:* unit tests for default, max, cursor
  across inserts, empty last page; the process scenarios unchanged and green.
  *Test:* `cargo test -p vam-orchestrator --lib`, `cargo test -p vam-cli`,
  `--smoke`. *Turns green:* none (rides with IT-2.4).
  `Decide: NOW — proof 2 (confirmed defect: unbounded lists, vam-subjects §2), cost 1 (proto fields exist), line security page (rate limits and paging on)`
- [x] **IT-2.6 Idle-session TTL.** A non-terminal session with no run and no
  activity for `VAMD_SESSION_IDLE_TTL_SECS` (default 86400) is terminated and
  its workspace lock released, by a new sweep in a new
  `crates/vam-orchestrator/src/ttl.rs`, called once from the existing 60 s loop
  in `bin/vamd.rs` (`:589-627`). Pending approvals keep today's expiry (open
  question 3); no row purge. Wave plan first confirms that a bare session lives
  until terminated. *Files:* `ttl.rs` (new), `lib.rs`, `bin/vamd.rs` (one
  call), `daemon.rs` (config), a store query in `session.rs`. *Accept:* unit
  tests with an injected clock: idle bare session ended, a session with a live
  or parked run untouched, a second pass returns 0. *Test:* `cargo test -p
  vam-orchestrator --lib ttl`. *Turns green:* none (no exit scenario).
  `Decide: NOW — proof 2 (confirmed defect: a bare session's VM lives forever, vam-subjects §2 Fact 3), cost 1, line security page`
- [x] **IT-2.7 No panics in the daemon's request paths.** The 14 request-path
  sites above become typed errors (`hsp.rs`: a `NotConnected` error from one
  helper instead of seven `expect("connected")`); the guarded ones are
  restructured with `if let`/`let else`. The 11 startup sites exit with a
  one-line error naming what failed (not a panic backtrace).
  `#![cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))]` on
  the `vam-orchestrator` lib and `vamd` bin, so Phase 3's clippy gate keeps
  them out. *Files:* `hsp.rs` (`:175-290`), `api/mod.rs` (`:1315`, `:1828`),
  `security.rs:927`, `run_checkpoint.rs:817`, `hsp_registry.rs:105`,
  `api/composition.rs:276`, `signing.rs:166`, `bin/vamd.rs`, `daemon.rs:231`,
  `lib.rs`. *Accept:* the inventory script (in the PR) reports 0 non-test
  `expect`/`unwrap`/`panic!` in `vam-orchestrator`; a unit test calls each
  `hsp.rs` method on an unconnected client and gets the error. *Test:* `cargo
  test -p vam-orchestrator --lib hsp`, `cargo clippy -p vam-orchestrator` for
  the two lints (local, not gated). *Turns green:* none.
  `Decide: NOW — security floor (a panic in the daemon), proof 3, cost 1, line security page (locked decision 6)`
- [x] **IT-2.8 EC2 defaults replaced by required config.** A VM node requires
  `VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH` and, with the jailer on,
  `VAMD_JAILER_BIN` (existing names, no default); a missing one stops `vamd`
  at start with a message naming each variable (locked decision 2 style).
  Process-guest nodes need none. The jailer preflight also refuses a binary
  not owned by root or writable by group or others. Seed examples come from
  `VAMD_EXAMPLES_DIR` when set; no home-directory fallback (unset: `./examples`
  and the dev-tree path as today, else no seeds). `build-rootfs.sh` caches Bun
  under `${XDG_CACHE_HOME:-$HOME/.cache}`; `start-vamd-lima.sh` exports the
  variables instead of symlinking into `/home/ec2-user`. The kernel search
  list under `/var/lib/vam/kernels` stays. *Files:* `daemon.rs` (`:215-216`,
  parse and a `validate`), `bin/vamd.rs` (refusal), `manager.rs:191` (one
  line), `golden.rs:105`, `crates/vam-core/src/jail.rs` (`:211-224`),
  `scripts/build-rootfs.sh:56`, `scripts/start-vamd-lima.sh`, docs pages that
  name the defaults. *Accept:* `rg /home/ec2-user crates scripts` finds only
  the vendored SDK; unit tests for each missing variable, a process node
  starting with none, the ownership refusal; the lane green (it already sets
  the variables). *Test:* `cargo test -p vam-orchestrator --lib daemon`,
  `cargo test -p vam-core jail`, `bash -n` on the scripts, the lane.
  *Turns green:* none.
  `Decide: NOW — security floor (a root vamd runs the jailer from a user's home directory when unset; preflight checks only the exec bit), proof 3, cost 1, line security page`

**Waves**

Each wave: one session in its own worktree of vam, base `feat/isolation-truth`,
branch `it-2/wave-N`, plan-only first turn, draft PR titled `IT-2 WN: …`,
marked ready after local checks. No local `cargo test --workspace`. Known local
failure not to chase: `hsp_uds::uds_socket_acl_deny_fails_fast` as root. Every
wave's files match the lane filter (`rust.yml:63`), so the lane runs unless the
PR carries `skip-vm-lane` (set while draft, before ready).

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Collisions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios (+ runner forms, fixture adapters) | — (red, right reason: three process scenarios) | vam | Sonnet medium: runner plus one orchestrator file (two crates) | 90 min | — | H0: `vam-e2e` `scenario.rs`, `engine.rs`, `check.rs` (H0 W0 and W4 add the `model` block, `ChatFull` fields, `--bench`). **Medium**: new fields at the end of `Expect`/`Step`, own engine functions, rebase on whichever lands first. `brokers.rs:596` one line: **Low**. Lane: `skip-vm-lane` (no firecracker scenario touched) | [x] | — |
| 1 | IT-2.3 | `guest-proxy-never-binds-wildcard` | vam | Sonnet high: egress boundary, guest code, debugged only through lane runs | 150 min | 0 | H0 owns `vam-runtime/src/agent.rs`, `primitives.rs`, `compaction.rs`; this wave edits `egress.rs`, `guest_init/pid1.rs` and the host boot args only. **Low**. E10 Phase 3: none. **The only wave whose scenario needs the lane** (runs it) | [x] | — |
| 2 | IT-2.1, IT-2.2 | `secret-never-reaches-guest-encoded`, `host-tool-output-fits-frame` | vam | Sonnet high: secrets and the vsock boundary | 150 min | 1 | `host_tools.rs` and `brokers.rs`: no H0 change; E10 Phase 3 may add `post_message` to the Port 52 dispatch (`brokers.rs`), this wave edits `write_control_envelope` and the serve loop only. **Low**. `vam-network`, `vam-security`: no other lane. Lane: `skip-vm-lane` allowed (host-only; the same code serves process and VM guests) | [x] | — |
| 3 | IT-2.4, IT-2.5 | `api-rate-limits-a-flood` | vam | Sonnet medium: new middleware, an additive proto field, CLI | 150 min | 2 | `api/mod.rs`: E10 Phase 3 will own the inbox and outbox paths there; this wave adds two lines in `router()`, the connect-info line in `serve` and edits three list handlers. **Medium**. `workflow.rs` approvals query vs E10 Phase 3 Stop: **Medium**. `receipt.proto`/`receipt.rs`: H0 W1 adds a receipt field; take the next free number at rebase. **Medium**. **HOLD: shared file `api/mod.rs`** if an E10 Phase 3 build wave is open in it when W3 is due (conflict rule, launch-0.1.0.md §4); else go. Lane: `skip-vm-lane` (owner 2026-10-26: API-only wave). Runs after H0's `receipt.proto`/`receipt.rs` changes land, else holds | [x] | — |
| 4 | IT-2.6, IT-2.7, IT-2.8 | — (no exit scenario; lane and smoke stay green) | vam | Sonnet medium: daemon config that fails closed at start, a new sweeper, `hsp.rs`; rerun at high if it fails twice (sweeper is reconciliation logic) | 120 min | 3 | `manager.rs:191` one line, `api/mod.rs:1315`, `:1828` two lines (E10 Phase 3): **Low**. `bin/vamd.rs` sweeper loop and startup (H0: none): **Low**. `golden.rs`, `daemon.rs`, `jail.rs`: no other lane. Lane: runs (VM node config and `build-rootfs.sh`; the asset cache key hashes that script, so expect one cold lane run, about 14 min) | [x] | — |

W1 goes right after W0 because it is the only lane-dependent wave and the only
guest change; W2 before W3 so the frame and redaction land before the API
work; W4 last because its config change needs a deploy step and should be
proven on the lane after everything else. Wave decision lines: W0
`Decide: NOW — proof 3, cost 1, line P6 and the security page`; W1, W2, W4 as
their items; W3 `Decide: SLICE` as IT-2.4 (IT-2.5 rides).

**Open questions** (defaults in bold)

1. Rate limits: **per API key, 20 requests/s, burst 100; creation (`POST
   /v1/runs`, `/v1/sessions`) 2/s, burst 20 per key; per peer IP 5/s, burst
   20, charged only by requests without a valid key; `/v1/health` exempt;
   in-memory, per node, reset on restart.** 429 body: **the API error JSON
   (`RESOURCE_EXHAUSTED`), `Retry-After` in seconds,
   `X-VAM-Rate-Limited-Reason`.** Off only by `VAMD_RATE_LIMIT=off`, reported on
   node and receipt. IT-2.4 checks the defaults sit 3x over the runner's
   measured peak and adjusts within that rule.
2. Pagination: **runs, sessions and pending approvals only; default 50, max
   1000; opaque cursor, not offset; runs and sessions newest first, approvals
   oldest first (as today).** The CLI prints the first page with a hint and
   `--all` follows; the runner follows; **The VAM app unchanged this phase** (it
   shows the newest 50 runs; following pages is an app follow-up). Catalog
   lists, keys, users, secrets and brokers stay unpaged (bounded by operator
   action); run events keep their `after` cursor (E10 outbox).
3. TTL: **idle bare sessions 24 h (`VAMD_SESSION_IDLE_TTL_SECS`); pending tool
   approvals keep the 1 h expiry (`VAMD_TOOL_HITL_TIMEOUT_SECS`) and expire to
   `EXPIRED`, not denied (the receipt says expired); gates without a deadline
   never expire (P7); no row purge this phase** (receipts and evidence
   reference the rows; retention is the vam-subjects 30-day rule, later).
4. Minimum partial-secret length: **12 bytes**: any run of 12 or more
   consecutive secret bytes is redacted; a secret shorter than 12 bytes is
   redacted only whole and in its encoded forms.
5. Where redaction applies: **every guest-bound byte of a host-tool reply
   (text, artifact, extra artifacts; generic, hosted, MCP) and egress responses
   (`scrub_response`)**. The model sees only what the guest gets, so
   model-visible and guest-bound are the same bytes; host audit logs and
   telemetry keep their own redactors.
6. `secret_echo`: **registered only on a test node (mock scripts on)**; the
   docs row moves to the testing section. `echo`, `fail`, `artifact` stay.
7. Frame: **cap every path to one budget (60 KiB) with the truncation note; an
   oversize reply becomes an error envelope, never a dropped connection.**
   Chunked delivery of larger artifacts (a fetch loop like the checkpoint one,
   guest code): `Decide: BACKLOG — proof 1, cost 3 (new guest path), trigger "a real adapter needs artifacts over 60 KiB"`.
8. Guest proxy: **busy or unavailable `127.0.0.1:3128` → `127.0.0.1:0`,
   advertised; unix socket always; never non-loopback.** The lane's busy-port
   flag is a host-set kernel argument honoured by PID 1, added only on mock
   nodes; it only removes a loopback listener, so it weakens nothing.
9. Config for the EC2 paths: **existing names, required on VM nodes
   (`VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH`, `VAMD_JAILER_BIN` with the jailer
   on); new optional `VAMD_EXAMPLES_DIR`**; the vendored SDK's `FIRECRACKER`
   defaults go with Phase 3's dependency pass.
10. Panics: **all non-test code in `vam-orchestrator` (request paths fixed,
    startup made into clear error exits) plus the deny lints**; dependency
    crates inventoried for Phase 3's clippy gate, not changed here.
11. Lane minutes: **W0 and W2 carry `skip-vm-lane`; W1, W3 and W4 run the
    lane.** About 3 to 5 lane runs for the phase (W1 iterates on it).
12. Wave 0 touches product code (the fixture adapters, test nodes only):
    **accepted**, since no runner form can make a host tool emit an encoded
    secret or 200 KB.

**Coordination**

- *H0 (harness, `feat/harness`, 5 commits ahead of `main`, docs only today;
  build waves `h-0/wave-N`).* Its plan touches `vam-e2e` `scenario.rs`
  (W0, W4), `engine.rs` and `report.rs` (W4), `vam-runtime` `agent.rs`,
  `primitives.rs`, `compaction.rs`, `vam-orchestrator` `run_context.rs`,
  `receipt.rs`, `hitl.rs`, `events.rs`, `proto/vam/v1/workflow.proto` and
  `receipt.proto`. Overlap: our W0 (runner, Medium), W3 (`receipt.proto` and
  `receipt.rs`, Medium: additive fields, numbers at rebase). No overlap with
  W1's guest files.
- *E10 Phase 3 (product lane, `feat/agent-runtime`, 0 ahead and 25 behind
  `main`; breakdown not drafted).* It will own the inbox and outbox paths in
  `api/mod.rs` and `agent.rs`, likely `brokers.rs` (Port 52 `post_message`)
  and `workflow.rs` (Stop). Overlap: W3 (`api/mod.rs`, `workflow.rs`: Medium,
  conditional HOLD), W2 (`brokers.rs`: Low), W4 (one line each in
  `manager.rs` and `api/mod.rs`: Low). Its build waves wait for H0, so W0 to W2
  should land before they start.
- *E12* (run receipt) is on `main`; its Phase 2 waits for the E5 slice. W3's
  `ReceiptIsolation.rate_limits` is additive.
- No change in this phase to any existing scenario's `then`, to the TLS or
  jailer defaults, or to the isolation `summary` string.

**Deploy note fragments** (for the release PR)

- *Rootfs rebuild and vamd restart (W1).* The guest proxy change is in
  `vam-runtime`: rebuild the rootfs from the release tree
  (`scripts/build-rootfs.sh`), point `VAM_ROOTFS_PATH` at it, restart `vamd`.
  The golden snapshot re-bakes on its own (its key includes the rootfs).
  CI's lane builds the rootfs from the PR, so no CI step is needed.
- *Required config on VM nodes (W4). Set before restarting:*
  `VAMD_FIRECRACKER_BIN`, `VAMD_JAILER_BIN` (jailer on), `VAM_ROOTFS_PATH`;
  the binaries must be root-owned and not group- or world-writable. A node
  missing one **refuses to start and names each variable** in its log and
  exit message: it never falls back silently. The team instance (EC2, likely
  relying on the `/home/ec2-user` defaults today) must add them to its unit
  file in the same change, and `VAMD_EXAMPLES_DIR` if it relies on seed
  examples.
- *Rate limits on (W3).* Normal app and CLI use stays under the defaults;
  tune with `VAMD_RATE_LIMIT_RPS`/`_BURST`/`_CREATE_RPS`/`_CREATE_BURST`;
  `VAMD_RATE_LIMIT=off` is explicit, logged and shown as `rateLimits: false`
  on `vam node` and on every receipt.
- *Lists page (W3).* `vam run ls` and the app show the first 50; the CLI
  prints how to get the rest.
- *Idle sessions end after 24 h (W4)*, `VAMD_SESSION_IDLE_TTL_SECS` to change.

**Phase close** (2026-10-27, all waves merged into `feat/isolation-truth`)

- Scenarios: the four exit scenarios `active` and green in CI:
  `secret-never-reaches-guest-encoded`, `host-tool-output-fits-frame`,
  `api-rate-limits-a-flood` (process, `scenarios (smoke)`) and
  `guest-proxy-never-binds-wildcard` (firecracker, lane). The six Phase 1
  scenarios and E12's receipt scenarios stay green; `--smoke` 31/31, lane 8/8
  firecracker scenarios on W4's run. `receipt-isolation-line-matches-node-config`
  was not added: E12's `receipt-names-the-isolation-the-run-had` (firecracker)
  and `receipt-separates-host-facts-from-model-text` (process) cover it.
- Exit met: all four leak scenarios green; the lane's node still reports
  `firecracker microVM, jailer on` (`node-reports-the-isolation-it-runs`, W4 lane run).
- **Benchmark lines (§11, recorded, no paid runs):**
  - P6 "0 secret bytes in guest output": before W2, a base64 form of the bound
    secret reached the guest's thread (W0 red at then.1); after W2, 0 of 9 forms
    (raw, base64 and base64url at three alignments, hex, percent, JSON-escaped,
    split, Basic, skewed) and no 12-byte raw run anywhere the guest or the
    receipt can read, on the mock world. Stated limits (backlog): encoded
    partial runs, reversed/XOR/double-encoded, compressed bodies; egress bodies
    over the proxy's 8 MiB scrub buffer stream unscrubbed.
  - P9 `hostile-skill`: no task exists until B1, so the line is not scored. What
    this phase makes true for it: a tool result over the frame no longer drops
    the guest's control channel (error envelope, Port 52 stays open); the guest
    proxy never listens beyond loopback; `secret_echo` is gone from production
    nodes; the API refuses a flood. B1 scores the line on these.
- Scoreboard: 4 scenarios turned green over 5 build waves (W0-W4) plus one
  base sync; 0 fix cycles; lane runs 4 (W1 x2, W4), all green on
  the final pushes; wall time about 17 h elapsed, about 6 h of wave work.
- Owner decisions during the phase: W3 skips the lane; receipt `rateLimits`
  recorded per run; per-key default 30/s (measured runner peak 33 req/s);
  commit signing optional while the container's signing service is down
  (per-command `-c commit.gpgsign=false`); relay format (playbook §3 step 4).
- Known issues and follow-ups (not planned):
  - H0: `vam-runtime/src/instruction.rs:289` tells JS skills the proxy is at
    `127.0.0.1:3128`, wrong after the loopback fallback.
  - The busy-port test flag covers the golden bake only, not cold boot
    (`manager.rs`, owner).
  - Idle TTL: a session with any run that never got a receipt (runs older
    than E12) never counts as idle.
  - The VAM app does not follow `nextPageToken`: past 50 it sees only the newest
    runs and sessions and the oldest 50 pending approvals.
  - With no TCP listener, the guest's `proxy_url()` still defaults to
    `127.0.0.1:3128` (guest-internal only).
  - Local-only test failure as root: `hsp_uds::uds_socket_acl_deny_fails_fast`.
- No Mac run needed: the lane ran the phase's firecracker scenario.

**Team checklist — Phase 2** (team instance, CLI + `curl`, about 10 minutes)

Setup: rebuild the rootfs from `main` after the release
(`scripts/build-rootfs.sh`; the guest proxy changed). Set
`VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH` and `VAMD_JAILER_BIN` (vamd no longer
falls back to `/home/ec2-user`); the Firecracker and jailer binaries must be
owned by root and not writable by group or others (`chown root:root`,
`chmod go-w`). Optional: `VAMD_EXAMPLES_DIR` (unset: no example agents),
`VAMD_SESSION_IDLE_TTL_SECS` (default 86400). Restart `vamd` as root. `$KEY` is
an API key, `$H` is `https://<host>:8080`.

1. Start `vamd` once without `VAM_ROOTFS_PATH`: it refuses to start and names
   `VAM_ROOTFS_PATH`. Set it back and start again.
2. `vam node` (or `curl $H/v1/node -H "Authorization: Bearer $KEY"`): isolation
   reads `firecracker microVM, jailer on`; rate limits read
   `on (30/s burst 100, runs 2/s burst 20)`.
3. Chat with any agent, then `vam receipt show <run id>`: the isolation line
   reads `firecracker microVM, jailer on` and shows the rate limits on.
4. Ask an agent with a shell tool to run `cat /proc/net/tcp /proc/net/tcp6`:
   no line has local address `00000000:0C38` (0.0.0.0:3128) or the `::` form.
5. `for i in $(seq 300); do curl -s -o /dev/null -w '%{http_code}\n' -H "Authorization: Bearer $KEY" $H/v1/node; done | sort | uniq -c`:
   some `429`; wait 5 seconds, one more request answers `200`.
6. Send 30 requests with a wrong key (`Bearer nope`): the first are `401`, then
   `429` with header `X-VAM-Rate-Limited-Reason: ip-auth-rate`; a request with
   the real key from the same machine still answers `200`.
7. `vam run ls`: at most 50 runs and a hint on stderr when there are more;
   `vam run ls --all` lists them all.
8. Set `VAMD_SESSION_IDLE_TTL_SECS=120`, restart, start a chat and leave it;
   after about 3 minutes `vam session ls` shows that session terminated. Remove
   the variable and restart.

Report: pass, or the step number with the error text or a screenshot.

### Phase 3 — Honest docs and clean build

**Goal:** what the docs say is what a default node does. Every docs page
says whether it is `shipped`, `planned` or `stale`; every page that makes an
isolation claim names the scenario or test that proves it, and `vam-e2e check`
fails when one does not. The eBPF stub and its claims are gone, the 21 build
pages sit behind a planned banner, clippy is clean and gated in CI, and the
dependencies have a written report.
**Depends on:** Phases 1, 2 (both on `main`; base synced with `main`, 2026-10-28)
**Plan:** approved 2026-10-28 (owner: all open-question defaults, including Q3 keep 11 build pages behind the planned banner and move 10 to `design/vam-build/`, and Q5 fold `network/ebpf.md` into the overview, delete it, redirect; W0 stays Sonnet medium)
**Exit:** `docs_claim_without_proof_fails_check` green (the lint fails a
fixture page that claims an unproven isolation property) and `vam-e2e check`
clean on the base with no ratchet file left; the three new scenarios and the
changed jailer scenario `active` and green (the process one in `scenarios
(smoke)`, the others on the microVM lane); `rg -i ebpf crates proto AGENTS.md
docs/src docs/astro.config.mjs` empty; the `clippy` job green at `-D warnings`
and required.

- [ ] Docs status field and the docs lint in `vam-e2e check`
- [ ] eBPF stub and every docs claim removed (decided 2026-10-28)
- [ ] Selling pages rewritten to what the node does, each claim naming its scenario
- [ ] Honest page ("What VAM is not for")
- [ ] `docs/build/` behind a planned banner and sidebar group
- [ ] Status field set honestly on every remaining page; hand-written Status notes gone
- [ ] Clippy clean at `-D warnings`, then gated in CI
- [ ] Dependency pass: report, safe bumps only

**What the code map found** (2026-10-28, `feat/isolation-truth` = `main` + 2 merge commits)

- *The lint today.* `vam-e2e check` lints only `e2e/` (`check.rs:30`
  `check(root)`); `main.rs:113` and `run_cmd.rs:529` call it (every `run`
  lints the catalog first), and `tests/check_fixtures.rs:74`
  `repo_catalog_is_clean` runs it inside `cargo test --workspace`. A scenario's
  `needs` is `process | firecracker | ui` (`scenario.rs:72-77`) and every
  scenario starts a `vamd`, so a lint of the repository is **not** a catalog
  scenario: it belongs at the contract layer (`design/scenarios.md` §5,
  "Unit / contract"), as a `vam-e2e` integration test that drives the CLI on
  a fixture tree, like the existing one-fixture-per-rule tests. Plan numbers
  stay out of its name (S2).
- *Docs schema.* `docs/src/content.config.ts` uses a bare `docsSchema()`.
  Zod drops unknown keys, so a `status` key would not fail the build, and a
  typo (`status: shiped`) would pass silently. Extending the schema
  (`docsSchema({ extend: z.object({...}) })`) makes the build a second gate.
  No `docs/node_modules` in the container: a docs wave runs `pnpm install`
  first.
- *Docs today.* 207 pages (`build` 21, `runtime` 20, `orchestrator` 20,
  `telemetry` 18, `security` 18, `storage` 17, `network` 17, `spec` 15,
  `core` 15, `getting-started` 11, `hosted` 10, `architecture` 9, `authoring`
  8, 8 others). 40 pages carry a hand-written `:::note[Status: ...]` (16 say
  "Fully Implemented Local Storage Plane", 18 "Implemented": all 17 `network/`
  pages, all 17 `storage/` pages, `architecture/drive9`, three `runtime/`,
  `spec/skills`, `security/spiffe`, `security/secrets-proxy`). A claim-word
  list (below) hits the prose of **153 of 207** pages, so "every page that
  mentions isolation" is most of the site, and the field pass must decide
  `planned` or `stale` for most of them rather than find a proof.
- *Overclaims found* (examples; the waves find the rest):
  `README.md:15` "the complete `vam-storage` and `vam-network` implementations";
  `getting-started/project-status.md:27` `vam-build` "Fully runnable" (the
  crate is only `crates/vam-build/SKILLS.md`), `:21` "~115 ms VM-ready" (the
  owner replaced that number in the design docs on 2026-10-24) and an HTTP API
  on `127.0.0.1:8080` (TLS since Phase 1), `:19` the jailer listed as planned
  (on by default since Phase 1), `:30` docs "up-to-date"; `index.mdx:6`
  "zero-trust egress"; `network/overview.md:7` "fully implemented, verified"
  and an `ebpf.rs` "TC program loader"; `network/policy.md:13` "Phase 1 does
  not point the guest at port 55" (plan language, likely stale);
  `storage/overview.md:10` "transactional, and POSIX-compliant file system"
  and `storage/fuse.md` a FUSE client "linked into the guest" (no FUSE code
  in any crate: every `fuse` match is "refuse"); `security/overview.md:35-60`
  SPIFFE SVIDs "injected into Guest" (no SVID code) and "custom Seccomp BPF
  filters" (only Firecracker's built-in filter, the jailer runs without
  `--no-seccomp`); `core/jailer.md` cgroup limits (cgroup v2 is configured,
  no CPU or memory limit is set; a Phase 1 follow-up);
  `architecture/trust-boundaries.md:85` "each VM runs inside a dedicated
  netns" (brokered sessions skip TAP and netns since cold-start Phase 3);
  the per-session Hickory DNS proxy (`vam-network/src/dns.rs`) has no caller
  in `vam-orchestrator`. **Underclaims too:** `architecture/firecracker.md:7`
  "No native Firecracker code exists in the repository".
- *No `runtime/isolation*` page exists.* The isolation story lives in
  `architecture/firecracker.md` and `architecture/trust-boundaries.md`; this
  plan rewrites those two in its place. The launch "security page" is
  proposed as `security/overview.md` (open question 1).
- *eBPF.* `crates/vam-network/src/ebpf.rs` (40 lines: `EbpfEnforcer`, a
  `load_and_attach` that only probes and returns `Ok`, one test);
  `lib.rs:6` `pub mod ebpf`, `:25` `pub use ebpf::EbpfEnforcer` (no caller
  outside the crate); `capability.rs:9,21,35` `has_bpftool` (read only by
  `ebpf.rs:19`; set in `pool.rs:75,93`); `error.rs:19` `EbpfCompilationError`
  (never constructed); `proto/vam/v1/network.proto:65` comment
  `"ebpf_cidr"` (a value nothing writes; `proto/` is in the lane filter and
  `vam_descriptor.bin` holds the file, so the descriptor check must stay
  green); `crates/vam-network/SKILLS.md`; `AGENTS.md` crate table ("eBPF
  packet filtering"); `docs/astro.config.mjs:222` (sidebar entry
  `network/ebpf`). Docs: **25 pages, 70 lines** (`network/` 14 pages incl.
  the whole `network/ebpf.md`; `architecture/` 5; `getting-started/` 3;
  `core/security-model`, `telemetry/roadmap`, `index.mdx`).
- *Build pages.* 21 pages, 19 with an "Alpha" caution; `build/rootfs.md`
  describes `scripts/build-rootfs.sh`, which exists. Six non-build pages
  link into `/build/`. The launch doc cuts the Vamfile language and builder
  (E7 Phases 3-5) from 0.1.0.
- *CI.* `rust.yml:6-18` ignores `docs/**`, `design/**`, `.cursor/**` and
  `**/*.md`, so a docs-only PR runs nothing, and a docs lint inside the Rust
  workflow would never see a docs PR. No workflow builds the docs site today.
  No job runs clippy. `rust-toolchain.toml` is `channel = "stable"` (rustc
  1.97.0 today), so an unpinned clippy gate can go red on a new Rust release
  with no code change.
- *Clippy* (`cargo clippy --workspace --all-targets`, rustc 1.97.0, 2m23s
  warm, 0 errors): **642 warnings reported** (the owner's ~640), **440 unique
  sites** after removing duplicate targets. Per crate, reported / unique
  (src + tests): `vam-orchestrator` 327 / 259 (81 + 178), `vam-firecracker`
  102 / 59, `vam-runtime` 61 / 35, `vam-storage` 42 / 25, `vam-core` 32 / 16,
  `vam-network` 17 / 13, `vam-telemetry` 14 / 7, `vam-inference` 14 / 7,
  `vam-security` 10 / 6, `vam-vsock` 8 / 4, `vam-e2e` 8 / 5, `vam-cli` 5 / 4,
  `vam-spec` 2 / 1 (prost-generated `vam.v1.rs`), `vam-web` 0. Kinds: 119
  `await_holding_lock` (116 in `vam-orchestrator` tests, 3 in `vam-runtime`:
  `agent.rs:5428`, `exec.rs:1372`, `:1425`), 30 `result_large_err` (all
  orchestrator tests), 45 `too_many_arguments` (22 orchestrator src), and a
  long tail of mechanical style lints (redundant closures, `from_ref`,
  needless borrows, `new_without_default`, `0 * 1024`-style no-ops). **Sites
  that need judgement, listed not fixed blindly:** `vam-firecracker/src/agent/rt_std.rs:102`
  a `const LazyLock` (interior-mutable const, re-created on each use; test
  module, holds the `/home/ec2-user` default), 4 `suspicious_open_options` in
  `vam-firecracker`, `manager.rs:112` and `host_tools.rs:4597` identical `if`
  blocks, the 3 `vam-runtime` guards held across `.await`.
- *Dependencies.* `cargo tree -d`: **34 crates in two or three versions**
  (the design's 55 predates Phase 1 W3, which dropped openssl and
  native-tls): `prost` 0.12/0.13/0.14, `getrandom` 0.2/0.3/0.4, `hyper`
  0.14/1, `http` 0.2/1, `rustls` 0.21/0.23, `reqwest` 0.11/0.13, `tower`
  0.4/0.5, `thiserror` 1/2, `base64` 0.21/0.22, `rand` 0.8/0.9 and others.
  `tonic` 0.10.2 in `vam-spec` (+ `tonic-build` 0.10), `vam-orchestrator`
  and `brokers/vam-web`; `axum` 0.6 + `axum-server` 0.5 + `rustls` 0.21 in
  `vam-orchestrator` (Phase 1 W3 chose them, upgrade deferred to here);
  `vam-security` alone uses `prost-types` 0.13. The vendored SDK
  (`crates/vam-firecracker`, "Hardened fork of firecracker-rs-sdk", 4,189
  lines) is used by `vam-core` for `models` (17 uses), `jailer`, `events`,
  `instance`; it ignored HTTP status until Phase 1 W4b and still carries
  `/home/ec2-user` defaults in `agent/rt_*.rs`.
- *Other lanes.* Open PRs: `` (H1 W0, `h-1/wave-0` → `feat/harness`:
  `e2e/scenarios` and one `vam-inference` file). E10 Phase 3 (Steer) is not
  planned yet; its build waves will edit `vam-runtime/src/agent.rs`,
  `vam-orchestrator/src/api/`, `brokers.rs`, `workflow.rs` and runtime and
  orchestrator docs pages. H1 edits `vam-inference` and the catalog.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `docs-claim-without-proof-fails-check` (the Rust test `docs_claim_without_proof_fails_check`) | new | contract (`crates/vam-e2e/tests/docs_check.rs`, drives the built `vam-e2e check` on a fixture tree; not a catalog scenario: the catalog has no lint layer and every scenario starts a `vamd`) | fixture: a one-scenario catalog and one docs page with `status: shipped`, the prose "The VMM runs jailed." and no `proves` | `check --docs <fixture>` exits 1 naming the page and rule `docs-proof`; with `proves: [<active scenario>]` it exits 0. The phase's exit; locked decision 1 made mechanical |
| `egress-refuses-an-ungranted-host` | new | process | `org-admin`, `agent-with-shell` | The agent's shell asks the guest proxy for `http://169.254.169.254/latest/meta-data/` and for a public host the session was never granted: both refused, the thread carries the refusal, and `egress.decided` events say deny. The gate is host code shared by both layers (the process guest runs the same `start_guest_proxy_with` over the same stream), so process is honest for **the gate**; the no-bypass half is the next row. Proves `network/policy` and the security page's egress row |
| `guest-has-no-route-off-the-vm` | new | firecracker | `org-admin`, `agent-with-shell` | In a microVM the guest's routing table has no default route and only loopback is up; a direct TCP connect to a public address fails, while the proxy answers. This is the honest claim that replaces eBPF (design open question 2: "the guest has no route and the host proxy makes every connection"). A process guest is a host process with the host's routes, so it cannot show this |
| `vault-secret-is-not-in-the-guest` | new | firecracker | `org-admin`, `api-token`, `agent-with-shell` | With an org secret set, the guest's environment (`/proc/1/environ`, the tool's `env`) and its workspace hold no form of it (`secret-absent`, which searches the thread and workspace). Firecracker only: a process guest's environment is built by a different spawn path. Proves the security page's "the guest never holds a credential" row and `security/secrets-proxy` |
| `jailer-confines-the-vmm-process` | changed: the VMM also runs under a seccomp filter | firecracker | unchanged | The `vmm: jailed` probe (`crates/vam-e2e/src/vmm_probe.rs`) also reads `Seccomp:` from `/proc/<pid>/status` and requires `2` (filter mode). Proves the "Firecracker's built-in seccomp filters are on" line that `core/jailer` and the security page keep. Spec change: the owner approves it with this plan |

**Claims kept, and what proves them** (from `then` and `needs` of the 45
scenarios in `e2e/scenarios/`): microVM per session
(`vm-boots-and-registers-over-vsock`, lane); jailer: chroot, non-root uid,
own pid and mount namespaces (`jailer-confines-the-vmm-process`, lane);
node and receipt report the isolation they ran
(`node-reports-the-isolation-it-runs`, `receipt-names-the-isolation-the-run-had`,
lane; `receipt-separates-host-facts-from-model-text`, smoke); TLS by default
(`vamd-refuses-plain-http-by-default`); no encoded secret in a tool result
(`secret-never-reaches-guest-encoded`); a tool result cannot drop the control
channel (`host-tool-output-fits-frame`); loopback-only guest proxy
(`guest-proxy-never-binds-wildcard`, lane); API rate limits
(`api-rate-limits-a-flood`); guarded tools wait for a human and denials are
the host's (`approval-suspends-and-resumes`,
`receipt-shows-denial-before-approval`, `released-agent-approvals-are-once-only`,
`always-allow-and-always-deny-are-remembered`); injected text cannot start a
playbook (`injected-text-cannot-start-a-playbook`); agents' workspaces are
separate (`agent-workspaces-are-isolated`); the receipt verifies offline and
fails on tamper (`receipt-verifies-offline-and-fails-on-tamper`); files match
the conversation after a VM stop (`workspace-matches-conversation-after-vm-stop`,
lane: the honest form of "transactional" storage, per finished turn). The
web broker's guarded fetch (DNS pinning per hop, robots) is proven by unit
tests, cited as test paths (`brokers/vam-web/src/fetch/network/tests.rs`,
`fetch/robots.rs`; the wave names the functions).

**Claims with no scenario and no cheap one: rewritten as planned (or
removed)**: SPIFFE/SPIRE workload identity and SVIDs; seccomp profiles
generated by `vam-security` (beyond Firecracker's built-in filter); cgroup
CPU and memory limits on the VMM; PII and entropy redaction of guest output
(the telemetry redactor covers telemetry export only); the restricted-direct
network mode (per-session netns, TAP, nftables NAT, traffic control); the
per-session DNS proxy; a FUSE driver and a POSIX-compliant cloud-synced
`/mnt/drive9`; cloud object-store backends and encryption at rest; the
native `vam-build` pipeline and Vamfile; first-party `vam.files` and
`vam.rag` brokers (already marked design); a pre-booted standby pool
(`architecture/firecracker.md`; `WARM` is cut from 0.1.0). Kernel-level
egress filtering is **removed**, not planned (decided 2026-10-28).

Unchanged and must stay green: every existing scenario; this phase edits
`vam-network` (eBPF removal), lint fixes across every crate and one probe in
`vam-e2e`, none of which a scenario's `then` reads. The three new scenarios
assert behaviour the node is believed to have today, so **they are expected
to pass on the base**: wave 0 writes each and runs it. Passing → it lands
`active` (a proof of a shipped claim, not a spec of new behaviour; S8's "a
pending scenario that passes early is wrong" applies to new behaviour). Red
→ that is a finding: the claim is false today; it lands `pending` with the
step and class, the claim is rewritten as planned in the docs waves, and the
fix is brought to the owner as a new item (security floor, `Decide: NOW`
whatever the cost). Same for the seccomp step: if Firecracker's main thread
reads `Seccomp: 0` on the lane, the scenario keeps its old `then` and the
seccomp line becomes planned.

**Benchmark delta**

No line in `design/benchmark.md` §1 moves (read 2026-10-28: its 14 problem
lines measure runs, not docs or the build; P6 and P9 were moved in Phase 2).
No baseline run, no paid endpoint. Launch proofs moved
(`design/launch-0.1.0.md` §1): **the security page** ("every isolation claim
names its scenario; CI boots a VM; jailer and TLS on by default"): moved
means, at close, the lint green on the base with no ratchet, and
`security/overview.md` publishing one row per claim with its scenario, the
lane or smoke job that runs it, and the operator setting that turns it off;
the other two halves were done in Phase 1. **The honest page** ("what VAM is
not for, where open models still lose"): half-moved: the "not for" page ships
now; "where open models still lose" needs B1's open-model rows (benchmark
decision 4) and is a `planned` section until then. Recorded at close as
"security page: done; honest page: half (waits on B1)".

**Wave 0 "scenarios"** (Sonnet medium: three scenarios that only the microVM
lane can run, and a `/proc` probe change)

1. `crates/vam-e2e/tests/docs_check.rs`: `docs_claim_without_proof_fails_check`
   runs `env!("CARGO_BIN_EXE_vam-e2e") check --root <fx>/e2e --docs <fx>/docs`
   on a fixture tree and asserts exit 1 and `docs-proof` naming the page.
   Marked `#[ignore = "pending: docs lint"]`; red for the right reason with
   `cargo test -p vam-e2e --test docs_check -- --ignored`: clap refuses the
   unknown `--docs` (exit 2), so the assertion fails, not the build.
2. `egress-refuses-an-ungranted-host` (process): a `run` call whose output
   tokens are assembled by the shell (as `guest-proxy-never-binds-wildcard`
   does, so the command text never contains them), `thread` contains, and
   `expect: { event: egress.decided, fields: {...} }` (field names confirmed
   from `events.rs:301`). Run with `scripts/e2e/run.sh --scenarios`.
3. `guest-has-no-route-off-the-vm`, `vault-secret-is-not-in-the-guest`
   (firecracker): existing forms (`agent-with-shell`, `api-token`,
   `secret-absent`, `thread`); run on the lane.
4. `vmm_probe.rs`: read `Seccomp:` from `/proc/<pid>/status`, a new
   `seccomp: filter` option on `expect: { vmm: ... }` (default off so other
   uses keep their meaning), fake-tree unit test; the jailer scenario gains it.
5. `e2e/README.md` lines for the new probe option; `vam-e2e check` clean.

Proven: the contract test red as above; each new scenario's result on the
base recorded in the PR (pass → `active`; red → `pending` with step and
class, and a finding for the owner); `--smoke` otherwise unchanged; the lane
runs (W0 touches `crates/vam-e2e/` and `e2e/scenarios/`).

**Items**

- [ ] **IT-3.1 Docs status field and the docs lint.** Frontmatter on every
  page: `status: shipped | planned | stale` (`shipped`: the page describes
  what a default node does; `planned`: not built; `stale`: the code exists
  but the page says more than it does or is out of date) and `proves: [...]`,
  each entry a scenario id or a test path (`crates/x/tests/y.rs` or
  `...::fn_name`). `README.md` carries the same two keys in a leading
  `<!-- docs: ... -->` comment (GitHub would render YAML frontmatter).
  **Claim rule** (open question 2): a `shipped` page makes an isolation claim
  when its prose (outside frontmatter, code fences and HTML comments) matches
  a pattern in a new `e2e/docs-claims.yaml`; default list `isolat`, `jail`,
  `sandbox`, `exfiltrat`, `zero.trust`, `seccomp`, `kernel.level`, `ebpf`,
  `tamper`, `escape`, `never (reach|leave|enter|see)s?`,
  `(cannot|can't|never) (reach|read|see|access|bypass|escape)`
  (case-insensitive; 153 of 207 pages hit it today). Rules, each with a
  fixture test in `tests/docs_check.rs`: `docs-status` (missing or unknown
  value), `docs-proof` (a claim page with empty `proves`), `docs-proof-ref`
  (a scenario id that does not exist; on a `shipped` page a scenario that is
  not `active`: `pending` reads "not proven yet"; `retired` always fails; a
  test path whose file or `fn` is missing; `planned` pages may name
  `pending` scenarios as forward links), `docs-admonition` (a hand-written
  `:::note[Status` or `:::caution[Status` left), `docs-baseline` (below).
  **Ratchet:** `e2e/docs-baseline.txt` lists every page failing today (all
  207 plus the README on day one); a listed page is exempt; a listed page
  that no longer fails is itself an error, so the list only shrinks; each
  docs wave deletes its pages' lines; IT-3.7's last wave deletes the file.
  So `check` passes on the base after every merge, and the exit is the empty
  ratchet. `check` gains `--docs` (default `docs/src/content/docs`) and
  `--readme` (default `README.md`); the docs part runs in `check` only, not
  before `run` (a docs error must not stop a scenario run). New
  `repo_docs_are_honest` test beside `repo_catalog_is_clean`, so a code PR
  that retires a scenario or renames a cited test fails `cargo test
  --workspace`. **Site:** `content.config.ts` extends `docsSchema` with
  `status` (enum, optional until IT-3.7's last wave makes it required) and
  `proves` (string array); a Starlight component override renders the field:
  a "Planned: VAM does not do this yet" or "Out of date" banner, and on a
  shipped page with proofs a "Proved by" line linking each scenario file.
  **CI** (open question 4): new `.github/workflows/docs.yml` on `docs/**`,
  `README.md`, `e2e/**`, `crates/vam-e2e/**` and itself: `cargo run -p
  vam-e2e -- check` then `pnpm install --frozen-lockfile && pnpm build` in
  `docs/`; job name `docs`. Playbook §4 "Docs-only PRs have no checks" and
  AGENTS.md §5 CI paragraph updated in the same PR. *Files:*
  `crates/vam-e2e/src/{docs.rs (new), check.rs, main.rs, lib.rs}`,
  `tests/docs_check.rs`, `e2e/docs-claims.yaml`, `e2e/docs-baseline.txt`,
  `e2e/README.md`, `docs/src/content.config.ts`, `docs/astro.config.mjs`
  (component override), `docs/src/components/` (new),
  `.github/workflows/docs.yml`, `.cursor/workstreams/phase-playbook.md`,
  `AGENTS.md`. *Accept:* every rule has a failing fixture;
  `docs_claim_without_proof_fails_check` un-ignored and green; `check` clean
  on the base (ratchet); `pnpm build` green; a fixture page with
  `status: shiped` fails the build. *Test:* `cargo test -p vam-e2e`, `cargo
  run -p vam-e2e -- check`, `cd docs && pnpm build`. *Turns green:*
  `docs_claim_without_proof_fails_check`.
  `Decide: NOW — locked decision 1 made mechanical (not scored, rubric §3); proof 3 (security page), cost 1 (one wave, vam-e2e and docs config, no product surface)`
- [ ] **IT-3.2 eBPF removed.** Delete `crates/vam-network/src/ebpf.rs`, its
  `mod` and `pub use` (`lib.rs:6`, `:25`) and test, `has_bpftool`
  (`capability.rs:9,21,35`; `pool.rs:75,93` and the test caps), the unused
  `EbpfCompilationError` (`error.rs:19`); the `network.proto:65` comment
  lists the values the gate writes (kept only if `cargo test -p vam-spec`
  and `scripts/e2e/check-manifests.sh` stay green with the descriptor
  baseline; otherwise left and noted). Docs: every eBPF line on the 25 pages
  rewritten to what is true ("every session runs brokered: the guest has no
  route, and the host proxy makes every connection and decides it", citing
  `guest-has-no-route-off-the-vm` and `egress-refuses-an-ungranted-host`),
  never "planned"; `network/ebpf.md` folded into a short "Why there is no
  kernel filter" section of `network/overview.md` with the three reopen
  triggers, the page deleted, its sidebar line removed and an Astro redirect
  from `/network/ebpf/` (open question 5). `AGENTS.md` crate table and
  `crates/vam-network/SKILLS.md`. Each touched page gets its `status` and
  `proves` and leaves the ratchet. *Files:* the four `vam-network` sources,
  `proto/vam/v1/network.proto`, the 25 pages, `docs/astro.config.mjs`,
  `AGENTS.md`, `crates/vam-network/SKILLS.md`, `e2e/docs-baseline.txt`.
  *Accept:* `rg -i ebpf crates proto AGENTS.md docs/src docs/astro.config.mjs`
  empty; `/network/ebpf/` redirects. *Test:* `cargo test -p vam-network`,
  `cargo check --workspace`, `cargo run -p vam-e2e -- check`, `pnpm build`.
  *Turns green:* none (rides on W0's two egress scenarios).
  `Decide: NOW — locked decision 4 (not scored); owner decision 2026-10-28; proof 3 (security page), cost 1`
- [ ] **IT-3.3 The security page, README, home and project status.**
  `security/overview.md` becomes the launch security page (open question 1):
  "What a default node does", one row per claim (claim, scenario, where it
  runs: smoke or microVM lane, the operator setting that turns it off and
  that the node and receipt report), then a "Planned" list (the planned
  claims above) and a one-paragraph "Removed: kernel-level egress
  filtering". `architecture/trust-boundaries.md` and
  `architecture/firecracker.md` (in place of the missing `runtime/isolation`
  page) rewritten to the same rows. `README.md` "Current Project Status"
  rewritten to the security page's rows plus a link; `index.mdx` tagline
  reworded to what is proven. `getting-started/project-status.md`: the
  matrix rewritten crate by crate to what runs (TLS API, jailer on, golden
  restore about 225 ms session start on the CI lane, `vam-build` = one
  script, docs status now machine-checked), its "Documentation
  Synchronization Rule" pointed at the field. *Files:* those six pages,
  `README.md`, `e2e/docs-baseline.txt`. *Accept:* each claim row names an
  `active` scenario or moves to Planned; the ratchet loses these pages;
  `pnpm build` green. *Test:* `cargo run -p vam-e2e -- check`, `pnpm build`.
  *Turns green:* none.
  `Decide: NOW — proof 3 (the security page launch proof), cost 1 (docs only)`
- [ ] **IT-3.4 Network, storage, jailer and hosted pages.** Rewritten to
  what the node does, each claim naming its scenario: `network/overview.md`
  (brokered by default, TAP skipped, no kernel filter; the module tree as it
  is), `network/policy.md` (allowlist from secret bindings, protected deny
  set, ports; cites `egress-refuses-an-ungranted-host` and the `vam-network`
  gate unit tests; plan language removed), `storage/overview.md` and
  `storage/fuse.md` ("transactional POSIX" becomes: a per-session ext4 image
  at `/mnt/drive9`, committed per finished turn, matching the conversation
  after a stop: `workspace-matches-conversation-after-vm-stop`,
  `follow-up-continues-after-stop`, `work-survives-vamd-restart-mid-task`;
  FUSE and cloud backends `planned`), `core/jailer.md` (on by default, what
  the jail is, the opt-out the receipt reports; cgroup limits and custom
  seccomp planned), `hosted/first-party.md` (web broker shipped with its
  test paths; files and RAG planned). *Files:* those six pages,
  `e2e/docs-baseline.txt`. *Accept, Test:* as IT-3.3. *Turns green:* none.
  `Decide: NOW — proof 3 (security page), cost 1 (docs only)`
- [ ] **IT-3.5 The honest page.** New `getting-started/limits.md`, "What VAM
  is not for" (sidebar under Getting Started): one Linux host with KVM (Mac
  through Lima), one org (no multi-tenant), process mode is a test build and
  isolates nothing (the release gate keeps it out), no image builder (one
  standard base), no direct network mode and no kernel-level egress filter,
  no FUSE or cloud-synced storage, golden-restore session start about
  225 ms (the 50 ms SLO is not met), no certification (SOC 2, ISO 42001); a
  `planned` section "Where open models still lose" linking
  `runtime/benchmark-results` until B1 publishes open-model rows (open
  question 6). *Files:* the new page, `docs/astro.config.mjs`. *Accept:*
  `pnpm build` green, the page in the sidebar, `check` clean. *Turns green:*
  none.
  `Decide: SLICE — proof 3 (honest page launch proof), cost 2 with the open-model table (needs B1) → slice: the "not for" page now, docs only, cost 0; the losses table BACKLOG, trigger "B1 publishes its first open-model rows"`
- [ ] **IT-3.6 Build pages behind a planned banner.** All 21 get `status`:
  `build/rootfs.md` `shipped` (the script; proof: the lane builds the rootfs
  with it on every VM PR, cited as `scripts/build-rootfs.sh` plus
  `vm-boots-and-registers-over-vsock`), the rest `planned`; the "Alpha"
  cautions on planned pages removed (the banner replaces them); the sidebar
  group renamed "Images & Build (planned)" with `rootfs` first. Open
  question 3 proposes moving 10 of them to `design/vam-build/`; **moving or
  deleting a page needs the owner's word**; without it all 21 stay as
  `planned`. A moved page gets an Astro redirect to `/build/overview/`.
  *Files:* `docs/src/content/docs/build/*`, `docs/astro.config.mjs`,
  `e2e/docs-baseline.txt`, and `design/vam-build/` only if approved.
  *Accept:* `pnpm build` green, no 404 on the six inbound links, `check`
  clean. *Turns green:* none.
  `Decide: NOW — locked decision 4 (not scored); proof 2 (design finding 12: 21 pages describe an unbuilt pipeline), cost 0`
- [ ] **IT-3.7 Status on every remaining page.** About 155 pages, no
  rewrite: each gets an honest `status` and, where it makes a claim, either
  `proves` (an existing `active` scenario or test path) or `stale`/`planned`.
  The 40 hand-written Status notes are removed (the field replaces them);
  "Alpha" cautions stay on shipped pages. The last wave deletes
  `e2e/docs-baseline.txt` and makes `status` required in
  `content.config.ts`. Three waves by section (below). *Accept:* `check`
  clean with no ratchet; `pnpm build` green. *Test:* as IT-3.3. *Turns
  green:* the exit's "clean on the base".
  `Decide: NOW — proof 3 (the exit; security page), cost 1 per wave (frontmatter only)`
- [ ] **IT-3.8 Clippy clean, by crate.** Mechanical fixes and
  `#[allow(lint, reason = "...")]` only; no behaviour change. Policy:
  `too_many_arguments` allowed per function with a reason (a signature
  refactor is not mechanical); `await_holding_lock` and `result_large_err`
  in tests allowed per file with the reason (the guards serialize
  environment mutation across a whole test); `large_enum_variant` on the
  generated `vam-spec` module allowed at the `include!`;
  `suspicious_open_options` get the explicit `.truncate(false)` that is
  today's behaviour; `vam-firecracker` takes a crate-level
  `#![allow(clippy::style, clippy::complexity, ...)]` with the reason
  "vendored fork of firecracker-rs-sdk; replacement in the dependency
  report" (open question 7) and its correctness-group sites are fixed (the
  test `const LazyLock` becomes a `static`). A fix that would change
  behaviour (the 3 `vam-runtime` guards held across `.await` if they are not
  in test code, the two identical-`if` sites if merging changes order of
  effects) is listed in the PR body with file:line and left with an `allow`
  and a `TODO(clippy)` reason, not made. *Accept per wave:* `cargo clippy -p
  <crates> --all-targets -- -D warnings` clean; `cargo test -p <crates>`
  green; the diff holds only lint fixes and allows. *Turns green:* none.
  `Decide: NOW — locked decision 5 (not scored); cost 1 per wave`
- [ ] **IT-3.9 Dependency pass.** A written report as a new section
  "Dependency pass (Phase 3)" in `design/isolation-truth.md` (evidence
  belongs in the design; open question 8): the vendored SDK (what VAM uses
  of it, its fixes since the fork, replace with upstream
  `firecracker-rs-sdk` or a thin in-tree client, or keep), `tonic` 0.10 and
  what moving to 0.12 drags (`prost` 0.13, `hyper` 1, `axum` 0.7,
  `vam-spec` codegen, `brokers/vam-web`, the HSP brokers in TypeScript stay
  wire-compatible), the 34 duplicated crates each with the dependency that
  pins the old one. Only safe changes are made: patch and same-minor
  updates of direct dependencies, dedupes that need no code change (e.g.
  `vam-security` `prost-types` 0.13 → 0.12 if it compiles unchanged), the
  vendored SDK's `/home/ec2-user` test defaults (Phase 2 open question 9).
  Every other bump gets a `Decide:` line and a row in "Explicitly later".
  *Files:* `design/isolation-truth.md`, `Cargo.toml`s and `Cargo.lock` for
  safe changes only, `crates/vam-firecracker/src/agent/rt_*.rs`, this
  tracker. *Accept:* report with the before/after `cargo tree -d` count;
  workspace builds and tests green in CI; the lane green (`Cargo.lock` is in
  the filter). *Test:* `cargo tree -d`, `cargo check --workspace`, `cargo
  test -p` for each crate whose dependency moved.
  `Decide: SLICE — proof 2 (design finding 13; the SDK ignored HTTP status until W4b), cost 3 for the major upgrades (axum 0.7, hyper 1, tonic 0.12, rustls 0.23, SDK replacement) → slice: the report plus safe bumps, cost 1; majors BACKLOG each with its own line`
- [ ] **IT-3.10 Clippy gate in CI.** A new `clippy` job in `rust.yml`
  (parallel, not inside `checks`), toolchain pinned to the version the
  waves measured (open question 9), `cargo clippy --workspace --all-targets
  -- -D warnings`, own cache. The wave first takes `main`, reruns clippy on
  the fresh base and fixes what other lanes added meanwhile (mechanical
  only, same policy). AGENTS.md §5 CI paragraph names the job. Owner adds
  `clippy` to required checks. *Files:* `.github/workflows/rust.yml`,
  `AGENTS.md`, residue fixes. *Accept:* the job green on the PR and red on a
  scratch commit with one new warning (shown in the PR, then dropped).
  *Turns green:* the exit's "clippy gate green".
  `Decide: NOW — locked decision 5 (not scored); cost 1`

**Waves**

Each wave: one session in its own worktree of vam, base
`feat/isolation-truth`, branch `it-3/wave-N`, plan-only first turn, draft PR
titled `IT-3 WN: …`, marked ready after local checks. No local `cargo test
--workspace`. Known local failure not to chase:
`hsp_uds::uds_socket_acl_deny_fails_fast` as root. Docs waves run `pnpm
install` and `pnpm build` in `docs/` (routing and links) and `cargo run -p
vam-e2e -- check`. Docs-only waves trigger no Rust CI (paths-ignore) until
W1's `docs` workflow exists; after W1 they run the `docs` job only. A PR
touching `crates/vam-core|vam-firecracker|vam-runtime|vam-orchestrator|vam-vsock|vam-storage|vam-e2e`,
`e2e/`, `proto/` or `Cargo.lock` triggers the lane unless it carries
`skip-vm-lane`.

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Shared surfaces | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 | scenarios | `docs_claim_without_proof_fails_check` red (right reason); the three new scenarios and the seccomp step run on the base: green → `active`, red → finding | vam | Sonnet medium: three scenarios only the lane runs, a `/proc` probe | 90 min | — | `vam-e2e` `vmm_probe.rs` and tests; H1 W0 adds scenarios in `e2e/scenarios/` only: **Low**. Lane runs | [x] | — |
| 1 | IT-3.1 | `docs_claim_without_proof_fails_check` | vam | Sonnet medium (owner): a new lint, the docs schema and a component, a new workflow | 150 min | 0 | `vam-e2e` `check.rs`, `main.rs` (H1 may add `scenario.rs` fields: **Low**); `docs/astro.config.mjs` (any lane adding a docs page edits the sidebar: **Low**, rebase); `phase-playbook.md`, `AGENTS.md` §5: **Low**. Lane runs (`crates/vam-e2e/`) | [x] | — |
| 2 | IT-3.2 | — (rides on W0's egress scenarios) | vam | Sonnet low | 90 min | 1 | `vam-network` (no other lane); `network.proto` comment (lane filter: `skip-vm-lane`); `AGENTS.md` one line; 25 pages incl. `getting-started/platform-tour`, `workspace`: **Low** | [x] | — |
| 3 | IT-3.3 | — | vam | Sonnet low | 90 min | 2 | `README.md`, `index.mdx`, `project-status.md` (E10 and H1 closes edit project status: **Low**, rebase) | [ ] | |
| 4 | IT-3.4, IT-3.5 | — | vam | Sonnet low | 90 min | 3 | `hosted/first-party.md`, `network/`, `storage/`, `core/jailer.md`: no other lane. `astro.config.mjs` one line | [ ] | |
| 5 | IT-3.6 | — | vam | Sonnet low | 45 min | 4; open question 3 answered | `build/` and the sidebar group: no other lane | [ ] | |
| 6a | IT-3.7 (`security/` 17, `core/` 13, `spec/` 15, `architecture/` 3, `network/` 3) | — | vam | Sonnet low | 90 min | 5 | none | [ ] | |
| 6b | IT-3.7 (`runtime/` 20, `orchestrator/` 20, `hosted/` 9) | — | vam | Sonnet low | 90 min | 6a | E10 Phase 3 and H1 edit `runtime/*.md` and `orchestrator/*.md` bodies; this wave edits frontmatter and removes three Status notes: **Low**, rebase on conflict | [ ] | |
| 6c | IT-3.7 (`storage/` 15, `telemetry/` 17, `getting-started/` 8, `authoring/` 8, 7 others); deletes the ratchet, `status` required | exit: `check` clean, no ratchet | vam | Sonnet low | 90 min | 6b | none | [ ] | |
| C1 | IT-3.8: `vam-firecracker` (59), `vam-core` (16), `vam-vsock` (4), `vam-spec` (1) | — | vam | Sonnet low | 75 min | 6c | no other lane. Lane runs (VMM path; the one lane run proves the lint fixes) | [x] | — |
| C2 | IT-3.8: `vam-storage` (25), `vam-network` (13), `vam-security` (6), `vam-telemetry` (7), `vam-cli` (4), `vam-e2e` (5) | — | vam | Sonnet low | 75 min | C1 | `vam-e2e` (H1 W0 touches no `vam-e2e` code): **Low**. `skip-vm-lane` (style fixes; `vam-storage` and `vam-e2e` are in the filter, the next lane run covers them) | [x] | — |
| C3 | IT-3.8: `vam-orchestrator` tests except `tests/api_tests.rs` (about 83 sites; `thread_recorder/tests.rs`, `tests/*.rs`) | — | vam | Sonnet low | 75 min | C2 | test files only, no other lane. `skip-vm-lane` | [x] | — |
| D | IT-3.9 | — | vam | Sonnet medium: dependency changes across crates, `Cargo.lock` | 120 min | C3 | **`Cargo.lock` and each bumped crate's `Cargo.toml`**: HOLD while another lane has an open PR that edits `Cargo.lock` or the same `Cargo.toml` (H1: `vam-inference`); take `main` the same day. Lane runs | [x] | — |
| C4 | IT-3.8: `vam-orchestrator` src (81, incl. `api/`, `host_tools.rs`, `manager.rs:112`) and `tests/api_tests.rs` (95) | — | vam | Sonnet low | 90 min | D | **`vam-orchestrator`: HOLD while E10 Phase 3 or H1 has an open PR in the crate; take `main` the same day before starting.** `skip-vm-lane` unless a non-test file outside `api/` changes | [x] | — |
| C5 | IT-3.8: `vam-runtime` (35, incl. `agent.rs`) and `vam-inference` (7) | — | vam | Sonnet low | 75 min | C4 | **`vam-runtime` (`agent.rs`, E10 Phase 3) and `vam-inference` (H1 open today): HOLD per crate while that lane has an open PR in it; take `main` the same day.** Lane runs (`vam-runtime`) | [x] | — |
| C6 | IT-3.10 | exit: clippy gate green | vam | Sonnet medium (owner): CI job, residue across crates on a fresh base | 75 min | C5 | `rust.yml` (any lane's CI edit: **Low**); residue in any crate: take `main` the same day; hold a crate with an open lane PR as C4/C5 | [x] | — |

Rows 3 to 6c are superseded on approval by the "Docs plan: architecture
rebuild and developer docs (proposed 2026-10-29)" at the end of this phase
section (the 2026-10-28 re-plan it replaces is withdrawn).

Docs before clippy because the security page is the launch proof this phase
moves; W1 first so every later docs wave is checked by the lint; W2 before
W3/W4 so the rewritten pages already have the honest network claim; C4 and
C5 last among the crate waves because two other lanes edit those crates; D
before C4 so new versions' lints are cleaned in the same pass; C6 last.
Budget about 22 hours of wave time; lane runs on W0, W1, C1, D, C5 (and W2
skips), about 5 to 7 runs. Wave decision lines: W0 `Decide: NOW — proof 3
(security page claims need proofs), cost 1`; W1 to W6c, C1 to C6 and D as
their items.

**Open questions** (defaults in bold)

1. The launch "security page": **`security/overview.md`, rewritten as one
   row per claim (scenario, smoke or lane, the operator setting that turns
   it off)**; `architecture/trust-boundaries.md` and
   `architecture/firecracker.md` stand in for the `runtime/isolation` page
   the scope names (none exists).
2. Claim detection: **a word list in `e2e/docs-claims.yaml` over the prose
   of every `shipped` page (list in IT-3.1; 153 of 207 pages hit it today);
   no opt-out key**, so a shipped page that says "jail" names a proof or is
   marked `stale`/`planned`. The lint proves a page names at least one
   active proof, not that each sentence is covered; per-claim coverage on
   the selling pages is review's job (their claim tables). Rejected: an
   author-set `claims: isolation` flag (the problem is overclaiming authors);
   a path list (claims are everywhere). Scenarios in `proves` on a shipped
   page **must be `active`**; `pending` is allowed only on `planned` pages.
3. Build pages (design open question 4): **keep 11 on the site
   (`rootfs` shipped; `overview`, `architecture`, `vamfile`, `directives`,
   `cli`, `templates`, `sources`, `backends`, `runtime-injection`,
   `compatibility` planned) and move 10 internal specs of the unbuilt
   pipeline to `design/vam-build/` (`hardening`, `reproducibility`,
   `optimization`, `supply-chain`, `oci`, `signing`, `security`, `errors`,
   `testing`, `invariants`) with redirects.** Moving deletes them from the
   site, so it **needs the owner's word**; without it, all 21 stay `planned`.
4. Docs lint in CI given `paths-ignore`: **a new `docs.yml` workflow on
   `docs/**`, `README.md`, `e2e/**`, `crates/vam-e2e/**`: `vam-e2e check`
   plus `pnpm build` (about 3 to 5 billed minutes per docs PR), required
   check `docs`; plus `repo_docs_are_honest` in the workspace tests for code
   PRs.** Rejected: dropping `docs/**` from `rust.yml`'s `paths-ignore`
   (every docs PR would run workspace tests, smoke and the lane filter,
   about 14 minutes).
5. `network/ebpf.md`: **folded into `network/overview.md`, page deleted
   with a redirect** (the eBPF decision covers every docs claim and the page
   is only that claim). If the owner prefers no deletion: the page becomes
   the short "why there is no kernel filter" note, `status: shipped`.
6. Honest page: **ship "What VAM is not for" now (IT-3.5); "where open
   models still lose" is a `planned` section until B1's open-model rows
   exist (H0's baseline has frontier models only)**.
7. `vam-firecracker`: **crate-level allow of the style and complexity
   groups with the written reason; correctness-group sites fixed** (59
   sites; the crate's future is in the dependency report).
8. Dependency report: **a new section of `design/isolation-truth.md`**;
   deferred bumps as rows with `Decide:` lines in this tracker's "Explicitly
   later".
9. Clippy toolchain: **pin the `clippy` job to the Rust version C6
   measures (1.97.0 today) via `dtolnay/rust-toolchain@<version>`; a Rust
   bump is its own small PR that fixes the new lints.** `rust-toolchain.toml`
   stays `stable` for builds. Default features only (`--all-targets`, no
   `--all-features`: the SDK's `_rt-*` features are alternatives). **Both
   feature sets** are linted (owner, C6): the default and
   `vam-runtime/process-guest,vam-orchestrator/process-guest`. The same gate
   is in `scripts/ci/local.sh --full` (not `--fast`).
10. Status vocabulary: **`stale` = the code exists but the page says more
    than it does, or is out of date; it shows an "Out of date" banner and
    needs no proof.** The field pass marks a page `stale` rather than
    rewriting it (owner: no rewrite outside the selling pages); rewriting
    stale pages is backlog, carried by any wave that touches them.
11. "Alpha" cautions: **removed on `planned` pages (the banner replaces
    them), kept on `shipped` and `stale` pages** this phase.
12. New scenarios that pass on the base land **`active`** (proofs of
    shipped behaviour); one that is red makes its claim `planned` and goes
    to the owner as a security-floor item.

**Coordination**

- *E10 Phase 3 (Steer), not planned yet.* Its build waves will edit
  `vam-runtime/src/agent.rs`, `vam-orchestrator/src/api/`, `brokers.rs`,
  `workflow.rs` and runtime and orchestrator docs. C4 and C5 hold while it
  has an open PR in those crates (conflict rule, `launch-0.1.0.md` §4); W6b
  edits only frontmatter in its docs pages.
- *H1 (harness).* open (W0: scenarios, one `vam-inference` file). C5's
  `vam-inference` part and D's `vam-inference` bumps hold until H1's open
  PRs in that crate merge.
- *Docs CI.* After W1, every lane's docs PR runs the `docs` job: a page that
  claims isolation without a proof fails there. The orchestrator tells the
  other lanes when W1 merges.

### Docs waves re-plan (owner, 2026-10-28) — superseded 2026-10-29

**Status: superseded** by "Docs plan: architecture rebuild and developer docs
(proposed 2026-10-29)" below: the owner did not approve it as written; its
inventory, gap lists and generator analysis are reused there, its items
IT-3.11 to IT-3.18 and its waves are withdrawn. **Was: proposed.** Drafted 2026-10-28 by the phase-breakdown agent on
`feat/isolation-truth` (W0 and W1 merged). On approval it replaces
items IT-3.3, IT-3.4, IT-3.5 and IT-3.7 and wave rows 2 to 6c above. Kept
unchanged: W0, W1, IT-3.1, IT-3.2 (eBPF removal), IT-3.6 (build pages: 11
behind the planned banner, 10 to `design/vam-build/`), IT-3.8 to IT-3.10, the
C1 to C6 and D rows (C1 now depends on the last docs wave, W7), locked
decisions 1 to 6, the `network/ebpf.md` fold and redirect.

**What the owner asked.** The old waves rewrote a few selling pages and set a
`status` field on about 155 others without reading them against the code.
The owner wants more: the Developer Docs tab must describe every shipped
feature so a developer can use it, and the Architecture tab must say what the
code does today. The rule for every shipped feature:

- **what it is**, in an architecture page;
- **how to use it**, in a developer page, with real CLI commands, API calls
  and config;
- **where to check it**, the scenario or test that proves it, enforced by
  the lint.

"A feature is built, and if it is not presented properly to the user and they
don't use it, building it is of no use."

**How the inventory was built.** From the code, not the docs: every `vam`
subcommand (`crates/vam-cli/src/main.rs:33` and the `cmd/*.rs` enums: 20
commands, 83 subcommands), every route in `api::router`
(`crates/vam-orchestrator/src/api/mod.rs:87` plus `threads::routes`,
`skill_package_routes` and the `/v1/runs/*rest` suffixes `events`,
`evidence`, `trajectory`, `receipt`, `cancel`; about 60 paths), every
`VAMD_*` and `VAM_*` variable read by the daemon and its crates (about 80),
the `vam-spec` run inputs (`harness.rs`), the 15 directories in `examples/`,
`brokers/vam-web`, and the 48 scenarios (47 `active`, 1 `pending`). Releases
read from the trackers: E1 Phases 0-7, E9 Phase 1-2, E10 Phases 0-2, E12
Phase 1, H0 and B0, telemetry-product, HSP, workspace files, isolation-truth
Phases 1-2.

**Feature inventory**

Proof column: an `active` scenario or a test path; "(lane)" means the
scenario needs the microVM lane; "not yet run" marks the three W0 scenarios
landed `active` but never run on the lane (CI billing). Developer page
column: yes, part (the page touches it but a developer could not use the
feature from it), no, or wrong. Paths are relative to `crates/vam-orchestrator/src/`
unless they name a crate; docs pages relative to `docs/src/content/docs/`.

*Install and node*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Start a node: state DB, listen address | config `--db-path` / `VAMD_DB_PATH`, `--listen` / `VAMD_LISTEN` | `vam-orchestrator/src/daemon.rs:457-470` | `plain-chat-gets-a-reply` (every scenario starts a `vamd`) | `getting-started/installation`: part (no flag list) | `orchestrator/daemon` |
| VM node refuses to start without its paths | config `VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH`, `VAM_KERNEL_PATH`, `VAMD_JAILER_BIN` | `daemon.rs`, `bin/vamd.rs` | `daemon.rs` unit tests (wave names the fn) | `macos-lima`, `coldstart-baseline`: part; `quickstart`: no (its `cargo run -p vam-orchestrator` no longer starts a VM node) | `orchestrator/daemon` |
| TLS on by default, operator cert, self-signed CA, plain-HTTP opt-out | config `VAMD_TLS_CERT`, `VAMD_TLS_KEY`, `VAMD_TLS_SANS`, `--insecure-dev`; CLI `vam config set-ca`, `VAM_CA_FILE` | `vam-orchestrator/src/tls.rs`, `vam-cli/src/transport.rs` | `vamd-refuses-plain-http-by-default`, `crates/vam-orchestrator/tests/tls_refusals.rs` | `quickstart`: part; `installation`: wrong (`:92` `http://`) | `orchestrator/daemon`, `orchestrator/api` |
| Health and node status (isolation line, `apiTls`, `rateLimits`) | CLI `vam health`, `vam node status`; API `GET /v1/health`, `GET /v1/node` | `api/mod.rs`, `isolation.rs` | `node-reports-the-isolation-it-runs` (lane) | `cli`: part (`:464` "warm-pool count": no pool exists) | `orchestrator/api` |
| Golden snapshot bake and restore | config `VAM_SNAPSHOT_RESTORE`, `VAM_RESEED_TIMEOUT_SECS`, `VAM_BAKE_READY_TIMEOUT_SECS`, `VAM_SNAPSHOT_ROOT` | `golden.rs`, `driver.rs` | `vm-restores-from-the-golden-snapshot` (lane) | `coldstart-baseline`: yes | `architecture/cold-start`, `core/snapshot`, `orchestrator/pool` |
| Idle sessions end | config `VAMD_SESSION_IDLE_TTL_SECS` | `ttl.rs` | `ttl.rs` unit tests | no | `orchestrator/daemon` |
| Example seeds | config `VAMD_EXAMPLES_DIR` (alias `VAM_EXAMPLES_DIR`) | `daemon.rs`, `manager.rs` | none | no (the quickstart depends on it) | `orchestrator/daemon` |
| Runs and threads survive a `vamd` restart | behaviour | `reconcile.rs`, `run_checkpoint.rs` | `follow-up-continues-after-vamd-restart`, `work-survives-vamd-restart-mid-task` | no | `orchestrator/reconciliation` |
| Session operations | CLI `vam session create/ls/get/pause/resume/terminate/snapshot/restore`; API `/v1/sessions…` | `api/mod.rs`, `manager.rs` | `crates/vam-orchestrator/tests/api_tests.rs` | `cli`: yes | `orchestrator/session`, `orchestrator/lifecycle` |
| Mac and Windows hosts | scripts `start-vamd-lima.sh`, `build-rootfs-lima.sh`, `fetch-guest-kernel-lima.sh` | `scripts/` | none (manual) | `macos-lima`, `windows-wsl`: yes | none |
| Process mode: a test build that isolates nothing | build feature `process-guest`, `VAMD_PROCESS_GUEST_BIN` | `process_mode.rs` | `crates/vam-orchestrator/tests/process_mode_bin.rs`; release gate (`check-release-gate.sh`) | no (belongs on the honest page) | `architecture/deployment-model` |

*Auth and users*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Bootstrap key and first admin | config `VAMD_BOOTSTRAP_API_KEY`, `VAMD_BOOTSTRAP_ADMIN`, `VAMD_BOOTSTRAP_ADMIN_PASSWORD`, `--bootstrap-admin`, `--reset-password` | `daemon.rs`, `users.rs` | `crates/vam-orchestrator/tests/login_tests.rs` | `users-and-roles`: yes | `orchestrator/security` |
| Login, refresh, logout, who am I | CLI `vam login`, `vam logout`; API `/v1/login`, `/v1/login/refresh`, `/v1/logout`, `/v1/me` | `api/login.rs`, `api/me.rs` | `login_tests.rs`, `me_tests.rs` | `cli`: yes | `orchestrator/security` |
| Users and roles; deactivate, password reset, publish grant, owner reassign | CLI `vam users create`; API `/v1/users…`, `/v1/catalog/reassign-owner` | `api/users_admin.rs`, `api/publish_grant.rs`, `api/catalog_owner.rs` | `member-chats-with-a-released-agent` | `users-and-roles`: part (API-only actions not shown) | `orchestrator/user-isolation` |
| API keys, own and per user | CLI `vam keys create/list/revoke`; API `/v1/keys`, `/v1/users/:id/keys` | `keys.rs`, `api/users_keys_admin.rs` | `api_tests.rs` | `cli`: yes | `orchestrator/security` |
| One user cannot see another's runs (cross-user 404) | API behaviour | `owner.rs`, `api/auth.rs` | `crates/vam-orchestrator/tests/isolation_coverage_map_tests.rs` | no | `orchestrator/user-isolation` |
| API rate limits, on by default | config `VAMD_RATE_LIMIT` (`off`), `VAMD_RATE_LIMIT_RPS`/`_BURST`/`_CREATE_*`/`_IP_*`; 429 with `Retry-After` | `api/rate_limit.rs` | `api-rate-limits-a-flood` | no | none |
| List paging | API `pageSize`, `pageToken`, `pagination.nextPageToken`; CLI `--all` | `api/paging.rs` | `crates/vam-orchestrator/src/paging_store_tests.rs` | `cli`: part (`--all` only) | `orchestrator/api` |
| Scripting contract: `--json`, exit codes, no prompts | CLI global flags | `vam-cli/src/output.rs` | `crates/vam-cli/tests/cli_tests.rs` | `cli`: yes | none |

*Agents and specs*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Agent spec: put, get, list, draft, release, versions, suggested version | CLI `vam agent put/get/ls/release/versions/suggest-version`; API `/v1/agents…`, `…/release-suggestion` | `catalog*.rs`, `api/catalog_release.rs` | `released-agent-approvals-are-once-only`, `api_tests.rs` | `authoring/agent-spec`, `authoring/model`: yes | `spec/agentic-spec` |
| URN origin and reserved domains | config `VAMD_REGISTRY_DOMAIN`, `VAMD_MARKETPLACE_DOMAIN` | `registry.rs`, `catalog_tier.rs` | `api_tests.rs` | `authoring/model`: yes | `orchestrator/daemon` |
| Agent check | CLI `vam agent check`; API `POST /v1/agent-checks` | `api/mod.rs` | `crates/vam-orchestrator/tests/agent_check_tests.rs`, `crates/vam-cli/tests/agent_check_cli.rs` | `cli`: part | none |
| Permissions as data: tool and internet rules, presets, effective view, explain, migrate | CLI `vam agent permissions get/set/web/presets`, `vam agent migrate`, `vam policy explain`; API `/v1/agents/{urn}/permissions…`, `/v1/permission-presets` | `vam-security/src/permissions.rs`, `api/permissions.rs`, `permissions_view.rs` | `always-allow-and-always-deny-are-remembered`, `crates/vam-orchestrator/tests/permissions_api_tests.rs` | `authoring/agent-spec`: part | `security/cedar`, `security/agent-autonomy-and-governance` |
| Cedar policies (legacy `capabilityPolicy`, `policy.cedar`) | CLI `vam policy put/get/ls/release/versions`; spec field | `vam-security/src/pdp.rs`, `policy_legacy.rs` | `crates/vam-security/tests/pdp_tests.rs`, `legacy_permissions_tests.rs` | `security/cedar-and-hitl`: stale (written as the primary model) | `security/cedar` |
| Allowed models (operator list) | API `/v1/node/allowed-models` | `allowed_models.rs` | `crates/vam-orchestrator/tests/allowed_models_tests.rs` | no | `runtime/inference` |
| A workspace pins its agent version | behaviour | `session_targets.rs` | `crates/vam-orchestrator/tests/workspace_pin_tests.rs` | no | `orchestrator/session` |
| Run spec and `vam run` (name, URN or file; prompt; workspace and thread) | CLI `vam run`, `-p`, `--file`, `--workspace`; API `POST /v1/runs` | `vam-cli/src/cmd/run.rs`, `api/mod.rs` | `plain-chat-gets-a-reply`, `follow-up-uses-earlier-tool-results` | `authoring/run-spec`, `cli`: yes | `spec/workflow` |
| Dry run | CLI `vam run --dry-run` | `vam-cli/src/cmd/dry_run.rs` | `cli_tests.rs` | `cli`: yes | none |
| Harness switches and profiles (loop detection, todo recitation, repair budget) | run input `inputs.harness` | `vam-spec/src/harness.rs` | `harness-switches-print-on-the-receipt`, `repeated-identical-call-is-caught-and-observed`, `todo-is-recited-each-turn-when-enabled` | no | `runtime/agent`: part |
| Context compaction | agent context policy | `vam-runtime/src/compaction.rs` | `compaction-keeps-an-early-correction`, `follow-up-after-compaction` | no | `runtime/context` |
| Instruction layers (`.vam/SYSTEM.md`, template) | package files | `vam-runtime/src/instruction.rs`, `primitives.rs` | `primitives.rs` unit tests (`is_protected_write_path`) | `authoring/agent-package`: yes | `runtime/instruction-template` |

*Skills*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Skill package (`SKILLS.md`, `manifest.json`, executor) | directory layout | `skill_package_admit.rs:233-291` | `host-tool-output-fits-frame` (publishes and releases a package) | `authoring/skill-manifest`: yes, but titled `SKILL.md`; the code reads `SKILLS.md` only | `spec/skills` |
| Publish, release and version a skill | CLI `vam skill publish/put/get/ls/release/versions/suggest-version`; API `/v1/skills/packages…` | `skill_packages.rs`, `api/skill_packages.rs` | `api_tests.rs` | `authoring/publish-skill`: yes | `spec/skills` |
| `load_skill` and `INDEX.md` | built-in tool | `vam-runtime/src/skills.rs`, `skill_index.rs` | `skill_index.rs` unit tests | `authoring/skill-manifest`: yes | `runtime/agent` |
| Required secrets and declared capabilities | manifest fields | `secret_requirements.rs` | `vault-secret-is-not-in-the-guest` (lane) | `authoring/skill-manifest`: yes | `hosted/capability-manifest` |

*Tools*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Built-in tools: `read_file`, `write_file`, `edit_file`, `delete_file`, `glob`, `grep`, `run`, `todo_write`, `load_skill` | agent tools | `vam-runtime/src/primitives.rs` | `thread-history-lists-tool-results`, `tool-call-first-try-saves-a-draft` | no | `runtime/primitives` |
| Typed error for bad tool arguments | behaviour | `vam-spec/src/call_check.rs` | `invalid-tool-arguments-return-a-typed-error` | no | `runtime/errors` |
| Custom in-VM tools (stdin/stdout JSON, runtime templates) | skill executor | `vam-runtime/src/exec.rs` | none (examples only) | `tools/writing-tools`, `tools/tool-runtimes`: yes | `runtime/tools` |
| Tool process confinement: setuid 1000, env scrub, Landlock where the guest kernel has it (x86_64 only) | behaviour | `exec.rs:630-711`, `landlock.rs` | none | `tools/writing-tools` claims it | `runtime/tools` |
| Host-tool output bound (60 KiB, truncation note) | behaviour | `host_tools.rs` | `host-tool-output-fits-frame` | no | `runtime/host-tools` |
| MCP connections (remote servers, OAuth tokens, refresh) | CLI `vam mcp connect/update/list/disconnect/refresh`; API `/v1/mcp/connections…` | `mcp_registry.rs`, `mcp_client.rs`, `mcp_oauth_refresh.rs` | `crates/vam-orchestrator/tests/mcp_connections_tests.rs` | no (`examples/linear-mcp-assistant/README.md` only) | `runtime/mcp` |

*Playbooks*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Playbook spec: put, release; local `init` and `explain` | CLI `vam playbook put/get/ls/release/versions/init/explain` | `playbooks/`, `vam-cli/src/cmd/playbook_local.rs` | `crates/vam-cli/tests/playbook_put_cli.rs`, `playbook_examples_tests.rs` | `authoring/playbook-spec`, `playbook-tutorial`: yes | `spec/workflow`, `runtime/workflow` |
| Activation: start by itself or ask first | spec `when`, activation mode | `playbook_activation.rs` | `matching-request-starts-the-playbook`, `greeting-starts-no-playbook`, `injected-text-cannot-start-a-playbook` | `authoring/playbook-spec`: yes | `runtime/workflow` |
| Playbook run with inputs | CLI, API `POST /v1/runs` | `vam-spec/src/playbook_inputs.rs` | `playbook-run-starts-with-inputs` | `authoring/playbook-tutorial`: yes | `spec/workflow` |
| Gates and guards | spec steps | `workflow.rs` | `gate-decided-finishes-the-run`, `gate-offers-its-options-and-stop-ends-the-run`, `receipt-shows-denial-before-approval`, `vm-run-waits-at-a-gate-and-finishes` (lane) | `authoring/playbook-spec`: yes | `spec/hitl`, `runtime/workflow` |
| Playbook tests (scripted observations, pass^k) | CLI `vam playbook test --repeat`; API `POST /v1/playbook-tests` | `playbook_test.rs` | `crates/vam-cli/tests/playbook_test_cli.rs` | `authoring/playbook-tutorial`: yes | none |

*Approvals*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Tool approval: list, inspect, approve, reject, amend | CLI `vam approve ls/get/decide`; API `/v1/approvals…` | `approvals/`, `hitl.rs` | `approval-suspends-and-resumes` | `security/hitl-lifecycle`: yes | `runtime/hitl`, `spec/hitl` |
| Always allow, always deny | CLI `vam approve decide --always allow\|deny` | `vam-cli/src/cmd/approve.rs:130`, `session_targets.rs` | `always-allow-and-always-deny-are-remembered` | no | `security/agent-autonomy-and-governance` |
| A released agent's approvals are once only | behaviour | `session_targets.rs` | `released-agent-approvals-are-once-only` | no | `security/agent-autonomy-and-governance` |
| Parked approvals survive a restart; long threads resume | behaviour | `run_checkpoint.rs`, `vam-runtime/src/hitl_checkpoint.rs` | `parked-approval-survives-a-restart`, `long-thread-resumes-after-approval` | no | `runtime/hitl` |
| Approval timeout | config `VAMD_TOOL_HITL_TIMEOUT_SECS`, `--tool-hitl-timeout-secs` | `daemon.rs` | none | no | `spec/hitl` |

*Secrets*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Vault: set, list, get, delete; kinds; org and user scope | CLI `vam secrets set/list/get/delete --scope`; API `/v1/secrets…`, `/v1/org/secrets…` | `secrets.rs`, `api/org_secrets_admin.rs` | `api_tests.rs` | `security/secret-management`: yes | `security/key-management` |
| Vault master key | config `VAMD_SECRETS_MASTER_KEY`, else a 0600 key file beside the DB (`/var/lib/vam/secrets.key`) | `secrets.rs:17-18`, `:1183-1195` | `secrets.rs` unit tests | `security/secret-management`: wrong (`:182-192` names `VAMD_SECRET_MASTER_KEY` and an ephemeral key) | `security/key-management` |
| Placeholders substituted at the host proxy; the guest never holds the value | `vamsec_…` placeholders | `vam-network/src/secrets.rs` | `vault-secret-is-not-in-the-guest` (lane, not yet run) | `integrations/egress-proxy-deepdive`: yes | `security/secrets-proxy` |
| Encoded forms of a secret redacted from tool output | behaviour | `vam-security/src/secret_forms.rs` | `secret-never-reaches-guest-encoded` | no | `security/pii-redaction` (stale) |
| Import secrets from the environment at start | config `VAMD_IMPORT_ENV_SECRETS` | `daemon.rs` | none | `security/secret-management`: yes | `orchestrator/security` |

*Egress*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Guest proxy binds loopback only | behaviour | `vam-runtime/src/egress.rs` | `guest-proxy-never-binds-wildcard` (lane) | no | `network/overview` |
| The guest has no route; the host makes every connection | behaviour | guest init, `manager.rs` (brokered sessions skip TAP and netns) | `guest-has-no-route-off-the-vm` (lane, not yet run) | no | `network/overview` |
| Per-agent internet rules; **no `internet` setting resolves to allow** | spec `permissions.internet` | `vam-security/src/permissions.rs:274-280` | `egress-refuses-an-ungranted-host` is **pending** (not citable); `permissions.rs` and `vam-network/src/gate.rs` unit tests | `authoring/agent-spec:312`: says so | `network/policy` |
| IP floor: private, loopback and metadata addresses refused | behaviour | `vam-security/src/ssrf_guard.rs`, `vam-network/src/gate.rs` | `ssrf_guard.rs`, `gate.rs` unit tests | no | `network/policy` |
| HTTPS terminated with a per-session CA, so placeholders work over TLS | API `GET /v1/sessions/:id/egress-ca` | `vam-network/src/tls_ca.rs`, `proxy/mitm.rs` | `crates/vam-network/src/proxy/mitm_tests.rs` | `integrations/egress-proxy-deepdive`: part | `security/secrets-proxy` |
| Passthrough hosts and response cap | config `VAMD_EGRESS_PASSTHROUGH_HOSTS`, `VAMD_EGRESS_MAX_BYTES` | `daemon.rs`, `vam-network/src/proxy/` | none | no | `network/policy` |

*Receipts and evidence*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Signed receipt per run (host facts apart from model text; isolation, rate limits, harness) | CLI `vam receipt show`; API `GET /v1/runs/{id}/receipt` | `receipt.rs`, `vam-spec/src/receipt.rs` | `receipt-separates-host-facts-from-model-text`, `receipt-shows-denial-before-approval`, `receipt-names-who-decided-and-from-where`, `receipt-names-the-isolation-the-run-had` (lane) | no (`playbook-tutorial` §7 only) | `security/run-receipt` |
| Verify a receipt offline | CLI `vam receipt verify` | `vam-cli/src/cmd/receipt.rs`, `vam-telemetry/src/evidence.rs` | `receipt-verifies-offline-and-fails-on-tamper`, `crates/vam-cli/tests/receipt_cli.rs` | no | `security/run-receipt` |
| Node signing keys | CLI `vam node keys` | `signing.rs` | `crates/vam-orchestrator/tests/node_signing_keys_tests.rs` | no | `security/key-management` |
| Evidence pack | CLI `vam evidence export/verify/receipt`; API `/v1/runs/{id}/evidence` | `evidence.rs` | `receipt-states-effects-not-journaled-before-journal`, `evidence_pack_test.rs` | `cli`: yes | `telemetry/audit-ledger` |

*Workspaces and storage*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Workspaces and threads (multi-turn) | CLI `vam thread ls/show`, `vam run --workspace`; API `/v1/workspaces/:id/threads…` | `api/threads.rs`, `vam-storage/src/thread_log.rs` | `thread-history-lists-tool-results`, `crates/vam-orchestrator/tests/thread_api_tests.rs` | `tutorials/bun-assistant`: part; `vam thread`: no | `storage/turn-commits` |
| Workspace files: list, read, upload | CLI `vam workspace ls/cat/put`; API `/v1/workspaces/:id/files` | `api/workspace_files.rs` | `workspace_files_tests.rs` | `cli`: yes | `orchestrator/workspace-files`, `-upload` |
| Turn commits: files match the conversation after a stop | behaviour | `turn_commit.rs` | `workspace-matches-conversation-after-vm-stop` (lane), `follow-up-continues-after-stop` | no | `storage/turn-commits` |
| Each agent's workspace is separate | behaviour | `vam-storage/src/owner_prefix.rs` | `agent-workspaces-are-isolated` | no | `storage/architecture` |
| Thread log quota and GC | behaviour | `vam-storage/src/log_quota.rs`, `gc.rs` | unit tests | no | `storage/quota-gc` |

*Inference*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Host-brokered providers (OpenAI-compatible incl. OpenRouter and vLLM, Anthropic, Vercel gateway) | secret kind `inference`; run inputs `model`, `model_provider` | `vam-inference/src/provider.rs`, `catalog.rs` | `provider.rs` unit tests | `quickstart`: one line | `runtime/inference` |
| Prompt caching, usage and cost on the run | run events | `vam-inference/src/cache.rs`, `budget.rs` | `crates/vam-e2e/tests/usage_recorded.rs` | no | `runtime/inference`, `telemetry/cost-accounting` |
| Mock model (tests) | config `VAMD_E2E_MOCK_SCRIPTS` | `mock_script.rs` | every process scenario | no (contributor) | `runtime/testing` |
| Benchmark runner | `vam-e2e run --bench` | `vam-e2e/src/bench.rs` | `model-task-runs-k-trials-on-the-mock`, `bench-trial-starts-from-a-clean-thread` | no (contributor, `e2e/README.md`) | `runtime/benchmark-results` |

*Telemetry*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Run events, trajectory, compare | CLI `vam run events/trajectory/compare`; API `/v1/runs/{id}/events`, `/trajectory` | `events.rs`, `trajectory.rs` | `trajectory_timeline_test.rs`, `trajectory_replay_test.rs` | `cli`: part | `telemetry/event-catalog` |
| OTLP export | config `VAMD_OTLP_ENABLED`, `_ENDPOINT`, `_TLS_*`, `_SUCCESS_SAMPLE_RATE` | `vam-telemetry/src/otlp.rs` | `standards_egress_test.rs` (wave confirms the fn) | no | `telemetry/otel` |
| CloudEvents and OpenLineage webhooks | config `VAMD_CLOUDEVENTS_URL`, `VAMD_OPENLINEAGE_URL` | `egress.rs`, `vam-telemetry/src/lineage.rs` | `standards_egress_test.rs` | no | `telemetry/cloudevents` |
| gRPC telemetry stream (loopback, no TLS) | config `VAMD_TELEMETRY_LISTEN` | `api/telemetry_stream.rs` | `telemetry_stream_test.rs` | `macos-lima`: mention | `telemetry/streaming` |
| Audit spool, retention, tenant | config `VAMD_SPOOL_DIR`, `VAMD_SPOOL_MAX_*`, `VAMD_TENANT_ID` | `vam-telemetry/src/queue.rs`, `retention.rs` | `audit_fail_closed_test.rs` | no | `telemetry/audit-ledger`, `telemetry/retention` |
| Timing lines | config `VAM_TIMING`, `VAM_TIMING_STDERR_JSON` | `timing.rs` | none | `coldstart-baseline`: yes | `telemetry/latency` |

*Hosted brokers*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| Register a hosted broker (HSP over UDS gRPC, manifest, hooks, trust tier) | API `PUT /v1/hosted/brokers`; proto `hosted_services.proto` | `hsp*.rs` | `hsp_uds.rs`, `hsp_invoke.rs`, `hsp_hooks.rs`, `hsp_manifest.rs`, `hsp_trust.rs` | no | `hosted/overview`, `protocol`, `hooks`, `capability-manifest` |
| Web search and fetch (`vam.web`: search provider, guarded fetch, DNS pinned per hop, robots) | tools `web_search`, `web_fetch`; `scripts/run-web-broker.sh` | `brokers/vam-web` | `hsp_web.rs`, `hsp_web_sources.rs`, `brokers/vam-web/src/fetch/network/tests.rs`, `brokers/vam-web/tests/env_isolation.rs` | no | `hosted/web` |
| TypeScript broker examples (echo, files) | `examples/hsp-echo-ts`, `examples/hsp-files-ts` | examples | none (READMEs) | no | `hosted/first-party` |
| Broker and web text fenced as untrusted | behaviour | `vam-security/src/vendor_text.rs` | `injected-text-cannot-start-a-playbook`; `vendor_text.rs` unit tests | no | `security/threat-model` |

*Isolation*

| Feature | Surface | Code | Proof | Developer page (today) | Architecture page |
|---|---|---|---|---|---|
| A Firecracker microVM per session, control over vsock | behaviour | `vam-core/src/vmm.rs`, `vam-vsock` | `vm-boots-and-registers-over-vsock` (lane) | `introduction`: claims it | `architecture/firecracker` |
| Jailer on by default: chroot, uid 65400, own PID and mount namespaces, cgroup v2, Firecracker's built-in seccomp | config `--no-jailer` / `VAMD_USE_JAILER`, `VAMD_JAILER_UID`/`_GID`, `VAMD_JAILER_CGROUP_VERSION` | `vam-core/src/jail.rs` | `jailer-confines-the-vmm-process` (lane; seccomp step not yet run), `crates/vam-core/tests/jailed.rs` | no | `core/jailer` |
| Node and receipt report the isolation they ran | — | `isolation.rs` | `node-reports-the-isolation-it-runs`, `receipt-names-the-isolation-the-run-had` (lane) | no | `security/run-receipt` |
| Control channel survives oversize and hostile frames | behaviour | `vam-vsock/src/frame.rs` | `host-tool-output-fits-frame`, `crates/vam-orchestrator/tests/adversarial_vsock_tests.rs` | no | `spec/vsock-rpc`, `core/vsock-device` |
| No panics in the daemon's request paths | behaviour | `cfg_attr(not(test), deny(unwrap_used, expect_used))` on the lib | none (a lint attribute, not a test) | no | `orchestrator/errors` |

**Counts**

| Area | Features | With a proof | Developer page yes | part | no or wrong |
|---|---|---|---|---|---|
| Install and node | 11 | 9 | 3 | 4 | 4 |
| Auth and users | 8 | 8 | 4 | 2 | 2 |
| Agents and specs | 12 | 12 | 5 | 2 | 5 |
| Skills | 4 | 4 | 4 | 0 | 0 |
| Tools | 6 | 4 | 1 | 0 | 5 |
| Playbooks | 5 | 5 | 5 | 0 | 0 |
| Approvals | 5 | 4 | 1 | 0 | 4 |
| Secrets | 5 | 4 | 3 | 0 | 2 |
| Egress | 6 | 5 | 0 | 2 | 4 |
| Receipts and evidence | 4 | 4 | 1 | 0 | 3 |
| Workspaces and storage | 5 | 5 | 1 | 1 | 3 |
| Inference | 4 | 4 | 0 | 1 | 3 |
| Telemetry | 6 | 5 | 1 | 2 | 3 |
| Hosted brokers | 4 | 3 | 0 | 0 | 4 |
| Isolation | 5 | 4 | 0 | 0 | 5 |
| **Total** | **90** | **80** | **29** | **14** | **47** |

Four of the 80 proofs are weaker than they look: three `active` microVM
scenarios never ran on the lane (`guest-has-no-route-off-the-vm`,
`vault-secret-is-not-in-the-guest`, the seccomp step of
`jailer-confines-the-vmm-process`), and the egress row rests on unit tests
because `egress-refuses-an-ungranted-host` is `pending`.

**Gaps: shipped features with no usable developer page** (47 rows; the ones
a 0.1.0 user meets first): the receipt and `vam receipt show/verify`, `vam node keys`
(launch demo); the security settings an operator can turn off (jailer,
`--insecure-dev`, `VAMD_RATE_LIMIT`, idle TTL): no page lists them together,
and rate limits are on **no page at all**, developer or architecture; the
built-in tools (no page lists the nine tools an agent has); harness switches
and context compaction; always allow / always deny (`--always`); MCP
connections (`vam mcp`); the web broker (`web_search`, `web_fetch`) and
hosted brokers; telemetry export (OTLP, CloudEvents, OpenLineage, gRPC
stream, spool); allowed models; `vam thread`; paging; workspace pinning;
restart recovery; per-user isolation; encoded-secret redaction; the egress
IP floor and per-session CA; example seeds (`VAMD_EXAMPLES_DIR`, which the
quickstart silently needs).

**Gaps: developer pages describing features that do not exist** (or exist
differently):

- `getting-started/platform-tour.md:46,57,61`: eBPF evaluation, a network
  namespace per session, Drive9 mounted "using FUSE" and synced to cloud.
- `getting-started/workspace.md:57-59,67-76`: SPIFFE, "POSIX FUSE
  filesystem, multi-cloud S3 adapters", "eBPF TC firewalls", "Kernel-level
  egress filtering: Fully Supported".
- `getting-started/installation.md:92` `host = "http://127.0.0.1:8080"`
  (TLS since Phase 1); `:142` "Warm Pool: 4 ready microVMs" (no pool).
- `getting-started/cli.md:464` "ready warm-pool count".
- `getting-started/quickstart.md:25-27` `cargo run -p vam-orchestrator` (a VM
  node now refuses to start without `VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH`,
  `VAMD_JAILER_BIN`, and loads no examples without `VAMD_EXAMPLES_DIR`).
- `getting-started/project-status.md:19-30` (already in the overclaims list).
- `security/secret-management.md:182-192`: `VAMD_SECRET_MASTER_KEY` and "an
  ephemeral in-memory key"; the code reads `VAMD_SECRETS_MASTER_KEY`
  (`secrets.rs:17`) and otherwise keeps a 0600 key file beside the DB
  (`secrets.rs:18`, `:1183-1195`).
- `authoring/skill-manifest.md` (sidebar "Skills & Frontmatter (SKILL.md)"):
  the code reads `SKILLS.md` only (`skill_package_admit.rs:233-291`,
  `vam-runtime/src/agent.rs:3139`); `SKILL.md` is a launch "build now" row
  not built yet.
- `security/cedar-and-hitl.md`: Cedar as the way to govern tools; since E1
  Phase 5 permissions-as-data is the model and Cedar is the legacy path
  (`policy_legacy.rs`). "Filesystem Sandbox Rules" to be checked by the wave.
- `index.mdx:6,41`: "zero-trust egress", "eBPF networking".
- Sidebar group "Zero-Trust Integrations" (`astro.config.mjs`).

**Gaps: security claims that need a proof and have none**

- Tool processes run as uid 1000 with a scrubbed environment and Landlock
  (`vam-runtime/src/exec.rs:630-711`; `landlock.rs` is x86_64 only, degrades
  silently, and carries `#![allow(dead_code)]` at `:5`). `tools/writing-tools.md`
  claims it. No scenario. Cheapest proof: a process-layer scenario whose shell
  prints `id -u` and fails to write outside the workspace (process guests run
  the same `exec.rs`), or a guest unit test; until then the claim is
  rewritten as unproven.
- The three W0 microVM scenarios: `active`, never run on the lane. Cited as
  proofs (the lint accepts them), shown on the security page as "runs on the
  microVM lane; first lane run pending" until a lane run is green.
- Egress gate at run level: `egress-refuses-an-ungranted-host` is `pending`
  (deny events filed under the session id). The security page cites the gate
  unit tests and lists the run-level proof as not yet proven.
- Agent internet default: no `internet` setting resolves to **allow**
  (`vam-security/src/permissions.rs:277-279`). Not a proof gap but a claim
  the docs must make plainly (security page, egress page, permissions page);
  backlog row exists.
- Approval timeout and the 1 h approval expiry: unit tests only, no scenario.
- No panics in request paths: a `deny` lint attribute, not a test. Cited as
  the attribute's file, or left without a proof row.
- Egress responses over the proxy's 8 MiB scrub buffer stream unscrubbed
  (Phase 2 backlog); the gRPC telemetry listener has no TLS (backlog). Both
  must be stated on the security page, not hidden.
- vamd's `/docs` page loads Scalar from jsdelivr with no version and no SRI
  (`api/mod.rs:274-278`): third-party script on the node's own origin. Not an
  isolation claim; carried into W4d (`Decide: BACKLOG — proof 1, cost 0,
  carry into W4d which edits that handler`).

**Developer docs target** (the Developer Docs tab after the phase: 28 pages
today, 39 proposed, plus the cheat sheet in the third tab)

| Page | Action | What it covers |
|---|---|---|
| `getting-started/introduction` | rewrite | What VAM is in one page; isolation in one paragraph linking the security page |
| `getting-started/quickstart` | rewrite | Linux plus KVM, from build to a signed receipt: the VM paths, TLS and `set-ca`, a model key, `VAMD_EXAMPLES_DIR`, one chat, one approval, `vam receipt verify`; every command run in the wave |
| `getting-started/installation` | rewrite | Build or install `vam` and `vamd`; host requirements; TLS host URL; remote CLI |
| `getting-started/macos-lima` | keep | Refresh commands; status and proof |
| `getting-started/windows-wsl` | keep | Same; `stale` if the wave cannot confirm the steps |
| `getting-started/users-and-roles` | keep | Add deactivate, password reset, publish grant, per-user isolation |
| `getting-started/limits` | new | The honest page, "What VAM is not for" (old IT-3.5) |
| `getting-started/concepts` | rewrite of `platform-tour` | Agent, skill, playbook, run, workspace, thread, session, approval, receipt; no eBPF, netns or FUSE |
| `getting-started/project-status` | rewrite | Crate by crate what runs, pointing at the security page and the feature register |
| `getting-started/cli` | rewrite, renamed "CLI guide" | Tasks by command group; add `vam mcp`, `vam thread`, `vam receipt`, `vam node keys`, `--always`, paging; the flag list moves to the generated reference |
| `getting-started/coldstart-baseline` | move to the Architecture tab (sidebar only; URL kept) | Measurements are architecture, not use |
| `getting-started/workspace` | move to the Architecture tab as "Repository layout" (sidebar only) | For contributors; its eBPF, FUSE and S3 lines rewritten |
| `operate/configuration` | new (group "Operate a node") | Every `vamd` flag and variable: default, what it does, and which ones loosen a secure default and are reported by the node and receipt (jailer, `--insecure-dev`, rate limits, idle TTL, egress passthrough); VM paths; TLS; master key; example seeds; restart recovery |
| `operate/telemetry-export` | new | OTLP, CloudEvents, OpenLineage, gRPC stream (loopback, no TLS), spool and retention, timing lines |
| `authoring/model` | keep | |
| `authoring/agent-package` | keep | |
| `authoring/agent-spec` | update | Fields as the proto has them; allowed models; link the permissions page |
| `authoring/permissions` | new | Tool and internet rules, presets, effective view, explain, migrate, always allow/deny; **no `internet` setting means allow**; Cedar as the legacy path (absorbs `security/cedar-and-hitl`) |
| `authoring/skill-manifest` | rewrite | `SKILLS.md` named as the code reads it; title fixed |
| `authoring/publish-skill` | keep | |
| `authoring/run-spec` | update | Model pin, workspace and thread, compaction |
| `authoring/harness` | new | The `inputs.harness` switches and profiles, what the receipt prints |
| `authoring/playbook-spec` | keep | |
| `authoring/playbook-tutorial` | keep | |
| `tools/builtins` | new | The nine built-in tools, typed argument errors, the 60 KiB host-tool bound, loop detection |
| `tools/writing-tools` | rewrite | The tool protocol; the confinement claim cut to what is proven |
| `tools/tool-runtimes` | keep | |
| `tools/mcp` | new | `vam mcp connect/update/list/disconnect/refresh`, OAuth tokens, the Linear example |
| `integrations/egress` | rewrite of `egress-proxy-deepdive` (redirect) | How a tool reaches the internet: the guest proxy, placeholders, per-session CA, internet rules and their default, the IP floor, passthrough hosts |
| `integrations/web` | new | Run the web broker, give an agent `web_search` and `web_fetch`, what the fetch refuses |
| `integrations/hosted-brokers` | new | Write and register an HSP broker (TypeScript echo and files examples) |
| `integrations/jira-assistant-walkthrough` | keep | Checked against `examples/jira-assistant` |
| `security/overview` | sidebar: first item of "Security & Governance" (URL unchanged) | The launch security page (W3) |
| `security/approvals` | rewrite of `hitl-lifecycle` (redirect) | Ask, decide, amend, always allow/deny, once-only for released agents, timeout, survives restart |
| `security/secret-management` | rewrite | Vault, scopes, kinds, the real master-key variable and key file, encoded-form redaction |
| `security/receipts` | new | `vam receipt show/verify`, `vam node keys`, what the receipt says and does not, the evidence pack |
| `tutorials/jira-assistant` | keep | |
| `tutorials/bun-assistant` | keep | Add `vam thread` |
| `reference/api` | new, generated (W4d) | From `docs/api/openapi.yaml` |
| `reference/cli` | new, generated (W4d, if approved) | From the clap definitions |
| `reference/specs` | new, generated (W4d, if approved) | `AgentSpec`, `RunSpec`, `PlaybookSpec`, `SkillManifest`, receipt, harness switches |
| `reference/agent-cheat-sheet` | keep (third tab) | Refreshed against the generated references |

Sidebar changes: "Getting Started" loses two moved pages and gains
`limits`; new group "Operate a node"; "Zero-Trust Integrations" renamed
"Integrations"; "In-VM Tool Development" renamed "Tools"; `security/overview`
listed first under "Security & Governance"; new group "Reference". Every
rename keeps the old URL or adds an Astro redirect. **Questions for the
owner (deletes)**: `security/cedar-and-hitl` merged into
`authoring/permissions` and deleted with a redirect; `getting-started/coldstart-baseline`
and `architecture/cold-start` overlap (merge into the latter, redirect);
`integrations/jira-assistant-walkthrough` and `tutorials/jira-assistant`
overlap (default: keep both).

**The generated reference (open question 1).** There is more than the brief
assumed: **an OpenAPI file exists.** `docs/api/openapi.yaml` (5,172 lines,
OpenAPI 3.1, hand-written) is compiled into `vamd` (`api/mod.rs:71`) and
served at `/openapi.yaml`, with a Scalar UI at `/docs` (`api/mod.rs:91-92`);
`api_tests.rs:3366` checks it is reachable. It has drifted: no
`/v1/runs/{id}/receipt`, `/evidence`, `/trajectory`, `/v1/agent-checks`,
`/v1/playbook-tests`, `/v1/workspaces/{id}/files`, the permissions set and
migrate paths, or release on agents, playbooks and policies; `servers` is
`http://127.0.0.1:8080` (`openapi.yaml:10`); no `rateLimits` or `apiTls`. No
test compares it with the router. Proposal, cheapest first:

1. **API** (default, W4d): `openapi.yaml` stays the source. A drift test
   (`openapi_lists_every_route`, router paths against spec paths, both ways)
   and the missing paths added; the Starlight `starlight-openapi` plugin
   renders it as `reference/api`; the `/docs` Scalar script pinned with SRI.
   The drift test runs in `docs.yml` too, since an `openapi.yaml`-only PR
   skips `rust.yml`. Rejected for 0.1.0: `utoipa` annotations on every
   handler (the generated-from-code route; a rewrite of about 13,000 lines
   of `api/`, cost 3).
2. **CLI**: `clap-markdown` (or a hidden `vam __markdown`) writes
   `reference/cli.md`; a test fails when the committed file differs.
3. **Specs**: a `vam-spec` example `generate_reference` walks
   `DESCRIPTOR_POOL` (prost-build keeps source info, so field comments come
   through; the wave confirms) for the JSON formats users write
   (`AgentSpec`, `RunSpec`, `PlaybookSpec`, `SkillManifest`, receipt), plus
   the harness switch table from `vam_spec::harness`; a freshness test like
   the descriptor baseline's.
4. **Proof index** (part of IT-3.11): the feature register below renders a
   "Where to check it" table, and each subsystem's testing page lists the
   scenarios whose `guards` name that crate.

The owner names which generator he meant; W4d does item 1 by default and 2
and 3 only on his word.

**Architecture pages: how they are brought up to date** (156 pages outside
`build/`, 157 counting `network/ebpf.md`, which W2 deletes)

Every page ends in one of three states: **shipped** (rewritten to what the
code does, modules and functions checked, each claim with a proof, a "Not
built yet" section for the rest), **planned** (frontmatter, claims cut, page
kept short), or **moved/merged** (owner's word). No page stays `stale` at the
phase exit. Sizes are honest: a shipped rewrite is a read of the crate, so 20
to 25 pages fill one wave.

*Fix first: wrong today* (W3, 15 pages, the overclaims list):
`security/overview` (the security page), `architecture/trust-boundaries`,
`architecture/firecracker`, `network/overview`, `network/policy`,
`storage/overview`, `storage/fuse`, `core/jailer`, `security/spiffe`,
`security/seccomp`, `security/secrets-proxy`, `hosted/first-party`, plus
`README.md`, `index.mdx`, `getting-started/project-status`.

*Update to the code* (125 pages, the 21 boilerplate pages below included;
the crate exists and the page must match it):

| Section | Pages | What the wave checks against |
|---|---|---|
| `architecture/` | overview, design-principles, component-model, session-lifecycle, cold-start, deployment-model, drive9 (7) | the crate table, `manager.rs`, `golden.rs`, process vs VM mode |
| `spec/` | all 15 | `proto/vam/v1/*.proto` and `crates/vam-spec`; field tables link the generated reference instead of copying it |
| `core/` | overview, prerequisites, lifecycle, config, vmm, api-client, vsock-device, snapshot, health, errors, security-model, testing, invariants (13) | `vam-core`, the vendored SDK |
| `orchestrator/` | overview, daemon, api, workspace-files, workspace-files-upload, lifecycle, session, router, reconciliation, security, user-isolation, telemetry, errors, testing, invariants, health, draining (17) | `vam-orchestrator` (draining exists: `AppState.draining`) |
| `runtime/` | all 20 | `vam-runtime`, `vam-inference` |
| `hosted/` | overview, architecture, protocol, hooks, capability-manifest, resources, web (7) | `hsp*.rs`, `brokers/vam-web`, `hosted_services.proto` |
| `security/` | run-receipt, threat-model, cedar, agent-autonomy-and-governance, audit-log, hitl-crypto, key-management, errors, testing, invariants (10) | `vam-security`, `receipt.rs`, `signing.rs` |
| `storage/` | architecture, data-format, checkpoint, turn-commits, quota-gc, consistency, cache, errors, testing, invariants (10) | `vam-storage`, `turn_commit.rs` |
| `network/` | architecture, contracts, lifecycle, security, telemetry, errors, testing, invariants, ipam (9) | `vam-network` gate, proxy, `tls_ca.rs`, IPAM (used only for TAP names) |
| `telemetry/` | 17 of 18 (not `roadmap`) | `vam-telemetry`, `events.rs`, `egress.rs` |

*Pure design intent: planned, or moved on the owner's word* (18 pages):
`network/modes` (direct mode part), `network/tap`, `network/nat`,
`network/dns` (the Hickory proxy in `vam-network/src/dns.rs` has no caller),
`network/traffic-control`; `storage/backend-interface`, `storage/backends`,
`storage/sync`, `storage/overlay`, `storage/encryption`;
`orchestrator/pool` (rewrite: golden snapshot shipped, standby pool
planned), `orchestrator/scheduler` (no code), `orchestrator/admission`;
`core/ratelimit` (Firecracker rate limiter always `None`, `vam-core/src/vmm.rs:35`);
`security/pii-redaction` (rewrite to the telemetry redactor and
`secret_forms`, guest-output PII planned); `hosted/triggered-runs` (no
code), `hosted/roadmap`, `telemetry/roadmap`. Default: all stay on the site
as `planned`; moving any to `design/` needs the owner's word (open question 3).

*Subsystem boilerplate* (`errors`, `testing`, `invariants` in core,
orchestrator, network, runtime, security, storage, telemetry: 21 pages).
Default: kept and rewritten; `testing` pages become the table of scenarios
(by `guards`) and test files for the crate; each `invariants` line names its
proof or is cut; `errors` pages checked against the error enums. Merging
them into the section overviews (21 deletes) is open question 3.

**Items**

- [ ] **IT-3.11 Feature register, developer-page proof rule, status value.**
  (a) New `e2e/docs-features.yaml`, seeded from the inventory above: per
  feature `id`, `area`, `surface`, `dev` (page), `arch` (page), `proves`
  (scenario ids or test paths), or `unproven: <reason>`. (b) New lint rule
  `docs-feature`: each feature's `dev` and `arch` pages exist; once a page
  has left the baseline it must be `shipped` and its `proves` must hold the
  feature's proofs; a feature with neither `proves` nor `unproven` fails.
  `check` prints `features: N, proven M, with a developer page K`. (c) New
  rule `docs-dev-proof`: a `shipped` page in the Developer Docs directories
  (listed in `e2e/docs-claims.yaml`, key `developer:`) names at least one
  proof whether or not it matches a claim word: the owner's "where to
  check it". (d) Status value: no new value by default (open question 2);
  `e2e/README.md` states the rule "built: `shipped`, with a 'Not built yet'
  section for the rest; not built: `planned` or moved to `design/`". If the
  owner says yes to `design`: the enum in `docs.rs` and `content.config.ts`,
  a "Design note" banner in `PageTitle.astro`, and `design` refused in the
  Developer Docs directories. *Files:* `crates/vam-e2e/src/docs.rs`,
  `crates/vam-e2e/tests/docs_check.rs`, `e2e/docs-features.yaml` (new),
  `e2e/docs-claims.yaml`, `e2e/README.md`; if (d) yes,
  `docs/src/content.config.ts`, `docs/src/components/PageTitle.astro`.
  *Accept:* a failing fixture per new rule; `cargo run -p vam-e2e -- check`
  clean with the baseline unchanged at 207; every `proves` in the register
  resolves; `pnpm build` green. *Test:* `cargo test -p vam-e2e`, `check`,
  `pnpm build`. *Turns green:* none (new fixtures).
  `Decide: NOW — proof 3 (security page; the owner's rule "where to check it" made mechanical), cost 1 (one wave, vam-e2e and one data file, no product surface)`
- [ ] **IT-3.12 The security page, the overclaims, the honest page.**
  Replaces IT-3.3, IT-3.4 and IT-3.5 with the same scope, done together
  because they share claims: the 15 "fix first" pages above and new
  `getting-started/limits`. `security/overview` is the launch page: one row
  per claim (claim, proof, smoke or lane, the operator setting that turns it
  off and that the node and receipt report), the "first lane run pending"
  marking for the three W0 scenarios, a plain "no `internet` setting means
  allow" row, the stated limits (8 MiB scrub buffer, telemetry gRPC without
  TLS, tool confinement unproven), "Planned" and "Removed: kernel-level
  egress filtering". The honest page as IT-3.5, plus "process mode isolates
  nothing". *Files:* those 16 pages, `README.md`, `docs/astro.config.mjs`,
  `e2e/docs-baseline.txt`, `e2e/docs-features.yaml` (isolation and egress
  rows). *Accept:* each claim row names an `active` scenario or test, or
  moves to Planned; baseline 207 → 192; `check` clean; `pnpm build` green.
  *Turns green:* none.
  `Decide: NOW — proof 3 (security page and honest page launch proofs), cost 1 (docs only)`
- [ ] **IT-3.13 Developer docs: Getting Started and Operate a node.** The
  ten `getting-started/` pages left (actions in the target table), new
  `operate/configuration` and `operate/telemetry-export`, the sidebar
  groups. The configuration page is read from `daemon.rs`, `bin/vamd.rs`,
  `tls.rs`, `rate_limit.rs`, `ttl.rs`, `secrets.rs` and `vam-telemetry`, not
  from other pages. Every command block is run in the wave against a
  process-mode node (`scripts/e2e/run.sh` builds one; the mock model needs
  no key) and its output pasted; VM-only steps cite their lane scenario.
  *Files:* `docs/src/content/docs/getting-started/*`, `operate/*` (new),
  `docs/astro.config.mjs`, `e2e/docs-baseline.txt`, `e2e/docs-features.yaml`.
  *Accept:* baseline 192 → 182; every new or rewritten page `shipped` with
  `proves`, or `stale`/`planned` with a reason in the PR; `check` clean
  including `docs-feature` for the install, auth and telemetry rows;
  `pnpm build` green. *Turns green:* none.
  `Decide: NOW — proof 3 (install launch proof; owner: features presented), cost 1 (docs only)`
- [ ] **IT-3.14 Developer docs: Authoring and Tools.** The eight
  `authoring/` and two `tools/` pages; new `authoring/permissions`,
  `authoring/harness`, `tools/builtins`, `tools/mcp`. Same rules as IT-3.13.
  *Accept:* baseline 182 → 172; `docs-feature` clean for agents, skills,
  tools, playbooks. `Decide: NOW — proof 2 (P5 and the demo's agent, playbook and approval surfaces; owner), cost 1`
- [ ] **IT-3.15 Developer docs: Integrations, Security & Governance,
  Tutorials.** `integrations/egress` (from `egress-proxy-deepdive`, redirect),
  `integrations/jira-assistant-walkthrough`, new `integrations/web` and
  `integrations/hosted-brokers`; `security/approvals` (from `hitl-lifecycle`,
  redirect), `security/secret-management`, new `security/receipts`;
  `security/cedar-and-hitl` merged into `authoring/permissions` (delete and
  redirect only on the owner's word, else cut to a short `shipped` page on
  the legacy Cedar path that links the permissions page); both
  tutorials; `reference/agent-cheat-sheet`; sidebar renames. *Accept:*
  baseline 172 → 164; `docs-feature` clean for approvals, secrets, egress,
  receipts, hosted brokers. `Decide: NOW — proof 3 (receipt: demo and security page), cost 1`
- [ ] **IT-3.16 Generated API reference (slice).** As item 1 of the
  generator proposal: drift test, missing paths, `servers` on `https://`,
  `starlight-openapi` page `reference/api`, Scalar pinned with SRI, the
  drift test also in `docs.yml`. CLI and spec generators (items 2 and 3) only
  on the owner's word, same wave if they fit. *Files:*
  `docs/api/openapi.yaml`, `crates/vam-orchestrator/tests/openapi_drift.rs`
  (new), `api/mod.rs` (the `/docs` handler only), `docs/package.json`,
  `pnpm-lock.yaml`, `docs/astro.config.mjs`, `.github/workflows/docs.yml`.
  *Accept:* the drift test fails on a scratch route (shown in the PR, then
  dropped) and passes; `pnpm build` renders the reference; `check` clean.
  `Decide: SLICE — proof 2 (owner: features presented; the OpenAPI misses about 9 routes), cost 2 (three generators, a docs plugin, two crates) → slice: API drift test and rendered page, cost 1; CLI and spec generators on the owner's word; utoipa CUT for 0.1.0 (rewrite of api/)`
- [ ] **IT-3.17 Architecture pages to the code.** The 143 pages not in W3,
  W2 or `build/`, by section in seven waves (below). Per page: shipped,
  planned or moved as the classification above; `testing`, `invariants`
  and `errors` pages as above. A wave that finds a claim false in code
  raises it as a finding (security floor: `Decide: NOW`), never rewords it
  to pass. *Accept per wave:* the wave's pages leave the baseline; `check`
  clean; `pnpm build` green; the PR lists each page's state and the
  findings. `Decide: NOW — proof 2 (owner: content must match the code; design finding: docs overclaim), cost 1 per wave (docs only)`
- [ ] **IT-3.18 Close the ratchet.** Replaces IT-3.7's last wave: delete
  `e2e/docs-baseline.txt` (empty after W6), make `status` required in
  `content.config.ts`, make `docs-feature` strict (every register row has a
  shipped developer page with its proof, or `unproven` with a line on the
  security page), check zero `stale` pages, rerun the claim audit over pages
  other lanes edited during the phase, and fill the honest page's "where
  open models still lose" if B1/H1 rows are published by then. *Accept:*
  the phase exit's "`check` clean on the base with no ratchet file left".
  `Decide: NOW — proof 3 (the phase exit), cost 1`

**Waves** (replace rows 2 to 6c; each wave one session, branch `it-3/wave-N`,
draft PR `IT-3 WN: …`, as above)

| Wave | Items | Baseline after (turns green) | Repo | Model | Budget | Depends on | Shared surfaces | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 2 | IT-3.2 (unchanged) | 207 (`network/ebpf.md` deleted) | vam | Sonnet low | 75 min | 1 | as the old row 2; `skip-vm-lane` (`proto/` comment only) | [ ] | |
| 2b | IT-3.11 | 207 (new rule fixtures) | vam | Sonnet low: extends W1's rules in the same file, one data file | 90 min | 2; open question 2 answered | `vam-e2e/src/docs.rs` (H1 edits `bench.rs`, `engine.rs`, `main.rs`, `run_cmd.rs`, not `docs.rs`: **Low**); `skip-vm-lane` (lint only) | [ ] | |
| 3 | IT-3.12 | 192 | vam | Sonnet medium: the launch-proof page; each row is checked against code, scenario status and the operator setting (security floor judgement) | 150 min | 2b | `README.md`, `index.mdx`, `project-status.md` (E10 and H1 phase closes edit project status: **Low**, rebase) | [ ] | |
| 4a | IT-3.13 | 182 | vam | Sonnet medium: the configuration page reads every flag and variable from code and says which loosen a secure default (locked decision 2); commands run on a process node | 150 min | 3 | `getting-started/*`, sidebar (any lane adding a page: **Low**) | [ ] | |
| 4b | IT-3.14 | 172 | vam | Sonnet low | 120 min | 4a | `authoring/*`: E10 Phase 3 (steer) may document new run inputs: **Low**, rebase | [ ] | |
| 4c | IT-3.15 | 164 | vam | Sonnet low | 120 min | 4b; open question 3 (cedar-and-hitl) | sidebar | [ ] | |
| 4d | IT-3.16 | 164 (+ reference pages, passing) | vam | Sonnet medium: a Rust drift test, a docs build plugin and lockfile, `docs.yml` | 120 min | 4c; open question 1 answered | `docs/api/openapi.yaml` (a lane adding a route edits it: **Low**); `api/mod.rs` `/docs` handler only (E10 Phase 3 builds in `api/`: **HOLD** while it has an open PR in `api/mod.rs`); `skip-vm-lane` | [ ] | |
| 5a | IT-3.17: `architecture/` 7, `spec/` 15 | 142 | vam | Sonnet low | 120 min | 4d (4c if 4d waits on the owner) | `spec/` links the generated reference | [ ] | |
| 5b | IT-3.17: `core/` 14, `security/` 11 | 117 | vam | Sonnet low | 120 min | 5a | none | [ ] | |
| 5c | IT-3.17: `orchestrator/` 20 | 97 | vam | Sonnet low | 120 min | 5b | **HOLD while E10 Phase 3 or H1 has an open PR editing `docs/src/content/docs/orchestrator/`**; take `main` the same day | [ ] | |
| 5d | IT-3.17: `runtime/` 20 | 77 | vam | Sonnet low | 120 min | 5c | **HOLD likewise for `runtime/`** (H1 W4 open today, edits `runtime/benchmark-results.md`) | [ ] | |
| 5e | IT-3.17: `network/` 14, `hosted/` 9 | 54 | vam | Sonnet low | 120 min | 5d | none (W2 already removed eBPF lines) | [ ] | |
| 5f | IT-3.17: `storage/` 15 | 39 | vam | Sonnet low | 90 min | 5e | `storage/turn-commits` (E10 Phase 4 effect journal may edit: **Low**) | [ ] | |
| 5g | IT-3.17: `telemetry/` 18 | 21 | vam | Sonnet low | 90 min | 5f | none | [ ] | |
| 6 | IT-3.6 (unchanged) | 0 | vam | Sonnet low | 60 min | 5g | `build/`, `design/vam-build/`, sidebar | [ ] | |
| 7 | IT-3.18 | exit: no baseline, `status` required, register strict, zero `stale` | vam | Sonnet low | 75 min | 6 | `content.config.ts`, `vam-e2e/src/docs.rs`; `skip-vm-lane` | [ ] | |

Order as the owner set it: eBPF and the lint follow-up first, then the
security page (it fixes the overclaims every later page links to), then the
developer docs (the owner's primary focus; each wave leaves its pages
proven), then the architecture pages, then the build pages, then the wave
that empties the baseline. Every docs wave: `pnpm install` and `pnpm build`
in `docs/`, `cargo run -p vam-e2e -- check`; docs-only waves are
path-ignored by `rust.yml` and run the `docs` job only (not yet seen running
while the billing block lasts: the first docs wave watches it). W2, 2b, 4d and
7 touch Rust and run `rust.yml`; they carry `skip-vm-lane`. Budget: about
29 hours of wave time (old rows 2 to 6c: about 10 hours); the clippy and
dependency rows are unchanged and follow W7. Wave decision lines are their
items'.

**Open questions** (defaults in bold)

1. **The generator.** "From the schema itself" could mean: (a) the existing
   `docs/api/openapi.yaml` rendered as pages; (b) OpenAPI generated from Rust
   (`utoipa`); (c) the proto descriptor (`vam_descriptor.bin`) rendered for
   the spec formats; (d) the clap CLI rendered. **Default: (a) with a drift
   test in W4d; (c) and (d) on the owner's word in the same wave; (b) cut
   for 0.1.0.**
2. **A fourth status value `design`.** **Default: no.** `planned` already
   says "not built" with a banner, and a shipped page may hold a "Not built
   yet" section, so every page is either built (proved) or not (bannered).
   A `design` value would be a third answer to "does the node do this?",
   which is what locked decision 1 rules out, and the overclaiming pages
   would move to it to escape a proof. If the owner wants it anyway: allowed
   only outside the Developer Docs tab, "Design note" banner, no proof
   required, and still no claim words without a proof (IT-3.11 (d)).
3. **Deletes and moves that need the owner's word.** Defaults: **merge
   `security/cedar-and-hitl` into `authoring/permissions`, delete, redirect;
   merge `getting-started/coldstart-baseline` into `architecture/cold-start`,
   redirect; keep both Jira pages; keep all design-intent pages on the site
   as `planned` (none moved to `design/`); keep the 21 `errors`, `testing`,
   `invariants` pages, rewritten.** Alternatives for the owner: move
   `network/{tap,nat,dns,traffic-control}`, `storage/{sync,backends,encryption}`,
   `hosted/roadmap`, `telemetry/roadmap`, `orchestrator/scheduler` to
   `design/` (11 pages off the site); fold the 21 boilerplate pages into
   their overviews.
4. **The honest page ships now.** **Default: yes, in W3**, with "where open
   models still lose" as a `planned` section; H1 W4 (open) is adding
   open-model rows, so W7 fills the section if B1/H1 rows are published by
   then.
5. **Feature register and the developer proof rule** (IT-3.11). **Default:
   `e2e/docs-features.yaml` checked by `vam-e2e check`; every shipped
   developer page names a proof.** Alternative: the register as a docs page
   table, checked by review only.
6. **How commands in the developer docs are verified.** **Default: the
   wave runs each command against a process-mode node and pastes real
   output; VM-only steps cite their lane scenario.** A "docs smoke" that
   extracts and runs code blocks in CI is backlog (`Decide: BACKLOG — proof
   1, cost 2`).
7. **`SKILL.md`.** The sidebar promises `SKILL.md`; the code reads
   `SKILLS.md`. **Default: the docs say `SKILLS.md` until the launch row
   "`SKILL.md` read alongside `SKILLS.md`" ships.**

### Docs plan: architecture rebuild and developer docs (proposed 2026-10-29)

**Status: proposed, awaiting the owner.** Drafted 2026-10-29 by the planning
agent on `feat/isolation-truth` (W0 and W1 merged, W2 building).
Supersedes the waves and items of the "Docs waves re-plan (owner,
2026-10-28)" above (IT-3.11 to IT-3.18 and waves 2b to 7 are withdrawn; their
numbers are not reused). Kept from it: the feature inventory (corrected
below), the gap lists, the generator analysis. Kept unchanged from the phase
plan: W0, W1, IT-3.1, IT-3.2 (W2, eBPF, running), IT-3.6 (build pages:
11 behind the planned banner, 10 to `design/vam-build/`, approved), the
meanwhile item pinning the Scalar script on vamd `/docs` with the spec
`servers` URL, IT-3.8 to IT-3.10 and the C1 to C6 and D rows, locked
decisions 1 to 6. Item numbers here start at IT-3.21 (IT-3.19 and IT-3.20
are left to the orchestrator for the meanwhile items). The owner's decisions
of 2026-10-29 are the frame: the Architecture & RFCs tab is rebuilt from the
code, about one page per subsystem; accurate pages are kept; pure design
intent moves to `design/`; wrong pages are retired with redirects; the
Developer Docs are public and planned page by page; both tabs from one
inventory; **nothing moves or is retired without the owner's word on the
lists in (2).**

**The rule for every shipped feature** (owner): what it is (an architecture
page), how to use it (a developer page with real commands, config and
output), where it is proved (an `active` scenario or a test; the lint
checks it through the feature register, IT-3.21).

#### Inventory corrections (read 2026-10-29)

The 90-row inventory above holds for the base. Corrections:

- **E10 Phase 3 (Steer) is built, not "not planned yet"** (the Coordination
  bullet of this phase is stale). All its vam waves merged into
  `feat/agent-runtime`; release to
  `main` pending. It brings 9 scenarios and these rows (area Agents and
  specs unless noted): **steer and queue** (`vam run send [--queue]`,
  `POST /v1/runs/{id}/messages`; `steer-changes-the-running-task`,
  `queued-message-runs-after-the-task`, `correction-sent-mid-task-is-followed`);
  **stop** (`vam run stop`, `POST /v1/runs/{id}/stop`;
  `stop-ends-a-running-task-cleanly`, `stop-denies-a-pending-approval`,
  `stop-ends-a-run-parked-across-a-restart`); **questions** (`ask_user`,
  `vam run answer`, approval kind `question`, `waitingFor`;
  `question-waits-for-an-answer-and-resumes`; area Approvals); **progress
  and cards** (`post_message`, `show`, jig cards;
  `progress-shows-while-the-task-runs`, `long-task-posts-progress`; area
  Tools). The built-in tools row grows from 9 to 12 tools. Pages it adds:
  `runtime/inbox-outbox.md`, `runtime/jig.md`; edits `orchestrator/session.md`,
  `orchestrator/api.md`, `runtime/agent.md`, `telemetry/event-catalog.md`,
  `docs/api/openapi.yaml` (adds `/v1/runs/{urn}/messages` and `/stop`).
- **H1 adds** (on `feat/harness` to): one resample of an
  invalid tool call before the typed error
  (`invalid-call-is-resampled-once-before-a-typed-error`), named harness
  profiles printed on the receipt with their digest
  (`named-profile-prints-on-the-receipt-with-its-own-digest`), reasoning
  recorded and never replayed when stripped plus thinking-model tool calls
  (`reasoning-is-recorded-and-never-replayed-when-stripped`,
  `thinking-model-tool-calls-are-not-dropped`), one example per tool
  description (`tool-descriptions-carry-one-example-when-enabled`). Three
  rows (area Agents and specs). Edits `runtime/benchmark-results.md` and
  `telemetry/cost-accounting.md`.
- With both lanes on the base: **97 features**. The register (IT-3.21) is
  seeded with the 90 and each lane's rows are added by the wave that first
  runs after that lane reaches the base.
- Idle sessions: `orchestrator/daemon.md:95` documents
  `VAMD_SESSION_IDLE_TTL_SECS` (the inventory said no page; true for the
  Developer tab only). `orchestrator/daemon.md:82` has a 21-row `VAMD_*`
  table that feeds `operate/configuration`.
- **OpenAPI drift, measured.** The router has catch-all routes
  (`api/mod.rs:144,151,158,164,183` for agents, policies, playbooks,
  skills, runs; `:191` secrets; `:206` hosted brokers) whose suffixes are
  dispatched in handlers (`parse_run_path`, `api/mod.rs:1553`), so a string
  compare of router and spec paths reports 7 router-only and 39 spec-only
  paths, nearly all false. Logical routes missing from the spec:
  `GET /v1/runs/{id}/receipt`, `/evidence`, `/trajectory`;
  `POST /v1/agent-checks` (`:156`); `POST /v1/playbook-tests` (`:155`);
  `GET /v1/workspaces/{id}/files` and the per-path get and put
  (`workspace_files.rs:57-63`); the permissions set and `migrate` paths;
  release on agents, policies, playbooks and skills (only skill packages
  have it). `servers` is `http://127.0.0.1:8080` (`openapi.yaml:10`; the
  meanwhile item fixes it). There is no router fallback, so a test cannot
  tell "no such route" from a handler's 404 without a marker.
- `egress-refuses-an-ungranted-host` is still the one `pending` scenario (48
  on the base, 47 `active`). No page has a `status` field yet (W1 left all
  207 plus the README in `e2e/docs-baseline.txt`).

#### (1) Architecture tab target

Audience: contributors and agent sessions; `AGENTS.md` §6 points sessions at
these pages by subsystem. Each page has the same shape: what the subsystem
is, its modules (file paths), how a request flows through it, its settings,
a "Proved by" table (feature, scenario or test), a "Not built" list (each
item one line, linking the design doc that holds it), and its invariants,
each naming its proof or cut. Slugs reuse an existing URL where one fits, so
inbound links keep working and only the retired pages need redirects.

**25 pages: 18 rebuilt in place, 4 new, 3 kept.** Plus, kept beside them:
`orchestrator/workspace-files` and `orchestrator/workspace-files-upload`
(accurate API contracts), E10's `runtime/inbox-outbox` and `runtime/jig`
once they land, and the 11 `build/` pages behind the planned banner.

| # | Page (slug) | Covers | Inventory rows it carries | Proofs | Replaces |
|---|---|---|---|---|---|
| A1 | `architecture/overview` (rebuilt) | Planes, the crate table as it is, the path of one `vam run` from CLI to receipt, process mode versus VM mode, the repository layout | Process mode; Mac and Windows hosts; every area at one line | `plain-chat-gets-a-reply`; `crates/vam-orchestrator/tests/process_mode_bin.rs`; release gate `scripts/e2e/check-release-gate.sh` | `architecture/component-model`, `deployment-model`, `getting-started/workspace`; design: `architecture/design-principles` |
| A2 | `architecture/isolation` (new) | What isolates a session: microVM per session, jailer (chroot, uid 65400, PID and mount namespaces, cgroup v2, Firecracker's built-in seccomp), vsock control channel and its frame bound, no guest route, loopback-only guest proxy, the isolation line on node and receipt, what process mode does not isolate, threats and what answers each | Isolation (5 rows); guest proxy loopback; no guest route | `vm-boots-and-registers-over-vsock`, `jailer-confines-the-vmm-process`, `node-reports-the-isolation-it-runs`, `receipt-names-the-isolation-the-run-had`, `guest-proxy-never-binds-wildcard`, `guest-has-no-route-off-the-vm`, `host-tool-output-fits-frame`, `crates/vam-orchestrator/tests/adversarial_vsock_tests.rs`, `crates/vam-core/tests/jailed.rs` | `architecture/trust-boundaries`, `architecture/firecracker`, `core/security-model`, `security/threat-model`, `spec/vsock-rpc`; design: `security/spiffe`, `security/seccomp` |
| A3 | `architecture/cold-start` (rebuilt) | Golden snapshot bake and restore, jailed restore, the key cache, measured numbers with what each measured (about 225 ms session start on the CI lane; "~115 ms" only as the dedicated-host VM-ready figure), what is not built (UFFD, standby pool) | Golden snapshot | `vm-restores-from-the-golden-snapshot` | `getting-started/coldstart-baseline`, `orchestrator/pool`, `core/snapshot` |
| A4 | `architecture/testing` (new) | Scenarios as the spec, layers (contract, process, microVM lane), the mock model, smoke and lane jobs, the docs lint and the feature register, the benchmark runner; per crate, the scenarios whose `guards` name it | Mock model; benchmark runner | `crates/vam-e2e/tests/check_fixtures.rs`, `crates/vam-e2e/tests/docs_check.rs`, `model-task-runs-k-trials-on-the-mock` | the 7 `testing` pages (core, orchestrator, network, runtime, security, storage, telemetry) |
| A5 | `spec/overview` (rebuilt) | Contract-first: `proto/vam/v1/`, `vam-spec` codegen, JSON encoding, the descriptor baseline, conventions; links the generated spec reference (D5) or the proto files | — (contracts) | `crates/vam-spec` conformance tests (wave names them), `scripts/e2e/check-manifests.sh` | `spec/conventions`, `codegen`, `common` |
| A6 | `core/overview` (rebuilt) | `vam-core`: config builder, `Vmm` trait, process VMM, state machine, CID allocation, `jail.rs` staging and teardown, the vendored SDK and what VAM uses of it, VM path refusals | Microvm per session (mechanism); jailer settings; VM node refuses without paths | `jailer-confines-the-vmm-process`, `crates/vam-core/tests/jailed.rs`, `daemon.rs` unit tests (wave names them) | `core/lifecycle`, `config`, `jailer`, `vmm`, `api-client`, `vsock-device`, `health`, `errors`, `invariants`; design: `core/ratelimit` |
| A7 | `orchestrator/overview` (rebuilt) | `vamd`: startup order, state DB, TLS, listeners, readiness and draining, reconciliation and restart settle, idle TTL, example seeds, no panics in request paths | Start a node; TLS; idle sessions; example seeds; runs survive a restart; no panics | `vamd-refuses-plain-http-by-default`, `crates/vam-orchestrator/tests/tls_refusals.rs`, `follow-up-continues-after-vamd-restart`, `work-survives-vamd-restart-mid-task`, `ttl.rs` unit tests | `orchestrator/daemon`, `health`, `reconciliation`, `draining`, `invariants`; design: `orchestrator/router`, `scheduler`, `admission` |
| A8 | `orchestrator/api` (rebuilt) | The router, auth, rate limits, paging, the error map, idempotency keys, the OpenAPI file and `/docs`; links `reference/api` | Health and node status; rate limits; paging | `api-rate-limits-a-flood`, `crates/vam-orchestrator/src/paging_store_tests.rs`, `crates/vam-orchestrator/tests/api_tests.rs`, the drift test (IT-3.31) | `orchestrator/errors` |
| A9 | `orchestrator/session` (rebuilt; E10 edits it) | Sessions, runs, threads and workspaces: create, pause, resume, terminate, snapshot; the run state machine; workspace pins; stop versus terminate versus cancel; with E10, inbox and queue | Session operations; workspaces and threads; workspace pin; (E10) steer, queue, stop | `crates/vam-orchestrator/tests/api_tests.rs`, `crates/vam-orchestrator/tests/workspace_pin_tests.rs`, `thread-history-lists-tool-results`; (E10) its stop and steer scenarios | `orchestrator/lifecycle`; design: `architecture/session-lifecycle` |
| A10 | `orchestrator/user-isolation` (rebuilt, retitled "Users, keys and ownership") | Users, roles, bootstrap, login, API keys, ownership stamps, cross-user 404, publish grant, owner reassign | Auth and users (6 rows) | `crates/vam-orchestrator/tests/login_tests.rs`, `me_tests.rs`, `isolation_coverage_map_tests.rs`, `member-chats-with-a-released-agent` | `orchestrator/security` |
| A11 | `orchestrator/catalog` (new) | Agents, skills, skill packages, playbooks and policies as catalog entries: drafts, release, versions, suggested version, URN origin and reserved domains, admission of skill packages, agent check, allowed models | Agent spec; URN origin; agent check; allowed models; skill package; publish a skill; required secrets | `released-agent-approvals-are-once-only`, `crates/vam-orchestrator/tests/agent_check_tests.rs`, `allowed_models_tests.rs`, `api_tests.rs` | `spec/agentic-spec`, `spec/skills` |
| A12 | `runtime/overview` (rebuilt) | The guest: PID 1 init, mounts, the vsock server, the guest egress proxy, exec, errors, invariants | Guest proxy (guest half); control channel (guest half) | `guest-proxy-never-binds-wildcard`, `vm-boots-and-registers-over-vsock` | `runtime/init`, `vsock-server`, `errors`, `invariants` |
| A13 | `runtime/agent` (rebuilt; E10 and H0 edit it) | The agent loop: instruction layers, `load_skill` and `INDEX.md`, harness switches and profiles, loop detection, todo recitation, repair budget, context compaction | Harness switches; compaction; instruction layers; `load_skill`; (H1) resample, profiles, reasoning | `harness-switches-print-on-the-receipt`, `repeated-identical-call-is-caught-and-observed`, `todo-is-recited-each-turn-when-enabled`, `compaction-keeps-an-early-correction`, `follow-up-after-compaction` | `runtime/instruction-template`, `runtime/context` |
| A14 | `runtime/tools` (rebuilt, retitled "Tools") | Built-in tools, typed argument errors, custom in-VM tools (`exec.rs`) and what their confinement is proved to be, host tools and the 60 KiB bound, MCP connections | Tools (6 rows); (E10) `post_message`, `show`, `ask_user` | `thread-history-lists-tool-results`, `tool-call-first-try-saves-a-draft`, `invalid-tool-arguments-return-a-typed-error`, `host-tool-output-fits-frame`, `crates/vam-orchestrator/tests/mcp_connections_tests.rs` | `runtime/primitives`, `host-tools`, `mcp` |
| A15 | `runtime/inference` (rebuilt) | `vam-inference`: providers, catalog, prompt caching, usage and cost, the mock model, the model policy fields | Inference (3 rows); allowed models (link) | `provider.rs` unit tests, `crates/vam-e2e/tests/usage_recorded.rs` | `spec/model-policy` |
| A16 | `runtime/workflow` (rebuilt, retitled "Playbooks, gates and approvals") | Playbook activation, inputs, gates and guards, playbook tests, the approval lifecycle (ask, decide, amend, always allow/deny, once-only, park and resume, timeout), approval signing | Playbooks (5 rows); Approvals (5 rows); (E10) questions | `matching-request-starts-the-playbook`, `greeting-starts-no-playbook`, `injected-text-cannot-start-a-playbook`, `gate-decided-finishes-the-run`, `approval-suspends-and-resumes`, `always-allow-and-always-deny-are-remembered`, `parked-approval-survives-a-restart`, `crates/vam-cli/tests/playbook_test_cli.rs` | `runtime/hitl`, `runtime/evals`, `spec/workflow`, `spec/hitl`, `spec/evals`, `security/hitl-crypto` |
| A17 | `security/permissions` (new) | Permissions as data (tool and internet rules, presets, effective view, explain, migrate), the PEP and Cedar PDP (legacy `capabilityPolicy`), **no `internet` setting means allow**, untrusted vendor text fencing | Permissions; Cedar; per-agent internet rules; broker text fenced | `always-allow-and-always-deny-are-remembered`, `crates/vam-orchestrator/tests/permissions_api_tests.rs`, `crates/vam-security/tests/pdp_tests.rs`, `injected-text-cannot-start-a-playbook` | `security/cedar`, `agent-autonomy-and-governance`, `errors`, `invariants`, `spec/security` |
| A18 | `security/secrets-proxy` (rebuilt, retitled "Secrets: vault and injection") | Vault, scopes and kinds, the master key (`VAMD_SECRETS_MASTER_KEY` or the 0600 key file), placeholders substituted at the host proxy, per-session CA, encoded-form redaction, import from the environment, the 8 MiB scrub limit | Secrets (5 rows); per-session CA | `secret-never-reaches-guest-encoded`, `vault-secret-is-not-in-the-guest` (lane, first run pending), `crates/vam-network/src/proxy/mitm_tests.rs`, `secrets.rs` unit tests | `security/key-management`, `pii-redaction` |
| A19 | `security/run-receipt` (kept) | Receipt contents, two trust tiers, offline verification, node signing keys, the evidence pack | Receipts and evidence (4 rows) | `receipt-separates-host-facts-from-model-text`, `receipt-verifies-offline-and-fails-on-tamper`, `crates/vam-cli/tests/receipt_cli.rs`, `node_signing_keys_tests.rs` | — |
| A20 | `network/overview` (rebuilt; W2 edits it first) | `vam-network`: brokered sessions, the gate, the host proxy and MITM, the IP floor, passthrough hosts and response cap, why there is no kernel filter (W2's section), TAP and IPAM code that brokered sessions do not use | Egress (6 rows) | `egress-refuses-an-ungranted-host` (pending: forward link only), `gate.rs` and `ssrf_guard.rs` unit tests, `guest-has-no-route-off-the-vm` | `network/architecture`, `contracts`, `lifecycle`, `security`, `telemetry`, `errors`, `invariants`, `ipam`, `policy`; design: `modes`, `tap`, `nat`, `dns`, `traffic-control` |
| A21 | `storage/overview` (rebuilt) | `vam-storage`: the per-session ext4 image at `/mnt/drive9`, directory backend in process mode, CAS and manifest, owner prefixes, quota and GC, `LocalDirBackend` only | Agents' workspaces separate; quota and GC; workspace files (link) | `agent-workspaces-are-isolated`, `workspace-matches-conversation-after-vm-stop` (lane), `gc.rs`, `log_quota.rs` unit tests | `storage/architecture`, `backend-interface`, `data-format`, `overlay`, `cache`, `checkpoint`, `quota-gc`, `consistency`, `errors`, `invariants`, `architecture/drive9`, `runtime/drive9-fs`; design: `fuse`, `backends`, `sync`, `encryption` |
| A22 | `storage/turn-commits` (kept) | Turn commits, stop rollback, `kept/`, thread log | Turn commits; threads (storage half) | `workspace-matches-conversation-after-vm-stop`, `follow-up-continues-after-stop` | — |
| A23 | `telemetry/overview` (rebuilt) | Events and the outbox, OTLP, CloudEvents, OpenLineage, the gRPC stream (loopback, no TLS), spool, retention, tenant, audit chain, redaction of exports, timing lines, cost records | Telemetry (6 rows); usage and cost (export half) | `crates/vam-orchestrator/tests/standards_egress_test.rs`, `telemetry_stream_test.rs`, `audit_fail_closed_test.rs`, `trajectory_timeline_test.rs` (wave confirms paths) | `telemetry/architecture`, `otel`, `trace-context`, `cloudevents`, `streaming`, `reliability`, `metrics`, `cost-accounting`, `latency`, `audit-ledger`, `privacy`, `retention`, `errors`, `invariants`, `orchestrator/telemetry`, `runtime/telemetry`, `spec/events`, `spec/telemetry`, `security/audit-log`; design: `telemetry/roadmap` |
| A24 | `hosted/overview` (rebuilt) | HSP: UDS gRPC transport, manifest admission, trust tiers, Invoke, lifecycle hooks, registration; TypeScript examples | Register a hosted broker; TypeScript examples | `crates/vam-orchestrator/tests/hsp_uds.rs`, `hsp_invoke.rs`, `hsp_hooks.rs`, `hsp_manifest.rs`, `hsp_trust.rs` | `hosted/architecture`, `protocol`, `hooks`, `capability-manifest`; design: `first-party`, `resources`, `triggered-runs`, `roadmap` |
| A25 | `hosted/web` (kept) | The `vam.web` broker | Web search and fetch | `hsp_web.rs`, `hsp_web_sources.rs`, `brokers/vam-web/src/fetch/network/tests.rs`, `brokers/vam-web/tests/env_isolation.rs` | — |

Sidebar for the tab: System (A1 to A4), Contracts (A5), Host (A6 to A11
and the two workspace-files pages), Guest (A12 to A16 and E10's two pages),
Security (A17 to A19), Network (A20), Storage (A21, A22), Telemetry (A23),
Hosted (A24, A25), Images & Build (planned) (the 11 `build/` pages).

**Proposed `AGENTS.md` §6** (applied by the last architecture wave, A5c,
when the pages exist):

| Subsystem / task | Read first | Then |
|---|---|---|
| Whole system, crate map, process vs VM mode | `architecture/overview.md` | `architecture/isolation.md`, `architecture/testing.md` |
| Isolation, jailer, Firecracker, vsock | `architecture/isolation.md` | `core/overview.md`, `runtime/overview.md` |
| Cold start, golden snapshot | `architecture/cold-start.md` | `core/overview.md` |
| `vam-core`, vendored SDK | `core/overview.md` | `architecture/isolation.md` |
| `vamd`: startup, config, TLS, restart | `orchestrator/overview.md` | `operate/configuration.md` (developer tab) |
| HTTP API, auth, rate limits, paging | `orchestrator/api.md` | `reference/api` (generated) |
| Sessions, runs, threads, steer and stop | `orchestrator/session.md` | `runtime/inbox-outbox.md`, `storage/turn-commits.md` |
| Users, keys, ownership | `orchestrator/user-isolation.md` | |
| Catalog: agents, skills, playbooks, versions | `orchestrator/catalog.md` | `authoring/` (developer tab) |
| Guest runtime (`vam-runtime`) | `runtime/overview.md` | `runtime/agent.md`, `runtime/tools.md` |
| Agent loop, harness, compaction | `runtime/agent.md` | `authoring/harness.md` |
| Tools, host tools, MCP | `runtime/tools.md` | `tools/` (developer tab) |
| Inference (`vam-inference`) | `runtime/inference.md` | |
| Playbooks, gates, approvals | `runtime/workflow.md` | `authoring/playbook-spec.md` |
| Permissions, Cedar | `security/permissions.md` | `authoring/permissions.md` |
| Secrets, egress, network | `security/secrets-proxy.md`, `network/overview.md` | `integrations/egress.md` |
| Receipts, evidence, signing | `security/run-receipt.md` | `security/receipts.md` |
| Storage, workspaces (`vam-storage`) | `storage/overview.md` | `storage/turn-commits.md` |
| Telemetry (`vam-telemetry`) | `telemetry/overview.md` | `telemetry/event-catalog.md` |
| Hosted brokers (HSP), `vam.web` | `hosted/overview.md` | `hosted/web.md` |
| Contracts, proto, codegen | `spec/overview.md` | `proto/vam/v1/` |
| Tests and scenarios | `architecture/testing.md` | `e2e/README.md` |
| Image building (planned) | `build/overview.md` | `build/rootfs.md` (the one shipped part) |

Plus one sentence: "Pages in `design/docs-archive/` are design intent, not
behaviour; never cite them as what the node does."

#### (2) Disposition of every current page

207 pages today (206 after W2 deletes `network/ebpf.md`). "Keep as is" means
the text stays and the page gets `status` and `proves` (and loses a
hand-written Status note). "Keep and fix" includes the 18 pages rebuilt in
place above. "Move to design" sends the file to
`design/docs-archive/<section>/<page>.md` (outside the lint) with a one-line
header "Moved from the docs site on <date>: design intent, not built", and
adds a redirect. "Retire" deletes the page after its true content is merged
into the named page, and adds a redirect to that page.

**Counts**

| | Keep as is | Keep and fix | Move to `design/` | Retire, redirect | Total |
|---|---|---|---|---|---|
| Developer tab (incl. `index.mdx`, cheat sheet) | 15 | 10 | 0 | 5 | 30 |
| Architecture tab, outside `build/` | 6 | 20 | 22 | 107 | 155 |
| Already decided: `build/` (IT-3.6), `network/ebpf` (IT-3.2) | 11 (planned banner) | — | 10 (`design/vam-build/`) | 1 (W2) | 22 |
| **Total** | **32** | **30** | **32** | **113** | **207** |

**For the owner's word: 22 moves and 112 retirements** (the build and eBPF
ones are already approved). A wave may keep a listed page if it turns out
accurate and fits, or swap a page between "move" and "retire" (both leave
the site with a redirect), and says so in its PR; it never removes a page
that is not on these lists.

*Keep as is (21).* Developer: `getting-started/macos-lima`, `windows-wsl`,
`users-and-roles`; `authoring/model`, `agent-package`, `agent-spec`,
`publish-skill`, `run-spec`, `playbook-spec`, `playbook-tutorial`;
`tools/tool-runtimes`; `integrations/jira-assistant-walkthrough`;
`tutorials/jira-assistant`, `bun-assistant`; `reference/agent-cheat-sheet`.
Architecture: `orchestrator/workspace-files`, `workspace-files-upload`;
`runtime/benchmark-results` (H1 owns it); `security/run-receipt`;
`storage/turn-commits`; `hosted/web`. Developer pages here may still grow
(the outline in (3) adds sections); none has a wrong claim found.

*Keep and fix (30), with the wrong claims.*

| Page | Wrong today |
|---|---|
| `index.mdx` | `:6` "zero-trust egress", `:41` "eBPF networking" |
| `getting-started/introduction` | eBPF and isolation lines (W2 list); isolation in one paragraph linking the security page |
| `getting-started/quickstart` | `:25-27` `cargo run -p vam-orchestrator` (a VM node refuses without `VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH`, `VAMD_JAILER_BIN`; no examples without `VAMD_EXAMPLES_DIR`) |
| `getting-started/installation` | `:92` `http://127.0.0.1:8080`; `:142` "Warm Pool: 4 ready microVMs" |
| `getting-started/cli` | `:464` "ready warm-pool count" |
| `getting-started/project-status` | `:19` jailer planned, `:21` "~115 ms" and HTTP API, `:27` `vam-build` "Fully runnable", `:30` docs "up-to-date" |
| `authoring/skill-manifest` | title and sidebar "SKILL.md"; the code reads `SKILLS.md` only (`skill_package_admit.rs:233-238`) |
| `tools/writing-tools` | `:22,37` uid 1000, gid 100, root operations "strictly prevented": unproven, no scenario |
| `security/secret-management` | `:182-192` `VAMD_SECRET_MASTER_KEY` and an ephemeral key; the code reads `VAMD_SECRETS_MASTER_KEY` and keeps a 0600 key file (`secrets.rs:17-18`) |
| `security/hitl-lifecycle` | Cedar framed as the primary model; no always allow/deny, once-only, timeout, restart (gaps); retitled "Approvals", URL kept |
| `security/overview` (moves to the Developer tab as the launch security page) | `:35` SVID "injected into Guest", `:60` "custom Seccomp BPF filters via Jailer", `:86,91` `spiffe/` and `seccomp/` modules that do not exist |
| `architecture/overview` | crate states; eBPF (W2) |
| `architecture/cold-start` | `:39,96` "~115 ms" without the lane figure first |
| `core/overview` | "Alpha" crate role; planned illustrative code where the code exists (`MicroVmConfigBuilder`, `VamCoreError`, `Vmm` in `vam-core/src/`) |
| `spec/overview` | conventions spread over 4 pages with "Alpha" cautions |
| `orchestrator/overview` | `:11-12` "Today it provides a SQLite-backed ... store ... The future `vamd`" |
| `orchestrator/api` | `:12` TLS "parsed but plain HTTP is used in this alpha slice", default `127.0.0.1:8080` |
| `orchestrator/session` | design decisions ("Decision O5") presented as the schema; wave checks against `session.rs` |
| `orchestrator/user-isolation` | `:6` "Phases 1–10 are shipped on `feat/catalog-studio`" |
| `runtime/overview` | "Alpha" crate role |
| `runtime/agent` | "Instantiating the Agent" design text; check against `agent.rs` |
| `runtime/tools` | `:7` "fully implemented ... memory limits": confinement unproven |
| `runtime/inference` | "Measured" section: the U3 follow-up was never run |
| `runtime/workflow` | `:2,8` "Leased Task Executor", "planned": playbooks and gates are shipped |
| `security/secrets-proxy` | `:7` "fully implemented and verified"; its proof (`vault-secret-is-not-in-the-guest`) has not run on the lane |
| `network/overview` | `:7` "fully implemented, verified" (W2 rewrites the eBPF part) |
| `storage/overview` | `:7` "fully implemented"; `:10` "transactional, POSIX-compliant"; `:52` "guest FUSE frontend" (no FUSE code) |
| `telemetry/overview` | "the **planned** shared observability engine": the crate ships OTLP, CloudEvents, audit, spool |
| `telemetry/event-catalog` (listed in the Developer tab's Reference, URL kept) | names checked against `events.rs`; E10 adds six rows |
| `hosted/overview` | phase language ("Phases 1–6 are implemented") |

*Move to `design/docs-archive/` (22), with the redirect target.*
`architecture/design-principles` → `architecture/overview`;
`architecture/session-lifecycle` (its own banner: "planned design") →
`orchestrator/session`; `core/ratelimit` (the Firecracker rate limiter is
always `None`) → `core/overview`; `orchestrator/router` (a UDS-to-vsock
gRPC bridge for external clients, not built), `orchestrator/scheduler`
(NUMA `cpuset`, no code), `orchestrator/admission` (capacity barriers, no
code) → `orchestrator/overview`; `security/spiffe` (says not implemented),
`security/seccomp` (two tiers of generated profiles, not built; the true
line goes to A2) → `architecture/isolation`; `storage/fuse`,
`storage/backends` (cloud adapters), `storage/sync` (S3 sync),
`storage/encryption` → `storage/overview`; `network/modes` (direct mode),
`network/tap`, `network/nat`, `network/dns` (the Hickory proxy has no
caller), `network/traffic-control` → `network/overview`;
`hosted/first-party` (files and RAG), `hosted/resources` (no FileRef
runtime), `hosted/triggered-runs` (overlaps `design/hsp-triggers.md`, linked
from it), `hosted/roadmap` → `hosted/overview`; `telemetry/roadmap` →
`telemetry/overview`.

*Retire with a redirect (112), grouped by target.*

- → `getting-started/concepts` (new): `getting-started/platform-tour`
  (`:46,57,61` eBPF, netns per session, FUSE), `spec/glossary`.
- → `getting-started/installation`: `core/prerequisites`.
- → `integrations/egress` (new): `integrations/egress-proxy-deepdive`.
- → `authoring/permissions` (new): `security/cedar-and-hitl` (Cedar as the
  primary model; permissions as data replaced it).
- → `architecture/overview`: `architecture/component-model` (`:7` "all
  crates except `vam-core` represent planned logical boundaries"),
  `architecture/deployment-model`, `getting-started/workspace`
  (`:57-59,67-76` SPIFFE, FUSE, S3, eBPF).
- → `architecture/isolation`: `architecture/trust-boundaries` (`:85` netns per
  VM), `architecture/firecracker` (`:7` "No native Firecracker code exists"),
  `core/security-model`, `security/threat-model`, `spec/vsock-rpc`.
- → `architecture/cold-start`: `getting-started/coldstart-baseline`,
  `orchestrator/pool`, `core/snapshot`.
- → `architecture/testing`: `core/testing`, `orchestrator/testing`,
  `network/testing`, `runtime/testing`, `security/testing`,
  `storage/testing`, `telemetry/testing`.
- → `spec/overview`: `spec/conventions`, `codegen`, `common`.
- → `core/overview`: `core/lifecycle`, `config`, `jailer` (cgroup limits
  claimed), `vmm`, `api-client`, `vsock-device`, `health`, `errors`,
  `invariants`.
- → `orchestrator/overview`: `orchestrator/daemon`, `health`,
  `reconciliation`, `draining`, `invariants`.
- → `orchestrator/api`: `orchestrator/errors`.
- → `orchestrator/session`: `orchestrator/lifecycle`.
- → `orchestrator/user-isolation`: `orchestrator/security`.
- → `orchestrator/catalog` (new): `spec/agentic-spec`, `spec/skills`.
- → `runtime/overview`: `runtime/init`, `vsock-server`, `errors`,
  `invariants`.
- → `runtime/agent`: `runtime/instruction-template`, `runtime/context`.
- → `runtime/tools`: `runtime/primitives`, `host-tools`, `mcp`.
- → `runtime/inference`: `spec/model-policy`.
- → `runtime/workflow`: `runtime/hitl`, `runtime/evals`, `spec/workflow`,
  `spec/hitl`, `spec/evals`, `security/hitl-crypto`.
- → `security/permissions` (new): `security/cedar` ("fully implemented",
  Cedar as the main path), `security/agent-autonomy-and-governance` (cites a
  `runtime.rs` that does not exist), `security/errors`,
  `security/invariants`, `spec/security`.
- → `security/secrets-proxy`: `security/key-management` (SVID keys),
  `security/pii-redaction` (guest-output PII redaction not built).
- → `network/overview`: `network/architecture`, `contracts`, `lifecycle`,
  `security`, `telemetry`, `errors`, `invariants`, `ipam`, `policy` (`:13`
  plan language).
- → `storage/overview`: `storage/architecture`, `backend-interface`,
  `data-format`, `overlay`, `cache`, `checkpoint`, `quota-gc`,
  `consistency`, `errors`, `invariants`, `architecture/drive9`,
  `runtime/drive9-fs`.
- → `telemetry/overview`: `telemetry/architecture`, `otel`,
  `trace-context`, `cloudevents`, `streaming`, `reliability`, `metrics`,
  `cost-accounting` (H1 edits it), `latency`, `audit-ledger`, `privacy`,
  `retention`, `errors`, `invariants`, `orchestrator/telemetry` ("will
  export ... once Phase 1 lands"), `runtime/telemetry` ("will observe"),
  `spec/events`, `spec/telemetry`, `security/audit-log`.
- → `hosted/overview`: `hosted/architecture`, `protocol`, `hooks`,
  `capability-manifest`.

Redirects live in one file, `docs/redirects.json` (old slug → new slug,
kind `retired` or `design`), read by `astro.config.mjs` and by the lint
(IT-3.21), so no redirect is hand-copied twice.

#### (3) Developer Docs outline

Audience: the public. The reader's path: **install → first agent → run a
node → govern (permissions, approvals, secrets, egress) → receipts → API
and CLI reference.** 38 pages in the tab: 23 existing (10 of them fixed),
13 new, 2 listed from elsewhere by sidebar with their URL kept; plus the
generated CLI and spec references if approved (open questions 5, 6) and a
"Talking to a running task" page once E10 Phase 3 is on the base.

| Group | Page | Action | Teaches | Inventory rows | Fed by |
|---|---|---|---|---|---|
| Start | `getting-started/introduction` | fix | What VAM is, who it is for, isolation in one paragraph with a link to the security page | — | itself, `architecture/overview` |
| | `getting-started/concepts` | new | Agent, skill, playbook, run, workspace, thread, session, approval, receipt, node | — | `platform-tour`, `spec/glossary` |
| | `getting-started/limits` | new (honest page) | What VAM is not for: one Linux host with KVM, one org, process mode isolates nothing, no image builder, no direct network mode or kernel egress filter, no FUSE or cloud storage, session start about 225 ms, no certification; "where open models still lose" from H1's rows if published, else a `planned` section | Process mode | old IT-3.5, `runtime/benchmark-results` |
| | `runtime/benchmark-results` | sidebar only | Published benchmark rows | Benchmark | H1 |
| | `getting-started/project-status` | fix | What runs today, crate by crate, pointing at the security page and the feature register | all | itself |
| Install | `getting-started/installation` | fix | Host requirements (KVM, kernel, Firecracker and jailer binaries owned by root), building or installing `vam` and `vamd`, rootfs and kernel, first start, `vam config set-ca`, remote CLI | Start a node; VM paths; TLS | `core/prerequisites`, `orchestrator/daemon` |
| | `getting-started/macos-lima` | keep | Lima path | Mac host | itself |
| | `getting-started/windows-wsl` | keep (or `stale` if the wave cannot confirm the steps) | WSL path | Windows host | itself |
| First agent | `getting-started/quickstart` | fix | From a running node to a signed receipt: login, a model key, one agent, one chat, one approval, `vam receipt verify` | Run spec; approvals; receipt | itself |
| | `authoring/model` | keep | Drafts, release, versions, URN origin | Agent spec; URN origin | itself |
| | `authoring/agent-package` | keep | Package layout, `.vam/SYSTEM.md` | Instruction layers | itself |
| | `authoring/agent-spec` | keep | `agent.json` fields; allowed models; links permissions | Agent spec; agent check; allowed models | itself, `orchestrator/catalog` |
| | `authoring/run-spec` | keep, extended | `run.json`, `vam run`, dry run, workspace and thread, model pin, `vam thread` | Run spec; dry run; workspaces and threads; workspace pin | itself, `tutorials/bun-assistant` |
| | `authoring/harness` | new | `inputs.harness` switches and profiles, what the receipt prints; with H1, named profiles | Harness; compaction | `runtime/agent`, `vam-spec/src/harness.rs` |
| | `authoring/skill-manifest` | fix | `SKILLS.md`, `manifest.json`, required secrets, `load_skill` | Skill package; `load_skill`; required secrets | itself |
| | `authoring/publish-skill` | keep | Publish, release, version a skill | Publish a skill | itself |
| | `authoring/playbook-spec` | keep | Activation, inputs, gates and guards | Playbooks | itself |
| | `authoring/playbook-tutorial` | keep | One playbook end to end, `vam playbook test --repeat` | Playbook tests | itself |
| Tools | `tools/builtins` | new | The built-in tools (9 today, 12 with E10), typed argument errors, the 60 KiB host-tool bound, loop detection | Built-in tools; typed errors; output bound | `runtime/primitives` |
| | `tools/writing-tools` | fix | The tool protocol; confinement stated as proved (or as not proved) | Custom tools; confinement | itself |
| | `tools/tool-runtimes` | keep | Runtime templates | Custom tools | itself |
| | `tools/mcp` | new | `vam mcp connect/update/list/disconnect/refresh`, OAuth tokens, the Linear example | MCP connections | `runtime/mcp`, `examples/linear-mcp-assistant` |
| | `integrations/web` | new | Run the web broker, grant `web_search` and `web_fetch`, what the fetch refuses | Web search and fetch | `hosted/web` |
| | `integrations/hosted-brokers` | new | Write and register an HSP broker (TypeScript echo and files examples) | Hosted brokers; TS examples | `hosted/protocol`, `examples/hsp-*-ts` |
| Run a node | `operate/configuration` | new | Every `vamd` flag and variable read from code: default, effect, and which ones loosen a secure default and are reported by node and receipt (jailer, `--insecure-dev`, `VAMD_RATE_LIMIT`, idle TTL, egress passthrough); TLS; master key; example seeds; restart recovery; approval timeout | Install and node (11 rows); rate limits; approval timeout | `orchestrator/daemon:82-109`, `daemon.rs`, `bin/vamd.rs`, `tls.rs`, `rate_limit.rs`, `ttl.rs`, `secrets.rs` |
| | `getting-started/users-and-roles` | keep, extended | Bootstrap, login, users, roles, keys, deactivate, password reset, publish grant, cross-user 404 | Auth and users | itself |
| | `operate/telemetry-export` | new | OTLP, CloudEvents, OpenLineage, the gRPC stream (loopback, no TLS), spool and retention, timing lines | Telemetry (6 rows) | `telemetry/otel`, `cloudevents`, `streaming`, `retention` |
| Govern | `security/overview` | fix (launch security page; URL kept) | "What a default node does": one row per claim (claim, proof, smoke or lane, the setting that turns it off and that node and receipt report), "first lane run pending" for the three W0 scenarios, no `internet` setting means allow, stated limits (8 MiB scrub buffer, gRPC telemetry without TLS, tool confinement unproven), Planned, Removed: kernel-level egress filtering | Isolation; secrets; egress; receipts | old IT-3.3, A2 |
| | `authoring/permissions` | new | Tool and internet rules, presets, effective view, `vam policy explain`, `vam agent migrate`, Cedar as the legacy path | Permissions; Cedar; internet rules | `security/cedar-and-hitl`, `security/cedar` |
| | `security/hitl-lifecycle` | fix, retitled "Approvals" | Ask, decide, amend, `--always allow` and `--always deny`, once-only for released agents, timeout, survives restart; with E10, questions | Approvals (5 rows) | itself, `runtime/hitl` |
| | `security/secret-management` | fix | Vault, scopes, kinds, the master key and key file, encoded-form redaction, import from env | Secrets (5 rows) | itself |
| | `integrations/egress` | new | How a tool reaches the internet: guest proxy, placeholders, per-session CA, internet rules and their default, the IP floor, passthrough hosts, response cap | Egress (6 rows) | `integrations/egress-proxy-deepdive`, `network/policy` |
| Receipts | `security/receipts` | new | `vam receipt show/verify`, `vam node keys`, what the receipt says and does not, `vam evidence export/verify` | Receipts and evidence (4 rows) | `security/run-receipt`, `playbook-tutorial` §7 |
| Tutorials | `tutorials/jira-assistant`, `integrations/jira-assistant-walkthrough`, `tutorials/bun-assistant` | keep (both Jira pages; checked against `examples/`) | Worked examples | — | themselves |
| Reference | `reference/api` | new, generated | The HTTP API from `docs/api/openapi.yaml` | every API surface | `openapi.yaml` |
| | `getting-started/cli` | fix, retitled "CLI guide" | Tasks by command group; adds `vam mcp`, `vam thread`, `vam receipt`, `vam node keys`, `--always`, `--all` | Scripting contract; all CLI rows | itself |
| | `telemetry/event-catalog` | sidebar only | Event names and payloads | Run events | itself |

`index.mdx` stays the landing page (fixed, see (2)); the cheat sheet stays
in the third tab, refreshed against the references.

**The generated API reference.** The source stays `docs/api/openapi.yaml`
(compiled into vamd, `api/mod.rs:71`, served at `/openapi.yaml` and `/docs`,
`:91-92`). The wave (a) adds the missing logical routes listed in the
corrections (and E10's two when it lands); (b) adds
`crates/vam-orchestrator/tests/openapi_drift.rs` with
`openapi_matches_router`: one `pub const API_ROUTES: &[(&str, &str)]` beside
the router lists every logical route (method, templated path); the test
fails when a spec operation is not in the list or a listed route is not in
the spec, and drives each listed route through the real router with
placeholder ids, failing when the answer is the "no route" marker (a new
router `fallback` returning 404 `{"code":"NOT_FOUND","detail":"no route"}`
and the catch-all dispatchers' unknown-suffix arm returning the same), so a
route added to the router but not to the list is caught when the catch-all
dispatchers are exercised by their unit test over `API_ROUTES`; (c) renders
the file as `reference/api` with the `starlight-openapi` plugin if it
supports Starlight 0.41 and Astro 7, else a static page embedding the same
pinned Scalar build (version and SRI) as vamd's `/docs`; (d) runs the drift
test in `docs.yml` too, since an `openapi.yaml`-only PR skips `rust.yml`.

**Style and audience rules** (in `e2e/README.md` beside the lint rules, and
read by every developer-docs wave):

- Plain words, short sentences, the reader's task first. No marketing
  ("zero-trust", "military-grade", "blazing").
- Every command is run in the wave against a process-mode node
  (`scripts/e2e/run.sh` builds one; the mock model needs no key) and its
  output pasted verbatim (secrets and ids shortened, marked `…`). A step only
  a microVM can show cites its lane scenario instead of pasting output.
- Every security sentence names its proof inline (scenario or test) or says
  it is not proved. A setting that loosens a secure default says that the
  node and the receipt report it.
- Nothing about unbuilt features except on a page or section marked
  planned; no roadmaps, phase numbers or branch names on public pages.
- Every `shipped` developer page lists its proofs in `proves` (lint rule
  `docs-dev-proof`) and every inventory row names its developer page (rule
  `docs-feature`).

#### (4) Items and waves

- [ ] **IT-3.21 Docs plumbing: feature register, proof rules, redirects.**
  (a) `e2e/docs-features.yaml` seeded from the 90-row inventory: `id`,
  `area`, `surface`, `dev` (page), `arch` (page), `proves` or `unproven:
  <reason>`. (b) Rule `docs-feature`: each row's pages exist (or are listed
  as to-be-written in the row with `pending_page: true` until their wave);
  a row whose pages left the baseline must have them `shipped` and holding
  its proofs. (c) Rule `docs-dev-proof`: a `shipped` page in the Developer
  Docs directories (`developer:` list in `e2e/docs-claims.yaml`) names at
  least one proof, claim word or not. (d) `docs/redirects.json` (empty) read
  by `astro.config.mjs` into Astro `redirects`; rule `docs-redirect`: each
  entry's old page is gone, its target exists, and no page links to a
  redirected URL. (e) `design/docs-archive/README.md` (empty index) and one
  line in `design/README.md`. (f) The style rules in `e2e/README.md`.
  *Files:* `crates/vam-e2e/src/docs.rs`, `tests/docs_check.rs`,
  `e2e/docs-features.yaml`, `e2e/docs-claims.yaml`, `e2e/README.md`,
  `docs/redirects.json`, `docs/astro.config.mjs`, `design/docs-archive/README.md`,
  `design/README.md`. *Accept:* a failing fixture per new rule; `check`
  clean with the baseline unchanged; `pnpm build` green; `check` prints
  `features: 90, proven 80, with a developer page 29`.
  `Decide: NOW — proof 3 (security page; the owner's "where it is proved" made mechanical), cost 1 (one wave, vam-e2e and data files, no product surface)`
- [ ] **IT-3.22 Architecture: system pages.** A1 to A4 written; the 22
  pages whose target is one of them retired or moved (component-model,
  deployment-model, `getting-started/workspace`; trust-boundaries,
  firecracker, `core/security-model`, `security/threat-model`,
  `spec/vsock-rpc`; `getting-started/coldstart-baseline`,
  `orchestrator/pool`, `core/snapshot`; the 7 `testing` pages; moved:
  design-principles, session-lifecycle, `security/spiffe`,
  `security/seccomp`); their redirects. `architecture/isolation` is read
  from `vam-core/src/jail.rs`, `vmm.rs`, `vam-orchestrator/src/isolation.rs`,
  `vam-vsock/src/frame.rs` and the guest egress code, not from other pages.
  *Accept:* baseline 207 → 183; each written page `shipped` with its
  proofs; `check`, `pnpm build` green; the PR lists each removed page and
  where its true lines went (or that it had none).
  `Decide: NOW — proof 3 (the isolation page is the contributor twin of the security page), cost 1`
- [ ] **IT-3.23 Architecture: core and contracts.** A5, A6; 9 core pages
  and 3 spec pages retired, `core/ratelimit` moved. *Accept:* baseline
  183 → 168.
  `Decide: NOW — proof 2 (owner: content matches the code; agents read these first), cost 1`
- [ ] **IT-3.24 Architecture: host daemon.** A7 to A11 and the two
  workspace-files pages (status and proofs only); 10 pages retired
  (including `spec/agentic-spec`, `spec/skills`), 3 moved; E10's steer,
  queue and stop rows added to the register if E10 is on the base.
  *Accept:* baseline 168 → 149.
  `Decide: NOW — proof 2, cost 1`
- [ ] **IT-3.25 Architecture: guest runtime.** A12 to A16,
  `runtime/benchmark-results` (status only), E10's two pages if on the base
  and not yet marked; 16 pages retired (including `spec/model-policy`,
  `spec/workflow`, `spec/hitl`, `spec/evals`, `security/hitl-crypto`); H1's
  three rows added if H1 is on the base. *Accept:* baseline 149 → 127.
  `Decide: NOW — proof 2, cost 1`
- [ ] **IT-3.26 Architecture: security and network.** A17 (new), A18, A19
  (status only), A20; 16 retired, 5 moved. *Accept:* baseline 127 → 103.
  `Decide: NOW — proof 3 (the permissions default and the secrets rows feed the security page), cost 1`
- [ ] **IT-3.27 Architecture: storage and hosted.** A21, A22 (status only),
  A24, A25 (status only); 16 retired (including `architecture/drive9`,
  `runtime/drive9-fs`), 8 moved. *Accept:* baseline 103 → 75.
  `Decide: NOW — proof 2, cost 1`
- [ ] **IT-3.28 Architecture: telemetry, the tab's sidebar, `AGENTS.md`
  §6.** A23, `telemetry/event-catalog` (fix); 19 retired (including
  `orchestrator/telemetry`, `runtime/telemetry`, `spec/events`,
  `spec/telemetry`, `security/audit-log`), `telemetry/roadmap` moved; the
  tab's sidebar as in (1); the proposed `AGENTS.md` §6. *Accept:* baseline
  75 → 53 (32 after IT-3.6); `AGENTS.md` §6 names only pages that exist.
  `Decide: NOW — proof 2 (AGENTS.md §6 points sessions at these pages), cost 1`
- [ ] **IT-3.29 Developer: start, install, run a node.** `introduction`,
  `concepts` (new; retires `platform-tour` and `spec/glossary`), `limits`
  (new; the honest page), `project-status`, `index.mdx`, `README.md`; then
  `installation` (retires `core/prerequisites`), `quickstart`,
  `macos-lima`, `windows-wsl`, `users-and-roles`, `operate/configuration`
  (new), `operate/telemetry-export` (new); the tab's sidebar groups.
  *Accept:* baseline 32 → 20; `docs-feature` clean for the install, node,
  auth and telemetry rows.
  `Decide: NOW — proof 3 (install launch proof; the honest page launch proof, half), cost 2 → two waves (D1a, D1b), cost 1 each`
- [ ] **IT-3.30 Developer: govern and receipts.** `security/overview` (the
  launch security page), `authoring/permissions` (new; retires
  `security/cedar-and-hitl`), `security/hitl-lifecycle`,
  `security/secret-management`, `integrations/egress` (new; retires
  `egress-proxy-deepdive`), `security/receipts` (new). *Accept:* baseline
  20 → 15; `docs-feature` clean for permissions, approvals, secrets,
  egress, receipts, isolation.
  `Decide: NOW — proof 3 (security page and receipt: launch proofs), cost 1`
- [ ] **IT-3.31 Developer: authoring, tools, reference.** The eight
  `authoring/` pages and `harness` (new); `tools/builtins`, `tools/mcp`,
  `integrations/web`, `integrations/hosted-brokers` (new),
  `writing-tools`, `tool-runtimes`, the three tutorials; then the API
  reference (missing paths, `openapi_matches_router`, rendered page), the
  CLI guide, the cheat sheet, and the generated CLI reference (open
  question 5). A "Talking to a running task" page if E10 Phase 3 is on the
  base. *Accept:* baseline 15 → 0; the drift test fails on a scratch route
  (shown in the PR, then dropped) and passes.
  `Decide: SLICE — proof 2 (owner: every feature presented; the OpenAPI misses about 9 routes), cost 2 (three waves, a Rust test, a docs plugin) → slice: authoring and tools pages, the API reference and the CLI guide now (cost 1 each); generated CLI reference in D4 on the owner's word; generated spec reference its own wave D5 on the owner's word; utoipa CUT for 0.1.0`
- [ ] **IT-3.32 Close the ratchet.** Delete `e2e/docs-baseline.txt`; `status`
  required in `content.config.ts`; `docs-feature` strict (every register row
  has a shipped developer page with its proof, or `unproven` with a line on
  the security page); zero `stale` pages; re-run the claim audit over pages
  other lanes edited during the phase (E10, H1); the honest page's "where
  open models still lose" filled if H1's rows are published. *Accept:* the
  phase exit's "`check` clean on the base with no ratchet file left".
  `Decide: NOW — proof 3 (the phase exit), cost 1`

**Waves** (replace rows 2 to 6c of the phase table and the re-plan's waves;
each wave one session in its own worktree, base `feat/isolation-truth`,
branch `it-3/wave-N`, plan-only first turn, draft PR `IT-3 WN: …`, ready
after local checks)

| Wave | Items | Baseline after (turns green) | Repo | Model | Budget | Depends on | Shared surfaces | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 2 | IT-3.2 (unchanged, building) | 207 (`network/ebpf.md` deleted) | vam | Sonnet low | 75 min | 1 | as approved | [~] | |
| A0 | IT-3.21 | 207 (new rule fixtures) | vam | Sonnet low: extends W1's rules in the same file, data files | 90 min | 2; this plan approved | `vam-e2e/src/docs.rs` (no other lane); `astro.config.mjs` (W2 edits it: after W2); `skip-vm-lane` (lint only) | [x] | — |
| A1 | IT-3.22 | 183 | vam | Sonnet medium: the isolation page states each isolation claim against code and scenario status (security floor judgement) | 150 min | A0 | `architecture/`, the 7 `testing` pages: no other lane | [x] | — |
| A2 | IT-3.23 | 168 | vam | Sonnet low | 90 min | A1 | `core/`, `spec/`: no other lane | [x] | — |
| A3 | IT-3.24 | 149 | vam | Sonnet low | 120 min | A2; **E10 Phase 3 on the base** | **HOLD while E10 Phase 3 is unreleased** (its branch edits `orchestrator/session.md`, `api.md`): E10 releases to `main`, the base takes `main`, then A3 | [x] | — |
| A4 | IT-3.25 | 127 | vam | Sonnet low | 120 min | A3 | **HOLD likewise** for `runtime/agent.md` (E10) and `runtime/benchmark-results.md` (H1); E10's `runtime/inbox-outbox.md`, `jig.md` | [x] | — |
| A5a | IT-3.26 | 103 | vam | Sonnet low | 120 min | A4 | `network/overview.md` (W2 edits it first: after W2) | [x] | — |
| A5b | IT-3.27 | 75 | vam | Sonnet low | 120 min | A5a | none | [x] | — |
| A5c | IT-3.28 | 53 | vam | Sonnet low | 120 min | A5b | `telemetry/cost-accounting.md` (H1 edits it: carry its lines into A23, or HOLD until H1 is on the base); `telemetry/event-catalog.md` (E10 adds six rows: after E10 is on the base); `AGENTS.md` §6 (any lane: **Low**) | [x] | — |
| B | IT-3.6 (unchanged) | 32 | vam | Sonnet low | 60 min | A5c | `build/`, `design/vam-build/`, sidebar | [x] | — |
| D1a | IT-3.29 (start) | 26 | vam | Sonnet low | 90 min | B | `README.md`, `index.mdx`, `project-status.md` (E10 and H1 closes edit project status: **Low**, rebase) | [ ] | |
| D1b | IT-3.29 (install, run a node) | 20 | vam | Sonnet medium: `operate/configuration` reads every flag and variable from code and says which loosen a secure default (locked decision 2); every command run on a process node | 150 min | D1a | `getting-started/*`, sidebar (**Low**) | [ ] | |
| D2 | IT-3.30 | 15 | vam | Sonnet medium: the launch security page; each row checked against code, scenario status and the operator setting | 150 min | D1b | `security/overview.md`: no other lane | [ ] | |
| D3a | IT-3.31 (authoring) | 7 | vam | Sonnet low | 120 min | D2 | `authoring/*` (E10 may document run inputs: **Low**, rebase) | [ ] | |
| D3b | IT-3.31 (tools, tutorials) | 2 | vam | Sonnet low | 120 min | D3a | none | [ ] | |
| D4 | IT-3.31 (reference) | 0 | vam | Sonnet medium: a Rust drift test and a router fallback, a docs plugin and lockfile, `docs.yml` | 150 min | D3b | `docs/api/openapi.yaml` (a lane adding a route edits it: **Low**); `api/mod.rs` fallback and `API_ROUTES` (**HOLD while E10 or H1 has an open PR in `api/mod.rs`**); runs `rust.yml`; `skip-vm-lane` | [ ] | |
| D5 | generated spec reference (open question 6) | 0 | vam | Sonnet medium: walks `vam-spec`'s descriptor pool, a freshness test | 90 min | D4; owner's word | `crates/vam-spec` (in the lane filter: `skip-vm-lane`) | [ ] | |
| Z | IT-3.32 | exit: no baseline, `status` required, register strict, zero `stale` | vam | Sonnet low | 75 min | D4 (D5 if approved) | `content.config.ts`, `vam-e2e/src/docs.rs`; `skip-vm-lane` | [ ] | |

Order as the owner set it: the architecture tab first (smaller per page,
and it unblocks agents through `AGENTS.md` §6), then the developer docs,
then the last wave that empties the baseline and makes `status` required.
The build-pages wave B sits between the two tabs because its pages are in
the Architecture tab. The clippy and dependency waves (C1 to C6, D) are
unchanged and run alongside, as the owner decided on 2026-10-29. Budget:
about 34 hours of docs wave time (A0 to Z with B, D5 excluded; the 2026-10-28 re-plan was about 29), in 16 waves. **Proof while
GitHub Actions is down (until 2026-11-01):** every wave proves locally with
`cargo run -p vam-e2e -- check` and `cd docs && pnpm install && pnpm build`
(A0, D4, D5, Z also `cargo test -p vam-e2e` or `-p vam-orchestrator --test
openapi_drift`); the orchestrator re-runs them and merges on that local
proof; the `docs` job is first seen running after billing returns.
Developer-docs waves run their commands against a process-mode node
(`scripts/e2e/run.sh`); the cloud container has no KVM, so VM-only steps
cite lane scenarios. Shared surfaces checked 2026-10-29 with the GitHub
tools: the only open PR in the repository is (jig design, unrelated);
E10 Phase 3 and H1 have no open PR but unreleased docs edits on their
branches (listed in the corrections), hence the A3, A4 and A5c holds.

#### (5) Open questions (defaults in bold)

1. **The lists in (2): approve the 22 moves and 112 retirements as listed.**
   A wave may keep a listed page if accurate, or swap move and retire; it
   never removes an unlisted page.
2. **The honest page lives in the Developer Docs** (`getting-started/limits`,
   group Start): it is for the public. The launch security page also lives
   there (`security/overview`, URL kept); `architecture/isolation` is its
   contributor twin and each links the other.
3. **Moved pages go to `design/docs-archive/<section>/<page>.md`**, unlinted,
   indexed in its README; pages that overlap a design of record link from it
   (`hosted/triggered-runs` from `design/hsp-triggers.md`).
4. **Redirects in `docs/redirects.json`**, one source for Astro and the lint;
   targets are pages, with an anchor only where the target heading is
   stable.
5. **CLI reference: generated, in D4**, from the clap tree by a hidden
   `vam __markdown` (a small walker over `clap::Command`, no new crate),
   committed as `reference/cli.md`, with a freshness test in `vam-cli`; the
   "CLI guide" stays hand-written tasks. Alternative: no generated page, the
   guide only.
6. **Spec reference: generated, in its own wave D5, on the owner's word**
   (`AgentSpec`, `RunSpec`, `PlaybookSpec`, `SkillManifest`, receipt, the
   harness switch table, walked from `vam-spec`'s descriptor pool with a
   freshness test). Without it, `spec/overview` links the `.proto` files,
   whose comments are the field reference.
7. **API reference rendering: `starlight-openapi` if it supports Starlight
   0.41 and Astro 7, else a static page with the pinned Scalar build**; the
   drift test as described; `utoipa` cut for 0.1.0.
8. **No `design` status value** (the re-plan's question 2): pages of design
   intent leave the site, so every page left is `shipped`, `planned` (the 11
   build pages, sections inside shipped pages) or, until Z, `stale`.
9. **E10 and H1 docs:** **the architecture waves A3, A4 and A5c wait for E10
   Phase 3 on the base** (its release is next on that lane); its two new
   pages get `status` in E10's release PR if the lint is on `main` by then,
   else in A4. A "Talking to a running task" developer page is written only
   once E10 is on the base.
10. **Commands in developer pages: run on a process-mode node and pasted**;
    a CI docs smoke that runs code blocks is backlog
    (`Decide: BACKLOG — proof 1, cost 2`).
11. **`SKILLS.md`:** the docs say `SKILLS.md` until the launch row
    "`SKILL.md` read alongside `SKILLS.md`" ships.
12. **Tool confinement** (uid 1000, scrubbed env, Landlock on x86_64;
    `vam-runtime/src/exec.rs:630-711`): **stated as not proved** on
    `tools/writing-tools` and the security page; a process-layer scenario
    (`tool-runs-as-an-unprivileged-user`, the shell prints `id -u` and fails
    to write outside the workspace) is a new item for the owner
    (`Decide: NOW — proof 3 (security page row), cost 1`) if he wants the
    claim kept.


**Phase close** (2026-11-02, owner: close now; developer docs become Phase 4)

Exit criteria (`design/isolation-truth.md` Phases table, row 3):
- `vam-e2e check` fails on a doc page that claims an unproven isolation property:
  **met** (W1; `docs_claim_without_proof_fails_check`; rules `docs-status`, `docs-proof`,
  `docs-proof-ref`, `docs-admonition`, `docs-baseline`, `docs-feature`, `docs-redirect`;
  a test proof must be a `tests/` file or a `#[test]` fn).
- Clippy gate green: **met** (C1 to C6; 642 warnings at the start, 0 now; `rust.yml` job
  `clippy`, `cargo +1.97.0`, default and `process-guest` features; `local.sh --full`).
- eBPF removed (decided 2026-10-28): **met** (W2; `rg -i ebpf crates proto AGENTS.md docs/src` empty).
- Build pages behind a planned banner: **met** (B; 11 planned, 10 moved to `design/vam-build/`).
- Dependency report: **met** (D; `design/isolation-truth.md` "Dependency pass (Phase 3)";
  unused `prost-types` removed; six Decide rows).
- Every page has an honest status, no exemption list: **partly**. The Architecture tab is
  rebuilt from the code (A0 to A5c, B). The exemption list went from 208 to 32; the 32 are the
  Developer-tab pages, `security/overview`, `core/prerequisites`, `spec/glossary` and
  `README.md`, which belong to the developer docs the owner is reviewing. Moved to Phase 4 with Z.

Scenarios (all `active`; 71 in the catalog, every one active):
- New and green: `docs-claim-without-proof-fails-check` (contract test),
  `egress-refuses-an-ungranted-host` (process; was pending, IT-3.34),
  `guest-has-no-route-off-the-vm` (lane), `vault-secret-is-not-in-the-guest` (lane),
  `oversize-response-with-a-secret-never-reaches-the-guest` (process, IT-3.33).
- Changed and green: `jailer-confines-the-vmm-process` (seccomp filter step, lane).

Findings fixed in the phase (security floor): oversize responses from a secret-bound host are
refused with a 502 (IT-3.33); egress denies reach the run and `denials.jsonl`, which had silently
skipped every CloudEvent denial (IT-3.34); `/docs` loads a pinned, integrity-checked Scalar
(IT-3.19). Findings recorded, not fixed: an agent with no `internet` setting resolves to allow
(backlog); tool confinement (Landlock, uid 1000) not proved; the always-true assertion at
`hsp_results.rs:545` (backlog).

Benchmark delta: no `design/benchmark.md` §1 line moves. Launch proofs: **security page**: the
proofs exist and are cited on the architecture pages (`architecture/isolation`,
`security/secrets-proxy`, `security/permissions`, `network/overview`); the launch page itself,
`security/overview`, is rewritten in Phase 4 (D2). **Honest page**: Phase 4.

**Team checklist** (a reading checklist; about 10 minutes; a browser and the docs site built
from `feat/isolation-truth` or `main` after the release)

Setup: `cd docs && pnpm install --frozen-lockfile && pnpm dev`, open http://localhost:4321.
1. Open Architecture, Isolation (`/architecture/isolation/`). For each layer, click the scenario
   or test named in "Proved by". You should land on a file in `e2e/scenarios/` or a test that
   exists. The "What is not proved" list names tool confinement and the open-internet default.
2. Open Security, Secrets (`/security/secrets-proxy/`). Confirm it cites
   `vault-secret-is-not-in-the-guest`, `secret-never-reaches-guest-encoded` and
   `oversize-response-with-a-secret-never-reaches-the-guest`, and states the 502 for an oversize
   response from a host that holds a secret.
3. Open Network (`/network/overview/`). It says brokered mode only, no eBPF, and no kernel-level
   egress filter, with the reason.
4. Open Storage (`/storage/overview/`). It says no FUSE layer, no cloud backends, no encryption
   at rest.
5. Open Images & Build (planned). Every page shows the "Planned" banner.
6. Open the old URL `/network/ebpf/`. It redirects to the network overview.
7. Open `/security/overview/` and `/getting-started/project-status/`. These are not rewritten
   yet (Phase 4); note anything that contradicts the pages in steps 1 to 4.

Report: pass, or the step number with a screenshot or the text that is wrong.


### Phase 4 — Developer docs (held for the owner)

**Depends on:** Phase 3. **Plan:** the developer-docs part of "Docs plan: architecture rebuild
and developer docs (proposed 2026-10-29)" in the Phase 3 section: waves D1a, D1b, D2 (launch
security page), D3a, D3b, D4 (generated API reference from `docs/api/openapi.yaml` with a drift
test), D5 (generated spec reference, owner's word), then Z (delete `e2e/docs-baseline.txt`,
make `status` required). Held for the owner's review of the outline (public pages). Starts with
the "developer pages must name a proof" lint rule deferred from A0. Exit: no exemption list;
the launch security page and the honest page published.

## Explicitly later

- Rust 1.99 clippy flags `result_large_err` in tonic-generated `vam-spec` code (99 errors in `vam.v1.rs`, seen on the first C6 CI run). From IT-3.10 (2026-10-31).
  `Decide: BACKLOG — proof 0, cost 1, the next toolchain bump adds a build.rs server_attribute/type_attribute allow`
- eBPF build: decided **remove** by the owner 2026-10-28 (Phase 3, IT-3.2); reopen only as its own design on one of the three triggers in design open question 2.
- Internet deny-by-default for agents: today an agent with no `internet` setting resolves to allow (`vam-security/src/permissions.rs:277-279`); private and metadata addresses are refused by the IP floor regardless. Found by IT-3 W0 (2026-10-28); owner: document it honestly in the Phase 3 docs waves, decide the default later (E8 or owner).
  `Decide: BACKLOG — proof 2 (security page egress row), cost 2 (product default change, app and receipt surfaces)`
- `/docs` Content-Security-Policy header (script-src limited to the pinned Scalar URL; needs a browser check of Scalar's inline styles). From IT-3.19 (2026-10-29).
  `Decide: BACKLOG — proof 1, cost 1, carry into a wave that touches api/mod.rs docs_handler`
- Bundle the Scalar viewer into vamd instead of the CDN (about 4.5 MB), so `/docs` works on air-gapped nodes. From IT-3.19 (2026-10-29).
  `Decide: BACKLOG — proof 1 (install proof for sovereign buyers), cost 1, revisit with the install slice`
- vam-security argument structs for `audit.rs` `record_decision`, `record_decision_with_telemetry` and `token.rs` `verify_capability_token` (10, 15 and 12 arguments; scoped `too_many_arguments` allows since IT-3 C1). Callers: 13 in vam-orchestrator, 1 in vam-runtime, 9 in vam-security, so the refactor crosses other lanes' crates.
  `Decide: BACKLOG — proof 0, cost 1, carry into a wave that already edits those callers`
- `crates/vam-orchestrator/tests/hsp_results.rs:545` `assert!(14_000 * 15 > 200 * 1024);` in `a_200_kib_markdown_file_arrives_fenced_and_capped_with_a_note` compares two constants, so it checks nothing about the file (the test's other asserts still run). Left unchanged under a scoped allow by IT-3 C3 (owner's call). Replace with a real check of the delivered size or cap.
  `Decide: BACKLOG — proof 1, cost 0, carry into a wave that touches hsp_results.rs`
- Vendored Firecracker SDK replacement (keep the fork; a thin in-tree client is about 1,000 lines and touches `vam-core`, vsock and the API path). From IT-3.9 (2026-10-30).
  `Decide: BACKLOG — proof 1 (finding 13), cost 3 (security-relevant paths, jailed re-proof)`
- tonic/axum/hyper/rustls stack: tonic 0.12 with prost 0.13, hyper 1, axum 0.7, axum-server, rustls 0.23, tower 0.5, as one move (`vam-spec` codegen, `vam-orchestrator`, `brokers/vam-web`, `vam-cli`). From IT-3.9.
  `Decide: BACKLOG — proof 1, cost 3 (TLS listener and client auth are security-relevant; needs HSP wire tests)`
- reqwest 0.11 to 0.13 in 7 crates. From IT-3.9.
  `Decide: BACKLOG — proof 0, cost 2, wait until H1 is clear of vam-inference`
- thiserror 1 to 2 (workspace pin; removes 2 duplicates). From IT-3.9.
  `Decide: BACKLOG — proof 0, cost 1, carry into a wave that already edits workspace dependencies`
- rand 0.8 to 0.9 (`vam-orchestrator`, `vam-telemetry`, `vam-vsock`). From IT-3.9.
  `Decide: BACKLOG — proof 0, cost 1, do after the tower move`
- webpki-roots 0.26 to 1 in `vam-network` (removes no duplicate; TLS root store of the egress proxy). From IT-3.9.
  `Decide: BACKLOG — proof 0, cost 0, take it with the reqwest or rustls move`
- TLS on the telemetry gRPC listener (loopback by default).
  `Decide: BACKLOG — proof 1, cost 1, carry into a wave that touches bin/vamd.rs listeners`
- `session.booted` `boot_duration_ms` is always 0 (`manager.rs:1581`, `:1816`).
  `Decide: BACKLOG — proof 1, cost 0, carry into a wave that touches the boot path`

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-10-21 | — | Tracker opened; base `feat/isolation-truth` cut from `main` |
| 2026-10-21 | 1 | Breakdown drafted (background Opus agent): 6 new scenarios (5 firecracker, 1 process), 7 items, waves 0-4, 11 open questions; awaiting owner approval. Scoreboard: 0 green, 0 red (none written), 0 fix cycles |
| 2026-10-21 | 1 | Owner approved the breakdown: all open-question defaults, TLS deploy note accepted; locked decisions 1-6 locked. Wave 0 started |
| 2026-10-22 | 1 | W0 merged (squash): six scenarios `pending`; runner forms `node: {fields}`, `plain-http: refused` (no key sent; tested), `vmm: jailed` (`under:` default `/srv/jailer`, pure `/proc` probe tested on fake trees); optional scenario `description`; lint rule: a `needs: process` scenario needs `mock:` only when it sends a model message. Orchestrator re-ran fmt, `cargo test -p vam-e2e`, `vam-e2e check` (17 scenarios), `--smoke` (10 pass, 2 red right reason) and the plain-HTTP scenario (red at then.0, `product`); CI green. Findings: vamd answers the keyless plain probe 401 with an API JSON body (not 200), still red for the right reason; for W3, the probe must not count an HTTP→HTTPS redirect followed to an API body as an API answer on plain HTTP (reqwest follows redirects by default). Scoreboard: 0 green, 1 red (right reason) run + 5 `check` only, 1 fix cycle (own self-test compile error), wall time about 10 min (plan 2, build 7) plus 4 min CI |
| 2026-10-22 | 1 | W1 merged (squash): **CI boots a microVM.** Advisory job `firecracker (microVM, advisory)` on hosted `ubuntu-latest` (KVM via udev rule); `scripts/e2e/firecracker-assets.sh` pins Firecracker/jailer v1.16.1 x86_64 (sha256…`) and kernel `vmlinux-6.1.155` (Firecracker CI `v1.15`, sha256…`, computed: no published sum), builds the rootfs from the PR's `vam-runtime` without Bun (`VAM_ROOTFS_SKIP_BUN=1`; dev and release builds keep Bun); `build-rootfs.sh` runner-relative (`-p vam-runtime` also stops feature unification pulling `openssl-sys` into the musl guest). 5 VM tests run with `--ignored` on the lane (4 `vam-core` + `test_vamd_real_firecracker_e2e`); none deleted; the 4 live-model tests stay ignored with the reason. Lane: cold 7m57s, warm 4-6 min (free-disk step 1.5-3.5 min of it); cold boot 29 ms, boot to registered 205-246 ms, ping RTT 0.8 ms, vamd boot to running 435-446 ms. Orchestrator read the diff and the lane log, sent back one fix (summary grep dropped the boot and registration rows). Known intermittent: `cargo test (workspace)` failed once at `-p vam-storage --lib` (a CI-yaml-only change of the green); passed on the one rerun and 6/6 local runs; test name not captured (proxy blocks the raw log). Scoreboard: 0 scenarios green (none planned in W1), 1 red (right reason), 5 VM tests green on a real VM, 4 fix cycles (pipefail masking, openssl in the musl build, summary grep; plus one rerun), wall time about 2h40m |
| 2026-10-22 | 1 | W2 `[~]` (head), **blocked**: GitHub Actions was unavailable; rerun once it was back. Built: additive `NodeIsolation` (field 13), one `isolation::describe()` from the running config, `vam node` line, runner `--firecracker` (selection by `needs`, preflight `guestMode: vm`, restore on). The session first wrote `run.sh` into the main checkout; the orchestrator moved it into the PR and cleaned the checkout. Lane run 1 (real microVMs, base `run.sh`; `vamd` started with a cleared env, so `guestMode: vm`): gate passes; boot and restore fail on `session.booted` missing from the run (product: `manager.rs:1581`, `:1816` append it under the session id, the run's event read is by run id); jailer red at then.0 class `env` (non-root runner cannot read Firecracker's `/proc/<pid>/root`); node red at then.0 class `product` (right reason). `vam-storage` intermittent identified: `test_vam_timeline_survives_commit_cycle`, "Disk quota exceeded" on the `test` runner (fix exists on E10: free-disk step). Awaiting owner: boot-event fix, jailer expectFail to `env` until W4, port the free-disk step. Scoreboard so far: 1 scenario passing on the lane (not yet flipped), 4 red, 1 lane cycle, wall time about 3h (mostly CI) |
| 2026-10-24 | 1 | Owner: GitHub Pro (3,000 Actions min/month, shared by all epics); CI unblocked, run 39752874905 re-run on head. Owner said yes to the three W2 calls: `session.booted` also on the run (two `manager.rs` sites), jailer `expectFail` to then.0 `env` until W4 runs the lane as root (only if the new lane run confirms it), E10's free-disk step in the `test` job. New item IT-1.8 and wave 2b (CI minutes: lane only on VM-path changes, trimmed free-disk, the three small jobs merged, no CI on draft PRs), after W2. From now on wave sessions open PRs as drafts and mark them ready after local checks |
| 2026-10-24 | 1 | W2 lane results on (real microVMs, golden bake OK): gate passes; `session.booted` now on the run (fix landed); restore scenario path corrected to `data.bootPath` (orchestrator, path-only); boot scenario `pending` at then.2 `product` (owner (a): the VM workspace is committed at session end, not run end; turns green with commit-on-stop or a run-end commit; E10 to know: the process layer does not show this gap); restore **parked** `pending` at when.1 `product` (owner): the restored guest never answers `ResumeFromSnapshotRequest`, vamd falls back to cold (about 19 s per session). New item IT-1.9 and wave 2c (golden restore on the lane, Sonnet medium), after 2b and before W4. CI notes for 2b: Docker Hub 502 on a cold rootfs build (add pull retries); `test_vam_timeline_survives_commit_cycle` EDQUOT recurred with the free-disk step (likely a size-limited temp dir; investigate) |
| 2026-10-24 | 1 | Restore is **intermittent**, not broken: on run 40072580312 every session restored (`bootPath: restore`, scenario passed), on the two runs before every session hit the reseed timeout and fell back to cold. Owner: restore scenario back to `active` with a known-issue note (no quarantine, no retry); merge W2 with the advisory lane possibly red on restore only; wave 2c (IT-1.9, now "restore is reliable", green on three consecutive lane runs) runs before 2b |
| 2026-10-24 | 1 | W2 merged (squash). `NodeStatus.isolation` from one `describe()` over the running config (installed in `vamd` from `config.use_jailer`, the value the VMM factory uses; uninstalled fallback claims jailer off and TLS off and warns); `session.booted` also on the run; runner `--firecracker` mode (default vamd, assets from env, restore on, selection by `needs`, preflight `guestMode: vm`); new `Scenarios (microVM)` step; firecracker job timeout 45; free-disk step in the `test` job. Final lane: gate and restore pass; boot red at then.2 (session-end commit gap), jailer red at then.0 `env`, node red at then.0 `product` (all right reason). Orchestrator re-ran fmt, isolation and boot-event unit tests, `cargo test -p vam-e2e`, `-p vam-cli`, `vam-e2e check`, `--smoke` (10 pass, 2 right reason) and read the isolation and boot-event code; CI 6/6 green after one rerun of `cargo test (workspace)` (the `vam-storage` EDQUOT intermittent; standing-down comment on the PR). Incidents: the session first wrote `run.sh` into the main checkout (moved into the PR); Actions billing block (owner moved to GitHub Pro). Scoreboard: 2 scenarios green on a real VM (gate, restore; restore intermittent), 3 red right reason, 6 lane cycles (2 lost to billing and Docker Hub), 2 reruns, wall time about 5h over three days (mostly CI) |
| 2026-10-24 | 1 | W2c plan approved (owner, option a). Root cause: not random. Every restore takes 8.6-10 s and the host waits a fixed 10 s for the guest's resume answer (`driver.rs:278`), so slower runners go over and fall back to cold. Side bug: `host_dial` has no timeout on its OK read. Fix: 30 s configurable reseed timeout, dial timeout, per-phase restore timing (host and guest), `lane_only` dispatch input; done = three consecutive green lane runs plus latency recorded. New item IT-1.10 (restore latency, about 100x over target) planned after W2c |
| 2026-10-24 | 1 | W2c cycle 1 (draft): 30 s reseed timeout, dial timeout, timing lines, `lane_only` dispatch. Restore failed for every session even at 30 s, so the "slow but answers" diagnosis was wrong. Host side is fast (snapshot load, patch and resume 22-40 ms; vsock dial accepted); the guest never answers. Orchestrator approved one diagnostics-only push (guest console capture, guest checkpoints in the resume handler); a third push needs the owner. Scoreboard: 0 moved, 1 cycle, about 20 lane minutes |
| 2026-10-24 | 1 | W2c cycle 2 (diagnostics): restore worked for all 5 sessions, reseed under 120 ms (dial 0-20 ms, response 24-92 ms); the 8.6 s is host-side session start. Failure is bimodal per golden bake (run 1 all timed out at 30 s; runs 2 and 40072580312 all fine): suspected non-deterministic snapshot point. Guest consoles were empty because the guest has no UART unless `VAM_SERIAL_CONSOLE=1`, logs were deleted on teardown, and the runner's cleared env dropped both switches. Owner chose option B: close W2c with a final push (pass the two switches through the runner; everything else as built), then W3 (TLS), then IT-1.10 (deterministic bake and restore latency, Sonnet high) before W4. Scoreboard W2c: 0 moved (restore still intermittent), 2 cycles, about 35 lane minutes |
| 2026-10-24 | 1 | W2c merged (squash): reseed timeout 30 s (`VAM_RESEED_TIMEOUT_SECS`), a timeout on the vsock dial (was unbounded), host and guest restore timing lines, `VAM_KEEP_VM_LOGS` keeps Firecracker logs, runner forwards `VAM_SERIAL_CONSOLE` and `VAM_KEEP_VM_LOGS`, `lane_only` dispatch input, lane diagnostics and restore-latency summary. Final CI 6/6 green, no rerun; restore passed on 2 of the PR's 3 lane runs (intermittent per bake, stays `active` with the known-issue note). Orchestrator read driver.rs and vmm.rs, re-ran fmt, the reseed and keep-log tests, `cargo test -p vam-e2e` and `vam-e2e check`. Finding for IT-1.10: guest `eprintln!` checkpoints never reach the serial console (PID 1 does not route stderr to /dev/console), so wire that first. Scoreboard: 0 scenarios moved, 3 lane runs (about 45 lane minutes), 2 fix cycles plus the closing push, wall time about 3h |
| 2026-10-24 | 1 | W2b merged (squash): `changes` job (plain `git diff`, no third-party action) gates the firecracker lane on VM-path files; push to `main` and dispatch always run it; `rustfmt`, `example JSON protos` and `release gate` merged into one `checks` job (**required-check names changed: owner to update branch protection if it lists them**); no job runs on draft PRs; `docker pull` retries 3x. Draft PR triggered no billed run. Ready run 40082685747: changes 1, checks 2, smoke 3, test 8, lane 13 billed minutes (27 with the lane; about 14 for a non-VM PR). Filter checked locally on three samples. Free-disk timing: android 66 s (12 GB), dotnet 13 s (5 GB), CodeQL 12 s (2 GB), ghc 0 s; trim not applied (under a minute per lane run). Follow-ups for the next push that touches `rust.yml`: remove the `TMPDIR on /mnt` and `Disk of TMPDIR` steps (on this runner `/mnt` is the root filesystem, so they do nothing and the comment is wrong); drop `/opt/ghc` from the free-disk lists. The `vam-storage` EDQUOT intermittent is not fixed (real cause: root disk pressure). Orchestrator recommendation, awaiting the owner: workspace tests stay on every non-draft code PR (sessions run only changed-crate tests); affected-crate testing decided from per-crate times. Scoreboard: 1 CI run, 0 fix cycles, wall time about 40 min |
| 2026-10-24 | 1 | W3 (TLS) plan approved by the owner with three additions: the CLI's `https://` hint probe sends no key; a warning before the CLI sends a key over plain HTTP to a non-loopback host; self-signed cert valid 2 years, auto-renewed with 30 days left. Choice: `axum-server` 0.5.1 `tls-rustls` (only new crate; rustls 0.21 tied to axum 0.6, upgrade belongs to the Phase 3 dependency pass). The W2b `TMPDIR` no-op steps are removed in the same push |
| 2026-10-24 | 1 | W3 merged (squash): **TLS on vamd by default.** `axum-server` 0.5.1 `tls-rustls` (only new crate; openssl and native-tls left the lockfile). Operator pair from `VAMD_TLS_CERT`/`VAMD_TLS_KEY`, else a self-signed ECDSA P-256 leaf in `<state>/tls/` (key 0600 written atomically, dir 0700, 2-year validity, renewed with 30 days left, reused across restarts). Fails closed on a half-set, unreadable or mismatched pair (in-memory handshake self-check) and on `--insecure-dev` with an operator pair. `--insecure-dev` serves plain HTTP with a WARN; `apiTls` comes from the resolved transport. CLI: `https://` default, `vam config set-ca` / `VAM_CA_FILE`, guided errors, credential-free hint probe, a warning before a key goes in clear to a non-loopback host. Runner speaks TLS and trusts `<state>/tls/ca.pem`; the plain-HTTP probe follows no redirects. Lima and bench scripts pass `--insecure-dev`; six doc pages updated; the W2b `TMPDIR` no-op steps removed. Orchestrator read `tls.rs`, `serve_with`, the vamd wiring and the CLI transport; re-ran fmt, 17 `tls` tests, 5 `tls_refusals`, CLI transport tests, `cargo test -p vam-e2e`, `vam-e2e check`, the plain-HTTP scenario (pass) and `--smoke` (11 pass, 1 right reason). CI green; the first lane attempt hit the known bad-bake restore (IT-1.10), the rerun passed; node reports `apiTls: true`. **Deploy note for the release:** the team instance needs an operator cert (`VAMD_TLS_CERT`, `VAMD_TLS_KEY`); app and CLI URLs become `https://<host>:8080`; the old `http://` stops answering. Scoreboard: 1 scenario turned green (`vamd-refuses-plain-http-by-default`), lane 2 pass / 3 right reason, 1 push, 1 infra rerun, about 45 billed minutes, wall time about 1h |
| 2026-10-24 | 1 | W3b (IT-1.10) plan approved by the owner with: a golden-key cache keyed on path, device, inode, size, mtime and ctime; a lane-only bake soak (`VAM_GOLDEN_BAKE_SOAK=5`, never publishes); budget 180 min. Evidence: bad bakes are paused about 0.3 s later in guest time, after the guest's flush loop and egress connector have started dialling the host. Fix: defer both until resume during the bake, and a host-to-guest `PrepareForSnapshot` handshake before pause; a bake that never gets ready aborts and sessions cold-boot. Latency: about 5.5 s of each session start is re-hashing the kernel and rootfs for the golden key on every start; cached once. No `manager.rs` or proto edits |
| 2026-10-24 | 1 | W3b merged (squash): **golden restore reliable.** Root cause confirmed: the host paused the bake VM while its flush loop and egress connector were mid-dial. The bake guest now defers both until resume and answers a host `PrepareForSnapshot` from its accept loop; the host pauses only after the ok reply and EOF; a guest that never registers or never gets ready aborts the bake (no hang, nothing published, sessions cold-boot). Golden key cached on path, dev, ino, size, mtime, ctime (files changed in the last 2 s not cached). Soak `VAM_GOLDEN_BAKE_SOAK` (default off, cap 20, scratch dir only). Results: restore green on 4 of 4 lane runs (D1-D3, the PR run, docs and diagnostics only after D3); soak 20/20; every session `bootPath: golden`. **Latency: session start with restore median 224 ms (min 152, max 610, n=15) on the CI lane, down from about 5.5 s; the per-session key hash was 10.4 s uncached and is now 0 ms. VM-ready (load, patch, resume, reseed) about 60-120 ms. The 50 ms SLO is not met; what remains is materialize 34 ms, reseed about 70 ms, broker setup about 80 ms (E10/manager.rs-adjacent, not planned).** Six docs pages now state what each latency number measured; `design/launch-0.1.0.md` "~115 ms" and `design/mast-framework.md` left for the owner. Orchestrator read the guest bake path and `PrepareForSnapshot` arm, the bake abort and soak gating; re-ran fmt, golden (23), driver (11), vam-runtime bin (10) tests and `vam-e2e check`. CI green after one infra rerun (the `vam-storage` EDQUOT). Notes for W4: the bake builds `/tmp/vam-vsock-<id>.socket` itself, and `vm_state.bin` records the absolute disk path; both need handling inside the jailer chroot. The soak runs on every lane run (+1.5-2 min); drop it after W4. Scoreboard: 1 scenario made reliable (`vm-restores-from-the-golden-snapshot`), 4/4 lane runs, 0 fix cycles, about 74 billed minutes, wall time about 4h45m (plan turn included) |
| 2026-10-24 | 1 | W4 plan (tested against the real v1.16.1 jailer as root, no KVM) split by the owner: **4a** jailer on by default with jailed cold boot, opt-out and preflight refusals, teardown and sweep, lane `vamd` as root, the two jailer scenarios green; **4b** bake and restore inside the jail. Owner decisions: during 4a `vm-restores-from-the-golden-snapshot` is `pending` (jailed nodes cold boot), back to `active` in 4b; for 4b the golden-key cache drops ctime for kernel and rootfs (keeps dev, ino, size, mtime), since preflight refuses kernel or rootfs writable by anyone but root. Follow-ups recorded: per-VM jail uid, a root-only run dir instead of `/tmp` symlinks, cgroup resource limits |
| 2026-10-24 | 1 | Owner: the "~115 ms restore" claim in `design/launch-0.1.0.md` and `design/mast-framework.md` replaced with the W3b measurement (about 225 ms session start on the CI lane, VM ready in about 60-120 ms). Commit signing in the cloud container failed for a while during W4a and is back |
| 2026-10-24 | 1 | W4a merged (squash): **jailer on by default (jailed cold boot).** `--no-jailer` / `VAMD_USE_JAILER` opt-out (WARN, node reports `jailer off`); preflight refuses a VM node that cannot jail, naming `--no-jailer`; uid/gid 65400 (never 0); cgroup v2; vam-core `jail.rs` stages hard links into a flat in-jail layout (shared files root-owned 0644, per-VM files uid-owned); jailed nodes keep vsock sockets in a root-only `<jailer base>/run` (hosted runner `/tmp` is tmpfs: no hard links), linked into the jail 0600 uid-owned; no `--no-seccomp` (tested); teardown by pid file incl. adopted VMMs and the cold-fallback path, stale-jail sweep at start; jailed nodes skip the golden bake and cold-boot (logged). Lane runs vamd and the probe as root via `sudo env` (hosted sudo ignores `-E`). `vmm: jailed` probe: the jailer pivots into a new mount namespace, so `/proc/<pid>/root` reads `/`; it now also matches the root directory's dev and inode against `<under>/<exec>/<id>/root` (orchestrator read it; tests reject unjailed, outside-base, uid 0, shared pid and mnt ns). `manager.rs`: three one-line socket-path swaps plus a doc comment (E10 overlap). Lane: `jailer-confines-the-vmm-process` and `node-reports-the-isolation-it-runs` PASS and `active`; gate PASS; `vm-boots-and-registers-over-vsock` passed on both lane runs of (file present after the run on a jailed cold-boot node) and was flipped `active` by the orchestrator with the owner's word (metadata only; merged without a new CI run, run 40117296189 cancelled); restore `pending` until 4b. **Finding:** the "no committed HEAD after the run" gap seen in W2 appeared on unjailed nodes that restored from the golden snapshot, not on jailed cold boot; 4b must recheck it with restore back on. Jailed VM tests pass (boot to registered 165-206 ms). Scoreboard: 3 scenarios turned green, 1 pending (restore, planned), about 95 billed minutes (about 35 lost to runner surprises), wall time about 4h40m |
| 2026-10-24 | 1 | W4b plan approved by the owner: a per-session sparse placeholder `/data0.ext4` instead of linking the shared golden disk into the jail (the snapshot reopens the drive read-write, so a link would hand every restore jail a writable golden disk); jailed and unjailed golden keys differ (snapshots record in-jail vs host drive paths); the bake kills its Firecracker before exporting `/mem.snap` and `/vm.state` (O_NOFOLLOW, nlink 1, fchown and fchmod on the fd, root-owned 0644); key cache drops ctime (approved earlier); soak back on the lane; zero `manager.rs` edits planned. Release to `main` after 4b closes Phase 1 (owner) |
| 2026-10-24 | 1 | W4b merged (squash): **golden bake and restore inside the jailer.** The bake VM is jailed; vamd kills it, then exports `/mem.snap` and `/vm.state` (O_NOFOLLOW, one link, fchown root and fchmod 0644 on the fd, inode re-checked after the link; copy on EXDEV). Restore stages mem, state and rootfs read-only, a per-session sparse placeholder `/data0.ext4` (the snapshot reopens data0 read-write; a root-owned golden disk gives EACCES, proven with the real jailer), loads with `/v.sock`, PATCHes data0 to the session image, unlinks the placeholder, then resumes: no shared golden inode enters a session jail. Jailed golden keys carry a tag (in-jail vs host paths); a jailed node refuses VMM-writable golden files and cold-boots. Key cache drops ctime. **Bug found and fixed:** the vendored SDK ignored HTTP status, so a refused `load_snapshot` or drive PATCH returned Ok; 4xx/5xx now errors with Firecracker's `fault_message`. Orchestrator read `export_from_jail`, `reclaim_file`, the SDK status check; re-ran fmt, vam-core (29), vam-firecracker (6), golden (29) tests and `vam-e2e check`. Lane: all five firecracker scenarios PASS, every session `bootPath: golden` jailed, soak 5/5 on both runs; jailed restore median 172 and 233 ms. Boot scenario stays green with restore on (the W2 gap was the pre-W3b restore stall). CI green after one EDQUOT rerun. Scoreboard: 1 scenario back to `active` (restore), all six phase scenarios active and green, 0 fix cycles, about 56 billed minutes, wall time about 2h |
| 2026-10-24 | 1 | **Phase 1 closed in the base** (all waves merged, all six scenarios active and green, exit met on the lane). Team checklist written. Release to `main` approved by the owner after 4b; release PR next |
| 2026-10-24 | 1 | opened (feat/isolation-truth → main, merge commit, deploy note). Its `checks` job ran the release gate build check for the first time: green. `cargo test (workspace)` hit the `vam-storage` EDQUOT intermittent again **with 28 GB free on `/`**, so the cause is not root-disk pressure: `/tmp` on hosted runners is a separate tmpfs (found in W4a) and `tempfile::tempdir()` writes there. Likely fix for a later CI change: set `TMPDIR=$RUNNER_TEMP` (root ext4) in the `test` job. One rerun used |
| 2026-10-24 | 1 | **Released to `main`** (merge commit). CI on the release: all six jobs green (the release gate build check ran for the first time; one `vam-storage` EDQUOT rerun, and one rerun the orchestrator caused by pushing a docs commit to the release branch). Base fast-forwarded to `main`. Phase marked released, checklist out (playbook §6); archive after the team reports a pass |
| 2026-10-26 | 2 | Breakdown drafted (background Opus agent): 4 new scenarios (3 process, 1 firecracker; `receipt-isolation-line-matches-node-config` covered by E12, not added), 8 items IT-2.1 to IT-2.8, waves 0-4, 12 open questions; benchmark lines P6 (0 secret bytes in guest output) and P9 (`hostile-skill`) to prove, no baseline run; conditional HOLD on W3 (`api/mod.rs` vs E10 Phase 3); awaiting owner approval. Scoreboard: 0 green, 0 red (none written), 0 fix cycles |
| 2026-10-26 | 2 | Owner approved the breakdown: all open-question defaults; W3 carries `skip-vm-lane` (lane runs on W1 and W4 only); receipt `rateLimits` field kept for locked decision 2, W3 sequenced after H0's receipt changes; W0 on Sonnet medium with test-only fixture adapters. Wave 0 started |
| 2026-10-26 | 2 | W0 merged (merge commit): three scenarios `pending`, each red at then.1 class `product` (secret: base64 form in the thread; frame: 200 KB result became a short transport error, no truncation note; flood: 300 x 200, no 429). `guest-proxy-never-binds-wildcard` moved to W1 (needs W1's busy-port boot flag). Runner: world pieces `secret` and published skill package (`PUT /v1/skills` does not reach a session; the piece publishes and releases a tar.gz), `flood` step and expect, `secret-absent` (names form and place, never the value), thread `maxBytes`, approvals paging, `{{urn:}}`/`{{name:}}` placeholders. Orchestrator: `host_tools_fixtures.rs` (`secret_forms`, `bulk_output`) registered only when `VAMD_E2E_MOCK_SCRIPTS` is loaded; `secret_echo` untouched (W2). Note for W2: that gate is a runtime env var, not a build feature (same switch as the mock model). Orchestrator read the fixtures and the brokers line; re-ran fmt, `-p vam-e2e`, fixtures and brokers tests, `check` (34 scenarios), the three scenarios and `--smoke` (24 pass, 3 right reason). CI green, lane skipped. Scoreboard: 0 green, 3 red (right reason), 0 fix cycles, wall time about 25 min build plus 9 min CI |
| 2026-10-26 | 2 | W1 plan approved by the owner. Findings: holding 127.0.0.1:3128 cannot make today's code bind a wildcard (the 0.0.0.0:3128 fallback fails EADDRINUSE too; the proxy is left unix-only while tools are told 3128), so the scenario is red because the advertised proxy does not answer; the real wildcard path is a loopback bind failing for another reason, covered by a VM-build unit test and the no-wildcard guard. Restored guests keep the bake cmdline, so the busy-port flag goes into `bake_boot_args` with a golden-key tag on mock nodes only. Cold-boot path skipped (no `manager.rs` edit). H0 follow-up: `vam-runtime/src/instruction.rs:289` tells JS skills to use `127.0.0.1:3128`, wrong after a fallback |
| 2026-10-26 | 2 | W1 merged (merge commit): **guest proxy binds loopback only.** One bind path: preferred loopback, then a free loopback port (advertised to tools via `advertise_proxy_url`), then unix socket only; a non-loopback preferred address is never bound; `0.0.0.0:3128` gone from production guest code (guard test). Busy-port hook `vam_test_hold_proxy_port=1` in `bake_boot_args` plus a golden-key tag, on mock-script nodes only (real nodes' keys unchanged); PID 1 holds 127.0.0.1:3128 at proxy start, after restore. No `manager.rs`/`vamd.rs` edit; cold-boot path not covered (owner). Lane: push 1 red at then.1 `product` (right reason), 7 other VM scenarios green; push 2 8/8 green, every session `bootPath: golden`. Orchestrator read the bind path, PID 1 hold, golden key change; re-ran fmt, egress (15), guest guards (3), golden (32). CI 5/5 green. Minor note: with no TCP listener the guest's `proxy_url()` still defaults to 127.0.0.1:3128 (guest-internal only; egress still goes through the host). Deploy: rootfs rebuild. Scoreboard: 1 scenario green (`guest-proxy-never-binds-wildcard`), 2 red right reason, 0 fix cycles, 2 lane runs, wall time about 55 min |
| 2026-10-26 | 2 | W2 plan approved by the owner: one shared `SecretRedactor` in `vam-security` (feature `secret-forms`, aho-corasick already in the lockfile; guest build unchanged) over every host-tool path and egress `scrub_response`; raw, base64 std/url padded/unpadded at three alignments, hex, percent, JSON-escaped, 12-byte raw windows, whitespace-split; redact then cap; 60 KiB host-tool budget with a host truncation note; oversize reply becomes an error envelope; `secret_echo` to test nodes. Over-redaction accepted (safe side). Backlog (stated limits): encoded partial runs, reversed/XOR/double-encoded forms, compressed bodies. `Decide: BACKLOG — proof 1, cost 1, trigger "a benchmark or audit finds one of these forms leaking"` |
| 2026-10-26 | 2 | W2 merged (merge commit): **encoded secrets redacted; host-tool output fits the frame.** `vam-security::secret_forms::SecretRedactor` (feature `secret-forms`; one Aho-Corasick pass plus a whitespace-stripped pass) on the generic, hosted and MCP paths (MCP parts every MIME, hosted binary files, adapter error text, which was raw) and egress `scrub_response`. 60 KiB `HOST_TOOL_FRAME_BUDGET_BYTES` (const-asserted 4 KiB under the 64 KiB frame), redact then cap, host truncation note, final `fit_response_to_frame`; `write_control_envelope` turns an oversize reply into an error envelope (code 8) and keeps Port 52 open. `secret_echo` on test nodes only, docs rows moved. Deviations accepted: short percent/JSON forms always exact; 12-byte windows starting or ending on whitespace skipped; MCP text shares the 60 KiB budget with its files. Pre-existing gap found: an egress response over the proxy's 8 MiB scrub buffer streams unscrubbed (`Decide: BACKLOG — proof 2, cost 1, carry into the next wave touching vam-network/src/secrets.rs`). Orchestrator read `secret_forms.rs` in full, the call sites and the frame guard; re-ran fmt, corpus (17), scrub (11), host_tools+brokers (106), both scenarios (pass). CI green, lane skipped. Scoreboard: 2 scenarios green (`secret-never-reaches-guest-encoded`, `host-tool-output-fits-frame`), 1 red right reason (flood), 0 fix cycles, wall time about 1h10m |
| 2026-10-26 | 2 | Base took `main` (Harness H0 release) through (merge commit): conflicts in `vam-e2e/src/scenario.rs` (W0 runner structs vs H0 `harness_tests`, both kept) and the workstreams README; resolution checked against the auto-merge (markers plus one brace only); local fmt, `-p vam-e2e`, `check` (39 scenarios), `--smoke` (30 pass, flood red right reason). Merged before the long jobs under the 2026-10-25 rule (both sides already green); CI watched after. E10 has no build wave open in `api/mod.rs`, H0 is released: W3 has no hold |
| 2026-10-26 | 2 | CI after merge (run 40389703965): all green: checks, smoke, workspace tests, firecracker lane (H0's guest changes plus IT-2 W1 on a real microVM). W3 plan in, awaiting the owner (receipt per-run vs at-issue; 20 vs 30 req/s) |
| 2026-10-27 | 2 | W3 plan approved by the owner: `api/rate_limit.rs` (per-key 20/s burst 100, create 2/s burst 20, per-IP 5/s burst 20 charged only by failed auth and login; 429 with `Retry-After` and `X-VAM-Rate-Limited-Reason`; health, OpenAPI and docs exempt; `X-Forwarded-For` ignored; untracked IPs past the 50k cap share one overflow bucket); `VAMD_RATE_LIMIT=off` opt-out; `rateLimits` on `NodeIsolation` (field 5) and on the receipt **recorded per run** (small additive migration; locked decision 3); default raised to 30/s only if the measured runner peak exceeds ~7/s; cursor paging (50, max 1000) on runs, sessions, approvals with `pagination.nextPageToken`; CLI `--all` |
| 2026-10-27 | 2 | W3 merged (merge commit): **API rate limits on by default; list endpoints paged.** `api/rate_limit.rs`: per-key 30/s burst 100 (raised from 20: measured runner peak 33 req/s on the bootstrap key during `--smoke`), create 2/s burst 20, per-IP 5/s burst 20 charged only by failed auth (response marker set after auth) and login; a throttled IP with a valid key still passes; untracked IPs past 50k share one overflow bucket; 429 `RESOURCE_EXHAUSTED` with `Retry-After` and `X-VAM-Rate-Limited-Reason`; health, OpenAPI, docs exempt; `X-Forwarded-For` ignored; `VAMD_RATE_LIMIT=off` and tuning env vars, WARN when off. `rateLimits` on `NodeIsolation` (5) and `ReceiptIsolation` (5), recorded per run in a nullable `run_isolation.rate_limits` column (old runs read `unknown`). `api/paging.rs`: `pageSize` 50 (max 1000), opaque kind-bound `pageToken`, keyset queries, `pagination.nextPageToken` on runs, sessions, approvals; CLI `--all`. `api/mod.rs`: router layers, connect-info, three list handlers only. No existing test needed higher limits. **Owner decision 2026-10-27: commit signing is optional; while the container's signing service is down, commits are made with a per-command `-c commit.gpgsign=false` (no config change).** Orchestrator read `rate_limit.rs` and the migration; re-ran fmt, 40 lib tests (rate_limit, paging, run_facts), 5 API tests, the flood scenario (pass). CI green, lane skipped. Local-only failure `hsp_uds::uds_socket_acl_deny_fails_fast` (sandbox uid 0; green in CI). Scoreboard: 1 scenario green (`api-rate-limits-a-flood`); all four Phase 2 exit scenarios now active and green; 0 fix cycles (1 blocked on signing), wall time about 2h |
| 2026-10-27 | 2 | W4 plan approved by the owner (first relay in the playbook §3 step 4 format): idle-session sweep in the existing 60 s loop (`VAMD_SESSION_IDLE_TTL_SECS`, default 86400; only `off` disables; invalid or `0` warns and keeps the default; never ends a session with a live, parked or approval/gate-waiting run); approvals' existing 1 h EXPIRED re-tested only; 14 request-path panic sites to typed errors, 11 startup sites to clear exits, `cfg_attr(not(test), deny(clippy::unwrap_used, clippy::expect_used))`; VM nodes require `VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH`, `VAMD_JAILER_BIN` (jailer on) and refuse to start naming each missing var; jailer and firecracker binaries owned by root or the vamd uid and not group/other-writable; `VAMD_EXAMPLES_DIR` unset loads no seeds |
| 2026-10-27 | 2 | W4 merged (merge commit): **idle sessions end; no panics in the daemon; VM nodes need explicit paths.** `ttl.rs` in the 60 s sweeper (`VAMD_SESSION_IDLE_TTL_SECS`, default 86400, only `off` disables; idle = no live or parked run, no undecided approval or gate, no run awaiting its receipt, last activity older than the TTL; terminate plus lock release; injected-clock tests). 25 panic sites (14 request, 11 startup incl. 2 `panic!`) to 0: typed errors and an `or_exit` helper; `cfg_attr(not(test), deny(unwrap_used, expect_used))` on the lib, `vamd` also denies `panic`. VM nodes refuse to start naming each missing `VAMD_FIRECRACKER_BIN`, `VAM_ROOTFS_PATH`, `VAMD_JAILER_BIN`; jailer preflight refuses binaries not owned by root or the vamd uid, or group/other-writable; no `/home/ec2-user` left outside the vendored SDK. Deviations accepted: examples var is `VAMD_EXAMPLES_DIR` with `VAM_EXAMPLES_DIR` as alias, dev-tree fallback only under cargo (in-process tests use it), the runner sets the var; `manager.rs` `examples_base_dir` returns `Option` (a few lines, flagged for E10); the lane's assets script chowns the binaries to root (vamd runs as root there, the runner owns them); Lima script exports the paths and drops the symlinks; `build-rootfs.sh` Bun cache under XDG. Orchestrator read `ttl.rs`, the ownership check, the scripts and `manager.rs`; re-ran fmt, ttl/daemon/hsp (58), jail (21). CI green incl. the lane on the first run. Scoreboard: 0 scenarios (none planned), 0 fix cycles, 1 lane run, wall time about 1h20m |
| 2026-10-27 | 2 | **Phase 2 closed in the base** (all waves merged; four exit scenarios active and green; benchmark lines P6 and P9 recorded). Team checklist written. Release PR to `main` awaits the owner's word |
| 2026-10-27 | 2 | opened on the owner's word (feat/isolation-truth → main, merge commit, deploy note). Base 30 ahead, 0 behind; merges cleanly |
| 2026-10-27 | 2 | `checks` red: the release gate (runs only on PRs into `main`, so no wave saw it) failed twice. (1) `check-guest-build.sh` still required the default guest binary to contain `0.0.0.0:3128`, the wildcard fallback W1 removed: inverted so the gate now fails if it returns (tightens). (2) W4's startup error text named `process-guest`, a process-mode marker in the default vamd: reworded. Fix wave (merge commit, Sonnet low, 1 cycle); orchestrator read the 2-line diff and re-ran `check-release-gate.sh` locally (PASS) before merging under the 2026-10-25 rule. From now on, any wave touching guest code or vamd strings runs the release gate locally. Workspace tests, smoke and the lane were green on |
| 2026-10-27 | 2 | CI all green after (run 40454640566): checks with the release gate, workspace tests, smoke, firecracker lane. Awaiting the owner's word to merge |
| 2026-10-27 | 2 | **Released to `main`** (merge commit) on the owner's word; CI on the release all green (run 40454640566, release gate included). Base fast-forwarded to `main`. Phase 2 `released, checklist out`; team checklist in the Phase close block |
| 2026-10-28 | 3 | Breakdown drafted (background Opus agent): Scenario delta of 5 (contract test `docs-claim-without-proof-fails-check`; new `egress-refuses-an-ungranted-host` process, `guest-has-no-route-off-the-vm` and `vault-secret-is-not-in-the-guest` firecracker; `jailer-confines-the-vmm-process` changed: seccomp filter), expected to pass on the base as proofs of shipped claims (red = finding); 10 items IT-3.1 to IT-3.10; 16 waves (W0, W1-W6c docs, C1-C6 clippy, D dependencies), C4/C5/D hold on E10 Phase 3 and H1 open PRs; 12 open questions (security page = `security/overview.md`, claim word list with no opt-out, 10 build pages to `design/` on the owner's word, `docs.yml` workflow, clippy toolchain pin). Measured: clippy 642 warnings reported, 440 unique (orchestrator 259, firecracker 59, runtime 35, storage 25, core 16, network 13, others 33); `cargo tree -d` 34 duplicated crates; eBPF in 25 pages (70 lines) plus 4 `vam-network` sources, `AGENTS.md`, `SKILLS.md`, a proto comment and the sidebar; 40 hand-written Status notes; claim words hit 153 of 207 pages. No benchmark §1 line moves; launch proofs: security page (done at close), honest page (half, waits on B1). Awaiting owner approval. Scoreboard: 0 green, 0 red, 0 fix cycles |
| 2026-10-28 | 3 | Owner approved the breakdown: all open-question defaults (Q3: keep 11 build pages behind the planned banner, move 10 to `design/vam-build/`; Q5: fold `network/ebpf.md` into the overview, delete it, redirect); W0 stays Sonnet medium. W0 started |
| 2026-10-28 | 3 | W0 plan approved by the owner with two changes: branch from the base, not `main`; bind the secret to the session in `vault-secret-is-not-in-the-guest` if an existing piece allows it. New world piece `agent-with-shell-and-no-internet` accepted. Finding: unspecified agent internet resolves to allow; owner: document it, backlog row added. Docs-test exit-0 half deferred to W1. W0 building |
| 2026-10-28 | 3 | W0 merged (merge commit) on local proof (orchestrator re-ran fmt, `cargo test -p vam-e2e`, `check`, the egress scenario): CI could not start on the PR (GitHub account billing block), so the merge is under the 2026-10-25 rule (test-only change). `docs_claim_without_proof_fails_check` ignored, red for the right reason (`--docs` unknown). `egress-refuses-an-ungranted-host` `pending` at then.3 class product: **finding**, both hosts are refused and the thread says so, but `egress.decided` events are filed under the session id, so a run's events never list the denies. `guest-has-no-route-off-the-vm`, `vault-secret-is-not-in-the-guest` and the seccomp step in `jailer-confines-the-vmm-process` landed `active` but **unproven on the lane** (billing); the first lane run proves them or flips them to findings. Scoreboard: 0 green (VM three unproven), 2 red right reason, 0 fix cycles, about 1 h 20 min |
| 2026-10-28 | 3 | W1 plan approved by the owner with two changes: the README default comes from `--root` (`<root>/../README.md`); `docs.yml` caches the Rust build like `rust.yml`. Exemption list starts at 208 files (207 pages + README), can only shrink. Owner: W1 may merge on local proof while a GitHub billing block stops CI; watch `docs.yml` on its first run after billing returns. W1 building |
| 2026-10-28 | 3 | W1 merged (merge commit) on local proof (owner: allowed while billing blocks CI). Orchestrator re-ran: fmt, `cargo test -p vam-e2e` (12 docs tests), `check` with docs exit 0, `pnpm build` 208 pages. `check --docs` with rules `docs-status`, `docs-proof`, `docs-proof-ref`, `docs-admonition`, `docs-baseline`; claim words in `e2e/docs-claims.yaml`; exemption list `e2e/docs-baseline.txt` at 208 (shrink-only, `repo_docs_are_honest`); Starlight schema rejects a bad `status`; `PageTitle` banner (Planned, Out of date, Proved by); new `docs.yml` workflow, **not yet seen running** (billing). Scoreboard: 1 green (`docs_claim_without_proof_fails_check`), 1 red right reason (egress), 0 fix cycles, about 1 h. Owner 2026-10-28: waves 2-6c to be re-planned after W1 as a developer-docs-first pass (every shipped feature: what it is in the architecture pages, how to use it in the developer docs, where it is proved), architecture pages updated too; clippy and dependency waves unchanged |
| 2026-10-28 | 3 | Docs waves re-plan drafted (background Opus agent), awaiting the owner: feature inventory of 90 shipped features in 15 areas from the code (CLI, routes, config, spec inputs, scenarios): 80 with a proof (4 of them weak: 3 W0 lane scenarios not yet run, the egress gate on unit tests), 29 with a usable developer page, 14 partial, 47 none or wrong. Developer Docs target 39 pages (28 today; 13 new, `security/overview` listed in the tab, 2 moved to the Architecture tab, 1 merged); architecture pages: 15 fixed first in W3, 125 updated to the code, 18 planned. New items IT-3.11 to IT-3.18 (feature register `e2e/docs-features.yaml` and a developer-page proof rule; security page; three developer-docs waves; generated API reference slice; architecture in seven waves; close the ratchet); waves 2, 2b, 3, 4a-4d, 5a-5g, 6, 7 (about 29 h, was about 10 h). Found: `docs/api/openapi.yaml` exists, is served by vamd at `/docs`, and misses about 9 routes; `security/secret-management` names the wrong master-key variable; rate limits are on no page. 7 open questions (generator, `design` status value: default no, deletes, honest page now, register, command verification, `SKILL.md`). Scoreboard: 0 green, 0 red, 0 fix cycles |
| 2026-10-29 | 3 | Owner on the docs re-plan : developer docs are public-facing and get their own careful plan; not approved as written. Owner decided: (1) rebuild the Architecture & RFCs tab from the code (about one page per subsystem, from the 90-feature inventory), accurate pages kept, pure design-intent pages moved to `design/`, wrong pages retired with redirects, instead of fixing 143 pages one by one; planned together with the developer docs in one planning pass, brought to the owner before any page is written. (2) Meanwhile run W2 (eBPF removal), a small item pinning the Scalar script on vamd `/docs` (version and integrity hash; `api/mod.rs:278`) with the spec `servers` URL fixed, and the clippy waves C1-C3, D, C4-C6 as approved. The baseline exit and the honest-page proof move to the docs plan |
| 2026-10-29 | 3 | Owner: GitHub Actions is down until Nov 1. Until then validation runs in the cloud session (the orchestrator re-runs each wave's checks locally), and PRs that are not heavy code changes merge on that local proof; the firecracker lane scenarios and `docs.yml` are first seen running after Nov 1. W2 plan approved with one change: reword the telemetry roadmap's OpenTelemetry profiler mention so the `rg -i ebpf` exit is exact. W2 building |
| 2026-10-29 | 3 | Docs plan drafted (background Opus agent), awaiting the owner; supersedes the 2026-10-28 re-plan's items and waves. Architecture & RFCs tab rebuilt as 25 subsystem pages (18 rebuilt in place, 4 new: `architecture/isolation`, `architecture/testing`, `orchestrator/catalog`, `security/permissions`; 3 kept) plus the two workspace-files contracts, E10's two runtime pages and the 11 planned build pages; proposed `AGENTS.md` §6 table. Disposition of all 207 pages: keep as is 32 (11 of them the decided build pages), keep and fix 30, move to `design/docs-archive/` 22 (+10 build, decided), retire with a redirect 112 (+`network/ebpf`, decided); **22 moves and 112 retirements for the owner's word**. Developer Docs: 38 pages on the path install, first agent, run a node, govern, receipts, reference (13 new incl. `limits`, `concepts`, `operate/configuration`, `authoring/permissions`, `security/receipts`, `reference/api`); style rules; API reference from `openapi.yaml` with a probe-based drift test (`openapi_matches_router`). Items IT-3.21 to IT-3.32; waves A0, A1-A5c, B, D1a-D4, (D5 on the owner's word), Z, about 34 h; A3, A4, A5c hold for E10 Phase 3 on the base. Found: E10 Phase 3 is built on `feat/agent-runtime` (not "not planned"): 4 feature rows, 9 scenarios, 2 new docs pages that will fail `docs-status` when the lanes meet; H1 adds 3 rows; the router's catch-all routes make a string-compare drift test useless; `orchestrator/api.md:12` still says plain HTTP. 12 open questions. Scoreboard: 0 green, 0 red, 0 fix cycles |
| 2026-10-29 | 3 | W2 merged (merge commit) on local proof (Actions down until Nov 1). Orchestrator re-ran: fmt, `cargo test -p vam-network -p vam-spec` (descriptor conformance green), `cargo check --workspace`, `vam-e2e check`, `pnpm build` 207 pages, `dist/network/ebpf/` redirects to the overview. eBPF stub, `has_bpftool`, `EbpfCompilationError`, the proto comment, the `AGENTS.md` and `SKILLS.md` lines and 24 pages of claims removed; `network/ebpf.md` folded into the overview and deleted. Exit: `rg -i ebpf crates proto AGENTS.md docs/src` empty (the config redirect is a URL, not a claim; the search narrowed by the orchestrator). Baseline 207. Scoreboard: 0 scenarios moved (none named), 0 fix cycles (one review change: the redirect key kept literal), about 50 min. Docs plan (architecture rebuild, developer docs) drafted, awaiting the owner |
| 2026-10-29 | 3 | Full `cargo test --workspace --no-fail-fast` on the base in the cloud session (in place of CI, Actions down): 3,145 passed, 1 failed, 16 ignored. The one failure is the known root-only `hsp_uds::uds_socket_acl_deny_fails_fast` (the session runs as root). IT-3.19 (pin the Scalar script on `/docs`, `servers` to https) planned; awaiting the owner's hash check from a machine that can reach the CDN |
| 2026-10-29 | 3 | Actions back (owner). Dispatch runs on the base: Docs run 40822978635 green (first `docs.yml` run: lint and site build); Rust run 40822975063: checks, workspace tests, smoke green; firecracker lane 9 of 10 scenarios PASS, including the first lane runs of `vault-secret-is-not-in-the-guest` and the seccomp step in `jailer-confines-the-vmm-process` (both now proven). `guest-has-no-route-off-the-vm` FAIL at then.1 class product: the tool output lacks `default-route-none`; the output itself is truncated in the brief. Suspected probe bug, not a route off the VM: Linux keeps an IPv6 "unreachable" default route on `lo`, which the probe counts. Fix proposed to the owner: the probe ignores `lo` routes and prints the raw tables. Scoreboard: +2 green (vault, seccomp), 1 red under diagnosis |
| 2026-10-29 | 3 | Owner approved: (1) the docs plan : architecture waves A0-A5c and B approved; developer-docs waves D1a-D5 held for a separate review of the outline (public pages); the 22 moves to `design/docs-archive/` and 112 retirements with redirects approved as listed (a wave may keep a listed page, never remove an unlisted one); CLI reference generated, spec reference waits (D5); tool confinement stated as not proved, scenario offered as a new item. (2) The no-route probe fix wave (fixture only, `then` unchanged). (3) IT-3.19 build (Scalar pinned to 1.74.0 with SRI, `servers` https); CSP and bundling go to the backlog. Probe fix and IT-3.19 building in parallel (disjoint files); A0 planning next |
| 2026-10-29 | 3 | Probe fix merged (merge commit): the no-route probe ignores routes on `lo` and IPv6 reject routes, and prints the raw route tables; `then` unchanged. CI all green including the firecracker lane: **`guest-has-no-route-off-the-vm` PASS on a real microVM** (the failure was the probe counting the loopback IPv6 unreachable route). IT-3.19 merged (merge commit): `/docs` loads `@scalar/api-reference@1.74.0` with sha384 SRI and `crossorigin`; `servers` https; API tests guard both; CI green (lane skipped by label). The SRI hash was computed from the npm tarball (CDN blocked from the session); one check of the CDN bytes is still owed by the owner. Scoreboard: all 10 microVM scenarios green on the lane; W0's three VM proofs now proven; 0 fix cycles counted (fixture fix), about 40 min |
| 2026-10-29 | 3 | A0 plan approved by the owner with two changes: the "developer pages must name a proof" rule waits for the developer-docs waves; the feature register lists only features on this base, with real counts (E10/H1 rows added when those lanes land). A0 building |
| 2026-10-29 | 3 | C1 plan approved by the owner: vam-firecracker 33, vam-core 16, vam-vsock 4, vam-spec 1 warnings; vam-firecracker takes a crate-level allow for 10 style lints (vendored fork); vam-spec fixed at generation (`build.rs` `type_attribute`); 4 `suspicious_open_options` sites (VMM stdout/stderr logs at per-session paths) get scoped allows, no truncate; `rt_std.rs:102` listed. C1 building |
| 2026-10-29 | 3 | A0 merged (merge commit), CI green (checks, workspace tests, smoke, docs; lane skipped by label). Feature register `e2e/docs-features.yaml`: 90 features, 81 proven, 48 with a developer page; rules `docs-feature` and `docs-redirect`; redirects in `docs/redirects.json`; `design/docs-archive/`. Review change: a test proof must be a `tests/` file or a `path::fn` marked `#[test]`/`#[tokio::test]` (14 rows had cited bare source files). Nine features unproven with written reasons. Scoreboard: 0 scenarios moved (plumbing), 1 review cycle, about 1 h 30 min. C1 open; vam-security's three `too_many_arguments` got scoped allows in C1 (orchestrator call: they blocked `-D warnings` on vam-core and vam-vsock as a dependency) |
| 2026-10-29 | 3 | C1 merged (merge commit), CI all green including the firecracker lane. `clippy -D warnings` clean on vam-firecracker, vam-core, vam-vsock, vam-spec (54 warnings): mechanical fixes; vam-firecracker crate-level allow for style lints (vendored fork); vam-spec allow via `build.rs` `type_attribute`; scoped allows listed in the PR (4 per-session log opens, `rt_std.rs` test module, `seq` wire counter, `StdUnixStream` cfg alias); vam-security three `too_many_arguments` allows moved forward from C2 (argument structs stay listed for C2). Scoreboard: 0 scenarios moved (lint), 0 fix cycles, about 1 h |
| 2026-10-30 | 3 | C2 plan approved by the owner with both recommendations: vam-e2e moves from C2 to C5 (it depends on vam-inference, H1's crate, cleaned in C5); the three loop sites are rewritten only when obviously identical, otherwise allowed and listed. C2 covers vam-storage, vam-network, vam-security (tests only), vam-telemetry, vam-cli, about 45 warnings. vam-security argument structs to the backlog. C2 building |
| 2026-10-30 | 3 | C2 merged, CI green including the firecracker lane. `clippy -D warnings` clean on vam-storage, vam-network, vam-security (tests only), vam-telemetry, vam-cli; vam-e2e moved to C5. Allowed and listed: `thread_log/tree.rs` counter loop, `vam_dir.rs` `too_many_arguments` (public, called from vam-orchestrator). A vam-cli test that never ran (`decide_accepts_idempotency_key`, missing `#[test]`) now runs and passes. Smoke went red once with the wrong reason: `egress-refuses-an-ungranted-host` at then.1, the metadata token missing. It reproduced red-for-the-right-reason locally at the PR head and was green on one re-run, so it is a flaky probe (8 s `nc` timeout on a slow runner), not C2. A robustness fix for that probe (longer timeout or one retry; `then` unchanged) is added as a small item. Scoreboard: 0 scenarios moved (lint), 0 fix cycles, 1 re-run, about 1 h |
| 2026-10-30 | 3 | Owner (CI credits): PRs stay drafts, so no CI runs on them. Waves prove locally: the crate tests and `cargo test --workspace` in the session, fmt, `cargo check`, smoke, `vam-e2e check`, `pnpm build` for docs. The orchestrator re-runs the checks and merges a draft by pushing a merge commit to `feat/isolation-truth` (GitHub marks the PR merged; pushes to the base run no CI). GitHub CI is used only when a scenario needs the firecracker lane, through `workflow_dispatch` of `rust.yml` with `lane_only: true` on the wave branch |
| 2026-10-30 | 3 | C3 plan approved by the owner with one change: the always-true assertion is left unchanged under a scoped allow and reported as a finding (not fixed in a lint wave). Owner: architecture waves A1 and A2 may run alongside the clippy waves (docs only, no code overlap); A1 and A2 stay in order because they share the redirects file, the baseline and the sidebar. C3 building, A1 planning |
| 2026-10-30 | 3 | C3 merged (merge commit) by pushing a merge commit (draft PR, no CI, per the owner's rule). Orchestrator re-ran: fmt ok; owned test files 0 clippy warnings (filtered grep plus `#[cfg(test)]` line ranges); `cargo test -p vam-orchestrator --no-fail-fast` 1529 passed, 1 failed (known root-only `uds_socket_acl_deny_fails_fast`); smoke 37 pass, 1 red right reason. 83 owned sites plus about 13 in src `#[cfg(test)]` modules; scoped allows listed in the PR. Finding: the always-true assertion at `hsp_results.rs:545`, backlog row added. Scoreboard: 0 scenarios moved (lint), 0 fix cycles, about 1 h 20 min |
| 2026-10-30 | 3 | A1 plan approved by the owner with both recommendations: no "~115 ms" figure (retired 2026-10-24; cold start cites the lane measurement); minimal `AGENTS.md` §6 fix for pages A1 retires (full rewrite stays with A5c). A1 building; D (dependencies) planning next |
| 2026-10-30 | 3 | A1 merged by merge commit (draft, no CI). Orchestrator re-ran: `vam-e2e check` clean (features 90, proven 81, with a developer page 47), `cargo test -p vam-e2e` 225 passed, `pnpm build` complete, redirects resolve (`architecture/firecracker` to isolation, `network/ebpf` to overview). Pages: `architecture/overview` and `cold-start` rebuilt, `architecture/isolation` and `testing` new (shipped, with proofs; isolation states the open-internet default, unproved tool confinement and the pending egress scenario); 18 retired with redirects, 4 moved to `design/docs-archive/`; baseline 207 to 183; `AGENTS.md` §6 rows repointed; no "~115 ms" (project-status still has it; it belongs to D1a). Disk: the agent built into its own worktree `target/` (15 GB, disk at 935 MB free); the orchestrator deleted that worktree's build output, now 16 GB free. Future briefs repeat the shared `CARGO_TARGET_DIR`. Scoreboard: 0 scenarios moved (docs), 0 fix cycles, about 2 h |
| 2026-10-30 | 3 | Owner approved A2 (core and contracts: `core/overview` and `spec/overview` rebuilt; 12 retired, `core/ratelimit` moved; baseline 183 to 168) and D with both recommendations: the `webpki-roots` bump dropped (TLS path, removes no duplicate; Decide row instead); no lane run. D's only code change removes the unused `prost-types 0.13` from vam-security; the report goes in `design/isolation-truth.md`; upgrades become Decide rows. A2 and D building in parallel (docs vs manifest, no shared files except this tracker, which D edits under Explicitly later) |
| 2026-10-30 | 3 | A2 merged by merge commit (draft, no CI). Orchestrator re-ran: `vam-e2e check` clean, `pnpm build` complete, redirects resolve (`core/jailer` and `spec/common`). `core/overview` and `spec/overview` rebuilt from the code with `file:line` cites (no cgroup CPU or memory limit, Firecracker rate limiter off); 12 retired, `core/ratelimit` moved; baseline 183 to 168; `AGENTS.md` core row updated. Scoreboard: 0 scenarios moved (docs), 0 fix cycles, about 30 min |
| 2026-10-30 | 3 | D merged by merge commit (draft, no CI, no lane per the owner). Orchestrator re-checked on the merged tree: `cargo check --workspace` ok, vam-security and vam-orchestrator lib tests 1210 passed, fmt ok, `cargo tree -d` 1171 lines (was 1183); the wave ran the full workspace suite (3160 passed, 1 known root-only failure) and smoke. Only code change: unused `prost-types 0.13` removed from vam-security (34 to 33 duplicated crates). Report: "Dependency pass (Phase 3)" in `design/isolation-truth.md` (vendored SDK: keep the fork, upstream dormant; tonic/axum/hyper/rustls: one later wave, cost 3; `cargo audit` could not run, advisory DBs blocked by the proxy). Six Decide rows in Explicitly later. Scoreboard: 0 scenarios moved, 0 fix cycles, about 1 h |
| 2026-10-30 | 3 | Finding: E10 Phase 3 and H1 are already on `main` (`feat/agent-runtime` and `feat/harness` are 1 commit past `main`); this base is 98 behind `main`. The owner approved a sync wave (merge `main` into the base; two doc conflicts; give the new pages from `main` an honest `status` and proofs; local workspace tests, smoke, `vam-e2e check`; one firecracker lane run on the synced base) before C4. C4 is re-planned on the synced base. A3, A4 and A5c are unblocked after the sync. A5a approved by the owner with both recommendations (the vault scenario is proven on the lane; the egress gate cites `broker.rs` tests, `gate.rs` has none); A5a builds after the sync merges |
| 2026-10-30 | 3 | Sync merged by merge commit: `main` (E10 Phase 3, H1) into the base. 3 doc conflicts resolved (workstreams index, `e2e/README.md`, `phase-playbook.md`: `main`'s Actions policy matches the owner's CI rule, plus `scripts/ci/local.sh`); `runtime/inbox-outbox` (shipped, with proofs) and `runtime/jig` (shipped, no proofs; A4 checks it claims only what is built) given a status; 4 register rows (steer and queue, stop, jig, model catalog and profiles). Local: 3412 workspace tests passed (1 known root-only failure), smoke 59 of 60 (egress pending, right reason), `vam-e2e check` clean (70 scenarios, 94 features, 83 proven), `pnpm build` ok. Lane-only dispatch run 41034081260 green. A3, A4 and A5c are unblocked |
| 2026-10-30 | 3 | A5a merged by merge commit (draft, no CI). Orchestrator re-ran: `vam-e2e check` clean, `pnpm build` complete, redirects resolve (`security/cedar` to permissions, `network/dns` to overview). `security/permissions` new; `security/secrets-proxy` rebuilt ("Secrets: vault and injection", vault scenario cited as proven); `network/overview` rebuilt (gate cited to `broker.rs` tests; unused TAP, IPAM and DNS code stated); `security/run-receipt` given status and proofs; 16 retired, 5 network design pages moved; baseline 168 to 144. **Finding (security floor, to the owner):** the egress proxy scrubs injected secrets out of responses only when the body fits the 8 MiB scrub buffer; a larger response streams back to the guest unscrubbed, recorded as `scrubbed=false` (`vam-network/src/proxy/mitm.rs:9-14,42`). Documented on the secrets page. Scoreboard: 0 scenarios moved (docs), 0 fix cycles, about 1 h |
| 2026-10-31 | 3 | Owner approved C4 (189 sites in vam-orchestrator, mechanical or scoped allows; one lane run for the `manager.rs` and session edits) and option 1 for the scrub-limit finding: fail closed. For a session with an injected secret, a response larger than the scrub buffer is refused or cut at the limit, and the guest gets an error, with a new scenario proving an oversize echo never reaches the guest. New item IT-3.33, Sonnet high (egress security boundary), plan first. C4 building |
| 2026-10-31 | 3 | IT-3.33 plan approved by the owner with both recommendations. (1) Fail closed per destination: any response from a host that holds a secret binding in the session, larger than the scrub buffer, is refused with a 502 (zero bytes to the guest), not only requests that carried a placeholder; this closes the echo-later gap. (2) The test seam (vamd fake HTTPS origin, `serve_proxy_with_upstream`) is gated behind `process-guest`, and the release gate is checked. A required gate method makes a new gate fail closed by default; a second `egress.decided` DENY records `oversize_with_secret`. New scenario `oversize-response-with-a-secret-never-reaches-the-guest`, red on the base, green with the fix. Building (Sonnet high) |
| 2026-10-31 | 3 | C4 merged by merge commit (draft; lane-only dispatch run 41168800746 green). vam-orchestrator clippy 189 to 0 (73 src, 116 `api_tests.rs`). Orchestrator read the security files line by line (`security.rs`, `secrets.rs`, `session_tokens.rs`, `keys.rs`, `daemon.rs`): all behaviour-identical. Four `brokers.rs` accept loops became `while let Ok`, which ends on the first error as before. `drive9_root` `||` merge flagged. Local re-run: fmt ok, 0 owned warnings, 1647 tests passed. One extra failure, `process_mode_bin::default_vamd_rejects_the_process_variable`, passes after rebuilding the default `vamd`: the shared `target/debug/vamd` had been replaced by a `process-guest` build (smoke runner or a parallel wave), not a C4 regression. Note for later waves: rebuild the default `vamd` before that test. Scoreboard: 0 scenarios moved (lint), 0 fix cycles, about 1 h 30 min |
| 2026-10-31 | 3 | IT-3.33: lane green and local re-run clean, but merging into the base conflicted in `crates/vam-orchestrator/src/brokers.rs` (C4 rewrote the port-55 accept loop to `while let`; edits inside it). The orchestrator aborted the local merge (nothing pushed); a fresh Sonnet-high session merges the base into the branch and re-proves. Orchestrator mistake noted: the original agent's worktree was removed before the merge was confirmed; cleanup now waits for a confirmed merge. A3 plan approved by the owner, to build from the base after merges (orchestrator pages rebuilt, `orchestrator/catalog` new, 10 retired, 3 moved; baseline 144 to 125) |
| 2026-10-31 | 3 | IT-3.33 merged by merge commit after a conflict fix. The base was merged into the branch  and the port-55 accept loop resolved as C4's `while let` with's body, a plain combination (orchestrator read the diff: only's additions). A follow-up commit fixed a test-only `redundant_closure` that introduced. Fix session results: vam-orchestrator 0 clippy warnings, vam-network clippy clean, vam-network 100 passed, orchestrator 1648 passed (1 known root-only failure), release gate PASS, oversize and encoded scenarios PASS, smoke 60 of 61 (egress pending), `vam-e2e check` ok. Orchestrator on the merged tree: `cargo check --workspace` ok, 0 orchestrator clippy warnings, fmt ok. No second lane run: the lane was green and the merge only combines two lane-green changes. **The scrub-limit finding is closed:** a response over the 8 MiB scrub buffer from a host that holds a session secret binding is refused with a 502 before any byte, status or header reaches the guest; HEAD, 204 and 304 are exempt; unbound hosts stream as before; `prepare_request` is required (fail closed by default); a second `egress.decided` DENY records `oversize_with_secret`. New scenario `oversize-response-with-a-secret-never-reaches-the-guest` red on the base, green with the fix. Test seam behind `process-guest`. Consequences documented: a download over 8 MiB from a secret-bound host fails with a clear 502; a `*` secret destination binds every host. Scoreboard: +1 green, 1 merge-conflict fix, about 3 h |
| 2026-10-31 | 3 | A3 merged by merge commit (draft, no CI). Orchestrator re-ran: `vam-e2e check` clean, `pnpm build` complete, redirects resolve (`orchestrator/errors` to api, `orchestrator/scheduler` to overview). Rebuilt: `orchestrator/overview` ("Host daemon"), `api` (the fail-closed 502 cites the new scenario), `session` ("Sessions and runs", E10 linked to `runtime/inbox-outbox`), `user-isolation` ("Users, keys and ownership"); new `orchestrator/catalog`; the workspace-files pages got status and proofs; 10 retired, 3 moved; baseline 144 to 125. Steer endpoint: both `POST /v1/runs/{id}/messages` and `POST /v1/runs` with `inboxMode` exist, so no text was wrong. Register: `skill-package-executor` arch cleared (its proof is about host-tool output, not admission). Scoreboard: 0 scenarios moved (docs), 0 fix cycles, about 1 h |
| 2026-10-31 | 3 | Owner approved C5 (vam-runtime about 40 sites incl. 15 in `pid1.rs`, vam-inference 7, vam-e2e 5; the three `await_holding_lock` sites are test-only, none in runtime code) with one change: also fix the leftover warning each in vam-security and vam-spec from the `main` merge, so the whole workspace is clippy clean for C6; one lane run required (`pid1.rs` boot path). A4 approved (5 runtime pages rebuilt, 16 retired, baseline 125 to 103) with one change: the jig page cites the progress and question scenarios only if they exercise jig. Both building |
| 2026-10-31 | 3 | A4 merged by merge commit (draft, no CI). Orchestrator re-ran: `vam-e2e check` clean, `pnpm build` complete, redirects resolve (`runtime/hitl` and `security/hitl-crypto` to `runtime/workflow`). Orchestrator checked the jig proofs: `progress-shows-while-the-task-runs` runs with `jig: true` and asserts the `vam.progress` card and the `check_data` refusal (`outbox.rs`); `question-waits-for-an-answer-and-resumes` hits `check_result` (`hitl.rs`); `long-task-posts-progress` is not cited (no jig code runs). Rebuilt: `runtime/overview`, `agent`, `tools`, `inference`, `workflow` ("Playbooks, gates and approvals"); `benchmark-results` got status, proofs and a link to the open-model report; 16 retired; baseline minus 22 (103 entries). Not proved, stated on the pages: tool confinement, approval signer and replay rule, `calls.repairBudget`, live MCP. Scoreboard: 0 scenarios moved (docs), 0 fix cycles, about 1 h |
| 2026-10-31 | 3 | C5 merged by merge commit (draft; lane-only run 41180895782 green, which builds and boots the musl guest). **The whole workspace is clippy clean at `-D warnings`**, with default features and with `vam-runtime/process-guest,vam-orchestrator/process-guest`. vam-runtime (about 40 sites, incl. `pid1.rs` `c""` literals: same NUL-terminated bytes and pointers), vam-inference (7), vam-e2e (5), plus the leftover warning each in vam-spec (a test assert) and vam-security (`manual_strip` in `ssrf_guard.rs`). The three `await_holding_lock` sites are test-only: one scoped, two scoped allows (deliberate telemetry test lock). Orchestrator local re-run: both workspace clippy runs clean, fmt ok, guest build PASS, 687 tests passed. The musl target is not installed in the session; the lane is the musl proof. Scoreboard: 0 scenarios moved (lint), 0 fix cycles, about 1 h. C6 (CI gate) next |
| 2026-10-31 | 3 | Owner approved C6 (clippy gate: a separate `clippy` job in `rust.yml` and a step in `scripts/ci/local.sh --full` only, not `--fast`; default and `process-guest` features; pinned `dtolnay/rust-toolchain@1.97.0`; `rust-toolchain.toml` stays `stable`) and A5b (storage and hosted pages; 16 retired, 8 moved; baseline 103 to 75) with the change that `storage/turn-commits` and `hosted/web` are read in full and any stale claim fixed before they are marked shipped. Both building. Owner action after the `clippy` job first runs: add `clippy` as a required check |
| 2026-10-31 | 3 | A5b merged by merge commit (draft, no CI). Orchestrator re-ran: `vam-e2e check` clean, `pnpm build` complete, redirects resolve (`storage/fuse` and `hosted/protocol`). `storage/overview` rebuilt: only what is built; "No FUSE layer", no cloud backends, no encryption at rest, stated under Not built; "transactional" is per finished turn. `hosted/overview` rebuilt; `vam.files`, `vam.rag`, vsock for third-party brokers and triggered runs under Planned. `storage/turn-commits` and `hosted/web` read in full: turn-commit numbers match the code; three unverifiable web-broker limits removed, robots and DNS-pinning text corrected. 16 retired, 8 moved; baseline 103 to 75. Stale links remain in `build/overview`, `getting-started/platform-tour` and `orchestrator/telemetry`, all still on the exemption list; waves B, A5c and the developer docs fix them. Scoreboard: 0 scenarios moved (docs), 0 fix cycles, about 1 h 30 min |
| 2026-10-31 | 3 | C6 merged by merge commit. **The clippy gate is live** (locked decision 5): a `rust.yml` job named `clippy` runs `cargo +1.97.0 clippy --workspace --all-targets -- -D warnings` on default features and on `vam-runtime/process-guest,vam-orchestrator/process-guest`; `scripts/ci/local.sh --full` runs the same (not `--fast`). The first dispatch (run 41210345114) caught that `dtolnay/rust-toolchain@1.97.0` alone did not hold the pin against `rust-toolchain.toml` (`stable`, 1.99 on the runner, 99 `result_large_err` errors in tonic-generated vam-spec code); fixed with the explicit `+1.97.0`; the 1.99 lints are a backlog row. Run 41212151473: `clippy` green, both steps. Negative test proven locally. Commits unsigned per the owner's 2026-10-27 rule (signing helper down). In that run smoke failed once on the new `oversize-response-with-a-secret-never-reaches-the-guest` at when.0, class env (22 s); it passed in run 41210345114 on the same code, so it is a flake of the new scenario, not C6; added to the egress robustness item. Owner action: add `clippy` as a required check. Scoreboard: 0 scenarios moved, 1 fix cycle (the pin), about 1 h 30 min |
| 2026-11-02 | 3 | A5c approved by the owner with one change: no Developer-tab change (Event Catalog stays in the Architecture Telemetry group; its public place is decided with the developer-docs outline). Telemetry overview rebuilt, event catalog checked against `events.rs`, 19 retired and `telemetry/roadmap` moved, `AGENTS.md` §6 rewritten (pages that exist only) plus one-line §1-§3 fixes; baseline 75 to 53. A5c building |
| 2026-11-02 | 3 | A5c merged by merge commit (draft, no CI) after one review cycle. Orchestrator checked claims against the code: the first push removed `session.booted` from the event catalog as "never emitted" (it is emitted at `manager.rs:1750`, `:1976`) and said a slow gRPC reader is "dropped" without a cite; fixed: `session.booted` restored, the slow-reader text now matches `bus.rs` (evicted on lag or past 16 MB) and `telemetry_stream.rs` (`RESOURCE_EXHAUSTED`, replay only by reconnecting with `resume_from_sequence`), and the redaction list matches `redact.rs:14-110`. Re-run: `vam-e2e check` clean, `pnpm build` complete, `orchestrator/telemetry` redirects to `telemetry/overview`. `telemetry/overview` rebuilt (6 test proofs, Not built: guest OTLP, SIEM, Prometheus, billed FOCUS, TLS on the gRPC stream); 19 retired, `telemetry/roadmap` moved; `AGENTS.md` §6 rewritten (pages that exist only), §2 fixes (rootfs built locally, 13 crates, no doc count); Developer tab untouched apart from one link the lint required. Baseline 75 to 53: left are `build/*` (21, wave B), `core/prerequisites`, `spec/glossary`, `security/overview` (developer docs) and 28 Developer-tab pages plus `README.md`. Scoreboard: 0 scenarios moved (docs), 1 review cycle, about 2 h |
| 2026-11-02 | 3 | Owner approved wave B as planned (11 build pages kept as `planned`, `build/rootfs` too because the page oversells `scripts/build-rootfs.sh`; 10 moved to `design/vam-build/`; sidebar "Images & Build (planned)"; baseline 53 to 32) and IT-3.34 with both recommendations: `spawn_blocking`, not `block_in_place` (which panics on a current-thread runtime); built on Sonnet high. IT-3.34: `egress.decided` filed under the session and, when one exists, the run (`append_egress_decided`, the `append_session_booted` pattern); `egress-refuses-an-ungranted-host` goes active with `then` unchanged; its probe retries; the oversize scenario's blocking host-tool call moves off the async workers. Both building |
| 2026-11-02 | 3 | B merged by merge commit (draft, no CI). Orchestrator re-ran: `vam-e2e check` clean, `pnpm build` complete, `build/signing` redirects to `build/overview`, `build/rootfs` is `planned`. 11 build pages kept as `planned` (Alpha cautions removed), 10 moved to `design/vam-build/` with redirects, sidebar "Images & Build (planned)"; `build/rootfs` stays planned because the page describes more than `scripts/build-rootfs.sh` does (no `vam-build` compiler, no reproducible mtimes, 256 MB not 2 GB). Baseline 53 to 32: what remains is Developer-tab pages, `security/overview`, `core/prerequisites`, `spec/glossary` and `README.md`. **The Architecture tab is done.** Scoreboard: 0 scenarios moved (docs), 0 fix cycles, about 45 min |
| 2026-11-02 | 3 | IT-3.34 merged by merge commit (draft; lane-only run 41422122745 green). `egress.decided` is filed under the session and, when one exists, the run (`events::append_egress_decided`). **Finding fixed:** `evidence.rs` `collect_denials` read `disposition` at the top level and so silently skipped every CloudEvent denial (the fields sit under `data`); it now reads both shapes, and egress denies reach `denials.jsonl`. `egress-refuses-an-ungranted-host` is **active and green** with `then` unchanged; its probe retries 3x. Host adapters run on `spawn_blocking` with a typed `JoinError`; port-55 probe timeout 20 s to 10 s. Local: `local.sh --full` ok (1 known root-only failure), smoke 61/61, each egress scenario 10/10, `vam-e2e check` ok, `pnpm build` ok. The oversize scenario's CI flake did not reproduce locally; the fix rests on current-thread and one-worker runtime tests. Scoreboard: +1 green, 0 fix cycles, about 3 h (one container restart) |
| 2026-11-02 | 3 | **Phase 3 closed in the base** on the owner's word. All waves merged: W0, W1, W2, probe fix, IT-3.19, A0, A1, A2, A3, A4, A5a, A5b, A5c, B, C1 to C6, D, sync IT-3.33, IT-3.34. Exit met except the empty exemption list (32 left, developer docs), moved to Phase 4 with Z. Team checklist written. Release to `main` needs a sync with `main` first (107 commits; code conflicts in `inbox.rs`, `api/mod.rs`, `events.rs`, `host_tools.rs`, `vam-e2e/src/world.rs`; three retired pages modified on `main`), then the owner's word. Scoreboard for the phase: +6 scenarios green (5 new, 1 changed), 0 red, 2 fix cycles (C6 pin, A5c review), CI used only for the microVM lane and the C6 gate |
| 2026-11-02 | 3 | Sync before release merged by merge commit: `main` (107 commits) into the base. 5 code conflicts resolved keeping both sides (`world.rs`, `api/inbox.rs`, `api/mod.rs`, `events.rs`, `host_tools.rs`); in `host_tools.rs` the effect-journal order is `main`'s (`mark_sent` before the adapter, same settle paths) and the adapter runs on `spawn_blocking` (journal key passed through `AdapterCall.idempotency_key`); `main`'s edits to three retired pages ported where code-backed (`hosted/overview` "Runs started by a broker", `orchestrator/user-isolation` broker keys, `orchestrator/api`); new clippy findings from `main` fixed. Local: clippy 1.97.0 both feature sets clean, `local.sh --full` ok (1 known root-only failure), smoke 84/84, `vam-e2e check` ok (95 scenarios), `pnpm build` ok, release gate PASS. Lane-only run 41429335643 green. Release PR opened |
| 2026-11-09 | 3 | Second sync before release merged by merge commit, at the VAM Master Orchestrator session's request (owner confirmed it merges after this report). `main` (128 commits: notifications 1-2, connectors 1-3, `connection` vault kind model and provider hotfixes, bench workflow) into the base. 11 conflicts resolved keeping both sides. **Security finding closed in the merge:** `main`'s `connection` vault rows never entered the static bindings, so a host a connector token is swapped into (e.g. `graph.microsoft.com`) was not secret-bound and an oversize response from it would have streamed unscrubbed; `secret_bound` now also covers `grant.is_some()` and `ConnectionTokenSource::binds_host` (a new required method), with a unit test. The 502 is proved by scenario for static bindings only; no scenario sends an oversize connector response (noted). Retired `runtime/host-tools` and `runtime/mcp` stay deleted; `main`'s edits ported into `runtime/tools`. Local: clippy 1.97.0 clean both feature sets, workspace tests 4098 passed (1 known root-only failure), smoke 115/115, `--all` 116 pass 0 fail 12 skipped (firecracker/ui), `vam-e2e check` ok (128 scenarios, 98 features, 87 proven), `pnpm build` ok, release gate PASS. Disk 15 GB free before, 8.4 GB after |
| 2026-11-09 | 3 | **Released to `main`** (merge commit) by the VAM Master Orchestrator session on the owner's word, on the green full run 42602013933 (checks with the release gate, clippy, cargo test, smoke, docs, firecracker lane). Base fast-forwarded to `main`. **Track complete:** Phases 1 to 3 released. Phase 4 (developer docs, Z) waits on the owner's outline review |
