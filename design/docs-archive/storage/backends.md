> Moved from the docs site on 2026-10-31: design intent, not built (no FUSE code, no cloud backends, no sync, no encryption at rest; storage is a local ext4 image per session).

---
title: Cloud Adapter Catalog
description: Operational configuration, credential boundaries, and multi-cloud compatibility for AWS S3, GCS, Azure Blob, and local fallback.
---

:::note[Status: Fully Implemented Local Storage Plane]
The storage data plane is optimized for **local-only** operation with the `LocalDirBackend` adapter to achieve high-performance and fully isolated unprivileged disk mounting. Cloud adapters are planned/future additions as compile-time features.
:::

To ensure high portability and flexibilty, VAM ships with four native compile-time storage adapters built into `vam-storage`. This catalog outlines the configurations, credentials boundaries, and functional traits of each provider.

---

## 1. Multi-Cloud Compatibility Matrix

The table below summarizes the operational differences and capabilities enforced by each of Drive9's native backend adapters:

| Backend Adapter | Primary Use-Case | Concurrency Check (CAS) | Credential Mechanism | Local Cache Needed? |
|---|---|---|---|---|
| **Amazon S3** | Standard AWS Cloud Production | ETag (`If-Match`) | AWS IAM Role / Web Identity | Yes (high-latency) |
| **GCS** | Google Cloud Production | Generation (`ifGenerationMatch`) | GCP Workload Identity | Yes (high-latency) |
| **Azure Blob** | Microsoft Azure Production | ETag (`If-Match`) | Azure Managed Identity | Yes (high-latency) |
| **Local File** | Local Development & Nesting | Sequence ID (Mutex locks) | Host Directory Permissions | No (sub-millisecond) |

---

## 2. Configuration Parameters

Backends are defined in the host-level `StorageConfig` configuration structure.

### Amazon S3 and S3-Compatible Configuration
The S3 adapter is fully compatible with S3-compliant API endpoints, enabling operators to swap Amazon S3 with alternative object systems (such as **MinIO**, **Cloudflare R2**, **Ceph RGW**, or **Backblaze B2**):

```toml
[storage]
backend = "s3"
bucket = "vam-workspace-prod"
prefix = "v1/workspaces"

[storage.s3_opts]
region = "us-west-2"
endpoint = "https://s3.us-west-2.amazonaws.com"  # Can be swapped to custom MinIO url
force_path_style = false
```

### Google Cloud Storage Configuration
```toml
[storage]
backend = "gcs"
bucket = "vam-workspace-prod"
prefix = "v1/workspaces"

[storage.gcs_opts]
service_account_path = "/etc/gcp/storage-key.json" # Left blank to default to Workload Identity
```

### Azure Blob Storage Configuration
```toml
[storage]
backend = "azure"
container = "workspaces"
prefix = "v1/workspaces"

[storage.azure_opts]
account_name = "vamstorageprod"
```

### Local Filesystem Configuration (Development Mode)
```toml
[storage]
backend = "local"
directory = "/var/lib/vam/storage/workspaces"
```

---

## 3. Host-Only Credential Boundaries (Decision ST5)

To maintain a secure sandbox boundary, **cloud credentials must never be injected inside the guest microVM**.
* **Zero VM Credentials**: In-VM processes do not have access to GCP credentials files, AWS credential chain variables (`AWS_ACCESS_KEY_ID`), or Azure managed token URLs.
* **Host Interception**: The `vam-storage` module running inside the host daemon manages all SDK handshakes (e.g., retrieving tokens from the AWS IMDS metadata service or GCP identity endpoints).
* **FUSE Redirection**: The guest's FUSE file-actions are routed entirely through VSOCK port 52 control lines. The guest has zero awareness of *where* or *how* its file blocks are physically written to cloud resources.

---

## 4. Next Steps

* To understand how `vam-storage` translates files into chunks, see [Drive9 Workspace Data Format](/storage/data-format/).
* Explore how synchronization barriers coordinate background uploads in [Differential S3 Background Sync Engine](/storage/sync/).
* Read the consistency models used to handle concurrent writers in [Consistency Model & Write Locks](/storage/consistency/).
