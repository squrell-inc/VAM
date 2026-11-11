//! The run receipt: built from the facts the host recorded, signed with the
//! node key, stored once (insert-only) and served by `GET /v1/runs/{id}/receipt`.
//!
//! Two tiers. The `receipt` object holds only what the host observed and is
//! what the signature covers. What the model wrote (the final reply and each
//! step's summary) lives only in `modelAppendix`, labelled, and the host tier
//! carries its digest. No raw tool argument or result is ever copied here:
//! decisions carry an argument digest only.
//!
//! It is built as `serde_json::Value`, never through prost JSON (which drops
//! `false` and zero fields such as `effects.journaled: false`). Numbers are
//! integers only, as the canonical form requires.

use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Map, Value};

use crate::effect_class::EffectClass;
use crate::effect_store::{EffectRow, EffectState};
use crate::error::OrchestratorError;
use crate::run_facts::{ApprovalProvenance, RunDecision, RunFacts, RunIsolationFact};
use crate::workflow::SqliteWorkflowStore;

pub const APPENDIX_LABEL: &str = "written by the model, not verified by the host";
pub const EFFECTS_STATEMENT: &str = "effects not journaled on this version";
/// What the journal cannot see, stated on every journaled effects section.
pub const EFFECTS_SCOPE: &str = "Lists calls the host mediated and journaled. It cannot see \
traffic a guest process sends directly, or a vendor applying one request twice; the vendor's \
own log is the independent count.";
/// Phase 1 always writes generation 1 (partial receipts are backlog).
pub const GENERATION: i64 = 1;

/// Run states in which a receipt is issued.
pub const TERMINAL_STATES: [&str; 6] = [
    "SUCCEEDED",
    "FAILED",
    "CANCELLED",
    "TIMED_OUT",
    "EXPIRED",
    "REJECTED",
];

pub fn is_terminal(state: &str) -> bool {
    TERMINAL_STATES.contains(&state)
}

// ---- storage ------------------------------------------------------------------------------

/// Insert-only: a stored receipt can be neither changed nor deleted.
pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS run_receipts (
            run_id TEXT NOT NULL,
            generation INTEGER NOT NULL,
            document TEXT NOT NULL,
            digest TEXT NOT NULL,
            key_id TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (run_id, generation)
        );
        CREATE TABLE IF NOT EXISTS receipt_effects_marker (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            first_at INTEGER NOT NULL
        );
        CREATE TRIGGER IF NOT EXISTS run_receipts_no_update
        BEFORE UPDATE ON run_receipts
        BEGIN SELECT RAISE(ABORT, 'run receipts are insert-only'); END;
        CREATE TRIGGER IF NOT EXISTS run_receipts_no_delete
        BEFORE DELETE ON run_receipts
        BEGIN SELECT RAISE(ABORT, 'run receipts are insert-only'); END;
        ",
    )?;
    // Written once: the time this node first started with the effects section. A run created
    // before it keeps the Phase 1 object (it never claims a journal it cannot show).
    connection.execute(
        "INSERT OR IGNORE INTO receipt_effects_marker (id, first_at) VALUES (1, ?1)",
        [crate::catalog::unix_timestamp()],
    )?;
    Ok(())
}

/// A receipt as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredReceipt {
    pub generation: i64,
    /// `{receipt, modelAppendix, signature}` as served.
    pub document: String,
    pub digest: String,
    pub key_id: String,
}

