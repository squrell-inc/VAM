---
title: Reproducibility & Caching
description: Achieving bit-for-bit identical image output, source normalization, and step caching mechanisms.
---

:::caution[Alpha Specification]
The caching layers and reproducibility verification rules are active design specifications.
:::

## The Goal of Bit-for-Bit Reproducibility

In virtualized systems, security and auditability depend on the predictability of the operating system image. If compiling the same `Vamfile` twice yields different block device checksums, validating image safety becomes impossible.

VAM enforces strict **bit-for-bit reproducibility** in its release pipeline: compiling identical input recipes against identical file contexts must produce binary-identical `rootfs.ext4` images down to the final byte.

---

## 1. Input Normalization & `SOURCE_DATE_EPOCH`

Uncontrolled metadata is the primary source of filesystem drift. Compiling the same directory twice usually yields different timestamps (`mtime`, `atime`, `ctime`) and different inode orders.

To eliminate this drift, the `rootfs` and `executor` modules normalize files in the staging tree:

### Time Normalization
VAM intercepts file write times and clamps them to a single, fixed Unix epoch timestamp using the `SOURCE_DATE_EPOCH` specification:

```text
Host File System (variable times)
  ├── file_A.py (mtime: 2026-08-10 10:14:15)
  └── file_B.js (mtime: 2026-08-10 10:15:32)
                     │
                     ▼ (Unified Normalization)
VAM Staging Tree (fixed epoch)
  ├── file_A.py (mtime: 1718000000)  <-- Clamped to SOURCE_DATE_EPOCH
  └── file_B.js (mtime: 1718000000)  <-- Clamped to SOURCE_DATE_EPOCH
```

Every file, directory, symlink, and socket inside the staging tree is set to this uniform timestamp before block device formatting begins.

### Permission & Ownership Mapping
* **umask Clamp**: Permissions are clamped to a strict umask (e.g., standard files are set to `0644`, directories to `0755`).
* **UID/GID Flattening**: Host system UIDs (e.g., `uid=1001(builder)`) are mapped to static guest system values (such as `root:root` or `1000:1000` for an unprivileged `agent` user).

---

## 2. Deterministic ext4 Formatting

Standard filesystem creation commands introduce random data into superblocks (such as random UUIDs, creation timestamps, and filesystem hash seeds).

VAM avoids this by overriding `mke2fs` default behaviors to configure deterministic fields:
* **Static UUIDs**: The filesystem UUID is generated as a UUID v5 hash calculated from the SHA-256 digest of the resolved staging tree contents.
* **Hash Seeds**: The Directory Index (dir_index) hash seed is initialized using a deterministic seed.
* **Deterministic Inode Allocation**: Files are written to the disk image in a strictly sorted alphabetical order, ensuring consistent physical inode block placement.

---

## 3. Content-Addressed Step Caching

To optimize build speeds without sacrificing safety, the `planner` compiles `Vamfile` directives into a content-addressed dependency tree.

The cache key for any stage is calculated as:

$$\text{CacheKey} = \text{SHA256}(\text{ParentStageHash} + \text{DirectiveText} + \text{ContextFilesHash} + \text{EnvVariables})$$

1. **Context Tracking**: The hash of any copied local file is calculated from its content digest, not its host modification time.
2. **Deterministic Cache Hits**: If the compiled `CacheKey` exists in the local or remote cache, the step execution is skipped, and the pre-compiled filesystem delta is extracted directly.
3. **Secret Isolation**: Ephemeral secrets mounted during `RUN` instructions are strictly excluded from cache key calculations to prevent credential-bound caches from leaking sensitive tokens.

---

## 4. Rebuild Verification

VAM provides automated tooling to verify reproducibility:
* **Double-Build Test**: In CI pipelines, the builder compiles the same recipe twice in parallel, using distinct staging roots and random worker PIDs.
* **Binary Diff**: The pipeline compares the two output `.ext4` files. If there is a single byte mismatch, the build fails the reproducibility audit, and a `diffoscope` report is generated to identify the leaky metadata.
