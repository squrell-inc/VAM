//! Operator-published node model allowlist and default model.
//!
//! Empty list means unrestricted (operator has not published yet). A non-empty
//! list is exclusive: a model the caller names must match an entry exactly.
//! That is the only refusal. An agent's preferred models are a hint and never
//! block a run (see [`resolve_run_model`]).

use std::collections::HashSet;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::params;
use vam_spec::v1::{AgentSpec, RunSpec};

use crate::error::OrchestratorError;
use crate::run_context::extract_model;
use crate::workflow::SqliteWorkflowStore;

const MAX_MODEL_LEN: usize = 256;
const SETTINGS_ROW_ID: i64 = 1;

/// Operator default for tool-level HITL approval lifetime. Replaces the
/// hardcoded one-hour wait.
pub const DEFAULT_TOOL_HITL_TIMEOUT_SECS: i64 = 3600;

/// Same ceiling as a playbook gate timeout.
pub const MAX_TOOL_HITL_TIMEOUT_SECS: i64 = 30 * 24 * 60 * 60;

pub fn normalize_tool_hitl_timeout_secs(secs: i64) -> Result<i64, OrchestratorError> {
    if secs <= 0 {
        return Err(OrchestratorError::InvalidContract(
            "tool HITL timeout must be a positive number of seconds".into(),
        ));
    }
    if secs > MAX_TOOL_HITL_TIMEOUT_SECS {
        return Err(OrchestratorError::InvalidContract(
            "tool HITL timeout must be at most 30 days".into(),
        ));
    }
    Ok(secs)
}

impl SqliteWorkflowStore {
    pub fn initialize_operator_allowed_models_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS node_operator_settings (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                allowed_models_json TEXT NOT NULL DEFAULT '[]',
                updated_at INTEGER NOT NULL
            );
            ",
        )?;
        let _ = connection.execute(
            "ALTER TABLE node_operator_settings ADD COLUMN tool_hitl_timeout_secs INTEGER",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE node_operator_settings ADD COLUMN default_model TEXT",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE node_operator_settings ADD COLUMN default_gateway TEXT",
            [],
        );
        Ok(())
    }

    /// Tool-level HITL lifetime in seconds. Missing or invalid rows use
    /// [`DEFAULT_TOOL_HITL_TIMEOUT_SECS`].
    pub fn tool_hitl_timeout_secs(&self) -> Result<i64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let stored: Option<i64> = connection
            .query_row(
                "SELECT tool_hitl_timeout_secs FROM node_operator_settings WHERE id = ?1",
                params![SETTINGS_ROW_ID],
                |row| row.get(0),
            )
            .optional_not_found()?;
        Ok(match stored {
            Some(secs) if normalize_tool_hitl_timeout_secs(secs).is_ok() => secs,
            _ => DEFAULT_TOOL_HITL_TIMEOUT_SECS,
        })
    }

    /// Persist the operator tool-level HITL timeout. Non-positive values and
    /// values above 30 days are rejected so the previous setting stays.
    pub fn set_tool_hitl_timeout_secs(&self, secs: i64) -> Result<(), OrchestratorError> {
        let secs = normalize_tool_hitl_timeout_secs(secs)?;
        let updated_at = unix_now_secs()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "
            INSERT INTO node_operator_settings (id, allowed_models_json, updated_at, tool_hitl_timeout_secs)
            VALUES (?1, '[]', ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET
                tool_hitl_timeout_secs = excluded.tool_hitl_timeout_secs,
                updated_at = excluded.updated_at
            ",
            params![SETTINGS_ROW_ID, updated_at, secs],
        )?;
        Ok(())
    }

    pub fn list_operator_allowed_models(&self) -> Result<Vec<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let json: Option<String> = connection
            .query_row(
                "SELECT allowed_models_json FROM node_operator_settings WHERE id = ?1",
                params![SETTINGS_ROW_ID],
                |row| row.get(0),
            )
            .optional_not_found()?;
        match json {
            Some(raw) => parse_models_json(&raw),
            None => Ok(Vec::new()),
        }
    }

    pub fn replace_operator_allowed_models(
        &self,
        models: &[String],
    ) -> Result<Vec<String>, OrchestratorError> {
        let normalized = normalize_allowed_models(models)?;
        let json = serde_json::to_string(&normalized)
            .map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let updated_at = unix_now_secs()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "
            INSERT INTO node_operator_settings (id, allowed_models_json, updated_at)
            VALUES (?1, ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET
                allowed_models_json = excluded.allowed_models_json,
                updated_at = excluded.updated_at
            ",
            params![SETTINGS_ROW_ID, json, updated_at],
        )?;
        Ok(normalized)
    }

    /// The operator's node default model, if one is set.
    pub fn get_operator_default_model(&self) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let stored: Option<Option<String>> = connection
            .query_row(
                "SELECT default_model FROM node_operator_settings WHERE id = ?1",
                params![SETTINGS_ROW_ID],
                |row| row.get(0),
            )
            .optional_not_found()?;
        Ok(stored.flatten().filter(|m| !m.trim().is_empty()))
    }

    /// The gateway a slash model id the catalog does not know is sent to: `vercel` unless the
    /// operator set `openrouter`.
    pub fn get_operator_default_gateway(&self) -> Result<String, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let stored: Option<Option<String>> = connection
            .query_row(
                "SELECT default_gateway FROM node_operator_settings WHERE id = ?1",
                params![SETTINGS_ROW_ID],
                |row| row.get(0),
            )
            .optional_not_found()?;
        Ok(stored
            .flatten()
            .filter(|g| validate_gateway(g).is_ok())
            .unwrap_or_else(|| DEFAULT_GATEWAY.to_string()))
    }

    /// Persists the node default gateway (`vercel` or `openrouter`); anything else is refused
    /// and the stored value stays.
    pub fn set_operator_default_gateway(&self, gateway: &str) -> Result<String, OrchestratorError> {
        let gateway = validate_gateway(gateway)?;
        let updated_at = unix_now_secs()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "
            INSERT INTO node_operator_settings (id, allowed_models_json, updated_at, default_gateway)
            VALUES (?1, '[]', ?2, ?3)
            ON CONFLICT(id) DO UPDATE SET
                default_gateway = excluded.default_gateway,
                updated_at = excluded.updated_at
            ",
            params![SETTINGS_ROW_ID, updated_at, gateway],
        )?;
        Ok(gateway)
    }

    /// Replaces the allowlist and, when `default` is `Some`, the node default (`Some("")`
    /// clears it). The pair is validated first: a default must be on a non-empty list, and
    /// the stored default is checked against the new list when `default` is `None`.
    /// Nothing is written when the pair is refused.
    pub fn replace_operator_models(
        &self,
        models: &[String],
        default: Option<&str>,
    ) -> Result<(Vec<String>, Option<String>), OrchestratorError> {
        let normalized = normalize_allowed_models(models)?;
        let wanted = match default {
            Some(d) => Some(d.trim().to_string()).filter(|d| !d.is_empty()),
            None => self.get_operator_default_model()?,
        };
        if let Some(d) = &wanted {
            validate_default_model(d, &normalized)?;
        }
        let json = serde_json::to_string(&normalized)
            .map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let updated_at = unix_now_secs()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "
            INSERT INTO node_operator_settings (id, allowed_models_json, updated_at, default_model)
            VALUES (?1, ?2, ?3, ?4)
            ON CONFLICT(id) DO UPDATE SET
                allowed_models_json = excluded.allowed_models_json,
                default_model = excluded.default_model,
                updated_at = excluded.updated_at
            ",
            params![SETTINGS_ROW_ID, json, updated_at, wanted],
        )?;
        Ok((normalized, wanted))
    }
}

