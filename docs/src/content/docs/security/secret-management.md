---
title: Secrets Vault Management
description: Managing host-brokered inference credentials and zero-trust egress tokens with encryption and automatic redaction.
---

VAM incorporates a dedicated **Host Secrets Vault** that stores, encrypts, and brokers sensitive API keys and tokens. 

Secrets are managed exclusively on the host orchestrator and are **never injected into guest microVM filesystems**, preventing credential exfiltration by untrusted AI workloads.

`host_secrets` rows are keyed by **`(scope, owner_user_id, binding_id)`**. `scope` is `org` (shared) or `user` (that caller). Org-scope writes are operator / org_admin only. User-scope writes are that user only. **A missing user-scope secret does not fall back to org.** Read-back is metadata plus an 8-character fingerprint — never plaintext.

---

## Vault Architecture & Security Model

```
┌─────────────────────────────────────────────────────────────────────────────┐
│ HOST ORCHESTRATOR (vamd)                                                    │
│                                                                             │
│  [Encrypted Secrets Vault (AES-256-GCM / ChaCha20-Poly1305)]                │
│    ├── openrouter/api-key ──▶ Brokered to host LLM inference client         │
│    ├── vercel/api-key     ──▶ Brokered to Vercel AI Gateway client          │
│    └── jira/auth-header   ──▶ Brokered to zero-trust egress proxy           │
│                                                                             │
│  SECURITY GUARANTEES:                                                       │
│  • Memory-isolated on the host (never mounted in guest MicroVMs)            │
│  • Values are strictly redacted from all CLI responses and logs             │
│  • Outbound proxy calls enforced with --allowed-destination rules           │
└─────────────────────────────────────────────────────────────────────────────┘
```

---

## Secret Kinds

VAM organizes secrets into two distinct categories:

| Kind | Purpose | Examples | Where Used |
|---|---|---|---|
| **`inference`** | LLM Provider API Keys | `openrouter/api-key`, `vercel/api-key`, `anthropic/api-key`, `openai/api-key` | Host LLM inference client (for agent reasoning turns) |
| **`egress`** | Third-party service tokens | `jira/base-url`, `jira/auth-header`, `github/token` | Zero-Trust Egress Proxy (substituted during tool calls) |

---

## Org vs user vault

| Scope | Owner | Who can write | Who can read metadata |
|---|---|---|---|
| **`org`** | empty | `operator` / `org_admin` | Any caller with `secrets:write` |
| **`user`** | `users.user_id` | That user only | That user only |

Unspecified `PUT` / `GET` / `DELETE` follow the caller's role default: org for operator and org_admin, user otherwise. Pass `--scope org` or `--scope user` (query `?scope=` on GET/DELETE/LIST) to be explicit.

`VAMD_IMPORT_ENV_SECRETS=1` seeds **org-scope** well-known bindings at daemon start (operator-only path). A user-scope row with the same `binding_id` does not skip or replace that import.

Alice and Bob may both store `jira/auth-header`. Alice's put does not overwrite Bob. Listing shows org rows plus the caller's user-scope rows — never another user's vault.

Org admin (and operator) manage shared org-scope rows from the VAM app via `PUT`/`GET /v1/org/secrets` and `DELETE /v1/org/secrets/{id}` (`users:admin`). That path always targets org-scope, returns metadata plus fingerprint only, and does not delete a same-`binding_id` user-scope row. Mixed `PUT /v1/secrets` (`secrets:write`) stays for user-scope writes.

HSP invoke injects **caller-scoped** `resolved_secrets`: the calling user's user-scope row if present, otherwise org-scope. Alice's invoke never receives Bob's user credential. A dedicated user-scope lookup still has no implicit fallback to org. In-process adapters and Port 53/54 egress still decrypt **org-scope** only.

## Requirement scope and the three surfacing moments

`SecretRequirement` is `{name, required, scope}` where `scope` is `org | user | either` (`0` unspecified defaults to `either`; no user→org fallback for `user`):

1. **Discovery** — `POST /v1/secrets/resolve` resolves declared requirements against the caller's vault and reports `configured` / `missing` plus `satisfiedScope`. It checks org rows plus the caller's own user rows only; Bob's resolve never reveals whether Alice configured a user-scope secret (org existence is shared on purpose).
2. **Admission** — `POST /v1/runs` fails fast with `412` listing `binding@scope` when the run's skills declare required secrets the caller lacks. Call the same resolve before skill `load_skill`.
3. **Tool call** — host tool execution stays fail-closed with a typed `missing secret binding 'X' (scope Y)` instead of a vague lookup failure.

---

## Setting Secrets with `vam` CLI

