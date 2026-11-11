//! The `harness` run input: every lever of the agent loop as a named switch
//! (`design/harness.md` section 6.1).
//!
//! A caller may send `inputs.harness` as a partial object of overrides. The node
//! resolves it once, when the run is created, into a host-owned record that
//! replaces the caller's object in the stored `RunSpec.inputs.harness`:
//!
//! ```json
//! {"profile":"base","version":"0.1.0","digest":"sha256:...",
//!  "overrides":{"loop":{"detect":true}},
//!  "switches":{"adapter":{...},"tools":{...},...,"sampling":{...}}}
//! ```
//!
//! `overrides` is absent when nothing differs from the profile. The guest reads
//! `switches`; the receipt carries `profile`, `version`, `digest` and `overrides`.
//!
//! A caller may also send `profile`, the name of a profile in [`PROFILES`]; the
//! overrides are then differences from that profile. No name means `base`: a
//! profile whose evidence is `pending` is never applied implicitly (locked
//! decision 3). An unknown name is an error naming `profile`.
//!
//! Rules (design/harness.md locked decisions 1, 3 and 6):
//! - an unknown key at any depth is an error naming the switch;
//! - a switch whose lever is not built on this version accepts only its default;
//! - `process.*` accepts only `true` (an off switch would loosen a guard or gate);
//! - `sampling.*` is recorded as fixed and cannot be overridden;
//! - a caller cannot send `version`, `digest` or `switches`: they are unknown
//!   keys, so a pre-built record is refused rather than trusted.
//!
//! Built in which wave (H1). A switch listed here is accepted and recorded
//! now; until its wave merges the loop does not act on it:
//!
//! | Switch | Acts from |
//! |---|---|
//! | `adapter.thinking` (`strip`, `keep`, `off`, `catalog`) | wave 2 (built) |
//! | `adapter.toolChoice`, `adapter.effort` | wave 2 (built; sent only when the catalog entry supports them) |
//! | `adapter.resampleOnInvalid` | wave 3 (built; one resample at 0.7, labelled `resample`) |
//! | `tools.examples` | wave 3 (built; one `Example:` line on builtins, `task_complete` and the playbook tools) |
//!
//! The profiles `open-strong` and `open-small` set some of these, so until
//! waves 2 and 3 merge they differ from `base` only in `calls.repairBudget`,
//! `loop.detect`, `context.reciteTodo` and `context.compaction.triggerPercent`.
//!
//! All numbers are integers, so the canonical form (`crate::receipt`) hashes it.
//! `calls.repairBudget` 0 means unlimited (today's behaviour).

use prost_types::{value::Kind, Struct, Value as PbValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

/// The profile a run uses when it names none.
pub const PROFILE_BASE: &str = "base";
/// Second default profile, for large open-weight models (GLM, DeepSeek).
pub const PROFILE_OPEN_STRONG: &str = "open-strong";
/// Third default profile, for small open-weight models (Qwen).
pub const PROFILE_OPEN_SMALL: &str = "open-small";
/// Version of the `base` profile: today's behaviour.
pub const BASE_VERSION: &str = "0.1.0";
/// Version of both open profiles.
pub const OPEN_VERSION: &str = "0.1.0";
/// The run-input key.
pub const INPUT_KEY: &str = "harness";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HarnessError {
    #[error("harness switch '{0}' is unknown")]
    Unknown(String),
    #[error("harness switch '{switch}' must be {expected}")]
    BadValue {
        switch: String,
        expected: &'static str,
    },
    #[error("harness switch 'profile' is unknown: no profile named '{0}' (known: base, open-strong, open-small)")]
    UnknownProfile(String),
    #[error("harness switch '{switch}' must be one of {allowed}")]
    OneOf {
        switch: String,
        allowed: &'static str,
    },
    #[error("harness switch '{0}' is not built on this version; only its default is accepted")]
    NotBuilt(String),
    #[error("harness switch '{switch}' must be between {lo} and {hi}")]
    OutOfRange { switch: String, lo: u32, hi: u32 },
    #[error("harness switch '{switch}' accepts only '{only}' on this version")]
    OnlyValue { switch: String, only: &'static str },
    #[error("harness switch '{0}' accepts only true: turning it off would loosen a guard, gate or check")]
    OnlyTrue(String),
    #[error("harness switch '{0}' is fixed on this version and cannot be overridden")]
    Fixed(String),
    #[error("harness input must be an object")]
    NotObject,
    #[error("harness record is not valid: {0}")]
    Record(String),
    #[error("harness canonical form: {0}")]
    Canon(String),
}

/// Every switch, with today's values as the default (profile `base` 0.1.0).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Harness {
    pub adapter: Adapter,
    pub tools: Tools,
    pub calls: Calls,
    pub context: Context,
    #[serde(rename = "loop")]
    pub loop_: Loop,
    pub process: Process,
    pub reliability: Reliability,
    pub human: Human,
    /// Recorded as fixed (Q9): `agent.rs` sends temperature 0.1 and 4096 output tokens.
    pub sampling: Sampling,
}

/// L1, the model boundary. `parser`, `strict` and `guided` are not built and
/// accept only their default. `thinking`, `toolChoice`, `resampleOnInvalid`
/// and `effort` are accepted and recorded; the inference broker acts on thinking,
/// tool choice and effort (wave 2), the guest loop on resample (wave 3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Adapter {
    pub parser: String,
    pub thinking: String,
    pub tool_choice: String,
    pub strict: bool,
    pub guided: bool,
    pub resample_on_invalid: u32,
    pub effort: String,
}

