---
title: Builder Backends
description: Deep dive into rootless, privileged, and nested microVM execution sandboxes for image builds.
status: planned
---

## The Backend Execution Model

The `executor` module in `vam-build` isolates untrusted `RUN` commands during compilation. Because these commands are capable of downloading unknown packages and executing arbitrary post-install shell hooks, they must not run directly on the host file system.

`vam-build` specifies three discrete execution backends, each representing a different security, privilege, and performance trade-off.

```text
               ┌───────────────────────────────┐
               │    VamBuild Executor Factory  │
               └───────────────┬───────────────┘
                               │ (Capability Probe)
         ┌─────────────────────┼─────────────────────┐
         ▼                     ▼                     ▼
┌─────────────────┐   ┌─────────────────┐   ┌─────────────────┐
│ Rootless Dir    │   │ Privileged Loop │   │ Nested MicroVM  │
│ (Safe Default)  │   │  (Opt-in / CI)  │   │ (Future Spec)   │
├─────────────────┤   ├─────────────────┤   ├─────────────────┤
│ - No loop devices│   │ - Loop mounting │   │ - Isolated kernel│
│ - mke2fs -d tree│   │ - Real chroot   │   │ - Full sandboxing│
│ - User namespace│   │ - Root required │   │ - Slow startup  │
└─────────────────┘   └─────────────────┘   └─────────────────┘
```

---

## 1. Rootless Directory Backend (Default)

The Rootless Directory backend is VAM's recommended default compilation mode. It requires zero host administrative privileges, eliminating the need for `sudo`, suid helpers, or dangerous loops.

### How it Works
1. **Staging Tree**: The executor creates a standard staging directory on the host (e.g., `/tmp/vam-build-stage-XYZ/`).
2. **User Namespaces**: To emulate root ownership during package installation, it uses Linux user namespaces (`user_namespaces(7)`). It maps the current unprivileged host UID to root (`0`) inside the execution sandbox.
3. **No-Loop Assembly**: Rather than formatting and mounting a block device to write files, the directory is populated entirely in user space.
4. **`mke2fs -d` Assembly**: Once the staging tree is fully prepared and validated, the `rootfs` module calls:
   ```bash
   mke2fs -t ext4 -d /tmp/vam-build-stage-XYZ/ ./rootfs.ext4 2G
   ```
   This constructs a pristine ext4 block device directly from the directory without needing loop devices, mount privileges, or kernel block layer interaction.

---

## 2. Privileged Loopback Backend (Opt-in)

The Privileged Loopback backend is an optional, high-fidelity compilation mode designed for dedicated, disposable build servers (e.g., GitHub Actions runners or cloud VM build pipelines).

### How it Works
1. **Device Allocation**: Allocates a raw sparse file and formats it using standard `mkfs.ext4` utilities.
2. **Loop Mount**: Mounts the raw file as a loopback block device (e.g., `/dev/loop0`) to a host mount point. This step requires administrative `CAP_SYS_ADMIN` privileges or running as `root`.
3. **Chroot Execution**: Steps are run inside a real Linux `chroot(2)` environment.
4. **Cleanup Guard**: Implements a strict `Drop` guard. Any interruption, panic, or user signal (e.g., `SIGINT`, `SIGTERM`) forces a recovery routine to unmount the loop device and detach it from `/dev/loopX` before exiting, preventing host resource contamination.

---

## 3. Nested MicroVM Backend (Future Spec)

A future extension of `vam-build` specifies a nested microVM builder backend.
* Instead of running commands in user namespaces or host chroots, the build executor launches a lightweight, throwaway Firecracker sandbox.
* The staging disk is attached directly as a secondary writable drive.
* The build manager sends commands over vsock to execute package installs inside the isolated guest OS itself.
* This achieves absolute virtualization isolation for builds but incurs a small startup latency overhead.

---

## Capability Detection & Safe Fallbacks

`vam-build` **never performs silent privilege escalations**. If a build is started, the `executor` probes the host kernel to detect available capabilities:

1. **User Namespace Support**: Checks `/proc/sys/user/max_user_namespaces`. If enabled, it initializes the Rootless Directory backend.
2. **Mount Privileges**: Probes for `CAP_SYS_ADMIN` or root UID. If missing and a privileged loop backend was requested, it **fails immediately with an explicit capability error** rather than attempting to escalate or silently continuing.

```rust
// Capability probe logic
pub fn detect_capabilities() -> SystemCapabilities {
    let has_namespaces = check_user_namespaces();
    let is_root = nix::unistd::Uid::effective().is_root();
    
    SystemCapabilities {
        user_namespaces: has_namespaces,
        loop_mount_allowed: is_root,
    }
}
```
