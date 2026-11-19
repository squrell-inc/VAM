# User identity: self-service password change, then opaque user IDs

Status: **plan of record, 2026-11-12** (owner decisions in the phase 1 brief); phase 2
breakdown 2026-11-14 (owner decisions 15–16); open questions on defaults until answered.
Tracker: `.cursor/workstreams/user-identity.md`. Base branch: `feat/user-identity` (phase 1),
`feat/user-identity-phase2` (phase 2).

Read only the section for the phase in hand.

---

## 1. Goal

A person who can log in to a VAM node manages their own credentials without an
admin, and the node's identity model stops tying a person's permanent ID to a
name they typed once.

1. **Phase 1 — self-service password change.** Any user with a password session
   changes their own password (`POST /v1/me/password`, `vam passwd`). Today only
   an org admin (`POST /v1/users/{id}/password`) or the host shell
   (`vamd --reset-password`) can, and the operator cannot change their own
   password over HTTP at all.
2. **Phase 2 — generated user IDs and email usernames.** New users get an opaque
   generated `user_id`; `username` becomes a login name that may be an email
   address (allows `@` and `+`) and is matched case-insensitively. Existing users
   keep their IDs.

Out of scope for the workstream: must-change-password-on-first-login (later),
any email or SMTP reset (no mail transport exists or is planned here), OIDC.

## 2. What exists today (evidence)

| Fact | Where |
|---|---|
| `users` table: `user_id` PK, `username` UNIQUE, `tenant_id`, `password_hash` (argon2id PHC), `password_updated_at`, `role`, `deactivated_at` | `crates/vam-orchestrator/src/users.rs:170` (`initialize_users_schema`) |
| Hashing and verify: argon2id m=19 MiB t=2 p=1, `validate_password` = non-empty and at most 1024 bytes | `crates/vam-orchestrator/src/password.rs:24`, `:56`, `:84`, `:128` |
| Store reset: replaces hash and `password_updated_at`, then `revoke_sessions_for_user`; API keys untouched | `users.rs:564` (`reset_user_password`) |
| Org-admin reset refuses `operator`, looks up by `user_id` only | `users.rs:594` (`admin_reset_user_password`), `users.rs:150` (`reject_operator_user_admin_target`) |
| Verify against the stored hash (`Ok(false)` when no hash) | `users.rs:606` (`verify_user_password`) |
| Routes `POST /v1/login`, `/v1/login/refresh`, `/v1/logout`, `GET /v1/me`, `POST /v1/users/:id/password` | `crates/vam-orchestrator/src/api/mod.rs:122-136` |
| Admin reset handler: `pdp.authorize(.., "ResetUserPassword", users:admin)`, `require_idempotency_key`, `parse_json_body("vam.v1.ResetUserPasswordRequest")` | `api/users_admin.rs:124` |
| Login: wrong secret, unknown user and password-less rows are all 401 | `crates/vam-orchestrator/src/session_tokens.rs:109` |
| Session revoke: whole user (`revoke_sessions_for_user`), one token (`revoke_session_by_token_id`, used by logout) | `session_tokens.rs:211`, `:225`; `api/login.rs:84` |
| Caller kind: `AuthContext.key.source` is `AuthSource::Session` (a `vam_st_` login token) or `AuthSource::ApiKey` (every key, including the bootstrap/master key) | `crates/vam-orchestrator/src/keys.rs:204`, `api/auth.rs:27` |
| Idempotency key: header required, value not stored or replayed by the user routes | `api/auth.rs:272`; `users_admin.rs` binds it to `_idempotency` |
| Login rate limit: per peer IP token bucket (5/s, burst 20), charged by every `/v1/login*` call and every unauthenticated 401/403; authenticated requests are charged only to the per-key bucket (30/s, burst 100) | `api/rate_limit.rs:56`, `:319`, `:376-399` |
| Errors: `PermissionDenied` → 403 `PERMISSION_DENIED`, `Unauthenticated` → 401, `InvalidContract` → 400 `INVALID_ARGUMENT`; 429 bodies carry `X-VAM-Rate-Limited-Reason` | `api/error.rs:36-90`, `rate_limit.rs:327` |
| Proto pattern: `ResetUserPasswordRequest { password }`, `ResetUserPasswordResponse { UserAccount user }`, rpc on the control plane service | `proto/vam/v1/orchestrator.proto:620-626`, `:1138` |
| OpenAPI: hand-written `docs/api/openapi.yaml`, served at `/openapi.yaml`; shape test `crates/vam-spec/tests/openapi_yaml.rs` | `docs/api/openapi.yaml:400` (admin reset) |
| CLI: `vam login` (`--username`, `--password-from-env <VAR>`, else an interactive prompt that **echoes**), stores `session_token` in `~/.config/vam/config.toml`; `vam logout` clears it | `crates/vam-cli/src/cmd/login.rs`, `crates/vam-cli/src/config.rs:198` (`prompt`), `:77`, `:83` |
| E2E: an `org` piece with role Member creates a user with a random password and mints one key; the password is not kept, and no step logs in | `crates/vam-e2e/src/engine.rs:650-712` |

