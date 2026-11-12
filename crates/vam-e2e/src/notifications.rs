//! The steps that read a user's notifications and decide from another surface:
//! `expect: { notifications }`, `expect: { notification-summary }`, `notification-mark`, and
//! `via: cli|app` on `approve`, `deny` and `decide-gate`. Also the two benchmark probes of the
//! notification list (decisions listed, decisions learned of within a minute).
//!
//! This module is a child of `engine`, so it reads the engine's private state; the shared
//! runner files only register the steps and call the probe hook.
//!
//! Wire shapes the steps read (docs: orchestrator/notifications):
//! - `GET /v1/notifications?state=open|all&pageSize=` answers `{ records: [...], pagination:
//!   { nextPageToken } }`; a record has `id`, `kind`, `severity`, `runId`, `approvalId`,
//!   `receiptDigest`, `count`, `createdAt`, `firstShownAt`, `readAt`, `actedAt`, `actedSurface`,
//!   `resolvedAt`, `resolvedReason` (camelCase; an unset field is absent or null);
//! - `GET /v1/notifications/summary` answers `{ unread, action, warn, etag }`;
//! - `POST /v1/notifications/{id}/read` and `/acted`.

use super::{conn, field_problems, Engine, Fail, Probe};
use crate::api::{seg, Reply};
use crate::scenario::{Class, Expect, Latest, Step};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Mutex;

// ---- step and expect shapes ------------------------------------------------------------------

/// `via: cli|app` on a decision: the surface the decision comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Via {
    /// The built `vam approve decide` (`VAM_E2E_CLI`); the CLI sends `X-Vam-Surface: cli`.
    Cli,
    /// The API with `X-Vam-Surface: app`, as the app sends it.
    App,
}

/// `expect: { notifications: { kind, count, run, unread, state, fields, absent, as, within,
/// approval, digest } }`: the user's list holds the rows described.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotificationsExpect {
    /// Only rows of this kind (`approval.pending`, `run.finished`, ...).
    pub kind: Option<String>,
    /// Exactly this many matching rows; at least one when omitted.
    pub count: Option<usize>,
    /// `latest`: only rows of the latest run.
    pub run: Option<Latest>,
    /// `true`: only unread rows; `false`: only read rows.
    pub unread: Option<bool>,
    /// `open` (unresolved) or `all` (the default).
    pub state: Option<String>,
    /// Dotted camelCase paths over the first matching row; same matchers as `receipt` fields.
    pub fields: Option<BTreeMap<String, serde_yaml::Value>>,
    /// `true`: no row matches.
    pub absent: Option<bool>,
    /// Alias of the `org` piece whose list is read; the bootstrap identity when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    /// How long the list may take to hold (`15s`); the step budget when omitted.
    pub within: Option<String>,
    /// `latest`: the first matching row's `approvalId` is the request id of the latest run's
    /// pending approval.
    pub approval: Option<Latest>,
    /// `receipt`: the first matching row's `receiptDigest` is the latest run's receipt digest.
    pub digest: Option<String>,
}

/// `expect: { notification-summary: { action, warn, unread, as, within } }`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SummaryExpect {
    pub action: Option<i64>,
    pub warn: Option<i64>,
    pub unread: Option<i64>,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    pub within: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MarkAction {
    Read,
    Acted,
    /// Not a mark: `GET /v1/notifications/{id}`, to see what another identity is answered.
    Get,
}

impl MarkAction {
    fn word(self) -> &'static str {
        match self {
            MarkAction::Read => "read",
            MarkAction::Acted => "acted",
            MarkAction::Get => "get",
        }
    }
}

/// `notification-mark: { action: read|acted|get, kind, as, of, status, refused, contains }`: marks (or, for `get`, reads) the first
/// record (of `kind`, when given) in the list of `of` (the bootstrap identity when omitted),
/// as `as`. Passes on a 2xx answer; `status` names the exact answer wanted, `refused` any 4xx.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mark {
    pub action: MarkAction,
    pub kind: Option<String>,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    pub of: Option<String>,
    pub status: Option<u16>,
    #[serde(default)]
    pub refused: bool,
    /// The answer body contains this text.
    pub contains: Option<String>,
}

