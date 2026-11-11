# App & Subject identity for VAM

> **Superseded model.** Sections 4–7 and 10 are replaced by
> [`vam-apps-spaces.md`](vam-apps-spaces.md) (Subject becomes Space, a place
> rather than an identity; one Credential type). The prior-art research in
> §3, §8, §9, and §12 still stands and is referenced from there.

## 1. What we are building

VAM's identity model today has exactly one caller shape: a `User` — a
human or a service holding one persistent API key, who owns one or more
long-lived `owner_user_id--workspace_suffix` workspaces. That model is
correct for the CLI, for a single operator running their own agents, and
for a small team behind one deployment.

It breaks the moment a `User`'s API key is used to serve **many independent
end users of that user's own application** — for example, a website that
lets each visitor talk to an agent. This document proposes two new
first-class identities, **App** and **Subject**, and the workspace,
Cedar, and lifecycle changes needed to support them, without disturbing
the existing `User`/workspace model for anyone who doesn't need them.

This is written to be explored and built directly against `vamd`
(`crates/vam-orchestrator`) and `vam-security`. It is not specific to any
one consumer of VAM — the motivating case is the Mast web framework
(see `design/mast-framework.md`), but nothing here is Mast-specific; any
multi-tenant product built on VAM hits the same wall.

**This revision** replaces the earlier draft's three-state `SubjectKind`
and 24h/1h TTL guesses with a design validated against seven comparable
platforms (E2B, Modal, Vercel Sandbox, Daytona, Fly.io Machines, Cloudflare
Workers for Platforms / Durable Objects, and the B2B2C identity layer of
Stripe Connect, Twilio, Clerk, WorkOS, and LiveKit). Where this doc departs
from or agrees with that prior art, it says so, with citations, so the
reasoning survives the decision.

## 2. Why this is blocking, not cosmetic

Three concrete facts about the current implementation, all confirmed
against code:

**Fact 1 — one workspace, one writer, always.** A workspace ID is formed by
`apply_owner_prefix(owner_user_id, suffix)` as `"{owner}--{suffix}"`
(`crates/vam-storage/src/owner_prefix.rs`), and a workspace is guarded by a
single `write.lock` file naming the owning session
(`crates/vam-storage/src/workspace.rs`, `WorkspaceManager::acquire_lock`).
If a session omits a `workspace_id`, it defaults to `"assistant"`
(`DEFAULT_WORKSPACE_ID`, `crates/vam-orchestrator/src/run_context.rs`). A
website that authenticates every visitor with the same developer API key,
and doesn't independently vary `workspace_id`, resolves every single
visitor to `webapp--assistant` — **one workspace, one lock, one visitor at a
time, globally, for the entire site.**

**Fact 2 — one Cedar principal, full blast radius.** `crates/vam-security/schema/vam.cedarschema`
declares a flat entity set (`Agent`, `User`, `Tool`, `Host`, `Path`,
`Workspace`) with no `in` / `memberOfTypes` relationships at all. Every
policy decision for every visitor's agent evaluates as
`principal == Vam::User::"webapp-dev-key"`. A prompt-injected visitor
session is authorized under the developer's full policy grant — every
secret their key can resolve, every tool their key can invoke — because
Cedar has no way to say "this principal is a visitor of that app, and may
do less than the app itself."

**Fact 3 — nothing is ever cleaned up.** `crates/vam-storage/src/gc.rs`
only does reference-counted CAS chunk GC across all workspaces
(`collect_referenced_chunk_hashes`, `sweep_unreferenced_chunks`);
`remove_workspace_tree` exists but is explicit-call-only. There is no TTL,
no age-based expiry, and no per-visitor cleanup trigger anywhere in the
codebase. A public site generates one permanent, ungoverned workspace
directory per visitor, forever. This is the same failure mode Modal hit in
production: filesystem snapshots persisted indefinitely until they shipped
a breaking change to add a default 30-day TTL. We should not repeat that
mistake — the ceiling needs to exist from day one, not be retrofitted after
disk usage becomes a problem (see §6.3).

None of these three problems are solved by convention or by documentation.
They need new entities in the identity model.

## 3. Design principles, validated against prior art

Before finalizing the shape below, we checked it against how E2B, Modal,
Vercel Sandbox, Daytona, Fly.io Machines, and Cloudflare (Workers for
Platforms, Durable Objects) solve "one developer, many of the developer's
own end users," and how Stripe Connect, Twilio, Clerk, WorkOS, and LiveKit
solve the identity side of the same problem. Three findings changed this
design from its first draft:

1. **No sandbox platform has a "tenant" primitive, and Cloudflare
   explicitly warns against building one.** Workers for Platforms'
   dispatch-namespace docs say in plain language: *"All your customers'
   Workers should live in a single namespace... Do not create a namespace
   per customer."* Every other platform (E2B, Modal, Vercel, Daytona, Fly)
   just uses a developer-chosen name plus filterable key/value metadata.
   **Consequence:** `App` stays a thin, cheap principal — one per
   product/environment, not one per enterprise customer. A B2B2C
   developer's own customers (e.g. an agency's clients) are a **label** on
   `Subject`, never a new hierarchy level.

