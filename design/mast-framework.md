# Squrell Mast — an agentic hypermedia framework on VAM

## 1. What we are building

**Mast** is a Rust web framework, built as a sibling project to VAM, for
developers who want to ship websites where most of the page is ordinary
server-rendered hypermedia and a few well-defined regions are filled, live,
by an AI agent running inside a VAM microVM.

Mast is not an end-user product. It is a framework — scaffolding, a runtime
library, a compiler, and a CLI — that developers use to build their own
agentic sites, the same way they'd reach for Axum, Next.js, or Phoenix
LiveView. The framework's job is to make the "agent writes part of the page"
pattern safe, fast, cacheable, and boring to operate, so product teams can
focus on the use case instead of re-solving streaming, sanitization, and
sandboxing every time.

The name **Mast** is deliberately doubled: in forestry, *mast* is the nut and
acorn crop a tree produces — the thing squirrels gather. In shipbuilding, a
mast is the fixed spar that carries a dynamic sail. Both meanings describe
the architecture: a fixed, deterministic structure that carries something an
agent yields.

Vocabulary we're standardizing on for the rest of this doc and for the code:

| Term | Meaning |
|---|---|
| **glade** | A developer-declared region of a page whose content is produced by an agent, e.g. `<div mast-glade="checkout.assist">`. Named for a clearing in dense forest — an intentional opening in otherwise static structure. |
| **yield** | The compiled, sanitized fragment an agent produces for a glade. |
| **Mast IR** | The closed, typed intermediate representation the compiler consumes. Lives in the `mast-ir` package. |
| **App** | A developer's registered service principal (their website) minting many Subjects. See `design/vam-subjects.md`. |
| **Subject** | An individual visitor/customer of an App — an ephemeral or claimed identity, distinct from a VAM `User`. See `design/vam-subjects.md`. |
| **cache** | The fragment memoization layer (Drive9-backed), keyed by glade + input digest + agent/model version. |

Mast depends on a companion VAM RFC, **`design/vam-subjects.md`**, which
introduces the `App` and `Subject` identities VAM needs before it can safely
serve more than one concurrent anonymous visitor. That dependency is called
out explicitly in §6 and §7 below — it is a hard prerequisite, not an
optional enhancement.

## 2. The core idea

Traditional AI-in-the-browser patterns fall into two bad shapes today:

1. **Chatbot bolted onto the side** — a widget, disconnected from the page,
   that can't actually change the page.
2. **LLM writes JSON, client-side JS reconciles it into React state** — heavy
   client bundles, a large trust boundary (arbitrary JSON driving arbitrary
   client logic), and an LLM that has to get *program structure* right, not
   just content right.

Mast takes a third shape, built on the observation that **LLMs are far more
reliable at producing markup-shaped content than at producing correct
client-side state transitions.** So we push reactivity to the server and
give the agent a narrow, typed vocabulary to emit into:

