---
title: Error Handling
description: Detailed typed build error representations, error codes, and recovery mechanisms in vam-build.
---

:::note[Implementation Status]
All errors listed on this page represent the strongly typed error catalog in `crates/vam-build/src/error.rs`.
:::

## Typed Build Errors

To guarantee robustness, predictability, and ease of debugging, the `vam-build` crate avoids generic string-based errors. Instead, it exposes a single, strongly-typed Rust `enum` using the `thiserror` library: `VamBuildError`.

```rust
// crates/vam-build/src/error.rs

use thiserror::Error;

#[derive(Error, Debug)]
pub enum VamBuildError {
    #[error("Parse error at line {line}: {reason}")]
    ParseError { line: usize, reason: String },

    #[error("Source resolution failed for '{source}': {reason}")]
    SourceResolutionFailed { source: String, reason: String },

    #[error("Inadequate host capabilities: {missing_cap}")]
    MissingHostCapability { missing_cap: String },

    #[error("Execution of build step '{step}' failed with exit code {exit_code}")]
    ExecutionFailed { step: String, exit_code: i32, stderr: String },

    #[error("RootFS generation error: {reason}")]
    RootFsError { reason: String },

    #[error("Guest compatibility check failed: {issue}")]
    CompatibilityViolation { issue: String },

    #[error("Image optimization failed during step '{stage}': {reason}")]
    OptimizationError { stage: String, reason: String },

    #[error("Metadata creation failure (SBOM/Provenance): {reason}")]
    MetadataGenerationFailed { reason: String },

    #[error("OCI Registry transport error: {reason}")]
    RegistryError { reason: String },

    #[error("Cryptographic signing or verification failed: {reason}")]
    SignatureFailure { reason: String },

    #[error("Admission policy rejected artifact: {reason}")]
    AdmissionRejected { reason: String },

    #[error("Cleanup operation failed for path '{path}': {reason}")]
    CleanupFailed { path: String, reason: String },
}
```

---

## Complete Error Classification

| Error Name | Cause | Retryable? | Description |
|---|---|---|---|
| `ParseError` | Syntax violation, bad directives, unclosed quotes in `Vamfile` | No | Syntax checking detects these during planning before any commands run. |
| `SourceResolutionFailed` | Unable to fetch base image, bad digest, DNS failure | Yes (network) | Triggered when remote packages or bases are unreachable or verify incorrectly. |
| `MissingHostCapability` | Running privileged loops without root, or missing `mke2fs` | No | The host lacks required tool binaries or namespaces. |
| `ExecutionFailed` | A `RUN` directive returned a non-zero exit code | No | Occurs when a command inside the guest staging directory fails. |
| `RootFsError` | Out of space, corruption, filesystem generation timeout | No | Problems initializing or writing the ext4 block device. |
| `CompatibilityViolation` | Missing `vam-runtime`, unsupported arch, missing FUSE paths | No | The output rootfs failed the mandatory guest compatibility contract tests. |
| `OptimizationError` | `resize2fs` failed, or free space block zeroing failed | No | Occurs when compressing, shrinking, or sparsifying the block device. |
| `MetadataGenerationFailed` | Unable to generate CycloneDX SBOM or build provenance logs | No | Failures writing build-time supply chain certificates. |
| `RegistryError` | ECN, ECR, Docker Hub down, credentials expired | Yes (network) | Network or authentication failures while talking to an OCI registry. |
| `SignatureFailure` | Bad Cosign keys, expired certificate, failed KMS validation | No | Cryptographic signature generation or verification failure. |
| `AdmissionRejected` | Signature mismatch, unauthorized base, outdated digest | No | The orchestrator `vamd` refused to boot the image due to security checks. |
| `CleanupFailed` | Leftover loop mounts, locked mount points, permission errors | Yes (forced) | Inability to clean up staging directories or mount tables. |

---

## Stable CLI Mappings & Exit Codes

When executed as a command-line interface tool, the typed error categories map directly to predictable, structured POSIX exit codes:

```text
0   = Build successful
1   = Unknown/generic failure
10  = Parse or syntax error in Vamfile
20  = Source resolution or registry pull failure
30  = Execution of RUN instruction failed
40  = RootFS formatting or writing failed
50  = Compatibility contract check failed
60  = Metadata/SBOM generation failed
70  = Cryptographic signing or policy admission failure
80  = System capability or resource limit reached
```

---

## Recovery & Cleanup Guarantees

If a build fails mid-execution, VAM guarantees complete host node hygiene:
* **Strict Drop Guards**: Internal execution structures implement Rust's `Drop` trait. Staging spaces, network TAP/namespaces, and loop devices are automatically unmounted and cleaned up if the process panics or returns an error.
* **No Orphaned Loops**: When the `privileged` backend runs, any failure forces a check and unmount of the specific loopback ID before exiting.
* **Partial Cache Retention**: Successful stages leading up to the failed stage remain safely cached in the host's content-addressed cache, ensuring retry attempts do not duplicate work.