2. **Every platform lets the developer address an instance by a stable,
   developer-chosen name, with get-or-create semantics** — Modal
   `Sandbox.from_name` (raises `AlreadyExistsError` on collision), Vercel
   `Sandbox.getOrCreate`, Cloudflare Durable Objects `idFromName`/
   `getByName` ("Named Durable Objects are the most common method of
   referring to Durable Objects"). E2B is the one outlier that uses opaque
   IDs only, and its own docs have to compensate by making metadata query
   the primary reconnect path. **Consequence:** `Subject` keeps a
   developer-supplied reference string (`subject_ref`) as first-class,
   used for idempotent get-or-create — this is *not* the same thing as
   storing the developer's PII, see point 3.

3. **The opaque-handle-with-no-PII pattern is real, established practice**,
   not something we invented to be conservative. LiveKit's docs say it
   outright: *"Don't put PII in identity or room name... Use an opaque
   identifier such as a UUID, and map it to user data in your own
   backend."* The stated reason is log/trace leakage, not just GDPR.
   OpenAI's abuse-tracking `safety_identifier` guidance is the same shape:
   *"Hash the username or email address in order to avoid sending us any
   identifying information."* **Consequence:** `subject_ref` is a
   developer-chosen *string* (can be a hash, a UUID, or their own customer
   ID — VAM does not care), never validated or interpreted as PII, and
   VAM never asks for or stores the developer's real customer record.

Two structural gaps the same research surfaced, which this doc now treats
as must-haves rather than nice-to-haves: **no rate limiting exists
anywhere on VAM's HTTP API today** (every comparable platform enforces a
concurrency ceiling and a creation-rate limit at the account/App boundary),
and **list endpoints return the entire result set with no pagination**
(`GET /v1/sessions`, `GET /v1/runs` both set `pagination: None`
unconditionally) — untenable once one App can have thousands of Subjects.
Both are addressed below (§8, §7.4).

## 4. The identity hierarchy

```
Tenant                     (VAMD_TENANT_ID; unchanged, daemon-wide today)
  └── User                 (existing; a developer/operator; holds API keys)
       └── App             (NEW; one product, one environment — "my website, in prod")
            └── Subject    (NEW; a visitor/customer of that App)
                 └── Workspace  (extended; now App- and Subject-scoped)
                      └── Session   (unchanged mechanically)
```

`User` is unchanged in meaning. **`App` and `Subject` are new.**

### 4.1 App — one product, one environment

An **App** is a registered, named service principal that a `User` creates
to represent one integration in one environment — "marketing-site / live",
"marketing-site / test" — that will mint many `Subject`s on the user's
behalf. An App is not a person; it never logs in interactively and never
holds a password. Per §3.1, an App is **not** created per enterprise
customer, per agency client, or per tenant of the developer's own
product — that distinction is a label on `Subject` (§4.2). An App exists
so that:

- Cedar has something to authorize *below* the developer's own `User`
  principal and *above* any individual visitor.
- Budgets, concurrency ceilings, and creation-rate limits can be set per
  integration rather than only per developer account.
- Test and production traffic for the same product are structurally
  separated — cheap to build in now, expensive to retrofit later (Stripe's
  `livemode` and WorkOS's per-environment scoping both exist because this
  was learned the hard way).

**Proposed record** (`crates/vam-orchestrator/src/apps.rs`, new module,
same shape as `crates/vam-orchestrator/src/users.rs`):

```rust
/// A registered service principal owned by a `User`: one product, in one
/// environment. Never authenticates interactively; mints `Subject`s and is
/// the Cedar parent for all of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppRecord {
    pub app_id: String,          // e.g. "app_9f2a..."
    pub owner_user_id: String,   // FK -> users.user_id
    pub name: String,            // developer-facing label, e.g. "marketing-site"
    pub environment: AppEnvironment, // Live | Test
    pub created_at: i64,
    pub deactivated_at: Option<i64>,
    pub max_concurrent_subjects: Option<i64>,     // abuse/cost ceiling, §8
    pub max_subject_create_rate_per_min: Option<i64>, // creation-rate limit, §8
    pub max_financial_budget_micro: Option<i64>,  // aggregate spend ceiling, §9
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppEnvironment {
    Live,
    Test,
}
```

**Proposed table:**

```sql
CREATE TABLE IF NOT EXISTS apps (
    app_id TEXT PRIMARY KEY NOT NULL,
    owner_user_id TEXT NOT NULL REFERENCES users(user_id),
    name TEXT NOT NULL,
    environment TEXT NOT NULL DEFAULT 'live' CHECK(environment IN ('live', 'test')),
    created_at INTEGER NOT NULL,
    deactivated_at INTEGER,
    max_concurrent_subjects INTEGER,
    max_subject_create_rate_per_min INTEGER,
    max_financial_budget_micro INTEGER
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_apps_owner_name_env
    ON apps(owner_user_id, name, environment);
```

