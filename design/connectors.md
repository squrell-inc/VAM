# Connectors: how VAM reaches the systems people work in

Status: **direction agreed with the owner 2026-11-07** (the decision record
below, D1 to D22; D23 to D27, the Git marketplace and installable connectors,
agreed 2026-11-09). **Owner, 2026-11-07 (later):** connectors are in 0.1.0
(0.1.0 launch scope); the per-call egress envelope
(open question 8) is part of Phase 2; the three-line amendment to
`design/mcp-oauth-connections.md` (§12) is approved and applied. Phases are proposed and are planned one at a time; Phase 1
is planned by a separate session. Written by a design session against `main`
at and brought up to date with `main` (notifications
Phase 2 released: `vam.notify`, Slack, contacts;
the DX-139 security floor).
Where `main` disagrees with an
earlier design, `main` is the fact. Nothing in a tracker is edited by this
doc; amendments to other designs are listed in §12. Tracker slug proposed:
`connectors`, item prefix `CN`.

## Decision record

| # | Topic | Decision |
|---|---|---|
| D1 | Tenancy | One `vamd` node per customer organization. Its employees are the users; its catalog holds its use cases. Squrell services (hubs, OAuth apps, relays) are shared across nodes; everything else is per node. |
| D2 | Microsoft route | Designed fresh. Graph skills and the MCP path are both inputs. |
| D3 | OAuth apps | Squrell owns the OAuth app registrations for Microsoft, Google and Slack. Customers never see a client id. The org admin consents for the organization once; each user connects their own account. |
| D4 | Admin gate | The admin of each node decides which marketplace items its users may use. Nothing reaches a member before the admin enables it. |
| D5 | Brokers | Third-party brokers are not ruled out; the bar is product stability and the risk of untrusted code on the node. First-party brokers are fine now. |
| D6 | MCP | MCP can be a catalog and supply product. The line "MCP is not marketplace supply" (`developer-experience` D20) is set aside for connectors. |
| D7 | First product | Microsoft 365: an org or an individual signs in, agents read and search Outlook mail and create calendar events. Later, Squrell's own identity sends and reads Teams messages. |
| D8 | What a connector is | **Connectivity only**: how a credential is obtained, whose it is, how the node reaches the system, how the credential is attached, and for MCP the server configuration plus a pinned, vetted tool list. A connector names no skill, playbook or agent. |
| D9 | MCP engine | The MCP client stays in VAM core (protocol, OAuth, refresh, reauth, pinning, fencing). Per-SaaS variation (URL, tenant, auth mode, headers, scopes, tools, risk classes) is connector data. Connections is the one place a user sees MCP, REST and database systems alike. |
| D10 | Direction | Dependencies point downward: agent → playbook → skill → connector. A skill declares `requiresConnector`. Jigs live inside the skill that owns the tool they render. |
| D11 | Bundling and discovery | A marketplace **listing** bundles (connector + skills + playbooks + agent) for one-click install. Discovery uses reverse lookup: a connector's page shows the skills, playbooks and agents that depend on it, computed from their references. |
| D12 | Databases | Approach recorded (§7); build parked. Until it exists, no database password goes into an egress secret or a skill. |
| D13 | Readable artifacts | An installed release is a full, readable, auditable copy on the node. Artifacts are not hidden from the node's admin. |
| D14 | Proprietary logic and data | Proprietary logic belongs behind a connector (the publisher's own service) or a broker. Data leaves the enterprise only through a connector the admin enabled. Squrell artifacts never send data off the node. |
| D16 | Marketplaces | Live outside VAM (`hub.squrell.cloud` first; others may speak the same protocol). The protocol is OCI-based. The node keeps trusted hubs, allow-lists, installs as VAM records. |
| D17 | Install | Pulls a signed, digest-pinned release into the node catalog under its publisher URN, read-only. Updates arrive as new releases per org policy. An org may fork into its own namespace at any time, which ends the upstream link. |
| D19 | Jigs | Minimal in version one: cards live inside skills. No separate jig catalog kind yet. |
| D20 | Scopes | One scope set per connector in version one. Writes are governed by tool defaults (`ask`) and agent rules, not by withholding scopes. |
| D21 | First connectors | `microsoft-365` (REST, Squrell public client) and `linear` (remote MCP) ship built into `vamd` for version one. The hub and OCI pull come after. |
| D22 | Teams | Later. Teams becomes a second adapter in `vam.notify` (built in notifications Phase 2), but its bot needs a public endpoint, so it also needs a Squrell-hosted relay (§6.4). |
| D23 | Marketplace for 0.1.0 (owner, 2026-11-09) | A temporary **Git marketplace**: a public Git repo with a `marketplace.json` index of listings. Squrell's is `squrell-inc/vam-marketplace` (public). Each node keeps a list of trusted marketplace URLs: the Squrell one preconfigured, others added by an org admin. The OCI hub (§9.3) later replaces Git as the source without changing the app page. |
| D24 | Code vs data (owner, 2026-11-09) | The `vam` repo holds code: the MCP engine, OAuth profiles, attach modes, brokers, the marketplace machinery, and the built-in connector files. Marketplace repos hold data: skills, playbooks, agents, and connector definitions once they are installable (D26). |
| D25 | Trust in 0.1.0 (owner, 2026-11-09) | Listings from non-Squrell marketplaces may install skills, playbooks and agents: read-only, an "Unverified" badge, a capability summary before install. Connectors come only built into `vamd` until Phase 5. Amended 2026-11-09 (owner, Phase 5 folded into Phase 4): a non-Squrell listing may also carry a connector within the limits of D26. |
| D26 | Installable connectors (owner, 2026-11-09) | Phase 5 makes a connector its own catalog kind (`urn:vam:connector`), installable from any trusted marketplace, so developers ship connectors without a Squrell release. A connector from a non-Squrell marketplace needs the admin's explicit approval of its capability summary (publisher, OAuth app, hosts, scopes, MCP server), shows "Unverified", cannot use a Squrell-reserved id, and an upgrade that widens any of these waits for re-approval. **Amended 2026-11-09 (owner): folded into Phase 4.** Connectors ship as files in marketplace listings, imported with the listing like skills, playbooks and agents, read-only with their origin, and returned by `GET /v1/connectors` beside the built-ins. The admin sees the connector's capabilities (publisher, client id, hosts, scopes, MCP server) before enabling, and again when an update changes them, which needs Enable again. Squrell-reserved ids (`microsoft-365`, `linear`) and unknown profiles are refused. Community connector files are limited to remote MCP with discovery, API-key providers, and the existing OAuth profiles with the publisher's own public client id. No bring-your-own client; a generic OAuth profile is a small follow-up later. |
| D27 | The org's own catalog (owner, 2026-11-09) | Unchanged. Publishers keep creating drafts and releasing at the org's registry domain, visible to the whole org; marketplace installs land in the same catalog beside them, read-only, with their origin (§9.2). |

## 1. The problem

People want outcomes from the systems they already work in (mail, calendar,
tickets, the finance database) without an integration project and without
handing an AI the keys. Today:

- **A connection is an MCP URL and an id the user types.** The app has a
  Connections view (`the VAM app/src/components/vam/McpConnectionInspector.tsx`)
  with static or OAuth auth, but no list of known providers and no admin
  gate. Only user scope is wired in the app.
- **Microsoft and Google do not connect.** Entra has no dynamic client
  registration and publishes a templated issuer; Google rejects custom URL
  schemes. A Squrell-owned app is required.
- **Building an integration is mostly auth and packaging**: device-code
  auth, token caches, digest mismatches, tool-name collisions and release
  deadlocks, not mail logic.
- **Connections belong to the user, not the agent.** Every MCP connection
  the user can see is seeded into every session
  (`crates/vam-orchestrator/src/mcp_registry.rs` `seed_mcp_skills`), and
  every tool defaults to "mutable external", so every mail read asks.
- **There is no marketplace.** Remote pull is refused
  (`skill_package_admit.rs`, "not a local catalog registry"); the marketplace
  domain only blocks publishing under it (`catalog_tier.rs`).

A connector turns "integrate system X safely" into a one-time, governed,
reusable fact on the node. Squrell does the hard part once per system; the
admin allows it once; each person connects once; every agent the org runs
can then use it under the org's rules.

## 2. Who owns what

Every artifact corresponds to one decision owned by one kind of person.

| Artifact | Decision | Owner | Changes |
|---|---|---|---|
| **Connector** | how the machine reaches system X on someone's behalf | Squrell, later trusted partners | rarely; security review and provider paperwork |
| **Connection** | whose account: a fact on a node, not a package | the user, gated by the admin | at run time |
| **Skill** | how to do a kind of work: tools as code, know-how, the cards those tools show | builders | often |
| **Playbook** | the procedure for a job | domain experts | per org |
| **Agent** | who the user talks to: identity, instructions, rules, model, references | builders, tuned by the org | most often |
| **Listing** | what an admin installs as one unit | hub curators | a view, not an artifact |

Rules that follow:

1. One artifact per decision, one owner per artifact.
2. Artifacts reference downward and never bundle. Pinned, released only.
3. A connector is published; a connection is a fact about a person on a
   node. Connectors carry no user data.
   A connection is also not a **contact**. A contact is where VAM reaches a
   person (a verified Slack account, later a Teams id or email; notifications
   §9.5, built in Phase 2). A connection is an account an agent acts with.
   They stay separate records: connecting Microsoft 365 does not create a
   Teams contact, and linking Slack for notifications does not let agents
   act in Slack.
4. Listings bundle; artifacts do not.
5. The end user sees two nouns: agents and connections.
6. Auth is platform work, capability is builder work, procedure is expert
   work. A skill never contains credentials or refresh logic.
7. Rules live on the agent (`agent-experience` D12). Connectors, admins and
   users may only tighten.
8. A new artifact kind must name the decision no existing kind owns.

```
Agent ──► Playbook ──► Skill ──► Connector          depends on; pinned, released only
                         └─ cards (jigs) for its own tools

Connection: user × connector × node, created when a person clicks Connect
Listing:    "install these released artifacts together", curated, admin-gated
```

## 3. The experience

### 3.1 The user's flow

```
 ADMIN (once per org)        MEMBER (once per person)        EVERY DAY
 Marketplace / Org settings  Connections
   "Microsoft 365"             tile: Microsoft 365
       │                           │
 Enable for org ───────────► Connect
   Microsoft admin consent     Microsoft sign-in
       │                           │
 Allowed ✓                   Connected as alice@contoso ✓
                                   │
                             "Triage my inbox, book 30 min with Raj"
                               reads mail, finds slots     (no prompt)
                               wants to send or book       (card: Approve?)
                                   │
                             Approve ─► done, receipt
```

The member never sees a URL, a client id, a token, a skill or a version.

States on a tile: **Connect** · **Connected as …** · **Needs reconnect** ·
**Ask your admin** · **Waiting for Microsoft admin approval**. Per tool on
the admin side: **Allow / Ask / Block**.

### 3.2 The builder's flow (Microsoft 365)

```
 SQURELL, once per provider              BUILDER, per capability
 Register Squrell app at Microsoft       Write a skill
   (publisher verification)                tools in code: mail_search, mail_send…
       │                                   call Graph with a placeholder header
 Write the connector definition            mark side effects, attach cards
   provider, scopes, who connects,         requiresConnector: microsoft-365
   reachable hosts                             │
       │                                 Test on a dev tenant ─► release skill
 Test sign-in ─► release connector
       └──────► Listing = connector + skill (+ playbook, agent) ─► review ─► hub
```

A builder writes no auth code. Skill code calls the provider with a
placeholder; the host does the rest (§5).

### 3.3 Walkthrough: Microsoft 365 at Contoso

1. Squrell publishes a "Microsoft 365" listing: the `microsoft-365`
   connector, the `m365-mail` skill, a "morning inbox triage" playbook and an
   "Executive assistant" starter agent.
2. The Contoso admin enables it. The node installs the four releases, records
   the install, and opens Microsoft admin consent once for the tenant. The
   admin can read every installed file.
3. A member clicks the Microsoft 365 tile and signs in.
4. The member asks the Executive assistant to triage the inbox and book a
   slot. Reads run freely; sending and booking ask with a card; every run has
   a receipt. Mail goes nowhere but Microsoft and the node.
5. Squrell ships skill 1.1. A patch applies by policy; a release that adds a
   scope or host waits for the admin to approve the diff.
6. A partner with proprietary matching logic for Oracle procure-to-pay
   publishes a connector to their own service. Contoso data reaches it only if
   the admin enables that connector.

In version one, steps 1 and 2 are simpler: the two connectors are built into
`vamd`, and the skill, playbook and agent are imported from Git (§10).

## 4. The connector definition

TypeScript is the readable form; the artifact is JSON validated by
`vam-spec`. Version one keeps only what the two built-in connectors need.

```ts
interface Connector {
  urn: `urn:vam:connector:${string}`;     // urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365
  version: string;
  displayName: string;                     // "Microsoft 365"
  publisher: string;
  description: string;
  icon: string;

  provider: OAuth2Provider | ApiKeyProvider;
  connection: {
    holder: "user" | "org";                // version one: "user"
    multiAccount: boolean;                 // version one: false (one account per user per node)
    label: string;                         // how the tile learns "alice@contoso.com"
  };
  bindings?: Binding[];                    // REST connectors: what skill code may send, to where
  mcp?: McpServer;                         // MCP connectors: the server and its vetted tools
}

interface OAuth2Provider {
  kind: "oauth2";
  profile: "entra" | "google" | "slack" | "discover";   // named quirk sets the app and vamd know
  clientId?: string;                       // Squrell-owned; absent for "discover"
  clientSecret: "none" | "squrell_relay";  // public client, or a secret only Squrell's relay holds
  scopes: string[];                        // one set (D20)
  orgConsent: "none" | "admin_required";
}

interface ApiKeyProvider { kind: "api_key"; fields: { name: string; label: string }[] }

interface Binding {
  name: string;                            // "graph"
  placeholder: string;                     // "vamsec_m365_graph"
  value: "access_token";                   // minted from the connection for this binding's audience
  inject: "bearer" | "header" | "query";   // "swap" mode (§5)
  hosts: string[];                         // exact hosts this value may be sent to
}

interface McpServer {
  url: string;                             // may contain {tenant}
  tools: { remote: string; sideEffect: "READ_ONLY" | "MUTABLE_LOCAL" | "MUTABLE_EXTERNAL";
           default?: "allow" | "ask" | "block" }[];
}
```

### 4.1 Microsoft 365 (REST, Squrell public client)

```jsonc
{
  "urn": "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365",
  "displayName": "Microsoft 365",
  "provider": { "kind": "oauth2", "profile": "entra", "clientId": "<squrell public client id>",
                "clientSecret": "none",
                "scopes": ["openid", "offline_access", "User.Read", "Mail.ReadWrite", "Mail.Send", "Calendars.ReadWrite"],
                "orgConsent": "admin_required" },
  "connection": { "holder": "user", "multiAccount": false, "label": "/me/mail" },
  "bindings": [ { "name": "graph", "placeholder": "vamsec_m365_graph", "value": "access_token",
                  "inject": "bearer", "hosts": ["graph.microsoft.com"] } ]
}
```

A skill that requires it:

```ts
// skills/m365-mail/tools/mail_search.ts  (runs in the guest)
const res = await fetch(`https://graph.microsoft.com/v1.0/me/messages?$search="${query}"&$top=${limit}`,
                        { headers: { Authorization: "vamsec_m365_graph" } });
