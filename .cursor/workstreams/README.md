# Workstreams

Phase trackers for design → plan → build. Procedure: `.cursor/skills/phased-workstream/SKILL.md`. How an epic phase runs from plan to close (roles, merges, microVM lanes, phase close): `phase-playbook.md`.

New tracker: copy `_template.md` to `<slug>.md` and add a row below. Team norms and CI hygiene: `working-agreement.md`. Finished phases move to `archive/`.

| Slug | Title | Status | Current phase |
|---|---|---|---|
| `isolation-verify-closeout` | Isolation UI verify: catalog read, workspace version pin, model picker | active | 4 |
| `developer-experience` | Developer experience: artifacts, policy layering, install/customize, playbooks, marketplace | active | 0 |
| `agent-experience` | Agent experience: problem-led roadmap; Epic 1 create, compose and release (draft, versions, no digests, toggles, URN origin) | active | 7 released, checklist out (2026-10-16) |
| `agent-runtime` | Agent runtime (E10): agents that finish the job (first try, web, context, steer, never twice, long work, learns) | active | 3 released to `main` 2026-10-29, checklist out; 2, 1 and 0 released, checklists out |
| `e2e-scenarios` | E2E scenarios (E9, from scratch): test-first phases, scenarios named by behaviour, process-mode guest on every PR, stop rules | active | 1 released to `main` 2026-10-18; later phases planned one at a time |
| `isolation-truth` | Isolation truth (E13): CI boots a microVM, jailer and TLS on by default, leaks shut, docs match the code | complete | 1 released to `main` 2026-10-24, 2 released 2026-10-27, 3 released 2026-11-09 ; checklists out; Phase 4 (developer docs) held for the owner's outline review |
| `harness` | Harness (H): every lever switchable, measured, profiled per model class | active | H0 + B0 released to `main` 2026-10-28 (baseline 0.1.0, 40/40); H1 next |
| `run-receipt` | Run receipt (E12): the signed, two-tier record of what a run did; `GET /v1/runs/{id}/receipt`, `vam receipt show/verify` | active | 1 released to `main` 2026-10-26, checklist out; 3 released to `main` 2026-11-10, checklist out; 2 waits on the E5 slice |
| `work-outside-chat` | Work outside chat (E5 slice): run grants, trigger gateway, Space and thread ids, schedule broker | active | 1 released to `main` 2026-10-31, checklist out; 2 (schedule broker) next |
| `notifications` | Notifications (E12 lane, row 7): one per-user record, many surfaces (app, CLI, agent tool; Slack in slice 2) | active | 1 released to `main` 2026-11-02, checklist out; 2 (slice 2, Slack) next |
| `connectors` | Connectors: connectivity, admin enable and consent, per-user connect; Microsoft 365 and Linear first (`design/connectors.md`) | active | 1 released to `main` 2026-11-08, checklist out; 0 done 2026-11-07 |
| `machine-base` | Machine base (E7 Phase 1 + install slice, launch row 14): Debian slim default base with Python and git, `requiresRuntime` check, `vam node init` | active | 1 closed in base 2026-11-10 (W0-W5, 5/5 scenarios active); release PR into `main` open |
| `user-identity` | User identity: self-service password change (`POST /v1/me/password`, `vam passwd`), then opaque user IDs and email usernames | active | 1 closed in base 2026-11-12 (self-service password change); 2 (opaque ids, email usernames) not drafted |

## Archived (done)

| Slug | Title | Status | Current phase |
|---|---|---|---|
| [`hsp`](archive/hsp.md) | HSP kernel (proto, UDS gRPC, manifests, hooks) | done | done |
| [`hsp-echo-ts`](archive/hsp-echo-ts.md) | TypeScript HSP echo + operator REST | done | done |
| [`telemetry`](archive/telemetry.md) | Telemetry kernel (libraries, validators, event/audit types) | done | 7 |
| [`telemetry-product`](archive/telemetry-product.md) | Telemetry productization (wires, journey UI, OTLP + standards egress) | completed | done |
| [`journey-debug-capture`](archive/journey-debug-capture.md) | Redacted tool args + LLM outcomes on run events for journey debug | done | done |
| [`workspace-files-ui`](archive/workspace-files-ui.md) | Workspace Files UI: Tauri bridge, list panel, save/open over Drive9 HEAD | done | done |
| [`workspace-files-upload`](archive/workspace-files-upload.md) | Owner upload into committed Drive9 HEAD under workspace/ | done | done |
