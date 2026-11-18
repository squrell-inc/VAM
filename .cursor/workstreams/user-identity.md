# Workstream: User identity (self-service password, opaque user IDs, one namespace per user)

```yaml
status: active
slug: user-identity
current_phase: 3 (one namespace per user) breakdown and wave 0 drafted 2026-11-18, waiting for the owner; 1 and 2 released to main
base_branch: claude/wizardly-feynman-d5ch88 (phase 3; carries feat/member-draft-agents merged into main)
design_docs:
  - design/user-identity.md
  - docs/src/content/docs/getting-started/users-and-roles.md
  - docs/src/content/docs/orchestrator/api.md
  - docs/src/content/docs/orchestrator/member-agents.md
```

## Goal

Every user with a password session can change their own password without an
admin (`POST /v1/me/password`, `vam passwd`), with every session signed out and
API keys kept; then new users get opaque generated IDs and usernames become
case-insensitive login names that may be email addresses, while existing users
keep their IDs; then every user holds a URN namespace no other user holds, kept
across renames, so a member's draft agent can never land in, or reveal, another
member's namespace.

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

13. A successful self-service change writes `vam.v1.user.password_changed` to the audit ledger partition `vam_audit_identity` (owner, 2026-11-13: required before release). No secret in it; refusals record nothing; a failed audit write after the change is logged and the request still answers 200. Admin reset and `vamd --reset-password` are not audited yet.
14. Open-question defaults kept (owner, 2026-11-13): a success does not reset the wrong-attempt bucket; every non-session credential, run grants included, is refused.
15. New users get a generated `user_id`: `usr_` plus a random identifier, lowercase and URL-safe
    (owner, 2026-11-14). Existing users keep their IDs. `CreateUserRequest.user_id` is no longer
    client-chosen for new users.
16. Usernames are login names that may contain `@` and `+` (email style), matched
    case-insensitively (owner, 2026-11-14). If existing usernames clash once compared
    case-insensitively, the migration refuses to run and reports the clashing usernames; an admin
    resolves them first. No automatic renaming.

## Status legend

- `[ ]` not started
- `[~]` in progress
- `[x]` done (code + tests, or merged)

## Phases

### Done phases

Done phase sections are deleted; git history keeps them.

| Phase | Name | Done |
|---|---|---|

### Phase 1 — Self-service password change

