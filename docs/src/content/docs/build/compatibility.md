---
title: Guest Compatibility Contract
description: The essential OS-level structures, device nodes, and system paths required for a rootfs to boot in VAM.
status: planned
---

## The Compatibility Interface

An arbitrary Linux filesystem image cannot be booted by the VAM host. To ensure stable startup, correct network routing, and successful communication over VSock and FUSE, the compiled rootfs must adhere to a strict **Guest Compatibility Contract**.

This contract acts as the API interface between the host control plane (`vamd` / `vam-core`) and the guest virtual machine space.

```text
    VAM Host Plane (Host Daemon / Kernel)
                  │
                  ▼  (AF_VSOCK Port 52 / /dev/fuse)
    Guest VM Plane (Isolated Sandbox)
     ├── PID 1 Entrypoint: `/usr/local/bin/vam-runtime`
     ├── System Paths: `/proc`, `/sys`, `/dev`, `/mnt/drive9`
     └── Network Config: `/etc/resolv.conf`, resolv paths
```

---

## 1. The PID 1 Entrypoint Contract

When the Linux kernel boots inside the Firecracker sandbox, it mounts the rootfs block device and searches for a system init process (traditionally `/sbin/init` or `/usr/lib/systemd/systemd`).

In VAM, the guest initialization process is handled differently:
* **Direct Init Execution**: The image is configured to launch the static, compiled `vam-runtime` directly as PID 1.
* **Responsibilities of PID 1**:
  * **System Mounting**: Mounts crucial pseudo-filesystems (`/proc`, `/sys`, `/dev`, `/dev/pts`, `/tmp`, `/run`).
  * **Signal Reaping**: Acts as system init, catching `SIGCHLD` signals to prevent zombie process accumulation during tool execution.
  * **VSock Setup**: Opens AF_VSOCK port 52 to establish RPC connections back to the host control plane.
  * **Drive9 Mount**: Initializes the FUSE engine to map `/mnt/drive9`.

---

## 2. Mandatory System Directories & Device Nodes

The rootfs staging tree must contain a set of pre-allocated directories and hardware special files.

### Required Directories
* **`/proc`**: Target mount point for the kernel process information filesystem.
* **`/sys`**: Target mount point for the sysfs device-tree interface.
* **`/dev`**: Target mount point for the kernel devtmpfs device nodes.
* **`/tmp` & `/run`**: Ephemeral, writable RAM-backed scratch spaces.
* **`/mnt/drive9`**: The explicit, pre-created empty mount point reserved for mapping mutable Drive9 workspace clusters.

### Required Special Devices
VAM's virtualization drivers assume the existence of the following device nodes within `/dev/`:
* `/dev/null` (char, 1:3)
* `/dev/zero` (char, 1:5)
* `/dev/random` (char, 1:8)
* `/dev/urandom` (char, 1:9)
* `/dev/vsock` (char, 10:124) — Critical for low-level host communication.
* `/dev/fuse` (char, 10:229) — Critical for Drive9 storage mounts.

---

## 3. Core Network Configuration Paths

While the network interfaces are allocated dynamically by the host (see [crates/vam-network](/build/overview/)), the guest OS requires localized configurations to resolve nameservers safely:

* **`/etc/resolv.conf`**: This file is overwritten dynamically at boot time by `vam-runtime`. It points the guest system's DNS lookup queries to the host-backed isolated proxy address (`172.20.0.1` or equivalent).
* **CA Certificate Bundle**: To execute HTTPS/TLS connections (e.g., calling OpenAI, Anthropic, or GitHub APIs), the guest OS must have a valid root certificate bundle installed at standard paths (e.g., `/etc/ssl/certs/ca-certificates.crt` on Alpine/Ubuntu or `/etc/pki/tls/certs/ca-bundle.crt` on UBI).

---

## 4. Host-Side Validation & Boot Smoke Tests

To guarantee compatibility before an image is pushed to the OCI registry, `vam-build` executes an automated boot validation pass:
1. **Compatibility Inspection**: Parses the compiled ext4 filesystem and verifies that all required paths and device nodes exist.
2. **Firecracker Smoke Test**: Boots the raw `rootfs.ext4` in a headless Firecracker sandbox.
3. **Init Handshake Check**: Waits up to 2 seconds for a `vam-runtime` "ready" handshake over a mock VSock CID. If the handshake times out, the build fails the compatibility validation audit, indicating that the kernel or guest agent failed to initialize correctly.
