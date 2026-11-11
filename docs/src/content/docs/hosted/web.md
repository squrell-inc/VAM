---
title: Web Broker
status: shipped
proves:
  - crates/vam-orchestrator/tests/hsp_web.rs
  - crates/vam-orchestrator/tests/hsp_web_sources.rs
  - brokers/vam-web/tests/env_isolation.rs
  - brokers/vam-web/src/fetch/network/tests.rs::dns_rebinding_between_hops_is_caught
  - brokers/vam-web/src/fetch/network/tests.rs::robots_disallow_blocks_the_path_and_is_cached
description: The first-party web broker (vam.web) gives agents web_search and web_fetch with host-enforced admission, citations and least-privilege secrets.
---

`vam.web` is a first-party HSP broker (package `vam-web`, binary `vam-web-broker`, source in `brokers/vam-web`). It gives an agent two read-only tools. Web access is **off by default** and is switched on per agent.

## Tools

| Tool | What it does |
|---|---|
| `web_search` | Searches the web through a provider (Brave) and returns titles, URLs, snippets and dates. Arguments: `query` (required, 1 to 400 characters), `recency` (`day`, `week`, `month`, `year`), `include_domains` and `exclude_domains` (up to 20 each), `count` (1 to 20, default 10). |
| `web_fetch` | Fetches one `http` or `https` URL, converts the page to markdown and saves it as a file artifact. Arguments: `url` (required, up to 2048 characters) and `focus` (optional, up to 200 characters, picks which part of the page goes in the preview). |

`web_fetch` returns `output {file, url, final_url, title, preview}` and saves the page as `web-<host>-<8hex>.md`. The model reads the saved file for the full page. The broker declares `web_search` with `destination_policy` `ALLOWLIST` (its own egress domain is `api.search.brave.com`) and `web_fetch` with `PUBLIC_WEB`; see [Capability Manifests](/hosted/overview/).

## Running the broker

Run it on the vamd host, as the **same user as vamd**. The broker listens on a Unix socket with mode `0600` and vamd dials it, so a different user cannot connect. On a Mac, vamd runs inside Lima: run the script inside the Lima VM.

```bash
# Real search: the key is stored once in the vamd vault
BRAVE_API_KEY=... scripts/run-web-broker.sh

# Fixtures, no key, no network (benchmarks and demos)
scripts/run-web-broker.sh --mock

scripts/run-web-broker.sh --dry-run   # print the registration request and exit
scripts/run-web-broker.sh --stop      # stop the broker the script started
```

Other options: `--fixtures DIR` (for `--mock`) and `--foreground`. The script needs `jq` and `curl`, registers the manifest (`scripts/web-broker/manifest.json`) with `PUT /v1/hosted/brokers`, and reads `VAMD_URL` (default `http://127.0.0.1:8080`), `VAMD_ADMIN_KEY` (an API key with `brokers:admin`, default `$VAM_API_KEY`), `VAM`, `VAM_WEB_SOCKET` (default `/tmp/vam-web.sock`), `VAM_WEB_BIN` and `VAM_WEB_STATE_DIR`.

The fetcher sends `User-Agent: VAM-WebFetch/1 (+<docs-url>)`. By default `<docs-url>` is this page, `https://docs.squrell.dev/hosted/web/`, so site owners can find out who is fetching. Operators can override it with the broker's `--docs-url` flag.

## The search key

The Brave API key is stored in the vamd vault as `web/brave-api-key`, kind `inference`, scope org. That kind is one the guest egress proxy never injects, so the key cannot reach the guest. The broker process never has the key in its environment or arguments: vamd sends it with each `web_search` call, and only to that tool (see least-privilege secrets below). The key is redacted from results and errors.

Brave specifics (endpoint, response fields, rate limits, prices) are not restated here. Verify against Brave's docs and pricing pages before you rely on them.

## Turning web on for an agent

```bash
vam agent permissions web <agent> on
vam agent permissions web <agent> off
```

* The switch is `permissions.web.enabled` in the agent spec. It is off by default.
* **Legacy agents never get web.** An agent still on `capabilityPolicy` / `hitlPolicy` must be converted to `permissions` first; the command says so.
* While web is off, web brokers are hidden from the tool catalog and from skills. A call that arrives anyway is refused with `web_disabled`.
* Turning web on is a minor release difference, turning it off a major one. The effective permissions and `explain` show `web {enabled, available, note}`.

## Admission rules

vamd checks every `web_fetch` before the broker is called. A refusal comes back to the model as `tool_error: <tool>: <code>: <advice>`. The checks run in this order:

