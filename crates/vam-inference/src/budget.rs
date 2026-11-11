use crate::error::VamInferenceError;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use vam_spec::v1::Cost;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct EstimatedCost {
    pub currency: String,
    pub micro_amount: i64,
}

impl From<Cost> for EstimatedCost {
    fn from(c: Cost) -> Self {
        Self {
            currency: c.currency,
            micro_amount: c.micro_amount,
        }
    }
}

impl From<EstimatedCost> for Cost {
    fn from(c: EstimatedCost) -> Self {
        Self {
            currency: c.currency,
            micro_amount: c.micro_amount,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct UsageSummaryWithProvenance {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub estimated_cost: EstimatedCost,
    pub model_billed: String,
    pub provenance: Option<crate::catalog::PriceProvenance>,
}

#[derive(Debug)]
pub struct SessionBudget {
    pub max_financial_budget: Option<Cost>,
    /// Host-stamped owner for spend attribution only. Not used for caps.
    user_id: Option<String>,
    accumulated_cost_micro: Mutex<i64>,
    accumulated_tokens: Mutex<i64>,
    accumulated_prompt_tokens: Mutex<i64>,
    accumulated_completion_tokens: Mutex<i64>,
    /// Set once a run's persisted totals have been added. Further absorbs no-op
    /// so a later tool call does not add the same baseline again.
    baseline_absorbed: AtomicBool,
}

impl SessionBudget {
    pub fn new(max_financial_budget: Option<Cost>) -> Self {
        Self {
            max_financial_budget,
            user_id: None,
            accumulated_cost_micro: Mutex::new(0),
            accumulated_tokens: Mutex::new(0),
            accumulated_prompt_tokens: Mutex::new(0),
            accumulated_completion_tokens: Mutex::new(0),
            baseline_absorbed: AtomicBool::new(false),
        }
    }

    /// Add a run's persisted spend and token totals once.
    ///
    /// Called when a session attaches to a run that already has metering, and
    /// again on the next PEP evaluation. The second call does not add.
    pub fn absorb_baseline(
        &self,
        spend_micro: i64,
        token_count: i64,
        prompt_tokens: i64,
        completion_tokens: i64,
    ) {
        if self.baseline_absorbed.swap(true, Ordering::SeqCst) {
            return;
        }
        *self.accumulated_cost_micro.lock().unwrap() += spend_micro;
        *self.accumulated_tokens.lock().unwrap() += token_count;
        *self.accumulated_prompt_tokens.lock().unwrap() += prompt_tokens;
        *self.accumulated_completion_tokens.lock().unwrap() += completion_tokens;
    }

    /// Tag spend with a host-stamped `user_id` for attribution. No quota logic.
    pub fn with_user_id(mut self, user_id: impl Into<String>) -> Self {
        let id = user_id.into();
        self.user_id = if id.is_empty() { None } else { Some(id) };
        self
    }

    pub fn user_id(&self) -> Option<&str> {
        self.user_id.as_deref()
    }

    /// Check if the session is already over budget.
    pub fn check_preflight(&self) -> Result<(), VamInferenceError> {
        if let Some(ref limit) = self.max_financial_budget {
            let current = *self.accumulated_cost_micro.lock().unwrap();
            if current >= limit.micro_amount {
                return Err(VamInferenceError::BudgetExceeded {
                    limit: format!(
                        "{} {:.6}",
                        limit.currency,
                        limit.micro_amount as f64 / 1_000_000.0
                    ),
                    current: format!("{} {:.6}", limit.currency, current as f64 / 1_000_000.0),
                });
            }
        }
        Ok(())
    }

    /// Record one inference with its cost, cache reads and writes priced from
    /// the catalog entry (`ModelCatalogEntry::cost_micro`): the same number
    /// the broker reports as the exchange's spend.
    pub fn record_exchange(
        &self,
        usage: &vam_spec::v1::TokenUsage,
        entry: &crate::catalog::ModelCatalogEntry,
    ) -> Result<Cost, VamInferenceError> {
        self.record_cost(
            usage.prompt_tokens,
            usage.completion_tokens,
            entry.cost_micro(usage),
        )
    }

    /// Record a usage event with its cost, every prompt token at the input
    /// price (no cache information).
    pub fn record_usage(
        &self,
        prompt_tokens: i32,
        completion_tokens: i32,
        price_prompt_micro: i64,     // cost per million tokens
        price_completion_micro: i64, // cost per million tokens
    ) -> Result<Cost, VamInferenceError> {
        let prompt_cost = (prompt_tokens as i64 * price_prompt_micro) / 1_000_000;
        let completion_cost = (completion_tokens as i64 * price_completion_micro) / 1_000_000;
        self.record_cost(
            prompt_tokens,
            completion_tokens,
            prompt_cost + completion_cost,
        )
    }

    fn record_cost(
        &self,
        prompt_tokens: i32,
        completion_tokens: i32,
        total_cost_micro: i64,
    ) -> Result<Cost, VamInferenceError> {
        let mut current = self.accumulated_cost_micro.lock().unwrap();
        *current += total_cost_micro;

        let mut tokens = self.accumulated_tokens.lock().unwrap();
        *tokens += (prompt_tokens + completion_tokens) as i64;

        let mut p_tokens = self.accumulated_prompt_tokens.lock().unwrap();
        *p_tokens += prompt_tokens as i64;

        let mut c_tokens = self.accumulated_completion_tokens.lock().unwrap();
        *c_tokens += completion_tokens as i64;

        let currency = self
            .max_financial_budget
            .as_ref()
            .map(|c| c.currency.clone())
            .unwrap_or_else(|| "USD".to_string());

        if let Some(ref limit) = self.max_financial_budget {
            if *current > limit.micro_amount {
                return Err(VamInferenceError::BudgetExceeded {
                    limit: format!(
                        "{} {:.6}",
                        limit.currency,
                        limit.micro_amount as f64 / 1_000_000.0
                    ),
                    current: format!("{} {:.6}", limit.currency, *current as f64 / 1_000_000.0),
                });
            }
        }

        Ok(Cost {
            currency,
            micro_amount: total_cost_micro,
        })
    }

    pub fn get_accumulated_cost(&self) -> i64 {
        *self.accumulated_cost_micro.lock().unwrap()
    }

    pub fn get_accumulated_tokens(&self) -> i64 {
        *self.accumulated_tokens.lock().unwrap()
    }

    pub fn get_accumulated_prompt_tokens(&self) -> i64 {
        *self.accumulated_prompt_tokens.lock().unwrap()
    }

    pub fn get_accumulated_completion_tokens(&self) -> i64 {
        *self.accumulated_completion_tokens.lock().unwrap()
    }

    pub fn build_usage_summary(
        &self,
        model_billed: &str,
        provenance: Option<crate::catalog::PriceProvenance>,
    ) -> UsageSummaryWithProvenance {
        let currency = self
            .max_financial_budget
            .as_ref()
            .map(|c| c.currency.clone())
            .unwrap_or_else(|| "USD".to_string());
        UsageSummaryWithProvenance {
            prompt_tokens: self.get_accumulated_prompt_tokens(),
            completion_tokens: self.get_accumulated_completion_tokens(),
            total_tokens: self.get_accumulated_tokens(),
            estimated_cost: EstimatedCost {
                currency,
                micro_amount: self.get_accumulated_cost(),
            },
            model_billed: model_billed.to_string(),
            provenance,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_within_limit() {
        let budget = SessionBudget::new(Some(Cost {
            currency: "USD".to_string(),
            micro_amount: 10_000_000, // $10.00
        }));

        assert!(budget.check_preflight().is_ok());

        // Prompt price: $3/Mtok, Completion price: $15/Mtok
        // 1M prompt tokens ($3.00) + 0.1M completion tokens ($1.50) = $4.50
        let cost = budget
            .record_usage(1_000_000, 100_000, 3_000_000, 15_000_000)
            .unwrap();
        assert_eq!(cost.micro_amount, 4_500_000);
        assert_eq!(budget.get_accumulated_cost(), 4_500_000);

        assert!(budget.check_preflight().is_ok());
    }

    #[test]
    fn test_budget_exceeded() {
        let budget = SessionBudget::new(Some(Cost {
            currency: "USD".to_string(),
            micro_amount: 5_000_000, // $5.00 limit
        }));

        // 2M prompt tokens @ $3/Mtok = $6.00 (exceeds $5.00 limit)
        let res = budget.record_usage(2_000_000, 0, 3_000_000, 15_000_000);
        assert!(res.is_err());
        assert!(budget.check_preflight().is_err());
    }

    #[test]
    fn test_usage_summary_with_provenance() {
        let budget = SessionBudget::new(Some(Cost {
            currency: "USD".to_string(),
            micro_amount: 10_000_000,
        }));

        let catalog = crate::catalog::ModelCatalog::new();
        let _ = budget
            .record_usage(500_000, 100_000, 3_000_000, 15_000_000)
            .unwrap();

        let summary = budget
            .build_usage_summary("claude-3-5-sonnet-latest", Some(catalog.price_provenance()));
        assert_eq!(summary.prompt_tokens, 500_000);
        assert_eq!(summary.completion_tokens, 100_000);
        assert_eq!(summary.total_tokens, 600_000);
        assert_eq!(summary.estimated_cost.micro_amount, 3_000_000);
        assert_eq!(summary.model_billed, "claude-3-5-sonnet-latest");
        let prov = summary.provenance.unwrap();
        assert_eq!(prov.catalog_version, catalog.catalog_version());
        assert_eq!(prov.pricing_effective_date, "2026-09-01");
        assert_eq!(prov.calculation_status, "estimated");
    }

    #[test]
    fn user_id_is_attribution_only() {
        let budget = SessionBudget::new(Some(Cost {
            currency: "USD".to_string(),
            micro_amount: 10_000_000,
        }))
        .with_user_id("alice");
        assert_eq!(budget.user_id(), Some("alice"));
        assert!(budget.check_preflight().is_ok());
        let cost = budget.record_usage(1_000, 0, 1_000_000, 1_000_000).unwrap();
        assert_eq!(cost.micro_amount, 1_000);
        assert_eq!(budget.user_id(), Some("alice"));
        assert!(SessionBudget::new(None)
            .with_user_id("")
            .user_id()
            .is_none());
    }

    #[test]
    fn absorb_baseline_adds_once_on_top_of_usage() {
        let budget = SessionBudget::new(None);
        budget.record_usage(1, 1, 1_000_000, 1_000_000).unwrap();
        budget.absorb_baseline(10, 4, 3, 1);
        assert_eq!(budget.get_accumulated_cost(), 12);
        assert_eq!(budget.get_accumulated_tokens(), 6);
        assert_eq!(budget.get_accumulated_prompt_tokens(), 4);
        assert_eq!(budget.get_accumulated_completion_tokens(), 2);
        budget.absorb_baseline(100, 100, 100, 100);
        assert_eq!(budget.get_accumulated_cost(), 12);
        assert_eq!(budget.get_accumulated_tokens(), 6);
    }

    #[test]
    fn record_exchange_uses_the_same_cost_as_the_broker() {
        let catalog = crate::catalog::ModelCatalog::new();
        let entry = catalog.get("claude-sonnet-5-5").unwrap();
        let usage = vam_spec::v1::TokenUsage {
            prompt_tokens: 100_000,
            completion_tokens: 1_000,
            cached_tokens: 80_000,
            total_tokens: 101_000,
            cache_creation_tokens: 10_000,
            cached_tokens_unknown: false,
            cache_creation_long_tokens: 0,
        };
        let budget = SessionBudget::new(None);
        let cost = budget.record_exchange(&usage, entry).unwrap();
        assert_eq!(cost.micro_amount, entry.cost_micro(&usage));
        // 10k uncached x $2 + 80k read x $0.20 + 10k written x $2.50 + 1k out x $10.
        assert_eq!(cost.micro_amount, 20_000 + 16_000 + 25_000 + 10_000);
        assert_eq!(budget.get_accumulated_cost(), cost.micro_amount);
        assert_eq!(budget.get_accumulated_prompt_tokens(), 100_000);
        // The old wrapper still charges every prompt token at the input price.
        let full = SessionBudget::new(None)
            .record_usage(100_000, 1_000, 2_000_000, 10_000_000)
            .unwrap();
        assert!(full.micro_amount > cost.micro_amount);
    }

    #[test]
    fn cached_tokens_do_not_trip_a_budget_that_full_price_would() {
        let catalog = crate::catalog::ModelCatalog::new();
        let entry = catalog.get("claude-sonnet-5-5").unwrap();
        let usage = |cached: i32, unknown: bool| vam_spec::v1::TokenUsage {
            prompt_tokens: 100_000,
            completion_tokens: 0,
            cached_tokens: cached,
            total_tokens: 100_000,
            cache_creation_tokens: 0,
            cached_tokens_unknown: unknown,
            cache_creation_long_tokens: 0,
        };
        let limit = Cost {
            currency: "USD".to_string(),
            micro_amount: 50_000,
        };
        // Full price would be 200_000 micro; 99_000 cached tokens cost 2_000 + 19_800.
        let cached = SessionBudget::new(Some(limit.clone()));
        assert!(cached.record_exchange(&usage(99_000, false), entry).is_ok());
        // The same usage with unknown cache counts is charged in full and trips.
        let unknown = SessionBudget::new(Some(limit));
        assert!(unknown
            .record_exchange(&usage(99_000, true), entry)
            .is_err());
    }
}