You can populate the vault using inline values, environment variables, or files:

### 1. Storing LLM Inference Keys (`--kind inference`)

```bash
# Read OpenRouter API key from environment variable
vam secrets set openrouter/api-key \
  --kind inference \
  --from-env OPENROUTER_API_KEY

# Read Vercel AI Gateway API key from environment variable
vam secrets set vercel/api-key \
  --kind inference \
  --from-env AI_GATEWAY_API_KEY

# Store Anthropic key from environment variable
vam secrets set anthropic/api-key \
  --kind inference \
  --from-env ANTHROPIC_API_KEY
```

---

### 2. Storing Egress Proxy Credentials (`--kind egress`)

For egress credentials, use `--allowed-destination` to enforce strict domain allowlists:

```bash
# Store tenant base URL (mocked example)
vam secrets set jira/base-url \
  --kind egress \
  --value "https://acme.atlassian.net" \
  --allowed-destination "*.atlassian.net"

# Store base64 auth header from environment variable
vam secrets set jira/auth-header \
  --kind egress \
  --from-env JIRA_AUTH_HEADER \
  --allowed-destination "*.atlassian.net"

# Pair is `{app}/api-url` + `{app}/api-key`. Dummy host in docs; pin real API host at deploy.
vam secrets set hrms/api-url \
  --kind egress \
  --value "https://hrms.example.com" \
  --allowed-destination "hrms.example.com"
vam secrets set hrms/api-key \
  --kind egress \
  --from-env HRMS_API_KEY \
  --allowed-destination "hrms.example.com"
```

---

## Injection sites and transport

The guest only ever sees a placeholder. The egress gate swaps in the real value at the binding's declared injection site, and only for HTTPS destinations:

- **Header** and **BearerToken** sites match the header name case-insensitively. HTTP header names are case-insensitive (RFC 9110), and the proxy receives them lowercased, so a site declared as `Authorization` or `x-api-key` matches the request header whatever its case.
- **Plain HTTP** requests that carry a placeholder are refused by the egress gate, and nothing is sent upstream. A secret is never sent in cleartext. Use an `https://` URL for any destination that needs a secret.

---

## Managing Secrets

### MCP connection credentials (kind `connection`)

Credentials of an MCP connection are stored with the reserved kind `connection`. `vam secrets list` shows them, but no secrets command or API call can create one or turn one into another kind. They are host-only:

- They get no `vamsec_` placeholder and add no domain to a session's egress allowlist, so guest code cannot have the egress proxy inject them.
- An HSP broker never receives one. A call whose manifest names a `connection` row fails with `hsp denied: secret not permitted for this broker` before the broker is reached.

### What an HSP broker may be given

A broker receives a vault row only if every host the row is pinned to lies inside the `egressDomains` the broker's manifest was approved with (a domain covers itself and its subdomains; a `*` pin is never inside). A row pinned to no host passes. A manifest that names a row outside its own domains fails the call the same way, even for an optional binding.

---

### Listing Secrets (`vam secrets list`)
Lists all registered secret bindings. Secret values are **always redacted**:

```bash
vam secrets list
```

Output:
```text
BINDING ID           KIND         DESTINATION            UPDATED AT
openrouter/api-key   inference    (n/a)                  2026-08-22 05:00:00 UTC
jira/base-url        egress       *.atlassian.net        2026-08-22 05:00:00 UTC
jira/auth-header     egress       *.atlassian.net        2026-08-22 05:00:00 UTC
```

---

### Inspecting Secret Metadata (`vam secrets get`)

```bash
vam secrets get jira/auth-header
```

Output:
```json
{
  "bindingId": "jira/auth-header",
  "kind": "egress",
  "allowedDestination": "*.atlassian.net",
  "updatedAt": "2026-08-22T05:00:00Z",
  "isSet": true
}
```

---

### Deleting a Secret (`vam secrets delete`)

```bash
vam secrets delete jira/auth-header
```

---

## Vault Encryption Key (`VAMD_SECRET_MASTER_KEY`)

On the host machine, the orchestrator (`vamd`) encrypts secrets at rest using a 256-bit master key:

```bash
# Set master key before launching daemon
export VAMD_SECRET_MASTER_KEY="your-32-byte-hex-encoded-master-key"
cargo run -p vam-orchestrator
```

If `VAMD_SECRET_MASTER_KEY` is not provided in development mode, `vamd` generates an ephemeral in-memory key for the session.

---

## Next Steps

- Learn about [Zero-Trust Egress Architecture](/integrations/egress-proxy-deepdive/).
- Check [Cedar Policies & HITL](/security/cedar-and-hitl/).
- Review [CLI Command Reference](/getting-started/cli/).