```

```jsonc
// skills/m365-mail/manifest.json (excerpt)
{ "requiresConnector": "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365",
  "tools": [
    { "name": "mail_search", "sideEffect": "READ_ONLY",        "ui": { "show": "mail_list" } },
    { "name": "mail_send",   "sideEffect": "MUTABLE_EXTERNAL", "approvalRequired": true } ] }
```

### 4.2 Linear (remote MCP)

No bindings and no skill code: `vamd` is the MCP client and attaches the
token itself, so nothing reaches the guest. Discovery and registration work
today.

```jsonc
{
  "urn": "urn:vam:connector:hub.squrell.cloud/squrell/linear",
  "displayName": "Linear",
  "provider": { "kind": "oauth2", "profile": "discover", "clientSecret": "none",
                "scopes": ["read", "write"], "orgConsent": "none" },
  "connection": { "holder": "user", "multiAccount": false, "label": "whoami.email" },
  "mcp": { "url": "https://mcp.linear.app/mcp",
           "tools": [ { "remote": "list_issues",  "sideEffect": "READ_ONLY" },
                      { "remote": "get_issue",    "sideEffect": "READ_ONLY" },
                      { "remote": "save_issue",   "sideEffect": "MUTABLE_EXTERNAL", "default": "ask" },
                      { "remote": "save_comment", "sideEffect": "MUTABLE_EXTERNAL", "default": "ask" } ] }
}
```

What changes against today's `vam mcp connect linear`: the connection id is
fixed by the connector, so guest tool names (`linear__get_issue`) and rules
are the same on every node; read tools are vetted as read, so they stop
asking; the user clicks a tile instead of typing a URL.

### 4.3 GitHub (later; shows two holders)

The user holder is "act as me" (a user token, refreshable). The org holder is
"act as the Squrell app on these repositories" (an installation token minted
with the app's private key). Both need a secret, so a Squrell relay mints
them and the node stores them (§6.3). Skill code is the same as for
Microsoft 365. Whether GitHub's token exchange works with PKCE alone is to
be verified; if it does, the user holder needs no relay.

## 5. How secrets map

A connector's bindings name credentials, each tied to the hosts it may be
sent to and to how it is attached. Skill code only ever uses a placeholder.
At request time the host:

1. sees the placeholder in an outbound request from the guest,
2. checks the destination is one the binding allows,
3. finds the session owner's connection for that connector,
4. refreshes the access token if it is near expiry (single flight),
5. replaces the placeholder in the declared place.

```
skill code (guest)                        host egress proxy
  Authorization: vamsec_m365_graph  ───►  host = graph.microsoft.com ✓
                                          connection(alice, microsoft-365) ✓
                                          expires in 2 min → refresh
                                          Authorization: Bearer eyJ…  ───► Microsoft Graph
