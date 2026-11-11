> Moved from the docs site on 2026-10-31: design intent, not built (no FUSE code, no cloud backends, no sync, no encryption at rest; storage is a local ext4 image per session).

---
title: Storage Encryption & KMS
description: Transit security, server-side encryption, and KMS envelope keys inside Drive9.
---

:::note[Status: Fully Implemented Local Storage Plane]
For local-only operation, transit security is achieved via kernel-mediated host-guest VSOCK isolation, and at-rest security relies on standard host OS directory permissions and volume-level encryption (e.g. dm-crypt/LUKS). Advanced cloud envelope encryption remains a planned feature for the cloud adapter extension.
:::

Because Drive9 workspaces hold proprietary agent scripts, application code, databases, and potential user variables, securing data at rest and in transit is a paramount requirement. Drive9 implements three layers of security to guarantee absolute privacy of tenant files.

---

## 1. TLS in Transit Encryption

All communication channels used by Drive9 are strictly encrypted:
* **The Guest-Host Boundary**: The FUSE communication over `AF_VSOCK` utilizes kernel-mediated multiplexing. Guest microVMs cannot read, sniff, or alter other VM files.
* **The Host-Cloud Boundary**: Host-side cloud providers SDKs (AWS, GCP, Azure) enforce **TLS 1.3** for all chunk uploads and manifest reads, neutralizing any network-level sniffing or MITM (Man-in-the-Middle) threats.

---

## 2. Server-Side Encryption (SSE)

By default, Drive9 adapters require remote cloud backends to encrypt objects at rest. This handles hardware-level theft inside physical cloud data centers.

* **Amazon S3**: Enforces SSE-S3 (S3-managed keys) or SSE-KMS (AWS KMS keys).
* **Google Cloud Storage**: Integrates with Google-managed or Customer-managed encryption keys.
* **Azure Blob Storage**: Automatically encrypts all data using Microsoft-managed or Customer-managed keys.

---

## 3. Customer-Managed Envelope Encryption (BYOK)

For high-security enterprise environments, trusting the cloud provider with raw data keys is insufficient. Drive9 is designed with a planned **Customer-Managed Envelope Encryption (Bring-Your-Own-Key)** mode:

```text
 ┌────────────────────────────────────────────────────────┐
 │ Host Node: `vam-storage` Engine                        │
 │                                                        │
 │ 1. Requests Data Key (DEK) from AWS KMS / HashiCorp    │
 │    using Tenant Key (KEK)                              │
 └──────────────────────────┬─────────────────────────────┘
                            │
                            ▼ (KMS returns Encrypted DEK + Plaintext DEK)
 ┌────────────────────────────────────────────────────────┐
 │ Host Node: Encrypt Chunks using AES-256-GCM            │
 └──────────────────────────┬─────────────────────────────┘
                            │
                            ▼ (Pushes Encrypted Chunk Bytes)
 ┌────────────────────────────────────────────────────────┐
 │ Cloud Store: Host uploads encrypted chunk +            │
 │ encrypted DEK header                                   │
 └────────────────────────────────────────────────────────┘
```

### Encryption Protocol
1. **The Key Hierarchy**:
   - **Key Encrypting Key (KEK)**: Stored inside an enterprise Key Management Service (KMS) or HSM. It never exits the secure boundary.
   - **Data Encryption Key (DEK)**: An ephemeral, high-entropy key generated on the host using AES-256-GCM.
2. **The Encryption Step**: Before writing chunks to the local cache or uploading them to cloud buckets, `vam-storage` encrypts the raw byte bytes using the plaintext DEK.
3. **The Metadata Payload**: The corresponding encrypted DEK is serialized directly inside the Merkle Manifest, ensuring that only users with active KMS decrypt permissions on the master KEK can read or restore workspace files.

---

## 4. Next Steps

* To check the credential boundaries used to separate key custody, see [Cloud Adapter Catalog](/storage/backends/).
* Read how we enforce strict security rules in [Security Invariants](/storage/invariants/).
* To see how keys are verified, explore [Storage Testing Strategy](/architecture/testing/).
