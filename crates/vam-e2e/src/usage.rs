//! What a run cost and how its tool calls went (H-0.8), read from what the API already
//! serves: `inference.completed` events, the receipt, and the thread messages. Pure functions
//! over JSON, so recorded runs are measured the same way as live ones.
//!
//! Counts come from the host-recorded thread by the stable prefixes in `vam_spec::call_check`:
//! a tool result starting `invalid arguments for ` is an invalid call, a user message starting
//! `Loop detected:` is a loop warning, a run error starting `loop_detected:` is a loop stop.

use serde_json::{json, Value};
use vam_inference::{ModelCatalog, ModelCatalogEntry};
use vam_spec::call_check::{INVALID_ARGS_PREFIX, LOOP_OBSERVATION_PREFIX, LOOP_STOP_PREFIX};

/// The model the process-mode node serves; its numbers are never scored.
pub const MOCK_MODEL: &str = "mock-model";

/// The reads of one run, borrowed.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunInput<'a> {
    /// Run events (`GET /v1/runs/{id}/events`).
    pub events: &'a [Value],
    /// The run's error text (empty when none).
    pub error: &'a str,
    /// `GET /v1/runs/{id}/receipt`, when it was issued.
    pub receipt: Option<&'a Value>,
    /// `GET /v1/workspaces/{w}/threads/{t}/messages?view=all`, when read.
    pub thread: Option<&'a Value>,
    /// Which thread `thread` is (`workspace/thread`). Chats that continue one thread share a
    /// key, and the thread then holds every earlier run's messages: it is counted once, from the
    /// last run that read it. Empty means the thread is this run's alone.
    pub thread_key: &'a str,
}

/// Owned receipt and thread of a run, kept next to the engine's run record.
#[derive(Debug, Clone, Default)]
pub struct RunExtra {
    pub receipt: Option<Value>,
    pub thread: Option<Value>,
}

