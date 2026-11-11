---
title: Runtime & Driver Injection
description: How the static musl-compiled vam-runtime and FUSE drivers are injected and verified inside the rootfs.
status: planned
---

## The Guest Compatibility Boundary

For an operating system image to be compatible with the VAM virtualization and storage planes, it must contain:
1. **The Guest Agent (`vam-runtime`)**: An engine compiled for the target CPU architecture that can receive Vsock control streams from `vam-core`.
2. **Drive9 FUSE Driver Support**: User space filesystem drivers permitting the agent to mount `/mnt/drive9` via standard FUSE 3 interfaces.

Rather than forcing users to manually download, build, and configure these interfaces inside their `Vamfile`, `vam-build` automates this process via a dedicated **injection pipeline**.

```text
       Raw Guest OS rootfs staging tree
                      │
                      ▼
┌──────────────────────────────────────────┐
│      `vam-build` Injection Module        │
├──────────────────────────────────────────┤
│  - Pull pinned `vam-runtime` binary      │
│  - Verify SHA-256 binary signature       │
│  - Inject to `/usr/local/bin/`           │
│  - Set execute permissions (chmod +x)    │
│  - Inject FUSE 3 libraries & paths       │
└──────────────────────────────────────────┘
                      │
                      ▼
        Compatible Bootable RootFS Image
```

---

## 1. Acquiring & Verifying `vam-runtime`

To protect the guest virtual machine from supply chain tampering, the `injector` module never compiles `vam-runtime` on the fly. Instead, it pulls a pre-compiled, static binary from a trusted release registry.

* **Cryptographic Pinning**: The `injector` matches the requested VAM platform version against a hardcoded, cryptographically signed lockfile.
* **Signature Verification**: Before injection, the pulled `vam-runtime` executable is verified against a Cosign public key. If the signature is invalid or the digest does not match the pinned release, the build is aborted.
* **Architecture Matching**: The compiler detects the target guest CPU architecture (`x86_64` or `aarch64`) and pulls the corresponding static target binary. It strictly rejects compiling a cross-architecture image without compatible binaries.

---

## 2. Target Injection Paths & Permissions

The `injector` writes the validated guest runtime and its supporting system scripts to standard, immutable paths inside the staging tree:

* **`/usr/local/bin/vam-runtime`**: The absolute path of the static guest agent.
* **Executable Bit Enforced**: The file permissions are normalized to `0755` (`-rwxr-xr-x`), ensuring the file is executable.
* **System Ownership**: File ownership is strictly set to `0:0` (root/root) within the ext4 inode table, preventing non-privileged guest processes from modifying or replacing the agent.

---

## 3. FUSE 3 & FUSE Driver Configuration

To permit the guest agent to mount the Drive9 workspace over standard OS file systems, `vam-build` ensures the guest operating system has FUSE 3 libraries.

* **Static vs. Dynamic Linking**: For minimal distributions (like Alpine), the static-musl version of `vam-runtime` includes embedded FUSE helper paths. For glibc-based systems (like Ubuntu and RHEL), the injector verifies the presence of `/usr/lib/libfuse3.so.3` or installs it via the staging environment's package manager.
* **`/dev/fuse` Device Node**: The injector ensures that the guest `/dev` directory includes the correct character device node configuration for FUSE:
  ```text
  crw-rw-rw- 1 root root 10, 229 Jan 1 00:00 /dev/fuse
  ```

---

## 4. No Secret Leakage Guarantee

The injector enforces a strict, automated sweep before closing the staging tree:
* **No Cache Files**: Cleans up package manager databases, command histories (`.bash_history`, `.ash_history`), and temporary staging folders (`/tmp/*`, `/var/cache/*`).
* **Entropy Sweep**: Inspects the staging folder for high-entropy strings or file names resembling private certificates (e.g., `.pem`, `.key`, `id_rsa`) to guarantee that no build-time secret accidentally escapes into the immutable image.