pub const KINDS: &[&str] = &[
    "approval.pending",
    "question.asked",
    "gate.opened",
    "run.finished",
    "report.arrived",
    "delegation.stopped",
    "delegation.expired",
    "grant.expiring",
    "budget.hit",
    "trigger.fired",
    "trigger.skipped",
    "connection.needs_reauth",
];

/// Problems with a notifications step or expect, for `vam-e2e check`. `orgs` are the aliases of
/// the `org` pieces in `given`.
pub fn check_expect(e: &Expect, orgs: &BTreeSet<String>) -> Vec<String> {
    let mut bad = Vec::new();
    if let Some(x) = &e.notifications {
        bad.extend(check_common(
            "notifications",
            x.as_.as_deref(),
            x.within.as_deref(),
            orgs,
        ));
        if let Some(k) = x.kind.as_deref().filter(|k| !KINDS.contains(k)) {
            bad.push(format!(
                "`notifications.kind: {k}` is not a notification kind"
            ));
        }
        if x.state
            .as_deref()
            .is_some_and(|s| s != "open" && s != "all")
        {
            bad.push("`notifications.state` is `open` or `all`".into());
        }
        if x.absent == Some(true) && (x.count.is_some_and(|c| c > 0) || x.fields.is_some()) {
            bad.push("`notifications.absent` cannot be combined with count or fields".into());
        }
        if x.digest.as_deref().is_some_and(|d| d != "receipt") {
            bad.push("`notifications.digest` supports only `receipt`".into());
        }
    }
    if let Some(x) = &e.notification_summary {
        bad.extend(check_common(
            "notification-summary",
            x.as_.as_deref(),
            x.within.as_deref(),
            orgs,
        ));
        if x.action.is_none() && x.warn.is_none() && x.unread.is_none() {
            bad.push("`notification-summary` needs action, warn or unread".into());
        }
    }
    bad
}

/// Problems with a `notification-mark` step.
pub fn check_mark(m: &Mark, orgs: &BTreeSet<String>) -> Vec<String> {
    let mut bad = check_common("notification-mark", m.as_.as_deref(), None, orgs);
    if let Some(of) = m.of.as_deref().filter(|o| !orgs.contains(*o)) {
        bad.push(format!("`of: {of}` is not an org piece in `given`"));
    }
    if let Some(k) = m.kind.as_deref().filter(|k| !KINDS.contains(k)) {
        bad.push(format!("`kind: {k}` is not a notification kind"));
    }
    bad
}

fn check_common(
    what: &str,
    as_: Option<&str>,
    within: Option<&str>,
    orgs: &BTreeSet<String>,
) -> Vec<String> {
    let mut bad = Vec::new();
    if let Some(a) = as_.filter(|a| !orgs.contains(*a)) {
        bad.push(format!("`{what}.as: {a}` is not an org piece in `given`"));
    }
    if within.is_some_and(|d| super::parse_duration(d).is_none()) {
        bad.push(format!("`{what}.within` must look like `30s`"));
    }
    bad
}

// ---- reading the list ------------------------------------------------------------------------

/// A page walk of the list: the rows, or the status and body of a refusal.
type Listed = Result<Vec<Value>, (u16, String)>;

fn text_of(v: &Value) -> Option<&str> {
    v.as_str().or_else(|| v["urn"].as_str())
}

fn num_of(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .or_else(|| v.as_f64().map(|f| f as i64))
}