An App-scoped API key is the natural extension of the existing
`ApiKeyRecord` (`crates/vam-orchestrator/src/keys.rs`): add an optional
`app_id` column, so a key can be minted that is valid *only* for minting
and operating Subjects under one App, with a narrower scope set than a
full user key (no `users:admin`, no `secrets:write`, no `catalog:write`).
This is the key a website's backend actually holds — never the
developer's own login credentials.

### 4.2 Subject — one visitor, one workspace

A **Subject** is an individual visitor, customer, or end user of an App.
Subjects are cheap, numerous, and — critically — **not** rows in the
`users` table. They do not have roles, do not log in via `/v1/login`, and
are never grantable principals for catalog publish or admin actions.

The earlier draft of this document gave Subjects a three-way
`kind` — `Anonymous` / `Claimed` / `Managed`. That distinction collapses:
once `external_ref` is a developer-chosen string rather than a link back
to something VAM verifies, "claimed" (promoted after the fact) and
"managed" (known up front) are the same state reached by two different
paths. This revision uses **one lifecycle flag with one transition**:

```rust
/// A visitor/customer of one `App`. Not a `User`: no role, no login, no
/// catalog-publish rights. Cedar parent is the owning `App`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SubjectRecord {
    pub subject_id: String,             // e.g. "subj_..." — server-generated, always
    pub app_id: String,                 // FK -> apps.app_id
    pub state: SubjectState,            // Ephemeral | Durable
    pub subject_ref: Option<String>,    // developer-chosen stable name; NOT validated as PII
    pub labels: Vec<Label>,             // indexed key/value; see §7.4
    pub created_at: i64,
    pub promoted_at: Option<i64>,
    pub last_active_at: i64,            // drives idle reaping, §6.3
    pub expires_at: Option<i64>,        // NULL once durable
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubjectState {
    /// Workspace is discard-on-idle. Default for every new Subject.
    Ephemeral,
    /// Workspace is retained with no idle TTL. Reached via `promote`,
    /// or set directly at creation when the App already knows its
    /// customer (the old "managed" case — just Durable from birth).
    Durable,
}
```

**Proposed table:**

```sql
CREATE TABLE IF NOT EXISTS subjects (
    subject_id TEXT PRIMARY KEY NOT NULL,
    app_id TEXT NOT NULL REFERENCES apps(app_id),
    state TEXT NOT NULL CHECK(state IN ('ephemeral', 'durable')),
    subject_ref TEXT,
    created_at INTEGER NOT NULL,
    promoted_at INTEGER,
    last_active_at INTEGER NOT NULL,
    expires_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_subjects_app_id ON subjects(app_id);
CREATE UNIQUE INDEX IF NOT EXISTS idx_subjects_app_subject_ref
    ON subjects(app_id, subject_ref) WHERE subject_ref IS NOT NULL;
CREATE INDEX IF NOT EXISTS idx_subjects_expiry
    ON subjects(expires_at) WHERE expires_at IS NOT NULL;

CREATE TABLE IF NOT EXISTS subject_labels (
    subject_id TEXT NOT NULL REFERENCES subjects(subject_id),
    key TEXT NOT NULL,
    value TEXT NOT NULL,
    PRIMARY KEY (subject_id, key)
);
CREATE INDEX IF NOT EXISTS idx_subject_labels_kv ON subject_labels(key, value);
```

The `(app_id, subject_ref)` unique index is what makes `POST /v1/subjects`
an idempotent get-or-create keyed on the App's own reference to its
customer — the same shape as Modal's `Sandbox.from_name` — **without VAM
ever storing what that reference means.** `subject_labels` is separate
from `subject_ref` on purpose (§7.4): `subject_ref` is the one stable
identity key with a uniqueness constraint; labels are unlimited,
mutable, multi-valued, and exist purely for filtering, billing rollups,
and the B2B2C "which of the developer's own tenants does this visitor
belong to" case from §3.1.

## 5. Cedar schema changes

### 5.1 Entity hierarchy

Today (`crates/vam-security/schema/vam.cedarschema`):

```cedar
entity Agent;
entity User;
entity Tool;
entity Host;
entity Path;
entity Workspace;
```

Proposed — add `App` and `Subject`, and introduce the `in` hierarchy Cedar
supports but this schema does not currently use anywhere:

```cedar
entity User;
entity App in [User];
entity Subject in [App];
entity Agent in [Subject, User];   // Agent keeps acting for a User directly
                                    // when there is no App/Subject involved
entity Tool;
entity Host;
entity Path;
entity Workspace in [Subject, User];
```

