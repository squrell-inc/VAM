---
title: Template Catalog
description: Standard declarative recipes and base operating systems for VAM virtual machine guests.
status: planned
---

## Pre-Configured Templates

VAM provides a catalog of standard base images. These templates serve as starting points for developers, offering distinct OS properties, package ecosystems, security levels, and licensing bounds.

```text
┌────────────────────────────────────────────────────────┐
│                    Base Image Catalog                  │
├────────────────────────────────────────────────────────┤
│                                                        │
│  [ Vamfile.alpine ] ────────► Minimal & Dev (~15MB)    │
│  [ Vamfile.ubuntu-hardened ] ─► Production & Standard  │
│  [ Vamfile.ubi9-enterprise ] ─► Free Enterprise Base   │
│  [ Vamfile.rhel-byos ] ─────► Private Subscription Full│
│                                                        │
└────────────────────────────────────────────────────────┘
```

---

## 1. `Vamfile.alpine` (Minimal/Dev)

* **Purpose**: Designed for rapid development and testing. It features near-instant cold-boots and extremely small image sizes.
* **Size**: ~15MB compressed rootfs.
* **Distribution Rule**: Publicly redistributable without restriction under standard open-source licenses.
* **Sample Recipe**:
  ```text
  # syntax=vam/v1-alpha
  FROM alpine:3.20@sha256:026e63c0a5b18f62...
  RUN apk add --no-cache python3 nodejs
  ENTRYPOINT ["/usr/local/bin/vam-runtime"]
  ```

---

## 2. `Vamfile.ubuntu-hardened` (Standard Production)

* **Purpose**: Designed for enterprise production environments where tools require standard glibc runtime support (such as pre-compiled Python libraries or specialized binary wheels).
* **Hardening Profile**: Applies standard CIS Benchmark rules (such as disabling unused network protocols, disabling root SSH login, and setting defensive file permissions).
* **Size**: ~120MB compressed rootfs.
* **Distribution Rule**: Publicly redistributable.
* **Sample Recipe**:
  ```text
  # syntax=vam/v1-alpha
  FROM ubuntu:24.04@sha256:722978484...
  SECURITY_POLICY "cis-level-1"
  RUN apt-get update && apt-get install -y --no-install-recommends \
      ca-certificates \
      python3-minimal
  ENTRYPOINT ["/usr/local/bin/vam-runtime"]
  ```

---

## 3. `Vamfile.ubi9-enterprise` (Compliant Enterprise Baseline)

* **Purpose**: Provides a Red Hat Universal Base Image 9. Perfect for enterprise organizations requiring a secure, audited OS baseline with active vendor vulnerability patching.
* **Licensing Compliance**: Under the UBI EULA, Rocky Linux / AlmaLinux / Red Hat UBI base operating system packages are **freely redistributable** without active subscriptions or entitlements.
* **Size**: ~95MB compressed rootfs.
* **Distribution Rule**: Publicly redistributable.
* **Sample Recipe**:
  ```text
  # syntax=vam/v1-alpha
  FROM redhat/ubi9-minimal@sha256:abc3245...
  RUN microdnf update -y && \
      microdnf install -y python3-pip && \
      microdnf clean all
  ENTRYPOINT ["/usr/local/bin/vam-runtime"]
  ```

---

## 4. `Vamfile.rhel-byos` (Private Subscription Full)

* **Purpose**: Enables organizations with active Red Hat corporate agreements to bring their own fully entitled RHEL base images, including proprietary security and management agents.
* **Licensing Compliance**: Strictly restricted. Images compiled using this template **cannot be distributed publicly** due to RHEL BYOS (Bring Your Own Subscription) boundaries. They must be stored in private organization-controlled OCI registries.
* **Size**: ~180MB compressed rootfs.
* **Distribution Rule**: Private registries only. Never represented as freely redistributable.
* **Sample Recipe**:
  ```text
  # syntax=vam/v1-alpha
  # Requires access to private organization registry with subscription entitlements
  FROM private-registry.internal/rhel/rhel-byos:9.4@sha256:ef3249bc...
  RUN --mount=type=secret,id=rhel_entitlement \
      subscription-manager register --org="OrgID" --activationkey="Key" && \
      dnf install -y security-agent && \
      subscription-manager unregister
  ENTRYPOINT ["/usr/local/bin/vam-runtime"]
  ```
