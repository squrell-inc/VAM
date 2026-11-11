use crate::cache::{default_cache_mode, CacheMode, CacheRetention};
use crate::error::VamInferenceError;
use std::collections::HashMap;
use vam_spec::v1::{ModelPreference, ModelRequirements};

/// The Vercel AI Gateway (OpenAI-compatible) endpoint and its credential binding.
pub const GATEWAY_ENDPOINT: &str = "https://ai-gateway.vercel.sh/v1/chat/completions";
pub const GATEWAY_SECRET_REF: &str = "vercel/api-key";

/// How an open-weight model behaves at the model boundary (harness question 5).
/// Model-specific handling lives here, never in the loop (locked decision 7).
///
/// The broker reads these per request (`crate::wire::plan_wire`): the reasoning field
/// and thinking mode (`adapter.thinking: catalog` uses `thinking`), the `thinking_off`
/// fragment, `tool_choice` and `effort_field`. The loop never looks at them.
///
/// `thinking` is `strip`, `keep` or `off`. `keep` means the reasoning is sent back
/// with the next request (DeepSeek V4 returns a 400 on a tool round without it): the
/// broker re-attaches it on the host, never through the guest.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AdapterSpec {
    /// Name of the reasoning delta on the OpenAI-compatible stream
    /// (`reasoning_content` or `reasoning`).
    pub reasoning_field: String,
    /// Thinking mode the entry asks for: `strip`, `keep` or `off`.
    pub thinking: String,
    /// JSON fragment merged into the request body to turn thinking off; empty when
    /// the model cannot be told.
    pub thinking_off: String,
    /// Whether the API accepts a `tool_choice` other than `auto`.
    pub tool_choice: bool,
    /// Request field that carries effort; empty when the API has none.
    pub effort_field: String,
}

#[derive(Debug, Clone, Default)]
pub struct ModelCatalogEntry {
    pub provider: String,
    pub model_name: String,
    pub endpoint: String,
    pub secret_ref: String,
    pub price_prompt_micro: i64, // USD per million tokens in micro-amount (e.g. $3.00 = 3_000_000)
    pub price_completion_micro: i64, // USD per million tokens in micro-amount (e.g. $15.00 = 15_000_000)
    pub context_window: i32,
    pub supports_tools: bool,
    pub supports_structured_output: bool,
    /// Cache read price per million tokens. 0 means "not priced": the input price applies.
    pub price_cache_read_micro: i64,
    /// Cache write price (provider default TTL, 5 minutes). 0 falls back to the input price.
    pub price_cache_write_micro: i64,
    /// Cache write price for the long TTL (1 hour). 0 falls back to `price_cache_write_micro`.
    pub price_cache_write_long_micro: i64,
    /// How the provider takes the host's cache plan.
    pub cache_mode: CacheMode,
    /// How long the static prefix is kept.
    pub cache_retention: CacheRetention,
    /// Model-boundary handling of an open-weight model; `None` for the Claude and
    /// OpenAI entries.
    pub adapter: Option<AdapterSpec>,
    /// Harness profile this model belongs to (a name in `vam_spec::harness::PROFILES`).
    /// It applies only when a run or the benchmark names it; the catalog never
    /// applies a pending profile implicitly.
    pub profile: Option<String>,
}

impl ModelCatalogEntry {
    /// Estimated cost in micro-USD of one inference:
    /// `(prompt - cached - written) x input + cached x read + written x write plus
    /// completion x output`, summed first and divided once. Unknown cache
    /// usage is charged at the full input price (open question 17).
    pub fn cost_micro(&self, usage: &vam_spec::v1::TokenUsage) -> i64 {
        let prompt = usage.prompt_tokens.max(0) as i64;
        let (cached, written) = if usage.cached_tokens_unknown {
            (0, 0)
        } else {
            (
                (usage.cached_tokens.max(0) as i64).min(prompt),
                usage.cache_creation_tokens.max(0) as i64,
            )
        };
        let written = written.min(prompt - cached);
        let uncached = prompt - cached - written;
        let read_price = if self.price_cache_read_micro > 0 {
            self.price_cache_read_micro
        } else {
            self.price_prompt_micro
        };
        let write_price = if self.price_cache_write_micro > 0 {
            self.price_cache_write_micro
        } else {
            self.price_prompt_micro
        };
        let written_long = if usage.cached_tokens_unknown {
            0
        } else {
            (usage.cache_creation_long_tokens.max(0) as i64).min(written)
        };
        let long_price = if self.price_cache_write_long_micro > 0 {
            self.price_cache_write_long_micro
        } else {
            write_price
        };
        (uncached * self.price_prompt_micro
            + cached * read_price
            + (written - written_long) * write_price
            + written_long * long_price
            + usage.completion_tokens.max(0) as i64 * self.price_completion_micro)
            / 1_000_000
    }
}