## 3. Phase 1 — self-service password change

### 3.1 Contract

`POST /v1/me/password`, header `Idempotency-Key` required, bearer a password
session token.

```json
{ "currentPassword": "old secret", "newPassword": "new secret" }
```

Proto (`proto/vam/v1/orchestrator.proto`, next to the admin reset):

```proto
// Self-service password change. Only a password session may call it; API keys are refused.
message ChangeMyPasswordRequest {
  string current_password = 1;
  string new_password = 2;
}
message ChangeMyPasswordResponse {
  UserAccount user = 1;
  // Always true: every session of the user, this one included, was revoked.
  bool relogin_required = 2;
  int64 password_updated_at = 3;
}
rpc ChangeMyPassword(ChangeMyPasswordRequest) returns (ChangeMyPasswordResponse);
```

| Case | Status | `code` | `message` |
|---|---|---|---|
| Changed | 200 | — | body above, `reloginRequired: true` |
| No or invalid bearer, expired or revoked session, deactivated user | 401 | `UNAUTHENTICATED` | `missing or invalid bearer token` (unchanged) |
| Bearer is an API key (any key, the master key included) | 403 | `PERMISSION_DENIED` | `changing a password needs a password session (vam login); API keys cannot change a password` |
| `currentPassword` does not verify | 403 | `PERMISSION_DENIED` | `current password is incorrect` |
| Too many wrong `currentPassword` for this user | 429 | `RESOURCE_EXHAUSTED` | `too many wrong password attempts; retry after Ns`, `Retry-After`, `X-VAM-Rate-Limited-Reason: password-attempt-rate` |
| Missing `Idempotency-Key` | 400 | `INVALID_ARGUMENT` | existing message |
| Malformed body, `currentPassword` empty | 400 | `INVALID_ARGUMENT` | `currentPassword is required` |
| `newPassword` empty or over 1024 bytes | 400 | `INVALID_ARGUMENT` | the `validate_password` messages |
| `newPassword` equals the current password | 400 | `INVALID_ARGUMENT` | `new password must differ from the current password` |

### 3.2 Handler order (`crates/vam-orchestrator/src/api/me.rs`, new `change_my_password`)

1. `ensure_ready(&state, true)` (draining node refuses mutations).
2. `auth.key.source != AuthSource::Session` → 403 (the API-key message). The
   check is on the credential kind, not the role, so it holds for every role.
3. `require_idempotency_key`.
4. Parse `vam.v1.ChangeMyPasswordRequest`; `currentPassword` empty → 400;
   `newPassword` checked with the same rules as the admin reset (the shared
   `validate_password`, made `pub(crate)`) → 400. These checks need no secret, so
   they run before any verify and cost no attempt.
