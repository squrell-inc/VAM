---
title: Vamfile Directive Reference
description: Semantic rules, validation, security mounts, and constraints of Vamfile directives.
status: planned
---

## Overview of Directives

A `Vamfile` is composed of sequential directives that define how to build the root filesystem. Every directive is validated at parse time against a strict syntax schema and evaluated under a specific execution posture.

---

## Detailed Directive Reference

### `FROM`
Declares the base operating system rootfs image or stage.
* **Syntax**: `FROM <image_name>[:<tag>][@sha256:<digest>] [AS <alias>]`
* **Validation**:
  * In **release mode**, a SHA-256 digest is **mandatory**. Tag-only bases are strictly rejected to ensure build reproducibility.
  * In developer mode, tag-only references are resolved to a digest by querying the registry, and a warning is emitted.
* **Example**:
  ```text
  FROM alpine:3.20@sha256:026e63c0a5b18f62...
  ```

### `RUN`
Executes shell commands inside the temporary rootfs staging tree.
* **Syntax**: `RUN <command>` (shell form) or `RUN ["executable", "arg1", "arg2"]` (exec form)
* **Validation**:
  * Requires an active, isolated builder backend (user namespace or loopback sandbox).
  * Executed commands are run under a restricted environment with **no host networking access** unless an egress allowlist is matched (see [Sources & Context](/build/sources/)).

### `COPY`
Copies files and directories from the local build context or an upstream build stage into the rootfs staging tree.
* **Syntax**: `COPY [--from=<stage>] <src>... <dest>`
* **Validation**:
  * `<src>` must reside strictly within the declared build context folder.
  * Any attempt to use absolute paths outside the context or relative paths that resolve above the context root (e.g., `../etc/passwd`) results in an immediate security parse failure.
  * Preserves original file permissions (modes) and maps UIDs/GIDs to a normalized mapping.

### `ENV`
Sets persistent environment variables within the rootfs metadata. These are stored in the final OCI image manifest.
* **Syntax**: `ENV <key>="<value>"` or `ENV <key>=<value>`
* **CRITICAL SECURITY RULE**: `ENV` instructions **must never** contain passwords, private keys, API keys, or any other sensitive material. These variables are written in plain text to the public OCI registry metadata. For build-time secrets, use [Secret Mounts](#secret-mounts).

### `SECURITY_POLICY`
Enforces declarative compliance rules on the guest Operating System during build time.
* **Syntax**: `SECURITY_POLICY <policy_profile>`
* **Example**:
  ```text
  SECURITY_POLICY "cis-level-1"
  ```
* **Validation**: Compiles a set of validation checks (e.g., verifying that `/etc/shadow` has safe permissions, verifying password complexity constraints, and disabling unused network protocols).

### `ENTRYPOINT`
Specifies the default executable to run when the virtual machine boots.
* **Syntax**: `ENTRYPOINT ["/path/to/binary", "arg1"]`
* **Validation**:
  * Must be a valid executable file inside the rootfs staging tree.
  * In VAM, the entrypoint must resolve to `/usr/local/bin/vam-runtime` or a validated wrapper shell script that ultimately executes (via `exec`) `vam-runtime`.

---

## Secret Mounts

To safely pass credentials (e.g., Artifactory tokens, GitHub SSH keys, or private package repository credentials) to `RUN` instructions without baking them into the final image or build logs, VAM provides ephemeral **secret mounts**.

* **Usage**: Ephemeral mounts are requested directly on the `RUN` directive.
* **Syntax**:
  ```text
  RUN --mount=type=secret,id=npm_token \
      npm config set //registry.npmjs.org/:_authToken $(cat /run/secrets/npm_token) && \
      npm install && \
      rm -f ~/.npmrc
  ```
* **Engine Isolation**:
  * The compiler mounts the secret file as a read-only tmpfs volume at `/run/secrets/<id>` for the duration of that single `RUN` command.
  * As soon as the command completes, the tmpfs volume is torn down.
  * The secret content is never written to disk, never cached, and never exported in any layer of the OCI artifact.

---

## Release-Mode Constraints

When `vam-build` is invoked with `--release`, the parser enables strict mode to enforce absolute reproducibility:
1. **No Tags**: All `FROM` instructions must be digest-pinned.
2. **Deterministic Inputs**: The compiler verifies that all copied local files have matching SHA-256 signatures declared in a lockfile, or it fails.
3. **Immutable CAs**: All custom CA certificates copied during the build must be validated against their fingerprint files.
