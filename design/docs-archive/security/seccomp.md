> Moved from the docs site on 2026-10-30: design intent, not built.

---
title: Seccomp BPF Syscall Filtering
description: Restricting kernel system calls for both the host Firecracker VMM process and guest tool execution environments.
---

:::caution[Alpha Stability Warning]
VAM security features and subsystems (under the `vam-security` crate) are currently in **Alpha**. The security model, cryptography protocols, and policy integrations are actively evolving, breaking changes are expected, and no production-readiness guarantees are provided at this stage. Do not deploy in untrusted production environments without custom hardening.
:::

Secure virtualization requires locking down the system call interface between user space processes and the Linux kernel. If an attacker gains arbitrary code execution, they will attempt to exploit kernel vulnerabilities to escalate privileges. 

To prevent this, VAM implements **two distinct tiers** of Seccomp BPF (Berkeley Packet Filter) profiles, generated and managed via `vam-security`.

---

## 1. The Two-Tier Filtering Architecture (Decision S10)

VAM separates syscall filtering into two completely distinct isolation boundaries:

```text
  [ Host Physical Node ]
  ┌────────────────────────────────────────────────────────┐
  │ Host Kernel (Protected by Tier 1 Seccomp BPF)          │
  │                                                        │
  │    ┌──────────────────────┐                            │
  │    │ Firecracker VMM      │ (AWS Jailer applies        │
  │    │ (Host Process)       │  Tier 1 Filter)            │
  │    └──────────┬───────────┘                            │
  └───────────────┼────────────────────────────────────────┘
                  │ Hypervisor Boundary
  [ Guest Isolated MicroVM ]
  ┌───────────────┼────────────────────────────────────────┐
  │ Guest Kernel  ▼ (Protected by Tier 2 Seccomp BPF)      │
  │                                                        │
  │    ┌──────────────────────┐                            │
  │    │ Guest Tool Process   │ (Guest `vam-runtime`       │
  │    │ (e.g. Bun, Python)   │  applies Tier 2 Filter)    │
  │    └──────────────────────┘                            │
  └────────────────────────────────────────────────────────┘
```

### Tier 1: Host VMM Protection Profile
* **Target Process**: The Firecracker VMM process on the **host**.
* **Protected Kernel**: The **host's** Linux kernel.
* **Goal**: Prevent a guest that has broken out of guest memory from abusing the host kernel.
* **Mechanism**: Handled by AWS Jailer. The `vam-security` engine generates the binary BPF profile which is passed to Jailer on instance startup. See [AWS Jailer Sandbox](/core/jailer/) for details.

### Tier 2: Guest Tool Subprocess Profile
* **Target Process**: Third-party tool processes (e.g., executing Python, Bash, or Node.js scripts) inside the **guest**.
* **Protected Kernel**: The **guest's** Linux kernel.
* **Goal**: Prevent unverified agent tool dependencies from exploiting the guest kernel to gain root, escape namespaces, or corrupt the `vam-runtime` memory space.
* **Mechanism**: Applied by the guest `vam-runtime` before spawning tool subprocesses.

---

## 2. Tier 1: Firecracker VMM Allowlist

Because Firecracker has a highly specialized, minimalist device model, it requires an extremely narrow list of host system calls. The standard Firecracker VMM profile allows fewer than **40 system calls** out of the 450+ available in modern Linux.

The `vam-security` BPF generator compiles a profile containing the following essential host-side syscalls:

| Category | Syscalls | Purpose |
|---|---|---|
| **Memory** | `mmap`, `mprotect`, `munmap`, `madvise`, `brk` | Allocating guest RAM and managing virtual memory pools. |
| **I/O** | `read`, `write`, `pread64`, `pwrite64`, `epoll_wait` | Interacting with Drive9 disk blocks and handling vsock frames. |
| **Threads** | `futex`, `clone`, `rt_sigreturn`, `sigaltstack` | Managing multi-threaded virtual CPU contexts. |
| **Lifecycle** | `exit`, `exit_group`, `ioctl` | Instantiating KVM physical hardware parameters and exiting. |

Any system call outside this list (e.g., `fork`, `execve`, `ptrace`, `syslog`) is immediately blocked by the host kernel.

---

## 3. Tier 2: Guest Tool Allowlist

Unlike the host VMM, guest tool processes (like Python interpreters or JavaScript compilers) require wider capabilities to run code, parse files, and manage sub-environments. However, they must still be blocked from executing dangerous administrative system calls.

The `vam-security` engine compiles a broader but restricted Tier 2 profile for guest-side subprocess spawning:

* **Allowed**: Standard filesystem access, thread synchronization, subprocess control (`execve`, `fork`, `pipe`, `wait4`), dynamic memory expansion, and local loopback sockets.
* **Blocked**: Syscalls modifying network routing, mounting physical filesystems, triggering reboots (`reboot`), reloading kernel modules (`init_module`), or tracing other system processes (`ptrace`).

---

## 4. Syscall Action Tiers (Dev vs. Prod)

Developing seccomp profiles is highly iterative. If a developer runs a new tool dependency that makes an un-allowlisted syscall, the default action is process termination, which makes debugging extremely difficult.

To solve this, `vam-security` compiles filters in two distinct modes:

```rust
// status: Planned
pub enum SeccompAction {
    /// Dev Mode: Logs blocked syscalls to syslog/stderr but allows execution to continue.
    /// Maps to kernel `SCMP_ACT_LOG`.
    Log,
    
    /// Prod Mode: Instantly kills the process thread making the disallowed syscall.
    /// Maps to kernel `SCMP_ACT_KILL` or `SCMP_ACT_ERRNO(EPERM)`.
    Kill,
}
```

:::danger[Production Safety]
Dev mode (`SeccompAction::Log`) is strictly forbidden in production. Any container escape payload could run successfully while merely writing a diagnostic warning to log files. Production configurations must enforce `SeccompAction::Kill`.
:::

---

## 5. Next Steps

* To understand how AWS Jailer applies Tier 1 Seccomp profiles on the host, read [AWS Jailer Sandbox Launcher](/core/jailer/).
* To see how security boundaries are defined across the entire platform, see [Host Defense-in-Depth Ladder](/core/security-model/).
* Review the full list of security rules in [Security Invariants](/security/invariants/).