impl Default for Adapter {
    fn default() -> Self {
        Self {
            parser: "native".into(),
            thinking: "drop".into(),
            tool_choice: "auto".into(),
            strict: false,
            guided: false,
            resample_on_invalid: 0,
            effort: "default".into(),
        }
    }
}

/// L2, the tool surface. `examples` is accepted and recorded; it acts (wave 3).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Tools {
    pub rank: bool,
    pub deferred: bool,
    pub mask: bool,
    pub examples: bool,
}

/// L3, call hygiene.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Calls {
    /// Validate the model's tool arguments in the guest (default on, Q2).
    pub validate: bool,
    /// Invalid calls in a row to one tool before the task fails; 0 is unlimited.
    pub repair_budget: u32,
    pub parallel: bool,
    pub verify_reads: bool,
    /// Largest text tool result kept inline, in bytes (`agent.rs` `MAX_TEXT_RESULT_NO_ARTIFACT`).
    pub result_cap: u32,
}

impl Default for Calls {
    fn default() -> Self {
        Self {
            validate: true,
            repair_budget: 0,
            parallel: false,
            verify_reads: false,
            result_cap: 16 * 1024,
        }
    }
}

/// L4, context.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Context {
    pub prompt_variant: String,
    pub compaction: Compaction,
    pub clearing: Clearing,
    pub file_memory: bool,
    pub recite_todo: bool,
    pub reminders: bool,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            prompt_variant: "v11".into(),
            compaction: Compaction::default(),
            clearing: Clearing::default(),
            file_memory: false,
            recite_todo: false,
            reminders: true,
        }
    }
}

/// Summary compaction (`compaction.rs` `TRIGGER_PERCENT`, `KEEP_ROUNDS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Compaction {
    pub trigger_percent: u32,
    pub keep_rounds: u32,
}

impl Default for Compaction {
    fn default() -> Self {
        Self {
            trigger_percent: 70,
            keep_rounds: 3,
        }
    }
}

/// Clearing of old tool results (`context_window.rs` `CLEAR_EVERY_ROUNDS`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Clearing {
    pub every_rounds: u32,
}

impl Default for Clearing {
    fn default() -> Self {
        Self { every_rounds: 8 }
    }
}

/// L5, the control loop. Thresholds are Q3's defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Loop {
    pub detect: bool,
    /// Identical calls in a row.
    pub identical_calls: u32,
    /// A-B alternation cycles.
    pub cycles: u32,
    /// Identical error results in a row.
    pub identical_errors: u32,
    pub plan_first: bool,
    pub verify_before_finish: bool,
    pub escalate: bool,
    pub reset_at_tokens: u32,
    pub helper: String,
}

impl Default for Loop {
    fn default() -> Self {
        Self {
            detect: false,
            identical_calls: 5,
            cycles: 3,
            identical_errors: 3,
            plan_first: false,
            verify_before_finish: false,
            escalate: false,
            reset_at_tokens: 0,
            helper: "off".into(),
        }
    }
}

/// L6, what the machine holds. Only `true` is accepted (Q4, decision 6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Process {
    pub playbook: bool,
    pub guards: bool,
    pub gates: bool,
    pub evidence: bool,
    pub exit_check: bool,
    pub render_variant: String,
}

impl Default for Process {
    fn default() -> Self {
        Self {
            playbook: true,
            guards: true,
            gates: true,
            evidence: true,
            exit_check: true,
            render_variant: "base".into(),
        }
    }
}

/// L7, reliability. `journal` is fixed `true` from this version: every outside call is journaled
/// before it is sent (a correctness rule, not a lever). `fallback` (default off): after the provider's
/// own retries fail, one switch to another model the operator allows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Reliability {
    pub journal: bool,
    pub fallback: bool,
}

impl Default for Reliability {
    fn default() -> Self {
        Self {
            journal: true,
            fallback: false,
        }
    }
}

/// L8, the human loop. `jig` is built (AR-3.9: it offers `show`); `askPolicy` is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Human {
    pub ask_policy: String,
    pub jig: bool,
}

impl Default for Human {
    fn default() -> Self {
        Self {
            ask_policy: "gate".into(),
            jig: false,
        }
    }
}

/// Sampling, recorded and fixed (Q9). Temperature is in thousandths so the
/// canonical form needs no float.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields, default)]
pub struct Sampling {
    pub temperature_milli: u32,
    pub max_output_tokens: u32,
}

impl Default for Sampling {
    fn default() -> Self {
        Self {
            temperature_milli: 100,
            max_output_tokens: 4096,
        }
    }
}

