# User identity: self-service password change, then opaque user IDs

Status: **plan of record, 2026-11-12** (owner decisions in the phase 1 brief).
Tracker: `.cursor/workstreams/user-identity.md`. Base branch: `feat/user-identity`.

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

## 4. Phase 2 — generated user IDs and email usernames (planned)

Not built in phase 1. Recorded so phase 1 does not paint it into a corner.

### 4.1 Model

- **Existing users keep their `user_id`.** No row is rewritten; every table that
  references a user keeps working.
- **New users get an opaque generated `user_id`** (`usr_` plus a random
  base32 or ULID; decided in phase 2's breakdown). `CreateUserRequest.user_id`
  stops being accepted from clients (today `users_admin.rs:70` falls back to the
  username, which is why IDs equal names).
- **`username` is a login name**: validation allows `@` and `+` (email shape
  allowed, not required), stored as typed, unique and matched
  **case-insensitively** (a normalized `username_key` column with a UNIQUE index,
  or `COLLATE NOCASE`; migration refuses or reports existing case-collisions).
- Login, `resolve_user` and admin lookups match on the normalized form.
- Renames (`update_username`, `users.rs`) change the login name only.

### 4.2 Audit: where `username` is used outside `users` (phase 2, first item)

The risk is code that treats the username as an identifier because today
`user_id == username` for users the API created. Audit list to work through:

| Use | Where (start) | Question |
|---|---|---|
| Default catalog namespace from the username | `crates/vam-spec/src/origin.rs:166` (`default_namespace`), `api/me.rs` | `@`, `+` and case in a namespace and URN; derive from a slug, keep for existing users |
| Catalog tiers and ownership | `crates/vam-orchestrator/src/catalog_tier.rs` | keyed by name or id? |
| Marketplace owner rows `marketplace:<id>` | `users.rs` (`list_users`), `marketplace/url.rs` | collision with generated ids |
| Notifications, contacts, node notify | `node_notify.rs` | display vs identity |
| Web admission and sources, MCP OAuth refresh | `web_admission.rs`, `web_sources.rs`, `mcp_oauth_refresh.rs` | name used as a key? |
| Process mode, `vamd` bootstrap and reset flags | `process_mode.rs`, `bin/vamd.rs` | `--reset-password <user>` resolves id then name |
| Network broker | `crates/vam-network/src/broker.rs` | name in paths or headers |
| CLI | `crates/vam-cli/src/cmd/login.rs`, `cmd/users.rs` | display only |
| Owner stamping (`owner_user_id`, about 800 references) | `crates/vam-orchestrator/src/owner.rs` and callers | must stay `user_id`; grep for any `username` passed where an owner id is expected |
| Workspace and Drive9 paths, receipts, telemetry | `vam-storage`, receipts, `vam-telemetry` | ids in paths must stay path-safe (no `@`); confirm they use `user_id` |
| E2E world pieces | `crates/vam-e2e/src/engine.rs:670` | runner reads `userId` from the response already |

Each row ends as "uses `user_id` (fine)", "display only (fine)" or a fix item.

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
