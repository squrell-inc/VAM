---
title: Image Optimization
description: Deep dive into rootfs filesystem shrinking, block-level zeroing, sparsification, and boot-time optimization.
---

:::caution[Alpha Specification]
The shrinking pipeline, zeroing tools, and optimization heuristics represent a planned design.
:::

## The Optimization Lifecycle

To maximize host density (the number of concurrent microVM instances running on a single physical host node) and minimize cold-boot latency, guest images must be compressed to their absolute minimum size.

`vam-build` executes an automated, five-stage filesystem optimization lifecycle:

```text
Staging Directory (populated)
               │
               ▼
┌──────────────────────────────────────────┐
│ 1. Policy-Controlled Cache Cleanup       │
│    - Remove package caches, logs, temp   │
├──────────────────────────────────────────┤
│ 2. Consistency & Integrity Check (fsck)  │
│    - Verify block-level structural safety│
├──────────────────────────────────────────┤
│ 3. Sizing Calculation & Headroom Booking  │
│    - Measure exact size and book buffer  │
├──────────────────────────────────────────┤
│ 4. Filesystem Resizing & Truncation      │
│    - Run resize2fs -M to shrink disk size│
├──────────────────────────────────────────┤
│ 5. Sparsification & Compression Packaging│
│    - Zero free blocks and compress Zstd  │
└──────────────────────────────────────────┘
```

---

## 1. Policy-Controlled Cache Cleanup

Before building the image, the `optimizer` module runs automated cleaning sweeps to remove non-essential compile-time metadata:
* **Package Database Clean**: Executes equivalent cleanup rules depending on the base distro (e.g., `apk cache clean`, `apt-get clean`, or `dnf clean all`).
* **Source/Debug Stripping**: Strips debug symbols from compiled binary tools and deletes intermediate compiler artifacts (like Rust's `target/` directories or Node's source caches).
* **Temp/Log Erasure**: Empties log folders (`/var/log/*`), temporary folders (`/tmp/*`), and command histories.

---

## 2. File Sizing & Headroom Reservation

A guest image that is packed "too tightly" has 0 bytes of free space left on its root block. While mutable workspaces reside in Drive9, the guest OS still requires small amounts of local write access during execution (for example, to write temporary cache files, parse Python imports, or update configuration maps in `/etc/`).

Rather than leaving the image completely full, `vam-build` calculates and books explicit **free-space headroom**:
* **Minimum Size Detection**: Measures the exact sum of bytes of all remaining files.
* **Headroom Percentage / Buffer**: Applies a configured headroom percentage (such as `10%`) or a static buffer size (such as `100MB`) to determine the target disk image geometry.

---

## 3. Resizing and Truncating Safely

The compiler applies the resizing operation on the unmounted block device using standard Linux filesystem utilities:

```bash
# Force a clean filesystem check before resizing
e2fsck -f -y ./rootfs.ext4

# Shrink the filesystem to its minimum physical size
resize2fs -M ./rootfs.ext4

# Truncate the sparse file to match the newly shrunk ext4 boundary
truncate -s $(du -b ./rootfs.ext4 | cut -f1) ./rootfs.ext4
```

This sequence guarantees that the filesystem block mapping is safe and stable, reducing the raw size of the image file on disk.

---

## 4. Sparsification & Transfer Compression

To ensure the output image is lightweight for network distribution, VAM sweeps unallocated blocks:
* **Block Zeroing**: The compiler zero-fills all unallocated space within the ext4 disk. Because compression algorithms achieve extreme ratios when processing continuous arrays of zeros, this step drastically reduces the final packaged size.
* **Transfer Compression**: The final sparse `.ext4` image is wrapped inside standard OCI layers compressed with **Zstd** (`--ultra -22`) or high-ratio **Gzip**. For example, a 1GB raw sparse Alpine disk containing Node.js and the VAM agent is typically reduced to **under 45MB** for network transport.
