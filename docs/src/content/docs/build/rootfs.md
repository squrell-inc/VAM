---
title: RootFS Block Device Assembly
description: Low-level formatting, sparse image file handling, ext4 file structures, and metadata determinism.
status: planned
---

## Host prerequisites (`scripts/build-rootfs.sh`)

| Requirement | Notes |
|---|---|
| Rust `x86_64-unknown-linux-musl` target | `rustup target add x86_64-unknown-linux-musl` |
| `x86_64-linux-musl-gcc` on `PATH` | **Amazon Linux 2023:** no `musl-gcc` in `dnf`; install [musl.cc](https://musl.cc/) `x86_64-linux-musl-cross` under `/opt` and export `PATH="/opt/x86_64-linux-musl-cross/bin:$PATH"`. **Debian/Ubuntu:** `apt install musl-tools`. |
| Docker | Pulls `busybox:musl` and `alpine:latest` for staging |
| `e2fsprogs` | `mke2fs` (the host builds the data image with it), `e2fsck`. The host no longer runs `debugfs` on guest images: it reads them with a built-in safe reader. |

Do **not** use `sudo dnf install musl-gcc` on AL2023 — the package does not exist in the default repositories.

---

## Direct Bootable RootFS Layout

Unlike standard virtual machine images (which include partition tables, Master Boot Records (MBR), or EFI system partitions), Firecracker boots the guest Linux kernel directly using host-side configurations. Therefore, the compiled guest image must be a **raw ext4 filesystem block device containing no partition tables**.

```text
Standard VM Disk (.qcow2 / .img)
┌───────────────────┬────────────────────────────────────────┐
│ Partition Table   │ Partition 1: ext4 Filesystem (guest OS) │
│ (MBR / GPT / EFI) │ /etc, /bin, /usr, /var...              │
└───────────────────┴────────────────────────────────────────┘

Firecracker RootFS (.ext4) — DIRECT EXT4 BLOCK DEVICE
┌────────────────────────────────────────────────────────────┐
│ Raw ext4 Filesystem containing guest OS structure           │
│ /etc, /bin, /usr, /var...                                  │
└────────────────────────────────────────────────────────────┘
```

The virtual machine's virtual block device mounts this file directly as `/dev/vda` or `/dev/root` at boot.

---

## Sparse Raw ext4 Generation

To optimize physical storage allocation and speed up remote transfers, `vam-build` structures filesystems as sparse files.

1. **Space Allocation**: The compiler allocates a sparse file using `truncate` or `dd` with seek limits, creating a virtual file of, for example, 2GB, while taking 0 bytes of physical host storage initially.
2. **Deterministic Formatting**: The block size, inode counts, and reserved block percentage (usually `0%` for guest agents) are configured.

### The `mke2fs -d` Assembly Path
The rootless directory assembler creates the filesystem without mount helpers by packing files into the raw image directly via the `-d` (directory) flag:

```bash
mke2fs -t ext4 \
       -F \
       -O ^has_journal \
       -m 0 \
       -N 200000 \
       -d /tmp/vam-build-stage-XYZ/ \
       ./rootfs.ext4 \
       2G
```

* **`-O ^has_journal`**: Disables the ext4 filesystem journal. Because guest virtual machines are ephemeral and state-isolated, journal writes incur unnecessary performance overhead.
* **`-m 0`**: Reduces the super-user reserved block percentage to 0% (standard Linux reserves 5% of storage for system maintenance, which is not required inside an isolated agent microVM).
* **`-N 200000`**: Pre-allocates a generous number of inodes to accommodate intensive guest development environments (such as nested node_modules folders).

---

## Metadata Determinism

To ensure identical build inputs yield bit-for-bit identical outputs, VAM normalizes all filesystem metadata before compiling the block device:

* **File Timestamps**: All files in the staging directory are modified to match a single, fixed epoch timestamp (such as the standard `SOURCE_DATE_EPOCH` environment variable).
* **UID/GID Mapping**: Files are mapped to specific, deterministic guest users (e.g., all files owned by `root` or a non-privileged `agent` user).
* **UUID & Hash Seeds**: The ext4 UUID and hash seed are generated from the content hash of the resolved staging tree rather than randomized variables, ensuring deterministic block ordering.

---

## Verification & Integrity Checking

Before finalizing an image, VAM executes automated filesystem checks to verify structural consistency:

```text
┌────────────────────────────────────────────────────────┐
│                   Staging Check Suite                  │
├────────────────────────────────────────────────────────┤
│                                                        │
│  [ fsck -f -y -p ./rootfs.ext4 ] ──► (Verify blocks)   │
│                                                        │
│  [ compatibility-check ./rootfs ] ──► (Check contract) │
│                                                        │
└────────────────────────────────────────────────────────┘
```

1. **`e2fsck` Pass**: Forces a complete filesystem consistency check to guarantee there are no bad inodes, corrupt blocks, or overlapping directories.
2. **Sparsification**: The image is processed with `fallocate --dig` or written out sparsely, zeroing out unallocated blocks to ensure high-ratio Gzip or Zstd transfer compression.
3. **Inventory Manifest**: VAM generates an image-specific manifest summarizing:
   * Block size, UUID, and inode counts.
   * A full tree catalog listing files, file sizes, and individual SHA-256 file hashes.

---

## Shared Library & Runtime Injection (Decision R7)

Ephemeral guest agent VMs require full developer runtimes to process complex tool logic. VAM's `scripts/build-rootfs.sh` automatically installs:
1. **Bun (Musl Baseline)**: Downloaded directly from official releases and unpacked to `/usr/local/bin/bun`.
2. **Alpine Base Shared Libraries**: Because Bun is a dynamically linked ELF binary, running it inside the bare `busybox:musl` environment without a dynamic linker returns `os error 2 (No such file or directory)`. VAM mounts Alpine, runs `apk add --no-cache libstdc++` in a temporary layer, and extracts:
   * `/lib/ld-musl-x86_64.so.1` (the dynamic loader)
   * `/usr/lib/libgcc_s.so.1`
   * `/usr/lib/libstdc++.so.6`
   * `/lib/libc.musl-x86_64.so.1` (symbolic link mapping to the loader)

This injects full modern C++ and JS capabilities into an ultra-lean, 256MB ext4 direct-boot image.