```

Refresh tokens, client secrets and private keys are never bindings. They
stay on the host and only `vamd` uses them, to mint access tokens.

Three ways to attach a credential; each connector declares one:

| Mode | How | Examples | Status |
|---|---|---|---|
| **Swap** | replace a placeholder with a token | Microsoft Graph, GitHub, Slack, Zendesk | the egress proxy does this today for static secrets (`crates/vam-network/src/secrets.rs` `phase2_inject_secrets`); refresh-before-swap is new |
| **Sign** | compute a signature over the request with a key the guest never sees | AWS APIs (SigV4), HMAC APIs | later |
| **Speak** | the host runs the protocol | Postgres, MySQL, SSH, SMTP | parked (§7) |

MCP connectors use none of the three: `vamd` is the client
(`mcp_registry.rs`, `host_tools.rs`), attaches the token, refreshes on 401
and parks the run on reauth (`api/reauth.rs`).

**Security floor before any of this ships.** MCP connection credentials are
stored today as ordinary `SecretKind::Egress` rows
(`mcp_registry.rs` around `store_mcp_secret`), and
`session_egress_bindings_for_caller` (`secrets.rs`) hands every egress row to
the session's egress broker with a convention placeholder. A guest script can
therefore send `vamsec_<conn>_oauth_access_token` to the MCP host and have
the real bearer injected, bypassing per-tool Cedar, approval and the pinned
tool list. This contradicts `docs/.../runtime/mcp.md` ("the `vamsec_`
placeholder is never used for MCP"). **Fixed on `main`
(DX-139, 2026-11-07):** a reserved vault
kind `connection` (proto `KIND_CONNECTION = 3`) for MCP credentials, excluded
from guest egress bindings and not writable through the secrets API, plus
`decrypt_secret_for_broker`, which never hands a broker a `connection` row
and refuses rows pinned outside its approved egress domains. That kind is
the natural home for the connector credentials of §5 and Phase 2: refresh
tokens and per-audience access tokens are `connection` rows, never guest
bindings.

## 6. Microsoft 365 specifics

### 6.1 Constraints (from research, 2026-11-04; verify on a test tenant)

1. Entra has no dynamic client registration; the client id is Squrell's,
   pre-registered.
2. The `/organizations` metadata issuer is the literal
   `https://login.microsoftonline.com/{tenantid}/v2.0` and advertises no
   PKCE methods. The app's strict issuer check (`oauthDiscovery.ts`) must not
   run for the Entra profile; validate the token's tenant instead.
