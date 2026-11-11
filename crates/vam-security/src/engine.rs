use crate::pdp::{DecisionOutcome, EvaluationRequest, PolicyDecisionPoint, PolicyMode};
use crate::SecurityError;
use arc_swap::ArcSwap;
use cedar_policy::{
    Authorizer, Context, Decision, Entities, EntityUid, PolicySet, Request, Schema, ValidationMode,
    Validator,
};
use sha2::{Digest, Sha256};
use std::str::FromStr;
use std::sync::Arc;

pub const DEFAULT_CEDAR_SCHEMA: &str = include_str!("../schema/vam.cedarschema");

#[derive(Clone)]
pub struct CedarPolicyEngine {
    policy_set: PolicySet,
    schema: Schema,
    authorizer: Authorizer,
    mode: PolicyMode,
    generation: u64,
    bundle_digest: String,
}

impl CedarPolicyEngine {
    pub fn new(
        schema_src: &str,
        policy_src: &str,
        mode: PolicyMode,
        generation: u64,
    ) -> Result<Self, SecurityError> {
        let (schema, _) = Schema::from_cedarschema_str(schema_src)
            .map_err(|e| SecurityError::SchemaValidation(format!("Schema parse error: {}", e)))?;

        let policy_set = PolicySet::from_str(policy_src)
            .map_err(|e| SecurityError::PolicySyntax(format!("Policy set parse error: {}", e)))?;

        let validator = Validator::new(schema.clone());
        let validation_result = validator.validate(&policy_set, ValidationMode::Strict);
        if !validation_result.validation_passed() {
            let errors: Vec<String> = validation_result
                .validation_errors()
                .map(|e| e.to_string())
                .collect();
            return Err(SecurityError::SchemaValidation(format!(
                "Strict schema validation failed: {:?}",
                errors
            )));
        }

        let mut hasher = Sha256::new();
        hasher.update(policy_src.as_bytes());
        let bundle_digest = format!("{:x}", hasher.finalize());

        Ok(Self {
            policy_set,
            schema,
            authorizer: Authorizer::new(),
            mode,
            generation,
            bundle_digest,
        })
    }

    pub fn from_default_schema(
        policy_src: &str,
        mode: PolicyMode,
        generation: u64,
    ) -> Result<Self, SecurityError> {
        Self::new(DEFAULT_CEDAR_SCHEMA, policy_src, mode, generation)
    }

    pub fn empty(mode: PolicyMode, generation: u64) -> Result<Self, SecurityError> {
        Self::from_default_schema("", mode, generation)
    }
}

