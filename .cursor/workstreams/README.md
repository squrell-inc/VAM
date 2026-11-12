# Workstreams

Phase trackers for design → plan → build. Procedure: `.cursor/skills/phased-workstream/SKILL.md`. How an epic phase runs from plan to close (roles, merges, microVM lanes, phase close): `phase-playbook.md`.

New tracker: copy `_template.md` to `<slug>.md` and add a row below. Team norms and CI hygiene: `working-agreement.md`. Finished phases and trackers are deleted; git history keeps them.

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
| `work-outside-chat` | Work outside chat (E5 slice): run grants, trigger gateway, Space and thread ids, schedule broker | shipped | 1 and 2 shipped (run grants, trigger gateway, Space and thread ids, `vam.schedule`); docs: `hosted/triggered-runs`, `hosted/schedule` |
| `notifications` | Notifications (E12 lane, row 7): one per-user record, many surfaces (app, CLI, agent tool; Slack in slice 2) | shipped | slices 1 and 2 shipped (records, API, CLI, agent tool, `vam.notify` with Slack); slice 3 (email, push, digests) backlog; docs: `orchestrator/notifications`, `hosted/notify` |
| `connectors` | Connectors: connectivity, admin enable and consent, per-user connect; Microsoft 365 and Linear first (`design/connectors.md`) | active | 1 released to `main` 2026-11-08, checklist out; 0 done 2026-11-07 |
| `machine-base` | Machine base (E7 Phase 1 + install slice, launch row 14): Debian slim default base with Python and git, `requiresRuntime` check, `vam node init` | active | 1 closed in base 2026-11-10 (W0-W5, 5/5 scenarios active); release PR into `main` open |


## Backlog (no tracker)

| Item | Note |
|---|---|
| E11 Jig, the larger plan | Queries and mutations, composed components, jig packages and a registry, agent-written `.jig`, Slack and A2UI adapters. The basics are shipped (`docs/src/content/docs/runtime/jig.md`); the earlier plan is in git history. |
| Primary agent | One default agent per user that asks the other agents the user may run (a delegate broker, depth one). Not started; the earlier proposal is in git history. |
