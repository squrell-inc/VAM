---
title: Enterprise Hardening
description: Securing the guest environment with CIS profiles, custom CA certificate injection, and private package mirrors.
---

:::caution[Alpha Specification]
The hardening profiles, compliance checkers, and CA injection systems represent active design specifications.
:::

## Enterprise Hardening Priorities

When virtualized agents are executed within enterprise networks, they must comply with corporate IT governance. A default open-source Linux rootfs is rarely sufficient.

VAM allows organizations to inject custom **hardening and compliance rules** during the build stage. These rules run deterministically and are evaluated inside the isolated staging environment.

```text
       VAM Base Image (e.g. standard Alpine/UBI)
                     │
                     ▼
┌──────────────────────────────────────────┐
│       Staging Hardening Operations       │
├──────────────────────────────────────────┤
│  - Inject corporate Root CA Certificates │
│  - Swap public mirrors for Artifactory   │
│  - Apply CIS hardening script profiles    │
│  - Strip development tooling/logs        │
└────────────────────┬─────────────────────┘
                     │
                     ▼
         Compliant Hardened RootFS
```

---

## 1. Injecting Internal CA Certificates

Enterprise developers often operate behind corporate firewalls or require agents to communicate with internal APIs secured by private Certificate Authorities (CAs).

* **Safe Injection Mechanics**: Public CA root certificates (e.g., `company-root.crt`) are safely copied into the staging tree's trusted certificate directories:
  * `/usr/local/share/ca-certificates/` (Debian/Ubuntu/Alpine)
  * `/etc/pki/ca-trust/source/anchors/` (Red Hat / UBI)
* **Trust Update Trigger**: The `injector` runs the distribution-specific update utility (such as `update-ca-certificates` or `update-ca-trust`) inside the staging tree.
* **CRITICAL SECURITY RULE**: **Never inject private keys** into the image. Only public CA certificates are permitted. Any private keys used for SSL handshakes must reside in host keyrings or be passed dynamically at runtime.

---

## 2. Diverting to Private Package Repositories

To avoid dependencies on public mirror servers (such as Alpine's public mirrors or standard Debian APT repositories), enterprise builds must redirect package queries to internal package management systems (like JFrog Artifactory, Sonatype Nexus, or private GCP Artifact Registry mirrors).

* **Mirror Redirection**: `Vamfile` directives can overwrite default system package configurations:
  * For Alpine, overwriting `/etc/apk/repositories`.
  * For Red Hat, setting custom `.repo` files in `/etc/yum.repos.d/`.
* **Access Tokens via Secrets**: Authentication passwords or tokens needed to fetch packages from secure internal repos are mounted using ephemeral build-time secret mounts (`--mount=type=secret`), guaranteeing that no corporate keys are baked into the raw image layer blocks.

---

## 3. CIS and OpenSCAP Hardening Profiles

VAM supports running declarative compliance checks inside the staging tree to align with CIS (Center for Internet Security) standards or custom organization profiles:

* **File System Sanitization**: Disables world-writable file permissions, enforces strict ownership on system paths (`/bin`, `/sbin`, `/etc`), and secures critical configuration files (such as `/etc/shadow` and `/etc/passwd`).
* **Kernel & Shell Restrictions**: Disables standard interactive shell histories, limits available TTY interfaces, and sets conservative security parameters on the agent system.
* **Compliance Certification Disclaimer**: Running these profiles generates audit evidence logs, but **does not make an image or VAM automatically SOC 2, HIPAA, or FedRAMP compliant**. True compliance requires organizational auditing of the entire physical network host infrastructure and access keys.
