# Workstream: User identity (self-service password, then opaque user IDs)

```yaml
status: active
slug: user-identity
current_phase: 2 planned 2026-11-14
base_branch: feat/user-identity-phase2
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
PR into it.  
**Exit:** the five phase 2 scenarios are `active` and green; the existing-user and clash tests
below pass; `scripts/ci/local.sh --full` green on the base; OpenAPI and docs say how ids and
login names work.

- [x] Audit of username use outside `users` (design §4.2, closed 2026-11-14)
- [x] Scenarios and harness steps land pending, red for the right reason (W0, #14)
- [x] Generated ids for new users; `CreateUserRequest.user_id` no longer client-chosen (W1, #15)
- [x] Login-name validation and case-insensitive uniqueness, with migration (W1, #15)
- [x] One resolution rule; operator guard after resolution (W1, #15)
- [~] Session principal from the user id; no name maps to the bootstrap key (W2 replaced by hotfix #16 into `main`, owner 2026-11-14; reaches the base when `main` is merged back)
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
- [~] `UI-2.5` (hotfix #16 into `main`) — Principal and bootstrap match (found by the audit, rows 2 and 3). Today a session
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

## Explicitly later

- Must-change-password-on-first-login.
- Email or SMTP reset; OIDC.

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