3. Do not send the RFC 8707 `resource` parameter to Entra; the audience goes
   in `scope`. `vamd` sends `resource` on refresh today
   (`mcp_oauth_refresh.rs`); the profile turns it off.
4. Assume admin consent is required for mail, calendar and Teams scopes
   under Microsoft's 2025–26 default consent policies. Never send
   `prompt=consent` on user sign-in.
5. **Publisher verification** (a Microsoft Partner Global Account id) is a
   launch prerequisite: without it users cannot consent to a multi-tenant
   app. External work; start now.
6. Register a **public client** under "Mobile and desktop applications"
   (PKCE, no secret). Refresh tokens last 90 days there, not the 24 hours of
   the browser-app platform. `squrell://oauth` works for Entra.
7. Device-code flow is blocked by a Microsoft-managed Conditional Access
   policy in many tenants. Do not depend on it.
8. Outlook throttling is per app per mailbox (10,000 requests per 10 minutes,
   4 concurrent). Serialize per mailbox.
9. Refresh tokens can be revoked at any time (password change, admin
   revoke). The "Needs reconnect" path is a normal path.

### 6.2 Why not Microsoft's hosted MCP servers

Microsoft's hosted MCP servers are not the base: the per-app servers are
labelled legacy, and their successor needs an admin enable step and has
undocumented use by a multi-tenant app. A Squrell-hosted Graph MCP server was
rejected: mail and tokens would transit Squrell's cloud, which contradicts
D14.

