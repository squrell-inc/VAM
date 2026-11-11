# Linear MCP Assistant

Minimal sample for the **remote MCP** path: connect Linear with a Bearer token →
start a **new** session (skills seed at session start) → `load_skill mcp-linear`
→ call curated `linear__*` tools.

There is **no** guest `skills/` package in this example. `vamd` seeds
`mcp-linear/` when the connection id is `linear`. Tool names are
`linear__{vendor}` (Cedar resource = that string).

```text
examples/linear-mcp-assistant/
├── README.md
├── agent.json      # empty skillRefs; MCP-focused instructions
├── policy.cedar    # builtins + curated linear__* permits
└── run.json        # sample prompt for linear__get_issue
```

Samples under `examples/` are not auto-loaded into a fresh vamd catalog. Publish
with `vam policy put` / `vam agent put` as below.

## Try path

```text
connect Linear  →  new session seeds mcp-linear  →  policy put + agent put
  →  vam run  →  load_skill mcp-linear  →  linear__get_issue
```

### 1. Create a Linear API key and connect

Create a personal API key in Linear. The vault value must be the **full**
`Authorization` header value, including the `Bearer ` prefix
(e.g. `Bearer lin_api_…`).

```bash
# Prefer --from-env so the token never lands in shell history.
export LINEAR_MCP_TOKEN='Bearer lin_api_…'

vam mcp connect linear \
  --url https://mcp.linear.app/mcp \
  --scope user \
  --from-env LINEAR_MCP_TOKEN \
  --side-effect get_issue=read \
  --side-effect list_issues=read
```

`--side-effect …=read` lowers those tools so the first fetch is not blocked on
HITL. Mutating tools (e.g. `create_issue`) stay default side effect 3 and need
`hitl_approved`.

Connection id **must** be `linear` so the guest skill is `mcp-linear` and tools
are `linear__*`. Secret binding is `linear/auth-header`.

### 2. Confirm tools

```bash
vam mcp list
# or inspect Connections in the VAM app
```

Expect ~59 tools and binding `linear/auth-header`. Vendor tool names vary;
confirm `get_issue` / `list_issues` appear (Connections UI or list output)
before relying on them in Cedar.

### 3. Start a **new** session

A connection takes effect at the **next** session start (no hot-reload). If a
session was already running when you connected, terminate it and create a new
one so `mcp-linear` is seeded under Drive9 skills.

### 4. Publish policy and agent

```bash
vam policy put examples/linear-mcp-assistant/policy.cedar \
  --urn urn:vam:policy:local.vam.dev/vam/linear-mcp-assistant

vam agent put examples/linear-mcp-assistant/agent.json
```

Cedar permits builtins (`load_skill`, `read_file`, `glob`, `grep`, `write_file`,
`todo_write`) plus read Linear tools `linear__get_issue` and
`linear__list_issues`. `linear__create_issue` requires HITL. Expand the
`.contains` lists in `policy.cedar` when you want more of the pinned tools —
there is no `Tool::"linear__*"` wildcard.

### 5. Run

```bash
vam run --file examples/linear-mcp-assistant/run.json --follow
```

Override the issue key for your workspace:

```bash
vam run --file examples/linear-mcp-assistant/run.json \
  --prompt "Call load_skill with skill mcp-linear. Then call linear__get_issue for issue id YOUR-KEY and report title and status. Do not grep skill files." \
  --follow
```

`agent.json` instructions require `load_skill` with `mcp-linear` before any
Linear tool. Do not answer by grepping `SKILLS.md` or `manifest.json`.

For create flows, approve when Suspended:

```bash
vam approve ls
vam approve decide <approval-id> --decision approved
```

## Troubleshooting

| Symptom | Likely cause |
|---|---|
| Auth / 401 from Linear | Token missing `Bearer ` prefix |
| No `mcp-linear` in catalog | Session started **before** connect — start a new session |
| Model greps skill files | Prompt/instructions not followed — reinforce `load_skill` |
| Cedar deny on a Linear tool | Tool not in the curated `.contains` allowlist |
| Create hangs / Suspend | Expected — `linear__create_issue` needs HITL |

## Related

- Runtime MCP overview: [docs/src/content/docs/runtime/mcp.md](../../docs/src/content/docs/runtime/mcp.md)
- CLI catalog puts: [docs/src/content/docs/getting-started/cli.md](../../docs/src/content/docs/getting-started/cli.md) (`vam policy put`, `vam agent put`)
- Sibling policy pattern: [examples/jira-assistant/policy.cedar](../jira-assistant/policy.cedar)
