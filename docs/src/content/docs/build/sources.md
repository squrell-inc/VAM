---
title: Sources & Build Context
description: Handling base images, local contexts, digest pinning, and network egress rules during builds.
status: planned
---

## Resolving Base Images

When a build begins, the `source` module evaluates the base operating system declared in the `FROM` directive. It supports two primary formats:

1. **OCI Reference**: A standard container image reference pointing to an OS root filesystem (e.g., `alpine:3.20` or `redhat/ubi9:latest`).
2. **Raw Tarball**: A local or remote compressed rootfs archive (e.g., `https://distfiles.gentoo.org/releases/.../stage3.tar.xz`).

### Digest Pinning
To prevent "invisible drift" where an upstream vendor updates a tag (e.g., `latest` or `3.20`) and breaks guest builds, VAM strictly enforces **digest pinning** in release mode.

* **Developer Mode**: If only a tag is provided, `vam-build` queries the remote registry, resolves the tag to its immutable SHA-256 digest, and updates the local plan.
* **Release Mode**: The build fails immediately if any base image or external resource is referenced by tag rather than a SHA-256 digest.

```text
# REJECTED IN RELEASE MODE:
FROM alpine:3.20

# REQUIRED IN RELEASE MODE:
FROM alpine:3.20@sha256:026e63c0a5b18f62...
```

---

## Build Context & `.vamignore`

The build context is the directory containing the files and folders that can be copied into the rootfs image using the `COPY` directive.

### File Traversal Security
To protect the host node, the `source` and `recipe` modules enforce strict traversal limits:
* **Chroot-like Jail**: The build context is treated as a jail. Any paths, symlinks, or hardlinks in the context that point to files outside the context root directory are ignored or trigger a safety parse error.
* **Symlink Resolution**: Symlinks that target files inside the context are evaluated, while symlinks targeting files outside (e.g., `/etc/resolv.conf` or `~/.ssh/id_rsa`) are safely dereferenced to empty files or blocked.

### The `.vamignore` File
Similar to `.gitignore`, a `.vamignore` file at the root of the build context excludes specified patterns from being packaged into the compilation context. This keeps the build staging tree lightweight and prevents sensitive files from being accidentally copied.

```text
# .vamignore
node_modules/
target/
.git/
.env*
*.pem
*.key
```

---

## Egress Fetch Policies

By default, executing arbitrary package installer commands (like `apk add` or `apt-get install`) during a build is a security liability. It risks communicating with compromised mirrors or exfiltrating build-time assets.

To mitigate this, VAM specifies a **strict deny-by-default network policy** inside the staging sandbox:

```text
┌────────────────────────────────────────────────────────┐
│                   VAM Build Sandbox                    │
├────────────────────────────────────────────────────────┤
│                                                        │
│  [ RUN apk add python3 ] ──► (Blocked by default)      │
│                                                        │
│  [ Allowed Repositories ] ──► (Permitted via whitelist) │
│    - alpine.org/mirrors                                │
│    - pypi.org/packages                                 │
└────────────────────────────────────────────────────────┘
```

1. **Offline Mode (`--offline`)**: The network is completely disabled. Package downloads are disallowed; all dependencies must be pre-fetched into the local build context and copied in.
2. **Egress Whitelisting**: If online compilation is permitted, the build agent restricts outbound packets to a strict domain and IP list (e.g., standard OS package mirror domains). Any outbound traffic to arbitrary domains is dropped.

---

## Licensing Metadata

VAM images must be compliant with corporate software asset policies. The `metadata` module automatically parses and inventories package license data during compilation:
* **Distro Licensing**: The build output manifest includes explicit tags declaring the base OS and licensing boundary (e.g., "Alpine Linux (MIT)", "Red Hat UBI (EULA)").
* **Third-Party Disclaimers**: If copyleft or entitlement-protected software is introduced during a build, the compiler auto-generates a `/usr/share/doc/vam-licenses.txt` file listing all legal disclaimers.