/// Gateway an unknown slash model id goes to when the operator set none.
pub const DEFAULT_GATEWAY: &str = "vercel";

/// The two gateways a node default can name.
pub const GATEWAYS: [&str; 2] = ["vercel", "openrouter"];

/// `vercel` or `openrouter`, trimmed; anything else is refused.
pub fn validate_gateway(gateway: &str) -> Result<String, OrchestratorError> {
    let g = gateway.trim();
    if GATEWAYS.contains(&g) {
        return Ok(g.to_string());
    }
    Err(OrchestratorError::InvalidContract(format!(
        "default gateway '{g}' is not supported (use one of: {})",
        GATEWAYS.join(", ")
    )))
}

/// A node default must be a plain model id and, when the list is non-empty, be on it.
pub fn validate_default_model(
    default: &str,
    allowlist: &[String],
) -> Result<(), OrchestratorError> {
    normalize_allowed_models(&[default.to_string()])?;
    if !allowlist.is_empty() && !allowlist.iter().any(|m| m == default) {
        return Err(OrchestratorError::InvalidContract(format!(
            "default model '{default}' is not on the operator allowlist (allowed: {})",
            allowlist.join(", ")
        )));
    }
    Ok(())
}

trait OptionalNotFound<T> {
    fn optional_not_found(self) -> Result<Option<T>, OrchestratorError>;
}

impl<T> OptionalNotFound<T> for rusqlite::Result<T> {
    fn optional_not_found(self) -> Result<Option<T>, OrchestratorError> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

fn unix_now_secs() -> Result<i64, OrchestratorError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .map_err(|e| OrchestratorError::Core(e.to_string()))
}

fn parse_models_json(raw: &str) -> Result<Vec<String>, OrchestratorError> {
    let parsed: Vec<String> =
        serde_json::from_str(raw).map_err(|e| OrchestratorError::Core(e.to_string()))?;
    Ok(parsed)
}

