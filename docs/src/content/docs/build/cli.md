---
title: The `vam build` Command Line Interface
description: Syntax and usage of the vam-build CLI frontend, commands, options, and behaviors.
status: planned
---

## Standalone CLI Architecture

While the `vam-build` crate is designed as a reusable Rust library, it exposes a standalone command-line executable (e.g., `vam-build` or `vam build` in the CLI wrapper) to orchestrate the image-making pipeline. 

```text
┌────────────────────────────────────────────────────────┐
│                        User CLI                        │
└───────────────────────────┬────────────────────────────┘
                            │ (CLI Commands)
                            ▼
┌────────────────────────────────────────────────────────┐
│                   `vam-build` Crate                    │
├────────────────────────────────────────────────────────┤
│  recipe  -> source  -> planner -> executor -> rootfs   │
└───────────────────────────┬────────────────────────────┘
                            │ (Library API Calls)
                            ▼
┌────────────────────────────────────────────────────────┐
│                 Output disk/OCI artifact               │
└────────────────────────────────────────────────────────┘
```

---

## Core Subcommands

### `vam-build lint`
Analyzes a `Vamfile` for syntax errors, deprecated instructions, security policy violations, or unpinned base images.
* **Usage**: `vam-build lint -f Vamfile`
* **Output**: Linting warnings, errors, and an exit code indicating compliance.

### `vam-build plan`
Compiles the `Vamfile` syntax tree, resolves base images and files into exact content-addressed digests, and outputs a execution DAG (Directed Acyclic Graph) showing which steps will be pulled from cache vs. executed.
* **Usage**: `vam-build plan -f Vamfile --context .`
* **Output**: A JSON or structured text graph of compile steps.

### `vam-build build`
Executes the planning tree, compiles stages, structures the staging rootfs, formats the ext4 disk image, and packages the result.
* **Usage**:
  ```bash
  vam-build build \
    --file Vamfile \
    --context . \
    --output ./rootfs.ext4 \
    --release
  ```
* **Key Flags**:
  * `--offline`: Completely disables network adapters in the build stage. All bases and dependencies must be locally provided.
  * `--build-arg KEY=VALUE`: Sets or overrides environment variables referenced inside the recipe.
  * `--release`: Enforces strict reproducibility checks and signature verifications.

### `vam-build inspect`
Inspects an existing `rootfs.ext4` or OCI build artifact and prints package inventories, metadata, kernel requirements, and signature statuses.
* **Usage**: `vam-build inspect ./rootfs.ext4`

### `vam-build verify`
Runs a local smoke test suite against the built image, spinning up a minimal Firecracker instance using the default kernel profile to verify basic guest init responsiveness and `vam-runtime` handshakes.
* **Usage**: `vam-build verify ./rootfs.ext4 --kernel ./vmlinux`

---

## Configuration Precedence

When orchestrating a build, configuration values are resolved using a strict precedence order (from highest priority to lowest):

1. **Explicit CLI Flags** (e.g., `--file`, `--output`, `--offline`).
2. **Environment Variables** (e.g., `VAM_BUILD_RELEASE=true`).
3. **Local Workspace Configuration** (`vam.toml` or `build.toml`).
4. **System Defaults** (defined in compile-time crate structures).

---

## Progress Reporting & Event Streams

To power modern terminal UIs and automated CI pipelines, `vam-build` provides structured, real-time logging and progress reporting.
* **Human-Readable Output**: Standard terminal output features clean progress bars, step-by-step logs, and colored status indicators.
* **Machine-Readable JSON Output**: Passing the `--log-format=json` flag redirects output into newline-delimited JSON streams, permitting telemetry collectors to parse step durations, active cache hits, and errors in real time.

```json
{"time":"2026-08-10T10:14:02Z","level":"info","step":"executing_run","index":3,"text":"apk add python3"}
{"time":"2026-08-10T10:14:15Z","level":"info","step":"step_complete","index":3,"duration_ms":13000}
```
