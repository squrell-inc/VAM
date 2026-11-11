> Moved from the docs site on 2026-10-30: design intent, not built.

---
title: SPIFFE/SPIRE Workload Attestation
description: Cryptographic identity issuance, short-lived SVID management, and vsock delivery in VAM.
---

:::caution[Alpha Stability Warning]
VAM security features and subsystems (under the `vam-security` crate) are currently in **Alpha**. The security model, cryptography protocols, and policy integrations are actively evolving, breaking changes are expected, and no production-readiness guarantees are provided at this stage. Do not deploy in untrusted production environments without custom hardening.
:::

:::note[Status: Planned]
SPIFFE identity validation and integration is the chosen direction for VAM but is **not yet implemented**. This document serves as the architectural spec for development.
:::

VAM uses [SPIFFE](https://spiffe.io/) (Secure Production Identity Framework for Everyone) and [SPIRE](https://spiffe.io/docs/latest/spire-about/) (the SPIFFE Runtime Environment) to solve a fundamental security challenge: **How does an isolated, ephemeral guest microVM securely identify itself to external services without hardcoded API keys or static credentials?**

---

## 1. The Workload Attestation Boundary

Rather than letting the untrusted guest VM contact the SPIRE Server directly, VAM implements a delegated, host-mediated attestation model. 

The **Host VAM Daemon** (`vamd` / `vam-core`) performs attestation on behalf of the guest:

```text
 ┌─────────────┐             ┌────────────┐             ┌─────────────┐
 │ SPIRE Agent │ ◄────────── │ `vam-core` │ ──────────► │ Firecracker │
 │  (on Host)  │  Attest Host│   (Host)   │  Spawns VMM │  VMM (Host) │
 └─────────────┘             └────────────┘             └─────────────┘
                                    │                          │
                                    │ Establishes              │ Launches
                                    ▼                          ▼
                             ┌────────────┐             ┌─────────────┐
                             │ `AF_VSOCK` │ ◄────────── │ `vam-runtime│
                             │  Port 52   │  Injects    │   (Guest)   │
                             │            │  SVID       │             │
                             └────────────┘             └─────────────┘
```

### Attestation Criteria
The host `vam-security` module queries the local SPIRE Agent using the Unix Domain Socket workload API. The SPIRE Agent attests `vam-core` based on host-level criteria:
* **Linux PID & cgroup**: Verifies the calling daemon belongs to the trusted systemd slice.
* **Binary Hash**: Matches the SHA-256 of the `vamd` executable.
* **Owner UID/GID**: Enforces that only the unprivileged `vamd` user can request identities.

---

## 2. SVID Lifecycle & Vsock Delivery

Once the host daemon is attested, it requests a short-lived **X.509 SVID** (SPIFFE Verifiable Identity Document) specifically scoped for the newly spawned guest session (e.g., `spiffe://vam.local/ns/default/agent/session-abc123`).

The SVID lifecycle proceeds as follows:

1. **Generation**: SPIRE Agent generates an X.509 certificate with a short lifetime (**default: 1 hour**).
2. **Serialization**: The host serializes this identity into the `SpiffeIdentity` Protobuf structure defined in `spec/security.md`.
3. **Injection**: During the VM bootstrap barrier (prior to starting agentic workflows), the host pushes the `SpiffeIdentity` payload across `AF_VSOCK` Port 52 to `vam-runtime`.
4. **Acquisition**: `vam-runtime` writes the certificate (`svid.pem`) and private key (`svid_key.pem`) into an in-memory `tmpfs` RAM disk inside the guest (`/run/keys/`).

---

## 3. Key Rotation & Hot-Reload

Because SVIDs expire in 1 hour, they must be rotated continuously:
* **Host Monitor**: The host `vam-security` engine monitors the SVID's TTL.
* **Re-Push**: At **50% of the SVID's lifetime** (30 minutes), the host requests a renewed certificate from SPIRE, wraps it in a new Protobuf frame, and writes it over vsock.
* **Runtime Update**: `vam-runtime` receives the frame, atomically replaces the files in `/run/keys/`, and triggers a signal (or file-watcher event) so active guest tools can reload their client TLS configurations without process restarts.

---

## 4. The Snapshot Identity-Collision Hazard

The use of instant VM snapshots (`/snapshot/load`) introduces a critical cryptographic hazard that must be mitigated by `vam-security` and `vam-runtime`.

:::danger[Snapshot Identity Collision]
When N microVMs are cloned from a single, warm-booted parent snapshot:
1. **Clock Skew**: The restored guests inherit a frozen, stale clock. TLS handshake validations against modern time servers will immediately fail.
2. **Key Duplication**: Every cloned instance inherits the *exact same* memory pages and disk files, meaning they share the same SPIFFE private key and SVID certificate.
3. **Replay Vulnerability**: Multiple sibling VMs presenting the same identity certificate simultaneously will trigger security anomaly alerts in external trust bundles.
:::

### Mitigation Protocols
To resolve these hazards, the snapshot restoration process must execute three steps sequentially:
1. **Host Re-Attestation**: Upon `/snapshot/load`, the host *must* immediately generate a brand-new SVID for the new CID instance.
2. **Immediate Override**: The host writes the new `SpiffeIdentity` over the vsock connection *before* releasing the guest execution thread.
3. **Guest Clock Sync & Reload**: The guest kernel clock is synced (using vsock timing sync), and the `/run/keys/` directory is overwritten before any external tool makes an outbound connection.

---

## 5. Next Steps

* To understand how granular actions are evaluated after identity is established, see the [AWS Cedar Policy Engine](/security/cedar/).
* To see how secrets are handled on external calls, explore the [Zero-Trust Secret Injection Proxy](/security/secrets-proxy/).