pub fn normalize_allowed_models(models: &[String]) -> Result<Vec<String>, OrchestratorError> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for model in models {
        let trimmed = model.trim();
        if trimmed.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "allowed model ids must not be empty".into(),
            ));
        }
        if trimmed.len() > MAX_MODEL_LEN {
            return Err(OrchestratorError::InvalidContract(format!(
                "allowed model id exceeds {MAX_MODEL_LEN} characters"
            )));
        }
        if trimmed.chars().any(|c| c.is_control()) {
            return Err(OrchestratorError::InvalidContract(
                "allowed model ids must not contain control characters".into(),
            ));
        }
        if seen.insert(trimmed.to_string()) {
            out.push(trimmed.to_string());
        }
    }
    Ok(out)
}

/// The scripted mock model id. A node answers it only when it loaded scripts from
/// `VAMD_E2E_MOCK_SCRIPTS` (design/e2e-journeys.md D3).
pub const MOCK_MODEL: &str = "mock-model";

/// Refuses `mock-model` on a node without loaded scripts. Every other model passes unchanged.
/// Used both when the operator publishes an allowlist and when a run resolves its model.
pub fn require_scripted_mode_for_model(
    model: &str,
    scripted_mode: bool,
) -> Result<(), OrchestratorError> {
    if !scripted_mode && model.trim() == MOCK_MODEL {
        return Err(OrchestratorError::InvalidContract(
            "mock-model is available only on a node started with VAMD_E2E_MOCK_SCRIPTS".into(),
        ));
    }
    Ok(())
}

impl SqliteWorkflowStore {
    /// The allowlist and the node default a run is resolved against. A process-guest node
    /// serves one model, which is its default unless the operator set another.
    pub fn node_model_settings(&self) -> Result<(Vec<String>, Option<String>), OrchestratorError> {
        let allowlist = self.list_operator_allowed_models()?;
        let default = self.get_operator_default_model()?;
        #[cfg(feature = "process-guest")]
        let default = default.or_else(|| {
            crate::process_mode::is_active().then(|| crate::process_mode::ONLY_MODEL.to_string())
        });
        Ok((allowlist, default))
    }
}

/// The model a run gets when nothing names one: the default, else the first listed model,
/// else the catalog default.
pub fn effective_default_model(
    allowlist: &[String],
    default: Option<&str>,
    catalog_default: &str,
) -> String {
    default
        .filter(|d| !d.trim().is_empty())
        .or_else(|| allowlist.first().map(String::as_str))
        .unwrap_or(catalog_default)
        .to_string()
}

/// The model settings a run is resolved against.
#[derive(Debug, Clone, Default)]
pub struct NodeModels<'a> {
    /// Operator allowlist. Empty is unrestricted.
    pub allowlist: &'a [String],
    /// Operator default model (on a process-guest node: the one model it serves).
    pub default: Option<&'a str>,
    /// The catalog's default, the last resort.
    pub catalog_default: &'a str,
}

/// Why a run got the model it got.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    Caller,
    Preference,
    NodeDefault,
    Allowlist,
    CatalogDefault,
}

impl ModelSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ModelSource::Caller => "caller",
            ModelSource::Preference => "preference",
            ModelSource::NodeDefault => "node-default",
            ModelSource::Allowlist => "allowlist",
            ModelSource::CatalogDefault => "catalog-default",
        }
    }
}

/// The model a run resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedModel {
    pub model: String,
    pub source: ModelSource,
    /// Set when the agent named preferred models and none could be used.
    pub note: Option<String>,
}

/// Pins `inputs.model` on the run spec. A model is a preference, never a gate.
///
/// Order: the caller's `inputs.model`; else the agent's first preferred model that is
/// on the allowlist; else the node default; else the allowlist's first entry; else the
/// catalog default. The only refusal is a caller-named model that is not on a non-empty
/// allowlist. `inputs.model` is always written.
pub fn resolve_run_model(
    spec: &mut RunSpec,
    agent: &AgentSpec,
    node: &NodeModels<'_>,
) -> Result<ResolvedModel, OrchestratorError> {
    let requested = extract_model(spec);
    let requested = requested.trim();
    let resolved = if !requested.is_empty() {
        if !node.allowlist.is_empty() && !node.allowlist.iter().any(|m| m == requested) {
            return Err(OrchestratorError::InvalidContract(format!(
                "model '{requested}' is not on the operator allowlist (allowed: {})",
                node.allowlist.join(", ")
            )));
        }
        ResolvedModel {
            model: requested.to_string(),
            source: ModelSource::Caller,
            note: None,
        }
    } else if let Some(model) = first_allowed_preferred(agent, node.allowlist) {
        ResolvedModel {
            model,
            source: ModelSource::Preference,
            note: None,
        }
    } else {
        let (model, source) = if let Some(d) = node.default.filter(|d| !d.trim().is_empty()) {
            (d.trim().to_string(), ModelSource::NodeDefault)
        } else if let Some(first) = node.allowlist.first() {
            (first.clone(), ModelSource::Allowlist)
        } else {
            (
                node.catalog_default.to_string(),
                ModelSource::CatalogDefault,
            )
        };
        let named_any = agent
            .preferred_models
            .iter()
            .any(|p| !p.model_name.trim().is_empty());
        ResolvedModel {
            note: named_any.then(|| format!("preference not allowed, used {model}")),
            model,
            source,
        }
    };
    set_input_model(spec, &resolved.model);
    // Host-written, never the caller's: whether the caller pinned the model decides if a failing
    // model may fall back (AR-4.8).
    set_input_string(spec, INPUT_MODEL_SOURCE, resolved.source.as_str());
    Ok(resolved)
}

