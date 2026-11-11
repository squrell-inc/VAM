---
title: Quickstart (5 Minutes)
description: Get up and running with VAM — start the daemon, configure secrets, run your first agent, and approve a Human-in-the-Loop (HITL) action.
---

In this quickstart guide, you will boot the VAM orchestrator daemon, store necessary API credentials, execute an autonomous Jira assistant inside a Firecracker microVM, and interact with the Human-in-the-Loop (HITL) approval gate using the `vam` CLI.

---

## Prerequisites

Before starting, ensure you have:
1. Built the VAM workspace binaries (`vamd` orchestrator and `vam` CLI).
2. A Linux host with KVM enabled — on Apple Silicon Macs, use [macOS + Lima](/getting-started/macos-lima/) (`fetch-guest-kernel-lima.sh` → `build-rootfs-lima.sh` → `start-vamd-lima.sh`) — or access a remote `vamd` instance.
3. An LLM API key (e.g. OpenRouter, OpenAI, Anthropic, or Gemini) and optional Jira Cloud credentials.

---

## Step 1: Start the VAM Orchestrator Daemon (`vamd`)

In your first terminal window, start the host orchestrator service:

```bash
# From workspace root (native Linux)
cargo run -p vam-orchestrator --bin vamd
```

**On Apple Silicon Mac:** use Lima instead — see [macOS + Lima](/getting-started/macos-lima/) and run `./scripts/start-vamd-lima.sh` after building the guest rootfs.

The daemon initializes the SQLite catalog, **bakes or reuses the Tier 1 golden snapshot** (first start may take ~20 s — wait for `Golden snapshot ready: {key}` in logs), and listens on `https://127.0.0.1:8080` (TLS by default: with no operator certificate `vamd` makes a self-signed one and logs its fingerprint and the path of `tls/ca.pem` under the state directory; `--insecure-dev` serves plain HTTP for dev hosts). A new node no longer prints a key on first start: `vam node init` (or `vamd --init`) mints the admin key and shows it once, and `vamd` never logs it. That secret is operator-only (`keys:admin`). Export it as `VAM_API_KEY` for CI.

Optional first loginable admin (no browser): set `VAMD_BOOTSTRAP_ADMIN` + `VAMD_BOOTSTRAP_ADMIN_PASSWORD` before start, then `vam login --username admin`. Which user to use: [Users and roles](/getting-started/users-and-roles/).

For cold-start measurement and golden restore tuning, see [Cold-start & Golden Snapshot](/architecture/cold-start/).

---

## Step 2: Configure CLI and Secrets

In a second terminal, configure your `vam` CLI client to talk to the local daemon:

```bash
# Point CLI to local daemon
vam config set-host https://127.0.0.1:8080
vam config set-ca <state dir>/tls/ca.pem   # trust the node's self-signed certificate

# Check daemon health
vam health
```

### Store LLM Inference Secret

Store an API key in the host secrets vault. The key is managed securely on the host and never injected directly into the guest microVM filesystem:

```bash
# Store OpenRouter API key for host-brokered inference
vam secrets set openrouter/api-key --kind inference --from-env OPENROUTER_API_KEY
```

### Store Egress Secrets (Jira Cloud)

For egress integrations, secrets are substituted at runtime by the host secrets proxy:

```bash
# Store Jira base URL and authorization header
vam secrets set jira/base-url --kind egress --value "https://your-domain.atlassian.net"
vam secrets set jira/auth-header --kind egress --value "Basic <base64-encoded-email-and-token>"
```

Verify secrets are safely registered (values are always redacted):

```bash
vam secrets list
```

---

## Step 3: Run the Jira Assistant Agent

Start a run using the bundled `jira-assistant` example with the `--follow` flag:

```bash
vam run --file examples/jira-assistant/run.json --follow
```

The CLI prints the run's JSON, then follows it on stderr: a `[vam] following run <id>` line,
one line per step as it progresses, and a `[vam] run state: <STATE>` line each time the state
changes.

---

## Step 4: Human-in-the-Loop (HITL) Approval

When the agent attempts a mutating action (such as `jira_transition_issue` to move ticket `CGA-1298` to *In Progress*), VAM's Cedar policy engine intercepts the call and **Suspends** the run for human authorization.

In another terminal, list pending approvals:

```bash
vam approve ls
```

Each parked call prints as one line:

```text
parked  run=<run id>  tool=jira_transition_issue  approval=<approval id>
```

Inspect the exact tool arguments the agent wants to execute:

```bash
vam approve get <approval id>
```

Approve the request to resume execution:

```bash
vam approve decide <approval id> --decision approved
```

---

## Step 5: Verify Completion

Switch back to the terminal following the run. The run state moves back to running, and when
the run ends the CLI prints its final `[vam] run state:` line followed by `[vam] agent answer:`
and the answer (or `[vam] run error:` and the error).

---

## Next Steps

- Read [Users and roles](/getting-started/users-and-roles/) to create the first admin and pick operator vs org admin.
- Read [Cold-start & Golden Snapshot](/architecture/cold-start/) for measured latencies and the golden bake operator guide.
- Explore the [CLI Command Reference](/getting-started/cli/) for all management commands.
- Learn how to define your own agents in [Writing `agent.json`](/authoring/agent-spec/). Start with the [Authoring model](/authoring/model/) so agents, skills, playbooks, and policies match the catalog.
- Understand how skills and tools work in [Skills & Frontmatter](/authoring/skill-manifest/).
- Read about [Zero-Trust Egress Proxies](/integrations/egress-proxy-deepdive/).
