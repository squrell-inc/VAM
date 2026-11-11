---
title: Supply-Chain Metadata
description: Recording Software Bills of Materials (SBOM), SLSA provenance, and vulnerability scanning certificates.
---

:::caution[Alpha Specification]
The supply chain schemas, SBOM formatting, and SLSA attestation structures are active design specifications.
:::

## Immutable Supply Chain Assertions

A secure software supply chain requires that every built guest image carries a cryptographically verifiable trail of evidence. In VAM, the build process generates extensive metadata detailing **exactly how** the block device was constructed.

This metadata allows security teams and automated orchestrators (`vamd`) to verify compliance before allowing virtual machines to boot.

```text
       VAM Image Compilation Pipeline
                     │
                     ▼
┌──────────────────────────────────────────┐
│   - Compile rootfs.ext4                  │
│   - Inventory all guest packages         │  (CycloneDX / SPDX)
│   - Map all build environment steps      │  (SLSA Provenance)
└────────────────────┬─────────────────────┘
                     │
                     ▼
┌──────────────────────────────────────────┐
│        Signed Supply Chain Bundle        │
├──────────────────────────────────────────┤
│  - rootfs.ext4                           │
│  - image-manifest.json                   │
│  - sbom.json                             │
│  - provenance.json                       │
└──────────────────────────────────────────┘
```

---

## 1. The VAM Image Manifest

Every build generates an `image-manifest.json` describing the block device's properties. This manifest is included as an immutable descriptor layer inside the final OCI package.

```json
{
  "schema_version": "vam/v1-alpha",
  "architecture": "x86_64",
  "rootfs_sha256": "sha256:026e63c0a5b18f625902...",
  "filesystem": {
    "type": "ext4",
    "uuid": "4f9c8d62-1111-4a24-8b65-7284b39e6a0a",
    "block_size": 4096,
    "total_size_bytes": 2147483648
  },
  "compatibility": {
    "runtime_version": "v1.0.2",
    "kernel_profile": "standard-l7-isolated"
  }
}
```

---

## 2. Software Bill of Materials (SBOM)

To comply with modern cybersecurity standards, VAM automatically generates a **Software Bill of Materials (SBOM)** at build time using the CycloneDX or SPDX JSON format.

* **Automatic Scanning**: The `metadata` module scans the final guest root directory structure to inventory all installed files:
  * **System Packages**: Reads the guest's OS database (e.g., `/lib/apk/db/installed` on Alpine or `/var/lib/dpkg/status` on Ubuntu).
  * **Application Libraries**: Traverses target paths for package lockfiles (like `package-lock.json` or `poetry.lock`) to capture nested Node/Python libraries.
* **Integrity Hash**: Every item recorded in the SBOM includes its origin version, author license, and binary hash, making package drift immediately detectable.

---

## 3. SLSA Provenance Attestations

To prevent "man-in-the-middle" build attacks, VAM generates SLSA (Software Supply Chain Levels for Software Artifacts) Level 3 compliant **provenance certificates**.

These attestations record the history of the build execution environment:
* **The Builder**: The specific build machine type, builder engine version (`vam-build v1.0.0`), and host environment ID.
* **The Recipe**: The exact SHA-256 digest of the `Vamfile` used to compile the image.
* **The Context**: Digests of all copied local context folder files and environment variable definitions.
* **Build Steps**: An execution log recording each command, start/stop times, and output exit codes.

The SLSA provenance is written as an in-toto attestation format and linked directly to the output `rootfs.ext4` digest.

---

## 4. Vulnerability & Hardening Evidence

If the build system executes an automated security policy scan (such as running OpenSCAP checks or Trivy vulnerability audits), the results are compiled as additional attestations:
* **Scan Score**: Summary of any detected CVE vulnerabilities.
* **Hardening Evidence**: Assertion maps showing which CIS benchmark steps successfully verified inside the image.

The orchestrator `vamd` checks these signatures at runtime, enforcing a policy that **rejects images with unverified SBOMs or failed vulnerability scans**.
