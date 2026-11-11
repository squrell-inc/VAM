---
title: OCI Distribution
description: Distributing raw guest rootfs images using the OCI Image Specification and ORAS transport semantics.
---

:::caution[Alpha Specification]
The OCI manifest formats, custom media types, and registry transport models represent active design specifications.
:::

## Non-Container OCI Artifacts

VAM guest images are not runnable OCI containers (like Docker or Podman images). They do not share host kernel namespaces or run inside a Docker daemon. Instead, a VAM image is a raw, block-level `.ext4` disk file meant to be booted by the Firecracker hypervisor.

To leverage existing enterprise infrastructure, VAM packages these disk images as **non-container OCI artifacts** conforming to the OCI Image Specification.

```text
       VAM OCI Image Manifest
┌──────────────────────────────────────────────┐
│  schemaVersion: 2                            │
│  mediaType: application/vnd.oci.image.manifest.v1+json
│  config:                                     │
│    mediaType: application/vnd.vam.image.config.v1+json
│  layers:                                     │
│    - mediaType: application/vnd.vam.image.layer.v1.ext4+zstd
│      digest: sha256:abcd...                  │
└──────────────────────────────────────────────┘
```

---

## 1. Custom Media Types & Registry Compatibility

By declaring custom OCI media types, VAM files are safely stored, indexed, and queryable in any standard container registry (such as Amazon ECR, GitHub Packages, Google Artifact Registry, or JFrog Artifactory):

* **Image Config Media Type**: `application/vnd.vam.image.config.v1+json` — Describes the target CPU architecture (`x86_64` or `aarch64`), protocol version requirements, and metadata.
* **Disk Layer Media Type**: `application/vnd.vam.image.layer.v1.ext4+zstd` — Represents the Zstd-compressed raw `rootfs.ext4` sparse block device.
* **Metadata Media Type**: `application/vnd.vam.metadata.v1+json` — Packages supporting supply-chain metadata such as SBOMs and SLSA provenance files.

---

## 2. Pushing & Pulling with ORAS Semantics

VAM utilizes the **ORAS (OCI Registry as Storage)** client libraries in the `registry` module to manage push and pull operations.

* **Digest-Pinned Pulls**: While standard registries support tag names (such as `:latest` or `:v1-alpine`), tags are mutable and can be reassigned. To prevent execution drift, VAM orchestrators and hosts **strictly enforce pull-by-digest** in release pipelines.
* **Manifest Layer Stitching**: When pulling, the `registry` downloader parses the OCI manifest, fetches the `.ext4+zstd` layer block, extracts and decompresses it to local sparse storage, and validates the raw SHA-256 digest of the uncompressed file.

---

## 3. Connecting Signatures & SBOMs with Referrers

Using the **OCI Referrers API**, VAM links security signatures, SBOMs, and vulnerability scanning reports directly to the immutable image manifest.

```text
       [ Base Image Manifest (rootfs.ext4) ]
         SHA256: 026e63c0a5b18f62...
                     ▲
                     │ (Subject Referrer Link)
         ┌───────────┴───────────┐
         │                       │
 ┌───────┴───────┐       ┌───────┴───────┐
 │   Cosign      │       │ CycloneDX     │
 │  Signature    │       │   SBOM        │
 └───────────────┘       └───────────────┘
```

* **No Layer Modification**: Because SBOMs and signatures are added after compilation, they do not mutate the raw `.ext4` image bytes.
* **Subject Relationship**: They are uploaded to the registry as distinct artifacts that declare the base image's SHA-256 digest as their `subject` reference.
* **Verification Query**: When the host daemon (`vamd`) resolves an image, it queries the registry's `/v2/<name>/referrers/<digest>` endpoint to fetch and verify all signed associations before executing the container sandbox.
---

## 4. Promotion & Garbage Collection

* **Image Promotion**: Security teams can promote approved images from staging registries to production namespaces by copying their raw manifests and referrers without modifying the base image hash.
* **Garbage Collection**: Unreferenced image layers are cleaned up on the host cache using Least Recently Used (LRU) rules. If an image's signature or attestation is revoked, it is immediately pruned from the host's active cache.
