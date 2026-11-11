---
title: Security Overview
description: Where to read about VAM's security model. The pages linked here describe what the node does today.
status: stale
---

This page used to describe a planned security design: SPIFFE attestation, PII
masking, signed approval tokens and custom seccomp profiles. **None of those
exist.** The pages below describe what the node does today, and each is proved
by scenarios or tests.

The rule they all share: the guest microVM is untrusted. Policy decisions,
credentials and signing keys stay on the host, and the guest reaches host
services only through brokers over `AF_VSOCK`.

| Topic | Page |
|---|---|
| What separates a session from the host (Firecracker, the jailer and Firecracker's built-in seccomp filter, vsock), and what is not isolated | [Isolation](/architecture/isolation/) |
| Tool and internet rules: how they are stored, resolved and enforced | [Permissions](/security/permissions/) |
| How the vault stores a secret and the host proxy injects it, so the guest never holds it | [Secrets: vault and injection](/security/secrets-proxy/) |
| The signed record of a run, and how to verify it offline | [Run Receipt](/security/run-receipt/) |

Internet access is governed per agent. An agent with no `internet` setting is
allowed out; see [Permissions](/security/permissions/) for how to restrict it.

To report a vulnerability, see `SECURITY.md` at the repository root. Do not
open a public issue.
