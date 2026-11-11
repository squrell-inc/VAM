---
title: Vamfile Language
description: Syntax versioning, lexical rules, variable expansion, and structure of VAM build recipes.
status: planned
---

## Overview

A `Vamfile` is a declarative, versioned recipe file used by `vam-build` to compile a customized guest operating system block device (`rootfs.ext4`). Conceptually similar to a `Dockerfile`, a `Vamfile` is optimized specifically for generating minimal ext4 filesystems rather than container layers.

---

## Syntax Versioning & Lexical Rules

Every `Vamfile` must begin with a `# syntax=` comment to declare the parser version compatibility. This ensures that future syntax additions do not break existing build configurations.

### Lexical Conventions
* **Case Sensitivity**: Directives (e.g., `FROM`, `RUN`, `COPY`) are case-sensitive and must be written in uppercase.
* **Line Continuation**: Backslashes (`\`) are used to continue a directive across multiple lines.
* **Comments**: Any line starting with `#` (that is not a syntax/parser directive) is treated as a comment and ignored during compilation.
* **Whitespace**: Leading and trailing whitespace around directives and arguments is trimmed.

```text
# syntax=vam/v1-alpha

# This is a comment.
FROM alpine:3.20@sha256:beef...

# Line continuation example
RUN apk add --no-cache \
    python3 \
    py3-pip
```

---

## Abstract Syntax Tree (AST) & Stages

The `recipe` module parses a `Vamfile` into a structured Abstract Syntax Tree (AST). Internally, this representation maps to an execution graph made of one or more **stages**.

```rust
pub struct RecipeAst {
    pub syntax_version: String,
    pub stages: Vec<BuildStage>,
}

pub struct BuildStage {
    pub name: String,
    pub base: BaseSource,
    pub instructions: Vec<Instruction>,
}
```

### Multi-Stage Builds
A `Vamfile` supports multi-stage compilation using `AS` aliases. This enables a pattern where compilers and build dependencies are isolated to an early, ephemeral stage, while the final stage only copies the compiled binaries, minimizing final block device sizes.

```text
# syntax=vam/v1-alpha

# Stage 0: Build environment
FROM rust:1.80-alpine AS compiler
RUN apk add --no-cache musl-dev
COPY ./src /src
RUN cd /src && cargo build --release

# Stage 1: Runtime environment
FROM alpine:3.20@sha256:beef...
COPY --from=compiler /src/target/release/my-agent /usr/local/bin/my-agent
ENTRYPOINT ["/usr/local/bin/my-agent"]
```

---

## Variable Expansion

The language supports environment variable expansion using shell-like `${VAR}` syntax. Variables are populated from:
1. `ENV` instructions defined within the file.
2. Build-time argument overrides passed via the command line (`--build-arg`).

### Expansion Rules
* **Format**: Both `$VAR` and `${VAR}` formats are recognized.
* **Default Values**: Standard shell fallback syntax is supported (e.g., `${MY_VAR:-default_value}`).
* **Strict Mode**: If a variable is referenced but not declared (and has no default value), the parser will throw an evaluation error during the planning stage, failing the build immediately before execution begins.

```text
ENV APP_VERSION="1.0.0"
ENV DATA_DIR="/opt/app"

# Resolves to: RUN mkdir -p /opt/app/1.0.0
RUN mkdir -p ${DATA_DIR}/${APP_VERSION}
```

---

## Language Compatibility & Deprecation Policy

VAM maintains a strict compatibility policy to protect enterprise builds:
* **Backward Compatibility**: A parser supporting `vam/v1` will compile any valid `v1-alpha` file, though it may output deprecation warnings.
* **Breaking Changes**: Any breaking syntax changes require a major version bump in the syntax declaration comment (e.g., `# syntax=vam/v2`). The `recipe` parser will automatically swap its internal parser engine based on this header.
* **Explicit Failure**: If a `Vamfile` omits the `# syntax` declaration, the compiler operates in strict legacy fallback mode and emits a warning. For production/release builds, the syntax header is **mandatory**; omission is a policy failure.
