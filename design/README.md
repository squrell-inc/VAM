# Design proposals

`design/` holds proposals for open or in-progress work only. What the node does today is in the
docs site (`docs/src/content/docs/`). When a design's work ships, its user-facing facts move into
the docs (with a short "Design notes" section where the reasoning is worth keeping) and the design
doc is deleted; git history keeps it. Trackers for active work are in `.cursor/workstreams/`.

Several docs below are partly shipped. Their shipped parts are trimmed as each workstream closes.

| Design | Open work | Tracker |
|---|---|---|
| [Agent experience](agent-experience.md) | Problem-led roadmap (problems P1–P14, epics E1–E8); machine images and Vamfile (E7 Phases 2–5); egress Phases 4–5. Shipped parts: drafts and versions, permissions as data, egress Phases 1–3. | `agent-experience`, `machine-base` |
| [Agent runtime](agent-runtime.md) | E10: effect journal (Phase 4), long work (Phase 5), memory (Phase 6). Phases 0–3 shipped. | `agent-runtime` |
| [Apps and Spaces](vam-apps-spaces.md) | Apps, shared Spaces, App authority, retention, per-App quotas. Space and thread ids and Credentials shipped (`hosted/triggered-runs`). | none yet |
| [Connectors](connectors.md) | Real-tenant proof, installable connectors, more providers. Phases 0–4 built. | `connectors` |
| [Developer experience](developer-experience.md) | Packages and lockfile, `vam init`/`validate`/`dev`, signing, deployments (tracker Phases 3–11). | `developer-experience` |
| [Harness](harness.md) | Ablation grid, harness profiles per model class, recursive harnessing (H2–H4). H0 and H1 shipped. | `harness` |
| [Benchmark](benchmark.md) | Machine-off runs, business task set, on-vs-off table (B1–B3). B0 shipped (`runtime/benchmark-results`). | `harness` |
| [Isolation verify closeout](isolation-verify-closeout.md) | App-side version pin and model picker, member surfaces. Server side shipped. | `isolation-verify-closeout` |
| [Jig](jig.md) | Shipped basics only (`runtime/jig`); the larger E11 plan is a backlog line. | none |
| [Run receipt](run-receipt.md) | Phase 2: receipt surfaces (app view). Phases 1 and 3 shipped (`security/run-receipt`). | `run-receipt` |
| [Scenarios](scenarios.md) | UI layer and `--changed` selection (S4–S5); coverage appendix. The rest is in `architecture/testing` and `e2e/README.md`. | `e2e-scenarios` |
| [User identity](user-identity.md) | Phase 3 (proposed): one stored, unique namespace per user, kept across renames, with a migration that moves no draft. Phases 1 (self-service password change) and 2 (generated user IDs, case-insensitive email login names) shipped (`getting-started/users-and-roles`, `orchestrator/api`). | `user-identity` |
| [Release artifacts](release-artifacts.md) | Versioning, the manual release workflow, immutable tags, build matrix, provenance, install a node from a release with its brokers, `vam dock` and `vam upgrade`, one rc per milestone, final `v0.1.0`. Phases A, B, Final; Lima and Windows in the next release (`v0.2.0`). Approved 2026-11-17, restructured 2026-11-18; nothing built yet. | `release-artifacts` |
| [Skill list](skill-list.md) | Paged `GET /v1/skills`: released by default, own drafts on request with `catalog:write`. | `skill-list` |
