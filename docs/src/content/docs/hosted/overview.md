---
title: Hosted Brokers
description: How vamd talks to out-of-process brokers over HSP, what the host checks before it calls one, and what is planned.
status: shipped
proves:
  - crates/vam-orchestrator/tests/hsp_uds.rs
  - crates/vam-orchestrator/tests/hsp_invoke.rs
  - crates/vam-orchestrator/tests/hsp_hooks.rs
  - crates/vam-orchestrator/tests/hsp_manifest.rs
  - crates/vam-orchestrator/tests/hsp_trust.rs
  - vault-secret-is-not-in-the-guest
  - trigger-starts-a-run-as-the-grant-owner
  - grant-token-is-refused-over-http
  - revoked-grant-starts-nothing
---

A hosted broker is a separate process on the node that gives agents tools, or runs at set points in a run, and holds credentials the guest never sees. `vamd` talks to it over the Host Service Protocol (HSP), a gRPC service defined in `proto/vam/v1/hosted_services.proto`. The guest never talks to a broker. Its tool calls go to `vamd`, which calls the broker.

## The protocol

The broker serves `HostedBroker` (`hosted_services.proto:280-287`) with seven calls: `Register`, `ListTools`, `Invoke`, and four hooks, `OnRequest`, `OnComplete`, `OnResponse` and `OnPaused`.

* **Transport.** `vamd` dials the broker on a Unix socket (`crates/vam-orchestrator/src/hsp.rs:334`). Test: `hsp_uds.rs`.
* **Registration.** An operator registers a broker with `PUT /v1/hosted/brokers` (`crates/vam-orchestrator/src/api/mod.rs:207`). The request carries the broker's manifest and socket path.
* **Manifest.** The manifest lists the tools the broker exports, each with a side-effect class and a destination policy, plus the secrets and egress domains it needs. `vamd` checks it before the tools join the catalog. Test: `hsp_manifest.rs`.
* **Invoke.** A tool call from the guest reaches the broker as `Invoke`. Test: `hsp_invoke.rs`.
* **Hooks.** `vamd` calls the hooks around a run: before the guest starts, after the agent finishes, before the API reply, and when a run pauses for an approval. Test: `hsp_hooks.rs`.
* **Trust tiers.** Each broker has a tier: core, first party, operator or third party. `vamd` pairs a tier with a transport and refuses a mismatch. Core is in-process and is refused. First party and operator brokers may use a Unix socket. A third party broker needs vsock, which is not built, so it is refused (`hsp_registry.rs:344-370`). Test: `hsp_trust.rs`.
* **Secrets.** A broker call receives only the secrets listed on that tool's export in its manifest. See [Web Broker](/hosted/web/).

## Runs started by a broker

A broker that may ask `vamd` to start runs declares `run_request_permission` in its manifest (`hosted_services.proto:98`): the `grant_tools` whose `Invoke` may carry a `RunGrant`, the `loop_guarded_tools` that are refused inside a triggered run, a receipt `surface` and request limits. A limit left at 0 means the node default (for example 50 active grants per user, `vam-spec/src/validation.rs:661`), never unlimited. The operator approves the declaration with the same subset gate as secrets and egress domains.

`vamd` serves `HostedTriggerGateway` (`hosted_services.proto:364`) to such a broker on a socket of its own, with `RequestRunTrigger` and `RevokeRunGrant`. The run starts as the user who approved the grant. `CallerContext` carries `space_id`, `thread_id` and `run_source` (`"broker:<id>"` for a triggered run). Proved by `trigger-starts-a-run-as-the-grant-owner`, `grant-token-is-refused-over-http` and `revoked-grant-starts-nothing`. Design: `design/hsp-triggers.md`.

## Brokers that exist

* `vam.web` gives agents `web_search` and `web_fetch`. See [Web Broker](/hosted/web/).
* Two TypeScript examples show how to write a broker: `examples/hsp-echo-ts` and `examples/hsp-files-ts`. They have a README and no test.

## Planned

These are design only. No code runs them.

* First-party `vam.files` and `vam.rag` brokers.
* A vsock transport for third party brokers.

Earlier design notes on these live in `design/docs-archive/hosted/`.