/// How a switch treats a value other than its default.
#[derive(Clone, Copy)]
enum Rule {
    /// Any well-typed value; the lever is built in this phase (waves 2-3).
    Free,
    /// The lever does not exist yet.
    NotBuilt,
    /// An integer lever with inclusive bounds.
    Range(u32, u32),
    /// A string lever with a closed set of values (the text lists them).
    OneOf(&'static [&'static str], &'static str),
    /// A variant lever with a single value until H2 adds variants.
    Only(&'static str),
    /// `process.*`: only `true`.
    OnlyTrue,
    /// Recorded, not settable.
    Fixed,
}

fn rule_for(path: &str) -> Rule {
    match path {
        "calls.validate"
        | "calls.repairBudget"
        | "loop.detect"
        | "loop.identicalCalls"
        | "loop.cycles"
        | "loop.identicalErrors"
        | "context.reciteTodo"
        | "context.reminders"
        // Built in AR-3.9: with it on, the agent is offered `show`.
        | "human.jig"
        // Built in AR-4.7: one verify read after a confirmed non-idempotent call that declares one.
        | "calls.verifyReads"
        // Built in AR-4.8: one fallback to another allowlisted model after the provider's retries fail.
        | "reliability.fallback" => Rule::Free,
        "tools.examples" => Rule::Free,
        "adapter.thinking" => Rule::OneOf(
            &["drop", "strip", "keep", "off", "catalog"],
            "drop, strip, keep, off or catalog",
        ),
        "adapter.toolChoice" => {
            Rule::OneOf(&["auto", "required", "none"], "auto, required or none")
        }
        "adapter.effort" => Rule::OneOf(
            &["default", "low", "medium", "high"],
            "default, low, medium or high",
        ),
        "adapter.resampleOnInvalid" => Rule::Range(0, 1),
        "calls.resultCap" => Rule::Range(1024, 16 * 1024),
        // Ceiling: a round may add `ROUND_RESULTS_PERCENT` (15) of the window plus a
        // 4,096-token reply (~6%), so a trigger above 75 can overflow the summary request.
        "context.compaction.triggerPercent" => Rule::Range(10, 75),
        "context.compaction.keepRounds" => Rule::Range(1, 50),
        "context.clearing.everyRounds" => Rule::Range(1, 200),
        "context.promptVariant" => Rule::Only("v11"),
        "process.renderVariant" => Rule::Only("base"),
        p if p.starts_with("process.") => Rule::OnlyTrue,
        p if p.starts_with("sampling.") => Rule::Fixed,
        // The effect journal is always on (locked decision 11): recorded, not settable.
        "reliability.journal" => Rule::Fixed,
        _ => Rule::NotBuilt,
    }
}

/// A named, versioned set of switches, as data (open question 3).
#[derive(Debug, Clone, Copy)]
pub struct Profile {
    pub name: &'static str,
    pub version: &'static str,
    /// `pending` until a benchmark row backs the profile (locked decision 3).
    pub evidence: &'static str,
    build: fn() -> Harness,
}

impl Profile {
    /// Every switch of the profile, written out.
    pub fn harness(&self) -> Harness {
        (self.build)()
    }
}

/// `base` 0.1.0: today's behaviour. The digest of this profile is pinned by test.
fn profile_base() -> Harness {
    Harness::default()
}

/// `open-strong` 0.1.0: `base` plus the open-model levers (Q4).
fn profile_open_strong() -> Harness {
    let mut h = Harness::default();
    h.adapter.thinking = "catalog".into();
    h.adapter.resample_on_invalid = 1;
    h.tools.examples = true;
    h.calls.repair_budget = 3;
    h.loop_.detect = true;
    h.context.recite_todo = true;
    h
}

/// `open-small` 0.1.0: `open-strong` with a smaller repair budget and an earlier compaction.
fn profile_open_small() -> Harness {
    let mut h = profile_open_strong();
    h.calls.repair_budget = 2;
    h.context.compaction.trigger_percent = 60;
    h
}

/// The profiles a run may name. Only `base` is applied when none is named.
pub const PROFILES: &[Profile] = &[
    Profile {
        name: PROFILE_BASE,
        version: BASE_VERSION,
        evidence: "baseline",
        build: profile_base,
    },
    Profile {
        name: PROFILE_OPEN_STRONG,
        version: OPEN_VERSION,
        evidence: "pending",
        build: profile_open_strong,
    },
    Profile {
        name: PROFILE_OPEN_SMALL,
        version: OPEN_VERSION,
        evidence: "pending",
        build: profile_open_small,
    },
];

/// The profile called `name`.
pub fn profile(name: &str) -> Option<&'static Profile> {
    PROFILES.iter().find(|p| p.name == name)
}

/// A resolved harness: the effective switches and what the caller changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    /// The profile the overrides apply to.
    pub profile: &'static str,
    pub version: &'static str,
    pub harness: Harness,
    /// Leaves that differ from the profile, nested like the switches; empty when none.
    pub overrides: Map<String, Value>,
}

impl Harness {
    /// Applies the caller's partial object to the profile it names (`profile`),
    /// or to `base` when it names none.
    pub fn resolve(overrides: &Value) -> Result<Resolved, HarnessError> {
        let Value::Object(given) = overrides else {
            return Err(HarnessError::NotObject);
        };
        let mut given = given.clone();
        let chosen = match given.remove("profile") {
            None => PROFILES[0],
            Some(Value::String(name)) => {
                *profile(&name).ok_or(HarnessError::UnknownProfile(name))?
            }
            Some(_) => {
                return Err(HarnessError::BadValue {
                    switch: "profile".into(),
                    expected: "a string",
                })
            }
        };
        let given = &given;
        let base = serde_json::to_value(chosen.harness())
            .map_err(|e| HarnessError::Record(e.to_string()))?;
        let mut merged = base.clone();
        let mut changed = Map::new();
        walk(given, &base, &mut merged, &mut changed, "")?;
        let harness: Harness =
            serde_json::from_value(merged).map_err(|e| HarnessError::Record(e.to_string()))?;
        Ok(Resolved {
            profile: chosen.name,
            version: chosen.version,
            harness,
            overrides: changed,
        })
    }