### 6.3 Providers that need a secret

Google and Slack token exchange needs a client secret, which must not ship
to nodes. A stateless Squrell relay does the exchange and refresh and hands
tokens to the node, keeping no copy. Home Assistant's account linking is the
precedent. Google Gmail scopes also need an annual security assessment
(CASA). A distributed Slack app needs Marketplace approval for reasonable
rate limits.

Two Slack apps must not be confused. The **notification channel** shipped
in notifications Phase 2 uses a Slack app the operator creates in their own
workspace (Socket Mode, tokens in the org vault deploy note). It is
internal to that workspace, so the distributed-app rate limits do not apply,
and it stays that way for 0.1.0. A **Slack connector** (agents reading and
posting in a user's Slack, D3) would use a Squrell-owned app and is later.

### 6.4 Teams as a channel (later, D22)

Squrell's own identity in Teams must be an Azure Bot with a Teams app; Graph
cannot send chat messages as an app outside migration mode. On `main`,
`vam.notify` (`brokers/vam-notify`) already has the adapter registry, the
neutral rendering, verified contacts, single-use action handles and
`decide_as`; Teams is a second adapter in that crate. What Slack does not
need and Teams does: the bot needs a public HTTPS messaging endpoint (no
equivalent of Socket Mode), so Teams traffic goes through a Squrell relay
service that nodes reach outbound. New repo, new trust boundary.

### 6.5 Events

"When a new email arrives" uses delta queries from a scheduled run first
(`vam.schedule`, 15-minute floor), which needs no inbound endpoint. Graph
change notifications through a relay are later. The general model for
events is §6.6.

### 6.6 Events that wake the machine (inbound, later)

Connectors are the outbound half: agents call systems. Events are the
inbound half: a system wakes an agent. Listeners, sockets and clocks are a
broker's job (`agent-runtime` D15), so an event source is always a broker,
first-party while third-party brokers may not hold grants.

**What `main` already has (2026-11-07):**

| Piece | State | Where |
|---|---|---|
| Run grants: a person approves, `vamd` mints a Credential bound to agent, Space, thread and message, held by one broker | built | `grants.rs`, `credentials.rs` |
| Trigger gateway: the broker calls `RequestRunTrigger`; `vamd` checks the grant and starts the run through the one start path, as the user | built | `gateway.rs`, `api/run_start.rs` |
| `FIRE_KIND_EVENT` in the fire kinds | exists, accepted and labelled | `hosted_services.proto` `FireKind` |
| `event_context` (what happened) on a trigger | **reserved, not built** | `hosted_services.proto` field 8 |
| `DeliverEvent` and resume grants (wake a waiting task) | designed, not built | `design/hsp-triggers.md` amendment §2 |
| Inbound Slack over Socket Mode (no public endpoint), verified contacts mapping a Slack user to a VAM user, single-use handles | built for **deciding** approvals and gates, not for starting runs | `brokers/vam-notify`, `notify_gateway.rs` |

So today a broker can wake a run, but cannot say what the event was, and
Slack can resume a parked run but cannot start one.

**The model, reusing what exists:**

```
 SUBSCRIBE (once, a person approves)          FIRE (every event)
 "When a PR opens in squrell/vam,             GitHub ─► relay or poll ─► vam.github
  review it"                                     │ RequestRunTrigger(kind=EVENT,
   agent calls github_subscribe(...)             │   event_context={repo, pr, action},
   approval card ─► vamd mints a run grant       │   idempotency_key=delivery id)
   bound to reviewer agent + Space               ▼
   broker stores grant, subscribes upstream   vamd: grant ✓, dedupe ✓, loop guard ✓
                                                 start run as the user; the event is
                                                 untrusted input, fenced like vendor text
                                                 agent uses connector tools (read diff),
                                                 posts the review with ask, receipt
                                                 names the trigger
```

Rules: identity comes only from the grant (`work-outside-chat` D3); the
event payload is data, never instructions; a triggered run cannot arm
another trigger (D6, loop guard); one grant per subscription, revocable
from the app; the run uses the subscriber's connections.

**Transports, cheapest first:** Slack Socket Mode (built in `vam.notify`);
polling an API from the broker (GitHub events, Graph delta), which needs a
token at the broker, the same Phase 2 connection work; webhooks through the
Squrell relay that Teams and Google already need, delivered to the node over
its outbound connection.

**What to build, later than the minimal version:** un-reserve
`event_context` with a fenced payload in the run inputs; grant minting from
a subscription (an agent grant tool, and later an "Automations" view in the
app); an optional `events` list on the connector definition (name, payload
schema, the broker that listens), so listings can say "can wake an agent";
the first event source. **Slack mentions are the cheapest first source**:
transport, identity mapping and the gateway socket exist in `vam.notify`.
GitHub pull requests second, on the relay.

Phase 1 needs no change for this. It should keep the connector definition
open to an `events` field and make sure a triggered run resolves the
subscriber's connections like any other run.

## 7. Databases (parked, D12)

A database cannot use header swapping: the password is never sent (SCRAM and
other challenge-response schemes), TLS starts inside the protocol, and risk
is per statement, not per request. The host must speak the protocol.

Recorded approach, in order:

1. **Host-side tools**: `db_describe`, `db_query` (read-only transaction,
   row limit, timeout, SELECT only), `db_explain`, and **business-level
   write tools** with fixed statements written by the builder (an approval
   card says "Refund 40 USD on order 1182", not a block of SQL). One SQL
   runner serves RDS, Aurora, Azure SQL and on-prem Postgres and MySQL; each
   further database is a connector definition.
2. **A wire proxy** for script-heavy data agents later: the guest uses a
   real driver against a local socket with a dummy password; the host
   authenticates upstream. Coarser governance (read-only user, statement
   logging).

The connection for a database is held by the org, and the admin grants it
to agents or groups. The node needs a network path (often a private VPC).

## 8. Cards from skills (the minimal Jig piece)

A tool can name a card. When the tool returns, the host attaches
`{jig, data: result}` to the outbox card, so the model does nothing extra.
When a tool that asks for approval is suspended, it can name a richer
approval card than `vam.approve`, with the arguments bound in; an edited
submit is the existing AMENDED decision (`proto/vam/v1/hitl.proto`).

```jsonc
// on a ToolSpec in a skill manifest
"ui": { "show": "mail_list", "ask": "send_approval" }
```

```text
// skills/m365-mail/jigs/mail_list.jig, source form from design/jig.md §1.2
rows = List($data, "id", [Text(item.subject, {emphasis: item.unread}), Text(item.from), Text(item.at)])
root = Stack([Text("Messages", {emphasis: true}), rows])
```

What version one needs from Jig (`spec/jig/catalog.json`, `vam/1`):

- A `List` primitive (repeat over an array with an item template). Named in
  `design/jig.md` Level 1, not in the catalog today. Probably `Link` too.
- `ToolSpec.ui` in `proto/vam/v1/skills.proto` and its validation.
- Jig sources in the skill package, compiled and checked at publish
  against the catalog and the tool's output schema.
- Host: attach the card on tool result; app: render it in the thread with
  the existing `JigRenderer`.

Cards in channels need no new rule. The notifications jig rendering rule
(NT-2.10, on `main`) sends a card natively to Slack only when every node is
in the adapter's primitive set and the only action is a submit; otherwise it
sends the title, body and **Open in VAM**. Skill cards follow the same rule.
A new `List` primitive renders in the app at once and falls back to Open in
VAM in Slack until the Slack adapter learns it.

Not in version one: `Query` (live cards), App pages, component libraries,
jig packages as a catalog kind (D19).

## 9. The marketplace

### 9.1 Version one: a Git marketplace (D23–D25)

A marketplace is a public Git repo with an index:

```
vam-marketplace/
  marketplace.json
  listings/microsoft-365/
    skills/m365-mail/          (skill package with its cards)
    agents/executive-assistant/
    playbooks/morning-triage/
```

```jsonc
{ "name": "Squrell marketplace",
  "listings": [
    { "id": "microsoft-365", "displayName": "Microsoft 365", "description": "…",
      "connector": "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365",
      "artifacts": ["skills/m365-mail", "agents/executive-assistant", "playbooks/morning-triage"],
      "ref": "microsoft-365-v1.0.0", "requiresNode": ">=0.1.0" } ] }
```

- **Trust**: each node keeps a list of trusted marketplace URLs. The Squrell
  one is preconfigured; an org admin adds others. Each has its own origin
  badge and is refreshed on its own.
- **Browse**: the app's Org settings Marketplace page reads each index and
  shows the listings, with what each installs and can reach.
- **Enable**: `vamd` imports the listing's artifacts at the pinned tag
  (`vam catalog import <git url> --path <dir> --ref <tag>` is the same
  path from the CLI) and admits them as released, read-only rows with their
  origin: Squrell content under `hub.squrell.cloud/squrell/...` with the
  Squrell badge; other marketplaces with "Unverified".
- **Update**: a newer tag shows "update available"; the admin updates.
- **Release**: Squrell merges to `vam-marketplace`, CI validates every package
  with a pinned `vam` CLI, and a per-listing tag (`microsoft-365-v1.0.0`)
  is the release. Air-gapped nodes mirror the repo.
- **Why a separate repo**: the marketplace URL is what every node trusts, so
  it is sticky; content releases on its own cadence.

### 9.2 How the org's own catalog and marketplaces fit together (D27)

```
 ORG PUBLISHERS (today, unchanged)            MARKETPLACES (new)
 draft ─► release at the org domain           admin trusts a marketplace URL
   urn:vam:skill:acme.example/finance/…       admin enables a listing
       │  visible to the whole org               │ vamd imports at the pinned tag
       ▼                                         ▼
 ┌──────────────────── the node's one catalog ────────────────────┐
 │ Org releases (editable by their owner) │ Installed (read-only)  │
 │ badge: Org                             │ badge: Squrell / Unverified │
 └────────────────────────────────────────────────────────────────┘
       members see released rows of both; drafts stay private to their owner
```

- A marketplace listing is only an entry on the Marketplace page until an
  admin enables it. Nothing reaches the catalog before that.
- Enabling creates catalog rows on the node: the artifacts are copied in and
  digest-pinned (D17). Members then see them like any released artifact.
- Org publishers can build on installed artifacts: an org agent may pin an
  installed Squrell skill (it is a released dependency), or fork an installed
  artifact into the org's namespace, which makes an editable copy with a new
  URN and no upstream link. Overlays without copying wait for `extends` at
  run time (§9.3).
- To share beyond their org, a publisher pushes to a marketplace repo of
  their own (or to Squrell's, by pull request), and other orgs add that URL.

### 9.3 Later: the hub (after 0.1.0)

- **Hubs** are services outside VAM: listings, publisher accounts, review,
  signatures. The community lives there.
- **The protocol is OCI**, the standard behind container registries. Each
  release is an OCI artifact with a VAM media type; the URN maps to the
  registry name (`urn:vam:skill:hub.squrell.cloud/squrell/m365-mail@1.2.0` ↔
  `hub.squrell.cloud/squrell/m365-mail:1.2.0`); the existing SHA-256 digest
  stays the identity; signatures attach as OCI referrers. Customers can
  mirror a hub inside an air-gapped network with Harbor or Artifactory.
- **The node** keeps trusted hubs, allow-lists of publishers and artifacts,
  installs (URN, channel, pinned digest, source hub).
  `vamd` pulls, verifies, and admits the release through the existing release
  path with its origin.
- **Install is a pull, not a transfer of ownership** (D17). Updates follow
  org policy (auto-patch, auto-minor, manual). A release that adds a
  connector, scope, host or tool waits for admin approval with a diff. A
  yanked release blocks new installs.
- **Org customisation** of an installed agent goes through an overlay:
  appended instructions, tighten-only rules, its own connector bindings. This
  needs `extends` merged at run time (`crates/vam-spec/src/extends.rs`
  `merge_agent_chain` exists; nothing calls it).

## 10. Phases to the minimal version

| Phase | Outcome | Builds | Rubric (proposed) |
|---|---|---|---|
| **0 Security floor and paperwork** | nothing visible | **Done 2026-11-07.** DX-139 merged as (also covers the HTTP host adapter and notify tokens). The Squrell Entra app and publisher verification are owner items carried into Phase 1's real-tenant wave (`.cursor/workstreams/connectors.md`). | Security floor: done |
| **1 Connectors on the node** | Admin enables a connector and consents; members see tiles, connect, see status. Linear works as a connector. | Connector definition in `vam-spec`; `microsoft-365` and `linear` built into `vamd`; `GET /v1/connectors` (merged with node enablement); `node_enabled_connectors` with consent record and `PUT` for org admins (`users:admin`: enabling is the org admin's decision, D4, unlike the operator-only Notifications place); Org settings "Connectors" place; daily Connections tiles; MCP connectors with fixed id and vetted risk classes; Entra profile in the app's OAuth flow (no issuer check, no `resource`, `offline_access`, tenant from consent). | Proof: P6 line below. Cost 2. |
| **2 The connection behind skills** | A skill reaches Microsoft Graph as the user without holding a token. | Account-kind connection (refresh token pinned to the login host, per-audience access token); bindings from the connector become session egress bindings; refresh-before-swap in the egress provider (reusing `mcp_oauth_refresh.rs`); reauth park for egress calls; `connection.needs_reauth` notification kind (not on `main` yet; once recorded, it reaches the app and, through `vam.notify`, a Slack DM); `requiresConnector` on skills and a "connect to continue" park. **Per-call egress envelope (owner, 2026-11-07):** each code tool that uses a connection binding declares its egress shape (host, methods, path prefixes); the egress provider attaches the user's token only to a request inside the calling tool's declared shape and refuses the rest, so a tool declared as a read cannot POST a send (question 8, finding F8). | Cost 2–3: amends three lines of `design/mcp-oauth-connections.md` (§12). |
| **3 Cards from skills** | Tool results render as cards; send and book ask with a richer card. | `ToolSpec.ui`; `List` primitive in `vam/1`; jig sources in skill packages, checked at publish; host attaches cards; app renders them. Can run as a parallel lane with Phase 2 (different files). | Proof: P14. Cost 2. |
| **4 Microsoft 365 in use** | The walkthrough on a real tenant, enabled from the marketplace. | The Git marketplace (§9.1): trusted marketplace URLs per node, index reading, listing import at a tag, Squrell-domain admission, "update available", the Org settings Marketplace page; `squrell-inc/vam-marketplace` bootstrapped with CI; the `m365-mail` skill, the Executive assistant starter agent, the morning triage playbook on `vam.schedule`. Built-in connectors loaded from every file in `spec/connectors/`; import code generic across artifact kinds. **Phase 5 folded in (owner, 2026-11-09):** connectors ship as files in listings (D26 as amended), the install summary shows a connector's capabilities, an update that changes them needs Enable again. | Cost 3: a new repo, an app page, third-party connector files. |
| **5 Installable connectors** (folded into Phase 4, owner 2026-11-09; what remains below is later) | Developers ship connectors from any marketplace without a Squrell release (D26). | Connector as a catalog kind (`urn:vam:connector`, draft and release, digest), imported like other artifacts; `GET /v1/connectors` merges built-in and installed; qualified connection ids and Squrell-reserved ids; the admin's approved snapshot of each connector's capability summary, and re-approval for an upgrade that widens it; a generic OAuth profile with "bring your own client" (the admin enters a client id and, if needed, a secret kept in the vault); `vam connector init`, `validate`, `test`. What still needs Squrell: its own OAuth apps, new auth mechanics (profiles, relays, signing, databases), brokers. | Cost 3: third-party connectors are a new trust boundary. About 5–6 waves. |
| Later | | Hub and OCI pull, reverse lookup; Teams relay; Google with the secret relay; databases; verified publishers (namespace proof, signing); GitHub. | |

About four phases to the minimal version. With Phases 2 and 3 in parallel
lanes, it is three steps in sequence after Phase 0. The Microsoft paperwork
runs alongside from the first day.

**Benchmark lines to add** (`design/benchmark.md`), so the rubric can score
this work:

- P6: minutes and clicks from "Enable" to the first successful mail read on a
  real tenant, by a member with no instructions.
- P6/P13: a scheduled Microsoft 365 run survives seven days unattended with
  zero reconnect prompts; on a forced revoke it parks, the user reconnects,
  and it resumes with zero duplicate effects.

## 11. Open questions

1. ~~A connector as its own catalog kind or a skill package with a provider
   section?~~ **Decided (owner, 2026-11-09): its own kind,
   `urn:vam:connector`, installable in Phase 5 (D26).**
2. Where connector settings live (timezone, default calendar): on the
   connection row seems right.
3. Org-held connections (a shared mailbox, a database): grant to agents, to
   groups of people, or both?
4. Who may run a Squrell relay and where (region)?
5. (Withdrawn.)
6. Can partner hubs sign releases themselves, or does Squrell countersign?
7. Revocation latency for air-gapped nodes: how long may an offline node
   run after a release is yanked?
8. ~~The per-call egress envelope~~ **Decided (owner, 2026-11-07): yes, in
   Phase 2.** A code tool's declared egress shape (host, methods, path
   prefixes) is enforced per call for connection bindings, so a tool declared
   as a read cannot POST a send. Reason: Phase 4 imports skills from Git, so
   the gap between declared and enforced side effects (finding F8) opens with
   Phase 2, not with community skills.
   **Exact paths (Phase 4, CN-4.3):** a shape may also list `paths` (exact, no
   trailing slash); a request matches only a path equal to one of them. Writes
   are declared this way, so a create at `/v1.0/me/events` cannot reach
   `/v1.0/me/events/{id}/forward`. Prefixes stay for reads; publish warns on a
   write method declared by prefix.

## 12. Amendments to other designs (the first bullet applied 2026-11-07 with the owner's approval; the rest proposed)

- `design/mcp-oauth-connections.md`: "No generic OAuth broker for non-MCP
  egress", "No proactive background refresh scheduler" and "Refresh owner:
  vamd, triggered by 401" are amended for connector bindings: `vamd` mints
  per-audience access tokens and refreshes before swap. "Client
  registration: DCR first, CIMD fallback" gains a profile client id ahead of
  DCR. "No token passthrough" still holds: a token goes only to its audience.
- `.cursor/workstreams/developer-experience.md` D20 ("MCP is a platform
  ability, not marketplace supply"): set aside for connectors by D6.
- the 0.1.0 launch plan: the marketplace backend stays cut from 0.1.0;
  version one of connectors uses built-in connectors and Git import, which
  matches "install-from-Git plus a curated gallery".
- `proto/vam/v1/hosted_services.proto`: `CapabilityManifest` field 11 is now
  `notification_channel` (notifications Phase 2). Any connector field added
  to the broker manifest starts at 12.
- `design/jig.md`: cards inside skills (D19) is the first use of jig sources
  in a package; jig packages as a separate kind stay in Phase 3 of that
  design.

## Appendix A. Options considered and set aside

| Option | Why set aside |
|---|---|
| Every connector as a broker | A broker is an extension of the machine, rare and trusted. Most SaaS calls are one HTTP request; a marketplace of brokers would put code on every node. |
| Declarative HTTP tools in the connector (a template and projection language) | Removes code but limits what builders can do; skills as code already work. Kept on the shelf as an optional execution kind for host-enforced side effects. |
| Connector bundles its skills, playbooks and jigs | Many skills from different authors use one connector; every new skill would force a connector release with security review. The listing bundles instead (D11). |
| Microsoft-hosted MCP as the base | Admin enable step, legacy servers (§6.2). |
| Squrell-hosted Graph MCP | Mail and tokens transit Squrell (D14). |
| Device-code sign-in | Commonly blocked by Conditional Access; an agent-driven handshake also confuses models. |
| Protected (hidden) artifacts on the node | The node's admin is root; prompts are extractable; protection would be deterrence only. Private logic goes behind connectors (D13, D14). |
| Raw `execute(sql)` for database writes | Unreviewable approvals; business-level write tools instead. |
