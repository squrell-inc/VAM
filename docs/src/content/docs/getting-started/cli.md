---
title: VAM CLI Command Reference
description: Complete reference for the vam command-line client — managing runs, approvals, secrets, sessions, keys, and catalog specs.
---

The **`vam`** binary (`crates/vam-cli`) is a control-plane client that communicates exclusively with the `vamd` daemon via HTTP REST APIs. It enforces a strict security boundary by never directly touching Firecracker, cgroups, or host SQLite databases.

---

## Global Flags & Environment

Every `vam` subcommand supports global flags:

| Flag | Env Variable | Description |
|---|---|---|
| `--host <URL>` | `VAM_HOST` | URL of the `vamd` orchestrator (default: `https://127.0.0.1:8080`). `vamd` serves TLS by default; use `http://` only for a node started with `--insecure-dev`. The CLI warns on stderr before sending an API key over `http://` to a host that is not loopback. |
| — | `VAM_CA_FILE` | PEM file with the node's certificate to trust (overrides `vam config set-ca`). For a node with a self-signed certificate, copy `<state dir>/tls/ca.pem` from the node and compare the fingerprint with the one `vamd` logged. |
| `--token <KEY>` | `VAM_API_KEY` | Bearer credential. Overrides a session token from `vam login` and a saved `api_key`. |
| `--json` | — | Machine-readable output. See [Scripting and agents](#scripting-and-agents). |
| (on run create) | `VAM_TRACEPARENT` | Optional W3C `traceparent` header on `vam run`, `vam run --file`, and `vam run --workspace`. Invalid values are ignored by `vamd` (not auth). |

Credential resolution order: `--token` → `VAM_API_KEY` → `session_token` from `~/.config/vam/config.toml` (written by `vam login`) → `api_key` from `vam config set-key`. There is **no OIDC/SAML/SSO** yet.

`vam --version` prints the crate version and the git commit it was built from, for example `vam 0.1.0 (93eb949+dirty)`. `+dirty` means the working tree had uncommitted changes at build time. Use it to confirm the CLI matches the `vamd` you are checking. A build without git prints `unknown` in place of the commit.

### Scripting and agents

**`--json` output schema (stable).**

- Success: stdout holds exactly one compact JSON document. For API commands it is the `vamd` response body unchanged. A non-JSON body is wrapped as `{"raw": "<text>"}`. Local commands emit: `login` → `{"username","expiresAt","config"}`; `logout` → `{"loggedOut":true,"config"}`; `config set-host|set-key` → `{"saved":"host"|"api_key","config"}`; `config set-ca` → `{"saved":"ca_file","caFile","fingerprintSha256","config"}`; `config show` → `{"host","caFile","apiKey","sessionToken","sessionExpiresAt","config"}` (secrets redacted, unset is `null`); `evidence export` → `{"runId","dir"}`; `evidence verify` → `{"verified":true,"dir"}`; `receipt show --json` → the signed receipt document as stored (`evidence receipt` is an alias, removed after 0.1.0); `receipt verify --json` → `{"verified":true,"keyId","publicKey","pinned"}`; `workspace cat -o … --json` → `{"workspaceId","path","sizeBytes","out"}`; `run --dry-run` → `{"dryRun":true,"started":false,"resolved":{"agent","policy":{"urn","source","warning"},"playbook","skills":[{"urn","version"}]},"tools":[{"name","decision","matchingPolicies","diagnostic"}]}`. `decision` is `allow`, `deny`, or `hitl`. `matchingPolicies` is the Cedar policy set that decided that row.
- Streaming (`run events --follow`, `vam run --follow`, `vam run --file --follow`): one JSON object per line (NDJSON).
- Failure: stdout holds `{"error":{"code":"<name>","exitCode":<n>,"message":"<text>","httpStatus":<n|null>}}`.
- Progress lines (`[vam] ...`) always go to stderr, never stdout.
- New fields may be added. Existing fields are never renamed or removed.

**Exit codes (stable).** Scripts must branch on these, not on message text.

| Code | Name | Meaning |
|---|---|---|
| 0 | `ok` | Success |
| 1 | `failure` | Other error |
| 2 | `usage` | Bad flags or arguments, or a prompt was needed with no TTY |
| 3 | `auth` | Missing credential, or HTTP 401/403 |
| 4 | `not_found` | HTTP 404 |
| 5 | `conflict` | HTTP 409 (for example, a run is already active) |
| 6 | `invalid` | HTTP 400/413/422 validation error |
| 7 | `server` | HTTP 5xx |
| 8 | `unreachable` | Could not connect to `vamd`, or timed out |
| 9 | `verify_failed` | `vam evidence verify` found a bad pack |

**Errors name the fix.** Failure text does not include a raw HTTP status. It has three lines:

```text
Wrong: <what was rejected>.
Expected: <what a successful call needs>.
Fix: <the command to run>.
```

`--json` still includes `httpStatus` for scripts that already read it. Branch on the exit code. Read `message` for what to do next.

**Idempotency.** Every mutating command accepts `--idempotency-key <KEY>`. Omit it and `vam` mints a new key. Pass the same key to retry that write without creating a second resource. `vam session create` also accepts `--idempotency` as an alias of `--idempotency-key`.

Mutating commands: `vam keys create`, `vam keys revoke`, `vam secrets set`, `vam secrets delete`, `vam agent put`, `vam agent release`, `vam agent permissions set`, `vam agent migrate`, `vam playbook put`, `vam playbook release`, `vam policy put`, `vam policy release`, `vam skill put`, `vam skill publish`, `vam skill release`, `vam session create`, `vam session pause`, `vam session resume`, `vam session terminate`, `vam session snapshot`, `vam session restore`, `vam run`, `vam run start`, `vam run cancel`, and `vam approve decide`, and `vam users create`.

**No TTY, no prompts.** When stdin is not a terminal, or `--json` is set, `vam` never prompts. It exits `2` and names the flag to use:

| Prompt | Flag |
|---|---|
| `vam login` username | `--username <NAME>` |
| `vam login` password | `--password-from-env <VAR>` |
| `vam config set-key` API key | `--from-env <VAR>` |

`vam` has no spinners. Help colour is off when stdout is not a terminal or `NO_COLOR` is set, so output piped to a file has no ANSI codes.

`vam agent permissions set <agent> -f -` reads the permissions JSON from stdin and does not prompt, so it works when stdin is a pipe. `vam policy explain`, `vam agent permissions get --effective`, and `vam agent migrate` print a human report by default; pass `--json` (or the global `--json`) for the `vamd` response. `vam agent permissions get` without `--effective` is always JSON (the owner's draft). A validation failure from `vam agent permissions set` prints the server's AX-5.1 message unchanged and exits `6`. `vam agent migrate` exits `5` when unmapped tool names block the migration.

---

## 0. Login (`vam login` / `vam logout`)

Exchange a username + password for a short-lived session token. No email. Create the first admin headlessly (`vamd --bootstrap-admin <username>` + `VAMD_BOOTSTRAP_ADMIN_PASSWORD`) or set a password with `vamd --reset-password` (no SMTP, no UI). Which user to log in as (operator key vs org admin vs publisher vs member): [Users and roles](/getting-started/users-and-roles/).

```bash
# Interactive username + password prompt
vam login

# Non-interactive (CI / scripts)
vam login --username local --password-from-env VAM_PASSWORD

# Subsequent commands use the stored session token
vam health
vam node status

# Revoke the server-side session and clear local tokens
vam logout
```

`vam login` writes `session_token`, `refresh_token`, and `session_expires_at` to `~/.config/vam/config.toml` (mode `0600`). If the access token is expired and a refresh token is present, the CLI refreshes before the next call. `VAM_API_KEY` still wins when set. After sign-in, `GET /v1/me` returns `userId`, `username`, `role`, and the bearer's scopes (members included).

### Create a user (`vam users create`)

An org admin with the `users:admin` scope creates a user. The password comes from an environment variable named by `--password-from-env`. It is never an argument, so it stays out of shell history and `ps` output.

```bash
export NEW_USER_PASSWORD='…'   # set it in your shell; never commit it
vam users create --username ops-bot --role publisher --password-from-env NEW_USER_PASSWORD
```

`--role` is `member`, `publisher`, or `org_admin`. `operator` is not offered here. The command sends an `Idempotency-Key` and prints the created account as JSON (`user`, which has no password). A username that already exists is refused.

On a single-user node, the implicit owner is seeded user **`local`** (`member` role) — same SQLite schema and owner filters as multi-user; only one human owner is expected. The bootstrap **`operator`** key is for daemon administration (`keys:admin`, `brokers:admin`), not a second isolation path.

---

## 1. Runs (`vam run`)

Manage autonomous agent prompt executions and playbook runs.

| Subcommand | Description |
|---|---|
| `vam run <AGENT> -p "..."` | Run an installed agent. No `run.json`. `<AGENT>` is a short name or URN |
| `vam run <AGENT> --playbook <URN> --input key=value` | Run that playbook. Inputs are checked against its `inputSchema` |
| `vam run <AGENT> --dry-run` | Resolve refs and print allow / deny / HITL for each tool. Starts nothing |
| `vam run --file <FILE>` | Reproducible / CI run from a run file or directory |
| `vam run --workspace <WS> --thread <T> -p "..."` | Continue a thread. No agent and no run file |
| `vam run ls` | List recent agent runs with status |
| `vam run get <RUN_ID>` | Get detailed status, timestamps, and error traces |
| `vam run events <RUN_ID>` | Stream or print raw CloudEvents for a run |
| `vam run trajectory <RUN_ID>` | Print the CAS trajectory replay (event type / tool / disposition only) |
| `vam run compare <A> <B>` | Two GETs + local diff of trajectory replays (`equal`, `onlyA`, `onlyB`) |
| `vam run cancel <RUN_ID>` | Request cancellation and abort active VM session |

### Run an installed agent

```bash
# Short name. The last segment of an installed agent URN.
vam run jira-assistant -p "Summarize open P1 issues"

# The same run, by URN. The host pins the latest released version.
vam run urn:vam:agent:local.vam.dev/vam/jira-assistant --prompt "Summarize open P1 issues" --follow
```

No `run.json` is read. `vam` asks the catalog (`vam agent ls`) for the short name, then posts a run whose `agentRef` is that URN. The workspace is `volume://<short-name>`.

If the name matches more than one installed agent, the command exits `2` and lists the URNs. Pass one of those URNs. If it matches none, the command exits `4` and tells you to run `vam agent ls` or `vam agent put <agent.json>`.

A positional path is not a run file. `examples/jira-assistant/run.json` and `run.json` exit `2` and name `vam run --file`.

### Reproducible run (`--file`)

```bash
# CI / batch: the run file is explicit.
vam run --file examples/jira-assistant/run.json --follow

# A directory that contains run.json, with a prompt override.
vam run --file ./examples/bun-assistant --prompt "List all files in workspace" --follow
```

`--file` is the only way to pass a run file. `vam run start <path>` exits `2` and tells you to use `--file`. `vam run start --file <path>` still loads that file.

### Continue a thread

```bash
vam run --workspace assistant --thread feature-login -p "Summarize your previous changes"
```

This posts a run with `workspaceRefUri` `volume://assistant`, `threadRef` `feature-login`, and the prompt. There is no `agentRef` and no `run.json`. `--thread` defaults to `main` when you pass `--workspace`. `--model <NAME>` sets `inputs.model` on that continue; it works on a first launch too (`vam run <agent> --model <NAME> -p "..."`).

`vam run continue` is not a command. It exits `2` and prints the `--workspace` form. There is no prompt: pass `-p` / `--prompt`. Without it, the command exits `2`.

### Playbook run

```bash
vam run jira-assistant \
  --playbook urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup \
  --input project_key=PROJ \
  --input sprint_name="Sprint 42" \
  --input cutoff_days=14
```

`--playbook` is a playbook URN. Each `--input` is `KEY=VALUE`. `vam` loads the playbook, checks the pairs against `inputSchema` (`string`, `integer`, `number`, `boolean`, `required`, and `additionalProperties: false`), and posts a run with that `playbookRef` and `inputs`. A missing required field or a bad type exits `6` and does not start a run. The playbook must be allowed by the agent (`allowAnyPlaybook` or `allowedPlaybookRefs`). `--input` without `--playbook` exits `2`.

`-p` is optional. When you pass it, the prompt is the input named `prompt`, and it has to fit the schema too.

### Dry run

```bash
vam run jira-assistant --dry-run
vam run jira-assistant --playbook urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup \
  --input project_key=PROJ --input sprint_name="Sprint 42" --input cutoff_days=14 --dry-run
```

`--dry-run` resolves the agent, its skills, and its capability policy, then evaluates each tool. The table lists `allow`, `deny`, and `hitl` (human approval required before the tool runs). The policy ids are the Cedar `matchingPolicies` on that decision. HITL uses the policies from the pass where `hitl_approved` is true. Nothing is posted to `/v1/runs`, and no VM starts. Exit `0` when the table prints. A missing declared policy falls back to builtins and says so. `--dry-run` with `--workspace` or `--follow` exits `2`.

**Options**:
- `--prompt <TEXT>`, `-p <TEXT>`: Prompt for `vam run <agent>`, override `spec.inputs.prompt` for `--file`, or the required prompt for `--workspace`.
- `--playbook <URN>`: Playbook URN for `vam run <agent>`.
- `--input <KEY=VALUE>`: One playbook input. Repeat the flag for each field.
- `--dry-run`: Print the policy decision table and do not start a run.
- `--model <NAME>`: The model for this run (`inputs.model`). It must be on the node's allowed models when the operator set a list (the node refuses it otherwise and names the allowed ones). Omit it and the run uses the agent's first allowed `preferredModels` entry, then the node's default model, then the first allowed model.
- `--thread <NAME>`: Thread id. Defaults to `main` for `--workspace`. Overrides `threadRef` for an agent or `--file` run.
- `--follow`: Stream execution events until the run reaches a terminal state (`COMPLETED`, `FAILED`, or `CANCELLED`). Not used with `--dry-run`. For playbook runs, stderr also prints step progress lines (`[vam] step <id>: <state>`) whenever `stepStatuses` changes.

---

## 2. Human-in-the-Loop (`vam approve`)

Inspect, approve, reject, or amend tool calls paused by Cedar security policies.

| Subcommand | Description |
|---|---|
| `vam approve ls` | List parked runs waiting on tool-level HITL |
| `vam approve get <APPROVAL_URN>` | Inspect proposed tool arguments and risk context |
| `vam approve decide <APPROVAL_URN>` | Decide a parked run (`approved`, `rejected`, `amended`). A warm VM resumes in place; after keep-warm park, resume boots a new VM |

### Usage Examples

```bash
# 1. List parked runs waiting on a tool approval
vam approve ls

# 2. Inspect the approval payload
vam approve get urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234

# 3. Approve the tool execution
vam approve decide urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234 --decision approved

# 4. Reject the tool execution (agent receives rejection error)
vam approve decide urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234 --decision rejected

# 5. Amend tool arguments before execution
vam approve decide urn:vam:approval:0198a2e1-4821-7291-a1b2-c98129e01234 \
  --decision amended \
  --amended-args '{"issue_key": "CGA-1298", "transition_id": "Ready for Review"}'
```

---

## 3. Host Secrets Vault (`vam secrets`)

Manage brokered credentials used by LLM inference providers and zero-trust egress proxies. Secrets are encrypted on the host and never leaked into guest microVM filesystems. The vault is partitioned: **org** (shared; operator / org_admin writes) and **user** (that caller only). There is no user→org fallback.

| Subcommand | Description |
|---|---|
| `vam secrets set <ID>` | Store or update a secret in the vault |
| `vam secrets list` | List org bindings plus your user-scope bindings (values redacted) |
| `vam secrets get <ID>` | Inspect secret metadata, kind, scope, and allowed destinations |
| `vam secrets delete <ID>` | Remove a secret from the vault |

### `vam secrets set` Options

- `--kind <inference|egress>`: Secret purpose (default: `inference`).
- `--scope <org|user>`: Vault partition. Omitted: org for operator/org_admin, user otherwise.
- `--value <STR>`: Inline secret string.
- `--from-env <VAR>`: Read secret value directly from environment variable.
- `--file <PATH>`: Read secret value from a file.
- `--allowed-destination <HOST>`: For egress secrets, restrict outbound domain (e.g. `*.atlassian.net`).

`vam secrets get` / `delete` / `list` accept the same `--scope` flag (`?scope=` on the HTTP API).

### Usage Examples

```bash
# Set OpenRouter LLM API key (operator default: org-scope)
vam secrets set openrouter/api-key --kind inference --from-env OPENROUTER_API_KEY --scope org

# Set Vercel AI Gateway API key
vam secrets set vercel/api-key --kind inference --from-env AI_GATEWAY_API_KEY --scope org

# User-scope Jira header (no fallback to org if missing)
vam secrets set jira/auth-header --kind egress --from-env JIRA_AUTH_HEADER --allowed-destination "*.atlassian.net" --scope user

# Set Jira credentials for Zero-Trust egress proxy
vam secrets set jira/base-url --kind egress --value "https://acme.atlassian.net"
vam secrets set jira/auth-header --kind egress --from-env JIRA_AUTH_HEADER --allowed-destination "*.atlassian.net"

# Pair is {app}/api-url + {app}/api-key. Placeholder is vamsec_ + id with / and - as _.
vam secrets set hrms/api-url --kind egress --value "https://hrms.example.com" --allowed-destination "hrms.example.com"
vam secrets set hrms/api-key --kind egress --from-env HRMS_API_KEY --allowed-destination "hrms.example.com"

# List stored secret bindings
vam secrets list
```

---

## 4. Sessions (`vam session`)

Manage low-level microVM session instances (useful for debugging and state inspection).

| Subcommand | Description |
|---|---|
| `vam session create` | Create a session; prints server-minted `session_id` and `vm_id` (do not pass client ids) |
| `vam session ls` | List active microVM sessions |
| `vam session get <SESSION_ID>` | Get detailed session status, PID, and TAP interface |
| `vam session pause <SESSION_ID>` | Pause guest VM vCPUs |
| `vam session resume <SESSION_ID>` | Resume paused guest VM vCPUs |
| `vam session snapshot <SESSION_ID>` | Trigger memory & disk snapshot |
| `vam session terminate <SESSION_ID>` | Forcefully destroy microVM session |

---

## 5. Catalog Management (`vam agent`, `vam playbook`, `vam policy`, `vam skill`)

Publish, inspect, and validate declarative specification files in the orchestrator catalog. Repo `examples/` trees are samples on disk; a fresh vamd catalog is empty until you `put` them.

```bash
# Register an agent specification
vam agent put examples/jira-assistant/agent.json

# List all agents in the catalog
vam agent ls

# Retrieve an agent specification by URN
vam agent get urn:vam:agent:local.vam.dev/vam/jira-assistant

# Register a playbook DAG
vam playbook put examples/jira-assistant/playbook.json

# Scaffold a starter playbook file. Local only: no vamd needed. Refuses to overwrite without --force
vam playbook init --out playbook.json

# Print a playbook's steps, gates, guards and bound/assisted ratio. Local only: no vamd needed
vam playbook explain examples/jira-assistant/playbook.json

# Run a scripted scenario (mocked tool observations) against the host's rules. --repeat k reports pass^k. Needs vamd
vam playbook test examples/jira-assistant/playbook.json --scenario scenario.json --repeat 3

# Upload Cedar capability policy (plain text, not JSON)
vam policy put examples/bun-assistant/policy.cedar --urn urn:vam:policy:local.vam.dev/vam/assistant-egress-only

# List policies in the catalog
vam policy ls

# Fetch a policy by URN
vam policy get urn:vam:policy:local.vam.dev/vam/assistant-egress-only

# Which rule decides a tool, or a host. Default version: your draft if you own one, else the latest release.
vam policy explain urn:vam:agent:local.vam.dev/vam/jira-assistant jira_create_issue
vam policy explain jira-assistant --host pastebin.com --version 1.3.0

# Draft permissions (JSON). --effective prints the decided table; --json prints that response.
vam agent permissions get jira-assistant
vam agent permissions get jira-assistant --effective
vam agent permissions set jira-assistant -f permissions.json
vam agent permissions presets

# Convert a legacy draft. Exits non-zero when unmapped tool names block it; --force drops them.
vam agent migrate jira-assistant

# Register a skill manifest (JSON metadata catalog, no tarball)
vam skill put examples/jira-assistant/skills/jira-integration/manifest.json

# Publish a skill directory as the owner's one draft (tar.gz upload)
vam skill publish examples/jira-assistant/skills/jira-integration

# List local skill packages
vam skill ls

# Fetch the draft. The draft is not a semver.
vam skill get urn:vam:skill:local.vam.dev/vam/jira-integration --channel draft

# Suggest the next version, then release the current draft (owner + live publish:grant).
# On a terminal, omit --version to accept the suggestion. --note is at most 500 characters.
vam skill suggest-version urn:vam:skill:local.vam.dev/vam/jira-integration
vam skill release urn:vam:skill:local.vam.dev/vam/jira-integration --note "Search and transition"

# List the draft, then releases newest first
vam skill versions urn:vam:skill:local.vam.dev/vam/jira-integration
```

`vam catalog validate <dir> [--strict] [--kind skill|agent|playbook]` checks a skill, agent or playbook directory, or a marketplace root (a directory with `marketplace.json`), offline: no vamd, no token. It runs the checks vamd runs at publish (manifest, `SKILLS.md`, cards, egress shapes), prints one line per finding and exits 6 on an error (`--strict` also on a warning).

`vam catalog import <git url> --path <dir> --ref <tag>` (org admin) installs a listing of a marketplace the node already trusts (`vam marketplace ls`): `--path` is the listing's directory in the repository (for example `listings/microsoft-365`) and `--ref` the tag the marketplace's index names for it. It is the same import as the Marketplace page: the node installs exactly the commit that tag names (a tag that moved is refused), checks every package, and writes released, read-only catalog rows with their origin. If an older tag of the listing is installed, the command updates it. A URL the node does not trust is refused; nothing is fetched from it.

The same `release`, `suggest-version`, and `versions` subcommands exist on `vam agent`, `vam playbook`, and `vam policy`. `vam agent check <urn> [--draft | --version X.Y.Z]` reports whether the agent's playbooks' required skills are attached. It exits 0 when a skill is missing. The rules are in [Authoring model](/authoring/model/).

```bash
vam agent release urn:vam:agent:local.vam.dev/vam/jira-assistant --note "Sprint cleanup"
vam agent check urn:vam:agent:local.vam.dev/vam/jira-assistant --draft
vam playbook versions urn:vam:playbook:local.vam.dev/vam/jira-sprint-backlog-cleanup
```

Retrieve registered skill **metadata** (not package rows):

```bash
vam skill get urn:vam:skill:local.vam.dev/vam/jira-integration
```

For a full publish → run walkthrough on Mac + Lima, see [Publish a Skill Package](/authoring/publish-skill/).

---

## 6. API Keys & Authorization (`vam keys`)

Manage long-lived API keys. Minted keys bind to a `user_id`. The first-start **bootstrap key is operator-only** (`keys:admin`, `brokers:admin`, plus `users:admin` and `publish:grant`); tenant keys never receive `keys:admin` or `brokers:admin`. Empty scope lists use the owner's role defaults (`member` / `publisher` / `org_admin` / `operator`). CI should keep using `VAM_API_KEY` (the bootstrap or another operator key). Org-admin `publish:grant` grant/revoke is `POST`/`DELETE /v1/users/{id}/publish-grant`. Catalog ownership reassignment is `POST /v1/catalog/reassign-owner`. User create/list/deactivate is `POST`/`GET /v1/users` and `POST /v1/users/{id}/deactivate`. Deactivate does not unpublish.

```bash
# Create a new API key with a descriptive label (bound to the caller)
vam keys create --name "ci-github-actions"

# Bind the key to a specific user
vam keys create --name "alice-laptop" --user-id alice --scope node:read --scope runs:write

# List all issued keys
vam keys list

# Revoke a compromised key
vam keys revoke key-0198a2...
```

---

## 7. Evidence pack (`vam evidence`)

Export a finished run while `vamd` is up, copy the directory, then verify it offline. Neither the VAM app nor a live node is required for verify. Print a playbook step receipt from run status without exporting the attestation pack.

| Subcommand | Description |
|---|---|
| `vam evidence export <RUN_ID> --out <DIR>` | `GET /v1/runs/{id}/evidence` and write the pack directory |
| `vam evidence verify <DIR>` | Recompute file hashes, verify the audit hash chain, unwrap DSSE, and check denials |
| `vam evidence receipt <RUN_ID>` | `GET /v1/runs/{id}` and print a markdown playbook step receipt (id, state, outcome, summary, evidence refs) |

```bash
# While vamd is running (same --host / --token as `vam run events`)
vam evidence export urn:vam:run:local.vam.dev/acme/run-1 --out ./evidence-run-1/

# Playbook step receipt (markdown on stdout; --json for structured output)
vam evidence receipt urn:vam:run:local.vam.dev/acme/run-1

# Later, offline — no daemon
vam evidence verify ./evidence-run-1/
```

Each user's API key can export only that user's finished runs. `vam evidence export` of another user's run is HTTP 404 (same as missing). Offline `vam evidence verify` still accepts audit rows keyed `tenant:{id}:{session}` — `user_id` is a record attribute, not a partition rewrite.

The directory contains `manifest.json`, `attestation.json`, `attestation.dsse.json`, `audit.jsonl`, `trajectory.json` (digest + event types only), `artifacts.json`, and `denials.jsonl`. An unfinished run (no `vam.v1.run.attested`) returns HTTP 409. Pack files use digests instead of raw prompts or tool args. The `verify_chain` binary in `vam-telemetry` can also check `audit.jsonl` alone.

---

## 8. Workspace files (`vam workspace`)

List or download **committed Drive9 HEAD** files under the guest `workspace/` mount. Same auth as sessions (`sessions:write`). Does not read live mid-run disks. See [Workspace Files API](/orchestrator/workspace-files/).

| Subcommand | Description |
|---|---|
| `vam workspace ls [WORKSPACE]` | `GET /v1/workspaces/{id}/files` (default workspace: `assistant`) |
| `vam workspace cat <PATH> [--workspace WORKSPACE] [-o FILE]` | Download raw bytes to stdout or `-o` |
| `vam workspace put <PATH> <LOCAL-FILE> [--workspace WORKSPACE]` | Upload raw bytes into committed HEAD (`PUT /v1/workspaces/{id}/files/*path`) |

```bash
vam workspace ls
vam workspace ls --json
vam workspace cat notes/a.md
vam workspace cat workspace/notes/a.md -o ./a.md
vam workspace cat notes/a.md -o ./a.md --json
vam workspace put notes/a.md ./a.md
vam workspace put workspace/notes/a.md ./a.md -w assistant
vam workspace put notes/a.md ./a.md --json
```

Paths may be full (`workspace/...`) or shorthand (`notes/a.md`); the CLI prepends `workspace/` when missing. Empty, absolute, and `..` paths exit `2`. With global `--json`, `cat` requires `-o` and prints `{"workspaceId","path","sizeBytes","out"}` (bytes stay in the file); `put` prints the API JSON body. Human `ls` prints path and size; a truncated list warns on stderr. Upload requires an idle workspace (no active session); a body over 32 MiB exits `2` locally or returns HTTP 413.

---

## 9. Node & Diagnostics (`vam node`, `vam health`, `vam config`)

```bash
# Check daemon liveness and ready warm-pool count
vam health

# Inspect node capacity, KVM version, memory allocation, and disk usage
vam node status   # also prints the node's default model, its allowed models and its default gateway (where an unknown provider/model id goes: vercel or openrouter)

# Set local CLI configuration
vam config set-host https://127.0.0.1:8080
vam config set-ca ~/.vam/node-ca.pem   # the node's tls/ca.pem; prints its fingerprint
vam config set-key --from-env VAM_API_KEY
vam config show

# Preferred for humans: username + password (stores a session token)
vam login --username local
```

---

## Understanding the Four Core Identities

When configuring runs and inspecting CLI outputs, keep in mind how VAM separates data and runtime lifecycles:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ 1. WORKSPACE (volume://assistant)                                           │
│    • Durable Drive9 volume storing workspace files and thread history.      │
├─────────────────────────────────────────────────────────────────────────────┤
│ 2. THREAD (main / feature-xyz)                                              │
│    • Conversation timeline appended to .vam/threads/<id>/timeline.jsonl     │
├─────────────────────────────────────────────────────────────────────────────┤
│ 3. SESSION (sess-run-...)                                                   │
│    • Ephemeral microVM writer spawned for one task and discarded.           │
├─────────────────────────────────────────────────────────────────────────────┤
│ 4. RUN (run-0198...)                                                        │
│    • A single prompt or playbook execution turn on the timeline.            │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Next Steps

- Read [Users and roles](/getting-started/users-and-roles/) for which user to log in as.
- Try the [Quickstart (5 Min)](/getting-started/quickstart/) to see these commands in action.
- Learn about [Agent Specifications (`agent.json`)](/authoring/agent-spec/). The use-case map is the [Authoring model](/authoring/model/).
