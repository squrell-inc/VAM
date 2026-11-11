---
title: Permissions
description: How tool and internet rules are stored, resolved and enforced, and where untrusted vendor text is fenced.
status: shipped
proves:
  - always-allow-and-always-deny-are-remembered
  - released-agent-approvals-are-once-only
  - injected-text-cannot-start-a-playbook
  - crates/vam-orchestrator/tests/permissions_api_tests.rs
  - crates/vam-orchestrator/tests/legacy_permissions_tests.rs
  - crates/vam-security/tests/pdp_tests.rs
  - crates/vam-security/src/permissions.rs::j1_defaults_allow_read_only_ask_mutable_external_and_open_internet
  - crates/vam-security/src/permissions.rs::default_deny_internet_allows_only_listed_hosts
  - crates/vam-security/src/permissions.rs::host_specificity_table
  - crates/vam-security/src/vendor_text.rs::closing_marker_cannot_escape
  - crates/vam-security/src/vendor_text.rs::fenced_body_reads_back_a_fence_without_the_nonce
---

Permissions are data on the agent. The host reads them, resolves them to one decision per tool call
and per outbound host, and enforces that decision. The guest cannot change them.

## Where each part lives

| Part | What it does | Where in code |
|---|---|---|
| Permissions data | Tool rules and internet rules on the agent spec (`AgentPermissions`). | `crates/vam-security/src/permissions.rs` |
| Resolver | The most specific matching rule inside a scope decides. Across scopes the strictest wins; source and org rules can only tighten. | `permissions.rs:1-10` |
| Tool decision | `resolve_tool`, `explain_tool`. A decision is allow, ask (human approval) or deny. | `permissions.rs:195`, `:237` |
| Internet decision | `resolve_internet` compiles host rules to Cedar; `explain_host` says which rule decided. | `permissions.rs:274`, `:342` |
| Policy decision point | The `PolicyDecisionPoint` trait, with a Cedar engine behind it. | `crates/vam-security/src/pdp.rs:289`, `engine.rs:15` |
| Enforcement point | The orchestrator evaluates each tool intent and, on a suspend, parks the run for approval. | `crates/vam-orchestrator/src/security.rs` |
| Egress enforcement | The session's egress authorizer is built from the agent's internet rules. | [Network](/network/overview/) |

## Defaults

* Read-only tools are allowed. Mutable-external tools, including MCP tools with no stored class, ask
  (`permissions.rs:1-13`; test `j1_defaults_allow_read_only_ask_mutable_external_and_open_internet`).
* **An agent with no `internet` setting resolves to allow** (`permissions.rs:277-279`). Private,
  loopback and metadata addresses are still refused by the IP floor, whatever the rules say.
* Setting the internet default to deny allows only the listed hosts (test
  `default_deny_internet_allows_only_listed_hosts`). A more specific host rule beats a less specific
  one (`host_specificity_table`).

## Using them

* `vam agent permissions get <agent> [--effective]` prints the draft, or the decided table.
* `vam agent permissions set <agent> -f file.json` replaces the owner's draft.
* `vam agent permissions web <agent> on|off` turns web search and fetch on or off (off by default).
* `vam agent permissions presets` lists starting rule sets, served by `GET /v1/permission-presets`.
* `vam agent migrate` moves a draft off the legacy fields; `vam policy explain` explains a tool or,
  with a host argument, an outbound host.

(`crates/vam-cli/src/cmd/permissions.rs:19-52`, `catalog.rs:78`, `:193`.) The API side is covered by
`crates/vam-orchestrator/tests/permissions_api_tests.rs`.

## Approvals

When a tool asks, the run waits for a person. "Always allow" and "always deny" are remembered
(`always-allow-and-always-deny-are-remembered`). A released agent's approvals are once only
(`released-agent-approvals-are-once-only`).

## Cedar and the legacy path

An agent spec may still carry a `capabilityPolicy`, a Cedar policy. It is the legacy path: new work
uses permissions data, and `vam agent migrate` converts a draft. The Cedar engine, shadow mode and
the policy bundle digest are covered by `crates/vam-security/tests/pdp_tests.rs`; the legacy fields
by `crates/vam-orchestrator/tests/legacy_permissions_tests.rs`.

## Untrusted vendor text

Tool results, tool errors, tool descriptions and server instructions from MCP and hosted brokers are
wrapped in a marker fence carrying a random nonce (`crates/vam-security/src/vendor_text.rs:31-36`,
`:58-64`, `:127`), and scanned for injection patterns (`:263`). Text inside the fence cannot close it
(`closing_marker_cannot_escape`), and the body reads back without the fence
(`fenced_body_reads_back_a_fence_without_the_nonce`). Proved at the run level by
`injected-text-cannot-start-a-playbook`.

## Not proved

* Confinement of custom in-VM tools. No scenario shows it.
* Fencing makes injected text recognisable; it does not stop a model from following it.
