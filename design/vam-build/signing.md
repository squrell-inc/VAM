---
title: Signing & Admission Policy
description: Enforcing cryptographic image validation, trust roots, and fail-closed orchestrator admission check pipelines.
---

:::caution[Alpha Specification]
The cryptographic signing standards, Sigstore keys, and admission controller validation loops represent planned design specifications.
:::

## Cryptographic Supply Chain Assurance

Constructing an immutable rootfs block device is only valuable if the VAM host can guarantee that the disk has not been altered or replaced. To establish trust, VAM enforces a **Signed Admission Policy** inside the host daemon (`vamd`).

```text
    OCI Registry (Signed Artifact Bundle)
                  │
                  ▼  (Pull image & signatures)
    VAM Host Daemon (`vamd`)
     ├── 1. Read Policy Schema (KMS / Trust Roots)
     ├── 2. Verify Cosign Signature
     ├── 3. Confirm SLSA Provenance matches Vamfile
     ├── 4. Check Vulnerability Scanning Attestations
     │
     ▼  (SUCCESS: Authorized to boot)
    Launch Firecracker Sandbox
```

---

## 1. Cosign & Sigstore Signing Models

VAM utilizes **Cosign (Sigstore)** as its native signing standard. During build packaging, the `metadata` and `registry` modules sign the resolved OCI manifest SHA-256 digest.

The signing framework supports three primary security models:

### Keyless Signing (Recommended)
Leverages Sigstore's Fulcio CA and Rekor public transparency ledger.
* **Mechanism**: The build agent authenticates via an OCI-compatible OpenID Connect (OIDC) identity token (e.g., from GitHub Actions, AWS, or GCP). Fulcio issues a short-lived x509 certificate matching the builder's identity, and the signature is recorded in the Rekor ledger.
* **Benefit**: Eliminates the need to store, rotate, or protect long-lived private signing keys on the build host.

### KMS-Backed Signing
Connects to Cloud Key Management Services (such as AWS KMS, GCP KMS, or Azure Key Vault).
* **Mechanism**: The signature is generated inside the cloud KMS HSM hardware boundary. Private keys never leave the secure KMS enclave.

### Offline/Static Key Signing
Uses standard, password-protected private keys managed on secure local HSMs or build-server keychains.

---

## 2. Host Trust Roots & Key Rotation

The admission checker inside `vamd` parses a local policy configuration defining the platform's trusted identities (trust roots):

```json
{
  "policy_version": "vam/v1-alpha",
  "trust_roots": [
    {
      "name": "internal-corp-builder",
      "verification_type": "keyless",
      "oidc_issuer": "https://token.actions.githubusercontent.com",
      "oidc_subject": "https://github.com/company/vam-images/.github/workflows/build.yml@refs/heads/main"
    },
    {
      "name": "kms-backup-key",
      "verification_type": "public_key",
      "public_key_pem": "-----BEGIN PUBLIC KEY-----\nMIIBIjANBgkqhkiG9w0BAQEFAAOCAQ8AMIIBCgKCAQEA..."
    }
  ]
}
```

### Key Rotation & Revocation
* **Signature Freshness**: Admission rules can enforce short expiration windows on keyless certificates to invalidate old, compromised builds automatically.
* **Revocation Lists**: If a private key or a specific image digest is compromised, its hash is added to a local or remote CRL (Certificate Revocation List), causing `vamd` to reject it instantly.

---

## 3. Fail-Closed Orchestrator Admission

When an SDK or CLI requests `vamd` to launch a new agent session, the host control plane executes a multi-step **Admission Verification Loop**:

1. **Verify Signature**: Pulls the Cosign signature and validates it against the trust root configuration.
2. **Verify Attestations**: Confirms that the linked CycloneDX SBOM exists and passes the CVE vulnerability policy (e.g., rejecting any image containing Critical severity findings).
3. **Fail-Closed Guarantee**: If any check fails, or if a network timeout occurs while querying the registry for attestations, the admission checker aborts the VM creation request immediately. It **never falls back to an unverified local cache or mutable tag**.

---

## 4. Preventing TOCTOU Attacks

To protect the host node from **Time-of-Check to Time-of-Use (TOCTOU)** attacks (where a malicious user replaces a verified local `rootfs.ext4` file with an unverified or modified disk right before boot), VAM enforces two system-level bounds:

* **ReadOnly Cache Directory**: The directory storing uncompressed pulled rootfs images is owned by `root:root` and set to `0755` permissions, preventing non-privileged processes from modifying cached disks.
* **Digest Re-hash**: Right before handing the disk descriptor to `vam-core` and the Jailer sandbox, `vamd` re-calculates the local file's SHA-256 digest and matches it against the verified admission ledger. Any mismatch cancels the boot pipeline.