/// The run's error: its status error when it has one, else the `error` of its `run.completed`
/// event (the status field is empty for a run the agent failed, `run.completed` carries it).
pub fn run_error(run: &RunInput) -> String {
    let e = run.error.trim().trim_matches('"');
    if !e.is_empty() && e != "null" {
        return e.to_string();
    }
    for ev in run
        .events
        .iter()
        .rev()
        .filter(|e| event_is(e, "run.completed"))
    {
        let raw = match &ev["payload"] {
            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
            other => other.clone(),
        };
        for p in [&raw, &raw["data"]] {
            if let Some(s) = p["error"].as_str().filter(|s| !s.is_empty()) {
                return s.to_string();
            }
        }
    }
    String::new()
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub calls: u64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub cached_tokens: i64,
    pub cache_creation_tokens: i64,
    /// Micro-USD. `None` when the model has no price and no event carried a spend.
    pub spend_micro: Option<i64>,
    /// Per run: the receipt's iteration count when it has one, else that run's inference calls.
    pub turns: u64,
    /// Some run had no receipt turns, so `turns` includes its call count instead.
    pub turns_partial: bool,
    pub wall_ms: u128,
    /// The numbers come from the mock model: tokens are estimates, nothing is scored.
    pub estimated: bool,
    pub cost_missing: bool,
    /// Internal only, never published as the cost: what the gateway charged, summed over the
    /// calls (`gatewayCostMicro` on `inference.completed`). `None` when any call lacked it.
    pub gateway_cost_micro: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Measure {
    pub usage: Usage,
    /// `{profile, version, digest, overrides}` of the first run that has one.
    pub harness: Option<Value>,
    /// Typed tool-call errors sent to the model.
    pub invalid: u64,
    /// Resamples made (`inference.completed` `data.resamples`). Each discarded the one invalid
    /// call the model made, so they are also counted in `invalid`; not counted as repairs.
    pub resamples: u64,
    /// Invalid calls after which the model went on to a later call that was not invalid.
    pub repairs: u64,
    pub loop_warnings: u64,
    pub loop_stops: u64,
    /// Catalog names of the models the runs used.
    pub models: Vec<String>,
    /// Creates in the world's ledger beyond one per intended effect (see `duplicate_effects`).
    pub duplicate_effects: u64,
    /// Runs that were lost (see `lost_task`).
    pub lost_tasks: u64,
    /// From the receipts' effects sections (RR-3.7): sums over runs whose receipt is journaled.
    pub receipt_duplicates: u64,
    pub receipt_unknown: u64,
    pub receipt_identical_repeats: u64,
    /// Runs whose receipt has a journaled effects section / the Phase 1 "not journaled" one.
    pub receipts_journaled: u64,
    pub receipts_not_journaled: u64,
}

impl Measure {
    /// The receipt's effects disagree with the world's ledger (RR-3.7): the ledger counted
    /// creates beyond one that the receipt's duplicates and identical repeats do not account
    /// for, or the receipt holds an unknown outcome. Never true for a run whose receipt is not
    /// journaled: that is "not journaled", not a count.
    pub fn effects_mismatch(&self) -> bool {
        self.receipts_journaled > 0
            && (self.duplicate_effects != self.receipt_duplicates + self.receipt_identical_repeats
                || self.receipt_unknown > 0)
    }

    pub fn tokens(&self) -> i64 {
        self.usage.prompt_tokens + self.usage.completion_tokens
    }

    pub fn loop_detections(&self) -> u64 {
        self.loop_warnings + self.loop_stops
    }

    /// The `summary.json` fields of one scenario.
    pub fn summary_fields(&self) -> Value {
        let u = &self.usage;
        json!({
            "usage": {
                "calls": u.calls,
                "promptTokens": u.prompt_tokens,
                "completionTokens": u.completion_tokens,
                "cachedTokens": u.cached_tokens,
                "cacheCreationTokens": u.cache_creation_tokens,
                "spendMicro": u.spend_micro,
                "turns": u.turns,
                "turnsPartial": u.turns_partial,
                "wallMs": u.wall_ms as u64,
                "estimated": u.estimated,
                "costMissing": u.cost_missing,
                "gatewayCostMicro": u.gateway_cost_micro,
            },
            "harness": self.harness.clone().unwrap_or(Value::Null),
            "toolCalls": { "invalid": self.invalid, "repairs": self.repairs, "resamples": self.resamples },
            "loopDetections": { "warnings": self.loop_warnings, "stops": self.loop_stops },
            "scored": false,
        })
    }
}

/// The payload of a run event, whether it is an object or a JSON string; a `data` wrapper is
/// looked through.
pub fn payload(ev: &Value) -> Value {
    let p = match &ev["payload"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
        other => other.clone(),
    };
    match p.get("data") {
        Some(d) if d.is_object() => d.clone(),
        _ => p,
    }
}

fn event_is(ev: &Value, name: &str) -> bool {
    crate::events::bare(ev["eventType"].as_str().unwrap_or_default()) == name
}

fn int(v: &Value) -> i64 {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .unwrap_or(0)
}

fn opt_int(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// The catalog entry that prices and routes a bench model: the direct entry, or, for a gateway
/// id (`anthropic/claude-haiku-5.5`), the gateway entry at the maker's list price. The gateway
/// is transport only; it is never the price source.
pub fn bench_entry(catalog: &ModelCatalog, model: &str) -> Option<ModelCatalogEntry> {
    catalog.list_price_entry(model)
}

/// The provider a bench model is served through (`anthropic`, `vercel`, `mock`, ...).
pub fn provider_of(catalog: &ModelCatalog, model: &str) -> String {
    if model == MOCK_MODEL {
        return "mock".into();
    }
    bench_entry(catalog, model)
        .map(|e| e.provider)
        .unwrap_or_else(|| "unknown".into())
}

/// Where the model's price comes from, `None` for the mock or an unpriced model.
pub fn price_source(catalog: &ModelCatalog, model: &str) -> Option<String> {
    catalog.list_price_source(model)
}

/// The micro-USD price of `prompt` / `completion` tokens for a catalog model, `None` when the
/// model is unknown or free (the mock). Cache tokens are charged at the input price, as the
/// catalog's own estimate does.
pub fn estimate_micro(
    catalog: &ModelCatalog,
    model: &str,
    prompt: i64,
    completion: i64,
) -> Option<i64> {
    catalog.estimate_list_micro(model, prompt, completion)
}

/// The host of a catalog model's endpoint (`api.anthropic.com`); `mock` for the mock model;
/// `unknown` for a name the catalog lacks. Never a key or a path.
pub fn endpoint_host(catalog: &ModelCatalog, model: &str) -> String {
    if model == MOCK_MODEL {
        return "mock".into();
    }
    match bench_entry(catalog, model) {
        Some(e) => e
            .endpoint
            .split("://")
            .nth(1)
            .unwrap_or(&e.endpoint)
            .split('/')
            .next()
            .unwrap_or("unknown")
            .to_string(),
        None => "unknown".into(),
    }
}

fn message_text_parts(msg: &Value) -> Vec<(String, bool)> {
    // (text, is_tool_result)
    let mut out = Vec::new();
    for part in msg["parts"].as_array().into_iter().flatten() {
        match part["type"].as_str() {
            Some("text") => {
                out.push((part["text"].as_str().unwrap_or_default().to_string(), false))
            }
            Some("tool_result") => {
                let mut t = String::new();
                for inner in part["content"].as_array().into_iter().flatten() {
                    t.push_str(inner["text"].as_str().unwrap_or_default());
                }
                if t.is_empty() {
                    t = part["content"].as_str().unwrap_or_default().to_string();
                }
                out.push((t, true));
            }
            _ => {}
        }
    }
    out
}

/// `(invalid, repairs, loop_warnings)` from a thread messages body.
pub fn count_thread(body: &Value) -> (u64, u64, u64) {
    // Tool results in thread order: true = an invalid-call error.
    let mut results: Vec<bool> = Vec::new();
    let mut warnings = 0;
    for rec in body["records"].as_array().into_iter().flatten() {
        if rec["kind"].as_str() != Some("message") {
            continue;
        }
        let msg = &rec["message"];
        let role = msg["role"].as_str().unwrap_or_default();
        for (text, is_result) in message_text_parts(msg) {
            let t = text.trim_start();
            if is_result || role == "tool" {
                results.push(t.starts_with(INVALID_ARGS_PREFIX));
            } else if role == "user" && t.starts_with(LOOP_OBSERVATION_PREFIX) {
                warnings += 1;
            }
        }
    }
    let invalid = results.iter().filter(|b| **b).count() as u64;
    let last_ok = results.iter().rposition(|b| !*b);
    let repairs = match last_ok {
        Some(last) => results[..last].iter().filter(|b| **b).count() as u64,
        None => 0,
    };
    (invalid, repairs, warnings)
}

/// Does a run error name a loop stop?
pub fn is_loop_stop(error: &str) -> bool {
    error
        .trim_matches('"')
        .trim_start()
        .starts_with(LOOP_STOP_PREFIX)
}

/// Measures the runs of one scenario (or trial). `wall_ms` is the scenario's wall time.
pub fn measure(runs: &[RunInput], wall_ms: u128, catalog: &ModelCatalog) -> Measure {
    let mut m = Measure::default();
    m.usage.wall_ms = wall_ms;
    let mut spend: Option<i64> = Some(0);
    let mut gateway_cost: Option<i64> = Some(0);
    let mut est: i64 = 0;
    let mut any_price = false;
    let mut turns = 0u64;
    // A thread several runs share is counted once, from the last run that read it.
    let last_of_thread = |i: usize| {
        let k = runs[i].thread_key;
        k.is_empty() || !runs[i + 1..].iter().any(|r| r.thread_key == k)
    };
    for (i, run) in runs.iter().enumerate() {
        let calls_before = m.usage.calls;
        let mut receipt_turns: Option<u64> = None;
        for ev in run
            .events
            .iter()
            .filter(|e| event_is(e, "inference.completed"))
        {
            let p = payload(ev);
            let model = p["modelName"].as_str().unwrap_or_default().to_string();
            if !model.is_empty() && !m.models.contains(&model) {
                m.models.push(model.clone());
            }
            let (pt, ct) = (int(&p["promptTokens"]), int(&p["completionTokens"]));
            m.usage.calls += 1;
            let resamples = int(&p["resamples"]).max(0) as u64;
            m.resamples += resamples;
            m.invalid += resamples;
            m.usage.prompt_tokens += pt;
            m.usage.completion_tokens += ct;
            m.usage.cached_tokens += int(&p["cachedTokens"]);
            m.usage.cache_creation_tokens += int(&p["cacheCreationTokens"]);
            match opt_int(&p["gatewayCostMicro"]) {
                Some(g) => {
                    if let Some(t) = gateway_cost.as_mut() {
                        *t += g;
                    }
                }
                None => gateway_cost = None,
            }
            match opt_int(&p["spendMicro"]) {
                Some(s) => {
                    if let Some(t) = spend.as_mut() {
                        *t += s;
                    }
                }
                None => spend = None,
            }
            if let Some(e) = estimate_micro(catalog, &model, pt, ct) {
                est += e;
                any_price = true;
            }
        }
        if let Some(r) = run.receipt {
            if m.harness.is_none() {
                let h = &r["receipt"]["harness"];
                if h.is_object() {
                    m.harness = Some(json!({
                        "profile": h["profile"],
                        "version": h["version"],
                        "digest": h["digest"],
                        "overrides": if h["overrides"].is_null() { json!({}) } else { h["overrides"].clone() },
                    }));
                }
            }
            receipt_turns = r["receipt"]["metering"]["turns"].as_u64();
            let e = &r["receipt"]["effects"];
            match e["journaled"].as_bool() {
                Some(true) => {
                    m.receipts_journaled += 1;
                    m.receipt_duplicates += e["duplicates"].as_u64().unwrap_or(0);
                    m.receipt_unknown += e["unknown"].as_u64().unwrap_or(0);
                    m.receipt_identical_repeats +=
                        e["counts"]["identicalRepeats"].as_u64().unwrap_or(0);
                }
                Some(false) => m.receipts_not_journaled += 1,
                None => {}
            }
        }
        match receipt_turns {
            Some(t) => turns += t,
            None => {
                turns += m.usage.calls - calls_before;
                m.usage.turns_partial = true;
            }
        }
        if let Some(t) = run.thread.filter(|_| last_of_thread(i)) {
            let (inv, rep, warn) = count_thread(t);
            m.invalid += inv;
            m.repairs += rep;
            m.loop_warnings += warn;
        }
        if is_loop_stop(&run_error(run)) {
            m.loop_stops += 1;
        }
    }
    m.usage.turns = turns;
    m.usage.estimated = !m.models.is_empty() && m.models.iter().all(|x| x == MOCK_MODEL);
    // Host-recorded spend wins; the catalog price fills in when an event had none.
    m.usage.spend_micro = match (m.usage.calls, spend) {
        (0, _) => None,
        (_, Some(s)) if s > 0 => Some(s),
        _ if any_price => Some(est),
        _ => None,
    };
    m.usage.cost_missing = m.usage.calls > 0 && m.usage.spend_micro.is_none();
    m.usage.gateway_cost_micro = if m.usage.calls > 0 {
        gateway_cost
    } else {
        None
    };
    m
}

/// Creates beyond one per intended effect in the world's ledger: each `(tool, title, count)`
/// line is one intended effect, so a count of 2 is one duplicate.
pub fn duplicate_effects(ledger: &[(String, String, u64)]) -> u64 {
    ledger.iter().map(|(_, _, c)| c.saturating_sub(1)).sum()
}

/// Whether a run was lost: it never reached an end state (still queued or running; a run that
/// waits on a person, `PAUSED`, is not lost), or it ended failed on a crash or restart reason
/// (`guest_crashed`, `vamd_restarted`, or, with its resumes spent or no way to resume,
/// `crashed_repeatedly`, `restarted_repeatedly`, `not_resumable`). An expired queued fire is
/// not lost.
pub fn lost_task(state: &str, error: &str, events: &[Value]) -> bool {
    match state {
        "RUN_STATE_SUCCEEDED"
        | "RUN_STATE_CANCELLED"
        | "RUN_STATE_TIMED_OUT"
        | "RUN_STATE_EXPIRED"
        | "RUN_STATE_REJECTED"
        | "RUN_STATE_PAUSED" => false,
        "RUN_STATE_FAILED" => {
            let text = if error.trim().is_empty() {
                run_error(&RunInput {
                    events,
                    ..Default::default()
                })
            } else {
                error.to_string()
            };
            [
                "guest_crashed",
                "vamd_restarted",
                "crashed_repeatedly",
                "restarted_repeatedly",
                "not_resumable",
            ]
            .iter()
            .any(|r| text.contains(r))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inf(model: &str, p: i64, c: i64, spend: Option<i64>, cached: i64) -> Value {
        let mut d = json!({"modelName": model, "promptTokens": p, "completionTokens": c});
        if let Some(s) = spend {
            d["spendMicro"] = json!(s);
        }
        if cached > 0 {
            d["cachedTokens"] = json!(cached);
        }
        json!({"eventType": "vam.v1.inference.completed", "payload": {"data": d}})
    }

    fn thread(msgs: Vec<(&str, Value)>) -> Value {
        json!({"records": msgs.into_iter().map(|(role, part)| json!({
            "kind": "message", "message": {"role": role, "parts": [part]}})).collect::<Vec<_>>()})
    }

    fn result(text: &str) -> Value {
        json!({"type": "tool_result", "content": [{"type": "text", "text": text}]})
    }

    #[test]
    fn spend_from_events_wins_and_the_catalog_fills_the_gap() {
        let cat = ModelCatalog::new();
        let ev = [inf("claude-haiku-5-5", 1000, 200, Some(5), 100)];
        let m = measure(
            &[RunInput {
                events: &ev,
                ..Default::default()
            }],
            10,
            &cat,
        );
        assert_eq!(m.usage.spend_micro, Some(5));
        assert_eq!((m.usage.cached_tokens, m.tokens()), (100, 1200));
        assert!(!m.usage.cost_missing && !m.usage.estimated);
        // No spend on the event: 1000 * 0.1 + 200 * 0.5 = 200 micro-USD.
        let ev = [inf("claude-haiku-5-5", 1000, 200, None, 0)];
        let m = measure(
            &[RunInput {
                events: &ev,
                ..Default::default()
            }],
            10,
            &cat,
        );
        assert_eq!(m.usage.spend_micro, Some(200));
    }

    fn gw(model: &str, p: i64, c: i64, spend: Option<i64>, gateway: Option<i64>) -> Value {
        let mut v = inf(model, p, c, spend, 0);
        if let Some(g) = gateway {
            v["payload"]["data"]["gatewayCostMicro"] = json!(g);
        }
        v
    }

    #[test]
    fn a_gateway_model_is_priced_at_the_makers_list_price_never_zero() {
        let cat = ModelCatalog::new();
        let id = "anthropic/claude-haiku-5.5";
        // The event carries spend 0 (an unpriced gateway) and a lower gateway charge: the list
        // price still wins, 1000 * 0.1 + 200 * 0.5 = 200.
        let ev = [gw(id, 1000, 200, Some(0), Some(30))];
        let m = measure(
            &[RunInput {
                events: &ev,
                ..Default::default()
            }],
            10,
            &cat,
        );
        assert_eq!(m.usage.spend_micro, Some(200));
        assert_eq!(m.usage.gateway_cost_micro, Some(30));
        assert!(!m.usage.cost_missing);
        // Without a gateway charge it is null, not 0.
        let ev = [gw(id, 1000, 200, None, None)];
        let m = measure(
            &[RunInput {
                events: &ev,
                ..Default::default()
            }],
            10,
            &cat,
        );
        assert_eq!(m.usage.spend_micro, Some(200));
        assert_eq!(m.usage.gateway_cost_micro, None);
        // An id no maker entry covers has no cost, flagged, never a silent 0.
        let ev = [gw("qwen/qwen3-max", 1000, 200, Some(0), None)];
        let m = measure(
            &[RunInput {
                events: &ev,
                ..Default::default()
            }],
            10,
            &cat,
        );
        assert!(m.usage.cost_missing);
    }

    #[test]
    fn gateway_endpoint_host_and_provider() {
        let cat = ModelCatalog::new();
        let id = "anthropic/claude-sonnet-5.5";
        assert_eq!(endpoint_host(&cat, id), "ai-gateway.vercel.sh");
        assert_eq!(provider_of(&cat, id), "vercel");
        assert_eq!(provider_of(&cat, "claude-sonnet-5-5"), "anthropic");
        assert_eq!(
            price_source(&cat, id).as_deref(),
            Some(format!("anthropic list price, catalog {}", cat.catalog_version()).as_str())
        );
        assert_eq!(price_source(&cat, MOCK_MODEL), None);
    }

    #[test]
    fn the_mock_has_tokens_only_and_is_marked_estimated() {
        let cat = ModelCatalog::new();
        let ev = [
            inf("mock-model", 10, 5, None, 0),
            inf("mock-model", 20, 5, None, 0),
        ];
        let m = measure(
            &[RunInput {
                events: &ev,
                ..Default::default()
            }],
            3,
            &cat,
        );
        assert_eq!((m.usage.calls, m.tokens(), m.usage.turns), (2, 40, 2));
        assert!(m.usage.estimated && m.usage.cost_missing);
        assert_eq!(m.usage.spend_micro, None);
        let f = m.summary_fields();
        assert_eq!(f["scored"], false);
        assert!(f["usage"]["spendMicro"].is_null());
    }

    #[test]
    fn a_model_the_catalog_lacks_has_no_cost() {
        let cat = ModelCatalog::new();
        let ev = [inf("no-such-model", 10, 5, None, 0)];
        let m = measure(
            &[RunInput {
                events: &ev,
                ..Default::default()
            }],
            3,
            &cat,
        );
        assert!(m.usage.cost_missing && !m.usage.estimated);
    }

    #[test]
    fn harness_and_turns_come_from_the_receipt() {
        let cat = ModelCatalog::new();
        let r = json!({"receipt": {"harness": {"profile": "base", "version": "1", "digest": "d",
            "overrides": {"loop": {"detect": true}}}, "metering": {"turns": 4}}});
        let m = measure(
            &[RunInput {
                receipt: Some(&r),
                ..Default::default()
            }],
            1,
            &cat,
        );
        let h = m.harness.unwrap();
        assert_eq!(h["profile"], "base");
        assert_eq!(h["overrides"]["loop"]["detect"], true);
        assert_eq!(m.usage.turns, 4);
        let none = measure(&[RunInput::default()], 1, &cat);
        assert!(none.harness.is_none() && none.summary_fields()["harness"].is_null());
    }

    #[test]
    fn invalid_calls_repairs_and_loop_warnings_are_counted_by_prefix() {
        let t = thread(vec![
            (
                "tool",
                result("invalid arguments for read_file: not valid JSON"),
            ),
            (
                "tool",
                result("invalid arguments for read_file: missing required field 'path'"),
            ),
            ("tool", result("note text")),
            (
                "user",
                json!({"type": "text", "text": "Loop detected: you repeated the same call"}),
            ),
            ("user", json!({"type": "text", "text": "please continue"})),
            (
                "tool",
                result("invalid arguments for write_file: not valid JSON"),
            ),
        ]);
        // Two invalid calls were followed by a good result (repairs); the last was not.
        assert_eq!(count_thread(&t), (3, 2, 1));
        let cat = ModelCatalog::new();
        let m = measure(
            &[RunInput {
                thread: Some(&t),
                error: "\"loop_detected: repeated\"",
                ..Default::default()
            }],
            1,
            &cat,
        );
        assert_eq!(
            (m.invalid, m.repairs, m.loop_warnings, m.loop_stops),
            (3, 2, 1, 1)
        );
        assert_eq!(m.loop_detections(), 2);
    }

    #[test]
    fn resamples_count_as_invalid_calls_and_are_reported_separately() {
        let cat = ModelCatalog::new();
        let mut resampled = inf("mock-model", 10, 5, None, 0);
        resampled["payload"]["data"]["resamples"] = json!("1");
        let events = vec![inf("mock-model", 10, 5, None, 0), resampled];
        let m = measure(
            &[RunInput {
                events: &events,
                ..Default::default()
            }],
            1,
            &cat,
        );
        assert_eq!((m.invalid, m.resamples, m.repairs), (1, 1, 0));
        let fields = m.summary_fields();
        assert_eq!(fields["toolCalls"]["resamples"], 1);
        let none = measure(&[RunInput::default()], 1, &cat);
        assert_eq!((none.invalid, none.resamples), (0, 0));
    }

    #[test]
    fn a_plain_thread_counts_nothing() {
        let t = thread(vec![
            ("tool", result("fine")),
            ("assistant", json!({"type": "text", "text": "done"})),
        ]);
        assert_eq!(count_thread(&t), (0, 0, 0));
        assert!(!is_loop_stop("null"));
        assert!(is_loop_stop("loop_detected: x"));
    }

    #[test]
    fn endpoint_host_names_the_host_only() {
        let cat = ModelCatalog::new();
        assert_eq!(
            endpoint_host(&cat, "claude-sonnet-5-5"),
            "api.anthropic.com"
        );
        assert_eq!(endpoint_host(&cat, MOCK_MODEL), "mock");
        assert_eq!(endpoint_host(&cat, "nope"), "unknown");
    }

    fn run_with<'a>(thread: &'a Value, key: &'a str) -> RunInput<'a> {
        RunInput {
            thread: Some(thread),
            thread_key: key,
            ..Default::default()
        }
    }

    #[test]
    fn a_thread_three_runs_share_is_counted_once() {
        // Three chats continue one thread: the thread holds one invalid call per run, and each
        // run's read of it is cumulative (1, 2, then 3 calls).
        let bad = |n: usize| {
            thread(
                (0..n)
                    .map(|_| {
                        (
                            "tool",
                            result("invalid arguments for read_file: not valid JSON"),
                        )
                    })
                    .collect(),
            )
        };
        let (t1, t2, t3) = (bad(1), bad(2), bad(3));
        let cat = ModelCatalog::new();
        let shared = [
            run_with(&t1, "w/t"),
            run_with(&t2, "w/t"),
            run_with(&t3, "w/t"),
        ];
        assert_eq!(measure(&shared, 1, &cat).invalid, 3);
        // Three separate threads with one call each also total three.
        let one = bad(1);
        let apart = [
            run_with(&one, "w/a"),
            run_with(&one, "w/b"),
            run_with(&one, "w/c"),
        ];
        assert_eq!(measure(&apart, 1, &cat).invalid, 3);
        // No key means no sharing.
        let unkeyed = [run_with(&one, ""), run_with(&one, "")];
        assert_eq!(measure(&unkeyed, 1, &cat).invalid, 2);
    }

    #[test]
    fn a_loop_stop_comes_from_the_run_completed_payload_when_status_has_no_error() {
        let ev = [json!({"eventType": "vam.v1.run.completed",
            "payload": "{\"error\":\"loop_detected: identical_calls: read_file called 5 times\"}"})];
        let cat = ModelCatalog::new();
        let run = RunInput {
            events: &ev,
            error: "null",
            ..Default::default()
        };
        assert_eq!(measure(&[run], 1, &cat).loop_stops, 1);
        let ok = [json!({"eventType": "vam.v1.run.completed", "payload": {"error": ""}})];
        let run = RunInput {
            events: &ok,
            ..Default::default()
        };
        assert_eq!(measure(&[run], 1, &cat).loop_stops, 0);
        let other =
            [json!({"eventType": "vam.v1.run.completed", "payload": {"error": "timed out"}})];
        let run = RunInput {
            events: &other,
            ..Default::default()
        };
        assert_eq!(measure(&[run], 1, &cat).loop_stops, 0);
    }

    #[test]
    fn turns_are_per_run_receipt_iterations_with_the_call_count_as_the_fallback() {
        let cat = ModelCatalog::new();
        let receipt = json!({"receipt": {"metering": {"turns": 4}}});
        let with = [inf("mock-model", 1, 1, None, 0)];
        let without = [
            inf("mock-model", 1, 1, None, 0),
            inf("mock-model", 1, 1, None, 0),
            inf("mock-model", 1, 1, None, 0),
        ];
        let runs = [
            RunInput {
                events: &with,
                receipt: Some(&receipt),
                ..Default::default()
            },
            RunInput {
                events: &without,
                ..Default::default()
            },
        ];
        let m = measure(&runs, 1, &cat);
        assert_eq!((m.usage.calls, m.usage.turns), (4, 7));
        assert!(m.usage.turns_partial);
        assert_eq!(m.summary_fields()["usage"]["turnsPartial"], true);
        // Every run has a receipt: not partial.
        let both = [
            RunInput {
                events: &with,
                receipt: Some(&receipt),
                ..Default::default()
            },
            RunInput {
                events: &with,
                receipt: Some(&receipt),
                ..Default::default()
            },
        ];
        let m = measure(&both, 1, &cat);
        assert_eq!(m.usage.turns, 8);
        assert!(!m.usage.turns_partial);
    }
}
