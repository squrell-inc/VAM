# Docs archive

Pages moved out of the published docs because they describe design intent, not what ships. They
are not linted and not built. Each entry is `<section>/<page>.md`, with a one-line reason and the
URL it redirects from (`docs/redirects.json`, kind `design`).

Pages in this directory are design intent, not a description of the product.

- `architecture/design-principles.md`: engineering rules, not behaviour. Redirects from `/architecture/design-principles/`.
- `architecture/session-lifecycle.md`: its own banner says planned design. Redirects from `/architecture/session-lifecycle/`.
- `security/spiffe.md`: SPIFFE/SPIRE attestation, which says it is not implemented. Redirects from `/security/spiffe/`.
- `security/seccomp.md`: two tiers of generated seccomp profiles, not built. Redirects from `/security/seccomp/`.
- `orchestrator/router.md`: a UDS-to-vsock gRPC bridge for external clients, not built. Redirects from `/orchestrator/router/`.
- `orchestrator/scheduler.md`: NUMA cpuset scheduling, no code. Redirects from `/orchestrator/scheduler/`.
- `orchestrator/admission.md`: capacity barriers and queuing, no code. Redirects from `/orchestrator/admission/`.
