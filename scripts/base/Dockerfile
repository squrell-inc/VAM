# syntax=docker/dockerfile:1
# The standard VAM guest base (vam/base:2): Debian slim (glibc) with a small curated Python
# set, git, curl, jq, busybox and Bun. Build time only: nothing here runs in the guest.
# vam-runtime is NOT in this image; scripts/build-rootfs.sh injects it as /sbin/init.
#
# scripts/build-rootfs.sh builds this with every argument read from scripts/base/pins.
ARG DEBIAN_TAG=trixie-slim
ARG DEBIAN_DIGEST
FROM debian:${DEBIAN_TAG}@${DEBIAN_DIGEST}

ARG SNAPSHOT_TIMESTAMP
ARG BUN_VERSION
ARG BUN_SHA256

ENV DEBIAN_FRONTEND=noninteractive LANG=C.UTF-8

# Reproducible packages: apt reads snapshot.debian.org at a pinned timestamp. The snapshot's
# Release files are old by design, hence Check-Valid-Until=false. http is fine: apt verifies
# the archive signature, and ca-certificates is not installed yet. No docs, man pages or
# locales are unpacked, to keep the image small.
RUN set -eux; \
    rm -f /etc/apt/sources.list /etc/apt/sources.list.d/*; \
    printf 'Acquire::Check-Valid-Until "false";\nAcquire::Retries "5";\n' > /etc/apt/apt.conf.d/90vam; \
    printf '%s\n' \
      "deb http://snapshot.debian.org/archive/debian/${SNAPSHOT_TIMESTAMP}/ trixie main" \
      "deb http://snapshot.debian.org/archive/debian/${SNAPSHOT_TIMESTAMP}/ trixie-updates main" \
      "deb http://snapshot.debian.org/archive/debian-security/${SNAPSHOT_TIMESTAMP}/ trixie-security main" \
      > /etc/apt/sources.list; \
    printf 'path-exclude=/usr/share/man/*\npath-exclude=/usr/share/info/*\npath-exclude=/usr/share/locale/*\npath-exclude=/usr/share/doc/*\npath-include=/usr/share/doc/*/copyright\n' \
      > /etc/dpkg/dpkg.cfg.d/90vam-slim; \
    apt-get update; \
    apt-get install -y --no-install-recommends \
      python3 python3-pip python3-requests python3-pydantic python3-pandas python3-numpy \
      python3-dateutil python3-openpyxl \
      git curl jq ca-certificates \
      busybox-static netcat-openbsd \
      unzip

# Bun (glibc baseline), version and sha256 pinned. unzip is only needed here.
RUN set -eux; \
    case "${BUN_VERSION}${BUN_SHA256}" in *UNRESOLVED*) echo "scripts/base/pins has an unresolved Bun pin; run scripts/base/resolve-pins.sh --write" >&2; exit 1;; esac; \
    curl -fsSL -o /tmp/bun.zip \
      "https://github.com/oven-sh/bun/releases/download/${BUN_VERSION}/bun-linux-x64-baseline.zip"; \
    echo "${BUN_SHA256}  /tmp/bun.zip" | sha256sum -c -; \
    unzip -q -o -j /tmp/bun.zip 'bun-linux-x64-baseline/bun' -d /usr/local/bin; \
    chmod 0755 /usr/local/bin/bun; \
    /usr/local/bin/bun --version; \
    rm -f /tmp/bun.zip; \
    apt-get purge -y --auto-remove unzip

# Python: bytecode is compiled now because the guest root is read-only. pip stays so that
# `pip list` works, but packages install at build time only (locked decision 5): no index,
# and Debian's EXTERNALLY-MANAGED marker is kept, so `pip install` fails at once.
RUN set -eux; \
    printf '[global]\nno-index = true\n' > /etc/pip.conf; \
    ls /usr/lib/python3*/EXTERNALLY-MANAGED; \
    python3 -m compileall -q /usr/lib/python3 /usr/lib/python3.*/ ; \
    pip list >/dev/null; \
    if pip install --no-input --disable-pip-version-check six >/dev/null 2>&1; then echo "pip install must fail" >&2; exit 1; fi; \
    python3 -c "import requests, pydantic, pandas, numpy, dateutil, openpyxl"; \
    git --version; jq --version; curl --version | head -1; /bin/busybox true; command -v nc

# Slim down: apt state, caches, logs, temp files. Copyright files stay (licences).
RUN set -eux; \
    apt-get clean; \
    rm -rf /var/lib/apt/lists/* /var/cache/apt/* /var/cache/debconf/*-old /var/lib/dpkg/*-old \
      /var/log/* /tmp/* /var/tmp/* /root/.cache /etc/apt/apt.conf.d/90vam /etc/dpkg/dpkg.cfg.d/90vam-slim
