---
title: "Secrets: vault and injection"
description: How the vault stores a secret, how the host proxy swaps a placeholder for it, and what is scrubbed from the way back.
status: shipped
proves:
  - vault-secret-is-not-in-the-guest
  - secret-never-reaches-guest-encoded
  - crates/vam-orchestrator/tests/api_tests.rs
  - crates/vam-network/src/proxy/mitm_tests.rs::connect_then_tls_get_returns_the_upstream_body
  - crates/vam-network/src/proxy/mitm_tests.rs::prepare_substitutes_authorization_and_finish_scrubs_the_echo
  - oversize-response-with-a-secret-never-reaches-the-guest
  - crates/vam-network/src/proxy/mitm_tests.rs::oversize_from_a_secret_bound_host_is_refused_by_content_length
  - crates/vam-network/src/proxy/mitm_tests.rs::oversize_without_a_placeholder_is_refused_when_the_host_holds_a_secret
  - crates/vam-network/src/proxy/mitm_tests.rs::oversize_from_a_host_without_a_binding_still_streams
  - crates/vam-network/src/broker.rs::secret_bound_is_per_destination_not_per_request
  - crates/vam-orchestrator/src/secrets.rs::import_env_secrets_are_org_scope_and_ignore_user_rows
---

A secret a session needs lives in the node's vault. The guest never holds it. It holds a
placeholder, and the host proxy swaps the placeholder for the real value on the way out.

Scenarios named here run on the microVM lane (`scripts/e2e/run.sh --firecracker`). Both
`vault-secret-is-not-in-the-guest` and `secret-never-reaches-guest-encoded` are `active`; the first
passed on that lane on 2026-10-29. `oversize-response-with-a-secret-never-reaches-the-guest` is
`active` and runs on the process lane (`scripts/e2e/run.sh --scenarios
oversize-response-with-a-secret-never-reaches-the-guest`).

## The vault

| Part | What it does | Where in code |
|---|---|---|
| Storage | Each value is encrypted with AES-256-GCM under a 32-byte master key. | `crates/vam-orchestrator/src/secrets.rs:1223` |
| Kinds | `inference` (model provider keys) and `egress` (keys swapped into outbound requests). | `secrets.rs:110-113` |
| Scopes | Org scope and user scope. Environment import writes org scope only. | `secrets.rs:489-494` |
| Master key | `VAMD_SECRETS_MASTER_KEY` (64 hex characters) if set. Otherwise a `secrets.key` file (0600) beside the state database, created on first start. | `secrets.rs:17`, `:1150-1182` |
| Import | With `VAMD_IMPORT_ENV_SECRETS=1`, well-known provider variables become org-scope secrets at start. A binding that already exists is skipped. | `secrets.rs:492-520` |

The key file is the only copy of the key when the variable is unset. A lost key file means the
stored values cannot be read. `crates/vam-orchestrator/tests/api_tests.rs` covers set, list, get and
delete through the API. No test reads the master key variable or the key file, so that path is not
proved.

## Placeholder substitution

1. The session gets a placeholder for each binding: `vamsec_` plus 64 hex characters, stable for the
   run (`crates/vam-network/src/secrets.rs:52-70`).
2. The guest puts the placeholder in a header, body or query string.
3. The request goes out through the host proxy. Phase 1 substitutes URL placeholders
   (`secrets.rs:240`); phase 2 injects header and body values (`:292`).
4. A request that would carry a real value over plain HTTP is refused
   (`refuse_placeholders_over_plain_http`, `:214`, test
   `placeholder_over_plain_http_is_refused_before_injection`).

Because the host makes the connection, the value never enters the guest's environment, boot
arguments or workspace. Proved by `vault-secret-is-not-in-the-guest`.

## HTTPS and the per-session CA

To see inside an HTTPS request the proxy terminates TLS with a CA made for that one session
(`crates/vam-network/src/tls_ca.rs:41`). One ECDSA P-256 key signs every leaf certificate, and the
private key is zeroed when the session ends (`tls_ca.rs:58-64`). The certificate, never the key, is
served at `GET /v1/sessions/:id/egress-ca` to the session owner or an operator
(`crates/vam-orchestrator/src/api/mod.rs:172`, `:893`). Hosts listed in
`VAMD_EGRESS_PASSTHROUGH_HOSTS` are tunnelled without termination, so no placeholder is swapped
there (see [Network](/network/overview/)).

Proved by the two `mitm_tests.rs` tests in the front matter.

## What is scrubbed on the way back

* **Proxy responses.** `scrub_response` (`secrets.rs`) replaces the real value in a response's
  headers and body with the placeholder. Only a fully buffered response is scrubbed, so a
  response larger than 8 MiB (`DEFAULT_SCRUB_BUFFER`, `proxy/mitm.rs`) cannot be.
  * From a host that holds a secret binding in the session, such a response **fails closed**.
    The proxy answers `502` ("response too large to scrub") before any byte, status or header of
    the upstream response reaches the guest, closes the connection, and records a second
    `egress.decided` event for the host: disposition `DENY`, rule `oversize_with_secret`. The rule
    is per destination, not per request: it holds even when the request carried no placeholder,
    because an upstream can hand back a secret it was sent earlier. A body of exactly 8 MiB is
    still scrubbed. A response to a `HEAD` request is not refused, since it has no body.
  * From any other host, such a response streams to the guest as before, and the log line
    `egress.decision` records `scrubbed=false`, so large downloads from hosts without a secret
    still work. A download over 8 MiB from a host that has a secret fails, even when the request
    needs no secret.
  * Proved by `oversize-response-with-a-secret-never-reaches-the-guest`, by the tests
    `oversize_from_a_secret_bound_host_is_refused_by_content_length`,
    `oversize_chunked_from_a_secret_bound_host_is_refused_before_any_byte_is_sent`,
    `oversize_without_a_placeholder_is_refused_when_the_host_holds_a_secret`,
    `oversize_from_a_host_without_a_binding_still_streams` and
    `a_body_exactly_at_the_scrub_buffer_is_scrubbed_and_one_byte_over_is_refused` in
    `mitm_tests.rs`, and by `secret_bound_is_per_destination_not_per_request` in `broker.rs`. The
    scenario's origin is a test-node fixture with a 64 KiB buffer, not the 8 MiB default; the
    fixture is not compiled into a default `vamd`.
* **Tool output.** Host tools, MCP and hosted brokers pass their output through one redactor
  (`crates/vam-security/src/secret_forms.rs`, used at
  `crates/vam-orchestrator/src/host_tools.rs:185`). It removes the raw value, base64 (both
  alphabets, padded or not, and inside a larger blob), hex, percent-encoding, JSON escapes, forms
  split by whitespace, and any run of 12 or more bytes of the value. Proved by
  `secret-never-reaches-guest-encoded`.

## Not built yet

* Guest-output PII redaction. The redactor handles secrets only.
* A streaming scrubber, so large responses from a host that holds a secret are scrubbed rather than refused.
* A guest's own proxy (`HTTP_PROXY`) answers `CONNECT` with 501 and sends each request through the
  port-54 broker, which buffers and scrubs the whole body at any size. The streaming path above
  is the host's port-55 socket, which a guest reaches only by speaking `AF_VSOCK` itself.
* Reversed, XOR-ed or multiply-encoded forms are not covered (`secret_forms.rs` header).
