//! Outcomes per agent version: a pure aggregation over parsed, stored, signed run receipts.
//!
//! The receipts are read as stored (never rebuilt or re-signed) and joined to the agent on
//! `runs.agent_urn`. A run belongs to the group its receipt pins: a released version by its
//! label, a draft by the digest of the draft it ran (a draft keeps changing, so only the digest
//! names its content). The answer is unsigned JSON for readers, so floats are allowed here and
//! zeros are kept. It carries no run id and no user id.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{params, Connection};
use serde_json::{json, Map, Value};
use vam_inference::ModelCatalog;

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// The pass^k default: the benchmark's default.
pub const DEFAULT_K: u32 = 5;

/// The index the join on `runs.agent_urn` needs (`owner.rs` indexes only the owner).
pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection
        .execute_batch("CREATE INDEX IF NOT EXISTS idx_runs_agent_urn ON runs(agent_urn);")?;
    Ok(())
}

/// What the aggregation reads from one stored receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptFacts {
    /// `SUCCEEDED`, `FAILED`, `TIMED_OUT`, `CANCELLED`, `EXPIRED` or `REJECTED`.
    pub state: String,
    /// `receipt.pins.agent.version`: `1.0.0`, or `1.0.0-draft` for a draft run.
    pub version: String,
    /// `receipt.pins.agent.digest` without its `sha256:` label.
    pub digest: String,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub turns: Option<i64>,
    pub approvals: i64,
    /// `(model name, prompt tokens, completion tokens)` per model.
    pub models: Vec<(String, i64, i64)>,
    /// `None` for a receipt written before the journal (the Phase 1 object).
    pub effects: Option<EffectFacts>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct EffectFacts {
    /// Confirmed plus reconciled: effects known to have happened.
    pub confirmed: i64,
    pub unknown: i64,
    pub duplicates: i64,
    pub identical_repeats: i64,
}

impl ReceiptFacts {
    pub fn is_draft(&self) -> bool {
        self.version.contains("-draft")
    }

    fn counted(&self) -> bool {
        matches!(self.state.as_str(), "SUCCEEDED" | "FAILED" | "TIMED_OUT")
    }
}

fn strip_label(digest: &str) -> String {
    digest.strip_prefix("sha256:").unwrap_or(digest).to_string()
}

/// Parses a stored `{receipt, modelAppendix, signature}` document. `None` when it names no agent.
pub fn parse_receipt(document: &str) -> Option<ReceiptFacts> {
    let doc: Value = serde_json::from_str(document).ok()?;
    let r = doc.get("receipt").unwrap_or(&doc);
    let agent = r.get("pins")?.get("agent")?;
    let version = agent.get("version")?.as_str()?.to_string();
    let digest = strip_label(agent.get("digest").and_then(Value::as_str).unwrap_or(""));
    let int = |v: &Value| v.as_i64().unwrap_or(0);
    let models: Vec<(String, i64, i64)> = r
        .get("model")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .map(|m| {
                    (
                        m["name"].as_str().unwrap_or_default().to_string(),
                        int(&m["promptTokens"]),
                        int(&m["completionTokens"]),
                    )
                })
                .collect()
        })
        .unwrap_or_default();
    let metering = &r["metering"];
    let effects = r
        .get("effects")
        .filter(|e| e["journaled"] == true)
        .map(|e| {
            let c = &e["counts"];
            EffectFacts {
                confirmed: int(&c["confirmed"]) + int(&c["reconciled"]),
                unknown: int(&e["unknown"]),
                duplicates: int(&e["duplicates"]),
                identical_repeats: int(&c["identicalRepeats"]),
            }
        });
    Some(ReceiptFacts {
        state: r["state"].as_str().unwrap_or_default().to_string(),
        version,
        digest,
        prompt_tokens: int(&metering["promptTokens"]),
        completion_tokens: int(&metering["completionTokens"]),
        turns: metering["turns"].as_i64(),
        approvals: int(&metering["approvals"]),
        models,
        effects,
    })
}