- **Client:** [Datastar](https://data-star.dev) — `data-*` attributes only,
  no client framework, no build step. The server pushes DOM patches and
  signal updates over Server-Sent Events.
- **Server:** a Rust/Axum gateway that owns routing, the SSE fan-out, a
  compiler from Mast IR to sanitized Datastar fragments, and a
  developer-defined action table.
- **Agent execution:** VAM. Each glade's agent turn runs inside a Firecracker
  microVM, brokered through VAM's existing Hosted Services Protocol (HSP).

Pages are three tiers, and only the third tier ever touches a VM:

```
Tier 1 — Static shell           no VM, no LLM      (nav, layout, most content)
Tier 2 — Deterministic dynamic  no VM, no LLM      (DB query -> template -> patch)
Tier 3 — Agentic glade          VAM session        (agent-authored yield)
```

Because Tier 3 is rare, bursty, and user-triggered, the interesting
engineering problem isn't raw invocation latency (the LLM round-trip already
dominates at 300ms-3s) — it's **session economics** (how many glades can a
host serve concurrently, how fast a glade goes from cold to answering, and
what happens while a visitor sits idle) and **safety** (what an agent is and
isn't allowed to put on a page).

### 2.1 Why "session economics" is not a nice-to-have

VAM's session model as it exists today assumes every session belongs to an
authenticated `User`, and a workspace is guarded by a single write lock
(`WorkspaceManager::acquire_lock`, `crates/vam-storage/src/workspace.rs`). A
website authenticating every visitor with one developer API key and not
independently varying `workspace_id` resolves every visitor to the same
workspace — **one lock, one visitor at a time, globally, for the entire
site.** This is not a performance tuning problem; it is a hard concurrency
ceiling of exactly one.

Worse, even once workspaces are varied per visitor, every visitor's agent
still evaluates Cedar policy as the developer's own `User` principal, because
VAM's Cedar schema has no entity hierarchy today — a prompt-injected visitor
session is authorized under the site owner's full policy grant. Mast cannot
be built safely on top of VAM as it stands; it needs the identity primitives
in `design/vam-subjects.md` (the `App` and `Subject` entities) first. That
RFC is written to stand on its own merit for VAM — Mast is its first and
motivating consumer, not its only one.

## 3. High-level architecture

```
Browser (Datastar: data-* only, strict CSP, no inline script)
   |  SSE: datastar-patch-elements / datastar-patch-signals
   v
+-----------------------------------------------------------------+
| mast (Axum gateway)                                              |
|  - static shell + deterministic route handlers                  |
|  - glade registry + server-side action table                    |
|  - SSE hub: per-browser-session fan-out, resumable               |
|  - OpenUI Lang parser -> Mast IR -> sanitized Datastar fragments |
|  - fragment cache (keyed on glade + input digest + agent version)|
|  - "mast.ui" HSP broker (the integration seam into VAM)          |
|  - vam-client: HTTP + telemetry-stream client                    |
+------------------------+------------------------------------------+
                         | HSP gRPC/UDS Invoke      | HTTP /v1/sessions,
                         v  (same host, see 3.2)     | /v1/subjects, /v1/approvals
+-----------------------------------------------------------------+
| vamd (VAM orchestrator, extended per design/vam-subjects.md)     |
|  Cedar PDP - catalog - budgets - vault - hash-chained audit ledger|
|  + App / Subject identity, ephemeral workspaces, TTL reaper       |
+------------------------+------------------------------------------+
                         | AF_VSOCK :52 control / :53 inference
                         v
                  Firecracker guest — vam-runtime ReAct loop
                  calls tool `hosted:mast.ui:yield_fragment`
```

**Why HSP is the integration seam, not a new VAM subsystem:** the in-guest
agent calls `yield_fragment` as an ordinary tool. That tool call already goes
through `RequestToolAuthorization` -> Cedar policy -> capability token ->
`HostedToolAdapter` -> our broker's `Invoke`. Authorization, per-session
budgets, redaction, HITL interception, and the audit ledger all apply for
free, and — once `design/vam-subjects.md` lands — that authorization is
scoped correctly per visitor rather than per developer account.

One structural consequence follows directly from HSP being request/response:
**agents yield whole fragments, not raw tokens.** We do not stream
partially-built markup to the compiler — a half-built AST cannot be safely
validated. Progressive rendering (skeleton, then replace) is done as
multiple discrete `yield_fragment` calls, not as a token pipe.

### 3.1 The UI contract: OpenUI Lang in, Mast IR is canonical

Agents do not emit HTML, and they do not emit an ad hoc JSON tree either.
They emit **[OpenUI Lang](https://www.openui.com/docs/openui-lang)**
(MIT-licensed, `thesysdev/openui`) — a compact, line-oriented, streaming-
friendly syntax purpose-built for LLMs to describe UI (`root =
Stack([chart])`), reporting up to 67% fewer tokens than the equivalent JSON.
We do not build our own surface syntax for this; there is no benefit to
inventing a second one, and OpenUI Lang already has LLM-familiarity and
ecosystem mindshare working in our favor.

What we do build is everything past the parser:

```
OpenUI Lang (agent output; parsed by our own Rust frontend)
        |
        v
   Mast IR   <-- canonical, typed, closed-vocabulary form; OUR contract
        |
        v
   compiler backend: escaping, action-table resolution, CSP-safe output
        |
        v
   Datastar HTML fragment
```

The published OpenUI ecosystem targets a client-side `<Renderer />` that
instantiates React components from the parsed tree — the opposite of Mast's
no-client-runtime thesis, and it validates against a developer-supplied
Zod schema that has no inherent concept of a closed action vocabulary. We
therefore treat OpenUI Lang as a **frontend syntax only**: our parser
produces Mast IR, and every safety property described in §5 pillar 4 is
enforced by *our* compiler, not inherited from the upstream project. If
OpenUI Lang stalls, forks in an unwanted direction, or a competing syntax
(Vercel's JSON-Render, A2UI) wins adoption, only the parser frontend is
replaced — the IR, compiler, and every downstream guarantee are unaffected.
This is the same swap-a-layer discipline applied to the HSP transport below.

### 3.2 Transport: co-located today, network-transparent later

HSP brokers register with `vamd` over a Unix domain socket
(`crates/vam-orchestrator` HSP registry) — this is a same-host mechanism by
construction. Mast's process is simultaneously an HTTP *client* of `vamd`
(via `vam-client`) and an HSP *server* registered with it. **v1 requires the
Mast application process and `vamd` to run on the same host.** That is a
fine constraint for self-hosted developer deployments and is not a blocker
for the initial build, but it does rule out a separate web tier or
Mast-as-a-hosted-service without further work.

VAM's existing HITL flow already proves the shape a distributed version
would need: an agent's tool call suspends the run, `vamd` publishes to a
durable outbox, an external caller observes and responds asynchronously
(`GET /v1/approvals`, `POST /v1/approvals/:id/decide`), and the session
resumes. Generalizing that into a **deferred/external tool** pattern —
published on the existing telemetry broadcast bus, answered by HTTP
callback — would give `yield_fragment` a network-transparent path with no
new wire protocol. We design the `mast.ui` broker behind a transport trait
from day one so this is a future swap, not a rewrite, but we do not build
the distributed path until a real deployment needs it.

### 3.3 Client layering

The VAM-facing client code Mast needs splits into three layers with
different ownership and maturity expectations:

| Layer | Contents | Home | Maturity |
|---|---|---|---|
| **L1 — `vam-client`** | Raw HTTP API client + telemetry gRPC stream client. Zero product opinions. | `squrell/vam` | Extracted from `vam-cli`, stable |
| **L2 — subject/session lifecycle** | Cookie-to-Subject mapping, session pool, warm-pool claim, idle reaping, quota accounting against App ceilings | Prototyped inside `squrell/mast` first | Upstream to VAM only once proven — see §6 |
| **L3 — `mast`** | Glades, IR compiler, SSE hub, developer-facing API | `squrell/mast` | Mast-owned, web release cadence |

L2 is deliberately not designed into VAM speculatively. It is general enough
that any multi-tenant product built on VAM would want it, but its correct
shape (pool sizing policy, reaping cadence, quota granularity) is best
discovered by building one real consumer first.

## 4. Repository plan

Three artifacts, each on its own release cadence:

| Repo | Contents | Depends on |
|---|---|---|
| `squrell/vam` (this repo) | Only the small, general-purpose additions listed in §6. No web-specific code. | — |
| `squrell/mast-ir` | The Mast IR schema (protobuf + JSON Schema), generated Rust and TypeScript bindings, the component/action vocabulary, versioning rules. | — |
| `squrell/mast` | The Axum gateway, SSE hub, IR compiler + hostile-input test suite, glade/action registry, `mast.ui` HSP broker, `mast` CLI and dev server, project templates. | `mast-ir`, VAM's `vam-client` |

Guardrail for the life of the project: **if `mast` ever needs to reach into
`vam_orchestrator` internals, that's a signal the seam is wrong.** Every
capability Mast needs from VAM should be expressible as an HSP broker, a
public HTTP API call, or a telemetry subscription — never a direct dependency
on orchestrator internals. This keeps Mast's release cadence decoupled from
VAM's and keeps VAM's core free of web-framework opinions.

`mast-ir` is split out on its own because it's the actual contract: guest-side
skills (written in Bun/TypeScript inside the VM) and the host-side compiler
(Rust) both need typed bindings generated from one schema, and it needs to be
versioned independently of both the framework and VAM.

## 5. Capability pillars

These are the high-level capabilities the framework must deliver. Each gets
its own detailed design doc before implementation.

1. **Rendering & transport** — static shell rendering, deterministic route
   handlers, the SSE hub (per-visitor connection, resumable across
   reconnects), and glade lifecycle (declared, mounted, active, idle,
   evicted).
2. **The Mast IR & compiler** — an OpenUI Lang parser frontend (see §3.1),
   a small closed component vocabulary in the canonical Mast IR (start at
   roughly a dozen: text, heading, list, table, card, form, input, select,
   button, choice-group, progress, error), a closed action vocabulary
   resolved only through a developer-registered server-side table, and a
   compiler with a standing hostile-input test suite (injection via text,
   attributes, and action names) that runs in CI on every change to the IR
   or compiler.
3. **Agent binding** — the `mast.ui` HSP broker, glade-to-session mapping,
   session creation/pause/resume driven by real visitor activity, and a warm
   pool so the first agent turn on a glade doesn't pay full cold-boot cost.
4. **Safety model** — closed vocabularies enforced at compile time (not by
   convention), zero trust of client-sent Datastar signals (they're hints,
   server state is authoritative), strict CSP with no inline script and no
   `data-on-*` passthrough of agent-authored expressions, and prompt-injection
   containment at the IR boundary rather than the HTML boundary.
5. **Determinism & fallback** — every glade declares a deterministic fallback
   (used for crawlers, no-JS clients, agent failure, and budget exhaustion),
   plus a fragment cache keyed on `(glade, input digest, agent version,
   model)` stored in Drive9 CAS so repeat/similar requests are near-instant
   and cheap.
6. **HITL as UI** — pending VAM approvals rendered as a glade in-page, with
   the decide action wired back to `/v1/approvals/:id/decide`. This is close
   to free given what VAM already exposes, and it's one of the more
   compelling demos of the whole framework.
7. **Developer experience** — `mast new` scaffolding, `mast dev` local server
   with a mocked/offline agent mode, glade authoring ergonomics, and
   attribution surfaced from VAM's audit ledger so a developer can see which
   agent/model/policy produced any rendered fragment.
8. **Operations & multi-tenancy** — per-app (not just per-session) budget and
   concurrency ceilings, session labeling so an operator can distinguish
   Mast-driven traffic from other VAM workloads, and idle-session reaping so
   abandoned browser tabs don't hold a VM open indefinitely.

## 6. VAM enhancements required

Mast is designed to need as little from VAM's core as possible — most of what
it needs already exists (streamed inference, HSP, Cedar-mediated tool
execution, the audit ledger, snapshot/restore). One gap is a hard blocker;
the rest are real but small enough to land independently of Mast's own
build-out.

| # | Enhancement | Why Mast needs it | Priority |
|---|---|---|---|
| 0 | **`App` and `Subject` identities, ephemeral workspace class, TTL reaper — the full `design/vam-subjects.md` RFC** | Without this, VAM serves exactly one concurrent visitor per site (single workspace write-lock) and authorizes every visitor under the developer's full Cedar grant. Nothing else in this document can be demoed with real anonymous traffic until this lands. | **P0 — blocking, own RFC, largest item on this list** |
| 1 | Extract a `vam-client` crate (HTTP API client + telemetry gRPC stream client) out of `vam-cli` | Mast needs the same client `vam-cli` uses, without vendoring CLI code or re-implementing auth/session calls | P0, small |
| 2 | Add `ui.*` event types to `vam-telemetry` semantic conventions and confirm HITL pending/decided transitions are published on the workflow outbox / broadcast bus | Mast subscribes to VAM's existing `TelemetryStreamService.StreamEvents` for out-of-band glade state (thinking, tool progress, budget warnings, approval state) instead of inventing a second channel | P1, small |
| 3 | Generic key/value labels on session creation (beyond `owner_user_id`) | Mostly subsumed by the App/Subject stamping in enhancement #0; keep as a small generic add for anything App/Subject don't already cover | P2, small |
| 4 | Aggregate, app-scoped budget and concurrency ceilings, layered above the existing per-session `SessionBudget` | Specified as part of `design/vam-subjects.md` §6.3 (`apps.max_financial_budget_micro`, `apps.max_concurrent_subjects`); listed here because it's an abuse ceiling for public sites, not just a cost control, and Mast should treat it as required, not optional | P0 (via #0), small once #0 lands |
| 5 | Prioritize the already-planned Tier 2 paused-standby pool (sub-10ms claim) | Mast's UX target is "glade responds immediately on first visitor interaction"; today's golden-snapshot restore (about 225 ms session start on the CI lane, VM ready in about 60-120 ms; measured 2026-10-24) is good, but a warm/paused pool removes it entirely for the common case | P1 — already on VAM's roadmap; we're asking to move it earlier |
| 6 | Orchestrator-side auto-idle pause policy (pause a session after N seconds of no activity, without an external caller) | Useful to any VAM host, not just Mast; anonymous tabs are abandoned constantly | P1 — Mast can implement this itself first (as the ephemeral-workspace TTL reaper in #0 already does) and propose upstreaming the general policy later |

None of these require changes to `vam-runtime`, `vam-vsock`, `vam-security`'s
guest-facing surface, or `vam-network`. Enhancement #0 does extend
`vam-security`'s Cedar schema (new `App`/`Subject` entities and hierarchy),
but nothing here weakens or bypasses the guest trust boundary — every
capability Mast uses from the guest side remains an ordinary Cedar-mediated
HSP tool call, now correctly scoped per visitor instead of per developer
account.

## 7. Development order

The order below front-loads the two riskiest bets — hypermedia ergonomics
without a client framework, and a safe agent-authored IR — before any VAM
wiring happens, so both can be validated with mocked/offline agents first. It
also front-loads the one hard VAM dependency so it isn't discovered late.

1. **VAM identity foundation (parallel, blocking track).** Implement
   `design/vam-subjects.md`: `App`/`Subject` tables, the Cedar entity
   hierarchy migration, ephemeral workspace class, and the TTL reaper. This
   can and should proceed in parallel with steps 2-3 below, but step 4
   cannot start with real multi-visitor traffic until it lands.
2. **Contracts.** Stand up `mast-ir`: the v0 component and action vocabulary,
   the glade manifest format, and generated Rust + TypeScript bindings. Land
   VAM enhancement #1 from §6 (`vam-client` extraction) in parallel — it's
   independent of everything else here.
3. **Static/deterministic core.** Build the `mast` gateway skeleton: routing,
   static shell rendering, the Datastar SSE hub, the slot/action registry,
   and `mast dev`. This should work end-to-end as a plain Rust hypermedia
   framework with zero agent involvement, and should be dogfooded as such —
   it's the foundation everything else sits on.
4. **Compiler & safety rail.** Build the OpenUI Lang parser, the Mast IR ->
   Datastar compiler, and the hostile-input test suite (injection via text,
   attributes, action names). No live agent needed yet; this is where we
   prove the safety model holds before anything untrusted touches it.
5. **VAM integration, single glade, single visitor.** Build the `mast.ui`
   HSP broker and `yield_fragment` tool, wire `vam-client` for session
   create/pause/resume, and ship one guest-side skill template. Demo: one
   glade, one real agent, one real VAM session, end to end.
6. **VAM integration, many visitors.** Wire session creation through
   `Subject` (step 1's output): cookie-to-Subject mapping, one workspace per
   visitor, App-scoped Cedar policy. Demo: two browsers, two independent
   agent conversations, on the same site, concurrently — the thing that is
   impossible without step 1.
7. **Streaming, HITL & progressive rendering.** Add multi-call progressive
   rendering (skeleton then replace), subscribe to VAM's telemetry stream for
   out-of-band glade state, and render pending HITL approvals as a glade.
8. **Caching & determinism.** Add the Drive9-backed fragment cache and
   require every glade to declare its deterministic fallback (crawler,
   no-JS, agent failure, budget exhaustion).
9. **Multi-tenancy & ops.** Land VAM enhancements #5-#6 from §6, add
   app-level budgets/labels/dashboards, and validate idle-eviction economics
   under load.
10. **DX polish & release.** `mast new` scaffolding, a couple of complete
    example apps, docs, and the first versioned release of `mast-ir` and
    `mast`.

## 8. Open questions for detailed design

These are deliberately left open here and will be resolved as each capability
gets its own design doc before implementation:

- Exact glade declaration surface — template attribute vs. Rust macro vs.
  manifest file (or some combination).
- Session-to-visitor identity model — cookie-based, one VAM session per
  browser session vs. pooled/shared sessions, and reconnect semantics.
- v0 component and action vocabulary — final list, and the process for
  extending it (every addition is permanent security surface).
- Cache invalidation policy beyond the input-digest key (time-based? agent
  version bump? manual purge?).
