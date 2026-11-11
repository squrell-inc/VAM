# VAM Security Development Invariants

This file details the strict coding and operational rules that every agent or developer must adhere to when working inside `crates/vam-security/`. For detailed design rationales, refer to [Security Invariants](/security/invariants/) in the VAM documentation site.

---

## 1. Zero Trust & Host-Side Enforcement
* **Never perform final security evaluations inside guest memory**. The guest environment is untrusted by definition. All Cedar policy checks, PII redactions, and credential substitutions must execute on the trusted **host** side.
* **Keep raw private keys and secrets out of guest RAM**. API keys, token signing private keys, and external vault credentials must never be injected into VM filesystems or environment variables. Swap raw keys only at the host egress broker boundary.

## 2. API & Authorization Gate Ordering
* **Enforce Deny-By-Default startup sequencing**. Ensure the Cedar Policy Engine is healthy, fully compiled, and ready *before* `vam-core` initiates Firecracker boot barriers.
* **Implement immediate fail-closed state transitions**. If any security component encounters an internal exception, out-of-memory error, or timeout, evaluate the active operation as `DECISION_DENY`, pause or terminate the microVM, and emit an audit warning.

## 3. Cryptography & Token Hygiene
* **Utilize Ed25519 asymmetric cryptography as the production default**. Allow HMAC-SHA256 symmetric signing ONLY in local development/mock environments.
* **Enforce short-lived token lifetimes**. Limit signed HITL tokens to a maximum of **5 minutes** and SPIFFE SVID certificates to a maximum of **1 hour**.
* **Protect against token replay attacks**. Atomically consume reviewer token
  IDs in the durable approval transaction; an in-memory cache is insufficient
  across host restarts.

## 4. Subsystem Isolation & Kernel Filtering
* **Maintain two-tier Seccomp BPF profiles**. Tier 1 (applied by AWS Jailer) protects the host kernel from the VMM. Tier 2 (applied by `vam-runtime`) protects the guest kernel from untrusted tool subprocesses.
* **Redact before exporting telemetry**. Always run the `pii-redaction` engine over standard streams *prior* to appending logs to `VamCloudEvent` payloads or dispatching metrics to external OTLP collectors.

## 5. Coding Standards & Error Handling
* **Zero tolerance for panics**. Report all errors using `thiserror` mapping to the spec's `VamErrorCode` payload. Never use `.unwrap()`, `.expect()`, or raw `panic!` blocks in security libraries.
* **Ensure secure memory management**. Wrap raw secrets using the `secrecy` crate's `SecretString` and use `zeroize` to write zeros over physical memory upon struct drop.