/// `inputs` key the host writes with how the run's model was chosen (`ModelSource::as_str`).
pub const INPUT_MODEL_SOURCE: &str = "model_source";

/// The fallback options of a run (AR-4.8), or `None` when it has none: the switch
/// `reliability.fallback` is off, the caller pinned the model, or the operator has not published
/// an allowlist (nothing to move to). The candidates are the allowlist in its order; the broker
/// skips the failing model and any without a credential. The agent has no way to exclude a model
/// yet, so none is left out here. `serve_as_mock` is for a process-guest test node.
pub fn fallback_plan_for(
    spec: &RunSpec,
    allowlist: &[String],
    serve_as_mock: bool,
) -> Option<vam_inference::FallbackPlan> {
    let harness = vam_spec::harness::Harness::from_inputs(spec.inputs.as_ref()).ok()?;
    if !harness.reliability.fallback || allowlist.is_empty() {
        return None;
    }
    let source = input_string(spec, INPUT_MODEL_SOURCE);
    // No recorded source (an older run) is treated as pinned: it never falls back.
    if source.is_none_or(|s| s == ModelSource::Caller.as_str()) {
        return None;
    }
    Some(vam_inference::FallbackPlan {
        candidates: allowlist.to_vec(),
        serve_as_mock,
    })
}

impl SqliteWorkflowStore {
    /// [`fallback_plan_for`] for the run now bound to `session_id`, read for every request from
    /// the stored spec and the current allowlist. `None` on any failure to read them.
    pub fn fallback_plan_for_session(
        &self,
        session_id: &str,
    ) -> Option<vam_inference::FallbackPlan> {
        let run_id = self.find_run_id_for_session(session_id).ok().flatten()?;
        let (json, _, _) = self.get_run_spec_context(&run_id).ok()?;
        let spec = vam_spec::encoding::json_to_proto::<RunSpec>("vam.v1.RunSpec", &json?).ok()?;
        let (allowlist, _) = self.node_model_settings().ok()?;
        #[cfg(feature = "process-guest")]
        let as_mock = crate::process_mode::is_active();
        #[cfg(not(feature = "process-guest"))]
        let as_mock = false;
        fallback_plan_for(&spec, &allowlist, as_mock)
    }
}

fn input_string(spec: &RunSpec, key: &str) -> Option<String> {
    spec.inputs
        .as_ref()
        .and_then(|i| i.fields.get(key))
        .and_then(|v| match &v.kind {
            Some(prost_types::value::Kind::StringValue(s)) => Some(s.clone()),
            _ => None,
        })
}

fn set_input_string(spec: &mut RunSpec, key: &str, value: &str) {
    let mut inputs = spec.inputs.take().unwrap_or_default();
    inputs.fields.insert(
        key.to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(value.to_string())),
        },
    );
    spec.inputs = Some(inputs);
}

fn first_allowed_preferred(agent: &AgentSpec, allowlist: &[String]) -> Option<String> {
    agent.preferred_models.iter().find_map(|pref| {
        let name = pref.model_name.trim();
        if name.is_empty() {
            return None;
        }
        if allowlist.is_empty() || allowlist.iter().any(|allowed| allowed == name) {
            Some(name.to_string())
        } else {
            None
        }
    })
}