5. Per-user attempt limiter (§3.3): out of tokens → 429, without verifying.
6. Store `change_own_password(user_id, current, new)`:
   - verify `current` against the stored hash; mismatch → charge the limiter,
     403 `current password is incorrect`;
   - verify `new` against the **same** stored hash; match → 400 (only reachable
     after the current password verified, so it reveals nothing);
   - then the existing `reset_user_password` path: new argon2id hash,
     `password_updated_at = now`, `revoke_sessions_for_user` (all session and
     refresh tokens, the caller's included), API keys untouched.
7. 200 with the user, `reloginRequired: true`, `passwordUpdatedAt`.

No `users:admin` scope and no operator refusal: the subject is always the
caller's own `owner_user_id()`, never a path parameter, so it works for every
role including `operator` (decision 6). No PDP action is needed for the same
reason `GET /v1/me` has none; a Cedar action can be added later if policy wants
to forbid it.

The store method lives in `users.rs` beside `reset_user_password` and reuses it,
so hashing, timestamping and revocation stay in one place. The admin reset is
unchanged.

### 3.3 Wrong-attempt limit

The existing login limit is per IP and only charges unauthenticated requests;
this route is authenticated, so it would only see the per-key bucket (100
burst), which is far too generous for guessing. So phase 1 adds an equivalent
**per-user** bucket for wrong current passwords:

- keyed by `user_id`, in memory, per node (resets on restart, like the other
  buckets);
- burst **5**, refill **1 per 60 s**;
- charged only by a wrong `currentPassword`; checked before every verify, so a
  user who used up the bucket gets 429 even with the right password until a
  token refills (a stolen session cannot keep guessing);
- **always on**, also when `VAMD_RATE_LIMIT=off`: it is an authentication
  control, not flood control;
- lives next to the other buckets in `api/rate_limit.rs` (same `Bucket` type,
  reason constant `REASON_PASSWORD_ATTEMPT = "password-attempt-rate"`), held where
  the handler can reach it (a field on `AppState` or a node-wide instance; the
  wave picks, unit tests use their own instance).

### 3.4 CLI: `vam passwd`

- Prompts `Current password:` and `New password:` then `Repeat new password:`
  **without echo** (`rpassword` crate in `vam-cli`; refuses when stdin is not a
  terminal, as `config::prompt` does, naming the flags).
- Non-interactive, consistent with `vam login`: `--current-password-from-env
  <VAR>` and `--new-password-from-env <VAR>`. Secrets are never taken from argv.
- Uses the stored session token (the same resolution as other commands). An API
  key in `--token` / `VAM_API_KEY` gets the server's 403 text.
- Sends a fresh `Idempotency-Key`.
- On 200: clears the stored session (`ConfigFile::clear_session`, as `vam
  logout` does), prints `password changed; every session was signed out. Run
  vam login.` (JSON: `{"passwordChanged": true, "reloginRequired": true}`).
- Wrong current password, rate limit and validation errors print the server
  message with the CLI's usual exit codes (`http_error.rs`).

### 3.5 Docs

- `docs/api/openapi.yaml`: path `/v1/me/password`, schemas
  `ChangeMyPasswordRequest` and `ChangeMyPasswordResponse`, every status in
  §3.1; `crates/vam-spec/tests/openapi_yaml.rs` asserts the path and schemas.
- `docs/src/content/docs/getting-started/users-and-roles.md`: a table of what a
  user can do for themselves vs what an org admin and the operator can do
  (change own password: everyone with a password session; reset someone
  else's: org admin, never the operator; reset any including the operator: the
  host shell `vamd --reset-password`).
- `docs/src/content/docs/orchestrator/api.md`: the route, auth rule, the
  attempt limit, the re-login rule. `getting-started/cli.md`: `vam passwd`.
- Security sentences name their proof (the phase 1 scenarios), per
  `e2e/README.md`.

### 3.6 Scenarios and harness

The runner cannot log in today (§2, last row), so wave 0 adds harness steps to
`crates/vam-e2e` (test code, no product change):

- an `org` piece keeps its generated password (in memory, never printed);
- `login: { as, password: current | previous, refused }` — `POST /v1/login` with
  the identity's username; on success the session token becomes the identity's
  current session (the previous one is kept as `previous-session`);
- `change-password: { as, via: session | key, current: right | wrong, new: fresh
  | same, idempotencyKey: true, status, contains }` — `POST /v1/me/password`; a
  200 moves `current` to `previous` and a fresh secret to `current`;
- `identity-request: { as, credential: session | previous-session | key,
  method, path, status }` — one request with that credential, judged by status.

Scenarios (all `needs: process`, tag `members`, no mock model):

| Scenario | Proves |
|---|---|
| `member-changes-own-password-and-signs-in-with-the-new-one` | 200, `reloginRequired`; old password 401 at login, new one logs in |
| `password-change-signs-out-every-session-and-keeps-api-keys` | two sessions both 401 after the change, the identity's API key still 200 |
| `wrong-current-password-is-refused-and-attempts-are-limited` | wrong current 403 with the fixed text, password unchanged; after 5 wrong, 429 even with the right one |
| `api-key-cannot-change-a-password` | 403 for an API key (member key and bootstrap key), password unchanged |
| `new-password-equal-to-current-is-refused` | 400, password unchanged, sessions still valid |
| `password-change-needs-an-idempotency-key` | 400 without the header, nothing changed |
Unauthenticated (401) and the operator case are asserted in
`crates/vam-orchestrator/tests/api_tests.rs`, the cheapest layer: the runner's
operator is the bootstrap key and the operator row has no password until
`vamd --reset-password`, so a scenario would need a host-shell step for no extra
proof.

### 3.7 Audit (owner, 2026-11-13)

A successful change appends `vam.v1.user.password_changed` to the audit ledger, node-level
partition `vam_audit_identity` of the current tenant, attributed to the user (store
`record_identity_audit_event`, `crates/vam-orchestrator/src/workflow.rs`). Payload: `userId`,
`username`, `role`, `actor: "self"`, `credential: "session"`, `passwordUpdatedAt`; no password,
hash or token (a typed struct in `api/me.rs`). Refusals record nothing. The ledger keeps a digest of
the payload and persists the record to the spool like other audit records. A failed append after the
change is logged at error level and the request still answers 200, because the password is already
changed and the sessions revoked; this is the one place the change does not fail closed on the
ledger. Admin reset and `vamd --reset-password` are not audited yet.

## 4. Phase 2 — generated user IDs and email usernames (breakdown 2026-11-14)

Owner decisions 10, 15 and 16 (tracker). The breakdown's proposals below are built under the
owner's brief of 2026-11-14; open questions and defaults are in the tracker's Phase 2 section.

### 4.1 Model and decisions

- **Existing users keep their `user_id`** (decision 10). No row is rewritten; every table that
  references a user keeps working.
- **New users get a generated `user_id`** (decision 15): `usr_` plus 26 characters `[a-z2-7]`,
  128 random bits from `OsRng`, minted by the existing `spaces::mint_id`
  (`crates/vam-orchestrator/src/spaces.rs:33`, the `sp_`/`th_`/`nt_`/`ct_` minter). No new crate
  and no timestamp in the id (a ULID would leak the creation time and add a dependency). Lowercase,
  URL-safe and path-safe: it passes the workspace owner-prefix check
  (`crates/vam-storage/src/owner_prefix.rs:222`, `[A-Za-z0-9._-]`, no `--`). A primary-key clash
  retries with a fresh id. `vamd --bootstrap-admin` mints one too.
- **`CreateUserRequest.user_id`**: a non-empty value is refused, 400 `userId is generated by the
  node; leave it empty`; the proto field number is kept and marked deprecated. Nobody sends it
  today (`crates/vam-cli/src/cmd/users.rs:34-39` uses `..Default::default()`;
  `crates/vam-e2e/src/engine.rs:676-681` and the API tests omit it), so refusing breaks no caller,
  and it fails loudly for a client that thinks it chose an id. Ignoring it silently would let such
  a client act on an id that does not exist. No operator path is kept: seed rows are made
  in-process (`users.rs:428`), and store methods with an explicit id stay for seeds and tests.
- **`username` is a login name** (decision 16): ASCII letters, digits, `.`, `-`, `_`, `+`, `@`, at
  most one `@`; email shape allowed, not required; stored as typed; unique and matched ignoring
  ASCII case. Non-ASCII stays refused, as today (`users.rs:766-787`). A new or renamed username
  may not start with `usr_`, may not equal another user's id ignoring case, and may not be
  `bootstrap` (`operator` and `local` are already taken by seed rows).
- **Index**: `CREATE UNIQUE INDEX idx_users_username_nocase ON users(username COLLATE NOCASE)`.
  NOCASE folds ASCII only, which is complete for an ASCII charset. Chosen over a normalized
  `username_key` column: no new column, no backfill, no second write to keep in step.
- **Migration refuses on clashes** (decision 16, §4.3); the admin renames with an offline flag.
- **Resolution** is one rule (§4.4). Renames (`update_username`) change the login name only, and
  now validate.
- **Namespace**: today it derives from the username, never the id (§4.2 row 1); for an email
  username the default becomes its local part (tracker open question 1).
- **Principal**: a session's Cedar principal becomes `User::"<user_id>"` instead of the username,
  and no name can map a caller to the bootstrap key (§4.2 rows 2–3).

### 4.2 Audit: username use outside `users` (closed 2026-11-14)

The risk is code that treats the username as an identifier because today `user_id == username`
for users the API created (`api/users_admin.rs:69-77`). Every row is closed. Paths are under
`crates/vam-orchestrator/src/` unless named.

| # | Use | Keys on | Evidence | New vs existing | Verdict |
|---|---|---|---|---|---|
| 1 | Default catalog namespace (`GET /v1/me` `defaultNamespace`) | username, lowercased; `default` when not a slug. Never the id. A default, not enforced | `crates/vam-spec/src/origin.rs:166-173`; `api/me.rs:34-47`; `docs/src/content/docs/authoring/model.md:50` | Existing: unchanged (no `@`/`+` today). New plain names: unchanged, `usr_` never leaks. New email names: all would get `default` | fix `UI-2.6` |
| 2 | Session Cedar principal | username: `User::"<username>"` | `session_tokens.rs:361`; matched by `owner.rs:29-51` (`User::"bootstrap"` → operator) | Existing (id = name): same string after the fix. New: stable across renames, no `@`. Today a user named `bootstrap` owns as the operator | fix `UI-2.5` |
| 3 | API key principal (found by the audit) | key name or client-sent principal, not the user | `api/users_keys_admin.rs:92-96`; `api/mod.rs:429-433`; `owner.rs:49-51`; start-up backfill `owner.rs:99-103` | An org admin can mint a key named `bootstrap` for a member; it then owns as the operator | fix `UI-2.5` |
| 4 | Cedar policies | `Agent::` principals only; no shipped policy names a `User::` | `examples/*/policy.cedar`; `security.rs:1707` records the principal in the ledger | No change | uses no user name (fine) |
| 5 | Login | `resolve_user`: exact id, then exact (case-sensitive) username | `session_tokens.rs:114`; `users.rs:482-501`, `:557-564` | New and existing: any letter case after the fix; the response keeps the stored name (`api/login.rs:44-52`) | fix `UI-2.4` |
| 6 | `/v1/users/{x}/keys*` and `POST /v1/keys` `userId` | `resolve_user`; operator guard on the **raw** path before resolution | `api/users_keys_admin.rs:26-37`, `:53-54`; `api/mod.rs:414-419`; `users.rs:152` | Once names fold case, `/v1/users/OPERATOR/keys` would resolve to the operator past the guard | fix `UI-2.4` |
| 7 | Other `/v1/users/{id}/…` routes, catalog reassign | `user_id` only (`get_user`) | `users.rs:344` (deactivate), `:637-648` (admin reset), `:405-425` (publish grant), `owner.rs:421` | `usr_` ids in URLs | uses `user_id` (fine) |
| 8 | Create user | id falls back to the username | `api/users_admin.rs:69-77`; `users.rs:303-339`; `proto/vam/v1/orchestrator.proto:593-598`; `docs/api/openapi.yaml:4167-4169` | New: generated. Existing: unchanged | fix `UI-2.2` |
| 9 | First-admin bootstrap | id = username | `users.rs:243-270` (`:262`); `password.rs:118-125`; `bin/vamd.rs:382-438` | New admin gets `usr_`; idempotent by name | fix `UI-2.2` |
| 10 | Uniqueness and charset | `username TEXT NOT NULL UNIQUE` (case-sensitive); ASCII `[A-Za-z0-9._-]`, at most 64 | `users.rs:179-191`, `:766-787` | Needs `@`, `+` and a case-insensitive index | fix `UI-2.3` |
| 11 | Rename | `update_username`: no charset check, no route, tests only | `users.rs:519-545`, `:903` | Becomes the offline fix for clashes | fix `UI-2.3` |
| 12 | Catalog tiers and ownership, owner stamps | `owner_user_id` FK to `users(user_id)`, from the key or session record | `catalog_tier.rs:479`; `owner.rs:29-37`, `:53-110`; `api/auth.rs:35-37`; no `username` passed as an owner (grep) | New owners are `usr_…` | uses `user_id` (fine), except rows 2–3 |
| 13 | Marketplace owner rows `marketplace:<id>` | a user row whose id and username are `marketplace:<id>` | `marketplace/installs.rs:22`, `:366-378`; `users.rs:510`; `owner.rs:416`; ids are `[a-z0-9-]` (`marketplace/fetch.rs:155`) | No collision: generated ids start `usr_`, login names cannot hold `:`, lowercase ids cannot clash ignoring case | fine |
| 14 | Notifications "decided by", notify delivery | `user_id`; username shown | `notify_gateway.rs:291-298`; `notify_delivery.rs:671-678`; `brokers/vam-notify/src/hub.rs:452-457` | An email may appear in a channel message | display only (fine) |
| 15 | Contacts, node notify settings | `contacts.user_id`; settings are node-wide | `contacts.rs:173-186`; `node_notify.rs:154` | No change | uses `user_id` (fine) |
| 16 | Web admission and sources, MCP OAuth refresh, node notify URL, marketplace URL, process mode, network broker, `vam.web` | `url.username()` (URL userinfo), not a VAM user | `web_admission.rs:178`; `web_sources.rs:131`; `mcp_oauth_refresh.rs:229`, `:290`, `:644`; `node_notify.rs:119`; `marketplace/url.rs:135`; `process_mode.rs:115`; `crates/vam-network/src/broker.rs:325`; `brokers/vam-web/src/fetch/rules.rs:41` | No change | not user identity (fine) |
| 17 | `vamd --reset-password <user>` | `resolve_user` | `bin/vamd.rs:353-377`; `password.rs:109-115` | Takes an id or a login name in any case after `UI-2.4` | follows the rule (fine) |
| 18 | Workspace and Drive9 paths | `owner_user_id` prefix `{owner}--…`, checked path-safe | `crates/vam-storage/src/owner_prefix.rs:27-31`, `:211-240`; `run_context.rs:112` | `usr_` passes; an email would fail, and is never used | uses `user_id` (fine) |
| 19 | Receipts | `decided_by_user_id`, `requested_by_user_id` | `receipt.rs:609`, `:871` | No change | uses `user_id` (fine) |
| 20 | Telemetry | `vam.user.id` stamped from the run owner; guest value stripped | `crates/vam-telemetry/src/guest_ingress.rs:114-118`, `:197-200` | No change | uses `user_id` (fine) |
| 21 | HSP caller context, brokers | `user_id` from the run or session owner | `caller_context.rs:91-105`; `brokers/vam-schedule/src/broker.rs:69` | No change | uses `user_id` (fine) |
| 22 | Identity audit payload | `userId` plus `username` | `api/me.rs:60-69`, `:134` | An email may appear in the ledger | display only (fine) |
| 23 | `users.tenant_id` | mirrors the id at insert; not read as a key | `users.rs:287-289`, `:439-444` | New: `usr_…` | fine |
| 24 | CLI | sends the username to `/v1/login`; shows it; `keys create --user` takes an id or a name; no local validation | `crates/vam-cli/src/cmd/login.rs`; `client.rs:332-346`; `cmd/users.rs:34-39`; `cmd/keys.rs:15` | No code change | display only (fine); help text in `UI-2.7` |
| 25 | E2E harness | reads `userId` from the create response, falls back to the username; stub answers id = username | `crates/vam-e2e/src/engine.rs:674-724` (`:690-694`); `identity.rs:197`, `:224-231`; `crates/vam-e2e/tests/support/mod.rs:334-337` | The fallback would hide a missing id | fix `UI-2.1` |
| 26 | Tests reading a created user by name as id | `store.get_user("carol")` after `POST /v1/users` | `crates/vam-orchestrator/tests/api_tests.rs:7060` | Breaks with generated ids | fix in `UI-2.2` (tests only) |
| 27 | `vamd` store open | an `open` error falls back to an in-memory store | `bin/vamd.rs:329-343`; `workflow.rs:636` | A clash raised from `open` would start an empty node | constraint on `UI-2.3` (§4.3) |

### 4.3 Migration

Runs in `initialize_users_schema` on every open; idempotent.

1. If `idx_users_username_nocase` exists, nothing to do.
2. Else find clash groups: `SELECT group_concat(user_id || ' (' || username || ')', ', ') FROM
   users GROUP BY username COLLATE NOCASE HAVING COUNT(*) > 1`.
3. None: create the index. Some: create nothing, keep the groups on the store, and return
   `Ok` (an `open` error would make `vamd` serve an empty in-memory store, §4.2 row 27).
4. `vamd`, after `--reset-password` and `--rename-user` (both work on a clashing database) and
   before `--bootstrap-admin`, `--init` and serving, exits 1 when groups exist:

   ```text
   vamd: usernames must be unique ignoring letter case; these clash:
     bob (user id bob), Bob (user id bob-2)
   Rename all but one of each group, then start vamd again:
     vamd --rename-user <user id> <new username>
   ```
5. `vamd --rename-user <user_id> <new_username>` (new, offline, in the style of
   `--reset-password`; tracker open question 3): validates the new name with the login-name rules,
   refuses a name that clashes, prints old and new, exits. The id and every owned row stay.

Proof: `crates/vam-orchestrator/tests/user_identity_migration_tests.rs` (old-schema database,
existing user unchanged) and `tests/vamd_username_clash.rs` (the real binary refuses, names the
clash, and starts after the rename).

### 4.4 Resolution

One rule wherever a user is looked up by an id-or-name today (login, `/v1/users/{x}/keys*`,
`POST /v1/keys` `userId`, `vamd --reset-password`):

1. An exact, case-sensitive `user_id` match wins.
2. Else `username = ? COLLATE NOCASE` (one row at most, by the index).
3. Creation and rename rules (§4.1) keep 1 and 2 apart for every new row. A legacy username equal
   to another user's id loses to the id; `vamd` logs it at start (tracker open question 4).
4. Guards that protect the operator check the resolved row's `user_id`, never the raw input.

Routes that take only an id today keep taking only an id.

## 5. Rejected options

| Option | Why not |
|---|---|
| `PUT /v1/users/{me}/password` or reuse `/v1/users/{id}/password` with an ownership check | Mixes the admin route (no current password, cannot target operator) with the self route; one path per rule is easier to reason about and to rate-limit |
| Keep the current session alive after a change (revoke only the others) | Owner decision 5: all sessions go, the user logs in again; one rule for clients |
| Revoke API keys too | Owner decision 5; keys are a separate credential with their own revoke, as the admin reset already does |
| Let API keys change the password | A leaked CI key would become a full account takeover; owner decision 2 |
| Return 401 for a wrong current password | Clients treat 401 as "session gone, log in again"; the session is fine, the input is wrong |
| Rely on the per-IP login bucket | It does not charge authenticated requests (`rate_limit.rs:393`); the per-key bucket allows 100 quick guesses |
| Password strength rules (length, classes, breach list) | Owner decision 4: same rules as the admin reset; a policy change is its own decision |
| Store and replay the idempotency key | No user route does today; a replay after success is refused anyway (session revoked → 401) |
| Echoing prompt (`config::prompt`) for `vam passwd` | Owner decision 8: no echo |
| Ignore a client-sent `userId` silently | A client that believes it chose an id would act on one that does not exist; refusing breaks no caller today (§4.1) |
| ULID ids | Leak the creation time and add a crate; `spaces::mint_id` already mints 128-bit lowercase ids |
| A normalized `username_key` column | Needs a backfill and a second write on every create and rename; a `COLLATE NOCASE` index does the same for an ASCII charset |
| Rename clashing usernames automatically | Owner decision 16 |
| Fail `SqliteWorkflowStore::open` on a clash | `vamd` would fall back to an empty in-memory store (`bin/vamd.rs:329-343`) |
| Derive the namespace from the generated id | Puts `usr_…` into user-visible URNs |
