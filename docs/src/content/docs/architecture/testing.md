---
title: Testing
description: Scenarios as the spec, the layers that run them, the mock model, the docs lint and the benchmark runner.
status: shipped
proves:
  - plain-chat-gets-a-reply
  - model-task-runs-k-trials-on-the-mock
  - crates/vam-e2e/tests/check_fixtures.rs
  - crates/vam-e2e/tests/docs_check.rs
---

A behaviour is specified by a **scenario**: a YAML file in `e2e/scenarios/` that says what is set up
(`given`), what a user does (`when`) and what they must see (`then`). A scenario asserts what a user
sees through the CLI, the API and run events, not internals. An `active` scenario passes on the base.
A new one starts `pending` and must fail at the step the work adds. The format is in `e2e/README.md`.

On this base there are 48 scenarios: 47 `active` and 1 `pending`
(`egress-refuses-an-ungranted-host`). 38 need a process-mode node (`needs: process`) and 10 need a
microVM (`needs: firecracker`).

## The layers

| Layer | Command | Needs | What it covers |
|---|---|---|---|
| Lint | `cargo run -p vam-e2e -- check` | nothing | Scenario format, world pieces, the docs lint, the feature register |
| Crate tests | `cargo test --workspace` | nothing | Unit tests and each crate's `tests/` |
| Process scenarios | `scripts/e2e/run.sh --smoke` | Linux, no KVM | The 38 `needs: process` scenarios against a test-only `vamd` |
| MicroVM lane | `scripts/e2e/run.sh --firecracker` | `/dev/kvm`, root, Firecracker assets | The 10 `needs: firecracker` scenarios against the default `vamd` |

Smoke selects every scenario with `needs: process` that is not retired
(`crates/vam-e2e/src/run_cmd.rs:201`). The lane selects the `needs: firecracker` ones (`:202`). The
two cannot be combined, because they start different nodes (`:178`).

### The process node

`run.sh` builds `vamd` and `vam-runtime` with the `process-guest` feature and copies them to
`target/e2e-bin/`, so an ordinary `cargo build` cannot replace them. In this mode a guest is a host
process with no isolation (see [Isolation](/architecture/isolation/#process-mode-isolates-nothing)).
A scenario that makes an isolation claim therefore needs the lane and says `needs: firecracker`.
The release gate (`scripts/e2e/check-release-gate.sh`) keeps these builds out of releases.

Here is one scenario run against a process-mode node:

```text
$ scripts/e2e/run.sh --scenarios plain-chat-gets-a-reply,host-tool-output-fits-frame
PASS                 host-tool-output-fits-frame  (2510 ms)
PASS                 plain-chat-gets-a-reply  (2198 ms)
2 scenario(s): 2 pass, 0 fail, 0 red (right reason), 0 red (wrong reason), 0 skipped. Output: target/e2e/20261030-071112-…
```

### The mock model

Process nodes serve only the mock model. A scenario scripts it with a `mock:` block. Each rule matches
on a step label (`e2e-step:<label>` in the user text), on a prior tool, or on text in the history, and
replies with text or a tool call. The same mock runs on the lane. Real models run only on the lane,
through the benchmark runner.

## Which crate is guarded by which scenarios

Each scenario lists `guards`: the files whose change should run it. Counting the scenarios whose
guards name a crate gives this table.

| Crate | Scenarios that guard it |
|---|---|
| `vam-orchestrator` | 39 |
| `vam-runtime` | 22 |
| `vam-storage` | 8 |
| `vam-core` | 3 |
| `vam-security` | 3 |
| `vam-network` | 2 |
| `vam-telemetry` | 2 |
| `vam-vsock` | 2 |
| `vam-e2e` | 2 |
| `vam-cli` | 1 |
| `vam-firecracker` | 1 |

`vam-inference`, `vam-spec` and `brokers/vam-web` are named by no scenario's `guards`. Their tests are
crate tests (for example `crates/vam-e2e/tests/usage_recorded.rs` covers usage records).

## The docs lint and the feature register

`cargo run -p vam-e2e -- check` also lints this site:

- every page has `status: shipped | planned | stale`;
- a `shipped` page that makes an isolation claim (the words are in `e2e/docs-claims.yaml`) names an
  `active` scenario or a test in `proves`;
- `e2e/docs-baseline.txt` lists the pages that do not pass yet. A listed page is exempt, and the list
  only shrinks;
- `e2e/docs-features.yaml` is the register: one row per shipped feature, the page that explains it,
  the page that teaches it, and its proofs. A page a row lists must exist, and once it leaves the
  baseline it must be `shipped` and name one of the row's proofs;
- `docs/redirects.json` maps every removed page to a page that exists.

A test counts as a proof if it is a file under a crate's `tests/` directory, or `path::fn` where `fn`
has a `#[test]` or `#[tokio::test]` attribute. A `pending` scenario counts as no proof. The lint is
tested by `crates/vam-e2e/tests/check_fixtures.rs` (a failing fixture per rule) and
`crates/vam-e2e/tests/docs_check.rs`.

## The benchmark runner

`scripts/e2e/run.sh --bench --mock -k N` runs the scenarios that have a `model:` block `N` times on the
mock and writes `bench-results.json`. A mock result is never publishable. The scenario
`model-task-runs-k-trials-on-the-mock` shows the runner doing this. Real-model runs need the lane, a
catalog model and a key in the bench node's vault; the runbook is in `e2e/README.md`.
