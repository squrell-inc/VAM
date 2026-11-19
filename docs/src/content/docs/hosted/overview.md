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
  - node-init-installs-and-registers-the-brokers-asked-for
  - node-init-check-names-a-missing-broker
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

`vamd` serves `HostedTriggerGateway` (`hosted_services.proto:364`) to such a broker on a socket of its own, with `RequestRunTrigger` and `RevokeRunGrant`. The run starts as the user who approved the grant. `CallerContext` carries `space_id`, `thread_id` and `run_source` (`"broker:<id>"` for a triggered run). Proved by `trigger-starts-a-run-as-the-grant-owner`, `grant-token-is-refused-over-http` and `revoked-grant-starts-nothing`. See [Broker-Triggered Runs](/hosted/triggered-runs/) for grants, the gateway and what `vamd` checks on a fire.

## Install the brokers with the node

The release bundle holds the three first-party brokers beside `vam`. Install and register the ones you want in the same run as the node:

```bash
sudo ./vam node init --broker web,schedule,notify
```

Names are `web`, `schedule` and `notify`, comma separated or repeated. Without `--broker`, no broker is installed.

* With systemd, `node init` writes one template unit, `/etc/systemd/system/vam-broker@.service`, and one argument file per broker, `/etc/vam/broker-<name>.env`. The unit starts after `vamd.service`, runs `vam-<name>-broker` and restarts it on failure. The argument file holds paths and flags only; a broker's credentials stay in the vault (`vam secrets set`).
* With `--no-systemd`, `node init` starts each broker itself, detached, and writes `/var/lib/vam/brokers/<name>.log` and `<name>.pid`. Nothing restarts it. A re-run leaves a running broker alone, and restarts it only when its arguments changed.
* Sockets are `/var/lib/vam/brokers/<name>.sock`, mode `0600`. Schedule and notify keep their state in `/var/lib/vam/brokers/<name>/`.
* Registration is `PUT /v1/hosted/brokers` with the manifest the broker prints (`vam-<name>-broker manifest`). It uses the admin key the first run mints, else `--token`, `VAM_API_KEY` or your signed-in config.
* With no key, the files are written, nothing is registered, and `node init` prints the command to run for each broker, then exits `0`:

```bash
VAM_API_KEY=<admin key> vam node init --broker web
```

Run it again whenever you like. Registration is idempotent, a re-run changes only what differs, and it never removes a broker you did not name. `sudo vam node init --check --broker web` names a broker binary missing beside `vam`, with the fix, and changes nothing.

## Brokers that exist

* `vam.web` gives agents `web_search` and `web_fetch`. See [Web Broker](/hosted/web/).
* `vam.schedule` starts runs at a set time or on a repeat. See [Schedule Broker](/hosted/schedule/).
* `vam.notify` sends notification records to channels (Slack first) and relays decisions back. See [Notify Broker](/hosted/notify/).
* Two TypeScript examples show how to write a broker: `examples/hsp-echo-ts` and `examples/hsp-files-ts`. They have a README and no test.

## Planned

These are design only. No code runs them.

* First-party `vam.files` and `vam.rag` brokers.
* A vsock transport for third party brokers.
