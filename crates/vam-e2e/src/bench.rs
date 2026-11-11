//! The folded benchmark lane (`vam-e2e run --bench`, H-0.9): k trials of each `model` scenario,
//! aggregated into `bench-results.json`. Benchmark decisions 3, 4, 5, 7: a versioned task set,
//! no scores from the mock, cost per success, the scenario runner is the lane.
//!
//! Row fields: `turns` is the receipt's iteration count (`metering.turns`), not the number of
//! inference calls (the summary's `usage.calls` is that). Per run it falls back to that run's call
//! count when its receipt has no turns, and `turnsPartial` says some run did. "Per success" fields divide the total over every trial by the successes.
//!
//! A *trial* is one run of a scenario; it passes when every `then` holds. Recorded replay
//! (`--recorded DIR`) aggregates saved trials with no vamd and no network.

use crate::usage::{
    endpoint_host, measure, price_source, provider_of, Measure, RunInput, MOCK_MODEL,
};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use vam_inference::ModelCatalog;

pub const RESULTS_FILE: &str = "bench-results.json";
pub const TASK_SETS_FILE: &str = "task-sets.yaml";
/// Where a published (real, live) result goes, relative to the repository root.
pub const PUBLISH_DIR: &str = "bench/results";

// ---- pinned task sets ------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSet {
    pub version: String,
    pub tasks: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskSets {
    pub sets: Vec<TaskSet>,
}

impl TaskSets {
    pub fn load(path: &Path) -> Result<TaskSets, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("task-set file {}: {e}", path.display()))?;
        serde_yaml::from_str(&text).map_err(|e| format!("task-set file {}: {e}", path.display()))
    }

    /// Is `task` in the pinned set `version`? The refusal names the task set.
    pub fn check(&self, version: &str, task: &str) -> Result<(), String> {
        let Some(set) = self.sets.iter().find(|s| s.version == version) else {
            let known: Vec<_> = self.sets.iter().map(|s| s.version.as_str()).collect();
            return Err(format!(
                "task-set version `{version}` is not a pinned set (pinned: {})",
                known.join(", ")
            ));
        };
        if set.tasks.iter().any(|t| t == task) {
            Ok(())
        } else {
            Err(format!(
                "task `{task}` is not in task-set version `{version}`"
            ))
        }
    }

    pub fn has_version(&self, version: &str) -> bool {
        self.sets.iter().any(|s| s.version == version)
    }
}

// ---- model gates -----------------------------------------------------------------------------

/// The real (non-mock) models of a bench selection.
pub fn real_models(models: &[String]) -> Vec<&String> {
    models.iter().filter(|m| m.as_str() != MOCK_MODEL).collect()
}

/// `--provider direct`: the model maker's own endpoint (the catalog entry).
pub const PROVIDER_DIRECT: &str = "direct";
/// `--provider vercel`: the Vercel AI Gateway, transport only. Cost is still the maker's list price.
pub const PROVIDER_VERCEL: &str = "vercel";

/// `--provider` is `direct` or `vercel`.
pub fn check_provider(provider: &str) -> Result<(), String> {
    match provider {
        PROVIDER_DIRECT | PROVIDER_VERCEL => Ok(()),
        other => Err(format!(
            "unknown --provider `{other}` (use `{PROVIDER_DIRECT}` or `{PROVIDER_VERCEL}`)"
        )),
    }
}

/// Refuses what cannot run before any node starts. Order: unknown model, `--real`, the lane.
/// A real model runs only on the microVM lane (E9 S5: a process-mode node serves the mock only).
/// The credential is never read here: the node answers for it (`check_node`).
pub fn check_models(
    models: &[String],
    real: bool,
    catalog: &ModelCatalog,
    firecracker: bool,
    provider: &str,
) -> Result<(), String> {
    check_provider(provider)?;
    for m in real_models(models) {
        if provider == PROVIDER_VERCEL {
            if catalog.gateway_entry(m).is_none() {
                return Err(format!(
                    "gateway model `{m}` has no maker list price in the model catalog, so its cost \
                     could not be reported (use an id like `anthropic/claude-haiku-5.5`)"
                ));
            }
        } else if catalog.get(m).is_none() {
            return Err(format!("model `{m}` is not in the model catalog"));
        }
        if !real {
            return Err(format!(
                "refusing real model `{m}` without --real (it spends money and needs the owner's go-ahead)"
            ));
        }
        if !firecracker {
            return Err(format!(
                "real model `{m}` cannot run on the process-mode node (it serves the mock model only); \
                 real models run on the microVM lane (pass --firecracker)"
            ));
        }
    }
    Ok(())
}

/// The node-side gates, after the node is up: it must report `guestMode: vm`, and its secret
/// store must hold each real model's credential. `secret_ids` are binding ids from the node's
/// secret metadata list (no value is ever read).
pub fn check_node(
    guest_mode: Option<&str>,
    secret_ids: &[String],
    models: &[String],
    catalog: &ModelCatalog,
    provider: &str,
) -> Result<(), String> {
    check_provider(provider)?;
    let real = real_models(models);
    if real.is_empty() {
        return Ok(());
    }
    if guest_mode != Some("vm") {
        return Err(format!(
            "the node reports guestMode: {}, not vm; real models run only on the microVM lane",
            guest_mode.unwrap_or("absent")
        ));
    }
    for m in real {
        let entry = if provider == PROVIDER_VERCEL {
            catalog.gateway_entry(m)
        } else {
            catalog.get(m).cloned()
        };
        let Some(entry) = entry else {
            return Err(format!("model `{m}` is not in the model catalog"));
        };
        if !secret_ids.iter().any(|id| id == &entry.secret_ref) {
            return Err(format!(
                "the node cannot serve `{m}`: no credential `{r}` in its secret store. Add it with \
                 `vam secrets set {r} --from-env {env} --scope org --host <node>` (see e2e/README.md)",
                r = entry.secret_ref,
                env = env_hint(&entry.provider),
            ));
        }
    }
    Ok(())
}

/// The shell variable the owner exports the key from for `vam secrets set --from-env`. A hint
/// in a message only: the runner never reads it.
fn env_hint(provider: &str) -> &'static str {
    match provider {
        "anthropic" => "ANTHROPIC_API_KEY",
        "openai" => "OPENAI_API_KEY",
        "openrouter" => "OPENROUTER_API_KEY",
        "vercel" => "VERCEL_AI_GATEWAY_KEY",
        _ => "PROVIDER_API_KEY",
    }
}

/// The built-in master key a throwaway node uses (a test constant, public in the source).
pub const TEST_MASTER_KEY_HEX: &str =
    "abababababababababababababababababababababababababababababababab";

/// The master key for a node with a persistent state dir: it must come from the environment and
/// must not be the built-in test constant. `value` is the environment variable's value.
pub fn persistent_master_key(value: Option<&str>) -> Result<String, String> {
    match value.map(str::trim).filter(|v| !v.is_empty()) {
        None => Err(
            "a persistent state dir needs VAMD_SECRETS_MASTER_KEY in the environment \
             (generate one: `openssl rand -hex 32`); the vault holds the provider key"
                .into(),
        ),
        Some(v) if v.eq_ignore_ascii_case(TEST_MASTER_KEY_HEX) => Err(
            "VAMD_SECRETS_MASTER_KEY is the built-in test constant; a persistent state dir needs \
             your own (`openssl rand -hex 32`)"
                .into(),
        ),
        Some(v) => Ok(v.to_string()),
    }
}