/// The provider a run's model goes to, decided on the host so the guest never guesses.
///
/// Order: the caller's `inputs.model_provider` (must be a provider the catalog knows); the
/// agent's preferred-models entry for this model that names a provider; the catalog (a
/// static entry's provider, else `vercel` when the gateway covers the id); for a slash id
/// the catalog does not know, the node's default gateway; a bare id the catalog does not
/// know is refused. Always writes `inputs.model_provider` and returns it.
pub fn resolve_model_provider(
    spec: &mut RunSpec,
    agent: &AgentSpec,
    model: &str,
    catalog: &vam_inference::ModelCatalog,
    default_gateway: &str,
) -> Result<String, OrchestratorError> {
    let model = model.trim();
    let caller = spec
        .inputs
        .as_ref()
        .and_then(|i| i.fields.get("model_provider"))
        .and_then(|v| match &v.kind {
            Some(prost_types::value::Kind::StringValue(s)) => Some(s.trim().to_string()),
            _ => None,
        })
        .filter(|s| !s.is_empty());
    let provider = if model == MOCK_MODEL {
        "mock".to_string()
    } else if let Some(p) = caller {
        if !catalog.knows_provider(&p) {
            return Err(OrchestratorError::InvalidContract(format!(
                "model_provider '{p}' is not a provider this node knows (known: {})",
                catalog.provider_names().join(", ")
            )));
        }
        p
    } else if let Some(p) = agent
        .preferred_models
        .iter()
        .find(|p| p.model_name.trim() == model && !p.provider.trim().is_empty())
    {
        p.provider.trim().to_string()
    } else if let Some(entry) = catalog.get(model) {
        entry.provider.clone()
    } else if catalog.gateway_entry(model).is_some() {
        "vercel".to_string()
    } else if model.contains('/') {
        default_gateway.to_string()
    } else {
        return Err(OrchestratorError::InvalidContract(format!(
            "model '{model}' is not in the model catalog and has no provider. Name it as \
             provider/model (for example anthropic/claude-sonnet-5.5) so it goes to the \
             node's default gateway, set model_provider on the run, or add it to the catalog"
        )));
    };
    let mut inputs = spec.inputs.take().unwrap_or_default();
    inputs.fields.insert(
        "model_provider".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(provider.clone())),
        },
    );
    spec.inputs = Some(inputs);
    Ok(provider)
}