impl Engine {
    /// Every row of the list for `state` (`open` or `all`) as the identity `key`.
    pub(super) fn list_records(&self, key: Option<&str>, state: &str) -> Result<Listed, Fail> {
        let mut out = Vec::new();
        let mut token = String::new();
        for _ in 0..50 {
            let mut path = format!("/v1/notifications?state={state}&pageSize=100");
            if !token.is_empty() {
                path.push_str(&format!("&pageToken={}", seg(&token)));
            }
            let r = self.api.get_as(key, &path).map_err(conn)?;
            if !r.ok() {
                return Ok(Err((r.status, r.body)));
            }
            let v = r.json();
            out.extend(v["records"].as_array().cloned().unwrap_or_default());
            token = v["pagination"]["nextPageToken"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if token.is_empty() {
                break;
            }
        }
        Ok(Ok(out))
    }

    pub(super) fn budget_of(&self, within: Option<&str>) -> std::time::Duration {
        within
            .and_then(super::parse_duration)
            .unwrap_or(self.cfg.step_budget)
    }

    /// The suite shares one vamd and one identity, and a finished run now leaves a record, so
    /// `notification-summary` (node-wide for the identity) would count earlier scenarios'
    /// records. Marks everything older than now read before a scenario starts; best effort.
    pub fn quiesce_notifications(&mut self) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let _ = self
            .api
            .post("/v1/notifications/read", &json!({ "before": now + 1 }));
    }

    /// `expect: { notifications }`.
    pub(super) fn expect_notifications(&mut self, x: &NotificationsExpect) -> Result<(), Fail> {
        let key = self.key_of(x.as_.as_deref())?;
        let budget = self.budget_of(x.within.as_deref());
        let latest = if x.run.is_some() || x.approval.is_some() || x.digest.is_some() {
            Some(self.last_run()?)
        } else {
            None
        };
        let run_id = latest.map(|i| self.ctx.runs[i].run_id.clone());
        // The suite shares one vamd and one identity: without `run:`, only the records of this
        // scenario's own runs are matched.
        let own_runs: Vec<String> = self.ctx.runs.iter().map(|r| r.run_id.clone()).collect();
        let run_key = latest.and_then(|i| self.run_key(i));
        let state = x.state.clone().unwrap_or_else(|| "all".into());
        let what = format!(
            "the notification list has {} row(s){}{}",
            x.count
                .map(|c| c.to_string())
                .unwrap_or_else(|| if x.absent == Some(true) { "no" } else { "some" }.into()),
            x.kind
                .as_deref()
                .map(|k| format!(" of kind {k}"))
                .unwrap_or_default(),
            if x.run.is_some() {
                " for the latest run"
            } else {
                ""
            },
        );
        let mut seen = String::new();
        let r = self.wait_for(budget, None, |e| {
            let rows = match e.list_records(key.as_deref(), &state)? {
                Ok(rows) => rows,
                Err((status, body)) => {
                    return Ok(Probe::Impossible(format!(
                        "GET /v1/notifications answered {status} {body}"
                    )))
                }
            };
            let hits: Vec<&Value> = rows
                .iter()
                .filter(|r| {
                    x.kind
                        .as_deref()
                        .is_none_or(|k| r["kind"].as_str() == Some(k))
                        && run_id
                            .as_deref()
                            .filter(|_| x.run.is_some())
                            .is_none_or(|id| text_of(&r["runId"]) == Some(id))
                        && (x.run.is_some()
                            || own_runs.is_empty()
                            || text_of(&r["runId"])
                                .is_some_and(|id| own_runs.iter().any(|o| o == id)))
                        && x.unread.is_none_or(|u| {
                            let unread = r["readAt"].is_null();
                            unread == u
                        })
                })
                .collect();
            seen = format!("{} of {} listed row(s) match", hits.len(), rows.len());
            if x.absent == Some(true) {
                return Ok(if hits.is_empty() {
                    Probe::Met
                } else {
                    Probe::Impossible(format!("{seen}: {}", hits[0]))
                });
            }
            match x.count {
                Some(n) if hits.len() != n => return Ok(Probe::Pending),
                None if hits.is_empty() => return Ok(Probe::Pending),
                _ => {}
            }
            let Some(first) = hits.first() else {
                return Ok(Probe::Met);
            };
            let mut bad = Vec::new();
            if let Some(f) = &x.fields {
                bad.extend(field_problems(first, f));
            }
            if let (Some(_), Some(rid)) = (&x.approval, &run_id) {
                let want = e
                    .listed_approvals(rid, run_key.as_deref())?
                    .last()
                    .and_then(|c| c["requestId"]["urn"].as_str().map(String::from));
                let got = text_of(&first["approvalId"]).map(String::from);
                if want.is_none() || want != got {
                    bad.push(format!(
                        "approvalId: {got:?} (want the pending approval {want:?})"
                    ));
                }
            }
            if let (Some(_), Some(rid)) = (&x.digest, &run_id) {
                let rr = e
                    .api
                    .get_as(
                        run_key.as_deref(),
                        &format!("/v1/runs/{}/receipt", seg(rid)),
                    )
                    .map_err(conn)?;
                let want = rr.json()["signature"]["receiptDigest"]
                    .as_str()
                    .map(String::from);
                let got = first["receiptDigest"].as_str().map(String::from);
                if want.is_none() || want != got {
                    bad.push(format!(
                        "receiptDigest: {got:?} (want the receipt's {want:?})"
                    ));
                }
            }
            if bad.is_empty() {
                Ok(Probe::Met)
            } else {
                seen = format!("{seen}; {}", bad.join("; "));
                Ok(Probe::Pending)
            }
        });
        r.map_err(|w| {
            let mut f = self.wait_fail(w, &what, None);
            f.observed = format!("{}; last read: {seen}", f.observed);
            // The routes answered but the record never came: the host did not write it.
            if f.class == Class::Harness {
                f.class = Class::Product;
            }
            f
        })
    }