`Agent in [Subject, User]` is a union deliberately: an existing
single-user CLI session still has an `Agent` acting directly for a `User`
(no App/Subject in play, zero behavior change for existing deployments).
An App-mediated session has an `Agent` acting for a `Subject`, which is
`in` an `App`, which is `in` a `User` — so a policy written as
`principal in Vam::User::"alice"` still matches every agent alice's apps
ever spawn, without that policy being rewritten.

**This is the entire point of the exercise:** policies can now be written
*once* at the `App` level — "subjects of this app may call `mast.ui:*` and
the public catalog tools, and may never touch `secrets:write` or
`ExecuteTool` against anything outside their own workspace" — and every
Subject that App ever mints inherits that ceiling automatically, with no
per-visitor policy authored or evaluated specially. The alternative
(authorizing every visitor against the developer's own `User` grant) is
the privilege-escalation hole described in §2, Fact 2; this hierarchy is
what closes it structurally rather than by review discipline.

**Compatibility risk to verify before implementation:** Cedar's `==` is an
exact match and does not follow `in`; only policies written with
`principal in X` benefit automatically from the new hierarchy. Any
existing policy using `principal == Vam::User::"..."` will not match an
`Agent` acting for a `Subject`, by design — that's the isolation we want —
but it means **any current policy intended to apply broadly must be
audited for `==` vs `in` usage** as part of implementation, not assumed
compatible.

### 5.2 Actions and context

`ExecuteTool`, `ReadFile`, `WriteFile`, `EgressRequest` currently declare
`principal: [Agent, User]`. Proposed: widen to
`principal: [Agent, User, App]` so an App's own operational calls (not
mediated through a Subject — e.g. an App-level health check or
maintenance task) can also be authorized, without changing what a bare
`Agent` principal is permitted by default.

Add two new resource-scoped actions that don't exist today, both scoped to
`App` as principal only (never `Subject` — an individual visitor should
never be able to create or delete other visitors):

```cedar
action "CreateSubject" appliesTo {
  principal: [App],
  resource: [App],
  context: { subject_ref?: String }
};

action "PromoteSubject" appliesTo {
  principal: [App],
  resource: [Subject],
  context: {}
};
```

### 5.3 Default policy posture

The default policy shipped for a newly created `App` (analogous to how
`local`/`operator` users are seeded today in
`crates/vam-orchestrator/src/users.rs`) should deny by default and
enumerate an allowlist, not the reverse — consistent with VAM's existing
Cedar posture elsewhere in the codebase. A strawman default:

```cedar
// Subjects of any App may execute app-scoped and catalog tools, and may
// touch only their own workspace. Everything else is denied by omission.
permit (
  principal in Vam::App::"?app",
  action in [Vam::Action::"ExecuteTool", Vam::Action::"ReadFile", Vam::Action::"WriteFile"],
  resource
) when {
  resource in principal  // resource (Workspace/Tool) must belong to this App's tree
};

forbid (
  principal in Vam::App::"?app",
  action == Vam::Action::"ExecuteTool",
  resource
) when {
  resource.side_effect_class == "secrets:write" ||
  resource.urn like "hosted:*:admin*"
};
```

The exact shape needs real Cedar-schema-validated authoring against the
existing policy test suite (`crates/vam-security/tests/pdp_tests.rs`); the
above is illustrative of intent, not a final policy.

## 6. Workspace model changes

### 6.1 Path formation

Today: `{drive9_root}/workspaces/{owner_user_id}--{suffix}`
(`apply_owner_prefix`, `crates/vam-storage/src/owner_prefix.rs`).

Proposed extension, additive and backward compatible — a `User`-only
workspace path is unchanged; an App/Subject workspace gets a longer,
still-`--`-delimited prefix:

```
{drive9_root}/workspaces/{owner_user_id}--{app_id}--{subject_id}
```

`apply_owner_prefix` already rejects embedded `--` and path traversal in
caller-supplied suffixes; the same normalization extends naturally to a
three-part prefix as long as `app_id` and `subject_id` are
server-generated (never client-suppliable strings), which they are —
a Subject never chooses its own ID, only the App does, indirectly, via
`POST /v1/subjects`.

This is deliberately **each Subject getting its own full workspace** —
own manifest, own lock, own commit history — rather than one shared
workspace with sub-paths carved out of it. That is a difference from
Modal's and Daytona's volume-sub-path pattern (`volume.with_mount_options
(sub_path="/users/user_123")`), which we considered and did not adopt as
the primary mechanism: a shared workspace with per-subject sub-paths would
still need a lock *per sub-path*, which is new locking logic VAM doesn't
have today, whereas one workspace per Subject reuses
`WorkspaceManager::acquire_lock` completely unchanged. Concurrent visitors
stop colliding because each gets a workspace, not because sub-paths got
smarter.

