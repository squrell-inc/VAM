---
title: Testing Strategy
description: Test suites, golden parser verification, fault injection, and automated boot checks for the build pipeline.
---

:::note[Implementation Status]
The testing methodologies outlined here represent the testing plan designed for the `vam-build` crate's unit and integration tests.
:::

## Multi-Layer Test Architecture

To ensure the safety, predictability, and correctness of the image build pipeline, `vam-build` implements a multi-layer test strategy. This strategy validates every stage of compilation from the initial recipe parse to the final virtual machine boot check.

```text
┌────────────────────────────────────────────────────────┐
│ 1. Unit Tests                                          │
│    - Lexer/Parser golden tests                         │
│    - Variable expansion correctness                    │
│    - Cache key calculation validity                    │
└──────────────────────────┬─────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────┐
│ 2. Integration Tests                                   │
│    - Rootless vs. Privileged filesystem comparison     │
│    - Duplicate build reproducible comparison           │
│    - Secret non-leakage verification scans             │
└──────────────────────────┬─────────────────────────────┘
                             │
                             ▼
┌────────────────────────────────────────────────────────┐
│ 3. System E2E Tests                                    │
│    - Firecracker boot smoke tests with vam-runtime     │
│    - Tampered image signature rejection tests          │
│    - OCI registry push/pull compatibility              │
└────────────────────────────────────────────────────────┘
```

---

## 1. Unit Testing: Golden Parsers & Fixtures

The `recipe` module uses golden file tests (or snapshot tests) to verify syntax correctness and prevent parser regressions:

* **Golden Parsers**: A collection of valid and invalid `Vamfile` syntax fixtures is maintained. The test suite parses each file and asserts that the resulting AST or error string matches an approved snapshot file.
* **Variable Expansion Correctness**: Asserts that variable substitution behaves identically to Bash specifications, verifying fallback defaults (`${VAR:-default}`) and strict-mode validation errors.
* **Cache Key Integrity**: Validates that modifying any single character in a `COPY` file, `RUN` directive, or `ENV` variable immediately cascades into a distinct step cache key.

---

## 2. Integration Testing: Parity, Seeding, and Leaks

Integration tests verify filesystem operations and safety constraints:

* **Rootless vs. Privileged Parity**: Compiles a standard recipe twice—once using the Rootless Directory backend and once using the Privileged Loopback backend. The tests inspect both output rootfs staging directories to verify that ownership, file permissions, and directory structures are identical.
* **Reproducibility Checks**: Executes the double-build test suite. It verifies that compiles of the same recipe context yield bit-for-bit identical raw `.ext4` files with matching SHA-256 digests.
* **Secret Non-Leakage Scans**: Deliberately injects mock high-entropy tokens via secure secret mounts, compiles the image, and then performs string searches across the raw block device sectors. The test asserts that the secret token is **completely undetectable** inside the final filesystem blocks.

---

## 3. System E2E Testing: Boots and Tampers

System-level end-to-end tests require a Linux host with KVM/VSock support to verify host-guest boundaries:

* **Firecracker Boot Smoke Tests**: Spins up a Firecracker microVM using the compiled `rootfs.ext4` image. The test suite asserts that the guest kernel boots, that `vam-runtime` successfully executes as PID 1, and that the VSock RPC channel establishes a connection to the mock host control plane.
* **Tamper Rejection Tests**: Deliberately alters a single byte of a compiled, signed `.ext4` file using `dd` and attempts to boot it. The test asserts that the orchestrator's admission controller detects the signature mismatch and cancels the boot immediately.
* **Registry Matrix Compatibility**: Runs automated ORAS push/pull routines against standard container registries (AWS ECR, Docker Hub, GCP Artifact Registry) to verify OCI manifest compliance and Referrer link querying.