// ---- trials and rows -------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Trial {
    pub task: String,
    pub model: String,
    pub mode: String,
    pub pass: bool,
    /// Cut off by a step or scenario budget (still a failure, but not a model verdict).
    pub timed_out: bool,
    pub measure: Measure,
}

fn ratio(total: f64, successes: u64) -> Value {
    if successes == 0 {
        Value::Null
    } else {
        json!(total / successes as f64)
    }
}

/// One row per (task, model, mode). "Per success" divides the total over every trial, failed
/// ones included, by the number of successes (decision 5), and is `null` with no success.
pub fn rows(trials: &[Trial], catalog: &ModelCatalog) -> Vec<Value> {
    rows_asked(trials, catalog, None)
}

/// The harness record a run asked for: the `--profile` name (else `base`) resolved the way the
/// node does, so its version and digest are the ones vamd records. `None` for a name vam-spec
/// does not know.
pub fn asked_harness(profile: Option<&str>) -> Option<Value> {
    let r = vam_spec::harness::Harness::resolve(&json!({ "profile": profile.unwrap_or("base") }))
        .ok()?;
    vam_spec::harness::Harness::record(&r).ok()
}

/// As `rows`. A group's harness comes from its receipts. A group with no receipt takes what the
/// run asked for (`asked`), so the row key (task, model, mode, profile) never reads `unknown`.
/// When a receipt and the request name different profiles the row says `mixed` and sets
/// `harnessMismatch`.
pub fn rows_asked(trials: &[Trial], catalog: &ModelCatalog, asked: Option<&Value>) -> Vec<Value> {
    let mut groups: BTreeMap<(String, String, String), Vec<&Trial>> = BTreeMap::new();
    for t in trials {
        groups
            .entry((t.task.clone(), t.model.clone(), t.mode.clone()))
            .or_default()
            .push(t);
    }
    groups
        .into_iter()
        .map(|((task, model, mode), ts)| {
            let n = ts.len() as u64;
            // A trial with a duplicate effect or a lost task is red, whatever its checks said.
            let clean = |t: &Trial| {
                t.measure.duplicate_effects == 0
                    && t.measure.lost_tasks == 0
                    && !t.measure.effects_mismatch()
            };
            let s = ts.iter().filter(|t| t.pass && clean(t)).count() as u64;
            let rate = s as f64 / n as f64;
            let sum = |f: &dyn Fn(&Measure) -> f64| ts.iter().map(|t| f(&t.measure)).sum::<f64>();
            let cost_missing = ts.iter().any(|t| t.measure.usage.cost_missing);
            let cost = if cost_missing {
                Value::Null
            } else {
                ratio(sum(&|m| m.usage.spend_micro.unwrap_or(0) as f64), s)
            };
            // Internal only (never the published cost): the gateway's own charge, when every
            // call reported it.
            let gateway_cost = if ts
                .iter()
                .any(|t| t.measure.usage.gateway_cost_micro.is_none())
            {
                Value::Null
            } else {
                ratio(sum(&|m| m.usage.gateway_cost_micro.unwrap_or(0) as f64), s)
            };
            let (std_err, note) = if n < 2 {
                (Value::Null, json!("k < 2"))
            } else {
                (json!((rate * (1.0 - rate) / n as f64).sqrt()), Value::Null)
            };
            let hs: Vec<&Value> = ts
                .iter()
                .filter_map(|t| t.measure.harness.as_ref())
                .collect();
            let mut mismatch = false;
            let (h_profile, h_version, h_digest, h_overrides) = match (hs.is_empty(), asked) {
                (true, Some(a)) => (
                    a["profile"].clone(),
                    a["version"].clone(),
                    a["digest"].clone(),
                    if a["overrides"].is_null() {
                        json!({})
                    } else {
                        a["overrides"].clone()
                    },
                ),
                (false, Some(a)) => {
                    let from_receipt = harness_of(&hs);
                    if from_receipt.0 != a["profile"] {
                        mismatch = true;
                        (json!("mixed"), json!("mixed"), json!("mixed"), json!({}))
                    } else {
                        from_receipt
                    }
                }
                _ => harness_of(&hs),
            };
            // The receipt's own counts: "not journaled" when any receipt says so (never 0),
            // null when no run had a receipt.
            let receipt_count = |f: &dyn Fn(&Measure) -> u64| -> Value {
                if ts.iter().any(|t| t.measure.receipts_not_journaled > 0) {
                    json!("not journaled")
                } else if ts.iter().all(|t| t.measure.receipts_journaled == 0) {
                    Value::Null
                } else {
                    json!(ts.iter().map(|t| f(&t.measure)).sum::<u64>())
                }
            };
            json!({
                "task": task,
                "model": model,
                "mode": mode,
                "harnessProfile": h_profile,
                "harnessVersion": h_version,
                "harnessDigest": h_digest,
                "harnessOverrides": h_overrides,
                "harnessMismatch": mismatch,
                "endpoint": endpoint_host(catalog, &model),
                "provider": provider_of(catalog, &model),
                "priceSource": price_source(catalog, &model),
                "k": n,
                "successes": s,
                "budgetTimeouts": ts.iter().filter(|t| t.timed_out).count() as u64,
                "passK": s == n,
                "passAtK": s > 0,
                "passRate": rate,
                "stdErr": std_err,
                "stdErrNote": note,
                "turns": ratio(sum(&|m| m.usage.turns as f64), s),
                "turnsPartial": ts.iter().any(|t| t.measure.usage.turns_partial),
                "tokens": ratio(sum(&|m| m.tokens() as f64), s),
                "costPerSuccess": cost,
                "costMissing": cost_missing,
                "gatewayCostPerSuccess": gateway_cost,
                "wallMsPerSuccess": ratio(sum(&|m| m.usage.wall_ms as f64), s),
                "invalidCalls": sum(&|m| m.invalid as f64) as u64,
                "resamples": sum(&|m| m.resamples as f64) as u64,
                "repairsPerSuccess": ratio(sum(&|m| m.repairs as f64), s),
                "loopDetections": sum(&|m| m.loop_detections() as f64) as u64,
                "duplicateEffects": sum(&|m| m.duplicate_effects as f64) as u64,
                "lostTasks": sum(&|m| m.lost_tasks as f64) as u64,
                "receiptDuplicateEffects": receipt_count(&|m| m.receipt_duplicates),
                "receiptUnknownEffects": receipt_count(&|m| m.receipt_unknown),
                "effectsMismatch": ts.iter().any(|t| t.measure.effects_mismatch()),
            })
        })
        .collect()
}

/// How a results file was produced; decides whether it may ever be published.
#[derive(Debug, Clone, Copy, Default)]
pub struct Provenance {
    pub mock: bool,
    pub recorded: bool,
    pub synthetic: bool,
}

impl Provenance {
    /// Only a live run of a real model is publishable. Mock, recorded and synthetic never are.
    pub fn publishable(&self) -> bool {
        !self.mock && !self.recorded && !self.synthetic
    }
}

pub struct ResultsInput<'a> {
    pub task_set_version: &'a str,
    pub k: u64,
    pub provenance: Provenance,
    pub run_tag: &'a str,
    /// The step and scenario budgets in force, in milliseconds.
    pub step_budget_ms: u64,
    pub scenario_budget_ms: u64,
    /// The harness record the run asked for (`asked_harness`), for rows with no receipt.
    pub asked: Option<Value>,
}

