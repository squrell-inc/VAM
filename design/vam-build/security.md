---
title: Build Security Model
description: Hardening the build plane against untrusted recipes, package exploits, and credential leakage.
---

:::caution[Alpha Security Specification]
The security policies and boundaries documented here represent VAM's planned threat mitigations.
:::

## Threat Vector Analysis

Executing third-party `Vamfile` compilation tasks poses significant security liabilities, as the builder must run arbitrary shell scripts, fetch public packages, and format filesystem layers. To counter these threats, the VAM build security model protects the host, the worker node, and the produced guest artifact.

---

## 1. Isolating Untrusted Recipes

When executing a `RUN` instruction (e.g., executing `npm install` or running a compilation hook), the script could attempt to scan the local build network, read files on the host worker, or install backdoor services inside the rootfs.

```text
┌────────────────────────────────────────────────────────┐
│                   VAM Build Host                       │
├────────────────────────────────────────────────────────┤
│  [ Untrusted Vamfile compilation ]                     │
│    │                                                   │
│    └──► Restricted Sandbox (User Namespace)            │
│         │                                              │
│         ├──► [ Read Host Files ] ──X (Blocked)          │
│         └──► [ Local LAN Scan ]  ──X (Blocked)          │
└────────────────────────────────────────────────────────┘
```

* **Mitigation**: `RUN` instructions are strictly jailed inside restricted user namespaces (for the default rootless directory builder) or within a throwaway nested virtual machine sandbox.
* **Host Resource ceiling**: Strict RAM and vCPU ceilings are configured per build worker to prevent malicious scripts from launching Denial-of-Service (DoS) attacks on the build server.

---

## 2. Strict Egress Filtering

Build scripts often download dependencies from public repositories (PyPI, npm, Alpine Mirrors). A compromised package or a typosquatted library could attempt to phone home to a Command-and-Control (C2) server or leak build-time context.

* **Offline Compile Path**: High-security organizations are encouraged to run builds with the `--offline` flag, shutting down all virtual network adapters inside the compilation container or sandbox.
* **Egress Domain Whitelisting**: If networking is enabled, outbound packets are intercepted on the host and verified against an approved destination whitelist (e.g., restricted to specific domains like `registry.npmjs.org` or `dl-cdn.alpinelinux.org`). Any other TCP/UDP requests are silently dropped.

---

## 3. Ephemeral Secret Management

A common developer mistake is including sensitive credentials (e.g., `.npmrc` tokens, private keys, or SSH credentials) within environment variables (`ENV`) or files copied permanently into the image context.

* **No Credentials in Metadata**: VAM strictly prohibits storing credentials inside `ENV` commands. Environment values are stored in plaintext inside the OCI Image Manifest and are globally readable once published.
* **Ephemeral Mounts**: Credentials must be supplied via ephemeral, secure tmpfs secret mounts (`--mount=type=secret`). These secrets reside only in host RAM and are unmounted immediately after the requiring instruction completes, guaranteeing that no private tokens survive inside the ext4 layers.

---

## 4. Path Traversal & Symlink Defenses

Malicious recipes might use symbolic links inside the build context folder to trick the builder into copying sensitive host assets (e.g., `/etc/shadow`, or active `.kube/config` files) into the rootfs image.

* **Jailed Traversal Checker**: The `source` parser performs exhaustive path resolution checks. It resolves all symbolic links and prevents the compiler from reading, copying, or tracing any path that points outside the designated build context folder.
* **Absolute Path Restriction**: Any absolute paths defined as the target of a `COPY` instruction are mapped relative to the guest root directory, preventing overwrite attacks on the host worker.

---

## 5. Build Registry Isolation

To publish built OCI image bundles, the build worker must hold authentication credentials for central registries.

* **Credential Sandboxing**: Registry credentials (like AWS ECR tokens or GitHub Packages personal access tokens) are stored within the host worker's private keychain. 
* **Worker Boundary**: The isolated sandbox running the `RUN` commands has absolutely no access to the registry credentials, ensuring compromised build packages cannot exfiltrate host registry keys.
