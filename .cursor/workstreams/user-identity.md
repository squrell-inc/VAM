# Workstream: User identity (self-service password, then opaque user IDs)

```yaml
status: active
slug: user-identity
current_phase: 1
base_branch: feat/user-identity
design_docs:
  - design/user-identity.md
  - docs/src/content/docs/getting-started/users-and-roles.md
  - docs/src/content/docs/orchestrator/api.md
```

## Goal

Every user with a password session can change their own password without an
admin (`POST /v1/me/password`, `vam passwd`), with every session signed out and
API keys kept; then new users get opaque generated IDs and usernames become
case-insensitive login names that may be email addresses, while existing users
keep their IDs.

## Out of scope

- Must-change-password-on-first-login (later).
- Any email or SMTP reset, password recovery by mail, OIDC.
- Password strength policy beyond the admin reset's rules.
- The VAM app's UI for the change (the frontend team builds on the API contract).

## Locked decisions

Do not re-litigate without editing this list. Owner decisions, 2026-11-12.

1. Route `POST /v1/me/password`, body `{ currentPassword, newPassword }`; proto
   messages in `proto/vam/v1/` following the `ResetUserPassword` pattern; JSON camelCase.
2. Only a password session (a login session token) may call it. API keys are refused
   with 403; unauthenticated is 401.
3. `currentPassword` must verify; a wrong one is refused with a clear error that reveals
   nothing else. Wrong attempts are limited per user (equivalent to the login limit).
4. `newPassword` follows the same rules as the admin reset; a new password equal to the
   current one is refused.
5. On success: new hash and `password_updated_at`, **all** of the user's sessions are
   revoked including the current one, API keys are kept; the response tells the client
   to log in again.
6. Works for every role, operator included (it is their own password).
7. An idempotency key is required, as on other mutating routes.
8. CLI `vam passwd`: prompts for current and new password without echo; flags or env for
   non-interactive use consistent with other commands; clears the stored token after.
9. OpenAPI (`docs/api/openapi.yaml` and its shape test) and docs pages (users-and-roles,
   api) are updated; users-and-roles says what admins vs users can do.
10. Phase 2 (planned): existing users keep their IDs; new users get opaque generated IDs;
    username becomes a login name allowing `@` and `+`, matched case-insensitively; it
    starts with an audit of username use outside `users`.
11. Wrong-attempt limit (orchestrator, from decision 3; design §3.3): per user, in memory,
    burst 5, refill 1 per 60 s, checked before every verify, always on (also with
    `VAMD_RATE_LIMIT=off`); 429 reason `password-attempt-rate`.
12. Wrong current password is 403 `PERMISSION_DENIED` "current password is incorrect",
    never 401 (clients read 401 as "session gone").

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases (archived)

Full sections: [archive/user-identity.md](archive/user-identity.md).

| Phase | Name | Done |
|---|---|---|

### Phase 1 — Self-service password change

**Goal:** any user with a password session changes their own password over HTTP and the
CLI; all sessions signed out, API keys kept; wrong attempts limited.  
**Depends on:** none  
**Plan:** approved 2026-11-12 (owner brief; design `design/user-identity.md` §3)  
**Exit:** the phase 1 scenarios are `active` and green; `scripts/ci/local.sh --full`
green on `feat/user-identity`; OpenAPI and docs describe the route and who can do what.

- [ ] Scenarios and harness steps land pending, red for the right reason (W0)
- [ ] Server: route, store method, attempt limiter, proto (W1)
- [ ] CLI `vam passwd`, OpenAPI, docs (W2)

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `member-changes-own-password-and-signs-in-with-the-new-one` | new | process | `org-member` | the phase's exit: old password fails, new works |
| `password-change-signs-out-every-session-and-keeps-api-keys` | new | process | `org-member` | decision 5 |
| `wrong-current-password-is-refused-and-attempts-are-limited` | new | process | `org-member` | decisions 3, 11, 12 |
| `api-key-cannot-change-a-password` | new | process | `org-admin`, `org-member` | decision 2 |
| `new-password-equal-to-current-is-refused` | new | process | `org-member` | decision 4 |
| `password-change-needs-an-idempotency-key` | new | process | `org-member` | decision 7 |

No existing scenario changes: none guards `api/me.rs`, `users.rs` password paths or
`session_tokens.rs` revocation (`grep guards e2e/scenarios`). `api-rate-limits-a-flood`
guards `rate_limit.rs` and must stay green. Unauthenticated 401 and the operator case
(decision 6) are proven in `crates/vam-orchestrator/tests/api_tests.rs`: the runner's
operator is the bootstrap key and has no password.

**Items** (each names the pending scenarios it turns green)