impl PolicyDecisionPoint for CedarPolicyEngine {
    fn evaluate(&self, req: &EvaluationRequest) -> DecisionOutcome {
        let principal = match EntityUid::from_str(&req.principal) {
            Ok(uid) => uid,
            Err(e) => {
                return DecisionOutcome::Deny {
                    diagnostic_message: format!(
                        "Malformed principal EntityUid '{}': {}",
                        req.principal, e
                    ),
                    matching_policies: Vec::new(),
                };
            }
        };

        let action = match EntityUid::from_str(&req.action) {
            Ok(uid) => uid,
            Err(e) => {
                return DecisionOutcome::Deny {
                    diagnostic_message: format!(
                        "Malformed action EntityUid '{}': {}",
                        req.action, e
                    ),
                    matching_policies: Vec::new(),
                };
            }
        };

        let resource = match EntityUid::from_str(&req.resource) {
            Ok(uid) => uid,
            Err(e) => {
                return DecisionOutcome::Deny {
                    diagnostic_message: format!(
                        "Malformed resource EntityUid '{}': {}",
                        req.resource, e
                    ),
                    matching_policies: Vec::new(),
                };
            }
        };

        let context_json = req.context.to_cedar_json_value();
        let context = match Context::from_json_value(context_json, Some((&self.schema, &action))) {
            Ok(ctx) => ctx,
            Err(e) => {
                // Fail-closed on context errors (Decision S9)
                return DecisionOutcome::Deny {
                    diagnostic_message: format!(
                        "Failed to parse evaluation context into Cedar schema: {}",
                        e
                    ),
                    matching_policies: Vec::new(),
                };
            }
        };

        let cedar_request = match Request::new(principal, action, resource, context, None) {
            Ok(r) => r,
            Err(e) => {
                return DecisionOutcome::Deny {
                    diagnostic_message: format!("Failed to construct Cedar Request: {}", e),
                    matching_policies: Vec::new(),
                };
            }
        };

        let entities = match request_entities(&self.schema, &req.entities) {
            Ok(entities) => entities,
            Err(err) => {
                return DecisionOutcome::Deny {
                    diagnostic_message: err,
                    matching_policies: Vec::new(),
                };
            }
        };
        let response = self
            .authorizer
            .is_authorized(&cedar_request, &self.policy_set, &entities);

        let matching_policies: Vec<String> = response
            .diagnostics()
            .reason()
            .map(|p| p.to_string())
            .collect();

        let errors: Vec<String> = response
            .diagnostics()
            .errors()
            .map(|e| e.to_string())
            .collect();

        if !errors.is_empty() {
            return DecisionOutcome::Deny {
                diagnostic_message: format!("Cedar evaluation errors encountered: {:?}", errors),
                matching_policies,
            };
        }

        match response.decision() {
            Decision::Allow => DecisionOutcome::Allow { matching_policies },
            Decision::Deny => {
                if self.mode == PolicyMode::Shadow {
                    // Shadow mode: log would_deny, but allow execution
                    DecisionOutcome::Allow {
                        matching_policies: vec![format!(
                            "SHADOW_PERMIT(would_deny_by: {:?})",
                            matching_policies
                        )],
                    }
                } else {
                    DecisionOutcome::Deny {
                        diagnostic_message:
                            "Policy explicitly denied or no permit rule matched (deny-by-default)"
                                .to_string(),
                        matching_policies,
                    }
                }
            }
        }
    }

    fn mode(&self) -> PolicyMode {
        self.mode
    }

    fn generation(&self) -> u64 {
        self.generation
    }

    fn policy_bundle_digest(&self) -> String {
        self.bundle_digest.clone()
    }
}

/// Thread-safe, atomically-swappable PDP container with monotonic generation fencing.
pub struct ArcSwapPolicyEngine {
    inner: ArcSwap<CedarPolicyEngine>,
}

impl ArcSwapPolicyEngine {
    pub fn new(engine: CedarPolicyEngine) -> Self {
        Self {
            inner: ArcSwap::from_pointee(engine),
        }
    }

    pub fn load(&self) -> arc_swap::Guard<Arc<CedarPolicyEngine>> {
        self.inner.load()
    }

    pub fn hot_reload(&self, new_engine: CedarPolicyEngine) -> Result<(), SecurityError> {
        let current = self.inner.load();
        if new_engine.generation <= current.generation {
            return Err(SecurityError::StaleGeneration {
                current: current.generation,
                attempted: new_engine.generation,
            });
        }
        self.inner.store(Arc::new(new_engine));
        Ok(())
    }
}

/// Empty `tools` uses `Entities::empty`, the same store legacy requests use.
fn request_entities(schema: &Schema, tools: &[crate::pdp::ToolEntity]) -> Result<Entities, String> {
    if tools.is_empty() {
        return Ok(Entities::empty());
    }
    let json =
        serde_json::Value::Array(tools.iter().map(|tool| tool.cedar_entity_json()).collect());
    Entities::from_json_value(json, Some(schema))
        .map_err(|err| format!("Failed to load request entities: {err}"))
}

impl PolicyDecisionPoint for ArcSwapPolicyEngine {
    fn evaluate(&self, req: &EvaluationRequest) -> DecisionOutcome {
        self.inner.load().evaluate(req)
    }

    fn mode(&self) -> PolicyMode {
        self.inner.load().mode()
    }

    fn generation(&self) -> u64 {
        self.inner.load().generation()
    }

    fn policy_bundle_digest(&self) -> String {
        self.inner.load().policy_bundle_digest()
    }
}