    /// `sha256:<hex>` of the canonical form of `{profile, version, switches}`.
    pub fn digest(&self, profile: &str, version: &str) -> Result<String, HarnessError> {
        let value = serde_json::json!({
            "profile": profile,
            "version": version,
            "switches": self,
        });
        let bytes = crate::receipt::canonical_bytes(&value)
            .map_err(|e| HarnessError::Canon(e.to_string()))?;
        Ok(crate::receipt::digest(&bytes))
    }

    /// The `switches` tree as JSON.
    pub fn to_value(&self) -> Result<Value, HarnessError> {
        serde_json::to_value(self).map_err(|e| HarnessError::Record(e.to_string()))
    }

    /// The host record stored in `inputs.harness`.
    pub fn record(resolved: &Resolved) -> Result<Value, HarnessError> {
        let mut rec = Map::new();
        rec.insert("profile".into(), Value::String(resolved.profile.into()));
        rec.insert("version".into(), Value::String(resolved.version.into()));
        rec.insert(
            "digest".into(),
            Value::String(
                resolved
                    .harness
                    .digest(resolved.profile, resolved.version)?,
            ),
        );
        if !resolved.overrides.is_empty() {
            rec.insert(
                "overrides".into(),
                Value::Object(resolved.overrides.clone()),
            );
        }
        rec.insert("switches".into(), resolved.harness.to_value()?);
        Ok(Value::Object(rec))
    }

    /// Reads the effective switches from a run's `inputs`. Missing means `Default`.
    /// A present `harness` must be the host record; anything else is an error.
    pub fn from_inputs(inputs: Option<&Struct>) -> Result<Harness, HarnessError> {
        let Some(field) = inputs.and_then(|s| s.fields.get(INPUT_KEY)) else {
            return Ok(Harness::default());
        };
        let Some(Kind::StructValue(rec)) = field.kind.as_ref() else {
            return Err(HarnessError::Record("harness is not an object".into()));
        };
        let value = struct_to_json(rec);
        let switches = value
            .get("switches")
            .ok_or_else(|| HarnessError::Record("harness has no switches".into()))?;
        serde_json::from_value(switches.clone()).map_err(|e| HarnessError::Record(e.to_string()))
    }
}

fn walk(
    given: &Map<String, Value>,
    base: &Value,
    merged: &mut Value,
    changed: &mut Map<String, Value>,
    prefix: &str,
) -> Result<(), HarnessError> {
    for (key, value) in given {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let Some(default) = base.get(key) else {
            return Err(HarnessError::Unknown(path));
        };
        if default.is_object() {
            let Value::Object(nested) = value else {
                return Err(HarnessError::BadValue {
                    switch: path,
                    expected: "an object",
                });
            };
            let mut nested_changed = Map::new();
            if let Some(slot) = merged.get_mut(key) {
                walk(nested, default, slot, &mut nested_changed, &path)?;
            }
            if !nested_changed.is_empty() {
                changed.insert(key.clone(), Value::Object(nested_changed));
            }
            continue;
        }
        check_type(&path, default, value)?;
        if value == default {
            continue;
        }
        match rule_for(&path) {
            Rule::Free => {}
            Rule::NotBuilt => return Err(HarnessError::NotBuilt(path)),
            Rule::Range(lo, hi) => {
                let n = value.as_u64().unwrap_or(0);
                if n < u64::from(lo) || n > u64::from(hi) {
                    return Err(HarnessError::OutOfRange {
                        switch: path,
                        lo,
                        hi,
                    });
                }
            }
            Rule::OneOf(values, text) => {
                if !value.as_str().is_some_and(|v| values.contains(&v)) {
                    return Err(HarnessError::OneOf {
                        switch: path,
                        allowed: text,
                    });
                }
            }
            Rule::Only(only) => {
                return Err(HarnessError::OnlyValue { switch: path, only });
            }
            Rule::OnlyTrue => return Err(HarnessError::OnlyTrue(path)),
            Rule::Fixed => return Err(HarnessError::Fixed(path)),
        }
        if let Some(slot) = merged.get_mut(key) {
            *slot = value.clone();
        }
        changed.insert(key.clone(), value.clone());
    }
    Ok(())
}

fn check_type(path: &str, default: &Value, value: &Value) -> Result<(), HarnessError> {
    let bad = |expected| HarnessError::BadValue {
        switch: path.to_string(),
        expected,
    };
    match default {
        Value::Bool(_) if value.is_boolean() => Ok(()),
        Value::Bool(_) => Err(bad("true or false")),
        Value::Number(_) => match value.as_u64() {
            Some(n) if n <= u64::from(u32::MAX) => Ok(()),
            _ => Err(bad("a non-negative integer")),
        },
        Value::String(_) if value.is_string() => Ok(()),
        Value::String(_) => Err(bad("a string")),
        _ => Err(bad("a supported value")),
    }
}