- [ ] `UI-1.1` — Harness: `org` pieces keep their generated password; steps `login`,
  `change-password`, `identity-request` (design §3.6) in `crates/vam-e2e`
  (`scenario.rs` parse, `check.rs` lint, engine), README step docs; the six scenarios
  as `pending`. Accept: `cargo run -p vam-e2e -- check` passes; each scenario fails at
  its first `change-password` step with class `product` (404 from vamd); `cargo test -p
  vam-e2e`. Turns green: none.
- [ ] `UI-1.2` — Proto `ChangeMyPasswordRequest/Response` + rpc; store
  `change_own_password` in `users.rs` reusing `reset_user_password`; shared
  `validate_password`; per-user attempt limiter in `api/rate_limit.rs`; handler in
  `api/me.rs`; route in `api/mod.rs`. Accept: design §3.1 table, every row tested in
  `api_tests.rs` (401, API key 403, master key 403, wrong 403 + unchanged, 429 after 5,
  same 400, empty 400, missing key 400, success revokes all sessions incl. caller, key
  still works, operator changes own password); store unit tests. Turns green: all six.
- [ ] `UI-1.3` — `vam passwd` (no-echo prompts via `rpassword`, repeat check,
  `--current-password-from-env`, `--new-password-from-env`, Idempotency-Key, clears the
  stored session on 200, JSON output); CLI tests for arg parsing and the cleared session.
  `docs/api/openapi.yaml` path + schemas + `openapi_yaml.rs` assertion. Docs:
  users-and-roles (admin vs user table), orchestrator/api, getting-started/cli; `proves:`
  frontmatter naming the scenarios; `docs-features.yaml` row. Accept: `cargo test -p
  vam-cli -p vam-spec`, `cargo run -p vam-e2e -- check`, `pnpm build` in `docs/` when
  available. Turns green: none (keeps all six green).

**Waves** (models per playbook §2, medium or high only per the owner's brief)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 docs | design + tracker | — | vam | orchestrator | 30 min | — | none | [ ] | |
| 0 | `UI-1.1` | — (pending, red for the right reason) | vam | Sonnet 5.5 medium (harness code in `vam-e2e`, not only YAML) | 60 min | 0 docs | none | [ ] | |
| 1 | `UI-1.2` | all six | vam | Sonnet 5.5 high (auth, sessions, rate limit: security boundary) | 90 min | 0 | none | [ ] | |
| 2 | `UI-1.3` | — (keeps six green) | vam | Sonnet 5.5 medium (CLI + OpenAPI + docs, two crates) | 75 min | 1 | none | [ ] | |

**Open questions** (each with a default)

- Should a successful change also reset the user's wrong-attempt bucket? Default: no;
  it refills on its own within five minutes.
- Should the API-key refusal also cover run grants (`vam_rg_`)? Default: yes, any
  credential that is not `AuthSource::Session` is refused.
- Should the change emit an audit or telemetry event? Default: a `tracing::info!` line
  with the user id (never the secret) in phase 1; a catalogued audit event is later.

**Phase close:** all six scenarios `active` and green; `--full` green on the base; no
release PR into `main` (owner's word); final report to the owner.

**Team checklist — Phase 1**

Setup: deploy `vamd` from `feat/user-identity`, then the `vam` CLI from the same commit.
An org admin creates a member: `P=<secret> vam users create --username tester --role member --password-from-env P`.

1. `vam login --username tester` with the admin's password. **See:** logged in.
2. `vam passwd`, type a wrong current password. **See:** "current password is incorrect";
   `vam login` with the old password still works.
3. `vam passwd`, the right current password and a new one twice (nothing echoes).
   **See:** "password changed; every session was signed out".
4. `vam session list` (any command that needs a login). **See:** refused, asks you to log in.
5. `vam login` with the old password. **See:** refused. With the new one: logged in.
6. With an API key (`VAM_API_KEY=...`), `vam passwd`. **See:** refused, names `vam login`.

Report: "Phase 1 pass", or the step number with the error text.

### Phase 2 — Generated user IDs and email usernames

**Goal:** new users get opaque generated `user_id`s; `username` is a case-insensitive
login name that may contain `@` and `+`; existing users keep their IDs.  
**Depends on:** Phase 1  
**Plan:** not drafted (design `design/user-identity.md` §4)  
**Exit:** an email-style username logs in in any letter case; a new user's id is opaque;
an existing user's id, catalog namespace and owned rows are unchanged; the audit table
in design §4.2 is closed row by row.

- [ ] Audit of username use outside `users` (design §4.2)
- [ ] Generated ids for new users; `CreateUserRequest.user_id` no longer client-chosen
- [ ] Login-name validation and case-insensitive uniqueness, with migration

## Explicitly later

- Must-change-password-on-first-login.
- Email or SMTP reset; OIDC.
- A catalogued audit event for password changes.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-12 | 1 | Tracker opened; design `design/user-identity.md`; Phase 1 plan (owner brief) |