| Code | When |
|---|---|
| `web_disabled` | The agent has web off. |
| `invalid_url` | The URL is missing, does not parse, or is not `http`/`https`. |
| `not_in_context` | The URL has not appeared in the user's messages or in an earlier tool result of this task. The broker is never called. Search results, fetched pages and redirect targets count. |
| `blocked` | The agent's internet rules deny the host, the port is not 80 or 443, or the host resolves to a restricted address or fails to resolve. |
| `invalid_query` | A string argument of a web tool holds an `http(s)` URL with a path, query, fragment or userinfo. Bare domains and scheme-less text are fine. |

The in-context rule stops the model from building or editing URLs, which is how data leaks out in a query string. The context set is per task; it is not carried across a thread yet.

## Fetch limits

| Limit | Value |
|---|---|
| Whole call (all hops and the body) | 20 s |
| Connect | 5 s |
| Raw body | 5 MiB, counted after content decoding (gzip) |
| Saved page | at most 46 KiB of markdown |
| Preview in the result | 4 KiB |
| Redirects | up to 5, same host (or its `www.` form) only |
| Concurrent conversions | 4 |

* **robots.txt** is checked with user agent `VAM-WebFetch/1`; a parsed file is cached for an hour. A disallowed path gives `blocked_by_robots`. An unreachable `robots.txt` (5xx, 429, network error) is remembered for 5 minutes.
* **Redirects** are followed by hand. A cross-host redirect is not followed: the result is `output.status = redirected` with `output.redirect_to`, and the target becomes usable in context. A redirect from https to http is blocked.
* **DNS pinning.** Each hop resolves the host again and connects to the validated IP, so a host that changes its answer between hops is caught. Proxy environment variables are ignored.
* **Strict IP ranges.** Beyond private and loopback addresses, the broker also blocks CGNAT (including `100.100.100.200`), NAT64, 6to4, Teredo, `198.18.0.0/15` and `240.0.0.0/4`.
* **Content.** HTML and text are converted. PDFs and other types give `unsupported_content_type`.
* Other fetch error codes: `url_too_long`, `too_large`, `timeout`, `rate_limited`, `not_accessible`. Search errors are typed too: invalid argument, missing secret, provider auth, rate limit, timeout, unavailable.

Page text is untrusted. It is redacted, scanned for injection patterns and fenced before the model sees it, like [MCP vendor text](/runtime/tools/).

## Citations

* The guest guidance tells the model to cite with `[S<n>]` markers (`[S1]`, `[S1, S3]`, `[S1][S2]`).
* Each result carries `sources` as `[{id, title, url, published}]`. Ids are host-assigned and unique per normalized URL. There is no snippet field. The broker reports its sources to vamd as `InvokeResponse.sources` ([protocol](/hosted/overview/)).
* When the final answer has markers and the agent has web on, the host checks them. `finalOutputs.sources[]` lists the **cited** sources only, in first-citation order. `finalOutputs.citations.unresolved` lists ids no source of the run answers to. The check only flags; it never rewrites the answer.

Two events (see the [event catalog](/telemetry/event-catalog/)):

* `vam.v1.web.used`: one per web tool call. Fields: broker, tool, host (hostname only), status (`ok`, `redirected`, `error`), optional error code, bytes, sources added. It never holds the query, snippets, page text, path or URL.
* `vam.v1.web.citations_unresolved`: the `unresolved` ids (shape `S<digits>`) and the `resolved` and `total_sources` counts. It never holds answer text.

## Least-privilege secrets

This applies to **every** broker, not only `vam.web`. Each tool export now receives only the secrets listed in its own `required_secrets`; a binding no export lists is never sent. Operators must add `required_secrets` to each export of every existing broker that uses a secret. If a call needs a secret its tool does not list, it fails with `missing_secret`, and `vam agent check` warns about bindings that no export lists. The redaction set is unchanged: all resolved and bound values are still scrubbed from results.

## What is stored

* The host stores source ids and normalized URLs for the run (tables `run_context_urls` and `run_sources`). It stores **no snippets and no page text** for sources.
* Snippets and fetched text do reach conversation checkpoints, because they are part of the conversation.
* Brave's storage-rights terms for search results are the operator's to check. Verify against Brave's terms.
* Search queries go to the search vendor. That is a residual channel; the query rule only refuses URLs with path or query data.

## Known limits

* No PDF support and no JavaScript rendering.
* The in-context URL set is per task, not per thread.
* No operator proxy option for fetch yet.