/// The one harness record the receipts agree on, else `mixed`; `unknown` with no receipt.
fn harness_of(hs: &[&Value]) -> (Value, Value, Value, Value) {
    let agree = hs.windows(2).all(|w| w[0] == w[1]);
    match hs.first() {
        Some(h) if agree => (
            h["profile"].clone(),
            h["version"].clone(),
            h["digest"].clone(),
            h["overrides"].clone(),
        ),
        Some(_) => (json!("mixed"), json!("mixed"), json!("mixed"), json!({})),
        None => (
            json!("unknown"),
            json!("unknown"),
            json!("unknown"),
            json!({}),
        ),
    }
}

/// The whole results file. Harness fields come from the trials' receipts: the one value they
/// agree on, else `mixed`.
pub fn results(trials: &[Trial], inp: &ResultsInput, catalog: &ModelCatalog) -> Value {
    let hs: Vec<&Value> = trials
        .iter()
        .filter_map(|t| t.measure.harness.as_ref())
        .collect();
    let (profile, version, digest, overrides) = harness_of(&hs);
    let mut hosts: Vec<String> = trials
        .iter()
        .map(|t| endpoint_host(catalog, &t.model))
        .collect();
    hosts.sort();
    hosts.dedup();
    let p = inp.provenance;
    json!({
        "schema": 1,
        "runTag": inp.run_tag,
        "taskSetVersion": inp.task_set_version,
        "harnessProfile": profile,
        "harnessVersion": version,
        "harnessDigest": digest,
        "harnessOverrides": overrides,
        "k": inp.k,
        "stepBudgetMs": inp.step_budget_ms,
        "scenarioBudgetMs": inp.scenario_budget_ms,
        "endpoint": hosts.join(","),
        "mock": p.mock,
        "recorded": p.recorded,
        "synthetic": p.synthetic,
        "publishable": p.publishable(),
        "rows": rows_asked(trials, catalog, inp.asked.as_ref()).into_iter().map(|mut r| {
            r["runTag"] = json!(inp.run_tag);
            r["mock"] = json!(p.mock);
            r["recorded"] = json!(p.recorded);
            r["synthetic"] = json!(p.synthetic);
            r
        }).collect::<Vec<_>>(),
    })
}

/// Where a result may be published to, relative to `repo`: only a publishable one.
pub fn publish_target(res: &Value, repo: &Path) -> Option<PathBuf> {
    if !res["stoppedAt"].is_null() {
        return None;
    }
    if res["publishable"].as_bool() != Some(true)
        || res["mock"].as_bool() != Some(false)
        || res["recorded"].as_bool() != Some(false)
        || res["synthetic"].as_bool() != Some(false)
    {
        return None;
    }
    let v = res["taskSetVersion"].as_str()?;
    Some(repo.join(PUBLISH_DIR).join(format!("{v}.json")))
}

/// The run-level fields that describe one run.
const RUN_FIELDS: [&str; 9] = [
    "runTag",
    "harnessProfile",
    "harnessVersion",
    "harnessDigest",
    "harnessOverrides",
    "k",
    "stepBudgetMs",
    "scenarioBudgetMs",
    "endpoint",
];

fn run_of(file: &Value) -> Value {
    let mut r = serde_json::Map::new();
    for f in RUN_FIELDS {
        r.insert(f.into(), file[f].clone());
    }
    Value::Object(r)
}

type RowKey = (String, String, String, String);

fn row_key(row: &Value, fallback_profile: &Value) -> RowKey {
    let g = |k: &str| row[k].as_str().unwrap_or_default().to_string();
    let profile = if row["harnessProfile"].is_null() {
        fallback_profile.as_str().unwrap_or_default().to_string()
    } else {
        g("harnessProfile")
    };
    (g("task"), g("model"), g("mode"), profile)
}

/// Merges a new publishable result into the published file of the same task-set version
/// (schema 2). Rows are keyed by (task, model, mode, harness profile). An existing row is kept
/// untouched unless the new result has the same key, which replaces that row in place; new keys
/// come after, in key order. A schema 1 file is one run: its rows (which carry no profile or run
/// tag) take the file's `harnessProfile` as the key. `runs` lists the runs that still own a row.
/// The per-run top-level fields are those of the one run left, else `mixed`.
pub fn merge_results(old: &Value, new: &Value) -> Value {
    let old_profile = old["harnessProfile"].clone();
    let mut runs: Vec<Value> = match old["runs"].as_array() {
        Some(r) => r.clone(),
        None => {
            let mut r = run_of(old);
            r["unmarkedRows"] = json!(true);
            vec![r]
        }
    };
    let mut rows: Vec<Value> = old["rows"].as_array().cloned().unwrap_or_default();
    let mut fresh: BTreeMap<RowKey, Value> = new["rows"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|r| (row_key(r, &new["harnessProfile"]), r.clone()))
        .collect();
    for r in rows.iter_mut() {
        if let Some(n) = fresh.remove(&row_key(r, &old_profile)) {
            *r = n;
        }
    }
    rows.extend(fresh.into_values());
    runs.retain(|run| {
        rows.iter().any(|r| match r["runTag"].as_str() {
            Some(t) => run["runTag"].as_str() == Some(t),
            None => run["unmarkedRows"] == json!(true),
        })
    });
    let new_tag = new["runTag"].as_str();
    if !runs.iter().any(|r| r["runTag"].as_str() == new_tag) {
        runs.push(run_of(new));
    }
    let mut out = old.clone();
    out["schema"] = json!(2);
    if runs.len() == 1 {
        for f in RUN_FIELDS {
            out[f] = runs[0][f].clone();
        }
    } else {
        for f in RUN_FIELDS {
            out[f] = json!("mixed");
        }
        out["harnessOverrides"] = json!({});
        let mut hosts: Vec<&str> = runs
            .iter()
            .flat_map(|r| r["endpoint"].as_str().unwrap_or_default().split(','))
            .filter(|h| !h.is_empty())
            .collect();
        hosts.sort();
        hosts.dedup();
        out["endpoint"] = json!(hosts.join(","));
    }
    out["runs"] = Value::Array(runs);
    out["rows"] = Value::Array(rows);
    out
}

/// What stops a bench run that has a `--max-usd` cap.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapStop {
    /// `max-usd` (the sum passed the cap) or `cost-missing` (a trial's cost is unknown).
    pub reason: &'static str,
    pub spend_micro: i64,
}

/// The cost of one trial for the cap, in micro-USD: the row's own cost (the maker's list price).
/// The mock has no price, so it counts a notional 1 micro-USD per token (for testing the cap
/// only; never written to a row). `None` when a real trial made calls and has no cost.
pub fn cap_cost_micro(m: &Measure) -> Option<i64> {
    if m.usage.estimated {
        return Some(m.tokens());
    }
    if m.usage.cost_missing {
        return None;
    }
    Some(m.usage.spend_micro.unwrap_or(0))
}

/// Adds a finished trial to the spend so far. `Err` says the run must start no further trial.
pub fn cap_check(spent: i64, trial: Option<i64>, cap_micro: i64) -> Result<i64, CapStop> {
    match trial {
        None => Err(CapStop {
            reason: "cost-missing",
            spend_micro: spent,
        }),
        Some(c) => {
            let total = spent.saturating_add(c);
            if total > cap_micro {
                Err(CapStop {
                    reason: "max-usd",
                    spend_micro: total,
                })
            } else {
                Ok(total)
            }
        }
    }
}

/// `--max-usd` as micro-USD: finite and above zero.
pub fn parse_max_usd(v: f64) -> Result<i64, String> {
    if !v.is_finite() || v <= 0.0 {
        return Err(format!("--max-usd must be above 0 (got {v})"));
    }
    Ok((v * 1_000_000.0).round() as i64)
}