impl SqliteWorkflowStore {
    /// Stores the run's receipt unless it has one. Returns true when this call stored it.
    pub fn insert_run_receipt(
        &self,
        run_id: &str,
        generation: i64,
        document: &str,
        digest: &str,
        key_id: &str,
    ) -> Result<bool, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        // One transaction: a receipt never exists without the `run.finished` record it owes.
        let tx = connection.unchecked_transaction()?;
        let changed = tx.execute(
            "INSERT OR IGNORE INTO run_receipts
               (run_id, generation, document, digest, key_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                generation,
                document,
                digest,
                key_id,
                crate::catalog::unix_timestamp()
            ],
        )?;
        if changed > 0 {
            crate::notifications::record_run_finished(&tx, run_id, Some(digest))?;
        }
        tx.commit()?;
        Ok(changed > 0)
    }

    /// Seconds since the epoch when this node first started with the effects section.
    pub fn receipt_effects_marker(&self) -> Result<Option<i64>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection
            .query_row(
                "SELECT first_at FROM receipt_effects_marker WHERE id = 1",
                [],
                |row| row.get(0),
            )
            .optional()?)
    }

    /// When the run was created: `runs.created_at`, else the time its request was recorded
    /// (`run_requests.created_at`; chat runs never write the first). `None` when neither exists.
    pub fn run_created_at(&self, run_id: &str) -> Result<Option<i64>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection
            .query_row(
                "SELECT COALESCE(
                   (SELECT created_at FROM runs WHERE run_id = ?1),
                   (SELECT created_at FROM run_requests WHERE run_id = ?1))",
                [run_id],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()?
            .flatten())
    }

    /// The run's receipt (the newest generation), if issued.
    pub fn run_receipt(&self, run_id: &str) -> Result<Option<StoredReceipt>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        Ok(connection
            .query_row(
                "SELECT generation, document, digest, key_id FROM run_receipts
                 WHERE run_id = ?1 ORDER BY generation DESC LIMIT 1",
                [run_id],
                |row| {
                    Ok(StoredReceipt {
                        generation: row.get(0)?,
                        document: row.get(1)?,
                        digest: row.get(2)?,
                        key_id: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    /// Terminal runs with no receipt that ended at least `grace_secs` ago.
    pub fn runs_missing_receipts(&self, grace_secs: i64) -> Result<Vec<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let cutoff = crate::catalog::unix_timestamp() - grace_secs;
        let mut statement = connection.prepare(
            "SELECT r.run_id FROM runs r
             WHERE r.state IN ('SUCCEEDED','FAILED','CANCELLED','TIMED_OUT','EXPIRED','REJECTED')
               AND r.ended_at IS NOT NULL AND r.ended_at <= ?1
               AND NOT EXISTS (SELECT 1 FROM run_receipts x WHERE x.run_id = r.run_id)
             ORDER BY r.ended_at",
        )?;
        let rows = statement.query_map([cutoff], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    fn receipt_plain_rows(&self, run_id: &str) -> Result<PlainRows, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let (state, spec_json, result_json): (String, Option<String>, Option<String>) = connection
            .query_row(
                "SELECT state, spec_json, result_json FROM runs WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("run '{run_id}' not found"))
                } else {
                    OrchestratorError::Database(e)
                }
            })?;
        let playbook: Option<PlaybookRow> = connection
            .query_row(
                "SELECT playbook_urn, playbook_version, playbook_digest
                 FROM run_playbook WHERE run_id = ?1",
                [run_id],
                |row| {
                    Ok(PlaybookRow {
                        urn: row.get(0)?,
                        version: row.get(1)?,
                        digest: row.get(2)?,
                    })
                },
            )
            .optional()?;
        let mut statement = connection.prepare(
            "SELECT step_id, state, outcome, agent_summary, started_at, finished_at
             FROM run_playbook_steps WHERE run_id = ?1 ORDER BY position ASC",
        )?;
        let steps = statement
            .query_map([run_id], |row| {
                Ok(StepRow {
                    id: row.get(0)?,
                    state: row.get(1)?,
                    outcome: row.get(2)?,
                    agent_summary: row.get(3)?,
                    started_at: row.get(4)?,
                    finished_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(PlainRows {
            state,
            spec_json,
            result_json,
            playbook,
            steps,
        })
    }
}

struct PlainRows {
    state: String,
    spec_json: Option<String>,
    result_json: Option<String>,
    playbook: Option<PlaybookRow>,
    steps: Vec<StepRow>,
}

#[derive(Debug, Clone)]
pub struct PlaybookRow {
    pub urn: String,
    pub version: String,
    pub digest: String,
}

#[derive(Debug, Clone)]
pub struct StepRow {
    pub id: String,
    pub state: String,
    pub outcome: String,
    pub agent_summary: Option<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct Pin {
    pub urn: String,
    pub version: String,
    pub digest: String,
}

/// The run's effective harness as stored on its spec (`vam_spec::harness`).
/// Absent for a run created before the harness input existed.
#[derive(Debug, Clone, PartialEq)]
pub struct HarnessFact {
    pub profile: String,
    pub version: String,
    pub digest: String,
    /// Only what differs from the profile; `None` when nothing does.
    pub overrides: Option<Value>,
}

/// Everything the builder reads; plain data so the builder is a pure function.
pub struct Input {
    pub run_id: String,
    pub state: String,
    pub facts: RunFacts,
    pub decisions: Vec<RunDecision>,
    pub approvals: Vec<ApprovalProvenance>,
    pub head_before: Option<String>,
    pub head_after: Option<String>,
    pub agent: Option<Pin>,
    pub other_pins: Vec<Pin>,
    pub playbook: Option<PlaybookRow>,
    pub steps: Vec<StepRow>,
    pub iterations: Option<i64>,
    pub result_json: Option<String>,
    pub trajectory_digest: Option<String>,
    pub harness: Option<HarnessFact>,
    /// The run's effect journal rows, oldest first. `None` for a run created before the
    /// node first started with the effects section (the Phase 1 object is written).
    pub effects: Option<Vec<EffectRow>>,
}

// ---- gather -------------------------------------------------------------------------------

fn version_label(v: &vam_spec::v1::Version) -> String {
    if v.prerelease.is_empty() {
        format!("{}.{}.{}", v.major, v.minor, v.patch)
    } else {
        format!("{}.{}.{}-{}", v.major, v.minor, v.patch, v.prerelease)
    }
}

/// The harness record `resolve_harness` stored on the spec, if the run has one.
fn harness_fact(spec: &vam_spec::v1::RunSpec) -> Option<HarnessFact> {
    use prost_types::value::Kind;
    let record = match spec
        .inputs
        .as_ref()?
        .fields
        .get(vam_spec::harness::INPUT_KEY)?
    {
        prost_types::Value {
            kind: Some(Kind::StructValue(s)),
        } => vam_spec::harness::struct_to_json(s),
        _ => return None,
    };
    let text = |key: &str| record.get(key).and_then(Value::as_str).map(str::to_string);
    Some(HarnessFact {
        profile: text("profile")?,
        version: text("version")?,
        digest: text("digest")?,
        overrides: record
            .get("overrides")
            .filter(|o| o.as_object().is_some_and(|m| !m.is_empty()))
            .cloned(),
    })
}

fn pin_of(r: &vam_spec::v1::ResourceRef) -> Pin {
    Pin {
        urn: r
            .resource_id
            .as_ref()
            .map(|id| id.urn.clone())
            .unwrap_or_default(),
        version: r.version.as_ref().map(version_label).unwrap_or_default(),
        digest: r
            .digest
            .as_ref()
            .map(|d| d.hex_hash.clone())
            .unwrap_or_default(),
    }
}

/// SHA-256 of the replay blob the attestation names, computed without writing it.
pub fn trajectory_replay_digest(events: &[crate::error::OutboxEvent]) -> String {
    let selected: Vec<&crate::error::OutboxEvent> = events
        .iter()
        .filter(|e| crate::trajectory::is_trajectory_event(&e.event_type))
        .collect();
    let bytes = serde_json::to_vec(&selected).unwrap_or_default();
    vam_storage::cas::sha256_hex(&bytes)
}

pub fn gather(
    store: &SqliteWorkflowStore,
    run_id: &str,
    trajectory_digest: Option<&str>,
) -> Result<Input, OrchestratorError> {
    let rows = store.receipt_plain_rows(run_id)?;
    let spec: Option<vam_spec::v1::RunSpec> = rows
        .spec_json
        .as_deref()
        .and_then(|j| vam_spec::encoding::json_to_proto("vam.v1.RunSpec", j).ok());
    let agent = spec.as_ref().and_then(|s| s.agent_ref.as_ref()).map(pin_of);
    let playbook_urn = rows.playbook.as_ref().map(|p| p.urn.clone());
    let other_pins = spec
        .as_ref()
        .map(|s| {
            s.resolved_refs
                .iter()
                .map(pin_of)
                .filter(|p| Some(&p.urn) != playbook_urn.as_ref())
                .filter(|p| Some(&p.urn) != agent.as_ref().map(|a| &a.urn))
                .collect()
        })
        .unwrap_or_default();
    let heads = store.run_workspace_heads(run_id)?;
    let trajectory = match trajectory_digest {
        Some(d) => Some(d.to_string()),
        None => store
            .read_outbox_for_run(run_id, 0)
            .ok()
            .map(|events| trajectory_replay_digest(&events)),
    };
    let facts = store.run_facts(run_id)?;
    let journaled = match (
        store.run_created_at(run_id)?,
        store.receipt_effects_marker()?,
    ) {
        (Some(created), Some(first)) => created >= first,
        _ => false,
    };
    let effects = if journaled {
        Some(store.effects_for_run(run_id)?.rows)
    } else {
        None
    };
    Ok(Input {
        run_id: run_id.to_string(),
        state: rows.state,
        facts,
        decisions: store.run_decisions(run_id)?,
        approvals: store.approval_provenance(run_id)?,
        head_before: heads.as_ref().map(|h| h.head_before.clone()),
        head_after: heads.and_then(|h| h.head_after),
        agent,
        other_pins,
        playbook: rows.playbook,
        steps: rows.steps,
        iterations: store.get_run_metering(run_id)?.map(|m| m.iteration),
        result_json: rows.result_json,
        trajectory_digest: trajectory,
        harness: spec.as_ref().and_then(harness_fact),
        effects,
    })
}

// ---- build --------------------------------------------------------------------------------

fn put(obj: &mut Map<String, Value>, key: &str, value: Option<Value>) {
    if let Some(v) = value {
        obj.insert(key.to_string(), v);
    }
}

fn digest_label(hex: &str) -> String {
    if hex.contains(':') {
        hex.to_string()
    } else {
        format!("sha256:{hex}")
    }
}

fn pin_json(p: &Pin) -> Value {
    json!({"urn": p.urn, "version": p.version, "digest": p.digest})
}

fn resolution_of(decision: Option<&str>, granted: bool) -> String {
    match decision.map(str::to_ascii_uppercase).as_deref() {
        Some("APPROVED") if granted => "approved-once".into(),
        Some("AMENDED") if granted => "approved-amended".into(),
        Some("APPROVED") | Some("AMENDED") => "approved".into(),
        Some("REJECTED") => "rejected".into(),
        Some("EXPIRED") => "expired".into(),
        Some(other) => other.to_ascii_lowercase(),
        None => "unresolved".into(),
    }
}

struct OpenLine {
    line: Map<String, Value>,
    request: Option<String>,
    tool: String,
    granted: bool,
}

/// The step a guard condition names: `args_within <arg> in steps.<id>.output..`
/// or `allowed_when <id>[:<outcome>]`. A name only, never a value.
pub(crate) fn rule_step(rule: &str) -> Option<String> {
    let id = if let Some(rest) = rule.strip_prefix("args_within ") {
        let reference = rest.split_once(" in ")?.1.trim();
        let reference = reference
            .strip_prefix("{{")
            .and_then(|r| r.trim().strip_suffix("}}"))
            .map_or(reference, str::trim);
        let after = reference.strip_prefix("steps.")?;
        after.split(['.', '[']).next()?.to_string()
    } else {
        let token = rule.strip_prefix("allowed_when ")?;
        token.split(':').next()?.trim().to_string()
    };
    (!id.is_empty()).then_some(id)
}

/// The gate step a guard condition refers to, when the step it names is a gate.
fn gate_step_of(input: &Input, rule: Option<&str>) -> Option<String> {
    let step = rule_step(rule?)?;
    input
        .approvals
        .iter()
        .any(|a| a.kind == "gate" && a.step_id == step)
        .then_some(step)
}

fn tool_calls(input: &Input) -> Value {
    let (mut allowed, mut asked, mut denied) = (0i64, 0i64, 0i64);
    // Lines: denied and asked calls, in decision order. An ask and the later
    // allowed row that passed on its approval are one line.
    let mut lines: Vec<OpenLine> = Vec::new();
    for d in &input.decisions {
        match d.disposition.as_str() {
            "denied" => {
                denied += 1;
                let mut l = line(d, "denied");
                if let Some(gate) = gate_step_of(input, Some(&d.deciding_rule)) {
                    l.insert("gateStep".into(), Value::String(gate));
                }
                lines.push(OpenLine {
                    line: l,
                    request: None,
                    tool: d.tool_name.clone(),
                    granted: false,
                });
            }
            "asked" => {
                asked += 1;
                lines.push(OpenLine {
                    line: line(d, "asked"),
                    request: d.approval_request_id.clone(),
                    tool: d.tool_name.clone(),
                    granted: false,
                });
            }
            _ if d.via_grant => {
                let slot = lines.iter_mut().rev().find(|l| {
                    l.line.get("decision").and_then(Value::as_str) == Some("asked")
                        && !l.granted
                        && match (&l.request, &d.approval_request_id) {
                            (Some(a), Some(b)) => a == b,
                            _ => l.tool == d.tool_name,
                        }
                });
                match slot {
                    Some(l) => {
                        l.granted = true;
                        if let Some(gate) = gate_step_of(input, d.constraint_rule.as_deref()) {
                            l.line.insert("gateStep".into(), Value::String(gate));
                        }
                    }
                    None => allowed += 1,
                }
            }
            _ => {
                allowed += 1;
                // An allowed call that a guard constrained on a gate is a line:
                // the gate is part of why it was allowed. Other allowed calls
                // stay counted only.
                if let Some(gate) = gate_step_of(input, d.constraint_rule.as_deref()) {
                    let mut l = line(d, "allowed");
                    l.insert("gateStep".into(), Value::String(gate));
                    put(
                        &mut l,
                        "constraint",
                        d.constraint_rule.clone().map(Value::String),
                    );
                    lines.push(OpenLine {
                        line: l,
                        request: None,
                        tool: d.tool_name.clone(),
                        granted: false,
                    });
                }
            }
        }
    }
    let lines: Vec<Value> = lines
        .into_iter()
        .map(|mut open| {
            if open.line.get("decision").and_then(Value::as_str) == Some("asked") {
                let approval = open
                    .request
                    .as_ref()
                    .and_then(|id| input.approvals.iter().find(|a| &a.request_id == id));
                open.line.insert(
                    "resolution".into(),
                    Value::String(resolution_of(
                        approval.and_then(|a| a.decision.as_deref()),
                        open.granted,
                    )),
                );
                if let Some(a) = approval {
                    put(
                        &mut open.line,
                        "approvedBy",
                        a.decided_by_user_id.clone().map(Value::String),
                    );
                    put(
                        &mut open.line,
                        "surface",
                        a.decided_surface.clone().map(Value::String),
                    );
                    put(&mut open.line, "approvedAt", a.decided_at.map(Value::from));
                }
            }
            Value::Object(open.line)
        })
        .collect();
    let mut out = Map::new();
    out.insert(
        "counts".into(),
        json!({"allowed": allowed, "asked": asked, "denied": denied}),
    );
    out.insert("lines".into(), Value::Array(lines));
    let missing = input.facts.decision_write_failures;
    if missing > 0 {
        out.insert("notRecorded".into(), Value::from(missing));
        out.insert(
            "statement".into(),
            Value::String(format!("{missing} decisions not recorded")),
        );
    }
    Value::Object(out)
}

fn line(d: &RunDecision, decision: &str) -> Map<String, Value> {
    let mut l = Map::new();
    l.insert("seq".into(), Value::from(d.seq));
    l.insert("at".into(), Value::from(d.decided_at));
    l.insert("tool".into(), Value::String(d.tool_name.clone()));
    l.insert("decision".into(), Value::String(decision.to_string()));
    l.insert("decidedBy".into(), Value::String(d.decided_by.clone()));
    l.insert("scope".into(), Value::String(d.deciding_scope.clone()));
    l.insert("rule".into(), Value::String(d.deciding_rule.clone()));
    l.insert("argsDigest".into(), Value::String(d.args_digest.clone()));
    l.insert("sideEffect".into(), Value::from(d.side_effect));
    l
}

fn isolation(facts: &[RunIsolationFact]) -> (Value, Option<Value>) {
    let Some(first) = facts.first() else {
        return (
            json!({"summary": "isolation not recorded", "jailer": false}),
            None,
        );
    };
    let mut iso = Map::new();
    iso.insert("summary".into(), Value::String(first.summary.clone()));
    iso.insert("vmm".into(), Value::String(first.vmm.clone()));
    iso.insert("jailer".into(), Value::Bool(first.jailer));
    iso.insert("apiTls".into(), Value::Bool(first.api_tls));
    // What the node enforced when the run started; a run recorded before the node reported it
    // is "unknown", never "on".
    iso.insert(
        "rateLimits".into(),
        Value::String(
            first
                .rate_limits
                .clone()
                .unwrap_or_else(|| "unknown".into()),
        ),
    );
    let uniform = facts
        .iter()
        .all(|f| f.summary == first.summary && f.jailer == first.jailer);
    if !uniform {
        iso.insert(
            "generations".into(),
            Value::Array(
                facts
                    .iter()
                    .map(|f| {
                        json!({
                            "generation": f.lifecycle_generation,
                            "summary": f.summary,
                            "jailer": f.jailer,
                        })
                    })
                    .collect(),
            ),
        );
    }
    let image = if first.vmm == "firecracker" {
        let digests = facts
            .iter()
            .find(|f| f.rootfs_sha256.is_some() && f.kernel_sha256.is_some());
        Some(match digests {
            Some(f) => json!({
                "kernelDigest": f.kernel_sha256.as_deref().map(digest_label),
                "rootfsDigest": f.rootfs_sha256.as_deref().map(digest_label),
            }),
            None => json!({"statement": "image digests unavailable"}),
        })
    } else {
        None
    };
    (Value::Object(iso), image)
}

/// The questions the agent asked and how they were answered (AR-3.10): a human
/// decision of kind `question`. Absent when the run asked none.
fn questions(input: &Input) -> Option<Value> {
    let rows: Vec<Value> = input
        .approvals
        .iter()
        .filter(|a| a.kind == crate::hitl::APPROVAL_KIND_QUESTION)
        .map(|a| {
            let asked = a
                .requested_args_json
                .as_deref()
                .and_then(|j| serde_json::from_str::<Value>(j).ok());
            let mut q = Map::new();
            q.insert("kind".into(), Value::String("question".into()));
            q.insert("requestId".into(), Value::String(a.request_id.clone()));
            put(
                &mut q,
                "jig",
                asked.as_ref().and_then(|v| v.get("jig")).cloned(),
            );
            put(
                &mut q,
                "question",
                asked
                    .as_ref()
                    .and_then(|v| v.pointer("/data/question"))
                    .cloned(),
            );
            put(&mut q, "decision", a.decision.clone().map(Value::String));
            put(
                &mut q,
                "decidedBy",
                a.decided_by_user_id.clone().map(Value::String),
            );
            put(
                &mut q,
                "surface",
                a.decided_surface.clone().map(Value::String),
            );
            put(&mut q, "decidedAt", a.decided_at.map(Value::from));
            if a.decision.as_deref() == Some("SUBMITTED") {
                put(
                    &mut q,
                    "answer",
                    a.response_json
                        .as_deref()
                        .and_then(|j| serde_json::from_str::<Value>(j).ok()),
                );
            }
            Value::Object(q)
        })
        .collect();
    if rows.is_empty() {
        None
    } else {
        Some(Value::Array(rows))
    }
}

fn procedure(input: &Input) -> Option<Value> {
    input.playbook.as_ref()?;
    let steps: Vec<Value> = input
        .steps
        .iter()
        .map(|s| {
            let mut step = Map::new();
            step.insert("id".into(), Value::String(s.id.clone()));
            step.insert("state".into(), Value::String(s.state.clone()));
            // A gate's stored outcome is the option the decider chose, and that is
            // the decider's answer (it is under `gate.response`). The host's fact
            // about the step is that it was decided or rejected.
            let gate_decision = input
                .approvals
                .iter()
                .find(|a| a.kind == crate::hitl::APPROVAL_KIND_GATE && a.step_id == s.id)
                .and_then(|a| a.decision.as_deref());
            let outcome = match gate_decision {
                Some("SUBMITTED") => "decided".to_string(),
                Some("REJECTED") => "rejected".to_string(),
                _ => s.outcome.clone(),
            };
            step.insert("outcome".into(), Value::String(outcome));
            put(&mut step, "startedAt", s.started_at.map(Value::from));
            put(&mut step, "finishedAt", s.finished_at.map(Value::from));
            if let Some(a) = input
                .approvals
                .iter()
                .find(|a| a.kind == crate::hitl::APPROVAL_KIND_GATE && a.step_id == s.id)
            {
                let mut gate = Map::new();
                gate.insert("requestId".into(), Value::String(a.request_id.clone()));
                put(&mut gate, "decision", a.decision.clone().map(Value::String));
                put(
                    &mut gate,
                    "decidedBy",
                    a.decided_by_user_id.clone().map(Value::String),
                );
                put(
                    &mut gate,
                    "surface",
                    a.decided_surface.clone().map(Value::String),
                );
                put(&mut gate, "decidedAt", a.decided_at.map(Value::from));
                put(
                    &mut gate,
                    "response",
                    a.response_json
                        .as_deref()
                        .and_then(|j| serde_json::from_str::<Value>(j).ok()),
                );
                step.insert("gate".into(), Value::Object(gate));
            }
            Value::Object(step)
        })
        .collect();
    Some(json!({"steps": steps}))
}

/// The model appendix: what the model wrote, and nothing the host observed.
pub fn appendix(input: &Input) -> Value {
    let mut a = Map::new();
    a.insert("label".into(), Value::String(APPENDIX_LABEL.into()));
    let reply = input
        .result_json
        .as_deref()
        .and_then(|j| serde_json::from_str::<Value>(j).ok())
        .and_then(|v| v.get("answer").and_then(Value::as_str).map(str::to_string));
    put(&mut a, "finalReply", reply.map(Value::String));
    let steps: Vec<Value> = input
        .steps
        .iter()
        .filter_map(|s| {
            s.agent_summary
                .as_ref()
                .filter(|t| !t.is_empty())
                .map(|t| json!({"id": s.id, "summary": t}))
        })
        .collect();
    a.insert("steps".into(), Value::Array(steps));
    Value::Object(a)
}

/// Builds the host tier and the appendix (the host tier carries the appendix digest).
pub fn build(input: &Input) -> Result<(Value, Value), OrchestratorError> {
    let appendix = appendix(input);
    let appendix_digest = vam_security::receipt::appendix_digest(&appendix)
        .map_err(|e| OrchestratorError::Core(format!("receipt appendix: {e}")))?;
    let mut r = Map::new();
    r.insert(
        "version".into(),
        Value::String(vam_spec::receipt::RECEIPT_VERSION.into()),
    );
    r.insert("runId".into(), Value::String(input.run_id.clone()));
    r.insert("state".into(), Value::String(input.state.clone()));
    r.insert("generation".into(), Value::from(GENERATION));
    put(
        &mut r,
        "requestedBy",
        input.facts.requested_by_user_id.clone().map(Value::String),
    );
    put(
        &mut r,
        "requestSurface",
        input.facts.surface.clone().map(Value::String),
    );
    put(
        &mut r,
        "requestTrigger",
        input.facts.trigger.as_ref().map(|t| {
            json!({
                "brokerId": t.broker_id,
                "grantId": t.grant_id,
                "triggerRef": t.trigger_ref,
                "fireAtMs": t.fire_at.saturating_mul(1000),
                "fireKind": t.fire_kind,
            })
        }),
    );
    put(&mut r, "startedAt", input.facts.created_at.map(Value::from));
    put(&mut r, "endedAt", input.facts.ended_at.map(Value::from));

    let mut pins = Map::new();
    put(&mut pins, "agent", input.agent.as_ref().map(pin_json));
    put(
        &mut pins,
        "playbook",
        input.playbook.as_ref().map(
            |p| json!({"urn": p.urn, "version": p.version, "digest": digest_label(&p.digest)}),
        ),
    );
    pins.insert(
        "other".into(),
        Value::Array(input.other_pins.iter().map(pin_json).collect()),
    );
    r.insert("pins".into(), Value::Object(pins));

    // The effective harness: profile, version, digest of the full switch tree
    // (kept on the run record), and only the switches the caller changed.
    if let Some(h) = &input.harness {
        let mut harness = Map::new();
        harness.insert("profile".into(), Value::String(h.profile.clone()));
        harness.insert("version".into(), Value::String(h.version.clone()));
        harness.insert("digest".into(), Value::String(h.digest.clone()));
        put(&mut harness, "overrides", h.overrides.clone());
        r.insert("harness".into(), Value::Object(harness));
    }

    r.insert(
        "model".into(),
        Value::Array(
            input
                .facts
                .models
                .iter()
                .map(|m| {
                    json!({
                        "name": m.model,
                        "provider": m.provider,
                        "endpointHost": m.endpoint_host,
                        "calls": m.calls,
                        "promptTokens": m.prompt_tokens,
                        "completionTokens": m.completion_tokens,
                    })
                })
                .collect(),
        ),
    );
    let (iso, image) = isolation(&input.facts.isolation);
    r.insert("isolation".into(), iso);
    put(&mut r, "image", image);

    let mut workspace = Map::new();
    put(
        &mut workspace,
        "headBefore",
        input.head_before.clone().map(Value::String),
    );
    put(
        &mut workspace,
        "headAfter",
        input.head_after.clone().map(Value::String),
    );
    r.insert("workspace".into(), Value::Object(workspace));

    r.insert("toolCalls".into(), tool_calls(input));
    put(&mut r, "procedure", procedure(input));
    put(&mut r, "questions", questions(input));

    let prompt: i64 = input.facts.models.iter().map(|m| m.prompt_tokens).sum();
    let completion: i64 = input.facts.models.iter().map(|m| m.completion_tokens).sum();
    let approvals = input
        .approvals
        .iter()
        .filter(|a| a.decision.is_some())
        .count() as i64;
    let mut metering = Map::new();
    metering.insert("promptTokens".into(), Value::from(prompt));
    metering.insert("completionTokens".into(), Value::from(completion));
    metering.insert("approvals".into(), Value::from(approvals));
    put(&mut metering, "turns", input.iterations.map(Value::from));
    r.insert("metering".into(), Value::Object(metering));

    r.insert("effects".into(), effects(input.effects.as_deref()));
    put(
        &mut r,
        "trajectoryDigest",
        input.trajectory_digest.clone().map(Value::String),
    );
    r.insert("modelAppendixDigest".into(), Value::String(appendix_digest));
    Ok((Value::Object(r), appendix))
}

/// The effects section. `None` is the Phase 1 object, byte for byte.
///
/// Per run, from the journal only: one line per non-read row (oldest first), reads
/// counted. No call id, no detail text, no argument or result. Integers only.
pub(crate) fn effects(rows: Option<&[EffectRow]>) -> Value {
    let Some(rows) = rows else {
        return json!({"journaled": false, "statement": EFFECTS_STATEMENT});
    };
    let (mut confirmed, mut failed, mut reconciled, mut not_sent) = (0i64, 0i64, 0i64, 0i64);
    let (mut unknown, mut duplicates, mut identical, mut reads) = (0i64, 0i64, 0i64, 0i64);
    let mut lines: Vec<Value> = Vec::new();
    // (seq, tool, args digest) of earlier non-idempotent rows that were sent and did not fail.
    let mut earlier_sent: Vec<(i64, &str, &str)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        let seq = index as i64 + 1;
        if row.class == EffectClass::Read {
            reads += 1;
            continue;
        }
        let checked_by = row.reconciled_by.as_deref().filter(|c| *c == "verify");
        let state = match row.state {
            EffectState::Confirmed if checked_by.is_some() => {
                reconciled += 1;
                "reconciled"
            }
            EffectState::Confirmed => {
                confirmed += 1;
                "confirmed"
            }
            EffectState::Failed => {
                failed += 1;
                "failed"
            }
            // `sent` at a terminal run left the host with no answer recorded.
            EffectState::Unknown | EffectState::Sent => {
                unknown += 1;
                "unknown"
            }
            EffectState::Intended => {
                not_sent += 1;
                "notSent"
            }
        };
        let mut repeat_of = None;
        if row.class == EffectClass::NonIdempotent {
            duplicates += i64::from(row.attempts.saturating_sub(1));
            if row.attempts >= 1 {
                repeat_of = earlier_sent
                    .iter()
                    .find(|(_, tool, args)| *tool == row.tool && *args == row.args_digest)
                    .map(|(seq, _, _)| *seq);
                if repeat_of.is_some() {
                    identical += 1;
                }
                if row.state != EffectState::Failed {
                    earlier_sent.push((seq, &row.tool, &row.args_digest));
                }
            }
        }
        let mut line = Map::new();
        line.insert("seq".into(), Value::from(seq));
        line.insert("tool".into(), Value::String(row.tool.clone()));
        line.insert("adapter".into(), Value::String(row.adapter.clone()));
        line.insert("class".into(), Value::String(row.class.as_str().into()));
        line.insert("state".into(), Value::String(state.into()));
        line.insert("attempts".into(), Value::from(i64::from(row.attempts)));
        line.insert(
            "keyDigest".into(),
            Value::String(format!(
                "sha256:{}",
                vam_storage::cas::sha256_hex(row.idempotency_key.as_bytes())
            )),
        );
        line.insert(
            "argsDigest".into(),
            Value::String(digest_label(&row.args_digest)),
        );
        put(
            &mut line,
            "resultDigest",
            row.result_digest
                .as_deref()
                .map(|d| Value::String(digest_label(d))),
        );
        put(
            &mut line,
            "checkedBy",
            checked_by.map(|c| Value::String(c.into())),
        );
        put(
            &mut line,
            "parentSeq",
            row.parent_call_id
                .as_deref()
                .and_then(|p| rows.iter().position(|r| r.call_id == p))
                .map(|i| Value::from(i as i64 + 1)),
        );
        put(&mut line, "repeatOf", repeat_of.map(Value::from));
        if row.call_id.starts_with("legacy-") {
            line.insert("hostDerivedId".into(), Value::Bool(true));
        }
        line.insert("at".into(), Value::from(row.created_at / 1000));
        lines.push(Value::Object(line));
    }
    json!({
        "journaled": true,
        "scope": EFFECTS_SCOPE,
        "duplicates": duplicates,
        "unknown": unknown,
        "counts": {
            "confirmed": confirmed,
            "failed": failed,
            "reconciled": reconciled,
            "notSent": not_sent,
            "identicalRepeats": identical,
            "reads": reads,
        },
        "lines": lines,
    })
}

// ---- issue --------------------------------------------------------------------------------

/// Builds, signs and stores the run's receipt. A no-op when it has one.
/// `trajectory_digest` is the one the attestation named, when the caller has it.
///
/// Call this only after the run's terminal work is done (the finalizer calls it
/// last); the GET route never does.
pub fn issue_receipt(
    store: &SqliteWorkflowStore,
    run_id: &str,
    trajectory_digest: Option<&str>,
) -> Result<(), OrchestratorError> {
    if store.run_receipt(run_id)?.is_some() {
        return Ok(());
    }
    let state = store.get_run_state(run_id)?;
    if !is_terminal(&state) {
        return Err(OrchestratorError::Conflict(format!(
            "run '{run_id}' is {state}; a receipt is issued when it ends"
        )));
    }
    let keys = crate::signing::node_keys().ok_or_else(|| {
        OrchestratorError::Core("node signing keys not installed; receipt not issued".into())
    })?;
    let input = gather(store, run_id, trajectory_digest)?;
    let (receipt, appendix) = build(&input)?;
    let signature = vam_security::receipt::sign_receipt(keys.current(), &receipt)
        .map_err(|e| OrchestratorError::Core(format!("receipt signing: {e}")))?;
    let document = json!({
        "receipt": receipt,
        "modelAppendix": appendix,
        "signature": serde_json::to_value(&signature)
            .map_err(|e| OrchestratorError::Core(e.to_string()))?,
    });
    let text = serde_json::to_string(&document)
        .map_err(|e| OrchestratorError::Core(format!("receipt serialize: {e}")))?;
    store.insert_run_receipt(
        run_id,
        GENERATION,
        &text,
        &signature.receipt_digest,
        &signature.key_id,
    )?;
    tracing::info!(run_id, key_id = %signature.key_id, "[orchestrator] run receipt issued");
    Ok(())
}

/// Issues the receipt, logging a failure: issuing never fails the caller's work.
pub fn issue_receipt_logged(
    store: &SqliteWorkflowStore,
    run_id: &str,
    trajectory_digest: Option<&str>,
) {
    if let Err(e) = issue_receipt(store, run_id, trajectory_digest) {
        tracing::warn!(run_id, "[orchestrator] receipt not issued: {e}");
    }
}

/// Issues every missing receipt of runs that ended at least `grace_secs` ago.
/// Used at startup (grace 0) and by the background sweeper.
pub fn issue_missing_receipts(store: &SqliteWorkflowStore, grace_secs: i64) -> usize {
    let ids = store.runs_missing_receipts(grace_secs).unwrap_or_default();
    let mut issued = 0;
    for id in ids {
        match issue_receipt(store, &id, None) {
            Ok(()) => issued += 1,
            Err(e) => tracing::warn!(run_id = %id, "[orchestrator] receipt reconcile: {e}"),
        }
    }
    // A run whose receipt cannot be signed (no node keys) still owes its record, without a
    // digest, once it has waited a minute; the receipt, when issued, fills the digest.
    for id in store
        .runs_missing_receipts(grace_secs.max(NO_KEYS_RECORD_GRACE_SECS))
        .unwrap_or_default()
    {
        if let Err(e) = store.record_run_finished_unsigned(&id) {
            tracing::warn!(run_id = %id, "[orchestrator] run.finished without a receipt: {e}");
        }
    }
    issued
}

/// How long a run waits for its receipt before `run.finished` is recorded without a digest.
const NO_KEYS_RECORD_GRACE_SECS: i64 = 60;

impl SqliteWorkflowStore {
    /// Records `run.finished` for a run with no receipt (section 2.3 still decides).
    fn record_run_finished_unsigned(&self, run_id: &str) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let tx = connection.unchecked_transaction()?;
        crate::notifications::record_run_finished(&tx, run_id, None)?;
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::ResourceId;

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/rc1";

    fn keys() {
        let _ = crate::signing::install(crate::signing::NodeKeys::ephemeral());
    }

    fn decision(
        seq: i64,
        tool: &str,
        disposition: &str,
        via_grant: bool,
        req: Option<&str>,
    ) -> RunDecision {
        RunDecision {
            seq,
            session_id: "s".into(),
            decided_at: 1,
            tool_name: tool.into(),
            target: format!("builtin:{tool}"),
            side_effect: 2,
            disposition: disposition.into(),
            decided_by: "cedar_rule".into(),
            deciding_scope: "agent".into(),
            deciding_rule: format!("builtin:{tool}"),
            args_digest: "sha256:aa".into(),
            approval_request_id: req.map(str::to_string),
            via_grant,
            constraint_rule: None,
        }
    }

    fn approval(id: &str, kind: &str, step: &str, decision: &str) -> ApprovalProvenance {
        ApprovalProvenance {
            request_id: id.into(),
            step_id: step.into(),
            kind: kind.into(),
            decision: Some(decision.into()),
            decided_at: Some(5),
            decided_surface: Some("api".into()),
            decided_by_user_id: Some("user-1".into()),
            reviewer_principal: None,
            response_json: Some(r#"{"option":"go"}"#.into()),
            requested_args_json: None,
        }
    }

    fn facts() -> RunFacts {
        RunFacts {
            created_at: Some(1),
            ended_at: Some(9),
            requested_by_user_id: Some("user-1".into()),
            surface: Some("api".into()),
            trigger: None,
            decision_write_failures: 0,
            isolation: vec![],
            models: vec![],
        }
    }

    fn input() -> Input {
        Input {
            run_id: RUN.into(),
            state: "SUCCEEDED".into(),
            facts: facts(),
            decisions: vec![],
            approvals: vec![],
            head_before: Some("h0".into()),
            head_after: Some("h1".into()),
            agent: Some(Pin {
                urn: "urn:a".into(),
                version: "1.0.0".into(),
                digest: "d".into(),
            }),
            other_pins: vec![],
            playbook: None,
            steps: vec![],
            iterations: Some(2),
            result_json: Some(r#"{"answer":"hi there"}"#.into()),
            trajectory_digest: Some("t".into()),
            harness: None,
            effects: None,
        }
    }

    #[test]
    fn requested_trigger_is_flat_and_absent_for_a_person() {
        let (person, _) = build(&input()).unwrap();
        assert!(person.get("requestTrigger").is_none());
        let mut i = input();
        i.facts.trigger = Some(crate::run_facts::TriggerFact {
            broker_id: "b1".into(),
            grant_id: "rg_1".into(),
            trigger_ref: "daily".into(),
            fire_at: 1_700_000_000,
            fire_kind: "scheduled".into(),
        });
        let (r, _) = build(&i).unwrap();
        assert_eq!(
            r["requestTrigger"],
            json!({"brokerId": "b1", "grantId": "rg_1", "triggerRef": "daily",
                   "fireAtMs": 1_700_000_000_000_i64, "fireKind": "scheduled"})
        );
        // Everything else is the person receipt.
        let mut without = r.clone();
        without.as_object_mut().unwrap().remove("requestTrigger");
        assert_eq!(without, person);
    }

    fn vm_fact(rootfs: Option<&str>) -> RunIsolationFact {
        let iso = crate::isolation::describe(&crate::isolation::IsolationInputs {
            guest_mode: "vm",
            use_jailer: true,
            api_tls: true,
            rate_limits: Default::default(),
        });
        let iso_rate_limits = iso.rate_limits.clone();
        RunIsolationFact {
            session_id: "s".into(),
            guest_mode: "vm".into(),
            vmm: iso.vmm,
            jailer: iso.jailer,
            api_tls: iso.api_tls,
            summary: iso.summary,
            kernel_sha256: rootfs.map(|_| "kk".into()),
            rootfs_sha256: rootfs.map(str::to_string),
            lifecycle_generation: 1,
            recorded_at: 1,
            rate_limits: Some(iso_rate_limits),
        }
    }

    #[test]
    fn ask_and_its_approved_call_are_one_line() {
        let mut i = input();
        i.decisions = vec![
            decision(1, "write_file", "denied", false, None),
            decision(2, "step_complete", "allowed", false, None),
            decision(3, "write_file", "asked", false, Some("r1")),
            decision(4, "write_file", "allowed", true, Some("r1")),
        ];
        i.approvals = vec![approval("r1", "tool_call", "finish", "APPROVED")];
        let (r, _) = build(&i).unwrap();
        let tc = &r["toolCalls"];
        assert_eq!(tc["counts"], json!({"allowed": 1, "asked": 1, "denied": 1}));
        assert_eq!(tc["lines"].as_array().unwrap().len(), 2);
        assert_eq!(tc["lines"][0]["decision"], "denied");
        assert_eq!(tc["lines"][1]["decision"], "asked");
        assert_eq!(tc["lines"][1]["resolution"], "approved-once");
        assert_eq!(tc["lines"][1]["approvedBy"], "user-1");
        assert!(tc.get("statement").is_none());
    }

    fn spec_with_harness(record: Option<Value>) -> vam_spec::v1::RunSpec {
        let mut fields = std::collections::BTreeMap::new();
        if let Some(r) = record {
            fields.insert(
                "harness".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StructValue(
                        vam_spec::harness::json_to_struct(&r).expect("object"),
                    )),
                },
            );
        }
        vam_spec::v1::RunSpec {
            inputs: Some(prost_types::Struct { fields }),
            ..Default::default()
        }
    }

    fn record_of(overrides: Value) -> Value {
        let resolved = vam_spec::harness::Harness::resolve(&overrides).unwrap();
        vam_spec::harness::Harness::record(&resolved).unwrap()
    }

    #[test]
    fn a_default_run_prints_profile_version_digest_and_no_overrides() {
        let spec = spec_with_harness(Some(record_of(json!({}))));
        let mut i = input();
        i.harness = harness_fact(&spec);
        let (r, _) = build(&i).unwrap();
        assert_eq!(r["harness"]["profile"], "base");
        assert_eq!(r["harness"]["version"], "0.1.0");
        assert!(r["harness"]["digest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        assert!(r["harness"].get("overrides").is_none());
        // Switch values stay on the run record, not on the receipt.
        assert!(r["harness"].get("switches").is_none());
    }

    #[test]
    fn an_override_is_printed_as_stored_integers_only() {
        let spec = spec_with_harness(Some(record_of(
            json!({"loop": {"detect": true, "cycles": 4}}),
        )));
        let mut i = input();
        i.harness = harness_fact(&spec);
        let (r, _) = build(&i).unwrap();
        assert_eq!(
            r["harness"]["overrides"],
            json!({"loop": {"detect": true, "cycles": 4}})
        );
        // Numbers came through a prost Struct (f64); the canonical form needs integers.
        vam_spec::receipt::canonical_bytes(&r).expect("canonical, no float");
    }

    #[test]
    fn a_run_without_a_harness_record_has_no_harness_key() {
        assert!(harness_fact(&spec_with_harness(None)).is_none());
        let (r, _) = build(&input()).unwrap();
        assert!(r.get("harness").is_none());
    }

    #[test]
    fn the_harness_digest_binds_the_override() {
        let a = harness_fact(&spec_with_harness(Some(record_of(json!({}))))).unwrap();
        let b = harness_fact(&spec_with_harness(Some(record_of(
            json!({"loop": {"detect": true}}),
        ))))
        .unwrap();
        assert_ne!(a.digest, b.digest);
    }

    #[test]
    fn unrecorded_decisions_are_stated() {
        let mut i = input();
        i.facts.decision_write_failures = 3;
        let (r, _) = build(&i).unwrap();
        assert_eq!(r["toolCalls"]["statement"], "3 decisions not recorded");
        assert_eq!(r["toolCalls"]["notRecorded"], 3);
    }

    #[test]
    fn model_text_is_only_in_the_appendix_and_bound_by_digest() {
        let mut i = input();
        i.playbook = Some(PlaybookRow {
            urn: "urn:p".into(),
            version: "1.0.0".into(),
            digest: "dd".into(),
        });
        i.steps = vec![StepRow {
            id: "summarise".into(),
            state: "completed".into(),
            outcome: "done".into(),
            agent_summary: Some("SECRET-MODEL-WORDS".into()),
            started_at: Some(1),
            finished_at: Some(2),
        }];
        let (r, a) = build(&i).unwrap();
        let host = serde_json::to_string(&r).unwrap();
        assert!(!host.contains("SECRET-MODEL-WORDS") && !host.contains("hi there"));
        assert_eq!(a["label"], APPENDIX_LABEL);
        assert_eq!(a["finalReply"], "hi there");
        assert_eq!(a["steps"][0]["summary"], "SECRET-MODEL-WORDS");
        assert_eq!(
            r["modelAppendixDigest"],
            vam_security::receipt::appendix_digest(&a).unwrap()
        );
        assert!(r["procedure"]["steps"][0].get("summary").is_none());
    }

    #[test]
    fn host_tier_carries_no_raw_arguments() {
        let mut i = input();
        i.decisions = vec![decision(1, "write_file", "denied", false, None)];
        let (r, _) = build(&i).unwrap();
        let keys: Vec<_> = r["toolCalls"]["lines"][0]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        for k in keys {
            assert!(
                !matches!(k.as_str(), "args" | "arguments" | "path" | "result"),
                "{k}"
            );
        }
        assert_eq!(r["toolCalls"]["lines"][0]["argsDigest"], "sha256:aa");
    }

    #[test]
    fn a_run_from_before_the_marker_keeps_the_phase_1_effects_object() {
        let (r, _) = build(&input()).unwrap();
        assert!(r.get("procedure").is_none());
        assert_eq!(
            r["effects"],
            json!({"journaled": false, "statement": EFFECTS_STATEMENT})
        );
        assert_eq!(
            serde_json::to_string(&r["effects"]).unwrap(),
            r#"{"journaled":false,"statement":"effects not journaled on this version"}"#
        );
        assert_eq!(r["workspace"]["headAfter"], "h1");
    }

    fn row(
        call: &str,
        tool: &str,
        class: EffectClass,
        state: EffectState,
        attempts: u32,
    ) -> EffectRow {
        EffectRow {
            run_id: RUN.into(),
            call_id: call.into(),
            parent_call_id: None,
            tool: tool.into(),
            adapter: "hsp".into(),
            class,
            state,
            idempotency_key: format!("vamfx_{call}"),
            args_digest: format!("{:0>64}", tool.len()),
            approval_id: None,
            attempts,
            generation_sent: None,
            result_digest: None,
            result_bytes: None,
            detail: Some("vendor says: secret text".into()),
            reconciled_by: None,
            created_at: 5_000,
            updated_at: 5_000,
        }
    }

    fn effects_of(rows: Vec<EffectRow>) -> Value {
        let mut i = input();
        i.effects = Some(rows);
        build(&i).unwrap().0["effects"].clone()
    }

    #[test]
    fn a_journaled_run_with_no_rows_says_zeros_never_the_old_line() {
        let e = effects_of(vec![]);
        assert_eq!(e["journaled"], true);
        assert!(e.get("statement").is_none());
        assert_eq!(e["duplicates"], 0);
        assert_eq!(e["unknown"], 0);
        assert_eq!(e["counts"]["reads"], 0);
        assert_eq!(e["lines"], json!([]));
        assert!(e["scope"].as_str().unwrap().contains("directly"));
    }

    #[test]
    fn states_map_from_the_journal_at_a_terminal_run() {
        use EffectClass::NonIdempotent as N;
        use EffectState as S;
        let mut checked = row("c3", "t", N, S::Confirmed, 1);
        checked.reconciled_by = Some("verify".into());
        let mut not_found = row("c5", "u", N, S::Unknown, 1);
        not_found.reconciled_by = Some("verify".into());
        let e = effects_of(vec![
            row("c1", "a", N, S::Confirmed, 1),
            row("c2", "bb", N, S::Sent, 1),
            checked,
            row("c4", "ccc", N, S::Intended, 0),
            not_found,
            row("c6", "dddd", N, S::Failed, 0),
        ]);
        let states: Vec<_> = e["lines"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["state"].as_str().unwrap())
            .collect();
        assert_eq!(
            states,
            [
                "confirmed",
                "unknown",
                "reconciled",
                "notSent",
                "unknown",
                "failed"
            ]
        );
        assert_eq!(e["unknown"], 2);
        assert_eq!(e["counts"]["confirmed"], 1);
        assert_eq!(e["counts"]["reconciled"], 1);
        assert_eq!(e["counts"]["notSent"], 1);
        assert_eq!(e["counts"]["failed"], 1);
        assert_eq!(e["lines"][2]["checkedBy"], "verify");
        assert_eq!(e["lines"][4]["checkedBy"], "verify");
        assert!(e["lines"][0].get("checkedBy").is_none());
        assert_eq!(e["lines"][5]["attempts"], 0);
    }

    #[test]
    fn duplicates_count_only_non_idempotent_resends() {
        use EffectClass::{Idempotent, NonIdempotent};
        let e = effects_of(vec![
            row("c1", "a", NonIdempotent, EffectState::Confirmed, 2),
            row("c2", "bb", Idempotent, EffectState::Confirmed, 2),
        ]);
        assert_eq!(e["duplicates"], 1);
        assert_eq!(e["counts"]["identicalRepeats"], 0);
        assert_eq!(e["lines"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn an_identical_repeat_under_a_new_call_id_names_the_earlier_line() {
        use EffectClass::{Idempotent, NonIdempotent as N};
        let e = effects_of(vec![
            row("c1", "a", N, EffectState::Unknown, 1),
            row("c2", "a", N, EffectState::Failed, 0),
            row("c3", "a", N, EffectState::Confirmed, 1),
            row("c4", "a", Idempotent, EffectState::Confirmed, 1),
            row("c5", "a", Idempotent, EffectState::Confirmed, 1),
        ]);
        // c2 never left (denied); c3 repeats c1; idempotent repeats are not repeats.
        assert_eq!(e["counts"]["identicalRepeats"], 1);
        assert!(e["lines"][1].get("repeatOf").is_none());
        assert_eq!(e["lines"][2]["repeatOf"], 1);
        assert!(e["lines"][3].get("repeatOf").is_none());
        assert_eq!(e["duplicates"], 0);
    }

    #[test]
    fn reads_are_counted_not_listed_and_verify_rows_name_the_check() {
        use EffectClass::{NonIdempotent as N, Read};
        let mut create = row("c1", "create", N, EffectState::Confirmed, 1);
        create.reconciled_by = Some("verify".into());
        let e = effects_of(vec![
            create,
            row("c1/verify", "find", Read, EffectState::Confirmed, 1),
            row("c2", "find", Read, EffectState::Confirmed, 1),
        ]);
        assert_eq!(e["counts"]["reads"], 2);
        assert_eq!(e["lines"].as_array().unwrap().len(), 1);
        assert_eq!(e["lines"][0]["checkedBy"], "verify");
        assert_eq!(e["lines"][0]["state"], "reconciled");
    }

    #[test]
    fn a_line_has_digests_and_a_derived_id_flag_but_no_call_id_detail_or_payload() {
        use EffectClass::NonIdempotent as N;
        let mut nested = row(
            "legacy-t-ab12cd34-1/step",
            "t",
            N,
            EffectState::Confirmed,
            1,
        );
        nested.parent_call_id = Some("legacy-t-ab12cd34-1".into());
        nested.result_digest = Some("ee".repeat(32));
        let parent = row("legacy-t-ab12cd34-1", "p", N, EffectState::Confirmed, 1);
        let e = effects_of(vec![parent, nested]);
        let line = &e["lines"][1];
        assert_eq!(line["parentSeq"], 1);
        assert_eq!(line["hostDerivedId"], true);
        assert_eq!(line["at"], 5);
        assert!(line["keyDigest"].as_str().unwrap().starts_with("sha256:"));
        assert!(line["argsDigest"].as_str().unwrap().starts_with("sha256:"));
        assert!(line["resultDigest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
        let text = e.to_string();
        for forbidden in ["legacy-t", "vamfx_", "secret text", "callId", "detail"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }

    #[test]
    fn a_run_with_no_runs_created_at_uses_its_request_time() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteWorkflowStore::open(dir.path().join("vam.db")).unwrap();
        let marker = store.receipt_effects_marker().unwrap().unwrap();
        {
            let c = store.connection.lock().unwrap();
            c.execute(
                "INSERT INTO runs (run_id, state, revision) VALUES ('r_new', 'SUCCEEDED', 1)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO run_requests (run_id, requested_by_user_id, surface, created_at)
                 VALUES ('r_new', 'u', 'api', ?1)",
                [marker],
            )
            .unwrap();
            c.execute(
                "INSERT INTO runs (run_id, state, revision) VALUES ('r_old', 'SUCCEEDED', 1)",
                [],
            )
            .unwrap();
            c.execute(
                "INSERT INTO run_requests (run_id, requested_by_user_id, surface, created_at)
                 VALUES ('r_old', 'u', 'api', ?1)",
                [marker - 10],
            )
            .unwrap();
            c.execute(
                "INSERT INTO runs (run_id, state, revision) VALUES ('r_none', 'SUCCEEDED', 1)",
                [],
            )
            .unwrap();
        }
        let journaled = |id: &str| {
            matches!(
                (
                    store.run_created_at(id).unwrap(),
                    store.receipt_effects_marker().unwrap()
                ),
                (Some(c), Some(m)) if c >= m
            )
        };
        assert!(journaled("r_new"));
        assert!(!journaled("r_old"));
        assert!(!journaled("r_none"));
    }

    #[test]
    fn the_marker_decides_which_runs_are_journaled() {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteWorkflowStore::open(dir.path().join("vam.db")).unwrap();
        let first = store.receipt_effects_marker().unwrap().unwrap();
        assert!(first > 0);
        // Opening again keeps the first time.
        drop(store);
        let store = SqliteWorkflowStore::open(dir.path().join("vam.db")).unwrap();
        assert_eq!(store.receipt_effects_marker().unwrap(), Some(first));
    }

    #[test]
    fn gate_step_names_who_where_when_and_the_answer() {
        let mut i = input();
        i.playbook = Some(PlaybookRow {
            urn: "urn:p".into(),
            version: "1.0.0".into(),
            digest: "dd".into(),
        });
        i.steps = vec![StepRow {
            id: "confirm".into(),
            state: "completed".into(),
            outcome: "go".into(),
            agent_summary: None,
            started_at: None,
            finished_at: None,
        }];
        i.approvals = vec![approval("g1", "gate", "confirm", "SUBMITTED")];
        let (r, _) = build(&i).unwrap();
        let g = &r["procedure"]["steps"][0]["gate"];
        assert_eq!(g["decidedBy"], "user-1");
        assert_eq!(g["surface"], "api");
        assert_eq!(g["decidedAt"], 5);
        assert_eq!(g["response"]["option"], "go");
        assert_eq!(r["procedure"]["steps"][0]["outcome"], "decided");
    }

    #[test]
    fn a_question_is_listed_as_a_human_decision_of_kind_question() {
        let mut i = input();
        let mut q = approval("q1", "question", "", "SUBMITTED");
        q.requested_args_json =
            Some(r#"{"jig":"vam.question","data":{"question":"Which name?"}}"#.into());
        q.response_json = Some(r#"{"answer":"Ada"}"#.into());
        i.approvals = vec![q];
        let (r, _) = build(&i).unwrap();
        let q = &r["questions"][0];
        assert_eq!(q["kind"], "question");
        assert_eq!(q["question"], "Which name?");
        assert_eq!(q["answer"]["answer"], "Ada");
        assert_eq!(q["decidedBy"], "user-1");
        assert_eq!(q["surface"], "api");
        // A run that asked nothing has no `questions`.
        let (r, _) = build(&input()).unwrap();
        assert!(r.get("questions").is_none());
    }

    fn constrained(mut d: RunDecision, rule: &str) -> RunDecision {
        d.constraint_rule = Some(rule.into());
        d
    }

    fn gate_input() -> Input {
        let mut i = input();
        i.approvals = vec![approval("g1", "gate", "confirm", "SUBMITTED")];
        i
    }

    #[test]
    fn an_allowed_call_constrained_by_a_gate_gets_a_line_naming_it() {
        let mut i = gate_input();
        i.decisions = vec![
            constrained(
                decision(1, "close_issue", "allowed", false, None),
                "args_within issue_key in steps.confirm.output.keys",
            ),
            decision(2, "read_file", "allowed", false, None),
        ];
        let (r, _) = build(&i).unwrap();
        let lines = r["toolCalls"]["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert_eq!(lines[0]["decision"], "allowed");
        assert_eq!(lines[0]["tool"], "close_issue");
        assert_eq!(lines[0]["gateStep"], "confirm");
        assert_eq!(r["toolCalls"]["counts"]["allowed"], 2);
    }

    #[test]
    fn an_unconstrained_allowed_call_is_counted_with_no_line() {
        let mut i = gate_input();
        i.decisions = vec![decision(1, "read_file", "allowed", false, None)];
        let (r, _) = build(&i).unwrap();
        assert!(r["toolCalls"]["lines"].as_array().unwrap().is_empty());
        assert_eq!(r["toolCalls"]["counts"]["allowed"], 1);
    }

    #[test]
    fn a_denied_args_within_line_names_the_gate() {
        let mut i = gate_input();
        let mut d = decision(1, "close_issue", "denied", false, None);
        d.deciding_rule = "args_within issue_key in steps.confirm.output.keys".into();
        i.decisions = vec![d];
        let (r, _) = build(&i).unwrap();
        assert_eq!(r["toolCalls"]["lines"][0]["gateStep"], "confirm");
    }

    #[test]
    fn a_rule_naming_a_step_that_is_not_a_gate_gets_no_gate_step() {
        let mut i = gate_input();
        let mut denied = decision(1, "close_issue", "denied", false, None);
        denied.deciding_rule = "args_within issue_key in steps.query.output.keys".into();
        i.decisions = vec![
            denied,
            constrained(
                decision(2, "close_issue", "allowed", false, None),
                "allowed_when query:done",
            ),
        ];
        let (r, _) = build(&i).unwrap();
        let lines = r["toolCalls"]["lines"].as_array().unwrap();
        assert_eq!(lines.len(), 1);
        assert!(lines[0].get("gateStep").is_none());
    }

    #[test]
    fn rule_step_reads_the_step_a_guard_names() {
        assert_eq!(
            rule_step("args_within k in {{ steps.g.output.keys[*] }}").as_deref(),
            Some("g")
        );
        assert_eq!(rule_step("allowed_when g:go").as_deref(), Some("g"));
        assert_eq!(rule_step("builtin:write_file"), None);
    }

    #[test]
    fn microvm_receipt_names_firecracker_jailer_and_image_digests() {
        let mut i = input();
        i.facts.isolation = vec![vm_fact(Some("rr"))];
        let (r, _) = build(&i).unwrap();
        assert_eq!(r["isolation"]["summary"], "firecracker microVM, jailer on");
        assert_eq!(r["isolation"]["jailer"], true);
        assert_eq!(r["image"]["rootfsDigest"], "sha256:rr");
        assert_eq!(r["image"]["kernelDigest"], "sha256:kk");
    }

    #[test]
    fn microvm_without_digests_says_so() {
        let mut i = input();
        i.facts.isolation = vec![vm_fact(None)];
        let (r, _) = build(&i).unwrap();
        assert_eq!(r["image"]["statement"], "image digests unavailable");
        assert!(r["image"].get("rootfsDigest").is_none());
    }

    #[test]
    fn the_receipt_carries_the_rate_limit_setting_the_run_had() {
        let mut i = input();
        i.facts.isolation = vec![vm_fact(None)];
        let (r, _) = build(&i).unwrap();
        assert_eq!(
            r["isolation"]["rateLimits"],
            "on (30/s burst 100, runs 2/s burst 20)"
        );
        assert_eq!(r["isolation"]["summary"], "firecracker microVM, jailer on");
        let mut off = vm_fact(None);
        off.rate_limits = Some("off".into());
        i.facts.isolation = vec![off];
        let (r, _) = build(&i).unwrap();
        assert_eq!(r["isolation"]["rateLimits"], "off");
    }

    #[test]
    fn a_run_recorded_before_the_setting_existed_reads_unknown_never_on() {
        let mut i = input();
        let mut old = vm_fact(None);
        old.rate_limits = None;
        i.facts.isolation = vec![old];
        let (r, _) = build(&i).unwrap();
        assert_eq!(r["isolation"]["rateLimits"], "unknown");
    }

    #[test]
    fn process_guest_prints_only_what_the_node_reports() {
        let mut i = input();
        i.facts.isolation = vec![RunIsolationFact {
            vmm: "process".into(),
            jailer: false,
            summary: "process guest (no isolation)".into(),
            guest_mode: "process".into(),
            ..vm_fact(None)
        }];
        let (r, _) = build(&i).unwrap();
        assert_eq!(r["isolation"]["summary"], "process guest (no isolation)");
        assert!(r.get("image").is_none());
    }

    #[test]
    fn image_digests_come_from_the_kernel_and_rootfs_files() {
        let dir = tempfile::tempdir().unwrap();
        let (k, r) = (dir.path().join("k"), dir.path().join("r"));
        std::fs::write(&k, b"kernel").unwrap();
        std::fs::write(&r, b"rootfs").unwrap();
        let (kd, rd) = crate::golden::image_digests_of(&k, &r).unwrap();
        let mut f = vm_fact(Some(&rd));
        f.kernel_sha256 = Some(kd);
        let mut i = input();
        i.facts.isolation = vec![f];
        let (receipt, _) = build(&i).unwrap();
        assert!(receipt["image"]["rootfsDigest"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
    }

    fn finished_run(store: &SqliteWorkflowStore, state: &str) {
        store.create_run(&ResourceId { urn: RUN.into() }).unwrap();
        store
            .complete_run_with_result_now(RUN, state, r#"{"answer":"ok"}"#)
            .unwrap();
    }

    #[test]
    fn one_immutable_receipt_per_run_and_reissue_is_a_noop() {
        keys();
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store.create_run(&ResourceId { urn: RUN.into() }).unwrap();
        // A live run gets none.
        assert!(matches!(
            issue_receipt(&store, RUN, None),
            Err(OrchestratorError::Conflict(_))
        ));
        assert!(store.run_receipt(RUN).unwrap().is_none());
        store
            .complete_run_with_result_now(RUN, "SUCCEEDED", r#"{"answer":"ok"}"#)
            .unwrap();
        issue_receipt(&store, RUN, Some("tdigest")).unwrap();
        let first = store.run_receipt(RUN).unwrap().unwrap();
        issue_receipt(&store, RUN, Some("other")).unwrap();
        assert_eq!(store.run_receipt(RUN).unwrap().unwrap(), first);
        // Signed, and verifies from the document alone.
        let v = vam_security::receipt::verify_receipt(first.document.as_bytes(), &[]).unwrap();
        assert_eq!(v.key_id, first.key_id);
        let doc: Value = serde_json::from_str(&first.document).unwrap();
        assert_eq!(doc["receipt"]["trajectoryDigest"], "tdigest");
        // Insert-only.
        let c = store.connection.lock().unwrap();
        assert!(c
            .execute("UPDATE run_receipts SET document = 'x'", [])
            .is_err());
        assert!(c.execute("DELETE FROM run_receipts", []).is_err());
    }

    #[test]
    fn reconcile_fills_the_gap_for_terminal_runs_only() {
        keys();
        let store = SqliteWorkflowStore::in_memory().unwrap();
        finished_run(&store, "CANCELLED");
        let live = "urn:vam:run:local.vam.dev/acme/live";
        store.create_run(&ResourceId { urn: live.into() }).unwrap();
        assert_eq!(
            store.runs_missing_receipts(0).unwrap(),
            vec![RUN.to_string()]
        );
        assert_eq!(issue_missing_receipts(&store, 0), 1);
        assert!(store.run_receipt(RUN).unwrap().is_some());
        assert!(store.run_receipt(live).unwrap().is_none());
        assert_eq!(issue_missing_receipts(&store, 0), 0);
        // The grace keeps the sweeper off a run that just ended.
        let other = "urn:vam:run:local.vam.dev/acme/fresh";
        store.create_run(&ResourceId { urn: other.into() }).unwrap();
        store
            .complete_run_with_result_now(other, "FAILED", "{}")
            .unwrap();
        assert!(store.runs_missing_receipts(3600).unwrap().is_empty());
    }

    #[test]
    fn replay_digest_equals_the_one_the_attestation_names() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store.create_run(&ResourceId { urn: RUN.into() }).unwrap();
        let kind = crate::trajectory::TRAJECTORY_EVENT_TYPES[0];
        store.append_outbox_event(RUN, kind, r#"{"a":1}"#).unwrap();
        store
            .append_outbox_event(RUN, "vam.v1.run.attested", "{}")
            .unwrap();
        let events = store.read_outbox_for_run(RUN, 0).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let chunks =
            vam_storage::ChunkStore::new(vam_storage::Drive9Layout::new(dir.path().to_path_buf()));
        let stored = crate::trajectory::persist_trajectory_replay_blob(&events, &chunks).unwrap();
        assert_eq!(trajectory_replay_digest(&events), stored);
    }

    #[test]
    fn a_stored_receipt_brings_its_run_finished_record_and_a_late_one_fills_the_digest() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        for (id, source) in [("r-trig", Some("broker:vam.schedule")), ("r-chat", None)] {
            store
                .persist_run_spec_with_origin(
                    id,
                    "{}",
                    Some("s"),
                    "urn:vam:agent:local/acme/bot",
                    None,
                    "p",
                    "local",
                    source,
                    None,
                )
                .unwrap();
            store.update_run_state(id, "SUCCEEDED").unwrap();
        }
        let digest_of = |id: &str| -> Option<(i64, Option<String>)> {
            let c = store.connection.lock().unwrap();
            c.query_row(
                "SELECT count, receipt_digest FROM notifications WHERE kind = 'run.finished' AND run_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok()
        };
        // No keys: the sweeper's minute pass records without a digest, once.
        assert_eq!(digest_of("r-trig"), None);
        store.record_run_finished_unsigned("r-trig").unwrap();
        store.record_run_finished_unsigned("r-trig").unwrap();
        assert_eq!(digest_of("r-trig"), Some((1, None)));
        // The receipt arrives: stored once, the record gets exactly its digest.
        assert!(store
            .insert_run_receipt("r-trig", 1, "{}", "sha256:aa", "k")
            .unwrap());
        assert!(!store
            .insert_run_receipt("r-trig", 1, "{}", "sha256:bb", "k")
            .unwrap());
        assert_eq!(digest_of("r-trig"), Some((1, Some("sha256:aa".into()))));
        let stored = store.run_receipt("r-trig").unwrap().unwrap();
        assert_eq!(
            digest_of("r-trig").unwrap().1.as_deref(),
            Some(stored.digest.as_str())
        );
        // A person's chat turn that did not wait leaves nothing.
        assert!(store
            .insert_run_receipt("r-chat", 1, "{}", "sha256:cc", "k")
            .unwrap());
        assert_eq!(digest_of("r-chat"), None);
    }
}