**Goal:** any user with a password session changes their own password over HTTP and the
CLI; all sessions signed out, API keys kept; wrong attempts limited.  
**Depends on:** none  
**Plan:** approved 2026-11-12 (owner brief; design `design/user-identity.md` §3); **closed in base 2026-11-12** (#1, #2, #8, #9; six scenarios active and green; no release PR into `main` without the owner's word)  
**Exit:** the phase 1 scenarios are `active` and green; `scripts/ci/local.sh --full`
green on `feat/user-identity`; OpenAPI and docs describe the route and who can do what.

- [x] Scenarios and harness steps land pending, red for the right reason (W0, #2)
- [x] Server: route, store method, attempt limiter, proto (W1, #8)
- [x] CLI `vam passwd`, OpenAPI, docs (W2, #9)
- [x] Audit event for a successful change (W3, #11, owner 2026-11-13)

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

- [x] `UI-1.1` — Harness: `org` pieces keep their generated password; steps `login`,
  `change-password`, `identity-request` (design §3.6) in `crates/vam-e2e`
  (`scenario.rs` parse, `check.rs` lint, engine), README step docs; the six scenarios
  as `pending`. Accept: `cargo run -p vam-e2e -- check` passes; each scenario fails at
  its first `change-password` step with class `product` (404 from vamd); `cargo test -p
  vam-e2e`. Turns green: none.
- [x] `UI-1.2` — Proto `ChangeMyPasswordRequest/Response` + rpc; store
  `change_own_password` in `users.rs` reusing `reset_user_password`; shared
  `validate_password`; per-user attempt limiter in `api/rate_limit.rs`; handler in
  `api/me.rs`; route in `api/mod.rs`. Accept: design §3.1 table, every row tested in
  `api_tests.rs` (401, API key 403, master key 403, wrong 403 + unchanged, 429 after 5,
  same 400, empty 400, missing key 400, success revokes all sessions incl. caller, key
  still works, operator changes own password); store unit tests. Turns green: all six.
- [x] `UI-1.3` — `vam passwd` (no-echo prompts via `rpassword`, repeat check,
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
| 0 docs | design + tracker | — | vam | orchestrator | 30 min | — | none | [x] | #1 |
| 0 | `UI-1.1` | — (pending, red for the right reason) | vam | Sonnet 5.5 medium (harness code in `vam-e2e`, not only YAML) | 60 min | 0 docs | none | [x] | #2 |
| 1 | `UI-1.2` | all six | vam | Sonnet 5.5 high (auth, sessions, rate limit: security boundary) | 90 min | 0 | none | [x] | #8 |
| 2 | `UI-1.3` | — (keeps six green) | vam | Sonnet 5.5 medium (CLI + OpenAPI + docs, two crates) | 75 min | 1 | none | [x] | #9 |

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
**Plan:** drafted 2026-11-14; the owner's brief authorises building it through phase close; the
open questions run on their defaults until the owner answers them (design `design/user-identity.md`
§4; decisions 10, 15, 16; audit §4.2 closed). Base `feat/user-identity-phase2`; each wave is one
PR into it. **Closed in base 2026-11-17** (#14, #15, #17, hotfix #16 via #18; five scenarios active
and green); release PR into `main` awaits the owner.  
**Exit:** the five phase 2 scenarios are `active` and green; the existing-user and clash tests
below pass; `scripts/ci/local.sh --full` green on the base; OpenAPI and docs say how ids and
login names work.

- [x] Audit of username use outside `users` (design §4.2, closed 2026-11-14)
- [x] Scenarios and harness steps land pending, red for the right reason (W0, #14)
- [x] Generated ids for new users; `CreateUserRequest.user_id` no longer client-chosen (W1, #15)
- [x] Login-name validation and case-insensitive uniqueness, with migration (W1, #15)
- [x] One resolution rule; operator guard after resolution (W1, #15)
- [x] Session principal from the user id; no name maps to the bootstrap key (W2 replaced by hotfix #16 into `main`, owner 2026-11-14; merged back into the base in #18)
- [x] Namespace for email usernames; OpenAPI, docs, CLI help (W3, #17)

**Chosen details** (proposals in this breakdown; design §4.1, §4.3, §4.4)

- Id: `usr_` + 26 characters `[a-z2-7]` (128 random bits from `OsRng`), minted with the existing
  `spaces::mint_id` (`crates/vam-orchestrator/src/spaces.rs:33`, already used for `sp_`, `th_`,
  `nt_`, `ct_`). No new crate, no timestamp in the id. Path-safe for workspace prefixes
  (`vam-storage/src/owner_prefix.rs:222`: `[A-Za-z0-9._-]`, no `--`).
- `CreateUserRequest.user_id`: a non-empty value is **refused** with 400 `userId is generated by
  the node; leave it empty`. The proto field number stays (marked deprecated). Why: nobody sends
  it today (CLI `cmd/users.rs:34-39` uses `..Default::default()`, the e2e engine
  `engine.rs:676-681` and the API tests omit it), so refusing breaks no caller, and a client that
  believes it chose an id learns it at once instead of later acting on a wrong id. No operator
  path is needed: existing users keep their ids and the seed rows are made in-process.
- Uniqueness: `CREATE UNIQUE INDEX idx_users_username_nocase ON users(username COLLATE NOCASE)`,
  made only after a clash check finds none. NOCASE folds ASCII only; login names stay ASCII (as
  today), so the fold is complete. No `username_key` column, no backfill.
- Clash: `vamd` exits 1 before serving, listing each clashing group with user ids and the fix;
  the fix is a new offline flag `vamd --rename-user <user_id> <new_username>` (open question 3).
  The check never fails `SqliteWorkflowStore::open`: an open error makes `vamd` fall back to an
  in-memory store (`bin/vamd.rs:329-343`).
- Resolution: exact `user_id` first, then username ignoring case; new usernames may not start
  with `usr_`, equal another user's id, or be `bootstrap`; operator guards check the resolved row.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `email-username-signs-in-in-any-letter-case` | new | process | `org-member-email` (new piece) | decision 16, the phase's exit; `/v1/me` shows the username as typed and a namespace from its local part (open question 1) |
| `new-user-gets-a-generated-id-it-cannot-choose` | new | process | `org-administrator`, `org-member` | decision 15: `userId` holds `usr_` and differs from the username; a chosen `userId` is 400 and creates nothing; namespace from the username, never the id |
| `username-differing-only-in-letter-case-is-refused` | new | process | `org-administrator`, `org-member-email` | decision 16 uniqueness: the upper-cased name is 409; a `usr_…` or `bootstrap` username is 400 |
| `user-path-resolves-by-id-then-by-login-name` | new | process | `org-administrator`, `org-member-email` | §4.4: `/v1/users/{id}/keys` and `/v1/users/{USERNAME}/keys` reach the same user; `/v1/users/OPERATOR/keys` is still 403 |
| `generated-id-and-email-login-survive-a-restart` | new | process | `org-member-email` | the migration runs on every start: id, any-case login and the member's key are unchanged after `restart-vamd` |

Proven in Rust, not as scenarios: the harness cannot seed a `vamd.db` (`vam-e2e` has no SQLite
and must not depend on `vam-orchestrator`, `crates/vam-e2e/Cargo.toml`), and after W1 the API can
no longer make a user whose id equals its name. So an **existing user** (old-schema db with
`alice`/`alice`, owned catalog and key rows) is proven unchanged in
`crates/vam-orchestrator/tests/user_identity_migration_tests.rs` (new: id, username, owner rows,
`defaultNamespace`, login as `ALICE`), and the **clash refusal** in
`crates/vam-orchestrator/tests/vamd_username_clash.rs` (new, runs the real `vamd` like
`tests/vamd_init.rs`: seeded `bob`/`Bob` exits 1 naming both with ids; `--rename-user` fixes it;
`--reset-password` still works on the clashing db). The seeded `operator` row is the one existing
user the process layer sees (bootstrap key); the Phase 1 scenarios keep reading it.

Existing scenarios that must stay green: the six Phase 1 scenarios (guard `users.rs`,
`session_tokens.rs`, `api/me.rs`); `member-chats-with-a-released-agent` (guards
`users_admin.rs`, `users_keys_admin.rs`, `owner.rs`); `node-init-run-twice-changes-nothing-and-shows-the-key-once`
and `vamd-refuses-plain-http-by-default` (guard `bin/vamd.rs`); `api-rate-limits-a-flood`
(`daemon.rs`); `admin-installs-the-squrell-listing-at-its-tag` and
`listing-from-an-added-marketplace-installs-as-unverified-within-its-limits` (guard
`vam-spec/src/origin.rs`). The 31 scenarios with a non-Owner org piece make their user through
`POST /v1/users`, so after W1 they all run on `usr_` ids (workspace prefixes, schedules, contacts,
marketplace): `--smoke` is the regression guard.

**Benchmark delta:** none. Identity moves no `design/benchmark.md` §1 line; no baseline run.

**Items** (each names the pending scenarios it turns green)

- [x] `UI-2.1` — Harness (`crates/vam-e2e`: `world.rs`, `scenario.rs`, `check.rs`, `engine.rs`,
  `identity.rs`, README "Password steps"): `org` piece field `username` (template with `{tag}`;
  new piece `e2e/world/org-member-email.yaml`, role Member, a mixed-case `…+e2e@Example.Test`
  name); `login: { case: as-created | lower | upper }`; `identity-request` gains `fields` (same
  matchers as other `fields`) and path placeholders `{<alias>.userId}`, `{<alias>.username}`,
  `{<alias>.USERNAME}`; new step `create-user: { as, username, userId, status, contains }`
  (`POST /v1/users`, fresh password, Idempotency-Key; `as` omitted is the bootstrap key). Drop the
  `userId` → username fallback (`engine.rs:690-694`, missing id is class `product`); a refused
  create for a piece with a `username` field is class `product`, not `harness`; the test stub
  (`tests/support/mod.rs:334-337`) answers a `usr_` id. The five scenarios as `pending`. Accept:
  `cargo run -p vam-e2e -- check`; `cargo test -p vam-e2e`; `scripts/e2e/run.sh --scenarios
  <five>` each red, class `product`: the four with `org-member-email` at the piece's create (400,
  charset), `new-user-gets-a-generated-id-it-cannot-choose` at `create-user` with a `userId`
  (answered 200). Turns green: none.
  Decide: NOW — locked decisions 15, 16 (owner); not scored.
- [x] `UI-2.2` — Generated ids. `api/users_admin.rs:69-77`: non-empty `userId` → 400; else mint
  `usr_` with `spaces::mint_id`, retry on a primary-key clash (at most 3). `bootstrap_first_admin`
  (`users.rs:262`) mints too (idempotent by name, unchanged). Store `create_user*` keep an
  explicit id for seeds and tests. Proto comment on `CreateUserRequest.user_id` (deprecated, must
  be empty). Fix tests that read a created user by name as id (`api_tests.rs:7060`). Accept: API
  tests (generated id shape, chosen id 400 and no row, bootstrap admin id `usr_`, retry path);
  `cargo test -p vam-orchestrator --lib users`. Turns green:
  `new-user-gets-a-generated-id-it-cannot-choose`.
  Decide: NOW — locked decision 15; not scored.
- [x] `UI-2.3` — Login names and migration (`users.rs`, `bin/vamd.rs`, `daemon.rs`). Validator
  for usernames: ASCII `[A-Za-z0-9._+@-]`, at most one `@`, length per open question 2, not
  starting `usr_` (any case), not `bootstrap` (any case), not equal (ignoring case) to another
  user's id; ids keep the old validator. Clash check and NOCASE index in
  `initialize_users_schema` (§4.3), clashes held on the store, never an `open` error;
  `vamd` refuses to serve or `--init` on clashes (exit 1, list, fix command), after
  `--reset-password` and the new `--rename-user`. `update_username` validates and maps a unique
  violation to 409. Accept: unit tests (charset, `@`/`+`, reserved, NOCASE dup 409, rename rules,
  index created once, clash leaves no index); `tests/vamd_username_clash.rs`;
  `tests/user_identity_migration_tests.rs`. Turns green:
  `username-differing-only-in-letter-case-is-refused`,
  `generated-id-and-email-login-survive-a-restart` (with UI-2.4).
  Decide: NOW — locked decision 16; not scored.
- [x] `UI-2.4` — Resolution (`users.rs:482-501`, `:557-564`, `session_tokens.rs:114`,
  `api/users_keys_admin.rs:26-37`, `:53-54`, `api/mod.rs:414-419`): `get_user_by_username` and
  `resolve_user` match `username = ?1 COLLATE NOCASE` after an exact id miss; login uses
  `resolve_user` (open question 5); `reject_operator_user_admin_target` runs on the resolved
  `user_id`, not the raw path (else `/v1/users/OPERATOR/keys` reaches the operator once names
  fold case). Id-only routes stay id-only. Accept: API tests (login in three cases, keys route by
  id and by upper-cased name, `OPERATOR` and `Operator` 403, id beats a legacy name equal to it);
  `cargo test -p vam-orchestrator --test login_tests --test api_tests`. Turns green:
  `email-username-signs-in-in-any-letter-case` (except its namespace field, UI-2.6),
  `user-path-resolves-by-id-then-by-login-name`.
  Decide: NOW — proof 3 (security floor: the raw-string operator guard would be bypassed by case
  folding), cost 0 (files already in W1).
- [x] `UI-2.5` (hotfix #16 into `main`, merged back in #18) — Principal and bootstrap match (found by the audit, rows 2 and 3). Today a session
  principal is `User::"<username>"` (`session_tokens.rs:361`) and any principal
  `User::"bootstrap"` maps to the operator (`owner.rs:29-51`), so a user an org admin names
  `bootstrap`, or a key an org admin mints with name `bootstrap` (`users_keys_admin.rs:92-96`),
  acts as the operator's owner; the start-up backfill (`owner.rs:99-103`) then stamps such a key
  `operator`. Fix: session principal `User::"<user_id>"` (same string for users whose id equals
  their name); a non-empty `owner_user_id` wins over the principal match for every non-bootstrap
  key id; both mint paths (`users_keys_admin.rs`, `api/mod.rs:429-433`) refuse a name or principal
  that is the bootstrap principal (400); the backfill touches only rows whose owner is empty or
  `local`. Accept: unit tests in `owner.rs` and `session_tokens.rs`; API tests (session of a
  legacy user `bootstrap` owns its own rows; mint named `bootstrap` 400; restart keeps a member
  key's owner). Turns green: none (Rust-proven).
  Decide: NOW — proof 3 (security floor override: org admin to operator ownership), cost 1.
- [x] `UI-2.6` — Namespace (`vam-spec/src/origin.rs:166`, `api/me.rs:47`): for a username with
  `@`, the default namespace is the local part, `+tag` dropped, lowercased; `default` when that is
  not a slug (open question 1). Names without `@` or `+` give exactly today's result, so existing
  users are unchanged. Accept: `cargo test -p vam-spec`; `tests/me_tests.rs` row for an email
  user. Turns green: `email-username-signs-in-in-any-letter-case`.
  Decide: NOW — proof 2 (closes a confirmed defect: every email user would get `default`), cost 0.
- [x] `UI-2.7` — Docs and contract: `docs/api/openapi.yaml` (`CreateUserRequest.userId`
  deprecated/400, username charset, 409, `UserAccount.userId` is opaque) and
  `crates/vam-spec/tests/openapi_yaml.rs`; `getting-started/users-and-roles.md` (login names,
  case, generated ids, the clash and `vamd --rename-user`), `orchestrator/user-isolation.md`,
  `orchestrator/api.md`, `orchestrator/overview.md` (the new flag), `getting-started/cli.md`
  (`vam keys create --user` takes an id or a login name), `authoring/model.md:50` (namespace
  rule); `proves:` naming the five scenarios and the two Rust tests; `docs-features.yaml` row.
  Accept: `cargo test -p vam-spec`; `cargo run -p vam-e2e -- check`; `pnpm build` in `docs/`
  when available. Turns green: none (keeps five green).
  Decide: NOW — locked decision 9 pattern (contract and docs ship with the change), cost 1.

**Waves** (models per the owner's brief: high for migration, ids, login and resolution)

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 docs | design + tracker | — | vam | orchestrator | 30 min | — | none | [~] | |
| 0 | `UI-2.1` | — (pending, red for the right reason) | vam | Sonnet 5.5 medium (harness code in `vam-e2e`, not only YAML) | 60 min | 0 docs | none | [x] | #14 |
| 1 | `UI-2.2`, `UI-2.3`, `UI-2.4` | `new-user-gets-a-generated-id-it-cannot-choose`, `username-differing-only-in-letter-case-is-refused`, `user-path-resolves-by-id-then-by-login-name`, `generated-id-and-email-login-survive-a-restart` | vam | Sonnet 5.5 high (migration, id generation, login and resolution: data integrity and auth) | 120 min | 0 | none | [x] | #15 |
| 2 | `UI-2.5` | — (Rust-proven; keeps W1's four green) | vam | Sonnet 5.5 high (principal to owner mapping: auth boundary) | 60 min | 1 | none | replaced | #16 (hotfix into `main`) |
| 3 | `UI-2.6`, `UI-2.7` | `email-username-signs-in-in-any-letter-case` | vam | Sonnet 5.5 medium (vam-spec + OpenAPI + docs, two crates) | 75 min | 2 | none | [x] | #17 |

Decide (waves): W0–W3 NOW — locked decisions 15, 16 and two security-floor findings; four PRs
into `feat/user-identity-phase2`. W2 touches no file W3 does; W1 and W2 share
`users_keys_admin.rs` and `session_tokens.rs`, so they run in order.

**Open questions** (each with a default)

1. Default namespace for an email username? Default: the local part before `@`, `+tag` dropped,
   lowercased (`Ada.Lovelace+work@Example.com` → `ada.lovelace`); `default` if that is not a slug.
   Never derived from the generated id. Two domains with the same local part share a default;
   the namespace is a default, not a rule (`authoring/model.md:50`).
2. Username length? Default: raise the limit from 64 to 254 (an email address can be longer than
   64); ids stay at most 64.
3. How does an admin fix a clash when `vamd` will not start? Default: new offline flag
   `vamd --rename-user <user_id> <new_username>` (validates, prints old and new, exits), in the
   style of `--reset-password`; no HTTP rename route. Alternative with no new surface: a documented
   `sqlite3` `UPDATE users SET username = … WHERE user_id = …`.
4. Legacy cross-matches (a username equal to another user's id, an existing username starting
   `usr_` or equal to `bootstrap`)? Default: kept, not renamed; the id wins in resolution; `vamd`
   logs one warning at start naming them. Not a refusal (decision 16 covers case clashes only).
5. Login by user id? Default: yes, login uses the one resolution rule (id exact, then name
   ignoring case), so existing users whose id differs from their name keep working.
6. Should the id-only routes (`deactivate`, `password`, `publish-grant`, catalog reassign) also
   accept a login name? Default: no; only routes that accept one today keep doing so.
7. An email username appears in channel messages ("decided by", `notify_gateway.rs:291-298`)
   and in the identity audit payload. Default: shown as stored; it is what the admin typed.
8. Ship UI-2.5 to `main` ahead of the phase (it is a pre-existing escalation, not new)? Default:
   no; it lands in W2 and the release follows the phase, unless the owner says otherwise.
   **Owner 2026-11-14: yes.** Shipped as hotfix #16 from `main`; W2 is dropped and `main` is
   merged back into the base after #16 lands. Questions 1–7 keep their defaults (owner, same day).

**Team checklist (Phase 2)** — on a node running the release, with an org admin session:

1. `POST /v1/users` with `{"username":"Ada.Lovelace+qa@Example.com","password":"…"}`. **See:** 200,
   `userId` starts `usr_`. With a non-empty `userId`: 400 `userId is generated by the node; leave it empty`.
2. Create `ada.lovelace+qa@example.com`. **See:** 409, "matched ignoring letter case".
3. `vam login` as `ADA.LOVELACE+QA@EXAMPLE.COM`. **See:** logged in; `GET /v1/me` shows the name as
   typed at create and `defaultNamespace` `ada.lovelace`.
4. `vam keys create --name qa --user-id ada.lovelace+qa@example.com`. **See:** key bound to the `usr_` id.
5. Existing users: sign in as before. **See:** same ids, same names.
6. Create a user named `bootstrap` or `usr_x`. **See:** 400.
7. Start-up log after the upgrade. **See:** no `usernames must be unique ignoring letter case`
   refusal (else rename with `vamd --rename-user`), and any `revoked API key` line from #16 noted.

Report: "Phase 2 pass", or the step number with the error text.

### Phase 3 — One namespace per user

**Goal:** every user has one stored URN namespace, unique on the node, assigned once (from the
username, with a fixed suffix only when the readable base is taken) and kept across renames and
restarts; `GET /v1/me` `defaultNamespace` reports it; the member draft rule enforces it; existing
users are migrated without moving a draft.  
**Depends on:** Phase 2 (released to `main`, #20) and `member-draft-agents` (closed 2026-11-18,
**not yet released to `main`**). The phase branch merges `feat/member-draft-agents` into `main`
(41af8b0; 16 conflict hunks in `crates/vam-e2e` and four docs pages, both sides kept). It lands
after that release, or the owner releases both together.  
**Plan:** drafted 2026-11-18 (design `design/user-identity.md` §6); wave 0 written and proven red
in the same session at the owner's request. **Waiting for the owner** to lock decisions 17–21 and
approve the waves.  
**Exit:** the three new scenarios `active` and green and the changed one green; the migration and
rule tests below pass; `scripts/ci/local.sh --full` green on the base;
`orchestrator/member-agents.md` has no "Known limitation" section and says how namespaces are
assigned and migrated.

- [x] Design §6 and this breakdown (W0 docs)
- [x] Scenarios land pending, red for the right reason (W0)
- [ ] Stored namespace, assignment at create, migration of existing users (W1)
- [ ] `/v1/me` and the member draft rule read the stored namespace (W1)
- [ ] Docs drop the limitation; OpenAPI, proto comments, docs-features row (W2)

**Proposed decisions** (owner to lock; numbered to follow the list above; design §6.2)

17. A user's namespace is stored in `users.namespace`, unique, set once at create; a rename, a
    restart or a role change never changes it. `GET /v1/me` keeps the field name
    `defaultNamespace` and reports the stored value.
18. Assignment: base from the username (email: local part without `+tag`; lowercased; characters
    outside `[a-z0-9._-]` become `-`; ends trimmed of `.`, `-`, `_`; at most 48; empty is `user`).
    Taken (another user holds it, it is reserved: `default`, `local`, `operator`, `bootstrap`, or
    a catalog row under it is owned by another user): `<base>-<6 of [a-z2-7] from a SHA-256 of the
    user id>`, then 10, then 16 characters. No counter. Never `default` for a new user.
19. Seed rows keep today's values (`operator`, `local`); marketplace owner rows get none.
20. Migration on every open, idempotent, never refuses a start: backfill in order of the earliest
    catalog row each user owns under their old derived namespace, then by `created_at`; first
    claimant keeps the old value, the others get the suffix; one log line per user moved.
21. Existing drafts do not move. A member who lost a shared namespace keeps and edits the drafts
    they own there; new drafts go under the new namespace.

**Scenario delta**

| Scenario | Change | Layer | Given | Why |
|---|---|---|---|---|
| `members-with-the-same-email-local-part-get-their-own-namespaces` | new | process | `org-member-same-local-part-north`, `org-member-same-local-part-south` (new), `skill-host-echo`, `agent-member-draft-with-host-echo` | the brief: two colliding usernames each get their own namespace; the second member's save on the first one's URN is the namespace 400, never the owner 403 |
| `members-whose-names-are-not-slugs-get-namespaces-of-their-own` | new | process | `org-member-name-not-a-slug`, `org-member-name-not-a-slug-other` (new), same agent pieces | decision 18: non-slug names no longer share `default`; each namespace is readable (`ann-…`, `bob-…`) |
| `member-namespaces-stay-apart-across-a-restart` | new | process | north and south pieces, `agent-member-draft-second` (new) | decisions 17 and 20: the assignment survives a restart (a new URN under the namespace cached before it is still 200) |
| `email-username-signs-in-in-any-letter-case` | changed: `then.0` `defaultNamespace: ada.lovelace` → `{ matches: '^ada\.lovelace(-[a-z2-7]{6})?$' }` | process | `org-member-email` | **spec change for the owner**: one `vamd` serves the whole run and four scenarios create an `ada.lovelace…` user, so only the first keeps the plain value. Green before and after the phase |

Must stay green: `member-draft-agent-is-private-and-in-the-members-namespace` (its two members
already have distinct names), `new-user-gets-a-generated-id-it-cannot-choose` (`defaultNamespace`
equals a unique default username), and every scenario whose piece body uses `{{ns}}`
(`member-builds-and-runs-own-draft-agent-from-a-released-skill`,
`member-draft-agent-refuses-a-skill-that-is-not-released`,
`demoted-publishers-draft-agent-stops-using-their-draft-skill`,
`member-draft-agent-uses-a-connector-skill-through-the-members-connection`,
`a-publisher-saves-a-draft-agent-and-runs-it-by-ref`); `--smoke` is the regression guard, because
every `org` piece now gets a stored namespace.

Proven in Rust, not as scenarios (the harness cannot seed a database, §4.3, and cannot write into
another identity's namespace under a free name): the migration of an old-schema database
(`tests/user_identity_migration_tests.rs`: `ann@north` and `ann@south`, `_x` and `-y` on
`default`, a publisher row under `bob`; first claimant keeps it, the other is suffixed, the log
line, existing drafts still editable, new drafts under the new value); a free and a taken name in
another member's namespace give the same 400 (`member_draft.rs`); a rename keeps the namespace;
two concurrent creates with one base get two namespaces (`api_tests.rs`); the base rule
(`vam-spec`).

**Benchmark delta:** none.

**Items** (each names the pending scenarios it turns green)

- [x] `UI-3.0` — Wave 0: world pieces `org-member-same-local-part-north`, `…-south`,
  `org-member-name-not-a-slug`, `…-other`, `agent-member-draft-second`; the three new scenarios as
  `pending` with `expectFail`; the changed `then` in `email-username-signs-in-in-any-letter-case`.
  No harness code (`put-draft` `urnOf`, `{{ns}}`, `fields` `matches` and `restart-vamd` exist).
  Accept: `cargo run -p vam-e2e -- check`; `scripts/e2e/run.sh --scenarios <four> ` gives three
  red at the `urnOf` step with 403 `only the artifact owner may version this agent`, class
  `product`, and the changed one green. Turns green: none.
- [ ] `UI-3.1` — Base rule in `vam-spec` (`origin.rs`): `namespace_base(username)` and
  `namespace_suffixed(base, user_id, len)`; keep `default_namespace` (callers outside the rule)
  or make it the base rule's caller. Accept: `cargo test -p vam-spec`, every row of design §6.1,
  every name in `default_namespace_rules` gives the same value as today when it is a slug.
  Turns green: none alone.
- [ ] `UI-3.2` — Store (`users.rs`, `catalog*.rs` read only): column `namespace`, unique partial
  index `idx_users_namespace`, assignment inside the create transaction for `create_user*`,
  `bootstrap_first_admin` and seeds (decisions 18, 19), migration in `initialize_users_schema`
  (decision 20; never an `open` error, §4.2 row 27), log lines from `bin/vamd.rs`,
  `update_username` leaves it alone, `UserRecord.namespace`. Accept: unit tests (free, reserved,
  taken by a user, taken by another owner's catalog row, suffix lengths, idempotent migration,
  order rule, seed values); `tests/user_identity_migration_tests.rs` rows above; concurrent create
  API test. Turns green: with UI-3.3, all three new scenarios.
- [ ] `UI-3.3` — Readers: `api/me.rs` reports `UserRecord.namespace` (fallback to today's
  derivation for a principal with no row); `member_draft.rs` `require_own_namespace` reads the
  column on the locked connection; `UserAccount.namespace` on `GET /v1/users` (open question 2);
  proto comments on `GetMeResponse.default_namespace`. Accept: `tests/me_tests.rs`,
  `member_draft_agents_tests.rs`, `member_draft.rs` tests (free and taken names alike; rename
  keeps it). Turns green: `members-with-the-same-email-local-part-get-their-own-namespaces`,
  `members-whose-names-are-not-slugs-get-namespaces-of-their-own`,
  `member-namespaces-stay-apart-across-a-restart`.
- [ ] `UI-3.4` — Docs and contract: `orchestrator/member-agents.md` (drop "Known limitation" and
  its "Not proved here" line; "Saving" step 4 and "Building a client" point 3 say the namespace
  is the user's own and stable; a "Namespaces" section on assignment and migration, with the log
  line), `authoring/model.md` namespace paragraph, `orchestrator/user-isolation.md` `/v1/me` row,
  `getting-started/users-and-roles.md`, `docs/api/openapi.yaml` (`GetMeResponse.defaultNamespace`,
  `UserAccount.namespace`) and `crates/vam-spec/tests/openapi_yaml.rs`; `proves:` naming the three
  scenarios and the Rust tests; a `docs-features.yaml` row; `.cursor/workstreams/README.md`
  backlog row "Member namespace collisions" removed. Accept: `cargo test -p vam-spec`,
  `cargo run -p vam-e2e -- check`, `pnpm build` in `docs/` when available. Turns green: none
  (keeps all green).

**Waves**

| Wave | Items | Turns green | Repo | Model | Budget | Depends on | Actions | Status | PR |
|---|---|---|---|---|---|---|---|---|---|
| 0 docs | design §6 + this breakdown | — | vam | orchestrator | 30 min | — | none | [x] | this branch |
| 0 | `UI-3.0` | — (pending, red for the right reason) | vam | Sonnet 5.5 low (YAML and world pieces only) | 45 min | 0 docs | none | [x] | this branch |
| 1 | `UI-3.1`, `UI-3.2`, `UI-3.3` | the three new scenarios | vam | Sonnet 5.5 high (schema migration of existing users and the member write boundary) | 120 min | 0, owner approval, member-draft-agents released or synced | none | [ ] | |
| 2 | `UI-3.4` | — (keeps them green) | vam | Sonnet 5.5 medium (OpenAPI test in `vam-spec` plus docs) | 60 min | 1 | none | [ ] | |

**Open questions** (each with a default)

1. Does the suffix ever apply to a user whose base is free, so every namespace looks alike?
   Default: no; most users keep the readable value they have today.
2. Show `namespace` to admins on `UserAccount` (`GET /v1/users`)? Default: yes, read only; it is
   how an admin answers "where are this member's agents".
3. Should a publisher (`catalog:write`) be refused a new URN under another user's namespace?
   Default: not in this phase (design §6.3); listed under "Explicitly later".
4. Who keeps a shared namespace in the migration? Default: the user who owns the earliest catalog
   row under it, else the oldest user (decision 20). Alternative: nobody keeps it, everyone gets
   a suffix (simpler, but moves every colliding user, including the one whose drafts are there).
5. A log line per moved user, or also a notification to the user? Default: the log line only; the
   member sees the new value on `GET /v1/me`, and their old drafts keep working.

**Phase close:** the three new scenarios active and green, the changed one green; `--full` green
on the base; release with (or after) member-draft-agents; team checklist out.

**Team checklist — Phase 3** (on a node running the release, as an org admin)

1. Create `ann.lee@north.test` and `ann.lee@south.test` (members). **See:** both 200; as each,
   `GET /v1/me` `defaultNamespace` is `ann.lee` for the first and `ann.lee-<6 letters or digits>`
   for the second.
2. As the second, `PUT /v1/agents` with a new URN under the first one's namespace, then under a
   name the first one already saved. **See:** both 400 `... (your namespace)`, the same message.
3. Create `_x@example.test`. **See:** `defaultNamespace` is `x` (or `x-…`), not `default`.
4. Restart `vamd`, then `vamd --rename-user <id> other@example.test` for the second user and start
   again. **See:** each `defaultNamespace` unchanged.
5. On an upgraded node with users that shared a namespace before: start-up log. **See:** one
   `namespace for user … is now …` line per moved user; their old drafts still open and save.

Report: "Phase 3 pass", or the step number with the error text.

## Explicitly later

- Must-change-password-on-first-login.
- Email or SMTP reset; OIDC.
- Refusing a publisher a new URN under another user's namespace (Phase 3 open question 3).
- A user-facing rename of one's namespace.

## Progress log

| Date | Phase | What landed (end with the scoreboard: scenarios green, red, fix cycles, wall time) |
|---|---|---|
| 2026-11-12 | 1 | Tracker opened; design `design/user-identity.md`; Phase 1 plan (owner brief) |
| 2026-11-12 | 1 | W0 docs (#1) and W0 scenarios (#2) merged: harness steps `login`, `change-password`, `identity-request`; six scenarios pending, all red for the right reason (404 at the first change-password). Scoreboard: 0 green, 6 red (right reason), 0 fix cycles, about 75 min. Session note: the session runs as root, so `LOCAL_CI_OUT` must sit in a 0755 directory under a short path (a long path overflows the 107-byte socket limit; a 0700 `mktemp -d` dir breaks the 10 Microsoft 365 smoke scenarios: probe.py permission denied). Never two builds at once on the shared target. |
| 2026-11-12 | 1 | W1 server merged (#8): `POST /v1/me/password`, store `change_own_password`, per-user attempt limiter (reserve before verify, refund unless wrong; review found and fixed a check-then-charge race, concurrent API test proves 5 of 16), 14 API tests incl. operator. Orchestrator `--full` at head: all steps ok, 4458 tests, smoke 140/140. Scoreboard: 6 green, 0 red, 1 fix cycle (review), about 100 min. |
| 2026-11-12 | 1 | W2 merged (#9): `vam passwd` (hidden prompts, env flags, clears the stored session after 200), OpenAPI `/v1/me/password` + shape test, docs (users-and-roles who-can-change table, host API, CLI), feature row. `cli.md` stays in the docs baseline (shipping it needs proofs for 13 unrelated rows). Orchestrator `--full` at head: all steps ok, 4470 tests, smoke 140/140; `vam-e2e check` ok. **Phase 1 closed in base**: 6/6 scenarios active and green, 1 fix cycle in the phase, about 4.5 h wall. Open for the owner: merge `main` (4147270) into the base; Phase 2 decisions relayed by another session (`usr_` ids; migration refuses case-insensitive clashes) await confirmation. |
| 2026-11-13 | 1 | Owner: merge `main` (#10, conflict in design/README.md only; `--full` green, 4470 tests, smoke 140/140), keep the open-question defaults, add an audit event before release. W3 built: `vam.v1.user.password_changed` in partition `vam_audit_identity`, 2 API and 2 unit tests. Then the release PR into `main`. |
| 2026-11-13 | 1 | W3 merged (#11): audit event. Orchestrator `--full` at b507173: all steps ok, 4474 tests, smoke 140/140; `vam-e2e check` ok. Scoreboard: 6 green, 0 red, 0 fix cycles. Release PR into `main` opened on the owner's word. |
| 2026-11-14 | 2 | Decisions 15–16 recorded; audit closed (design §4.2: 2 security-floor findings, operator guard before resolution and the `bootstrap` principal match); Phase 2 plan (5 scenarios, 7 items, W0–W3). Scoreboard: 0 green, 0 red, 0 fix cycles. |
| 2026-11-14 | 2 | W0 merged (#14): harness steps `create-user`, `login case`, `identity-request` fields, `matches` matcher, `expectFail.step: given`; five scenarios pending. Scoreboard: 0 green, 5 red (right reason), 0 fix cycles. |
| 2026-11-14 | 2 | W1 merged (#15): `usr_` ids, login-name validator, NOCASE unique index, start-up refusal on case clashes with `vamd --rename-user`, one resolution rule, operator guard after resolution. `--full` green. Scoreboard: 4 green, 1 red (right reason, namespace at `then.0`), 0 fix cycles. |
| 2026-11-14 | 2 | Owner: Q8 yes, Q1–7 defaults. UI-2.5 built from `main` as hotfix #16 (operator only for key id `bootstrap`, `bootstrap` reserved, upgrade revokes keys the old backfill made operator-owned); `--full --release` and all 9 Actions green; awaits the owner's merge. W2 dropped. |
| 2026-11-14 | 2 | W3 (#17): email-username namespace, OpenAPI, docs, feature row; email scenario active. Scoreboard: 5 green, 0 red, 0 fix cycles. Note: an agent's wait-for-free-build loop matched its own command line and stalled about 2 h; the wait script must not run in the same shell line as `cargo`. |
| 2026-11-17 | 2 | Owner merged hotfix #16 into `main` (a8cf6ab). `main` merged back (#18): one conflict in `update_username` (kept the base validator); the hotfix's reserved check removed from `validate_user_id` where the merge put it; new test for a first admin with a legacy `usr_` name. `--full` green, 4532 tests, smoke 145/145. **Phase 2 closed in base**: 5/5 scenarios active and green, 0 fix cycles in the phase. Release PR into `main` next. |
| 2026-11-18 | 3 | Phase 3 opened (one namespace per user, from the member-draft-agents backlog row): design §6, breakdown, proposed decisions 17–21. Branch merges `feat/member-draft-agents` into `main` (41af8b0; 16 hunks in `crates/vam-e2e` and four docs pages; `vam-e2e` tests and `check` green). W0: five world pieces, three new scenarios pending, one `then` changed (spec change for the owner). `run.sh --scenarios` six: 3 red (right reason: 403 `only the artifact owner may version this agent` at the `urnOf` step), 3 pass. Scoreboard: 0 green, 3 red (right reason), 0 fix cycles, about 2 h (disk allowance ran out twice; a full `cargo test -p vam-orchestrator` and the e2e build do not fit together in `target/debug`). |