    /// `expect: { notification-summary }`.
    pub(super) fn expect_notification_summary(&mut self, x: &SummaryExpect) -> Result<(), Fail> {
        let key = self.key_of(x.as_.as_deref())?;
        let budget = self.budget_of(x.within.as_deref());
        let want = [("action", x.action), ("warn", x.warn), ("unread", x.unread)];
        let what = format!(
            "the notification summary reads {}",
            want.iter()
                .filter_map(|(n, v)| v.map(|v| format!("{n}={v}")))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let mut seen = String::new();
        let r = self.wait_for(budget, None, |e| {
            let r = e
                .api
                .get_as(key.as_deref(), "/v1/notifications/summary")
                .map_err(conn)?;
            if !r.ok() {
                return Ok(Probe::Impossible(format!(
                    "GET /v1/notifications/summary answered {} {}",
                    r.status, r.body
                )));
            }
            let v = r.json();
            seen = format!(
                "action={} warn={} unread={}",
                v["action"], v["warn"], v["unread"]
            );
            let ok = want
                .iter()
                .all(|(n, w)| w.is_none_or(|w| num_of(&v[*n]).unwrap_or(0) == w));
            Ok(if ok { Probe::Met } else { Probe::Pending })
        });
        r.map_err(|w| {
            let mut f = self.wait_fail(w, &what, None);
            f.observed = format!("{}; last read: {seen}", f.observed);
            // The routes answered but the record never came: the host did not write it.
            if f.class == Class::Harness {
                f.class = Class::Product;
            }
            f
        })
    }

    /// `notification-mark`.
    pub(super) fn notification_mark(&mut self, m: &Mark) -> Result<(), Fail> {
        let of_key = self.key_of(m.of.as_deref())?;
        let as_key = self.key_of(m.as_.as_deref())?;
        let what = format!(
            "the {} mark of a notification{}",
            m.action.word(),
            m.kind
                .as_deref()
                .map(|k| format!(" of kind {k}"))
                .unwrap_or_default()
        );
        let rows = self
            .list_records(of_key.as_deref(), "all")?
            .map_err(|(s, b)| {
                Fail::new(
                    Class::Product,
                    "the notification list is read",
                    format!("GET /v1/notifications answered {s} {b}"),
                )
            })?;
        let target = rows
            .iter()
            .find(|r| {
                m.kind
                    .as_deref()
                    .is_none_or(|k| r["kind"].as_str() == Some(k))
            })
            .and_then(|r| r["id"].as_str())
            .ok_or_else(|| Fail::new(Class::Product, &what, "no such notification is listed"))?
            .to_string();
        let path = format!("/v1/notifications/{}", seg(&target));
        let r = match m.action {
            MarkAction::Get => self.api.get_as(as_key.as_deref(), &path),
            a => self.api.post_as(
                as_key.as_deref(),
                &format!("{path}/{}", a.word()),
                &json!({}),
            ),
        }
        .map_err(conn)?;
        let status_ok = match (m.status, m.refused) {
            (Some(s), _) => r.status == s,
            (None, true) => (400..500).contains(&r.status),
            (None, false) => r.ok(),
        };
        let text_ok = m.contains.as_deref().is_none_or(|t| r.body.contains(t));
        if status_ok && text_ok {
            return Ok(());
        }
        let want = match (m.status, m.refused) {
            (Some(s), _) => format!("{what} answers {s}"),
            (None, true) => format!("{what} is refused (a 4xx answer)"),
            (None, false) => format!("{what} is accepted"),
        };
        Err(Fail::new(
            Class::Product,
            want,
            format!("{} {}", r.status, r.body),
        ))
    }

    // ---- deciding from another surface -------------------------------------------------------

    /// Sends a decision from `via`. The answer is shaped like an API reply: the CLI's exit
    /// status maps to 200, or 400 with its output as the body.
    pub(super) fn decide_via(
        &mut self,
        via: Via,
        key: Option<&str>,
        req_id: &str,
        decision: &str,
        body: &Value,
    ) -> Result<Reply, Fail> {
        match via {
            Via::App => self
                .api
                .post_as_surface(
                    key,
                    &format!("/v1/approvals/{}/decide", seg(req_id)),
                    body,
                    "app",
                )
                .map_err(conn),
            Via::Cli => {
                let word = match decision {
                    "HITL_DECISION_APPROVED" => "approved",
                    "HITL_DECISION_REJECTED" => "rejected",
                    "HITL_DECISION_SUBMITTED" => "submitted",
                    other => {
                        return Err(Fail::new(
                            Class::Scenario,
                            "a decision the CLI can send",
                            other.to_string(),
                        ))
                    }
                };
                let mut args = vec![
                    "--host".to_string(),
                    self.api.base().to_string(),
                    "--token".to_string(),
                    key.unwrap_or(self.api.key()).to_string(),
                    "approve".into(),
                    "decide".into(),
                    req_id.to_string(),
                    "--decision".into(),
                    word.into(),
                ];
                if let Some(resp) = body.get("response").filter(|r| !r.is_null()) {
                    args.push("--response".into());
                    args.push(resp.to_string());
                }
                let want = "the built vam runs (VAM_E2E_CLI)";
                let cli = std::env::var("VAM_E2E_CLI")
                    .ok()
                    .filter(|c| !c.is_empty())
                    .ok_or_else(|| {
                        Fail::new(
                            Class::Env,
                            want,
                            "VAM_E2E_CLI is not set (scripts/e2e/run.sh exports it)",
                        )
                    })?;
                let mut cmd = std::process::Command::new(&cli);
                cmd.args(&args);
                if let Some(ca) = self.api.ca_path() {
                    cmd.env("VAM_CA_FILE", ca);
                }
                let out = cmd
                    .output()
                    .map_err(|e| Fail::new(Class::Env, want, format!("{cli}: {e}")))?;
                let text = format!(
                    "{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
                Ok(Reply {
                    status: if out.status.success() { 200 } else { 400 },
                    body: text.trim().to_string(),
                })
            }
        }
    }

    // ---- the probes --------------------------------------------------------------------------

    /// Called after every step that passed. Parking (`expect: { approval|gate: pending }`) and
    /// deciding are the moments the probes look; anything else is ignored. A probe never fails
    /// a scenario: a route that answers 4xx counts as "not listed" and "unmeasured".
    pub(super) fn notify_observe(&mut self, st: &Step) {
        let parked = matches!(
            st,
            Step::Expect(e) if e.approval.as_deref() == Some("pending")
                || e.gate.as_deref() == Some("pending")
        );
        let decided = matches!(st, Step::Approve(_) | Step::Deny(_) | Step::DecideGate(_));
        if !parked && !decided {
            return;
        }
        let Ok(i) = self.last_run() else { return };
        let key = self.run_key(i);
        let run_id = self.ctx.runs[i].run_id.clone();
        if parked {
            let mut ids = BTreeSet::new();
            for gate in [false, true] {
                if let Ok(v) = self.pending_approvals(&run_id, gate, key.as_deref()) {
                    ids.extend(
                        v.iter()
                            .filter_map(|c| c["requestId"]["urn"].as_str().map(String::from)),
                    );
                }
            }
            let listed = match self.list_records(key.as_deref(), "open") {
                Ok(Ok(rows)) => rows
                    .iter()
                    .filter(|r| r["resolvedAt"].is_null())
                    .filter_map(|r| text_of(&r["approvalId"]).map(String::from))
                    .filter(|a| ids.contains(a))
                    .collect::<BTreeSet<_>>()
                    .len() as u64,
                _ => 0,
            };
            Probes::add_pending(listed, ids.len() as u64);
            // The app polls the summary and lists when the badge rises: once, here.
            if let Ok(r) = self.api.get_as(key.as_deref(), "/v1/notifications/summary") {
                if r.ok() && num_of(&r.json()["action"]).unwrap_or(0) > 0 {
                    let _ = self.list_records(key.as_deref(), "open");
                }
            }
        }
        if let Ok(Ok(rows)) = self.list_records(key.as_deref(), "all") {
            for r in rows
                .iter()
                .filter(|r| r["severity"].as_str() == Some("action"))
            {
                if let (Some(id), Some(created)) = (r["id"].as_str(), num_of(&r["createdAt"])) {
                    Probes::see(
                        id,
                        created,
                        num_of(&r["firstShownAt"]),
                        num_of(&r["actedAt"]),
                    );
                }
            }
        }
    }
}

// ---- probe totals ----------------------------------------------------------------------------

#[derive(Default)]
struct Totals {
    listed: u64,
    pending: u64,
    /// id -> (created, first shown, acted)
    seen: BTreeMap<String, (i64, Option<i64>, Option<i64>)>,
}

static TOTALS: Mutex<Totals> = Mutex::new(Totals {
    listed: 0,
    pending: 0,
    seen: BTreeMap::new(),
});

/// The run-wide benchmark probes of the notification list (P2 and P7 on the mock).
pub struct Probes;

impl Probes {
    fn add_pending(listed: u64, pending: u64) {
        if let Ok(mut t) = TOTALS.lock() {
            t.listed += listed;
            t.pending += pending;
        }
    }

    fn see(id: &str, created: i64, shown: Option<i64>, acted: Option<i64>) {
        if let Ok(mut t) = TOTALS.lock() {
            t.seen.insert(id.to_string(), (created, shown, acted));
        }
    }

    /// The `bench.notify` object of `summary.json`:
    /// `pendingListed {listed, pending}`, `shownWithin60s {within, total}` and
    /// `medianToActSecs`, the last two `"unmeasured"` while no record was seen. The median is
    /// the script's own pace: recorded, never scored.
    pub fn summary() -> Value {
        let t = TOTALS.lock().map(|t| summarize(&t)).unwrap_or(Value::Null);
        json!({ "notify": t })
    }
}

fn summarize(t: &Totals) -> Value {
    let total = t.seen.len() as u64;
    let within = t
        .seen
        .values()
        .filter(|(c, shown, _)| shown.is_some_and(|s| s - c <= 60))
        .count() as u64;
    let mut acts: Vec<i64> = t
        .seen
        .values()
        .filter_map(|(c, _, a)| a.map(|a| a - c))
        .collect();
    acts.sort_unstable();
    let median = match acts.len() {
        0 => Value::from("unmeasured"),
        n if n % 2 == 1 => Value::from(acts[n / 2]),
        n => Value::from((acts[n / 2 - 1] + acts[n / 2]) as f64 / 2.0),
    };
    json!({
        "pendingListed": { "listed": t.listed, "pending": t.pending },
        "shownWithin60s": if total == 0 {
            Value::from("unmeasured")
        } else {
            json!({ "within": within, "total": total })
        },
        "medianToActSecs": median,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_parse_in_their_documented_shapes() {
        let s: Step = serde_yaml::from_str(
            "expect: { notifications: { kind: approval.pending, count: 1, run: latest, unread: true, state: open, approval: latest, as: bob, within: 90s, fields: { count: 2 } } }",
        )
        .unwrap();
        let Step::Expect(e) = s else {
            panic!("an expect")
        };
        assert_eq!(e.families(), 1);
        let n = e.notifications.unwrap();
        assert_eq!(n.count, Some(1));
        assert_eq!(n.within.as_deref(), Some("90s"));
        let s: Step = serde_yaml::from_str(
            "notification-mark: { action: acted, kind: approval.pending, status: 409, contains: decide_required }",
        )
        .unwrap();
        assert!(matches!(s, Step::NotificationMark(m) if m.status == Some(409)));
        let s: Step = serde_yaml::from_str("approve: { via: cli }").unwrap();
        assert!(matches!(s, Step::Approve(d) if d.opts().via == Some(Via::Cli)));
        assert!(serde_yaml::from_str::<Step>("approve: { via: slack }").is_err());
        assert!(serde_yaml::from_str::<Step>("expect: { notifications: { kinds: x } }").is_err());
    }

    #[test]
    fn check_names_the_problem() {
        let orgs: BTreeSet<String> = ["bob".to_string()].into();
        let e = |y: &str| match serde_yaml::from_str::<Step>(y).unwrap() {
            Step::Expect(e) => e,
            _ => panic!("an expect"),
        };
        assert!(check_expect(
            &e("expect: { notifications: { kind: run.finished, as: bob } }"),
            &orgs
        )
        .is_empty());
        assert_eq!(
            check_expect(&e("expect: { notifications: { kind: nope } }"), &orgs).len(),
            1
        );
        assert_eq!(
            check_expect(&e("expect: { notifications: { as: eve } }"), &orgs).len(),
            1
        );
        assert_eq!(
            check_expect(&e("expect: { notifications: { within: soon } }"), &orgs).len(),
            1
        );
        assert_eq!(
            check_expect(&e("expect: { notification-summary: {} }"), &orgs).len(),
            1
        );
    }

    #[test]
    fn probes_report_unmeasured_until_a_record_is_seen() {
        let mut t = Totals::default();
        let v = summarize(&t);
        assert_eq!(v["shownWithin60s"], "unmeasured");
        assert_eq!(v["medianToActSecs"], "unmeasured");
        assert_eq!(v["pendingListed"]["listed"], 0);
        t.listed = 1;
        t.pending = 2;
        t.seen.insert("a".into(), (100, Some(110), Some(130)));
        t.seen.insert("b".into(), (100, Some(200), None));
        let v = summarize(&t);
        assert_eq!(v["pendingListed"]["pending"], 2);
        assert_eq!(v["shownWithin60s"]["within"], 1);
        assert_eq!(v["shownWithin60s"]["total"], 2);
        assert_eq!(v["medianToActSecs"], 30);
    }
}