fn set_input_model(spec: &mut RunSpec, model: &str) {
    if model.is_empty() {
        return;
    }
    let mut inputs = spec.inputs.take().unwrap_or_default();
    inputs.fields.insert(
        "model".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(model.to_string())),
        },
    );
    spec.inputs = Some(inputs);
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::ModelPreference;

    fn run_with(source: Option<&str>, fallback: bool) -> RunSpec {
        let mut spec = RunSpec::default();
        if let Some(source) = source {
            set_input_string(&mut spec, INPUT_MODEL_SOURCE, source);
        }
        let mut overrides = serde_json::json!({});
        if fallback {
            overrides = serde_json::json!({"reliability": {"fallback": true}});
        }
        let mut inputs = spec.inputs.take().unwrap_or_default();
        inputs.fields.insert(
            vam_spec::harness::INPUT_KEY.to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StructValue(
                    vam_spec::harness::json_to_struct(&overrides).unwrap(),
                )),
            },
        );
        spec.inputs = Some(inputs);
        crate::run_context::resolve_harness(&mut spec).unwrap();
        spec
    }

    fn two() -> Vec<String> {
        vec!["a".to_string(), "b".to_string()]
    }

    #[test]
    fn fallback_plan_is_off_by_default_and_when_the_caller_pinned_the_model() {
        // Off by default, whatever else holds.
        assert!(fallback_plan_for(&run_with(Some("node-default"), false), &two(), false).is_none());
        // Pinned by the caller (or unknown, as an older run): never.
        assert!(fallback_plan_for(&run_with(Some("caller"), true), &two(), false).is_none());
        assert!(fallback_plan_for(&run_with(None, true), &two(), false).is_none());
        // No allowlist, no candidates.
        assert!(fallback_plan_for(&run_with(Some("node-default"), true), &[], false).is_none());
    }

    #[test]
    fn fallback_plan_lists_the_allowlist_in_order_for_a_chosen_model() {
        for source in ["preference", "node-default", "allowlist", "catalog-default"] {
            let plan = fallback_plan_for(&run_with(Some(source), true), &two(), true)
                .unwrap_or_else(|| panic!("{source} falls back"));
            assert_eq!(plan.candidates, two());
            assert!(plan.serve_as_mock);
        }
    }

    #[test]
    fn the_resolved_source_is_written_by_the_host_over_a_caller_value() {
        let mut spec = RunSpec::default();
        set_input_string(&mut spec, INPUT_MODEL_SOURCE, "preference");
        set_input_string(&mut spec, "model", "a");
        let node = NodeModels {
            allowlist: &two(),
            default: None,
            catalog_default: "a",
        };
        resolve_run_model(&mut spec, &AgentSpec::default(), &node).unwrap();
        assert_eq!(
            input_string(&spec, INPUT_MODEL_SOURCE).as_deref(),
            Some("caller")
        );
    }

    fn agent_with_preferred(names: &[&str]) -> AgentSpec {
        AgentSpec {
            preferred_models: names
                .iter()
                .map(|n| ModelPreference {
                    model_name: (*n).to_string(),
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        }
    }

    fn spec_with_model(model: Option<&str>) -> RunSpec {
        let mut spec = RunSpec::default();
        if let Some(model) = model {
            set_input_model(&mut spec, model);
        }
        spec
    }

    #[test]
    fn normalize_trims_and_dedups() {
        let models = vec![
            "  openai/gpt-4o  ".into(),
            "openai/gpt-4o".into(),
            "anthropic/claude-sonnet-4".into(),
        ];
        let out = normalize_allowed_models(&models).unwrap();
        assert_eq!(out, vec!["openai/gpt-4o", "anthropic/claude-sonnet-4"]);
    }

    #[test]
    fn normalize_rejects_blank() {
        let err = normalize_allowed_models(&[" ".into()]).unwrap_err();
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    }

    fn node<'a>(allowlist: &'a [String], default: Option<&'a str>) -> NodeModels<'a> {
        NodeModels {
            allowlist,
            default,
            catalog_default: "catalog/default",
        }
    }

    fn list(models: &[&str]) -> Vec<String> {
        models.iter().map(|m| m.to_string()).collect()
    }

    #[test]
    fn empty_allowlist_does_not_reject() {
        let mut spec = spec_with_model(Some("openai/gpt-4o-mini"));
        let r = resolve_run_model(&mut spec, &AgentSpec::default(), &node(&[], None)).unwrap();
        assert_eq!(r.model, "openai/gpt-4o-mini");
        assert_eq!(r.source, ModelSource::Caller);
        assert_eq!(extract_model(&spec), "openai/gpt-4o-mini");
    }

    #[test]
    fn listed_caller_model_wins_over_preference_and_default() {
        let mut spec = spec_with_model(Some("openai/gpt-4o"));
        let allowlist = list(&["a/one", "openai/gpt-4o"]);
        let agent = agent_with_preferred(&["a/one"]);
        let r = resolve_run_model(&mut spec, &agent, &node(&allowlist, Some("a/one"))).unwrap();
        assert_eq!(
            (r.model.as_str(), r.source),
            ("openai/gpt-4o", ModelSource::Caller)
        );
        assert_eq!(r.note, None);
    }

    #[test]
    fn caller_model_off_the_list_is_the_only_refusal_and_names_the_list() {
        let mut spec = spec_with_model(Some("openai/gpt-4o-mini"));
        let allowlist = list(&["openai/gpt-4o", "mock-model"]);
        let err = resolve_run_model(&mut spec, &AgentSpec::default(), &node(&allowlist, None))
            .unwrap_err();
        match err {
            OrchestratorError::InvalidContract(msg) => {
                assert!(msg.contains("openai/gpt-4o-mini"), "{msg}");
                assert!(msg.contains("allowed: openai/gpt-4o, mock-model"), "{msg}");
            }
            other => panic!("expected InvalidContract, got {other:?}"),
        }
    }

    #[test]
    fn first_allowed_preference_is_used_when_the_caller_names_none() {
        let mut spec = spec_with_model(None);
        let agent = agent_with_preferred(&["blocked/model", "openai/gpt-4o", "a/one"]);
        let allowlist = list(&["a/one", "openai/gpt-4o"]);
        let r = resolve_run_model(&mut spec, &agent, &node(&allowlist, Some("a/one"))).unwrap();
        assert_eq!(
            (r.model.as_str(), r.source),
            ("openai/gpt-4o", ModelSource::Preference)
        );
        assert_eq!(r.note, None);
        assert_eq!(extract_model(&spec), "openai/gpt-4o");
    }

    #[test]
    fn preference_on_an_empty_list_is_used() {
        let mut spec = spec_with_model(None);
        let agent = agent_with_preferred(&["any/model"]);
        let r = resolve_run_model(&mut spec, &agent, &node(&[], Some("d/default"))).unwrap();
        assert_eq!(
            (r.model.as_str(), r.source),
            ("any/model", ModelSource::Preference)
        );
    }

    #[test]
    fn preferences_off_the_list_do_not_block_and_are_noted() {
        let mut spec = spec_with_model(None);
        let agent = agent_with_preferred(&["blocked/model"]);
        let allowlist = list(&["a/one", "b/two"]);
        let r = resolve_run_model(&mut spec, &agent, &node(&allowlist, Some("b/two"))).unwrap();
        assert_eq!(
            (r.model.as_str(), r.source),
            ("b/two", ModelSource::NodeDefault)
        );
        assert_eq!(
            r.note.as_deref(),
            Some("preference not allowed, used b/two")
        );
        assert_eq!(extract_model(&spec), "b/two");
    }

    #[test]
    fn node_default_beats_the_first_listed_model() {
        let mut spec = spec_with_model(None);
        let allowlist = list(&["a/one", "b/two"]);
        let r = resolve_run_model(
            &mut spec,
            &AgentSpec::default(),
            &node(&allowlist, Some("b/two")),
        )
        .unwrap();
        assert_eq!(
            (r.model.as_str(), r.source),
            ("b/two", ModelSource::NodeDefault)
        );
        assert_eq!(r.note, None);
    }

    #[test]
    fn first_listed_model_is_used_without_a_default() {
        let mut spec = spec_with_model(None);
        let allowlist = list(&["a/one", "b/two"]);
        let r =
            resolve_run_model(&mut spec, &AgentSpec::default(), &node(&allowlist, None)).unwrap();
        assert_eq!(
            (r.model.as_str(), r.source),
            ("a/one", ModelSource::Allowlist)
        );
    }

    #[test]
    fn catalog_default_is_the_last_resort_and_inputs_model_is_always_written() {
        let mut spec = spec_with_model(None);
        let r = resolve_run_model(&mut spec, &AgentSpec::default(), &node(&[], None)).unwrap();
        assert_eq!(
            (r.model.as_str(), r.source),
            ("catalog/default", ModelSource::CatalogDefault)
        );
        assert_eq!(extract_model(&spec), "catalog/default");
    }

    #[test]
    fn blank_default_is_ignored() {
        let mut spec = spec_with_model(None);
        let allowlist = list(&["a/one"]);
        let r = resolve_run_model(
            &mut spec,
            &AgentSpec::default(),
            &node(&allowlist, Some("  ")),
        )
        .unwrap();
        assert_eq!(r.source, ModelSource::Allowlist);
    }

    #[test]
    fn effective_default_follows_the_same_order() {
        let l = list(&["a/one", "b/two"]);
        assert_eq!(effective_default_model(&l, Some("b/two"), "c/d"), "b/two");
        assert_eq!(effective_default_model(&l, None, "c/d"), "a/one");
        assert_eq!(effective_default_model(&[], None, "c/d"), "c/d");
    }

    #[test]
    fn default_must_be_on_a_non_empty_list() {
        let l = list(&["a/one"]);
        assert!(validate_default_model("a/one", &l).is_ok());
        assert!(validate_default_model("x/y", &[]).is_ok());
        let err = validate_default_model("x/y", &l).unwrap_err();
        assert!(err.to_string().contains("allowed: a/one"), "{err}");
        assert!(validate_default_model(" ", &[]).is_err());
    }

    #[test]
    fn default_gateway_is_vercel_until_set_and_survives_a_model_replace() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_operator_allowed_models_schema().unwrap();
        assert_eq!(store.get_operator_default_gateway().unwrap(), "vercel");
        assert!(store.set_operator_default_gateway("anthropic").is_err());
        assert_eq!(
            store.set_operator_default_gateway("openrouter").unwrap(),
            "openrouter"
        );
        store
            .replace_operator_models(&list(&["a/one"]), None)
            .unwrap();
        assert_eq!(store.get_operator_default_gateway().unwrap(), "openrouter");
    }

    #[test]
    fn stored_default_round_trips_and_is_validated_against_the_list() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_operator_allowed_models_schema().unwrap();
        assert_eq!(store.get_operator_default_model().unwrap(), None);

        let (models, d) = store
            .replace_operator_models(&list(&["a/one", "b/two"]), Some("b/two"))
            .unwrap();
        assert_eq!(
            (models, d.as_deref()),
            (list(&["a/one", "b/two"]), Some("b/two"))
        );
        assert_eq!(
            store.get_operator_default_model().unwrap().as_deref(),
            Some("b/two")
        );

        // Omitted default keeps the stored one while it is still on the list.
        store
            .replace_operator_models(&list(&["b/two", "c/three"]), None)
            .unwrap();
        assert_eq!(
            store.get_operator_default_model().unwrap().as_deref(),
            Some("b/two")
        );

        // A list that drops the stored default is refused and nothing changes.
        assert!(store
            .replace_operator_models(&list(&["c/three"]), None)
            .is_err());
        assert_eq!(
            store.list_operator_allowed_models().unwrap(),
            list(&["b/two", "c/three"])
        );

        // A default off the list is refused; an empty string clears it.
        assert!(store
            .replace_operator_models(&list(&["b/two"]), Some("x/y"))
            .is_err());
        let (_, d) = store
            .replace_operator_models(&list(&["b/two"]), Some(""))
            .unwrap();
        assert_eq!(d, None);
        assert_eq!(store.get_operator_default_model().unwrap(), None);
    }

    #[test]
    fn mock_model_needs_scripted_mode() {
        let err = require_scripted_mode_for_model("mock-model", false).unwrap_err();
        match err {
            OrchestratorError::InvalidContract(msg) => {
                assert!(msg.contains("VAMD_E2E_MOCK_SCRIPTS"), "{msg}");
            }
            other => panic!("expected InvalidContract, got {other:?}"),
        }
        assert!(require_scripted_mode_for_model(" mock-model ", false).is_err());
        assert!(require_scripted_mode_for_model("mock-model", true).is_ok());
    }

    #[test]
    fn other_models_pass_without_scripted_mode() {
        for model in [
            "openai/gpt-4o",
            "anthropic/claude-sonnet-4",
            "mock-model-2",
            "",
        ] {
            assert!(
                require_scripted_mode_for_model(model, false).is_ok(),
                "{model}"
            );
        }
    }

    // ---- provider derivation (the host decides; the guest never guesses) ----

    fn provider_for(
        model: &str,
        caller_provider: Option<&str>,
        agent: &AgentSpec,
        gateway: &str,
    ) -> Result<String, OrchestratorError> {
        let mut spec = spec_with_model(Some(model));
        if let Some(p) = caller_provider {
            let mut inputs = spec.inputs.take().unwrap_or_default();
            inputs.fields.insert(
                "model_provider".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(p.to_string())),
                },
            );
            spec.inputs = Some(inputs);
        }
        let got = resolve_model_provider(
            &mut spec,
            agent,
            model,
            &vam_inference::ModelCatalog::new(),
            gateway,
        )?;
        // Always written, so the guest has nothing to decide.
        let written = match &spec.inputs.as_ref().unwrap().fields["model_provider"].kind {
            Some(prost_types::value::Kind::StringValue(s)) => s.clone(),
            other => panic!("model_provider not a string: {other:?}"),
        };
        assert_eq!(written, got);
        Ok(got)
    }

    #[test]
    fn provider_gateway_id_with_a_maker_entry_is_vercel() {
        let a = AgentSpec::default();
        for m in ["anthropic/claude-sonnet-5.5", "anthropic/claude-haiku-5.5"] {
            assert_eq!(
                provider_for(m, None, &a, "openrouter").unwrap(),
                "vercel",
                "{m}"
            );
        }
    }

    #[test]
    fn provider_static_entry_wins_over_the_gateway() {
        let a = AgentSpec::default();
        assert_eq!(
            provider_for("claude-3-5-sonnet-latest", None, &a, "vercel").unwrap(),
            "anthropic"
        );
        assert_eq!(
            provider_for("mock-model", None, &a, "vercel").unwrap(),
            "mock"
        );
    }

    #[test]
    fn provider_unknown_slash_id_goes_to_the_node_default_gateway() {
        let a = AgentSpec::default();
        assert_eq!(
            provider_for("someone/some-model", None, &a, "vercel").unwrap(),
            "vercel"
        );
        assert_eq!(
            provider_for("someone/some-model", None, &a, "openrouter").unwrap(),
            "openrouter"
        );
    }

    #[test]
    fn provider_unknown_bare_id_is_refused_naming_the_model() {
        let err = provider_for("no-such-model", None, &AgentSpec::default(), "vercel").unwrap_err();
        match err {
            OrchestratorError::InvalidContract(msg) => {
                assert!(msg.contains("no-such-model"), "{msg}");
                assert!(msg.contains("provider/model"), "{msg}");
            }
            other => panic!("expected InvalidContract, got {other:?}"),
        }
    }

    #[test]
    fn provider_preference_beats_the_catalog_and_rescues_a_bare_id() {
        let agent = AgentSpec {
            preferred_models: vec![
                ModelPreference {
                    model_name: "anthropic/claude-sonnet-5.5".into(),
                    provider: "openrouter".into(),
                    ..Default::default()
                },
                ModelPreference {
                    model_name: "bare-custom".into(),
                    provider: "vercel".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        assert_eq!(
            provider_for("anthropic/claude-sonnet-5.5", None, &agent, "vercel").unwrap(),
            "openrouter"
        );
        assert_eq!(
            provider_for("bare-custom", None, &agent, "vercel").unwrap(),
            "vercel"
        );
        // A preference for another model does not apply.
        assert_eq!(
            provider_for("anthropic/claude-haiku-5.5", None, &agent, "openrouter").unwrap(),
            "vercel"
        );
    }

    #[test]
    fn provider_caller_value_wins_when_known_and_is_refused_when_not() {
        let agent = AgentSpec {
            preferred_models: vec![ModelPreference {
                model_name: "anthropic/claude-sonnet-5.5".into(),
                provider: "openrouter".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            provider_for(
                "anthropic/claude-sonnet-5.5",
                Some("vercel"),
                &agent,
                "openrouter"
            )
            .unwrap(),
            "vercel"
        );
        let err = provider_for("a/b", Some("nokey"), &agent, "vercel").unwrap_err();
        assert!(matches!(err, OrchestratorError::InvalidContract(m) if m.contains("nokey")));
    }

    #[test]
    fn gateway_values_are_validated() {
        assert_eq!(validate_gateway(" openrouter ").unwrap(), "openrouter");
        assert_eq!(validate_gateway("vercel").unwrap(), "vercel");
        assert!(validate_gateway("anthropic").is_err());
        assert!(validate_gateway("").is_err());
    }
}