/// Which runs one aggregate covers.
#[derive(Debug, Clone, Copy)]
pub enum Group<'a> {
    /// A released version by its label (`1.0.0`).
    Release(&'a str),
    /// The draft by the digest of the content it ran (no label).
    Draft(&'a str),
}

pub fn select<'a>(facts: &'a [ReceiptFacts], group: Group<'_>) -> Vec<&'a ReceiptFacts> {
    facts
        .iter()
        .filter(|f| match group {
            Group::Release(v) => !f.is_draft() && f.version == v,
            Group::Draft(d) => f.is_draft() && f.digest == strip_label(d),
        })
        .collect()
}

/// The unbiased pass^k estimator `C(s, k) / C(n, k)`: the chance that k runs drawn from this
/// version's `n` runs all succeeded. `None` when `n < k` (or `k == 0`).
pub fn pass_k(n: u64, s: u64, k: u64) -> Option<f64> {
    if k == 0 || n < k {
        return None;
    }
    if s < k {
        return Some(0.0);
    }
    // The product of (s - i) / (n - i): no factorial overflow.
    let mut value = 1.0f64;
    for i in 0..k {
        value *= (s - i) as f64 / (n - i) as f64;
    }
    Some(value)
}

/// One aggregate over `runs`. `k` is the pass^k size.
pub fn aggregate(runs: &[&ReceiptFacts], k: u32, catalog: &ModelCatalog) -> Value {
    let count = |state: &str| runs.iter().filter(|f| f.state == state).count() as u64;
    let successes = count("SUCCEEDED");
    let failed = count("FAILED");
    let timed_out = count("TIMED_OUT");
    let total = successes + failed + timed_out;
    let counted: Vec<&&ReceiptFacts> = runs.iter().filter(|f| f.counted()).collect();

    let mut out = Map::new();
    out.insert("runs".into(), json!(total));
    out.insert("successes".into(), json!(successes));
    out.insert("failed".into(), json!(failed));
    out.insert("timedOut".into(), json!(timed_out));
    out.insert("cancelled".into(), json!(count("CANCELLED")));
    out.insert("expired".into(), json!(count("EXPIRED")));
    out.insert("rejected".into(), json!(count("REJECTED")));
    if total > 0 {
        out.insert("successRate".into(), json!(successes as f64 / total as f64));
        out.insert(
            "approvalsPerRun".into(),
            json!(counted.iter().map(|f| f.approvals).sum::<i64>() as f64 / total as f64),
        );
    }

    let mut pk = Map::new();
    pk.insert("k".into(), json!(k));
    match pass_k(total, successes, u64::from(k)) {
        Some(v) => {
            pk.insert("value".into(), json!(v));
        }
        None => {
            pk.insert(
                "note".into(),
                json!(format!("needs at least {k} runs, this version has {total}")),
            );
        }
    }
    out.insert("passK".into(), Value::Object(pk));

    if successes > 0 {
        let tokens: i64 = counted
            .iter()
            .map(|f| f.prompt_tokens + f.completion_tokens)
            .sum();
        out.insert(
            "tokensPerSuccess".into(),
            json!(tokens as f64 / successes as f64),
        );
        let turns: Vec<i64> = counted.iter().filter_map(|f| f.turns).collect();
        if !turns.is_empty() {
            out.insert(
                "turnsPerSuccess".into(),
                json!(turns.iter().sum::<i64>() as f64 / successes as f64),
            );
        }
    }

    // Cost: the model maker's list price over the receipts' tokens; tokens only when any
    // model has no price (the mock). The receipt carries no cache split, so cache tokens
    // are not separated here.
    let mut micro: i64 = 0;
    let mut unpriced: BTreeSet<String> = BTreeSet::new();
    let mut sources: BTreeSet<String> = BTreeSet::new();
    for f in &counted {
        for (name, prompt, completion) in &f.models {
            match catalog.estimate_list_micro(name, *prompt, *completion) {
                Some(m) => {
                    micro += m;
                    if let Some(s) = catalog.list_price_source(name) {
                        sources.insert(s);
                    }
                }
                None => {
                    unpriced.insert(name.clone());
                }
            }
        }
    }
    if !unpriced.is_empty() {
        out.insert(
            "costMissing".into(),
            Value::Array(unpriced.into_iter().map(Value::String).collect()),
        );
    } else if successes > 0 {
        let per = (micro as f64 / successes as f64).round() as i64;
        out.insert("costPerSuccessMicroUsd".into(), json!(per));
        if !sources.is_empty() {
            out.insert(
                "priceSource".into(),
                json!(sources.into_iter().collect::<Vec<_>>().join("; ")),
            );
        }
        out.insert("catalogVersion".into(), json!(catalog.catalog_version()));
    }

    // Effects: summed over every journaled receipt of the group (a cancelled run's duplicate
    // is still a duplicate). Absent, never 0, when none is journaled.
    if !runs.is_empty() {
        let journaled: Vec<EffectFacts> = runs.iter().filter_map(|f| f.effects).collect();
        let mut e = Map::new();
        e.insert("journaledRuns".into(), json!(journaled.len()));
        e.insert(
            "unjournaledRuns".into(),
            json!(runs.len() - journaled.len()),
        );
        if !journaled.is_empty() {
            e.insert(
                "confirmed".into(),
                json!(journaled.iter().map(|x| x.confirmed).sum::<i64>()),
            );
            e.insert(
                "unknown".into(),
                json!(journaled.iter().map(|x| x.unknown).sum::<i64>()),
            );
            e.insert(
                "duplicates".into(),
                json!(journaled.iter().map(|x| x.duplicates).sum::<i64>()),
            );
            e.insert(
                "identicalRepeats".into(),
                json!(journaled.iter().map(|x| x.identical_repeats).sum::<i64>()),
            );
        }
        out.insert("effects".into(), Value::Object(e));
    }
    Value::Object(out)
}

fn semver_key(v: &str) -> (u64, u64, u64) {
    let mut p = v.split(['.', '-']).map(|s| s.parse::<u64>().unwrap_or(0));
    (
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
        p.next().unwrap_or(0),
    )
}

/// The release a diff of `candidate` (`draft` or `X.Y.Z`) compares against: the highest
/// release strictly below it (for `draft`, the latest release). Drafts in `releases` and
/// unparsable labels are skipped. `None` for a first release.
pub fn base_version(candidate: &str, releases: &[String]) -> Option<String> {
    let below = |v: &&String| {
        !v.ends_with("-draft")
            && v.as_str() != "draft"
            && (candidate == "draft" || semver_key(v) < semver_key(candidate))
    };
    releases
        .iter()
        .filter(below)
        .max_by_key(|v| semver_key(v))
        .cloned()
}

/// What changed from `base` to `candidate` (two aggregates). A metric is listed only when
/// both sides have it; duplicates show whichever sides have them.
pub fn changes(base: &Value, candidate: &Value) -> Value {
    let mut out = Map::new();
    let pairs: [(&str, &[&str]); 5] = [
        ("successRate", &["successRate"]),
        ("passK", &["passK", "value"]),
        ("costPerSuccessMicroUsd", &["costPerSuccessMicroUsd"]),
        ("tokensPerSuccess", &["tokensPerSuccess"]),
        ("approvalsPerRun", &["approvalsPerRun"]),
    ];
    let dig = |v: &Value, path: &[&str]| -> Option<f64> {
        path.iter().try_fold(v, |v, k| v.get(*k))?.as_f64()
    };
    for (name, path) in pairs {
        if let (Some(b), Some(c)) = (dig(base, path), dig(candidate, path)) {
            out.insert(
                name.into(),
                json!({ "base": b, "candidate": c, "delta": c - b }),
            );
        }
    }
    let b = dig(base, &["effects", "duplicates"]);
    let c = dig(candidate, &["effects", "duplicates"]);
    if b.is_some() || c.is_some() {
        let mut d = Map::new();
        if let Some(b) = b {
            d.insert("base".into(), json!(b as i64));
        }
        if let Some(c) = c {
            d.insert("candidate".into(), json!(c as i64));
        }
        if let (Some(b), Some(c)) = (b, c) {
            d.insert("delta".into(), json!((c - b) as i64));
        }
        out.insert("duplicates".into(), Value::Object(d));
    }
    Value::Object(out)
}

/// The versions that have receipts: released versions newest first, then `draft` when the
/// caller's current draft digest has runs. Each row has its counted runs and successes.
pub fn list_versions(facts: &[ReceiptFacts], draft_digest: Option<&str>) -> Vec<Value> {
    let mut released: BTreeMap<(u64, u64, u64), String> = BTreeMap::new();
    for f in facts.iter().filter(|f| !f.is_draft()) {
        released.insert(semver_key(&f.version), f.version.clone());
    }
    let mut rows: Vec<Value> = Vec::new();
    if let Some(d) = draft_digest {
        let runs = select(facts, Group::Draft(d));
        if !runs.is_empty() {
            rows.push(row("draft", "draft", &runs));
        }
    }
    for (_, label) in released.iter().rev() {
        let runs = select(facts, Group::Release(label));
        rows.push(row(label, "released", &runs));
    }
    rows
}

fn row(version: &str, channel: &str, runs: &[&ReceiptFacts]) -> Value {
    json!({
        "version": version,
        "channel": channel,
        "runs": runs.iter().filter(|f| f.counted()).count(),
        "successes": runs.iter().filter(|f| f.state == "SUCCEEDED").count(),
    })
}

impl SqliteWorkflowStore {
    /// The parsed stored receipts of every run of `agent_urn` (the newest generation of each).
    pub fn receipt_facts_for_agent(
        &self,
        agent_urn: &str,
    ) -> Result<Vec<ReceiptFacts>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT rr.run_id, rr.generation, rr.document
               FROM run_receipts rr JOIN runs r ON r.run_id = rr.run_id
              WHERE r.agent_urn = ?1",
        )?;
        let rows = stmt.query_map(params![agent_urn], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let mut newest: BTreeMap<String, (i64, String)> = BTreeMap::new();
        for row in rows {
            let (run, generation, document) = row?;
            match newest.get(&run) {
                Some((g, _)) if *g >= generation => {}
                _ => {
                    newest.insert(run, (generation, document));
                }
            }
        }
        Ok(newest
            .values()
            .filter_map(|(_, doc)| parse_receipt(doc))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(version: &str, digest: &str, state: &str) -> ReceiptFacts {
        ReceiptFacts {
            state: state.into(),
            version: version.into(),
            digest: digest.into(),
            prompt_tokens: 100,
            completion_tokens: 20,
            turns: Some(2),
            approvals: 0,
            models: vec![("mock-model".into(), 100, 20)],
            effects: Some(EffectFacts::default()),
        }
    }

    fn agg(runs: Vec<ReceiptFacts>, k: u32) -> Value {
        let refs: Vec<&ReceiptFacts> = runs.iter().collect();
        aggregate(&refs, k, &ModelCatalog::new())
    }

    #[test]
    fn pass_k_vectors() {
        assert!((pass_k(3, 2, 2).unwrap() - 1.0 / 3.0).abs() < 1e-12);
        assert_eq!(pass_k(3, 3, 2), Some(1.0));
        assert_eq!(pass_k(1, 1, 2), None);
        assert_eq!(pass_k(5, 1, 2), Some(0.0));
        assert_eq!(pass_k(5, 0, 5), Some(0.0));
        assert_eq!(pass_k(3, 2, 0), None);
    }

    #[test]
    fn pass_k_is_in_the_aggregate_and_absent_with_a_note_below_k() {
        let runs = vec![
            facts("1.0.0", "a", "SUCCEEDED"),
            facts("1.0.0", "a", "SUCCEEDED"),
            facts("1.0.0", "a", "FAILED"),
        ];
        let v = agg(runs.clone(), 2);
        assert_eq!(v["passK"]["k"], 2);
        assert!((v["passK"]["value"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-12);
        let v = agg(runs, 5);
        assert!(v["passK"].get("value").is_none());
        assert!(v["passK"]["note"].as_str().unwrap().contains("at least 5"));
    }

    #[test]
    fn denominator_excludes_cancelled_expired_and_rejected() {
        let v = agg(
            vec![
                facts("1.0.0", "a", "SUCCEEDED"),
                facts("1.0.0", "a", "FAILED"),
                facts("1.0.0", "a", "TIMED_OUT"),
                facts("1.0.0", "a", "CANCELLED"),
                facts("1.0.0", "a", "EXPIRED"),
                facts("1.0.0", "a", "REJECTED"),
            ],
            2,
        );
        assert_eq!(v["runs"], 3);
        assert_eq!(v["successes"], 1);
        assert_eq!(v["failed"], 1);
        assert_eq!(v["timedOut"], 1);
        assert_eq!(v["cancelled"], 1);
        assert_eq!(v["expired"], 1);
        assert_eq!(v["rejected"], 1);
        assert!((v["successRate"].as_f64().unwrap() - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn a_pre_journal_receipt_counts_for_success_and_tokens_but_not_effects() {
        let mut old = facts("1.0.0", "a", "SUCCEEDED");
        old.effects = None;
        let mut new = facts("1.0.0", "a", "SUCCEEDED");
        new.effects = Some(EffectFacts {
            confirmed: 2,
            unknown: 1,
            duplicates: 0,
            identical_repeats: 1,
        });
        let v = agg(vec![old.clone(), new], 2);
        assert_eq!(v["successes"], 2);
        assert_eq!(v["tokensPerSuccess"], 120.0);
        assert_eq!(v["effects"]["journaledRuns"], 1);
        assert_eq!(v["effects"]["unjournaledRuns"], 1);
        assert_eq!(v["effects"]["confirmed"], 2);
        assert_eq!(v["effects"]["unknown"], 1);
        assert_eq!(v["effects"]["duplicates"], 0);
        assert_eq!(v["effects"]["identicalRepeats"], 1);
        // None journaled: the sums are absent, never 0.
        let v = agg(vec![old], 2);
        assert_eq!(v["effects"]["journaledRuns"], 0);
        assert_eq!(v["effects"]["unjournaledRuns"], 1);
        for key in ["confirmed", "unknown", "duplicates", "identicalRepeats"] {
            assert!(v["effects"].get(key).is_none(), "{key}");
        }
    }

    #[test]
    fn a_draft_is_grouped_by_its_digest_and_an_edit_starts_a_new_group() {
        let all = vec![
            facts("1.0.0", "r", "SUCCEEDED"),
            facts("1.0.1-draft", "d1", "SUCCEEDED"),
            facts("1.0.1-draft", "d1", "FAILED"),
            facts("1.0.1-draft", "d2", "SUCCEEDED"),
        ];
        assert_eq!(select(&all, Group::Draft("sha256:d1")).len(), 2);
        assert_eq!(select(&all, Group::Draft("d2")).len(), 1);
        assert_eq!(select(&all, Group::Draft("d3")).len(), 0);
        // A draft run never counts toward a release label.
        assert_eq!(select(&all, Group::Release("1.0.0")).len(), 1);
        assert_eq!(select(&all, Group::Release("1.0.1")).len(), 0);
    }

    #[test]
    fn list_has_the_draft_first_then_releases_newest_first() {
        let all = vec![
            facts("1.0.0", "r", "SUCCEEDED"),
            facts("1.10.0", "r2", "FAILED"),
            facts("1.2.0", "r3", "SUCCEEDED"),
            facts("1.2.1-draft", "d", "SUCCEEDED"),
        ];
        let rows = list_versions(&all, Some("d"));
        let names: Vec<&str> = rows
            .iter()
            .map(|r| r["version"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["draft", "1.10.0", "1.2.0", "1.0.0"]);
        // A draft with no runs at its current digest is not listed.
        let rows = list_versions(&all, Some("other"));
        assert_eq!(rows[0]["version"], "1.10.0");
        assert_eq!(rows[0]["runs"], 1);
        assert_eq!(rows[0]["successes"], 0);
    }

    #[test]
    fn cost_is_priced_at_the_makers_list_price_and_absent_when_any_model_is_unpriced() {
        let priced = |state: &str| {
            let mut f = facts("1.0.0", "a", state);
            f.models = vec![("claude-haiku-5-5".into(), 1_000_000, 1_000_000)];
            f
        };
        let v = agg(vec![priced("SUCCEEDED"), priced("FAILED")], 2);
        // 2 runs at 600_000 micro each, one success.
        assert_eq!(v["costPerSuccessMicroUsd"], 1_200_000);
        assert!(v["priceSource"]
            .as_str()
            .unwrap()
            .starts_with("anthropic list price, catalog"));
        assert!(v["catalogVersion"].is_string());
        assert!(v.get("costMissing").is_none());

        let mut mixed = priced("SUCCEEDED");
        mixed.models.push(("mock-model".into(), 5, 5));
        let v = agg(vec![mixed], 2);
        assert!(v.get("costPerSuccessMicroUsd").is_none());
        assert_eq!(v["costMissing"], json!(["mock-model"]));
        assert!(v["tokensPerSuccess"].is_number());
    }

    #[test]
    fn no_runs_gives_zeros_and_no_ratios() {
        let v = agg(vec![], 5);
        assert_eq!(v["runs"], 0);
        assert!(v.get("successRate").is_none());
        assert!(v.get("tokensPerSuccess").is_none());
        assert!(v.get("effects").is_none());
        assert!(v["passK"].get("value").is_none());
    }

    #[test]
    fn parse_reads_the_stored_document_and_keeps_no_ids() {
        let doc = json!({
            "receipt": {
                "runId": "run-secret", "requestedBy": "user-secret",
                "state": "SUCCEEDED",
                "pins": {"agent": {"urn": "u", "version": "1.0.0", "digest": "sha256:abc"}},
                "model": [{"name": "m", "promptTokens": 7, "completionTokens": 3}],
                "metering": {"promptTokens": 7, "completionTokens": 3, "approvals": 1, "turns": 4},
                "effects": {"journaled": true, "duplicates": 1, "unknown": 2,
                    "counts": {"confirmed": 3, "reconciled": 1, "identicalRepeats": 5}}
            },
            "signature": {}
        })
        .to_string();
        let f = parse_receipt(&doc).unwrap();
        assert_eq!(f.digest, "abc");
        assert_eq!(f.turns, Some(4));
        assert_eq!(f.approvals, 1);
        assert_eq!(
            f.effects,
            Some(EffectFacts {
                confirmed: 4,
                unknown: 2,
                duplicates: 1,
                identical_repeats: 5
            })
        );
        let old = json!({"receipt": {"state": "FAILED",
            "pins": {"agent": {"version": "1.0.0", "digest": "x"}},
            "effects": {"journaled": false, "statement": "s"}}})
        .to_string();
        assert_eq!(parse_receipt(&old).unwrap().effects, None);
        assert!(parse_receipt("{}").is_none());
        assert!(parse_receipt("not json").is_none());
    }

    #[test]
    fn the_store_joins_receipts_on_the_agent_urn_and_keeps_the_newest_generation() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let doc = |state: &str, version: &str| {
            json!({"receipt": {"state": state,
                "pins": {"agent": {"version": version, "digest": "sha256:aa"}}}})
            .to_string()
        };
        {
            let c = store.connection.lock().unwrap();
            for (run, urn) in [("r1", "urn:a"), ("r2", "urn:a"), ("r3", "urn:b")] {
                c.execute(
                    "INSERT INTO runs (run_id, state, revision, agent_urn) VALUES (?1, 'SUCCEEDED', 1, ?2)",
                    params![run, urn],
                )
                .unwrap();
            }
            for (run, generation, state) in [
                ("r1", 1, "FAILED"),
                ("r1", 2, "SUCCEEDED"),
                ("r2", 1, "FAILED"),
                ("r3", 1, "SUCCEEDED"),
            ] {
                c.execute(
                    "INSERT INTO run_receipts (run_id, generation, document, digest, key_id, created_at) VALUES (?1, ?2, ?3, 'd', 'k', 0)",
                    params![run, generation, doc(state, "1.0.0")],
                )
                .unwrap();
            }
        }
        let facts = store.receipt_facts_for_agent("urn:a").unwrap();
        assert_eq!(facts.len(), 2);
        assert_eq!(facts.iter().filter(|f| f.state == "SUCCEEDED").count(), 1);
        assert_eq!(store.receipt_facts_for_agent("urn:b").unwrap().len(), 1);
        assert!(store.receipt_facts_for_agent("urn:c").unwrap().is_empty());
    }

    #[test]
    fn base_lookup_skips_drafts_and_picks_the_highest_release_below() {
        let v = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let all = v(&["1.0.0", "1.2.0", "1.10.0", "2.0.0-draft", "draft"]);
        assert_eq!(base_version("1.10.0", &all).as_deref(), Some("1.2.0"));
        assert_eq!(base_version("1.2.0", &all).as_deref(), Some("1.0.0"));
        assert_eq!(base_version("draft", &all).as_deref(), Some("1.10.0"));
        assert_eq!(base_version("1.0.0", &all), None);
        assert_eq!(base_version("draft", &v(&["3.0.0-draft"])), None);
    }

    #[test]
    fn changes_are_absent_when_a_side_lacks_the_value() {
        let base = json!({"successRate": 1.0, "passK": {"k": 2, "value": 0.5},
            "tokensPerSuccess": 100.0, "effects": {"duplicates": 1}});
        let cand = json!({"successRate": 0.5, "passK": {"k": 2, "note": "n"},
            "tokensPerSuccess": 80.0, "approvalsPerRun": 0.0, "effects": {"duplicates": 0}});
        let c = changes(&base, &cand);
        assert_eq!(c["successRate"]["delta"], -0.5);
        assert_eq!(c["tokensPerSuccess"]["delta"], -20.0);
        assert!(c.get("passK").is_none());
        assert!(c.get("approvalsPerRun").is_none());
        assert!(c.get("costPerSuccessMicroUsd").is_none());
        assert_eq!(c["duplicates"]["base"], 1);
        assert_eq!(c["duplicates"]["candidate"], 0);
        assert_eq!(c["duplicates"]["delta"], -1);
        let c = changes(&json!({}), &json!({"effects": {"duplicates": 2}}));
        assert_eq!(c["duplicates"]["candidate"], 2);
        assert!(c["duplicates"].get("base").is_none());
    }
}
