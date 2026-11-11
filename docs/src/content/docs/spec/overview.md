---
title: Spec Overview
description: The Protocol Buffer contracts under proto/vam/v1, how vam-spec compiles them, the JSON encoding, and the compatibility baseline.
status: shipped
proves:
  - crates/vam-spec/tests/conformance.rs
  - crates/vam-spec/tests/examples_json.rs
  - crates/vam-spec/tests/conformance.rs::test_protocol_compatibility
---

VAM is contract-first: the Protocol Buffer files in `proto/vam/v1/` (package `vam.v1`) define the
messages and services the host, the guest and the API exchange. The Rust crate `vam-spec` compiles
them; other code imports its types.

## What is in the tree

| Area | Files in `proto/vam/v1/` |
|---|---|
| Shared types, errors | `common.proto` |
| Agents, skills, model policy | `agentic_spec.proto`, `skills.proto`, `model_policy.proto` |
| Playbooks, runs, approvals, evals | `workflow.proto`, `hitl.proto`, `evals.proto` |
| Events and telemetry | `events.proto` (service `TelemetryStreamService`), `telemetry.proto` |
| Host and guest wire | `vsock_rpc.proto` (services `VamHostService`, `VamGuestService`), `inference.proto` |
| Daemon API | `orchestrator.proto` (service `Vamd`) |
| Other subsystems | `security.proto`, `storage.proto` (`Drive9Service`), `network.proto`, `hosted_services.proto` (`HostedBroker`), `receipt.proto` |

Not every service is served over gRPC. The `Vamd` service in `orchestrator.proto` documents the RPCs,
and the daemon serves them as HTTP and JSON.

## How vam-spec compiles them

`crates/vam-spec/build.rs` lists the 17 proto files, runs `protoc` (the vendored binary, unless
`PROTOC` is set) through `tonic-build`, and writes `vam.v1.rs` and a file descriptor set. The crate
includes both. Features `client` and `server` (both default) switch on the generated `tonic` code.

Beside the generated types, `vam-spec` holds hand-written rules shared by the host and the guest, for
example `validation`, `permissions`, `playbook_inputs`, `extends`, `receipt` and `args_digest`.

## JSON encoding

`crates/vam-spec/src/encoding.rs` loads the descriptor set into a `DescriptorPool`. `proto_to_json`
encodes a message with `prost`, decodes it dynamically against the descriptor and serialises it to
JSON, so the JSON follows the proto field names and types. The fixtures `proto/vam/v1/baseline/*.json`
and `*.bin` pair each valid and invalid example with its binary form.

## The compatibility baseline

`proto/vam/v1/baseline/vam_descriptor.bin` is a saved copy of the descriptor. The test
`test_protocol_compatibility` fails when a message or field in the baseline has been deleted, renamed
or given an incompatible type. Adding fields and messages is allowed. Changing the contract on purpose
means regenerating the baseline in the same change, which is then visible in review
(`crates/vam-spec/examples/generate_baseline.rs`).

## Conventions

- A spec describes desired state; a status describes observed state. Keep them in separate messages.
- Field numbers and names in the baseline are never changed or deleted (the test above).
- Resources are named by URN and pinned by `ResourceRef` with a version or digest.

## Proved by

| Claim | Proof |
|---|---|
| A change cannot silently break the baseline descriptor | `crates/vam-spec/tests/conformance.rs::test_protocol_compatibility` |
| Valid and invalid fixtures decode and validate as expected | `crates/vam-spec/tests/conformance.rs` |
| The shipped example catalogs encode to JSON and validate | `crates/vam-spec/tests/examples_json.rs` |
| Process-mode features are never on by default | `scripts/e2e/check-manifests.sh` |

## Not built

- Packages for languages other than Rust are not generated in this repository.
- A generated spec reference page is not published yet. Read the `.proto` files.