/// Refuses anything that must never be published: mock, recorded, synthetic, stopped at a cap,
/// or not marked publishable.
pub fn check_mergeable(res: &Value) -> Result<(), String> {
    let flag = |k: &str| res[k].as_bool();
    if flag("mock") != Some(false)
        || flag("recorded") != Some(false)
        || flag("synthetic") != Some(false)
        || flag("publishable") != Some(true)
    {
        return Err("not publishable (mock, recorded or synthetic): refusing to merge".into());
    }
    if !res["stoppedAt"].is_null() {
        return Err(format!(
            "the run stopped at its cost cap ({}): a partial result is not merged",
            res["stoppedAt"]
        ));
    }
    if res["taskSetVersion"].as_str().is_none() {
        return Err("no taskSetVersion in the result".into());
    }
    Ok(())
}

/// Merges the result file `file` into `into` (default `<repo>/bench/results/<version>.json`)
/// and writes it. Returns one line per row added or replaced. A missing target is created.
pub fn merge_file(file: &Path, into: Option<&Path>, repo: &Path) -> Result<Vec<String>, String> {
    let text = std::fs::read(file).map_err(|e| format!("{}: {e}", file.display()))?;
    let new: Value =
        serde_json::from_slice(&text).map_err(|e| format!("{}: not JSON ({e})", file.display()))?;
    check_mergeable(&new)?;
    let version = new["taskSetVersion"].as_str().unwrap_or_default();
    let target = match into {
        Some(p) => p.to_path_buf(),
        None => repo.join(PUBLISH_DIR).join(format!("{version}.json")),
    };
    let old: Option<Value> = match std::fs::read(&target) {
        Ok(b) => Some(
            serde_json::from_slice(&b)
                .map_err(|e| format!("{}: not JSON, not overwritten ({e})", target.display()))?,
        ),
        Err(_) => None,
    };
    let (out, report) = match &old {
        Some(old) => {
            if old["taskSetVersion"] != new["taskSetVersion"] {
                return Err(format!(
                    "{} is task-set {}, the result is {}",
                    target.display(),
                    old["taskSetVersion"],
                    new["taskSetVersion"]
                ));
            }
            let old_profile = old["harnessProfile"].clone();
            let existing: Vec<RowKey> = old["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| row_key(r, &old_profile))
                .collect();
            let report = new["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| {
                    let k = row_key(r, &new["harnessProfile"]);
                    let what = if existing.contains(&k) {
                        "replaced"
                    } else {
                        "added"
                    };
                    format!("{what} {} / {} / {} / {}", k.0, k.1, k.2, k.3)
                })
                .collect::<Vec<_>>();
            (merge_results(old, &new), report)
        }
        None => {
            let report = new["rows"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|r| {
                    let k = row_key(r, &new["harnessProfile"]);
                    format!("added {} / {} / {} / {}", k.0, k.1, k.2, k.3)
                })
                .collect();
            (new.clone(), report)
        }
    };
    if let Some(d) = target.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?;
    }
    let body = serde_json::to_vec_pretty(&out).map_err(|e| e.to_string())?;
    std::fs::write(&target, body).map_err(|e| format!("{}: {e}", target.display()))?;
    Ok(report)
}

/// Writes `<out>/bench-results.json`, and the published copy when the result may be published
/// (`repo` is the repository root). Returns the files written.
pub fn write_results(res: &Value, out: &Path, repo: &Path) -> std::io::Result<Vec<PathBuf>> {
    let body = serde_json::to_vec_pretty(res).unwrap_or_default();
    let mut written = Vec::new();
    std::fs::create_dir_all(out)?;
    let f = out.join(RESULTS_FILE);
    std::fs::write(&f, &body)?;
    written.push(f);
    if let Some(p) = publish_target(res, repo) {
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d)?;
        }
        let merged = match std::fs::read(&p) {
            Ok(old) => match serde_json::from_slice::<Value>(&old) {
                Ok(old) => serde_json::to_vec_pretty(&merge_results(&old, res)).unwrap_or(body),
                Err(e) => {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("{}: not JSON, not overwritten ({e})", p.display()),
                    ))
                }
            },
            Err(_) => body,
        };
        std::fs::write(&p, &merged)?;
        written.push(p);
    }
    Ok(written)
}

// ---- recorded replay -------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordedFile {
    task_set_version: String,
    trials: Vec<RecordedTrial>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecordedTrial {
    task: String,
    model: String,
    #[serde(default = "on")]
    mode: String,
    pass: bool,
    #[serde(default)]
    timed_out: bool,
    #[serde(default)]
    wall_ms: u64,
    #[serde(default)]
    error: String,
    #[serde(default)]
    events: Vec<Value>,
    receipt: Option<Value>,
    thread: Option<Value>,
}

fn on() -> String {
    "on".into()
}