The Modal/Daytona insight isn't wasted, though — it answers a real need
we don't yet have a mechanism for: **shared, App-level reference data
every Subject's agent can read** (a knowledge base, a product catalog).
That would be a read-only mount of the App's own workspace
(`{owner}--{app_id}`, no `subject_id` suffix) into each Subject's session.
Whether VAM's guest boot path can mount a second, read-only workspace
alongside a Subject's own is **not yet confirmed** and is out of scope for
this revision — flagged in §14 as a follow-up, not required for the
concurrency fix.

### 6.2 Lifecycle states

The earlier three-class table (`persistent` / `ephemeral` / `claimable`)
collapses to match the two-state `SubjectState` in §4.2:

| Subject state | Commit behavior | Idle handling |
|---|---|---|
| n/a (plain `User` workspace) | Commits normally (today's only behavior) | Unchanged — no TTL, exactly as today |
| `Ephemeral` | Commits normally during the session; **workspace is reaped on idle timeout** (§6.3), not on session end — a returning visitor within the idle window keeps their history | Reaped after `last_active_at` + idle window, **hard-capped at 30 days regardless of configuration** |
| `Durable` | Commits normally, retained indefinitely | No idle TTL. Deleted only by explicit `DELETE /v1/subjects/:id`, or by App-deactivation cascade (§6.3) |

This is a change from the earlier draft's `ephemeral` = "never commits,
discarded at session end." Discarding on session end is *stricter* than
every comparable platform: E2B, Modal, Vercel, and Daytona all default to
**pause-and-resume**, treating "visitor navigated away" as different from
"visitor is gone." Reaping on an idle timer rather than on session end
gives a returning visitor their conversation back without requiring the
App to have called `promote` — closer to what developers building on
comparable platforms will expect.

### 6.3 TTL reaper and cascade delete — the 30-day rule

New responsibility for `vamd`, proposed as a periodic task alongside the
existing sweeper loops already running today (HITL expiry and telemetry
retention, both every 60s, `crates/vam-orchestrator/src/bin/vamd.rs`).
Two independent 30-day rules, for two different reasons:

**Rule 1 — ephemeral idle ceiling.** An App may configure its own idle
window for `Ephemeral` subjects (a reasonable default to propose is 24h,
tunable per App), but the reaper enforces a **hard ceiling of 30 days**
from `last_active_at` regardless of App configuration or misconfiguration.
This is not a suggestion to the App — it is enforced by `vamd` itself, so
a forgotten or misconfigured idle window never turns into unbounded disk
growth. This mirrors Modal's filesystem-snapshot default (30 days) and
Twilio's subaccount purge window (30 days after closure) — both chosen
independently by mature platforms as the point past which "might still be
resumed" stops being worth the storage cost.

1. Scan `subjects` where `state = 'ephemeral'` and
   `last_active_at < now() - idle_window` (App-configured, capped at 30
   days) and no active session holds the workspace lock.
2. For each: terminate any lingering session, call
   `remove_workspace_tree` (`crates/vam-storage/src/gc.rs`, already
   implemented) on the Subject's workspace, delete the `subjects` row.
3. Rely on the **existing** reference-counted chunk GC
   (`collect_referenced_chunk_hashes`, `sweep_unreferenced_chunks`) to
   reclaim CAS bytes — it already scans manifests across all workspaces
   and is correctly shared-chunk-aware, so it needs no change for this to
   work.

**Rule 2 — App-deactivation grace period.** When an App is deactivated
(`POST /v1/apps/:id/deactivate`), its `Durable` subjects are **not**
deleted immediately — mirroring Twilio's "Twilio deletes closed
subaccounts 30 days after closure" — giving the developer a recovery
window if deactivation was a mistake. After 30 days past
`deactivated_at`, the same reaper cascades: all of the App's remaining
Subjects (ephemeral or durable) are workspace-removed and their rows
deleted.

Also add an explicit `DELETE /v1/subjects/:id` for the right-to-erasure
case: same cascade, triggered by the App rather than by TTL, with no
grace period — immediate deletion. Because the cascade is workspace-tree
removal followed by the existing chunk GC pass, and because VAM already
has a hash-chained audit ledger recording policy decisions, deleting a
Subject is both complete and provable — worth surfacing to Apps as a real
compliance feature (GDPR/CCPA erasure), not just an internal cleanup
mechanism.

## 7. Session, API, and CallerContext changes

### 7.1 Session creation

`CreateSessionRequest` (`proto/vam/v1/orchestrator.proto`) gains an
optional `subject_id`. When present:

- `resolve_session_workspace_id` (`crates/vam-orchestrator/src/run_context.rs`)
  resolves the three-part workspace path from §6.1 instead of the current
  two-part one.
- `stamp_session_owner` continues to stamp the `User` as owner (billing,
  audit, and existing owner-scoped queries are unaffected), and a new
  `stamp_session_subject` stamps the `subject_id` alongside it.
- Idempotency (`find_session_by_idempotency_key`,
  `crates/vam-orchestrator/src/owner.rs`) is scoped to `(owner_user_id,
  subject_id, idempotency_key)` rather than `(owner_user_id,
  idempotency_key)` alone, so two visitors reusing the same idempotency
  key (plausible if a client library generates them predictably) don't
  collide.

When `subject_id` is absent, behavior is **completely unchanged** — this
is additive to the existing session-create path, not a replacement of it.

### 7.2 `CallerContext` stamping

`CallerContext` (`proto/vam/v1/hosted_services.proto`, line 94) is already
host-stamped and explicitly documented as *"Host-stamped only. Never taken
from guest args."* — the correct, already-unforgeable place to add:

```proto
message CallerContext {
  // ...existing fields...
  string app_id = 8;      // NEW — empty if this session has no App
  string subject_id = 9;  // NEW — empty if this session has no Subject
}
```

Every HSP broker invocation for a Subject-scoped session now carries
`app_id`/`subject_id` for free, with no new plumbing between the vsock
boundary and the broker — brokers that want to attribute cost or apply
per-Subject policy have it without parsing anything guest-supplied.

### 7.3 New HTTP surface

Proposed routes, following the existing `/v1/*` admin-resource pattern
(`crates/vam-orchestrator/src/api/mod.rs`):

```
POST   /v1/apps                       create an App (requires a User-scoped key)
GET    /v1/apps                       list caller's Apps
GET    /v1/apps/:id                   inspect one App
POST   /v1/apps/:id/deactivate        stop minting new Subjects; starts the 30-day grace period
PUT    /v1/apps/:id/limits            set max_concurrent_subjects, create-rate, budget ceiling

POST   /v1/apps/:id/keys              mint an App-scoped API key (narrow scopes)

POST   /v1/subjects                   get-or-create a Subject (by subject_ref if supplied,
                                       else always creates new); requires an App-scoped key
POST   /v1/subjects/:id/promote       ephemeral -> durable (single transition, §4.2)
GET    /v1/subjects?app_id=&state=&label.<k>=<v>    cursor-paginated, filterable (§7.4)
DELETE /v1/subjects/:id               explicit erasure; cascades per §6.3, no grace period
DELETE /v1/apps/:id/subjects          bulk erase all of an App's Subjects (offboarding case)
```

`POST /v1/sessions` gains the optional `subject_id` field described above;
no existing route signature changes for callers that don't supply one.

### 7.4 Labels and pagination — reusing what already exists

Two gaps surfaced by research are both fixable by wiring up proto shapes
VAM **already has**, not by inventing new ones:

- **Labels.** `ResourceMetadata` (`proto/vam/v1/common.proto`, already
  used by `PolicyRecord` and skill publishing) already carries `repeated
  Label labels` (indexed, for filtering) and `repeated Label annotations`
  (non-indexed). `Subject` and `App` should use this same message rather
  than a bespoke metadata field — it is the exact key/value pattern every
  platform researched relies on (E2B `metadata`, Modal `tags`, Cloudflare
  `tags`) for the developer's own multi-tenant labeling (§3.1's "which of
  my own customers is this"), and it costs nothing new to the schema.
- **Pagination.** `PaginationResponse` already exists in the orchestrator
  proto and is already returned by list RPCs — it is simply always set to
  `None` today (`GET /v1/sessions`, `GET /v1/runs`). `GET /v1/subjects`
  must not repeat that gap: it needs real cursor pagination and
  label-based filtering from day one, because an App with a
  successful public site can have far more Subjects than any existing
  VAM deployment has sessions.

### 7.5 Budgets and quotas

`SessionBudget` (`crates/vam-inference/src/budget.rs`) already tracks
per-session token/cost usage and rejects at `max_financial_budget`. This
proposal adds an **aggregate** layer above it: `apps.max_financial_budget_micro`
and `apps.max_concurrent_subjects` are checked at session-create time
(sum of active sessions/spend across all of an App's Subjects), rejecting
new session creation once either ceiling is hit, independent of any single
Subject's own per-session budget. See §8 for the abuse-control angle this
also needs on the creation-rate side.

## 8. Quotas, rate limiting, and abuse controls (must-have)

This is a genuine gap in VAM today, not a refinement: **no rate limiting
of any kind exists on VAM's HTTP API** — no token bucket, no concurrency
semaphore, on `POST /v1/sessions` or anywhere else in
`crates/vam-orchestrator/src/api/mod.rs`. Every comparable platform
enforces this at the account boundary: E2B caps concurrent sandboxes (20
Hobby / 100–1,100 Pro) *and* creation rate (1/sec / 5/sec); Cloudflare
Workers for Platforms lets the developer clamp CPU/sub-requests **per
dispatched request**. A public-facing App backed by microVMs needs this
structurally, not optionally — one abusive visitor must not be able to
starve every other visitor of the same site.

Proposed, scoped to `App` (not to individual `Subject`s — the App is the
billing/abuse boundary, matching every platform surveyed):

- `apps.max_concurrent_subjects` (already in §4.1) — reject new session
  creation once the App's live session count is at the ceiling.
- `apps.max_subject_create_rate_per_min` (already in §4.1) — a simple
  token-bucket check in `POST /v1/subjects` and `POST /v1/sessions`,
  rejecting with a typed `429` when exceeded.
- The `429` response should carry a machine-readable reason, following
  Stripe's `Stripe-Rate-Limited-Reason` convention, so a developer's own
  backend can distinguish "your App is over its plan" from "this one
  Subject is unusually hot" — worth a `X-VAM-Rate-Limited-Reason` header
  with values like `app-concurrency`, `app-create-rate`,
  `app-spend-ceiling`.

Per-Subject spend caps and a threshold-webhook (Twilio's `UsageTrigger`
pattern — notify before a hard cap is hit, not only at it) are reasonable
v1.1 additions once §9's usage rollup exists, but are not required to
close the concurrency/abuse gap above.

## 9. Usage & cost attribution (should-have, but the clearest differentiator)

Verified gap: `SessionBudget` counters live in `Mutex<i64>` fields only
(`crates/vam-inference/src/budget.rs`) and are never persisted to a
queryable table. A cost summary (`spend_micro`, token counts) is already
emitted as a telemetry event at session completion
(`crates/vam-orchestrator/src/api/mod.rs:2337-2346`) — but there is no
durable, queryable "how much did Subject X cost me this month" answer
anywhere.

This matters more than it might look: neither OpenAI's nor Anthropic's
admin usage APIs break usage down by the *developer's own end user* —
only by API key or workspace, both of which are the developer's own
resources, not their customers'. Twilio's `UsageRecord` +
`Subaccount`combination is explicitly marketed as letting a developer
"build recurring usage-based billing systems... with just a few simple
API calls." A per-Subject usage rollup is the thing that lets a Mast (or
any VAM-based product) developer bill *their* customers — something the
current AI-platform incumbents don't offer.

Proposed: persist the already-emitted cost-summary event into a
`subject_usage` rollup table (`subject_id`, `period_start`, `period_end`,
`prompt_tokens`, `completion_tokens`, `spend_micro`) written at session end
— this is wiring an existing event into a new table, not new
instrumentation — plus `GET /v1/subjects/:id/usage?since=&until=` for the
developer to read it back.

## 10. Lifecycle: ephemeral → durable

This is the product-facing shape the primitives above are built to serve,
now two states instead of three:

```
ephemeral   Visitor lands on the App's site. App calls POST /v1/subjects
            (no subject_ref, or a subject_ref it doesn't yet recognize).
            Cookie/local-storage holds subject_id. Workspace commits
            normally during the session. Reaped after the idle window
            (default proposal 24h), hard-capped at 30 days (§6.3).

durable     Either: (a) visitor signs up / logs into the App's own auth,
            and the App calls POST /v1/subjects/:id/promote — the same
            Subject, same workspace, same history, TTL cleared; or
            (b) the App already knows its customer up front (a B2B SaaS
            onboarding a new account) and calls POST /v1/subjects with a
            subject_ref supplied at creation time, landing directly in
            the durable state — no ephemeral phase at all. Both paths
            reach the same state; the old "claimed" vs "managed"
            distinction added no behavior that state + subject_ref don't
            already express.
```

Both states share one workspace and Cedar model; `state` only changes
idle-TTL behavior.

## 11. What this does not change

- `User`, `UserRole`, login, API keys without an `app_id`, and the entire
  existing CLI/single-operator workflow are unaffected. A deployment that
  never creates an `App` behaves exactly as it does today.
- The existing `owner_user_id` stamping, ownership-scoped queries, and
  user-isolation guarantees documented in
  `docs/src/content/docs/orchestrator/user-isolation.md` are unchanged;
  App/Subject data is additive underneath a `User`, not a parallel system.
- `vam-runtime`, `vam-vsock`, `vam-network`, and `vam-inference` need no
  changes — a Subject-scoped session is, from the guest's perspective, an
  ordinary session.
- HITL approval: a Subject-created session's approval request still
  routes to the owning `User` by default
  (`HitlApprover::Owner { user_id }`, `crates/vam-orchestrator/src/hitl.rs`)
  — there is no Subject-level or App-level approver in this revision.
  That is an acceptable v1 posture (the developer is always the one
  approving on behalf of their own visitors) and is flagged, not solved,
  in §14.

## 12. Deferred: per-customer credentials (BYO OAuth/API tokens)

A question raised alongside this design, and worth answering plainly
rather than half-solving: **if one of an App's own customers connects
their own third-party account (their own Gmail, their own Jira) so an
agent can act on their behalf, where does that credential live?**

Today, nowhere good. VAM's `host_secrets` table has exactly two scopes,
`org` and `user` (`crates/vam-orchestrator/src/secrets.rs`); there is no
per-Subject scope, so a customer-owned credential would currently have to
be stored as the *developer's* secret, which is not isolated per customer
and is a real security problem, not a cosmetic one.

**This document does not attempt to solve it, on purpose.** Two reasons:

1. It's a materially different, harder problem than App/Subject identity
   — it touches OAuth flow ownership, token refresh, per-Subject secret
   rotation, and a decision about whether VAM ever becomes a place that
   *does* hold third-party PII-adjacent tokens on behalf of a Subject
   (which cuts against the "VAM holds zero customer PII" property this
   whole design is built to preserve).
2. **It is not a solved problem anywhere we looked**, either. None of
   E2B, Modal, Vercel, Daytona, Fly, or Cloudflare's sandbox products
   address "my end user's own third-party credential" at all — it is
   out of scope for every comparable platform, handled entirely in the
   developer's own backend. Twilio and Stripe Connect are the closest
   analogues, and both solve it by having the *platform itself* run an
   OAuth-like onboarding flow for the sub-tenant (Stripe Connect's
   "onboard and verify sellers") — a much bigger commitment than adding a
   secret scope.

The one thing worth reserving now, so it isn't foreclosed later: the
`host_secrets.scope` `CHECK` constraint is the extension point
(`CHECK(scope IN ('org', 'user'))` → eventually `'subject'`) if and when
we come back to this. Nothing in this document requires deciding that
now, and nothing here makes it harder to add later. Treat it as a
separate, focused design discussion once App/Subject itself has shipped
and there's a concrete use case in front of us — not a gap in this
proposal.

## 13. Requirements summary

| | Item | Why |
|---|---|---|
| **Must-have** | `App`/`Subject` records, Cedar hierarchy (§4, §5) | Closes Fact 1 and Fact 2 — nothing else works without this |
| **Must-have** | Per-Subject workspace + idle-TTL reaper, 30-day hard ceiling (§6) | Closes Fact 3; the ceiling must not be optional (Modal's retrofit lesson) |
| **Must-have** | `subject_id`/`app_id` in `CallerContext` (§7.2) | Free — the field is already host-stamped and unforgeable |
| **Must-have** | `subject_ref` get-or-create (§4.2, §7.3) | The #1 convention across every platform surveyed; painful to retrofit |
| **Must-have** | Labels via existing `ResourceMetadata` (§7.4) | Zero new schema; the B2B2C sub-tenant labeling every dev will want on day one |
| **Must-have** | Cursor pagination + filtering on `GET /v1/subjects` (§7.4) | Wiring up an existing unused proto field; breaks at scale otherwise |
| **Must-have** | Per-App concurrency cap + creation-rate limit + typed 429 (§8) | VAM's HTTP API has zero rate limiting today; a public endpoint cannot ship without this |
| **Must-have** | Cascade + bulk delete, App-deactivation grace period (§6.3, §7.3) | Compliance-critical; the Twilio 30-day pattern this doc adopts |
| **Should-have** | Persisted per-Subject usage rollup (§9) | The clearest differentiator vs. OpenAI/Anthropic; not required to unblock concurrency |
| **Should-have** | Per-Subject spend cap + threshold webhook | Builds on §9; v1.1 once the rollup exists |
| **Could-have (reserve the shape, don't build)** | Subject-scoped secrets / BYO credentials (§12) | Explicitly deferred — harder problem, not solved anywhere else either |
| **Could-have** | Audited impersonation ("view as Subject") | Clerk/WorkOS both gate this behind explicit enablement even in mature products; reserve the audit-event shape, don't build the flow yet |
| **Could-have** | Read-only App-level shared workspace mount (§6.1) | Real need (Modal/Daytona's sub-path insight), but feasibility against the guest boot path is unconfirmed |

## 14. Open questions for detailed design

- Should `App` be Cedar-modeled as `in [User]` (proposed above) or should
  `Tenant` be introduced as a real, separate Cedar entity first, with `App
  in [Tenant]`? Today `tenant_id` on `users` is documented as
  "reserved for future org split" and currently equals `user_id`
  (`crates/vam-orchestrator/src/users.rs`) — this proposal deliberately
  does not resolve the Tenant question and hangs `App` off `User` directly
  to stay small; revisit if/when multi-org support lands for real.
- Exact default idle window for `Ephemeral` subjects (proposed 24h,
  hard-capped at 30 days per §6.3) — product tuning, not architecture, but
  needs a default before ship.
- Whether a read-only, App-level shared workspace mount (§6.1) is
  feasible against the current guest boot path — needs a spike against
  `vam-runtime`'s mount setup before committing to it.
- Full audit of existing Cedar policies for `principal ==` vs
  `principal in` usage before the hierarchy migration lands (§5.1
  compatibility risk).
- HITL approval routing when the owning `User` isn't the right approver
  for a Subject-created session (e.g. a support team member should
  approve, not the developer who wrote the integration) — out of scope
  for v1 (§11), flagged for whoever picks up App-level approver delegation.
- Per-customer credentials (§12) — deferred entirely; revisit as its own
  design discussion once there's a concrete use case.