/// A prost `Struct` as JSON. Numbers are f64 there; a whole number becomes an
/// integer so the canonical form (integers only) accepts it.
pub fn struct_to_json(s: &Struct) -> Value {
    Value::Object(
        s.fields
            .iter()
            .map(|(k, v)| (k.clone(), pb_to_json(v)))
            .collect(),
    )
}

fn pb_to_json(v: &PbValue) -> Value {
    match v.kind.as_ref() {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::BoolValue(b)) => Value::Bool(*b),
        Some(Kind::StringValue(s)) => Value::String(s.clone()),
        Some(Kind::NumberValue(n)) => {
            if n.fract() == 0.0 && n.abs() < 9.0e15 {
                Value::from(*n as i64)
            } else {
                serde_json::Number::from_f64(*n)
                    .map(Value::Number)
                    .unwrap_or(Value::Null)
            }
        }
        Some(Kind::StructValue(s)) => struct_to_json(s),
        Some(Kind::ListValue(l)) => Value::Array(l.values.iter().map(pb_to_json).collect()),
    }
}

/// JSON as a prost `Struct`; `None` unless `v` is an object.
pub fn json_to_struct(v: &Value) -> Option<Struct> {
    let Value::Object(map) = v else { return None };
    Some(Struct {
        fields: map
            .iter()
            .map(|(k, v)| (k.clone(), json_to_pb(v)))
            .collect(),
    })
}