/// Providers that carry requests for a maker rather than being one.
fn is_transport(provider: &str) -> bool {
    matches!(provider, "vercel" | "openrouter" | "mock")
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct PriceProvenance {
    pub catalog_version: String,
    pub pricing_effective_date: String,
    pub calculation_status: String,
}

#[derive(Debug, Clone)]
pub struct ModelCatalog {
    entries: HashMap<String, ModelCatalogEntry>,
    default_model: String,
    catalog_version: String,
    effective_date: String,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelCatalog {
    pub fn new() -> Self {
        let mut entries = HashMap::new();

        // 1. Anthropic Claude 3.5 Sonnet
        entries.insert(
            "claude-3-5-sonnet-latest".to_string(),
            ModelCatalogEntry {
                provider: "anthropic".to_string(),
                model_name: "claude-3-5-sonnet-latest".to_string(),
                endpoint: "https://api.anthropic.com/v1/messages".to_string(),
                secret_ref: "anthropic/api-key".to_string(),
                price_prompt_micro: 3_000_000,
                price_completion_micro: 15_000_000,
                context_window: 200_000,
                supports_tools: true,
                supports_structured_output: true,
                price_cache_read_micro: 300000,
                price_cache_write_micro: 3750000,
                price_cache_write_long_micro: 6000000,
                cache_mode: CacheMode::ExplicitBreakpoints,
                cache_retention: CacheRetention::Long,
                adapter: None,
                profile: None,
            },
        );

        // Claude 5.5 family. Prices are USD per million tokens for prompts up to
        // 100K tokens. The catalog has no cache price fields: cache reads ($0.20 for
        // Sonnet 5.5) and writes (1.25x input) are billed by the provider, and the
        // estimate here charges every prompt token at the input price.
        entries.insert(
            "claude-sonnet-5-5".to_string(),
            ModelCatalogEntry {
                provider: "anthropic".to_string(),
                model_name: "claude-sonnet-5-5".to_string(),
                endpoint: "https://api.anthropic.com/v1/messages".to_string(),
                secret_ref: "anthropic/api-key".to_string(),
                price_prompt_micro: 2_000_000,
                price_completion_micro: 10_000_000,
                context_window: 200_000,
                supports_tools: true,
                supports_structured_output: true,
                price_cache_read_micro: 200000,
                price_cache_write_micro: 2500000,
                price_cache_write_long_micro: 4000000,
                cache_mode: CacheMode::ExplicitBreakpoints,
                cache_retention: CacheRetention::Long,
                adapter: None,
                profile: None,
            },
        );
        entries.insert(
            "claude-haiku-5-5".to_string(),
            ModelCatalogEntry {
                provider: "anthropic".to_string(),
                model_name: "claude-haiku-5-5".to_string(),
                endpoint: "https://api.anthropic.com/v1/messages".to_string(),
                secret_ref: "anthropic/api-key".to_string(),
                price_prompt_micro: 100_000,
                price_completion_micro: 500_000,
                context_window: 200_000,
                supports_tools: true,
                supports_structured_output: true,
                price_cache_read_micro: 10000,
                price_cache_write_micro: 125000,
                price_cache_write_long_micro: 200000,
                cache_mode: CacheMode::ExplicitBreakpoints,
                cache_retention: CacheRetention::Long,
                adapter: None,
                profile: None,
            },
        );

        // 2. OpenAI GPT-4o
        entries.insert(
            "gpt-4o".to_string(),
            ModelCatalogEntry {
                provider: "openai".to_string(),
                model_name: "gpt-4o".to_string(),
                endpoint: "https://api.openai.com/v1/chat/completions".to_string(),
                secret_ref: "openai/api-key".to_string(),
                price_prompt_micro: 5_000_000,
                price_completion_micro: 15_000_000,
                context_window: 128_000,
                supports_tools: true,
                supports_structured_output: true,
                price_cache_read_micro: 2_500_000,
                price_cache_write_micro: 0,
                price_cache_write_long_micro: 0,
                cache_mode: CacheMode::PromptKey,
                cache_retention: CacheRetention::Short,
                adapter: None,
                profile: None,
            },
        );

        // 3. Mock Model (for local unprivileged developer testing)
        entries.insert(
            "mock-model".to_string(),
            ModelCatalogEntry {
                provider: "mock".to_string(),
                model_name: "mock-model".to_string(),
                endpoint: "http://localhost/mock-infer".to_string(),
                secret_ref: "mock/api-key".to_string(),
                price_prompt_micro: 0,
                price_completion_micro: 0,
                context_window: 64_000,
                supports_tools: true,
                supports_structured_output: true,
                ..Default::default()
            },
        );

        // 4. OpenRouter GPT-4o-mini
        entries.insert(
            "openai/gpt-4o-mini".to_string(),
            ModelCatalogEntry {
                provider: "openrouter".to_string(),
                model_name: "openai/gpt-4o-mini".to_string(),
                endpoint: "https://openrouter.ai/api/v1/chat/completions".to_string(),
                secret_ref: "openrouter/api-key".to_string(),
                price_prompt_micro: 150_000,     // $0.15 per M
                price_completion_micro: 600_000, // $0.60 per M
                context_window: 128_000,
                supports_tools: true,
                supports_structured_output: true,
                ..Default::default()
            },
        );

        // 5. Vercel AI Gateway seed (OpenAI-compatible; distinct key from OpenRouter)
        entries.insert(
            "openai/gpt-4o".to_string(),
            ModelCatalogEntry {
                provider: "vercel".to_string(),
                model_name: "openai/gpt-4o".to_string(),
                endpoint: "https://ai-gateway.vercel.sh/v1/chat/completions".to_string(),
                secret_ref: "vercel/api-key".to_string(),
                price_prompt_micro: 0,
                price_completion_micro: 0,
                context_window: 128_000,
                supports_tools: true,
                supports_structured_output: true,
                ..Default::default()
            },
        );

        // 6. Open-weight models, priced at the maker's list price. These are data for the
        // gateway ids `zai/glm-5` and `alibaba/qwen3.6-27b` (`gateway_entry` derives the
        // Vercel entry from them); the broker serves them through the gateway, not through the
        // maker's own API. Both gateway ids are confirmed by one k = 1 call per model on the
        // owner's Mac before any paid run. DeepSeek V4 Pro is priced at its peak list price.
        //
        // owner-provided 2026-10-28: Z.ai first-party price,
        // docs.z.ai/guides/overview/pricing (as of 2026-10-17)
        entries.insert(
            "glm-5".to_string(),
            ModelCatalogEntry {
                provider: "zai".to_string(),
                model_name: "glm-5".to_string(),
                endpoint: String::new(),
                secret_ref: "zai/api-key".to_string(),
                price_prompt_micro: 1_000_000,
                price_completion_micro: 3_200_000,
                price_cache_read_micro: 200_000,
                context_window: 200_000,
                supports_tools: true,
                adapter: Some(AdapterSpec {
                    reasoning_field: "reasoning_content".to_string(),
                    thinking: "strip".to_string(),
                    thinking_off: r#"{"thinking":{"type":"disabled"}}"#.to_string(),
                    // The API accepts only `auto`.
                    tool_choice: false,
                    effort_field: String::new(),
                }),
                profile: Some("open-strong".to_string()),
                ..Default::default()
            },
        );

        // owner-approved 2026-10-28: Alibaba Model Studio international (Singapore) list
        // price, alibabacloud.com/help/en/model-studio/model-pricing. The price tier covers
        // up to 256K input; the larger window is unconfirmed.
        entries.insert(
            "qwen3.6-27b".to_string(),
            ModelCatalogEntry {
                provider: "alibaba".to_string(),
                model_name: "qwen3.6-27b".to_string(),
                endpoint: String::new(),
                secret_ref: "alibaba/api-key".to_string(),
                price_prompt_micro: 600_000,
                price_completion_micro: 3_600_000,
                context_window: 262_144,
                supports_tools: true,
                adapter: Some(AdapterSpec {
                    reasoning_field: "reasoning_content".to_string(),
                    // Qwen can emit tool calls inside its thinking.
                    thinking: "off".to_string(),
                    thinking_off: r#"{"enable_thinking":false}"#.to_string(),
                    // Unverified.
                    tool_choice: false,
                    effort_field: String::new(),
                }),
                profile: Some("open-small".to_string()),
                ..Default::default()
            },
        );

        // owner-approved 2026-10-29: DeepSeek peak list price (peak 01:00-04:00 and
        // 06:00-10:00 UTC weekdays; off-peak is half),
        // api-docs.deepseek.com/quick_start/pricing, read 2026-10-28
        entries.insert(
            "deepseek-v4-pro".to_string(),
            ModelCatalogEntry {
                provider: "deepseek".to_string(),
                model_name: "deepseek-v4-pro".to_string(),
                endpoint: String::new(),
                secret_ref: "deepseek/api-key".to_string(),
                price_prompt_micro: 1_320_000,
                price_completion_micro: 3_960_000,
                price_cache_read_micro: 44_000,
                context_window: 1_000_000,
                supports_tools: true,
                adapter: Some(AdapterSpec {
                    reasoning_field: "reasoning_content".to_string(),
                    // A tool round in thinking mode returns 400 unless the reasoning is
                    // sent back.
                    thinking: "keep".to_string(),
                    thinking_off: r#"{"thinking":{"type":"disabled"}}"#.to_string(),
                    // `required` or a named tool returns 400 in thinking mode.
                    tool_choice: false,
                    effort_field: String::new(),
                }),
                profile: Some("open-strong".to_string()),
                ..Default::default()
            },
        );

        Self {
            entries,
            default_model: "claude-3-5-sonnet-latest".to_string(),
            catalog_version: "2026-v3".to_string(),
            effective_date: "2026-09-01".to_string(),
        }
    }

    pub fn catalog_version(&self) -> &str {
        &self.catalog_version
    }

    pub fn effective_date(&self) -> &str {
        &self.effective_date
    }

    pub fn price_provenance(&self) -> PriceProvenance {
        PriceProvenance {
            catalog_version: self.catalog_version.clone(),
            pricing_effective_date: self.effective_date.clone(),
            calculation_status: "estimated".to_string(),
        }
    }

    /// The model a request gets when it names none (the last resort of a run's model choice).
    pub fn default_model(&self) -> &str {
        &self.default_model
    }

    pub fn add_entry(&mut self, entry: ModelCatalogEntry) {
        self.entries.insert(entry.model_name.clone(), entry);
    }

    /// Every provider the catalog can route to, sorted: the providers of its entries plus
    /// the two gateways.
    pub fn provider_names(&self) -> Vec<String> {
        let mut names: std::collections::BTreeSet<String> =
            self.entries.values().map(|e| e.provider.clone()).collect();
        names.insert("vercel".to_string());
        names.insert("openrouter".to_string());
        names.into_iter().collect()
    }

    pub fn knows_provider(&self, provider: &str) -> bool {
        self.provider_names().iter().any(|p| p == provider)
    }

    pub fn get(&self, model_name: &str) -> Option<&ModelCatalogEntry> {
        self.entries.get(model_name)
    }

    /// The maker's own catalog entry behind a gateway model id: `anthropic/claude-haiku-5.5`
    /// is `claude-haiku-5-5`. Any maker with a direct entry counts: Anthropic and OpenAI
    /// under any gateway prefix, every other maker (`zai`, `deepseek`, `alibaba`, ...) when
    /// the id's prefix names the entry's provider. Transports (gateway, router, mock) are
    /// not makers. `None` otherwise.
    fn direct_entry_for_gateway(&self, gateway_id: &str) -> Option<&ModelCatalogEntry> {
        let (maker, name) = gateway_id.split_once('/')?;
        let candidates = [name.to_string(), name.replace('.', "-")];
        candidates.iter().find_map(|c| {
            self.entries.get(c).filter(|e| {
                e.provider == "anthropic"
                    || e.provider == "openai"
                    || (e.provider == maker && !is_transport(&e.provider))
            })
        })
    }

    /// A gateway entry for `gateway_id` priced at the model maker's list price (the gateway may
    /// discount; it is transport, not the price source). `None` when no direct entry covers it.
    pub fn gateway_entry(&self, gateway_id: &str) -> Option<ModelCatalogEntry> {
        let d = self.direct_entry_for_gateway(gateway_id)?;
        Some(ModelCatalogEntry {
            provider: "vercel".to_string(),
            model_name: gateway_id.to_string(),
            endpoint: GATEWAY_ENDPOINT.to_string(),
            secret_ref: GATEWAY_SECRET_REF.to_string(),
            price_prompt_micro: d.price_prompt_micro,
            price_completion_micro: d.price_completion_micro,
            context_window: d.context_window,
            supports_tools: d.supports_tools,
            supports_structured_output: d.supports_structured_output,
            price_cache_read_micro: d.price_cache_read_micro,
            price_cache_write_micro: d.price_cache_write_micro,
            price_cache_write_long_micro: d.price_cache_write_long_micro,
            cache_mode: default_cache_mode("vercel", gateway_id),
            cache_retention: CacheRetention::default(),
            adapter: d.adapter.clone(),
            profile: d.profile.clone(),
        })
    }

    /// Where a gateway id's price comes from, e.g. `anthropic list price, catalog 2026-v3`.
    pub fn gateway_price_source(&self, gateway_id: &str) -> Option<String> {
        let d = self.direct_entry_for_gateway(gateway_id)?;
        Some(format!(
            "{} list price, catalog {}",
            d.provider, self.catalog_version
        ))
    }

    /// The entry that prices and routes a model at the maker's list price: the direct entry, or,
    /// for a gateway id (`anthropic/claude-haiku-5.5`), the gateway entry at the maker's price.
    /// The gateway is transport only; it is never the price source.
    pub fn list_price_entry(&self, model: &str) -> Option<ModelCatalogEntry> {
        self.get(model)
            .filter(|e| e.provider != "vercel")
            .cloned()
            .or_else(|| self.gateway_entry(model))
            .or_else(|| self.get(model).cloned())
    }

    /// Where a model's list price comes from, `None` for the mock or an unpriced model.
    pub fn list_price_source(&self, model: &str) -> Option<String> {
        if let Some(s) = self.gateway_price_source(model) {
            return Some(s);
        }
        let e = self.get(model)?;
        (e.provider != "mock" && (e.price_prompt_micro > 0 || e.price_completion_micro > 0)).then(
            || {
                format!(
                    "{} list price, catalog {}",
                    e.provider, self.catalog_version
                )
            },
        )
    }

    /// The micro-USD list price of `prompt` / `completion` tokens, `None` when the model is
    /// unknown or free (the mock). Cache tokens are charged at the input price, as the
    /// catalog's own estimate does.
    pub fn estimate_list_micro(&self, model: &str, prompt: i64, completion: i64) -> Option<i64> {
        let e = self.list_price_entry(model)?;
        if e.price_prompt_micro == 0 && e.price_completion_micro == 0 {
            return None;
        }
        let micro = (prompt as i128 * e.price_prompt_micro as i128
            + completion as i128 * e.price_completion_micro as i128)
            / 1_000_000;
        Some(micro as i64)
    }

    pub fn resolve(
        &self,
        pref: Option<&ModelPreference>,
        requirements: Option<&ModelRequirements>,
    ) -> Result<ModelCatalogEntry, VamInferenceError> {
        // 1. Identify model name to resolve
        let model_name = if let Some(p) = pref {
            if !p.model_name.is_empty() {
                p.model_name.as_str()
            } else if !p.provider.is_empty() {
                // Find first model matching the provider
                let found = self.entries.values().find(|e| e.provider == p.provider);
                if let Some(entry) = found {
                    entry.model_name.as_str()
                } else {
                    return Err(VamInferenceError::ModelNotFound {
                        model_name: format!("No models found for provider '{}'", p.provider),
                    });
                }
            } else {
                self.default_model.as_str()
            }
        } else {
            self.default_model.as_str()
        };

        // Preference provider "vercel" wins before static lookup so slash ids
        // already owned by OpenRouter (e.g. openai/gpt-4o-mini) route to the gateway.
        let entry = if pref.map(|p| p.provider.as_str()) == Some("vercel") {
            // The gateway is transport only: an id a maker's direct entry covers is priced at
            // that entry's list price (budgets charge it); any other id stays 0.
            self.gateway_entry(model_name)
                .unwrap_or_else(|| ModelCatalogEntry {
                    provider: "vercel".to_string(),
                    model_name: model_name.to_string(),
                    endpoint: GATEWAY_ENDPOINT.to_string(),
                    secret_ref: GATEWAY_SECRET_REF.to_string(),
                    price_prompt_micro: 0,
                    price_completion_micro: 0,
                    context_window: 128_000,
                    supports_tools: true,
                    supports_structured_output: true,
                    cache_mode: default_cache_mode("vercel", model_name),
                    ..Default::default()
                })
        } else if let Some(e) = self.entries.get(model_name).cloned() {
            e
        } else if pref.map(|p| p.provider.as_str()) == Some("openrouter") {
            // Dynamic OpenRouter model, only when the host said so. The host always sets the
            // provider; a slash id without one is not guessed (it is not found, below).
            ModelCatalogEntry {
                provider: "openrouter".to_string(),
                model_name: model_name.to_string(),
                endpoint: "https://openrouter.ai/api/v1/chat/completions".to_string(),
                secret_ref: "openrouter/api-key".to_string(),
                price_prompt_micro: 0,
                price_completion_micro: 0,
                context_window: 128_000,
                supports_tools: true,
                supports_structured_output: true,
                cache_mode: default_cache_mode("openrouter", model_name),
                ..Default::default()
            }
        } else {
            let hint = if model_name.contains('/') {
                " (no provider was set for this model; the host sets it at run creation: use \
                 `vercel` or `openrouter`)"
            } else {
                ""
            };
            return Err(VamInferenceError::ModelNotFound {
                model_name: format!("{model_name}{hint}"),
            });
        };

        // 2. Validate against requirements if present
        if let Some(reqs) = requirements {
            if reqs.min_context_tokens > 0 && entry.context_window < reqs.min_context_tokens {
                return Err(VamInferenceError::InvalidRequest {
                    reason: format!(
                        "Model '{}' context window {} is smaller than min required {}",
                        entry.model_name, entry.context_window, reqs.min_context_tokens
                    ),
                });
            }

            if reqs.requires_tool_calling && !entry.supports_tools {
                return Err(VamInferenceError::InvalidRequest {
                    reason: format!("Model '{}' does not support tool calling", entry.model_name),
                });
            }

            if reqs.requires_structured_output && !entry.supports_structured_output {
                return Err(VamInferenceError::InvalidRequest {
                    reason: format!(
                        "Model '{}' does not support structured output",
                        entry.model_name
                    ),
                });
            }

            if let Some(ref max_cost) = reqs.max_cost_per_million_tokens {
                if max_cost.currency == "USD" {
                    // Check prompt cost as simple gating proxy
                    if entry.price_prompt_micro > max_cost.micro_amount {
                        return Err(VamInferenceError::InvalidRequest {
                            reason: format!(
                                "Model '{}' prompt cost ${:.6} exceeds max allowed cost ${:.6}",
                                entry.model_name,
                                entry.price_prompt_micro as f64 / 1_000_000.0,
                                max_cost.micro_amount as f64 / 1_000_000.0
                            ),
                        });
                    }
                }
            }
        }

        Ok(entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::Cost;

    #[test]
    fn vercel_gateway_prices_at_the_makers_list_price() {
        let catalog = ModelCatalog::new();
        let pref = |name: &str| ModelPreference {
            provider: "vercel".to_string(),
            model_name: name.to_string(),
            ..Default::default()
        };
        let haiku = catalog
            .resolve(Some(&pref("anthropic/claude-haiku-5.5")), None)
            .unwrap();
        assert_eq!(haiku.provider, "vercel");
        assert_eq!(haiku.endpoint, GATEWAY_ENDPOINT);
        assert_eq!(haiku.secret_ref, "vercel/api-key");
        assert_eq!(haiku.price_prompt_micro, 100_000);
        assert_eq!(haiku.price_completion_micro, 500_000);
        assert_eq!(haiku.price_cache_read_micro, 10_000);
        assert_eq!(haiku.context_window, 200_000);
        assert_eq!(haiku.cache_mode, CacheMode::GatewayAuto);
        let sonnet = catalog
            .resolve(Some(&pref("anthropic/claude-sonnet-5.5")), None)
            .unwrap();
        assert_eq!(sonnet.price_prompt_micro, 2_000_000);
        assert_eq!(sonnet.price_completion_micro, 10_000_000);
        assert_eq!(
            catalog
                .gateway_price_source("anthropic/claude-sonnet-5.5")
                .as_deref(),
            Some(
                format!(
                    "anthropic list price, catalog {}",
                    catalog.catalog_version()
                )
                .as_str()
            )
        );
        // An id no direct entry covers stays free, as before.
        let other = catalog
            .resolve(Some(&pref("qwen/qwen3-max")), None)
            .unwrap();
        assert_eq!(other.price_prompt_micro, 0);
        assert_eq!(other.endpoint, GATEWAY_ENDPOINT);
        assert!(catalog.gateway_entry("qwen/qwen3-max").is_none());
        assert!(catalog.gateway_entry("claude-haiku-5-5").is_none());
    }

    #[test]
    fn claude_5_5_models_are_in_the_catalog_and_the_default_is_unchanged() {
        let catalog = ModelCatalog::new();
        let sonnet = catalog.get("claude-sonnet-5-5").expect("sonnet 5.5");
        assert_eq!(sonnet.provider, "anthropic");
        assert_eq!(sonnet.secret_ref, "anthropic/api-key");
        assert_eq!(sonnet.price_prompt_micro, 2_000_000);
        assert_eq!(sonnet.price_completion_micro, 10_000_000);
        let haiku = catalog.get("claude-haiku-5-5").expect("haiku 5.5");
        assert_eq!(haiku.provider, "anthropic");
        assert_eq!(haiku.price_prompt_micro, 100_000);
        assert_eq!(haiku.price_completion_micro, 500_000);
        let default = catalog.resolve(None, None).unwrap();
        assert_eq!(default.model_name, "claude-3-5-sonnet-latest");
    }

    #[test]
    fn test_catalog_resolve_default() {
        let catalog = ModelCatalog::new();
        let entry = catalog.resolve(None, None).unwrap();
        assert_eq!(entry.model_name, "claude-3-5-sonnet-latest");
        assert_eq!(entry.provider, "anthropic");
    }

    #[test]
    fn test_catalog_resolve_preference() {
        let catalog = ModelCatalog::new();
        let pref = ModelPreference {
            provider: "openai".to_string(),
            model_name: "gpt-4o".to_string(),
            temperature: 0.0,
            top_p: 0.0,
            max_completion_tokens: 0,
        };
        let entry = catalog.resolve(Some(&pref), None).unwrap();
        assert_eq!(entry.model_name, "gpt-4o");
        assert_eq!(entry.provider, "openai");
    }

    #[test]
    fn test_catalog_resolve_requirements_valid() {
        let catalog = ModelCatalog::new();
        let reqs = ModelRequirements {
            min_context_tokens: 100_000,
            requires_tool_calling: true,
            requires_structured_output: true,
            required_modalities: vec![],
            max_cost_per_million_tokens: Some(Cost {
                currency: "USD".to_string(),
                micro_amount: 10_000_000, // $10.00 max
            }),
        };
        let entry = catalog.resolve(None, Some(&reqs)).unwrap();
        assert_eq!(entry.model_name, "claude-3-5-sonnet-latest");
    }

    #[test]
    fn test_catalog_resolve_requirements_invalid_cost() {
        let catalog = ModelCatalog::new();
        let reqs = ModelRequirements {
            min_context_tokens: 100_000,
            requires_tool_calling: true,
            requires_structured_output: true,
            required_modalities: vec![],
            max_cost_per_million_tokens: Some(Cost {
                currency: "USD".to_string(),
                micro_amount: 1_000_000, // $1.00 max
            }),
        };
        let res = catalog.resolve(None, Some(&reqs));
        assert!(res.is_err());
        assert!(res
            .unwrap_err()
            .to_string()
            .contains("exceeds max allowed cost"));
    }

    #[test]
    fn test_catalog_resolve_vercel_overrides_openrouter_static() {
        let catalog = ModelCatalog::new();
        // Static catalog owns openai/gpt-4o-mini as OpenRouter; vercel preference must win.
        let static_entry = catalog.get("openai/gpt-4o-mini").unwrap();
        assert_eq!(static_entry.provider, "openrouter");

        let pref = ModelPreference {
            provider: "vercel".to_string(),
            model_name: "openai/gpt-4o-mini".to_string(),
            temperature: 0.0,
            top_p: 0.0,
            max_completion_tokens: 0,
        };
        let entry = catalog.resolve(Some(&pref), None).unwrap();
        assert_eq!(entry.provider, "vercel");
        assert_eq!(entry.model_name, "openai/gpt-4o-mini");
        assert_eq!(
            entry.endpoint,
            "https://ai-gateway.vercel.sh/v1/chat/completions"
        );
        assert_eq!(entry.secret_ref, "vercel/api-key");
    }

    #[test]
    fn test_catalog_resolve_vercel_haiku_uses_gateway() {
        let catalog = ModelCatalog::new();
        let pref = ModelPreference {
            provider: "vercel".to_string(),
            model_name: "anthropic/claude-haiku-5.5".to_string(),
            temperature: 0.0,
            top_p: 0.0,
            max_completion_tokens: 0,
        };
        let entry = catalog.resolve(Some(&pref), None).unwrap();
        assert_eq!(entry.provider, "vercel");
        assert_eq!(entry.model_name, "anthropic/claude-haiku-5.5");
        assert_eq!(
            entry.endpoint,
            "https://ai-gateway.vercel.sh/v1/chat/completions"
        );
        assert_eq!(entry.secret_ref, "vercel/api-key");
    }

    #[test]
    fn test_catalog_resolve_openrouter_preference_still_works() {
        let catalog = ModelCatalog::new();
        let pref = ModelPreference {
            provider: "openrouter".to_string(),
            model_name: "openai/gpt-4o-mini".to_string(),
            temperature: 0.0,
            top_p: 0.0,
            max_completion_tokens: 0,
        };
        let entry = catalog.resolve(Some(&pref), None).unwrap();
        assert_eq!(entry.provider, "openrouter");
        assert_eq!(
            entry.endpoint,
            "https://openrouter.ai/api/v1/chat/completions"
        );
        assert_eq!(entry.secret_ref, "openrouter/api-key");
    }

    #[test]
    fn test_catalog_resolve_bare_slash_without_provider_is_not_found() {
        let catalog = ModelCatalog::new();
        let mut pref = ModelPreference {
            provider: String::new(),
            model_name: "meta-llama/llama-3.1-8b-instruct".to_string(),
            temperature: 0.0,
            top_p: 0.0,
            max_completion_tokens: 0,
        };
        let err = catalog.resolve(Some(&pref), None).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("meta-llama/llama-3.1-8b-instruct"), "{text}");
        assert!(
            text.contains("vercel") && text.contains("openrouter"),
            "{text}"
        );
        // With the host's provider the same id resolves.
        pref.provider = "openrouter".to_string();
        let entry = catalog.resolve(Some(&pref), None).unwrap();
        assert_eq!(entry.provider, "openrouter");
        assert_eq!(entry.secret_ref, "openrouter/api-key");
        pref.provider = "vercel".to_string();
        let entry = catalog.resolve(Some(&pref), None).unwrap();
        assert_eq!(entry.secret_ref, "vercel/api-key");
    }

    #[test]
    fn test_catalog_provider_names_include_gateways() {
        let catalog = ModelCatalog::new();
        assert!(catalog.knows_provider("vercel"));
        assert!(catalog.knows_provider("openrouter"));
        assert!(catalog.knows_provider("anthropic"));
        assert!(!catalog.knows_provider("nokey"));
    }

    fn usage(prompt: i32, completion: i32, cached: i32, written: i32) -> vam_spec::v1::TokenUsage {
        vam_spec::v1::TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            cached_tokens: cached,
            total_tokens: prompt + completion,
            cache_creation_tokens: written,
            cached_tokens_unknown: false,
            cache_creation_long_tokens: 0,
        }
    }

    #[test]
    fn cost_charges_cached_and_written_tokens_at_their_prices() {
        let catalog = ModelCatalog::new();
        let sonnet = catalog.get("claude-sonnet-5-5").unwrap();
        // 1M prompt tokens: 700k uncached at $2, 200k read at $0.20, 100k
        // written (5 minutes) at $2.50; 100k completion at $10.
        let u = usage(1_000_000, 100_000, 200_000, 100_000);
        let expected = 700_000 * 2_000_000 / 1_000_000
            + 200_000 * 200_000 / 1_000_000
            + 100_000 * 2_500_000 / 1_000_000
            + 100_000 * 10_000_000 / 1_000_000;
        assert_eq!(sonnet.cost_micro(&u), expected);
        // Full price for the same prompt is more.
        assert!(sonnet.cost_micro(&u) < sonnet.cost_micro(&usage(1_000_000, 100_000, 0, 0)));
        // One hour writes cost 2x input.
        let mut long = usage(1_000_000, 0, 0, 100_000);
        long.cache_creation_long_tokens = 100_000;
        assert_eq!(
            sonnet.cost_micro(&long),
            900_000 * 2 + 100_000 * 4_000_000 / 1_000_000
        );
    }

    #[test]
    fn unpriced_cache_falls_back_to_the_input_price_and_unknown_is_full_price() {
        let entry = ModelCatalogEntry {
            price_prompt_micro: 1_000_000,
            price_completion_micro: 2_000_000,
            ..Default::default()
        };
        assert_eq!(
            entry.cost_micro(&usage(1_000_000, 0, 500_000, 100_000)),
            1_000_000
        );
        let priced = ModelCatalog::new().get("claude-haiku-5-5").unwrap().clone();
        let mut unknown = usage(1_000_000, 0, 0, 0);
        unknown.cached_tokens_unknown = true;
        unknown.cached_tokens = 900_000;
        assert_eq!(priced.cost_micro(&unknown), 100_000);
    }

    #[test]
    fn cost_never_goes_negative_on_inconsistent_usage() {
        let entry = ModelCatalog::new()
            .get("claude-sonnet-5-5")
            .unwrap()
            .clone();
        let cost = entry.cost_micro(&usage(100, 0, 500, 500));
        assert!(cost >= 0);
    }

    #[test]
    fn catalog_cache_defaults() {
        let catalog = ModelCatalog::new();
        let sonnet = catalog.get("claude-sonnet-5-5").unwrap();
        assert_eq!(sonnet.cache_mode, CacheMode::ExplicitBreakpoints);
        assert_eq!(sonnet.cache_retention, CacheRetention::Long);
        assert_eq!(
            (
                sonnet.price_cache_read_micro,
                sonnet.price_cache_write_micro,
                sonnet.price_cache_write_long_micro
            ),
            (200_000, 2_500_000, 4_000_000)
        );
        let haiku = catalog.get("claude-haiku-5-5").unwrap();
        assert_eq!(
            (
                haiku.price_cache_read_micro,
                haiku.price_cache_write_micro,
                haiku.price_cache_write_long_micro
            ),
            (10_000, 125_000, 200_000)
        );
        assert_eq!(
            catalog.get("gpt-4o").unwrap().cache_mode,
            CacheMode::PromptKey
        );
        assert_eq!(
            catalog.get("mock-model").unwrap().cache_mode,
            CacheMode::Implicit
        );
        let vercel = pref("vercel", "anthropic/claude-sonnet-5-5");
        let entry = catalog.resolve(Some(&vercel), None).unwrap();
        assert_eq!(entry.cache_mode, CacheMode::GatewayAuto);
        let router = pref("openrouter", "anthropic/claude-sonnet-5-5");
        let entry = catalog.resolve(Some(&router), None).unwrap();
        assert_eq!(entry.cache_mode, CacheMode::ExplicitBreakpoints);
        assert_eq!(entry.cache_retention, CacheRetention::Short);
        let other = pref("openrouter", "meta/llama");
        assert_eq!(
            catalog.resolve(Some(&other), None).unwrap().cache_mode,
            CacheMode::Implicit
        );
    }

    /// A test-only maker entry: not a real model, so it asserts no real price.
    fn fixture_entry(provider: &str, name: &str) -> ModelCatalogEntry {
        ModelCatalogEntry {
            provider: provider.to_string(),
            model_name: name.to_string(),
            price_prompt_micro: 700_000,
            price_completion_micro: 2_800_000,
            price_cache_read_micro: 100_000,
            context_window: 256_000,
            supports_tools: true,
            adapter: Some(AdapterSpec {
                reasoning_field: "reasoning_content".to_string(),
                thinking: "strip".to_string(),
                thinking_off: r#"{"thinking":{"type":"disabled"}}"#.to_string(),
                tool_choice: false,
                effort_field: String::new(),
            }),
            profile: Some("open-strong".to_string()),
            ..Default::default()
        }
    }

    #[test]
    fn any_maker_with_a_direct_entry_prices_its_gateway_id() {
        let mut catalog = ModelCatalog::new();
        catalog.add_entry(fixture_entry("zai", "fixture-glm"));
        catalog.add_entry(fixture_entry("deepseek", "fixture-ds"));
        // Resolved through the gateway: maker price, maker named, adapter and profile carried.
        let e = catalog
            .resolve(Some(&pref("vercel", "zai/fixture-glm")), None)
            .unwrap();
        assert_eq!(e.provider, "vercel");
        assert_eq!(e.endpoint, GATEWAY_ENDPOINT);
        assert_eq!(
            (e.price_prompt_micro, e.price_completion_micro),
            (700_000, 2_800_000)
        );
        assert_eq!(e.price_cache_read_micro, 100_000);
        assert_eq!(e.profile.as_deref(), Some("open-strong"));
        assert_eq!(
            e.adapter.as_ref().unwrap().reasoning_field,
            "reasoning_content"
        );
        assert_eq!(
            catalog.gateway_price_source("zai/fixture-glm").as_deref(),
            Some(format!("zai list price, catalog {}", catalog.catalog_version()).as_str())
        );
        assert_eq!(
            catalog
                .gateway_price_source("deepseek/fixture-ds")
                .as_deref(),
            Some(format!("deepseek list price, catalog {}", catalog.catalog_version()).as_str())
        );
        // The prefix must name the entry's own maker.
        assert!(catalog.gateway_entry("alibaba/fixture-glm").is_none());
        assert!(catalog.gateway_entry("fixture-glm").is_none());
    }

    #[test]
    fn the_real_open_model_entries_price_their_gateway_ids_at_the_maker() {
        let catalog = ModelCatalog::new();
        let v = catalog.catalog_version().to_string();
        let glm = catalog
            .resolve(Some(&pref("vercel", "zai/glm-5")), None)
            .unwrap();
        assert_eq!(glm.provider, "vercel");
        assert_eq!(
            (
                glm.price_prompt_micro,
                glm.price_completion_micro,
                glm.price_cache_read_micro
            ),
            (1_000_000, 3_200_000, 200_000)
        );
        assert_eq!((glm.context_window, glm.supports_tools), (200_000, true));
        let a = glm.adapter.as_ref().unwrap();
        assert_eq!((a.thinking.as_str(), a.tool_choice), ("strip", false));
        assert_eq!(glm.profile.as_deref(), Some("open-strong"));
        assert_eq!(
            catalog.gateway_price_source("zai/glm-5"),
            Some(format!("zai list price, catalog {v}"))
        );
        let qwen = catalog
            .resolve(Some(&pref("vercel", "alibaba/qwen3.6-27b")), None)
            .unwrap();
        assert_eq!(
            (
                qwen.price_prompt_micro,
                qwen.price_completion_micro,
                qwen.price_cache_read_micro
            ),
            (600_000, 3_600_000, 0)
        );
        assert_eq!((qwen.context_window, qwen.supports_tools), (262_144, true));
        let a = qwen.adapter.as_ref().unwrap();
        assert_eq!(a.thinking, "off");
        assert_eq!(a.thinking_off, r#"{"enable_thinking":false}"#);
        assert_eq!(qwen.profile.as_deref(), Some("open-small"));
        assert_eq!(
            catalog.gateway_price_source("alibaba/qwen3.6-27b"),
            Some(format!("alibaba list price, catalog {v}"))
        );
        let ds = catalog
            .resolve(Some(&pref("vercel", "deepseek/deepseek-v4-pro")), None)
            .unwrap();
        assert_eq!(ds.provider, "vercel");
        assert_eq!(
            (
                ds.price_prompt_micro,
                ds.price_completion_micro,
                ds.price_cache_read_micro
            ),
            (1_320_000, 3_960_000, 44_000)
        );
        assert_eq!((ds.context_window, ds.supports_tools), (1_000_000, true));
        let a = ds.adapter.as_ref().unwrap();
        assert_eq!((a.thinking.as_str(), a.tool_choice), ("keep", false));
        assert_eq!(ds.profile.as_deref(), Some("open-strong"));
        assert_eq!(
            catalog
                .gateway_price_source("deepseek/deepseek-v4-pro")
                .as_deref(),
            Some("deepseek list price, catalog 2026-v3")
        );
        // The prefix must name the maker.
        assert!(catalog.gateway_entry("alibaba/glm-5").is_none());
        // Claude gateway entries are unchanged.
        let c = catalog.gateway_entry("anthropic/claude-haiku-5.5").unwrap();
        assert_eq!(
            (c.price_prompt_micro, c.price_completion_micro),
            (100_000, 500_000)
        );
    }

    #[test]
    fn list_price_rule_vectors() {
        let catalog = ModelCatalog::new();
        let e = catalog.list_price_entry("claude-haiku-5-5").unwrap();
        assert_eq!(e.provider, "anthropic");
        assert_eq!(
            catalog.list_price_source("claude-haiku-5-5").unwrap(),
            format!(
                "anthropic list price, catalog {}",
                catalog.catalog_version()
            )
        );
        assert_eq!(
            catalog.estimate_list_micro("claude-haiku-5-5", 1_000_000, 1_000_000),
            Some(600_000)
        );
        let g = catalog
            .list_price_entry("anthropic/claude-haiku-5.5")
            .unwrap();
        assert_eq!(g.provider, "vercel");
        assert_eq!(
            catalog.estimate_list_micro("anthropic/claude-haiku-5.5", 1_000_000, 1_000_000),
            Some(600_000)
        );
        assert!(catalog
            .list_price_source("anthropic/claude-haiku-5.5")
            .unwrap()
            .starts_with("anthropic list price"));
        assert_eq!(catalog.estimate_list_micro("mock-model", 10, 10), None);
        assert_eq!(catalog.list_price_source("mock-model"), None);
        assert_eq!(catalog.estimate_list_micro("no-such-model", 10, 10), None);
        assert!(catalog.list_price_entry("no-such-model").is_none());
    }

    #[test]
    fn transports_are_not_makers() {
        let mut catalog = ModelCatalog::new();
        catalog.add_entry(fixture_entry("vercel", "fixture-v"));
        catalog.add_entry(fixture_entry("openrouter", "fixture-r"));
        assert!(catalog.gateway_entry("vercel/fixture-v").is_none());
        assert!(catalog.gateway_entry("openrouter/fixture-r").is_none());
        // The seeded gateway and router entries stay unpriced.
        assert!(catalog.gateway_entry("openai/gpt-4o").is_some()); // via the OpenAI direct entry
    }

    #[test]
    fn claude_and_openai_entries_carry_no_adapter_and_gateway_entries_stay_the_same() {
        let catalog = ModelCatalog::new();
        for name in ["claude-haiku-5-5", "claude-sonnet-5-5", "gpt-4o"] {
            let d = catalog.get(name).unwrap();
            assert!(d.adapter.is_none() && d.profile.is_none(), "{name}");
        }
        let g = catalog.gateway_entry("anthropic/claude-haiku-5.5").unwrap();
        assert!(g.adapter.is_none() && g.profile.is_none());
        assert_eq!(
            (
                g.price_prompt_micro,
                g.price_completion_micro,
                g.context_window
            ),
            (100_000, 500_000, 200_000)
        );
    }

    #[test]
    fn every_catalog_profile_names_a_harness_profile_and_a_known_thinking_mode() {
        let catalog = ModelCatalog::new();
        let mut all: Vec<_> = catalog.entries.values().cloned().collect();
        all.push(fixture_entry("zai", "fixture-glm"));
        for e in all {
            if let Some(p) = &e.profile {
                assert!(vam_spec::harness::profile(p).is_some(), "{}", e.model_name);
            }
            if let Some(a) = &e.adapter {
                assert!(["strip", "keep", "off"].contains(&a.thinking.as_str()));
                assert!(!a.reasoning_field.is_empty());
            }
        }
    }

    fn pref(provider: &str, model: &str) -> ModelPreference {
        ModelPreference {
            provider: provider.to_string(),
            model_name: model.to_string(),
            temperature: 0.0,
            top_p: 1.0,
            max_completion_tokens: 0,
        }
    }
}
