---
title: Build Subsystem Invariants
description: Non-negotiable technical bounds, security constraints, and operational invariants for VAM image building.
---

## System Invariants Overview

The VAM build pipeline is bound by strict, non-negotiable architectural and security principles. These invariants protect the integrity of the host, the confidentiality of organizational data, and the stability of the virtualized runtime.

Every module, CLI command, and library API in the `vam-build` crate must conform to these rules.

---

## 1. Input Immutability Invariant
* **Rule**: In release mode (`--release`), all external base images, packages, or assets **must be resolved to an immutable SHA-256 digest**.
* **Reasoning**: Tag-based references (e.g., `alpine:latest` or `node:18`) are mutable and introduce hidden drift, which compromises reproducibility and makes forensic audits impossible.

---

## 2. Builder Isolation Invariant
* **Rule**: Compilation steps (`RUN` instructions) **must never run directly on production virtualization hosts**. They must run on dedicated build workers inside isolated sandboxes.
* **Reasoning**: Package installations execute arbitrary post-install hook scripts. Jailing these executions prevents rogue scripts from scanning host networks, reading host files, or compromising the node hypervisor.

---

## 3. Unprivileged Default Invariant
* **Rule**: The builder **must default to the rootless directory executor**. Any switch to loopback or chroot interfaces requires explicit opt-in and is subject to immediate capability checks.
* **Reasoning**: Demanding root privileges or loop mounts for standard builds creates security holes and prevents running builds on unprivileged CI runners.

---

## 4. Rigid Compatibility Contract
* **Rule**: An image is **boot-incompatible if it lacks a verified static `vam-runtime` binary or FUSE 3 device layouts**.
* **Reasoning**: The host orchestrator relies on VSock port 52 to stream stdin/stdout/stderr and control frames. Booting an image without a compatible agent causes system-wide deadlocks and resource leaks.

---

## 5. Absolute Reproducibility Invariant
* **Rule**: Under identical inputs, the output `rootfs.ext4` **must be bit-for-bit identical**.
* **Reasoning**: Small variations in filesystem metadata (such as timestamps, inode orders, or UUIDs) break image digest verification, preventing signature caching and triggering policy admission rejections.

---

## 6. Immutable Secret Invariant
* **Rule**: Credentials **must never be stored in image environment variables (`ENV`) or persistent layers**.
* **Reasoning**: Environment variables are written in plaintext inside public OCI registry configs. Private keys or tokens must be mounted ephemerally using `--mount=type=secret` to ensure they never touch persistent disk blocks.

---

## 7. Fail-Closed Admission Invariant
* **Rule**: The orchestrator (`vamd`) **must reject any image that lacks a trusted signature, fails checksum validation, or triggers a scan policy violation**.
* **Reasoning**: A signature is a proof of origin. Accepting unverified or altered block devices bypasses the platform's supply-chain security boundary and invites host-level exploitation.

---

## 8. Licensing compliance Invariant
* **Rule**: RHEL images containing entitlement-restricted software **must never be published to public registries**. UBI-based images are permitted if they adhere to the Red Hat Universal Base Image EULA.
* **Reasoning**: Standard RHEL packages require active subscriptions. Violating these licensing limits exposes the platform and users to compliance liabilities.