fn json_to_pb(v: &Value) -> PbValue {
    let kind = match v {
        Value::Null => Kind::NullValue(0),
        Value::Bool(b) => Kind::BoolValue(*b),
        Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        Value::String(s) => Kind::StringValue(s.clone()),
        Value::Array(a) => Kind::ListValue(prost_types::ListValue {
            values: a.iter().map(json_to_pb).collect(),
        }),
        Value::Object(_) => match json_to_struct(v) {
            Some(s) => Kind::StructValue(s),
            None => Kind::NullValue(0),
        },
    };
    PbValue { kind: Some(kind) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resolve(v: Value) -> Result<Resolved, HarnessError> {
        Harness::resolve(&v)
    }

    /// Today's constants, written out. H-0.7 extends this as the loop reads them.
    #[test]
    fn default_is_todays_behaviour() {
        let h = Harness::default();
        assert!(h.calls.validate);
        assert_eq!(h.calls.repair_budget, 0);
        assert_eq!(h.calls.result_cap, 16 * 1024);
        assert_eq!(h.context.prompt_variant, "v11");
        assert_eq!(h.context.compaction.trigger_percent, 70);
        assert_eq!(h.context.compaction.keep_rounds, 3);
        assert_eq!(h.context.clearing.every_rounds, 8);
        assert!(h.context.reminders);
        assert!(!h.context.recite_todo);
        assert!(!h.loop_.detect);
        assert_eq!(
            (
                h.loop_.identical_calls,
                h.loop_.cycles,
                h.loop_.identical_errors
            ),
            (5, 3, 3)
        );
        assert!(h.process.playbook && h.process.guards && h.process.gates);
        assert!(h.process.evidence && h.process.exit_check);
        assert_eq!(h.sampling.temperature_milli, 100);
        assert_eq!(h.sampling.max_output_tokens, 4096);
        assert!(!h.tools.rank && h.reliability.journal && !h.human.jig);
    }

    #[test]
    fn no_overrides_resolve_to_default_with_none_recorded() {
        let r = resolve(json!({})).unwrap();
        assert_eq!(r.harness, Harness::default());
        assert!(r.overrides.is_empty());
        let rec = Harness::record(&r).unwrap();
        assert_eq!(rec["profile"], "base");
        assert_eq!(rec["version"], "0.1.0");
        assert!(rec.get("overrides").is_none());
        assert!(rec["digest"].as_str().unwrap().starts_with("sha256:"));
    }

    #[test]
    fn an_override_is_applied_and_recorded() {
        let r = resolve(json!({"loop": {"detect": true}})).unwrap();
        assert!(r.harness.loop_.detect);
        let rec = Harness::record(&r).unwrap();
        assert_eq!(rec["overrides"], json!({"loop": {"detect": true}}));
        assert_eq!(rec["switches"]["loop"]["detect"], true);
    }

    #[test]
    fn a_default_value_is_not_an_override() {
        let r = resolve(json!({"loop": {"detect": false}, "calls": {"validate": true}})).unwrap();
        assert!(r.overrides.is_empty());
    }

    #[test]
    fn record_round_trips_through_inputs() {
        let r = resolve(json!({"calls": {"repairBudget": 2}, "context": {"reciteTodo": true}}))
            .unwrap();
        let rec = Harness::record(&r).unwrap();
        let inputs = Struct {
            fields: [(INPUT_KEY.to_string(), json_to_pb(&rec))].into(),
        };
        let back = Harness::from_inputs(Some(&inputs)).unwrap();
        assert_eq!(back, r.harness);
        assert_eq!(back.calls.repair_budget, 2);
    }

    #[test]
    fn missing_input_means_default() {
        assert_eq!(Harness::from_inputs(None).unwrap(), Harness::default());
        assert_eq!(
            Harness::from_inputs(Some(&Struct::default())).unwrap(),
            Harness::default()
        );
    }

    #[test]
    fn a_harness_input_that_is_not_the_host_record_is_an_error() {
        let inputs = Struct {
            fields: [(
                INPUT_KEY.to_string(),
                json_to_pb(&json!({"loop": {"detect": true}})),
            )]
            .into(),
        };
        assert!(matches!(
            Harness::from_inputs(Some(&inputs)),
            Err(HarnessError::Record(_))
        ));
    }

    #[test]
    fn unknown_switches_are_refused_by_path() {
        assert_eq!(
            resolve(json!({"loop": {"nope": 1}})).unwrap_err(),
            HarnessError::Unknown("loop.nope".into())
        );
        assert_eq!(
            resolve(json!({"nope": {}})).unwrap_err(),
            HarnessError::Unknown("nope".into())
        );
    }

    #[test]
    fn a_pre_built_record_is_refused() {
        for key in ["switches", "digest", "version"] {
            let err = resolve(json!({ key: "x" })).unwrap_err();
            assert_eq!(err, HarnessError::Unknown(key.into()), "{key}");
        }
    }

    #[test]
    fn wrong_types_name_the_switch() {
        assert!(matches!(
            resolve(json!({"loop": {"detect": "yes"}})),
            Err(HarnessError::BadValue { switch, .. }) if switch == "loop.detect"
        ));
        assert!(matches!(
            resolve(json!({"loop": {"cycles": -1}})),
            Err(HarnessError::BadValue { .. })
        ));
        assert!(matches!(
            resolve(json!({"loop": {"cycles": 2.5}})),
            Err(HarnessError::BadValue { .. })
        ));
        assert!(matches!(
            resolve(json!({"loop": true})),
            Err(HarnessError::BadValue { .. })
        ));
        assert_eq!(resolve(json!(5)).unwrap_err(), HarnessError::NotObject);
    }

    #[test]
    fn verify_reads_is_a_free_switch_and_off_by_default() {
        assert!(!Harness::default().calls.verify_reads);
        let on = resolve(json!({"calls": {"verifyReads": true}})).unwrap();
        assert!(on.harness.calls.verify_reads);
    }

    #[test]
    fn levers_not_built_accept_only_their_default() {
        for (v, name) in [
            (json!({"tools": {"rank": true}}), "tools.rank"),
            (json!({"tools": {"deferred": true}}), "tools.deferred"),
            (json!({"tools": {"mask": true}}), "tools.mask"),
            (json!({"calls": {"parallel": true}}), "calls.parallel"),
            (
                json!({"context": {"fileMemory": true}}),
                "context.fileMemory",
            ),
            (json!({"loop": {"planFirst": true}}), "loop.planFirst"),
            (
                json!({"loop": {"verifyBeforeFinish": true}}),
                "loop.verifyBeforeFinish",
            ),
            (json!({"loop": {"escalate": true}}), "loop.escalate"),
            (json!({"loop": {"resetAtTokens": 9}}), "loop.resetAtTokens"),
            (json!({"loop": {"helper": "review"}}), "loop.helper"),
            (json!({"human": {"askPolicy": "early"}}), "human.askPolicy"),
            (json!({"adapter": {"parser": "hermes"}}), "adapter.parser"),
            (json!({"adapter": {"strict": true}}), "adapter.strict"),
            (json!({"adapter": {"guided": true}}), "adapter.guided"),
        ] {
            assert_eq!(resolve(v).unwrap_err(), HarnessError::NotBuilt(name.into()));
        }
        let msg = HarnessError::NotBuilt("tools.rank".into()).to_string();
        assert!(msg.contains("tools.rank") && msg.contains("not built"));
        // The default is accepted.
        assert!(resolve(json!({"tools": {"rank": false}}))
            .unwrap()
            .overrides
            .is_empty());
    }

    #[test]
    fn human_jig_is_a_free_switch_and_ask_policy_is_not_built() {
        let on = resolve(json!({"human": {"jig": true}})).unwrap();
        assert!(on.harness.human.jig);
        assert_eq!(on.overrides["human"]["jig"], json!(true));
        assert!(
            on.harness.digest(PROFILE_BASE, BASE_VERSION).unwrap()
                != Harness::default()
                    .digest(PROFILE_BASE, BASE_VERSION)
                    .unwrap()
        );
        // The default and an explicit false change nothing.
        assert!(resolve(json!({"human": {"jig": false}}))
            .unwrap()
            .overrides
            .is_empty());
        assert_eq!(
            resolve(json!({"human": {"jig": "yes"}})).unwrap_err(),
            HarnessError::BadValue {
                switch: "human.jig".into(),
                expected: "true or false"
            }
        );
        assert_eq!(
            resolve(json!({"human": {"askPolicy": "early"}})).unwrap_err(),
            HarnessError::NotBuilt("human.askPolicy".into())
        );
    }

    #[test]
    fn present_levers_accept_their_range_and_refuse_outside_it() {
        let cases: [(&str, [&str; 2], u32, u32); 4] = [
            ("calls.resultCap", ["calls", "resultCap"], 1024, 16 * 1024),
            (
                "context.compaction.triggerPercent",
                ["compaction", "triggerPercent"],
                10,
                75,
            ),
            (
                "context.compaction.keepRounds",
                ["compaction", "keepRounds"],
                1,
                50,
            ),
            (
                "context.clearing.everyRounds",
                ["clearing", "everyRounds"],
                1,
                200,
            ),
        ];
        for (name, path, lo, hi) in cases {
            let build = |n: u32| {
                if path[0] == "calls" {
                    json!({"calls": {path[1]: n}})
                } else {
                    json!({"context": {path[0]: {path[1]: n}}})
                }
            };
            assert!(
                resolve(build(lo)).is_ok() && resolve(build(hi)).is_ok(),
                "{name}"
            );
            for bad in [lo - 1, hi + 1] {
                assert_eq!(
                    resolve(build(bad)).unwrap_err(),
                    HarnessError::OutOfRange {
                        switch: name.into(),
                        lo,
                        hi
                    }
                );
            }
        }
        let r = resolve(json!({"context": {"reminders": false}})).unwrap();
        assert!(!r.harness.context.reminders);
    }

    #[test]
    fn variant_levers_accept_only_todays_value() {
        for (v, name, only) in [
            (
                json!({"context": {"promptVariant": "v12"}}),
                "context.promptVariant",
                "v11",
            ),
            (
                json!({"process": {"renderVariant": "x"}}),
                "process.renderVariant",
                "base",
            ),
        ] {
            assert_eq!(
                resolve(v).unwrap_err(),
                HarnessError::OnlyValue {
                    switch: name.into(),
                    only
                }
            );
        }
    }

    #[test]
    fn process_switches_accept_only_true() {
        for k in ["playbook", "guards", "gates", "evidence", "exitCheck"] {
            assert_eq!(
                resolve(json!({"process": {k: false}})).unwrap_err(),
                HarnessError::OnlyTrue(format!("process.{k}"))
            );
            assert!(resolve(json!({"process": {k: true}}))
                .unwrap()
                .overrides
                .is_empty());
        }
    }

    #[test]
    fn sampling_is_fixed() {
        assert_eq!(
            resolve(json!({"sampling": {"temperatureMilli": 700}})).unwrap_err(),
            HarnessError::Fixed("sampling.temperatureMilli".into())
        );
    }

    #[test]
    fn the_journal_is_always_on_and_fallback_is_a_switch_off_by_default() {
        let r = resolve(json!({})).unwrap();
        assert!(r.harness.reliability.journal);
        assert_eq!(
            Harness::record(&r).unwrap()["switches"]["reliability"]["journal"],
            true
        );
        // Naming the fixed value is accepted and recorded as no override.
        assert!(resolve(json!({"reliability": {"journal": true}}))
            .unwrap()
            .overrides
            .is_empty());
        assert_eq!(
            resolve(json!({"reliability": {"journal": false}})).unwrap_err(),
            HarnessError::Fixed("reliability.journal".into())
        );
        assert!(!r.harness.reliability.fallback);
        let on = resolve(json!({"reliability": {"fallback": true}})).unwrap();
        assert!(on.harness.reliability.fallback);
        assert_eq!(on.overrides.len(), 1);
    }

    #[test]
    fn this_phase_levers_take_any_well_typed_value() {
        let r = resolve(json!({
            "calls": {"validate": false, "repairBudget": 3},
            "loop": {"detect": true, "identicalCalls": 7, "cycles": 4, "identicalErrors": 2},
            "context": {"reciteTodo": true}
        }))
        .unwrap();
        assert!(!r.harness.calls.validate);
        assert_eq!(r.harness.loop_.identical_calls, 7);
        assert_eq!(r.overrides.len(), 3);
    }

    #[test]
    fn digest_is_stable_and_follows_the_switches() {
        let d = |h: &Harness| h.digest(PROFILE_BASE, BASE_VERSION).unwrap();
        let a = d(&Harness::default());
        assert_eq!(a, d(&Harness::default()));
        let b = d(&resolve(json!({"loop": {"detect": true}})).unwrap().harness);
        assert_ne!(a, b);
        assert_eq!(a.len(), "sha256:".len() + 64);
    }

    #[test]
    fn the_canonical_form_has_no_float() {
        // `digest` fails on a float, so success is the proof; also check the tree.
        fn no_float(v: &Value) -> bool {
            match v {
                Value::Number(n) => n.is_i64() || n.is_u64(),
                Value::Array(a) => a.iter().all(no_float),
                Value::Object(o) => o.values().all(no_float),
                _ => true,
            }
        }
        assert!(no_float(&Harness::default().to_value().unwrap()));
    }

    #[test]
    fn struct_numbers_come_back_as_integers() {
        let s = json_to_struct(&json!({"a": {"n": 5}})).unwrap();
        assert_eq!(struct_to_json(&s), json!({"a": {"n": 5}}));
        assert!(struct_to_json(&s)["a"]["n"].is_i64());
    }

    #[test]
    fn base_digest_is_the_published_one() {
        // Changed when `reliability.journal` became fixed `true` (the effect journal is always on);
        // runs recorded before that keep the digest they ran under.
        let r = resolve(json!({})).unwrap();
        let rec = Harness::record(&r).unwrap();
        assert_eq!(
            rec["digest"],
            "sha256:e657c4d4831c72db5721b7406940a183c0b849b0efc8ddd5265bd63b4b682430"
        );
        let named = resolve(json!({"profile": "base"})).unwrap();
        assert_eq!(Harness::record(&named).unwrap()["digest"], rec["digest"]);
    }

    #[test]
    fn a_named_profile_is_recorded_with_its_own_digest() {
        let base = Harness::record(&resolve(json!({})).unwrap()).unwrap();
        let mut digests = vec![base["digest"].to_string()];
        for name in ["open-strong", "open-small"] {
            let r = resolve(json!({"profile": name})).unwrap();
            assert!(r.overrides.is_empty(), "{name}");
            let rec = Harness::record(&r).unwrap();
            assert_eq!(rec["profile"], name);
            assert_eq!(rec["version"], "0.1.0");
            assert!(rec.get("overrides").is_none());
            digests.push(rec["digest"].to_string());
        }
        digests.sort();
        digests.dedup();
        assert_eq!(digests.len(), 3);
    }

    #[test]
    fn overrides_are_differences_from_the_named_profile() {
        let r = resolve(json!({"profile": "open-small", "loop": {"detect": false}})).unwrap();
        assert!(!r.harness.loop_.detect);
        let rec = Harness::record(&r).unwrap();
        assert_eq!(rec["profile"], "open-small");
        assert_eq!(rec["overrides"], json!({"loop": {"detect": false}}));
        // A value equal to the profile's own is no override; one equal only to `base` is.
        let r = resolve(json!({"profile": "open-small", "calls": {"repairBudget": 2}})).unwrap();
        assert!(r.overrides.is_empty());
        let r = resolve(json!({"profile": "open-small", "calls": {"repairBudget": 0}})).unwrap();
        assert_eq!(r.overrides.len(), 1);
    }

    #[test]
    fn an_unknown_or_malformed_profile_name_is_refused_naming_profile() {
        let err = resolve(json!({"profile": "x"})).unwrap_err();
        assert_eq!(err, HarnessError::UnknownProfile("x".into()));
        assert!(err.to_string().contains("'profile'"));
        assert!(matches!(
            resolve(json!({"profile": 3})),
            Err(HarnessError::BadValue { switch, .. }) if switch == "profile"
        ));
    }

    #[test]
    fn profiles_are_pending_except_base_and_every_value_passes_its_rule() {
        for p in PROFILES {
            assert_eq!(
                p.evidence == "pending",
                p.name != PROFILE_BASE,
                "{}",
                p.name
            );
            // Sent as overrides to `base`, every profile value passes `rule_for`.
            let tree = p.harness().to_value().unwrap();
            let r = Harness::resolve(&tree).unwrap();
            assert_eq!(r.harness, p.harness(), "{}", p.name);
        }
        assert_eq!(profile("base").unwrap().harness(), Harness::default());
        let strong = profile("open-strong").unwrap().harness();
        assert_eq!(strong.adapter.thinking, "catalog");
        assert_eq!(strong.adapter.tool_choice, "auto");
        assert_eq!(strong.adapter.effort, "default");
        assert_eq!(strong.adapter.resample_on_invalid, 1);
        assert!(strong.tools.examples && strong.loop_.detect && strong.context.recite_todo);
        assert_eq!(strong.calls.repair_budget, 3);
        let mut small = profile("open-small").unwrap().harness();
        assert_eq!(small.calls.repair_budget, 2);
        assert_eq!(small.context.compaction.trigger_percent, 60);
        small.calls.repair_budget = 3;
        small.context.compaction.trigger_percent = 70;
        assert_eq!(small, strong);
    }

    #[test]
    fn adapter_levers_unlocked_in_h1_take_their_values_and_refuse_others() {
        for v in [
            json!({"adapter": {"thinking": "strip"}}),
            json!({"adapter": {"thinking": "keep"}}),
            json!({"adapter": {"thinking": "off"}}),
            json!({"adapter": {"thinking": "catalog"}}),
            json!({"adapter": {"toolChoice": "required"}}),
            json!({"adapter": {"toolChoice": "none"}}),
            json!({"adapter": {"resampleOnInvalid": 1}}),
            json!({"adapter": {"effort": "low"}}),
            json!({"adapter": {"effort": "medium"}}),
            json!({"adapter": {"effort": "high"}}),
            json!({"tools": {"examples": true}}),
        ] {
            let r = resolve(v.clone()).unwrap();
            assert_eq!(r.overrides.len(), 1, "{v}");
        }
        for (v, name) in [
            (json!({"adapter": {"thinking": "loud"}}), "adapter.thinking"),
            (
                json!({"adapter": {"toolChoice": "any"}}),
                "adapter.toolChoice",
            ),
            (json!({"adapter": {"effort": "max"}}), "adapter.effort"),
        ] {
            assert!(matches!(
                resolve(v).unwrap_err(),
                HarnessError::OneOf { switch, .. } if switch == name
            ));
        }
        assert_eq!(
            resolve(json!({"adapter": {"resampleOnInvalid": 2}})).unwrap_err(),
            HarnessError::OutOfRange {
                switch: "adapter.resampleOnInvalid".into(),
                lo: 0,
                hi: 1
            }
        );
        for name in ["parser", "strict", "guided"] {
            let v = match name {
                "parser" => json!("hermes"),
                _ => json!(true),
            };
            assert_eq!(
                resolve(json!({"adapter": {name: v}})).unwrap_err(),
                HarnessError::NotBuilt(format!("adapter.{name}"))
            );
        }
    }
}
