---
title: Host Prerequisites
description: System requirements, kernel artifacts, and environment validation.
---

:::caution[Alpha Stability Warning]
VAM host architecture specifications are currently in **Alpha**. The prerequisites and configuration targets reflect active Linux-first strategies.
:::

Because `vam-core` interacts directly with hardware-assisted virtualization and low-level Linux namespaces, several structural dependencies must be configured on the host system.

---

## Linux System Requirements

To execute microVM instances safely and with bare-metal speed, the host environment must support the following:

### 1. KVM (Kernel-based Virtual Machine)
* **Path**: `/dev/kvm` must exist and be readable/writable by the process executing `vam-core`.
* **Nested Virtualization**: If running `vam-core` inside a parent cloud VM (e.g. AWS EC2, GCP Compute Engine), nested virtualization must be enabled (e.g., Intel VMX or AMD SVM passed through).
* **Permissions Group**: The process should execute as part of the `kvm` system group:
  ```bash
  # Verify host support
  ls -l /dev/kvm
  # crw-rw-rw- 1 root kvm 10, 232 Aug 10 00:00 /dev/kvm
  ```

### 2. cgroup v2 (Control Groups)
* VAM requires cgroup v2 (specifically the unified hierarchy mounted at `/sys/fs/cgroup/`) to restrict vCPU usage, memory growth, and process allocation inside AWS Jailer sandboxes.

### 3. Seccomp BPF Support
* The host kernel must support Secure Computing Mode (specifically `SECCOMP_MODE_FILTER`) so that Jailer can successfully inject the custom Seccomp BPF program restricting Firecracker's system calls.

---

## Required Boot & Disk Artifacts

`vam-core` does not build virtual disk layers or compile guest kernels dynamically. It acts as a consumer of pre-baked files.

```text
Host Storage Layout:
/var/lib/vam/
├── kernels/
│   └── vmlinux-5.10.x   # Uncompressed ELF Linux kernel image
└── rootfs/
    └── ubuntu-24-base.ext4  # Bare filesystem disk image (ext4 format)
```

1. **Guest Kernel (`vmlinux`)**: Must be an **uncompressed** ELF binary compiled with Firecracker-compatible minimal boot options. Standard bzImage/vmlinuz compressed formats are **not** supported by Firecracker.
2. **Root Filesystem (`rootfs.ext4`)**: A static ext4 block device image containing the minimal init system, `vam-runtime` binary configured to start on boot, and support dependencies (e.g. Bun, Python).

---

## Firecracker & Jailer Version Pinning

To maintain absolute stability across host clusters, VAM pins the binaries strictly. Mixing unmatched binaries causes silent boot freezes or API mismatches.

| Binary | Pin Version | Download Target |
|---|---|---|
| **Firecracker** | `v1.16.1` | `https://github.com/firecracker-microvm/firecracker/releases` |
| **AWS Jailer** | `v1.16.1` | Bundled in the same official Firecracker release |

---

## macOS Development Caveat (Decision C7)

:::danger[macOS Platform Restriction]
The `vam-core` crate **cannot compile or run natively on macOS** (Darwin). It relies on Linux-only kernel headers (`sys/ioctl.h`, KVM IOCTLs, and cgroup control directories).
:::

Because developers of VAM may work on macOS (such as on this Apple Silicon machine), the following **development workflow** is enforced:

1. **Build Trait Seams**: Local code editing and unit tests on macOS utilize the `Vmm` trait mocking structures (see [Testing Strategy](/architecture/testing/)) to ensure compilation succeeds locally.
2. **Remote Development Host**: For integration testing and runtime validation, developers use SSH, VS Code Remote Containers, or a Linux VM (running under UTM, Parallels, or [Lima with nested KVM](/getting-started/macos-lima/)) with cgroups v2 enabled:
   ```bash
   # Check if virtualization nested acceleration is running in Linux VM
   egrep -c '(vmx|svm)' /proc/cpuinfo
   # Output must be > 0
   ```