/// Replays `<dir>/trials.json` (task sets from `task_sets`) into a results value. A recorded
/// result is always `recorded: true, synthetic: true` (the fixtures are hand-built, never captured
/// from a paid run) and never publishable.
pub fn replay(
    dir: &Path,
    task_sets: &TaskSets,
    want_version: Option<&str>,
    catalog: &ModelCatalog,
) -> Result<Value, String> {
    let path = dir.join("trials.json");
    let text = std::fs::read_to_string(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let rec: RecordedFile =
        serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    let version = want_version.unwrap_or(&rec.task_set_version);
    if !task_sets.has_version(version) {
        return Err(format!("task-set version `{version}` is not a pinned set"));
    }
    for t in &rec.trials {
        task_sets.check(version, &t.task)?;
    }
    if version != rec.task_set_version {
        return Err(format!(
            "task-set version `{version}` does not match the recorded trials (`{}`)",
            rec.task_set_version
        ));
    }
    let mut counts: BTreeMap<(&str, &str), u64> = BTreeMap::new();
    let trials: Vec<Trial> = rec
        .trials
        .iter()
        .map(|t| {
            *counts.entry((&t.task, &t.model)).or_default() += 1;
            let run = RunInput {
                events: &t.events,
                error: &t.error,
                receipt: t.receipt.as_ref(),
                thread: t.thread.as_ref(),
                thread_key: "",
            };
            Trial {
                task: t.task.clone(),
                model: t.model.clone(),
                mode: t.mode.clone(),
                pass: t.pass,
                timed_out: t.timed_out,
                measure: measure(&[run], t.wall_ms as u128, catalog),
            }
        })
        .collect();
    let k = counts.values().copied().max().unwrap_or(0);
    Ok(results(
        &trials,
        &ResultsInput {
            task_set_version: version,
            k,
            provenance: Provenance {
                mock: false,
                recorded: true,
                synthetic: true,
            },
            run_tag: "recorded",
            step_budget_ms: 0,
            scenario_budget_ms: 0,
            asked: None,
        },
        catalog,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sets() -> TaskSets {
        serde_yaml::from_str("sets:\n  - version: '0.1.0'\n    tasks: [a, b]\n").unwrap()
    }

    fn trial(task: &str, model: &str, pass: bool, spend: Option<i64>, turns: u64) -> Trial {
        let mut m = Measure::default();
        m.usage.calls = 1;
        m.usage.prompt_tokens = 100;
        m.usage.completion_tokens = 20;
        m.usage.turns = turns;
        m.usage.wall_ms = 1000;
        m.usage.spend_micro = spend;
        m.usage.cost_missing = spend.is_none();
        Trial {
            task: task.into(),
            model: model.into(),
            mode: "on".into(),
            pass,
            timed_out: false,
            measure: m,
        }
    }

    #[test]
    fn the_task_set_refuses_an_unpinned_version_and_task() {
        let s = sets();
        assert!(s.check("0.1.0", "a").is_ok());
        let e = s.check("0.0.0-not-a-set", "a").unwrap_err();
        assert!(e.contains("task-set") && e.contains("0.1.0"), "{e}");
        assert!(s.check("0.1.0", "z").unwrap_err().contains("task-set"));
    }

    #[test]
    fn per_success_divides_every_trial_by_the_successes() {
        let cat = ModelCatalog::new();
        let ts = vec![
            trial("a", "claude-haiku-5-5", true, Some(100), 2),
            trial("a", "claude-haiku-5-5", false, Some(300), 6),
            trial("a", "claude-haiku-5-5", true, Some(100), 2),
        ];
        let r = &rows(&ts, &cat)[0];
        assert_eq!(r["k"], 3);
        assert_eq!(r["passK"], false);
        assert_eq!(r["passAtK"], true);
        assert_eq!(r["costPerSuccess"], 250.0);
        assert_eq!(r["turns"], 5.0);
        assert_eq!(r["tokens"], 180.0);
        let se = r["stdErr"].as_f64().unwrap();
        assert!((se - (2.0f64 / 3.0 * (1.0 / 3.0) / 3.0).sqrt()).abs() < 1e-9);
        assert!(r["stdErrNote"].is_null());
    }

    #[test]
    fn the_standard_error_is_null_with_a_reason_below_two_trials() {
        let cat = ModelCatalog::new();
        let r = &rows(&[trial("a", "claude-haiku-5-5", true, Some(1), 1)], &cat)[0];
        assert!(r["stdErr"].is_null());
        assert_eq!(r["stdErrNote"], "k < 2");
    }

    #[test]
    fn no_success_means_no_per_success_numbers_and_missing_cost_is_flagged() {
        let cat = ModelCatalog::new();
        let r = &rows(
            &[
                trial("a", "claude-haiku-5-5", false, Some(1), 1),
                trial("a", "claude-haiku-5-5", false, Some(1), 1),
            ],
            &cat,
        )[0];
        assert_eq!(r["passAtK"], false);
        assert!(r["costPerSuccess"].is_null() && r["turns"].is_null());
        let r = &rows(&[trial("a", "mock-model", true, None, 1)], &cat)[0];
        assert!(r["costPerSuccess"].is_null());
        assert_eq!(r["costMissing"], true);
        assert!(r["tokens"].is_number());
    }

    #[test]
    fn mock_recorded_and_synthetic_results_are_never_published() {
        let cat = ModelCatalog::new();
        let ts = vec![trial("a", "claude-haiku-5-5", true, Some(1), 1)];
        let tmp = tempfile::tempdir().unwrap();
        for prov in [
            Provenance {
                mock: true,
                ..Default::default()
            },
            Provenance {
                recorded: true,
                ..Default::default()
            },
            Provenance {
                synthetic: true,
                ..Default::default()
            },
            Provenance {
                mock: true,
                recorded: true,
                synthetic: true,
            },
        ] {
            let res = results(
                &ts,
                &ResultsInput {
                    task_set_version: "0.1.0",
                    k: 1,
                    provenance: prov,
                    run_tag: "t",
                    step_budget_ms: 30_000,
                    scenario_budget_ms: 120_000,
                    asked: None,
                },
                &cat,
            );
            assert_eq!(res["publishable"], false);
            assert!(publish_target(&res, tmp.path()).is_none());
            let out = tmp.path().join("out");
            let written = write_results(&res, &out, tmp.path()).unwrap();
            assert_eq!(written, vec![out.join(RESULTS_FILE)]);
            assert!(!tmp.path().join(PUBLISH_DIR).exists());
        }
        // A live real run is the only one that publishes.
        let live = results(
            &ts,
            &ResultsInput {
                task_set_version: "0.1.0",
                k: 1,
                provenance: Provenance::default(),
                run_tag: "t",
                step_budget_ms: 30_000,
                scenario_budget_ms: 120_000,
                asked: None,
            },
            &cat,
        );
        assert_eq!(
            publish_target(&live, tmp.path()),
            Some(tmp.path().join("bench/results/0.1.0.json"))
        );
    }

    #[test]
    fn real_models_are_refused_in_order() {
        let cat = ModelCatalog::new();
        let h = vec!["claude-haiku-5-5".to_string()];
        assert!(check_models(&[MOCK_MODEL.into()], false, &cat, false, "direct").is_ok());
        let e = check_models(&["nope".into()], false, &cat, false, "direct").unwrap_err();
        assert!(e.contains("catalog"), "{e}");
        let e = check_models(&h, false, &cat, true, "direct").unwrap_err();
        assert!(e.contains("--real"), "{e}");
        let e = check_models(&h, true, &cat, false, "direct").unwrap_err();
        assert!(
            e.contains("process-mode") && e.contains("--firecracker"),
            "{e}"
        );
        assert!(check_models(&h, true, &cat, true, "direct").is_ok());
    }

    #[test]
    fn node_gates_check_mode_then_credential_and_print_no_secret() {
        let cat = ModelCatalog::new();
        let h = vec!["claude-haiku-5-5".to_string()];
        let have = vec!["anthropic/api-key".to_string()];
        let none: Vec<String> = Vec::new();
        assert!(check_node(Some("process"), &none, &[MOCK_MODEL.into()], &cat, "direct").is_ok());
        let e = check_node(Some("process"), &have, &h, &cat, "direct").unwrap_err();
        assert!(
            e.contains("guestMode: process") && e.contains("microVM"),
            "{e}"
        );
        let e = check_node(None, &have, &h, &cat, "direct").unwrap_err();
        assert!(e.contains("absent"), "{e}");
        let e = check_node(Some("vm"), &none, &h, &cat, "direct").unwrap_err();
        assert!(
            e.contains("anthropic/api-key")
                && e.contains("vam secrets set")
                && e.contains("--scope org"),
            "{e}"
        );
        // Another provider's key does not count.
        let other = vec!["openai/api-key".to_string()];
        assert!(check_node(Some("vm"), &other, &h, &cat, "direct").is_err());
        assert!(check_node(Some("vm"), &have, &h, &cat, "direct").is_ok());
        // A canary in the environment is never read or printed.
        std::env::set_var("ANTHROPIC_API_KEY", "sk-canary-must-not-appear");
        let e = check_node(Some("vm"), &none, &h, &cat, "direct").unwrap_err();
        assert!(!e.contains("sk-canary"), "{e}");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    #[test]
    fn the_gateway_gate_accepts_any_maker_with_a_direct_entry() {
        // A test-only maker entry, not a real model or price.
        let mut cat = ModelCatalog::new();
        let id = vec!["zai/fixture-glm".to_string()];
        assert!(check_models(&id, true, &cat, true, "vercel").is_err());
        cat.add_entry(vam_inference::ModelCatalogEntry {
            provider: "zai".into(),
            model_name: "fixture-glm".into(),
            price_prompt_micro: 1_000_000,
            price_completion_micro: 3_000_000,
            context_window: 200_000,
            supports_tools: true,
            ..Default::default()
        });
        assert!(check_models(&id, true, &cat, true, "vercel").is_ok());
    }

    #[test]
    fn the_gateway_gate_accepts_the_real_open_model_ids() {
        let cat = ModelCatalog::new();
        let ids = vec![
            "zai/glm-5".to_string(),
            "alibaba/qwen3.6-27b".to_string(),
            "deepseek/deepseek-v4-pro".to_string(),
        ];
        assert!(check_models(&ids, true, &cat, true, "vercel").is_ok());
    }

    #[test]
    fn the_gateway_gate_checks_the_vercel_key_and_a_maker_price() {
        let cat = ModelCatalog::new();
        let g = vec!["anthropic/claude-haiku-5.5".to_string()];
        assert!(check_models(&g, true, &cat, true, "vercel").is_ok());
        // A gateway id no maker entry prices would report no real cost: refused.
        let e = check_models(&["qwen/qwen3-max".into()], true, &cat, true, "vercel").unwrap_err();
        assert!(e.contains("list price"), "{e}");
        assert!(check_models(&g, true, &cat, true, "direct").is_err());
        assert!(check_models(&g, true, &cat, true, "bogus").is_err());
        let vercel = vec!["vercel/api-key".to_string()];
        let anthropic = vec!["anthropic/api-key".to_string()];
        assert!(check_node(Some("vm"), &vercel, &g, &cat, "vercel").is_ok());
        let e = check_node(Some("vm"), &anthropic, &g, &cat, "vercel").unwrap_err();
        assert!(
            e.contains("vercel/api-key") && e.contains("VERCEL_AI_GATEWAY_KEY"),
            "{e}"
        );
        // A canary in the environment is never read or printed.
        std::env::set_var("VERCEL_AI_GATEWAY_KEY", "sk-canary-must-not-appear");
        let e = check_node(Some("vm"), &anthropic, &g, &cat, "vercel").unwrap_err();
        assert!(!e.contains("sk-canary"), "{e}");
        std::env::remove_var("VERCEL_AI_GATEWAY_KEY");
    }

    #[test]
    fn a_ledger_of_two_is_a_duplicate_and_turns_the_row_red() {
        let cat = ModelCatalog::new();
        let ledger = vec![(
            "echo_create_ticket".to_string(),
            "Q4 plan".to_string(),
            2u64,
        )];
        let mut t = trial("a", "mock-model", true, Some(0), 1);
        t.measure.duplicate_effects = crate::usage::duplicate_effects(&ledger);
        let ok = trial("a", "mock-model", true, Some(0), 1);
        let clean = rows(std::slice::from_ref(&ok), &cat);
        assert_eq!(clean[0]["duplicateEffects"], 0);
        assert_eq!(clean[0]["lostTasks"], 0);
        assert_eq!(clean[0]["passK"], true);
        let dup = rows(&[t], &cat);
        assert_eq!(dup[0]["duplicateEffects"], 1);
        assert_eq!(dup[0]["successes"], 0);
        assert_eq!(dup[0]["passK"], false);
        let mut lost = ok;
        lost.measure.lost_tasks = 1;
        assert_eq!(rows(&[lost], &cat)[0]["passK"], false);
    }

    fn journaled_receipt(duplicates: u64, unknown: u64, repeats: u64) -> Value {
        json!({"receipt": {"effects": {
            "journaled": true, "duplicates": duplicates, "unknown": unknown,
            "counts": {"identicalRepeats": repeats}, "lines": []
        }}})
    }

    fn trial_with_receipt(receipt: &Value, ledger_extras: u64) -> Trial {
        let events: Vec<Value> = vec![];
        let run = RunInput {
            events: &events,
            error: "",
            receipt: Some(receipt),
            thread: None,
            thread_key: "",
        };
        let mut t = trial("a", "mock-model", true, Some(0), 1);
        t.measure = measure(&[run], 0, &ModelCatalog::new());
        t.measure.duplicate_effects = ledger_extras;
        t
    }

    #[test]
    fn a_clean_row_carries_both_receipt_counts_at_zero_and_no_mismatch() {
        let cat = ModelCatalog::new();
        let t = trial_with_receipt(&journaled_receipt(0, 0, 0), 0);
        let r = rows(&[t], &cat);
        assert_eq!(r[0]["receiptDuplicateEffects"], 0);
        assert_eq!(r[0]["receiptUnknownEffects"], 0);
        assert_eq!(r[0]["effectsMismatch"], false);
        assert_eq!(r[0]["passK"], true);
    }

    #[test]
    fn a_receipt_that_disagrees_with_the_ledger_is_a_red_row() {
        let cat = ModelCatalog::new();
        // The receipt counts one duplicate and the ledger none.
        let t = trial_with_receipt(&journaled_receipt(1, 0, 0), 0);
        let r = rows(&[t], &cat);
        assert_eq!(r[0]["receiptDuplicateEffects"], 1);
        assert_eq!(r[0]["effectsMismatch"], true);
        assert_eq!(r[0]["passK"], false);
        // The ledger counts one extra and the receipt says clean.
        let t = trial_with_receipt(&journaled_receipt(0, 0, 0), 1);
        assert_eq!(rows(&[t], &cat)[0]["effectsMismatch"], true);
        // Both count the same extra: they agree (the row is still red for the duplicate).
        let t = trial_with_receipt(&journaled_receipt(0, 0, 1), 1);
        let r = rows(&[t], &cat);
        assert_eq!(r[0]["effectsMismatch"], false);
        assert_eq!(r[0]["duplicateEffects"], 1);
        assert_eq!(r[0]["passK"], false);
        // Any unknown outcome is a mismatch.
        let t = trial_with_receipt(&journaled_receipt(0, 1, 0), 0);
        let r = rows(&[t], &cat);
        assert_eq!(r[0]["receiptUnknownEffects"], 1);
        assert_eq!(r[0]["effectsMismatch"], true);
    }

    #[test]
    fn an_unjournaled_or_missing_receipt_never_reads_as_zero() {
        let cat = ModelCatalog::new();
        let old = json!({"receipt": {"effects": {"journaled": false, "statement": "x"}}});
        let r = rows(&[trial_with_receipt(&old, 0)], &cat);
        assert_eq!(r[0]["receiptDuplicateEffects"], "not journaled");
        assert_eq!(r[0]["receiptUnknownEffects"], "not journaled");
        assert_eq!(r[0]["effectsMismatch"], false);
        let none = rows(&[trial("a", "mock-model", true, Some(0), 1)], &cat);
        assert!(none[0]["receiptDuplicateEffects"].is_null());
        assert_eq!(none[0]["effectsMismatch"], false);
    }

    #[test]
    fn lost_means_not_finished_or_ended_on_a_crash_reason() {
        use crate::usage::lost_task;
        assert!(lost_task("RUN_STATE_RUNNING", "", &[]));
        assert!(lost_task("RUN_STATE_QUEUED", "", &[]));
        assert!(lost_task("RUN_STATE_FAILED", "guest_crashed", &[]));
        assert!(lost_task("RUN_STATE_FAILED", "vamd_restarted", &[]));
        assert!(lost_task("RUN_STATE_FAILED", "crashed_repeatedly", &[]));
        assert!(lost_task("RUN_STATE_FAILED", "restarted_repeatedly", &[]));
        assert!(lost_task("RUN_STATE_FAILED", "not_resumable", &[]));
        assert!(!lost_task("RUN_STATE_FAILED", "some other error", &[]));
        assert!(!lost_task("RUN_STATE_EXPIRED", "", &[]));
        assert!(!lost_task("RUN_STATE_SUCCEEDED", "", &[]));
        assert!(!lost_task("RUN_STATE_PAUSED", "", &[]));
    }

    #[test]
    fn a_budget_timeout_is_counted_labelled_and_still_a_failure() {
        let cat = ModelCatalog::new();
        let mut cut = trial("a", "mock-model", false, Some(0), 1);
        cut.timed_out = true;
        let ts = vec![trial("a", "mock-model", true, Some(0), 1), cut];
        let res = results(
            &ts,
            &ResultsInput {
                task_set_version: "0.1.0",
                k: 2,
                provenance: Provenance::default(),
                run_tag: "t",
                step_budget_ms: 180_000,
                scenario_budget_ms: 600_000,
                asked: None,
            },
            &cat,
        );
        assert_eq!(res["stepBudgetMs"], 180_000);
        assert_eq!(res["scenarioBudgetMs"], 600_000);
        let row = &res["rows"][0];
        assert_eq!(row["budgetTimeouts"], 1);
        assert_eq!(row["successes"], 1);
        assert_eq!(row["passK"], false);
    }

    fn gateway_trial(pass: bool, spend: Option<i64>, gateway: Option<i64>) -> Trial {
        let mut t = trial("a", "anthropic/claude-haiku-5.5", pass, spend, 2);
        t.measure.usage.gateway_cost_micro = gateway;
        t
    }

    #[test]
    fn a_gateway_row_publishes_the_list_price_and_keeps_the_gateway_charge_apart() {
        let cat = ModelCatalog::new();
        // The gateway charged less (40 vs 100 per trial); the published cost is the list price.
        let ts = vec![
            gateway_trial(true, Some(100), Some(40)),
            gateway_trial(false, Some(100), Some(40)),
        ];
        let r = &rows(&ts, &cat)[0];
        assert_eq!(r["endpoint"], "ai-gateway.vercel.sh");
        assert_eq!(r["provider"], "vercel");
        assert_eq!(
            r["priceSource"],
            format!("anthropic list price, catalog {}", cat.catalog_version())
        );
        assert_eq!(r["costPerSuccess"], 200.0);
        assert_eq!(r["gatewayCostPerSuccess"], 80.0);
        assert_eq!(r["costMissing"], false);
        // No gateway charge reported: null, and the published cost is unchanged.
        let ts = vec![gateway_trial(true, Some(100), None)];
        let r = &rows(&ts, &cat)[0];
        assert!(r["gatewayCostPerSuccess"].is_null());
        assert_eq!(r["costPerSuccess"], 100.0);
        // A direct row names its maker as provider and the same price source.
        let d = &rows(&[trial("a", "claude-haiku-5-5", true, Some(100), 2)], &cat)[0];
        assert_eq!(d["provider"], "anthropic");
        assert_eq!(d["endpoint"], "api.anthropic.com");
        assert_eq!(d["priceSource"], r["priceSource"]);
    }

    #[test]
    fn a_persistent_node_needs_its_own_master_key() {
        assert!(persistent_master_key(None)
            .unwrap_err()
            .contains("VAMD_SECRETS_MASTER_KEY"));
        assert!(persistent_master_key(Some("  ")).is_err());
        assert!(persistent_master_key(Some(TEST_MASTER_KEY_HEX))
            .unwrap_err()
            .contains("built-in test constant"));
        assert!(persistent_master_key(Some(&TEST_MASTER_KEY_HEX.to_uppercase())).is_err());
        let own = "cd".repeat(32);
        assert_eq!(persistent_master_key(Some(&own)).unwrap(), own);
    }
    fn with_harness(mut t: Trial, profile: &str) -> Trial {
        t.measure.harness = Some(json!({
            "profile": profile, "version": "1", "digest": format!("sha256:{profile}"), "overrides": {}
        }));
        t
    }

    fn one_run(tag: &str, profile: &str, model: &str, task: &str) -> Value {
        let ts = vec![with_harness(trial(task, model, true, Some(5), 1), profile)];
        results(
            &ts,
            &ResultsInput {
                task_set_version: "0.1.0",
                k: 1,
                provenance: Provenance::default(),
                run_tag: tag,
                step_budget_ms: 1,
                scenario_budget_ms: 2,
                asked: None,
            },
            &ModelCatalog::new(),
        )
    }

    #[test]
    fn rows_carry_profile_version_digest_and_run_tag() {
        let r = one_run("t1", "open-small", "alibaba/qwen3.6-27b", "a");
        let row = &r["rows"][0];
        assert_eq!(row["harnessProfile"], "open-small");
        assert_eq!(row["harnessVersion"], "1");
        assert_eq!(row["harnessDigest"], "sha256:open-small");
        assert_eq!(row["runTag"], "t1");
    }

    fn rows_body(t: &str) -> String {
        let head = "\"rows\": [\n";
        let a = t.find(head).unwrap() + head.len();
        t[a..t.rfind("\n  ]").unwrap()].to_string()
    }

    fn n_rows(v: &Value) -> usize {
        v["rows"].as_array().unwrap().len()
    }

    #[test]
    fn merging_keeps_the_published_rows_byte_identical_and_replaces_only_the_same_key() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/results-0.1.0-55cb5b.json");
        let text = std::fs::read_to_string(path).unwrap();
        let old: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(old["runTag"], "55cb5b");
        let old_body = rows_body(&text);
        let glm = one_run("aaaaaa", "open-strong", "zai/glm-5", "first-try-tool-use");
        let merged = merge_results(&old, &glm);
        assert!(
            rows_body(&serde_json::to_string_pretty(&merged).unwrap()).starts_with(&old_body),
            "the 55cb5b rows changed"
        );
        assert_eq!(merged["schema"], 2);
        assert_eq!(n_rows(&merged), n_rows(&old) + 1);
        assert_eq!(merged["runs"].as_array().unwrap().len(), 2);
        assert_eq!(merged["runTag"], "mixed");
        assert_eq!(merged["harnessProfile"], "mixed");
        assert!(merged["endpoint"].as_str().unwrap().contains("ai-gateway"));
        // The same key again replaces that row only.
        let again = one_run("bbbbbb", "open-strong", "zai/glm-5", "first-try-tool-use");
        let m2 = merge_results(&merged, &again);
        assert_eq!(n_rows(&m2), n_rows(&merged));
        assert_eq!(
            m2["rows"].as_array().unwrap().last().unwrap()["runTag"],
            "bbbbbb"
        );
        assert!(rows_body(&serde_json::to_string_pretty(&m2).unwrap()).starts_with(&old_body));
        assert_eq!(
            m2["runs"].as_array().unwrap().len(),
            2,
            "aaaaaa owns no row now"
        );
        // A different profile for the same task and model is a separate row.
        let other = one_run("cccccc", "base", "zai/glm-5", "first-try-tool-use");
        assert_eq!(n_rows(&merge_results(&m2, &other)), n_rows(&m2) + 1);
        // A legacy row (no profile) is keyed on the file's profile: a base re-run replaces it.
        let hk = one_run(
            "dddddd",
            "base",
            "anthropic/claude-haiku-5.5",
            "first-try-tool-use",
        );
        let m4 = merge_results(&old, &hk);
        assert_eq!(n_rows(&m4), n_rows(&old));
    }

    #[test]
    fn the_live_published_file_still_holds_the_55cb5b_rows_unchanged() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR"));
        let fixture: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("tests/fixtures/results-0.1.0-55cb5b.json")).unwrap(),
        )
        .unwrap();
        let live: Value = serde_json::from_str(
            &std::fs::read_to_string(dir.join("../../bench/results/0.1.0.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(live["schema"], 2);
        assert!(live["runs"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["runTag"] == "55cb5b"));
        let fixture_rows = fixture["rows"].as_array().unwrap();
        assert_eq!(fixture_rows.len(), 8);
        let live_rows = live["rows"].as_array().unwrap();
        for (i, row) in fixture_rows.iter().enumerate() {
            assert_eq!(
                serde_json::to_string_pretty(row).unwrap(),
                serde_json::to_string_pretty(&live_rows[i]).unwrap(),
                "published row {i} changed"
            );
        }
    }

    #[test]
    fn a_stopped_result_is_never_published() {
        let mut r = one_run("t", "base", "m", "a");
        r["publishable"] = json!(true);
        assert!(publish_target(&r, Path::new(".")).is_some());
        r["stoppedAt"] = json!("max-usd");
        assert!(publish_target(&r, Path::new(".")).is_none());
    }

    #[test]
    fn the_cap_stops_before_the_next_trial() {
        let cap = parse_max_usd(0.0001).unwrap();
        assert_eq!(cap, 100);
        let (mut spent, mut ran) = (0, 0);
        for cost in [40, 40, 40, 40] {
            ran += 1;
            match cap_check(spent, Some(cost), cap) {
                Ok(t) => spent = t,
                Err(s) => {
                    assert_eq!((s.reason, s.spend_micro), ("max-usd", 120));
                    break;
                }
            }
        }
        assert_eq!(
            (ran, spent),
            (3, 80),
            "the trial that passes the cap is the last"
        );
        assert!(parse_max_usd(0.0).is_err() && parse_max_usd(f64::NAN).is_err());
    }

    #[test]
    fn exactly_the_cap_is_allowed_and_no_cap_means_no_check() {
        assert_eq!(cap_check(60, Some(40), 100), Ok(100));
        // No --max-usd: run_cmd never calls cap_check, so a missing cost changes nothing.
        let t = trial("a", "claude-haiku-5-5", true, None, 1);
        assert_eq!(cap_cost_micro(&t.measure), None);
    }

    #[test]
    fn a_missing_cost_stops_the_run_and_the_mock_counts_tokens() {
        let s = cap_check(10, None, 1_000_000).unwrap_err();
        assert_eq!((s.reason, s.spend_micro), ("cost-missing", 10));
        let mut m = Measure::default();
        m.usage.calls = 1;
        m.usage.prompt_tokens = 30;
        m.usage.completion_tokens = 12;
        m.usage.estimated = true;
        m.usage.cost_missing = true;
        assert_eq!(cap_cost_micro(&m), Some(42));
        let priced = trial("a", "claude-haiku-5-5", true, Some(7), 1);
        assert_eq!(cap_cost_micro(&priced.measure), Some(7));
    }

    #[test]
    fn the_gateway_gate_refuses_an_unpriced_id() {
        let cat = ModelCatalog::new();
        for id in ["zai/not-in-catalog", "deepseek/not-in-catalog"] {
            let e = check_models(&[id.to_string()], true, &cat, true, "vercel").unwrap_err();
            assert!(e.contains("no maker list price"), "{e}");
        }
    }
    #[test]
    fn a_receipt_less_row_takes_the_profile_the_run_asked_for() {
        let asked = |p: &str| asked_harness(Some(p)).unwrap();
        let run = |tag: &str, p: &str| {
            let ts = vec![trial("a", "zai/glm-5", true, Some(5), 1)];
            results(
                &ts,
                &ResultsInput {
                    task_set_version: "0.1.0",
                    k: 1,
                    provenance: Provenance::default(),
                    run_tag: tag,
                    step_budget_ms: 1,
                    scenario_budget_ms: 2,
                    asked: Some(asked(p)),
                },
                &ModelCatalog::new(),
            )
        };
        let (b, o) = (run("t1", "base"), run("t2", "open-strong"));
        let (rb, ro) = (&b["rows"][0], &o["rows"][0]);
        assert_eq!(rb["harnessProfile"], "base");
        assert_eq!(ro["harnessProfile"], "open-strong");
        assert_ne!(rb["harnessDigest"], ro["harnessDigest"]);
        assert_eq!(rb["harnessMismatch"], false);
        let m = merge_results(&b, &o);
        assert_eq!(n_rows(&m), 2, "two profiles are two rows, not one");
        // The unset flag means base, with the digest vamd records for it.
        assert_eq!(asked_harness(None).unwrap()["profile"], "base");
        assert!(asked_harness(Some("nope")).is_none());
    }

    #[test]
    fn a_receipt_that_disagrees_with_the_request_is_mixed_and_flagged() {
        let ts = vec![with_harness(
            trial("a", "m", true, Some(1), 1),
            "open-small",
        )];
        let r = rows_asked(
            &ts,
            &ModelCatalog::new(),
            asked_harness(Some("base")).as_ref(),
        );
        assert_eq!(r[0]["harnessProfile"], "mixed");
        assert_eq!(r[0]["harnessMismatch"], true);
        // Agreeing: the receipt stays the source.
        let r = rows_asked(
            &ts,
            &ModelCatalog::new(),
            asked_harness(Some("open-small")).as_ref(),
        );
        assert_eq!(r[0]["harnessDigest"], "sha256:open-small");
        assert_eq!(r[0]["harnessMismatch"], false);
    }

    #[test]
    fn bench_merge_keeps_the_published_rows_byte_identical_and_refuses_unpublishable() {
        let src =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/results-0.1.0-55cb5b.json");
        let dir = tempfile::tempdir().unwrap();
        let into = dir.path().join("0.1.0.json");
        std::fs::copy(&src, &into).unwrap();
        let old_text = std::fs::read_to_string(&into).unwrap();
        let mut glm = one_run("aaaaaa", "open-strong", "zai/glm-5", "first-try-tool-use");
        for k in ["mock", "recorded", "synthetic"] {
            glm[k] = json!(false);
        }
        glm["publishable"] = json!(true);
        let file = dir.path().join("bench-results.json");
        std::fs::write(&file, serde_json::to_vec_pretty(&glm).unwrap()).unwrap();
        let rep = merge_file(&file, Some(&into), dir.path()).unwrap();
        assert_eq!(rep.len(), 1);
        assert!(
            rep[0].starts_with("added first-try-tool-use / zai/glm-5"),
            "{rep:?}"
        );
        let new_text = std::fs::read_to_string(&into).unwrap();
        assert!(rows_body(&new_text).starts_with(&rows_body(&old_text)));
        // The same file again replaces its own row.
        let rep = merge_file(&file, Some(&into), dir.path()).unwrap();
        assert!(rep[0].starts_with("replaced "), "{rep:?}");
        // Unpublishable results are refused and the target is untouched.
        let before = std::fs::read(&into).unwrap();
        for (k, v) in [
            ("mock", json!(true)),
            ("recorded", json!(true)),
            ("synthetic", json!(true)),
            ("stoppedAt", json!("max-usd")),
        ] {
            let mut bad = glm.clone();
            bad[k] = v;
            std::fs::write(&file, serde_json::to_vec_pretty(&bad).unwrap()).unwrap();
            assert!(merge_file(&file, Some(&into), dir.path()).is_err(), "{k}");
        }
        assert_eq!(std::fs::read(&into).unwrap(), before);
        // With no --into the target is bench/results/<version>.json under the repo.
        std::fs::write(&file, serde_json::to_vec_pretty(&glm).unwrap()).unwrap();
        merge_file(&file, None, dir.path()).unwrap();
        assert!(dir.path().join("bench/results/0.1.0.json").is_file());
    }
}
