---
title: Inference
description: How a model call leaves the guest, the model catalog and its prices, prompt caching, the cost record and the mock model.
status: shipped
proves:
  - crates/vam-e2e/tests/usage_recorded.rs
  - crates/vam-orchestrator/tests/allowed_models_tests.rs
  - crates/vam-inference/src/catalog.rs::claude_5_5_models_are_in_the_catalog_and_the_default_is_unchanged
  - crates/vam-inference/src/catalog.rs::vercel_gateway_prices_at_the_makers_list_price
  - crates/vam-inference/src/cache.rs::default_modes
  - crates/vam-inference/src/provider.rs::test_openai_sse_parsing
  - crates/vam-inference/src/provider.rs::test_anthropic_sse_parsing
  - crates/vam-inference/src/provider.rs::vercel_gateway_auto_sets_provider_options
  - named-profile-prints-on-the-receipt-with-its-own-digest
  - model-task-runs-k-trials-on-the-mock
---

The guest never holds a model key. It sends the request over vsock port 53 (`crates/vam-runtime/src/agent.rs:82`,
client in `src/inference.rs`) and the host broker (`crates/vam-inference/src/broker.rs`) calls the provider with
the key from the vault ([Secrets](/security/secrets-proxy/)). `vam-inference` holds the providers, the catalog,
the cache plan and the budget.

## Providers

`provider.rs` has an OpenAI-compatible provider (`OpenAiProvider`, line 562; OpenRouter, the Vercel gateway
and the open-weight vendors in the catalog use it), an Anthropic provider (line 1565) and a scripted `MockProvider` (line 180).
`RetryPolicy` (line 94) sets the retries.

## The model catalog

`ModelCatalog::new()` (`catalog.rs:133`) lists the models a node knows, with price per million tokens, context
window, tool and structured-output support, cache prices and cache mode. Catalog version `2026-v3`
(`catalog.rs:374`); the default model when nothing is named is `claude-3-5-sonnet-latest` (`catalog.rs:373`).

| Entry | Provider | Profile |
|---|---|---|
| `claude-sonnet-5-5`, `claude-haiku-5-5` | anthropic | none (`base`) |
| `gpt-4o` | openai | none |
| `openai/gpt-4o`, `openai/gpt-4o-mini` | openrouter | none |
| gateway entry (`GATEWAY_ENDPOINT`, `catalog.rs:7`) | vercel | none |
| `glm-5`, `deepseek-v4-pro` | zai, deepseek | `open-strong` |
| `qwen3.6-27b` | alibaba | `open-small` |
| `mock-model` | mock | none |

The profile column is the harness profile that belongs to the model (`catalog.rs:308,336,366`). It applies only when a
run or the benchmark names it ([Agent loop](/runtime/agent/)). An open-weight entry also carries an adapter spec for
the model boundary, for example the name of the reasoning field, whether thinking is stripped, kept or turned off, and whether `tool_choice` is accepted (`catalog.rs:21`). The Claude 5.5 entries and the unchanged default are
checked by `claude_5_5_models_are_in_the_catalog_and_the_default_is_unchanged`.

An operator can limit which models a node accepts (`/v1/node/allowed-models`); see
[Catalog](/orchestrator/catalog/). Proved by `crates/vam-orchestrator/tests/allowed_models_tests.rs`.

## Cost

`ModelCatalogEntry::cost_micro` (`catalog.rs:71`) prices one call as the uncached input, the cache reads, the cache
writes and the output, summed and divided once. Cache usage a provider does not report is charged at the full input
price. A call through the Vercel gateway is priced at the maker's list price, not the gateway's
(`vercel_gateway_prices_at_the_makers_list_price`). `SessionBudget` (`budget.rs:41`) adds up the cost and refuses the
next call once the financial limit is reached (`check_preflight`, `budget.rs:99`). The usage counters read from a run's
recorded events are checked on recorded smoke runs by `crates/vam-e2e/tests/usage_recorded.rs`. Export of the cost records is in
[Telemetry](/telemetry/overview/).

## Prompt caching

`default_cache_mode` (`cache.rs:118`) picks how the host helps the provider cache the stable prefix: explicit
breakpoints for Anthropic (and Anthropic models through OpenRouter), a cache key for OpenAI, gateway `auto` for
Anthropic and MiniMax models through Vercel, and nothing extra for any other provider. Proved by the unit test
`default_modes`.

## The mock model

A node started for tests reads scripted replies from `VAMD_E2E_MOCK_SCRIPTS`
(`crates/vam-orchestrator/src/process_mode.rs:27`, `crates/vam-inference/src/mock_script.rs`). A script rule matches
on the `e2e-step:<label>` in the user text and replies with text or a tool call. Every process-mode scenario uses it.
The benchmark runner can run on it; a mock result proves the runner, not a model
([Benchmark results](/runtime/benchmark-results/)).

## Not proved

- The Anthropic and OpenAI providers against the live APIs: no test calls them.
- The adapter behaviour of each open-weight model: only the profile name on the receipt is proved.
