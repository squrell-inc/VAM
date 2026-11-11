//! The steps that work with trigger grants, the test broker and Space and thread ids:
//! `fire-trigger`, `revoke-grant`, `bearer-request`, and the expects `grant`, `fire`, `spaces`,
//! `threads` and `runs`. Also the two world pieces they need: a `broker` (a test broker process
//! registered with vamd) and a `workspace-seed` (a workspace an earlier vamd wrote).
//!
//! This module is a child of `engine`, so it reads the engine's private state; the shared
//! runner files only register the steps and the pieces.
//!
//! Contract with the test broker (`$VAM_E2E_TEST_BROKER`, the `hsp-echo-broker` binary):
//! - it is started as `hsp-echo-broker --triggers --socket <dir>/b.sock`; `<dir>` is its state dir;
//! - once armed, it keeps the grant token in `<dir>/grant-token` (one line, mode 0600);
//! - `hsp-echo-broker fire --state-dir <dir> --gateway-socket <p> --key <k> --times <n> --kind
//!   <scheduled|run-now>` (`<p>` is the `gatewaySocket` of `GET /v1/hosted/brokers/{id}`; the step
//!   fails with a clear message while vamd reports it empty) fires the grant `n` times with idempotency key `k` and prints one JSON object per fire:
//!   `{"outcome":"ACCEPTED","runId":"urn:...","denyReason":"","busyReason":""}`.

use super::{conn, field_problems, Engine, Fail};
use crate::api::seg;
use crate::scenario::Class;
use crate::world::WorldPiece;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

// ---- step and expect shapes ------------------------------------------------------------------

/// `fire-trigger: { key, times, kind, label }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FireTrigger {
    /// The idempotency key of the fire; the same on every retry of one fire.
    pub key: String,
    #[serde(default = "one")]
    pub times: u32,
    #[serde(default)]
    pub kind: FireKind,
    /// The `e2e-step` label of the message the grant carries, so the mock can answer the run
    /// the fire starts (the grant was armed by the earlier chat; the label is not sent here).
    pub label: Option<String>,
}

fn one() -> u32 {
    1
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FireKind {
    #[default]
    Scheduled,
    RunNow,
}

impl FireKind {
    fn word(self) -> &'static str {
        match self {
            FireKind::Scheduled => "scheduled",
            FireKind::RunNow => "run-now",
        }
    }
}

/// `revoke-grant: { via }`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevokeGrant {
    #[serde(default)]
    pub via: RevokeVia,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RevokeVia {
    /// `DELETE /v1/keys/{id}`.
    #[default]
    Api,
}

/// `bearer-request: { method, path, token, status }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BearerRequest {
    #[serde(default = "post")]
    pub method: String,
    pub path: String,
    pub token: TokenForm,
    /// The status vamd must answer.
    pub status: u16,
}

fn post() -> String {
    "post".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum TokenForm {
    /// The whole token, `vam_rg_<grant id>.<secret>`.
    Grant,
    /// Only the secret part: the form whose hash vamd stores.
    GrantSecret,
}

/// `grant: { tool, fields, count, absent }`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GrantExpect {
    /// Only grants issued through this tool.
    pub tool: Option<String>,
    /// Matched against the first such grant (dotted camelCase paths of the key row).
    pub fields: Option<BTreeMap<String, serde_yaml::Value>>,
    /// Exactly this many such grants are listed.
    pub count: Option<usize>,
    /// `true`: no such grant is listed.
    pub absent: Option<bool>,
}

/// `fire: { outcomes, reason, sameRun }`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FireExpect {
    /// The outcome words of the latest `fire-trigger`, in order.
    pub outcomes: Vec<String>,
    /// The deny or busy reason of the last fire.
    pub reason: Option<String>,
    /// `true`: every fire after the first names the first fire's run.
    pub same_run: Option<bool>,
}

/// `create-space: { label, agent }`: `POST /v1/spaces` for an agent; later chats go into it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CreateSpace {
    pub label: String,
    /// Alias of the agent piece; the only agent when omitted.
    pub agent: Option<String>,
}

/// `rename-space: { label }`: renames the created Space (or the latest run's) in place.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameSpace {
    pub label: String,
}

/// `rename-thread: { label }`: renames the latest run's thread in place.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RenameThread {
    pub label: String,
}

/// `spaces: { workspace, fields }`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpacesExpect {
    /// Alias of a `workspace-seed` piece; the latest run's workspace when omitted.
    pub workspace: Option<String>,
    pub fields: BTreeMap<String, serde_yaml::Value>,
}

/// `threads: { workspace, count, fields, messages, all, distinct }`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadsExpect {
    /// Alias of a `workspace-seed` piece; the latest run's workspace when omitted.
    pub workspace: Option<String>,
    /// Exactly this many threads are listed.
    pub count: Option<usize>,
    /// Matched against the list entry of the thread (the latest run's, or the seed's).
    pub fields: Option<BTreeMap<String, serde_yaml::Value>>,
    /// Some message of `role` contains the text.
    pub messages: Option<ThreadMessage>,
    /// Matched against every listed thread.
    pub all: Option<BTreeMap<String, serde_yaml::Value>>,
    /// These list fields differ from thread to thread (for example `[threadId]`).
    #[serde(default)]
    pub distinct: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadMessage {
    pub role: String,
    pub contains: String,
}

/// `runs: { count, latest }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunsExpect {
    /// The node lists exactly this many runs of the latest run's agent.
    pub count: Option<usize>,
    /// Fields of the latest run's status (`GET /v1/runs/{id}`, dotted camelCase paths).
    pub latest: Option<BTreeMap<String, serde_yaml::Value>>,
}

// ---- state -----------------------------------------------------------------------------------

/// A started test broker. Dropping it stops the process and removes its state dir.
#[derive(Debug)]
pub struct BrokerProc {
    pub(super) child: Child,
    pub(super) dir: PathBuf,
    /// The id the piece registered the broker under (`manifest.brokerId`).
    pub(super) id: String,
    /// The binary and its arguments (placeholders resolved), to start it again.
    pub(super) bin: String,
    pub(super) args: Vec<String>,
    pub(super) socket: PathBuf,
}

/// Starts a broker process; `gateway` adds `--gateway-socket <path>`.
pub(super) fn spawn_broker(
    bin: &str,
    args: &[String],
    gateway: Option<&str>,
) -> std::io::Result<Child> {
    let mut cmd = Command::new(bin);
    cmd.args(args);
    if let Some(g) = gateway {
        cmd.args(["--gateway-socket", g]);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
}

impl Drop for BrokerProc {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[derive(Debug, Clone, Default)]
struct Fire {
    outcome: String,
    run_id: String,
    reason: String,
}

#[derive(Debug, Default)]
pub struct TrigState {
    pub(super) brokers: Vec<BrokerProc>,
    /// What the latest `fire-trigger` saw.
    fires: Vec<Fire>,
    pub(super) started: u32,
    /// The Space the latest `create-space` made: chats without a place go into it.
    space: Option<String>,
}

impl TrigState {
    pub fn stop(&mut self) {
        self.brokers.clear();
    }
}

fn product(expected: impl Into<String>, observed: impl Into<String>) -> Fail {
    Fail::new(Class::Product, expected, observed)
}

/// A key row is a grant when its holder names a broker.
fn is_grant(row: &Value) -> bool {
    row["holder"]
        .as_str()
        .is_some_and(|h| h.starts_with("broker:"))
}

/// A string with every occurrence of `secret` hidden.
fn hide(text: &str, secret: &str) -> String {
    if secret.is_empty() {
        text.to_string()
    } else {
        text.replace(secret, "<token>")
    }
}

impl Engine {
    // ---- world pieces ------------------------------------------------------------------------

    /// A `broker` piece: starts the broker process and registers it with vamd. The binary is
    /// named by the env var `bin` (default `VAM_E2E_TEST_BROKER`); `args` may use `{{socket}}`
    /// and `{{dir}}`. With `restartWithGateway` the process is started again once vamd reports
    /// its gateway socket (see `schedule.rs`).
    pub(super) fn start_broker_piece(&mut self, wp: &WorldPiece, name: &str) -> Result<(), Fail> {
        let var = wp.bin.as_deref().unwrap_or("VAM_E2E_TEST_BROKER");
        let want = format!("the broker starts ({var})");
        let bin = std::env::var(var)
            .ok()
            .filter(|b| !b.is_empty())
            .ok_or_else(|| {
                Fail::new(
                    Class::Env,
                    &want,
                    format!("{var} is not set (scripts/e2e/run.sh exports it)"),
                )
            })?;
        self.ctx.trig.started += 1;
        // A short path: a Unix socket path is limited to 108 bytes.
        let dir = std::env::temp_dir().join(format!(
            "vtb-{}-{}",
            std::process::id(),
            self.ctx.trig.started
        ));
        std::fs::create_dir_all(&dir)
            .map_err(|e| Fail::new(Class::Env, &want, format!("{}: {e}", dir.display())))?;
        let socket = dir.join("b.sock");
        let args: Vec<String> = match &wp.args {
            Some(a) => a.clone(),
            None => ["--triggers", "--socket", "{{socket}}"]
                .map(String::from)
                .to_vec(),
        }
        .into_iter()
        .map(|a| {
            a.replace("{{socket}}", &socket.to_string_lossy())
                .replace("{{dir}}", &dir.to_string_lossy())
        })
        .collect();
        let child = spawn_broker(&bin, &args, None)
            .map_err(|e| Fail::new(Class::Env, &want, format!("{bin}: {e}")))?;
        let proc = BrokerProc {
            child,
            dir: dir.clone(),
            id: wp
                .body
                .as_ref()
                .and_then(|b| b["manifest"]["brokerId"].as_str())
                .unwrap_or("trigger-test-broker")
                .to_string(),
            bin,
            args,
            socket: socket.clone(),
        };
        let start = Instant::now();
        while !socket.exists() {
            if start.elapsed() > Duration::from_secs(5) {
                return Err(Fail::new(
                    Class::Env,
                    &want,
                    format!("no socket at {} after 5s", socket.display()),
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let text = self.subst(
            wp.body.as_ref().map(Value::to_string).unwrap_or_default(),
            name,
        );
        let text = text.replace("{{socket}}", &socket.to_string_lossy());
        let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let r = self.api.put("/v1/hosted/brokers", &body).map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                format!("PUT /v1/hosted/brokers for piece `{}`", wp.piece),
                format!("{} {}", r.status, r.body),
            ));
        }
        self.ctx.trig.brokers.push(proc);
        if wp.restart_with_gateway {
            self.restart_with_gateway()?;
        }
        Ok(())
    }

    /// A `workspace-seed` piece: copies the fixture's workspace and chunks into the node's
    /// Drive9 store. A chunk already there is the same bytes (they are named by their hash).
    pub(super) fn seed_workspace_piece(&mut self, wp: &WorldPiece) -> Result<(), Fail> {
        let want = format!("the workspace of piece `{}` is seeded", wp.piece);
        let fixture = wp
            .fixture_dir
            .clone()
            .ok_or_else(|| Fail::new(Class::Scenario, &want, "the piece names no `fixture`"))?;
        let state = self.vamd.state_dir().ok_or_else(|| {
            Fail::new(
                Class::Env,
                &want,
                "the runner does not own the node's state",
            )
        })?;
        let to = state.join("drive9");
        copy_missing(&fixture, &to)
            .map_err(|e| Fail::new(Class::Harness, want, format!("{}: {e}", fixture.display())))
    }

    // ---- steps -------------------------------------------------------------------------------

    pub(super) fn fire_trigger(&mut self, f: &FireTrigger) -> Result<(), Fail> {
        let want = "the test broker fires the grant";
        let bin = std::env::var("VAM_E2E_TEST_BROKER")
            .ok()
            .filter(|b| !b.is_empty())
            .ok_or_else(|| Fail::new(Class::Env, want, "VAM_E2E_TEST_BROKER is not set"))?;
        let (dir, broker_id) = self
            .ctx
            .trig
            .brokers
            .first()
            .map(|b| (b.dir.clone(), b.id.clone()))
            .ok_or_else(|| {
                Fail::new(
                    Class::Scenario,
                    "a `trigger-test-broker` piece in `given`",
                    "none",
                )
            })?;
        let i = self.last_run()?;
        // vamd reports the broker's gateway socket; empty until the gateway exists.
        let r = self
            .api
            .get(&format!("/v1/hosted/brokers/{}", seg(&broker_id)))
            .map_err(conn)?;
        let gateway = r.json()["broker"]["gatewaySocket"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !r.ok() || gateway.is_empty() {
            return Err(Fail::new(
                Class::Product,
                want,
                format!(
                    "vamd reports no gatewaySocket for broker `{broker_id}` ({} {})",
                    r.status,
                    r.body.chars().take(200).collect::<String>()
                ),
            ));
        }
        let out = Command::new(&bin)
            .args(["fire", "--state-dir"])
            .arg(&dir)
            .args(["--gateway-socket", &gateway])
            .args(["--key", &f.key, "--times", &f.times.to_string()])
            .args(["--kind", f.kind.word()])
            .output()
            .map_err(|e| Fail::new(Class::Env, want, format!("{bin}: {e}")))?;
        if !out.status.success() {
            return Err(Fail::new(
                Class::Env,
                want,
                format!(
                    "`fire` exited {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                ),
            ));
        }
        let mut fires = Vec::new();
        for line in String::from_utf8_lossy(&out.stdout).lines() {
            let Ok(v) = serde_json::from_str::<Value>(line) else {
                continue;
            };
            let reason = ["denyReason", "busyReason"]
                .iter()
                .filter_map(|k| v[*k].as_str())
                .find(|s| !s.is_empty())
                .unwrap_or_default();
            fires.push(Fire {
                outcome: v["outcome"].as_str().unwrap_or_default().to_string(),
                run_id: v["runId"].as_str().unwrap_or_default().to_string(),
                reason: reason.to_string(),
            });
        }
        if fires.len() != f.times as usize {
            return Err(Fail::new(
                Class::Harness,
                format!("{} outcome lines from `fire`", f.times),
                format!("{} lines", fires.len()),
            ));
        }
        // A run the fire started or queued becomes the latest run, as a queued message does.
        let base = self.ctx.runs[i].clone();
        for fire in &fires {
            let fresh = matches!(fire.outcome.as_str(), "ACCEPTED" | "QUEUED")
                && !fire.run_id.is_empty()
                && !self.ctx.runs.iter().any(|r| r.run_id == fire.run_id);
            if fresh {
                self.ctx.runs.push(crate::engine::RunRec {
                    run_id: fire.run_id.clone(),
                    session_id: String::new(),
                    state: String::new(),
                    steps: Vec::new(),
                    error: String::new(),
                    events: Vec::new(),
                    ..base.clone()
                });
            }
        }
        self.ctx.trig.fires = fires;
        Ok(())
    }

    pub(super) fn expect_fire(&mut self, x: &FireExpect) -> Result<(), Fail> {
        let fires = &self.ctx.trig.fires;
        if fires.is_empty() {
            return Err(Fail::new(
                Class::Scenario,
                "an earlier `fire-trigger` step",
                "none",
            ));
        }
        let got: Vec<&str> = fires.iter().map(|f| f.outcome.as_str()).collect();
        if got != x.outcomes.iter().map(String::as_str).collect::<Vec<_>>() {
            return Err(product(
                format!("the fires answer {:?}", x.outcomes),
                format!("{got:?}"),
            ));
        }
        if let Some(r) = &x.reason {
            let last = &fires[fires.len() - 1].reason;
            if last != r {
                return Err(product(
                    format!("the last fire's reason is {r}"),
                    format!("`{last}`"),
                ));
            }
        }
        if x.same_run == Some(true) {
            let first = &fires[0].run_id;
            if first.is_empty() || fires.iter().any(|f| &f.run_id != first) {
                let ids: Vec<&str> = fires.iter().map(|f| f.run_id.as_str()).collect();
                return Err(product(
                    "every fire names the first fire's run",
                    format!("{ids:?}"),
                ));
            }
        }
        Ok(())
    }

    /// The grants the user's keys list holds (`GET /v1/keys?holder=broker`).
    fn list_grants(&self) -> Result<Vec<Value>, Fail> {
        let key = self.last_run().ok().and_then(|i| self.run_key(i));
        let r = self
            .api
            .get_as(key.as_deref(), "/v1/keys?holder=broker")
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(
                "the grants are listed",
                format!("GET /v1/keys?holder=broker -> {} {}", r.status, r.body),
            ));
        }
        // One node serves every scenario of a run, and its users overlap: keep the grants that
        // point at a Space one of this scenario's runs ran in.
        let spaces: Vec<&str> = self
            .ctx
            .runs
            .iter()
            .map(|r| r.space_id.as_str())
            .filter(|s| !s.is_empty())
            .collect();
        Ok(r.json()["keys"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|k| is_grant(k))
                    .filter(|k| k["targetId"].as_str().is_some_and(|t| spaces.contains(&t)))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default())
    }

    pub(super) fn expect_grant(&mut self, x: &GrantExpect) -> Result<(), Fail> {
        let tool = x.tool.clone();
        let mine = |g: &Vec<Value>| -> Vec<Value> {
            g.iter()
                .filter(|k| {
                    tool.as_deref()
                        .is_none_or(|t| k["toolName"].as_str() == Some(t))
                })
                .cloned()
                .collect()
        };
        let what = match (&x.tool, x.count, x.absent) {
            (Some(t), Some(n), _) => format!("{n} grant(s) issued through `{t}`"),
            (None, Some(n), _) => format!("{n} grant(s)"),
            (Some(t), None, Some(true)) => format!("no grant issued through `{t}`"),
            (None, None, Some(true)) => "no grant".to_string(),
            (Some(t), None, _) => format!("a grant issued through `{t}`"),
            (None, None, _) => "a grant".to_string(),
        };
        // A grant appears when the approved call returns: give it the settle time.
        let start = Instant::now();
        loop {
            let all = self.list_grants()?;
            let rows = mine(&all);
            let met = match (x.absent, x.count) {
                (Some(true), _) => rows.is_empty(),
                (_, Some(n)) => rows.len() == n,
                _ => !rows.is_empty(),
            };
            let bad = match (met, rows.first(), &x.fields) {
                (true, Some(row), Some(fields)) => field_problems(row, fields),
                _ => Vec::new(),
            };
            if met && bad.is_empty() {
                return Ok(());
            }
            if start.elapsed() >= self.cfg.settle {
                let seen = format!("{} grant(s) listed", rows.len());
                return Err(product(
                    what,
                    if bad.is_empty() { seen } else { bad.join("; ") },
                ));
            }
            std::thread::sleep(self.cfg.poll);
        }
    }

    pub(super) fn revoke_grant(&mut self, _r: &RevokeGrant) -> Result<(), Fail> {
        let want = "the latest grant is revoked (DELETE /v1/keys/{id})";
        let grants = self.list_grants()?;
        let id = grants
            .iter()
            .rev()
            .find(|g| g["revokedAt"].is_null() || g.get("revokedAt").is_none())
            .and_then(|g| {
                g["keyId"]["urn"]
                    .as_str()
                    .or(g["keyId"].as_str())
                    .or(g["id"].as_str())
            })
            .map(String::from)
            .ok_or_else(|| product("a grant to revoke", "no grant is listed"))?;
        let r = self
            .api
            .delete(&format!("/v1/keys/{}", seg(&id)))
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(want, format!("{} {}", r.status, r.body)));
        }
        Ok(())
    }

    pub(super) fn bearer_request(
        &mut self,
        b: &BearerRequest,
        scenario_id: &str,
    ) -> Result<(), Fail> {
        let want = format!(
            "{} {} with the grant as bearer answers {}",
            b.method.to_uppercase(),
            b.path,
            b.status
        );
        let dir = self
            .ctx
            .trig
            .brokers
            .first()
            .map(|b| b.dir.clone())
            .ok_or_else(|| {
                Fail::new(
                    Class::Scenario,
                    "a `trigger-test-broker` piece in `given`",
                    "none",
                )
            })?;
        let full = std::fs::read_to_string(dir.join("grant-token"))
            .map_err(|e| {
                product(
                    "the test broker holds the grant token",
                    format!("grant-token: {e}"),
                )
            })?
            .trim()
            .to_string();
        let token = match b.token {
            TokenForm::Grant => full.clone(),
            TokenForm::GrantSecret => full
                .split_once('.')
                .map(|(_, s)| s)
                .unwrap_or("")
                .to_string(),
        };
        if token.is_empty() {
            return Err(product(
                "the grant token has a secret part",
                "the token has no `.`",
            ));
        }
        // The body is a real run start, so an accepted token would start a run.
        let i = self.last_run()?;
        let (urn, thread, identity) = {
            let r = &self.ctx.runs[i];
            (
                r.agent_urn.clone(),
                r.thread_ref.clone(),
                r.identity.clone(),
            )
        };
        let slug = self.workspace_slug(&urn, identity.as_deref(), None);
        let prompt = format!("e2e-step:bearer Start a run mock-script:{scenario_id}");
        let body = self.run_body(&urn, &slug, &thread, &prompt, None, None);
        let reply = match b.method.to_lowercase().as_str() {
            "get" => self.api.get_as(Some(&token), &b.path),
            _ => self.api.post_as(Some(&token), &b.path, &body),
        }
        .map_err(conn)?;
        if reply.status != b.status {
            return Err(product(
                want,
                hide(&format!("{} {}", reply.status, reply.body), &token).replace(&full, "<token>"),
            ));
        }
        Ok(())
    }

    // ---- ids ---------------------------------------------------------------------------------

    /// The Space id and thread id an expect reads: a seed piece's, or the latest run's.
    fn place_of(&self, alias: Option<&str>) -> Result<(String, String, String), Fail> {
        match alias {
            Some(a) => {
                let rec = self.ctx.pieces.get(a).ok_or_else(|| {
                    Fail::new(Class::Scenario, format!("piece `{a}` in `given`"), "none")
                })?;
                let piece = self
                    .pieces
                    .get(&rec.piece)
                    .ok_or_else(|| Fail::new(Class::Harness, "the piece is known", a))?;
                let ws = piece.workspace_id.clone().ok_or_else(|| {
                    Fail::new(
                        Class::Scenario,
                        format!("piece `{a}` names a `workspaceId`"),
                        "none",
                    )
                })?;
                let thread = piece.thread.clone().unwrap_or_default();
                Ok((ws, thread, String::new()))
            }
            None => {
                let r = &self.ctx.runs[self.last_run()?];
                Ok((r.workspace_id.clone(), r.thread_id.clone(), String::new()))
            }
        }
    }

    fn list_threads(&self, ws: &str) -> Result<Vec<Value>, Fail> {
        let r = self
            .api
            .get(&format!("/v1/workspaces/{}/threads", seg(ws)))
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(
                "the workspace's threads are listed",
                format!("GET threads -> {} {}", r.status, r.body),
            ));
        }
        Ok(r.json()["threads"].as_array().cloned().unwrap_or_default())
    }

    /// The id the node reports for the latest run's thread; a chat can then name it.
    pub(super) fn latest_thread_id(&self) -> Result<String, Fail> {
        let (ws, thread, _) = self.place_of(None)?;
        let want = "the thread list names the thread with an id (th_...)";
        let threads = self.list_threads(&ws)?;
        let row = threads
            .iter()
            .find(|t| t["threadId"].as_str() == Some(thread.as_str()))
            .ok_or_else(|| product(want, format!("no entry for {thread}: {threads:?}")))?;
        match row["threadId"].as_str().filter(|i| i.starts_with("th_")) {
            Some(id) => Ok(id.to_string()),
            None => Err(product(want, format!("threadId {}", row["threadId"]))),
        }
    }

    pub(super) fn expect_spaces(&mut self, x: &SpacesExpect) -> Result<(), Fail> {
        let (ws, _, _) = self.place_of(x.workspace.as_deref())?;
        let want = format!("GET /v1/spaces lists the Space of workspace {ws}");
        let r = self.api.get("/v1/spaces?pageSize=1000").map_err(conn)?;
        if !r.ok() {
            return Err(product(
                want,
                format!("GET /v1/spaces -> {} {}", r.status, r.body),
            ));
        }
        let body = r.json();
        let spaces = body["spaces"].as_array().cloned().unwrap_or_default();
        let Some(space) = spaces
            .iter()
            .find(|s| s["workspaceId"].as_str() == Some(&ws))
        else {
            return Err(product(
                want,
                format!("{} Space(s), none for it", spaces.len()),
            ));
        };
        let bad = field_problems(space, &x.fields);
        if bad.is_empty() {
            Ok(())
        } else {
            Err(product(
                format!("{want} with the expected fields"),
                bad.join("; "),
            ))
        }
    }

    pub(super) fn expect_threads(&mut self, x: &ThreadsExpect) -> Result<(), Fail> {
        let (ws, thread, _) = self.place_of(x.workspace.as_deref())?;
        let threads = self.list_threads(&ws)?;
        if let Some(n) = x.count {
            if threads.len() != n {
                return Err(product(
                    format!("{n} thread(s) in workspace {ws}"),
                    format!("{} thread(s): {threads:?}", threads.len()),
                ));
            }
        }
        let row = threads
            .iter()
            .find(|t| t["threadId"].as_str() == Some(thread.as_str()));
        for field in &x.distinct {
            let values: Vec<String> = threads
                .iter()
                .map(|t| t[field.as_str()].to_string())
                .collect();
            let unique: std::collections::BTreeSet<_> = values.iter().collect();
            if unique.len() != values.len() {
                return Err(product(
                    format!("every thread has its own {field}"),
                    format!("{values:?}"),
                ));
            }
        }
        if let Some(all) = &x.all {
            for t in &threads {
                let bad = field_problems(t, all);
                if !bad.is_empty() {
                    return Err(product("every thread entry has the fields", bad.join("; ")));
                }
            }
        }
        if let Some(fields) = &x.fields {
            let Some(row) = row else {
                return Err(product(
                    format!("the thread list has an entry for {thread}"),
                    format!("{threads:?}"),
                ));
            };
            let bad = field_problems(row, fields);
            if !bad.is_empty() {
                return Err(product("the thread entry has the fields", bad.join("; ")));
            }
        }
        if let Some(m) = &x.messages {
            let segment = row
                .and_then(|r| r["threadId"].as_str())
                .map(String::from)
                .ok_or_else(|| product("the thread has an id", "no entry"))?;
            let r = self
                .api
                .get(&format!(
                    "/v1/workspaces/{}/threads/{}/messages?view=all&limit=500",
                    seg(&ws),
                    seg(&segment)
                ))
                .map_err(conn)?;
            let what = format!(
                "the thread, read by its id ({segment}), has a {} message containing {:?}",
                m.role, m.contains
            );
            if !r.ok() {
                return Err(product(what, format!("{} {}", r.status, r.body)));
            }
            if !super::thread_has_message(&r.json(), &m.role, &m.contains) {
                return Err(product(what, "no such message"));
            }
        }
        Ok(())
    }

    pub(super) fn expect_runs(&mut self, x: &RunsExpect) -> Result<(), Fail> {
        let i = self.last_run()?;
        let key = self.run_key(i);
        if let Some(want) = x.count {
            let urn = self.ctx.runs[i].agent_urn.clone();
            let r = self
                .api
                .get_as(key.as_deref(), "/v1/runs?pageSize=1000")
                .map_err(conn)?;
            if !r.ok() {
                return Err(product(
                    "the runs are listed",
                    format!("GET /v1/runs -> {} {}", r.status, r.body),
                ));
            }
            let body = r.json();
            let n = body["statuses"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter(|s| s["agentUrn"].as_str() == Some(&urn))
                        .count()
                })
                .unwrap_or(0);
            if n != want {
                return Err(product(
                    format!("{want} run(s) of the agent"),
                    format!("{n} run(s) listed"),
                ));
            }
        }
        if let Some(fields) = &x.latest {
            let id = self.ctx.runs[i].run_id.clone();
            let r = self
                .api
                .get_as(key.as_deref(), &format!("/v1/runs/{}", seg(&id)))
                .map_err(conn)?;
            if !r.ok() {
                return Err(product(
                    "the latest run's status is read",
                    format!("{} {}", r.status, r.body),
                ));
            }
            let status = r.json()["status"].clone();
            let bad = field_problems(&status, fields);
            if !bad.is_empty() {
                return Err(product(
                    "the latest run's status has the fields",
                    bad.join("; "),
                ));
            }
        }
        Ok(())
    }
}

/// Copies the files under `from` into `to`, keeping any file that is already there.
fn copy_missing(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for e in std::fs::read_dir(from)? {
        let e = e?;
        let dest = to.join(e.file_name());
        if e.file_type()?.is_dir() {
            copy_missing(&e.path(), &dest)?;
        } else if !dest.exists() {
            std::fs::copy(e.path(), dest)?;
        }
    }
    Ok(())
}

impl Engine {
    /// The Space a `create-space` step made; a chat that names no place goes into it.
    pub(super) fn created_space(&self) -> Option<String> {
        self.ctx.trig.space.clone()
    }

    /// The Space a rename step acts on: the created one, else the latest run's.
    fn target_space(&self) -> Result<String, Fail> {
        match self.created_space() {
            Some(id) => Ok(id),
            None => Ok(self.ctx.runs[self.last_run()?].space_id.clone()),
        }
    }

    pub(super) fn create_space(&mut self, c: &CreateSpace) -> Result<(), Fail> {
        let (_, urn) = self.agent_of(c.agent.as_deref())?;
        let want = format!("POST /v1/spaces makes a Space labelled {:?}", c.label);
        let r = self
            .api
            .post("/v1/spaces", &json!({ "agentUrn": urn, "label": c.label }))
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(want, format!("{} {}", r.status, r.body)));
        }
        let v = r.json();
        let id = v["spaceId"].as_str().unwrap_or_default();
        if !id.starts_with("sp_") || v["name"].as_str() != Some(c.label.trim()) {
            return Err(product(want, r.body.clone()));
        }
        self.ctx.trig.space = Some(id.to_string());
        Ok(())
    }

    /// The Space as `GET /v1/spaces/{id}` shows it.
    fn read_space(&self, id: &str, want: &str) -> Result<Value, Fail> {
        let r = self
            .api
            .get(&format!("/v1/spaces/{}", seg(id)))
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(
                want,
                format!("GET space -> {} {}", r.status, r.body),
            ));
        }
        Ok(r.json())
    }

    pub(super) fn rename_space(&mut self, x: &RenameSpace) -> Result<(), Fail> {
        let id = self.target_space()?;
        let want = format!("PATCH /v1/spaces/{id} renames the Space in place");
        let before = self.read_space(&id, &want)?;
        let r = self
            .api
            .patch(
                &format!("/v1/spaces/{}", seg(&id)),
                &json!({ "label": x.label }),
            )
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(want, format!("{} {}", r.status, r.body)));
        }
        let after = self.read_space(&id, &want)?;
        let ids = |v: &Value| -> Vec<Value> {
            v["threads"]
                .as_array()
                .map(|t| t.iter().map(|t| t["threadId"].clone()).collect())
                .unwrap_or_default()
        };
        if after["spaceId"].as_str() != Some(id.as_str())
            || after["workspaceId"] != before["workspaceId"]
            || after["name"].as_str() != Some(x.label.trim())
            || ids(&after) != ids(&before)
        {
            return Err(product(
                format!("{want}: new label, same ids"),
                format!("before {before}, after {after}"),
            ));
        }
        Ok(())
    }

    pub(super) fn rename_thread(&mut self, x: &RenameThread) -> Result<(), Fail> {
        let (space, thread) = {
            let r = &self.ctx.runs[self.last_run()?];
            (r.space_id.clone(), r.thread_id.clone())
        };
        let want = format!("PATCH /v1/spaces/{space}/threads/{thread} renames the thread in place");
        let r = self
            .api
            .patch(
                &format!("/v1/spaces/{}/threads/{}", seg(&space), seg(&thread)),
                &json!({ "label": x.label }),
            )
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(want, format!("{} {}", r.status, r.body)));
        }
        let shown = self.read_space(&space, &want)?;
        let row = shown["threads"].as_array().and_then(|t| {
            t.iter()
                .find(|t| t["threadId"].as_str() == Some(thread.as_str()))
        });
        match row {
            Some(t) if t["name"].as_str() == Some(x.label.trim()) => Ok(()),
            other => Err(product(
                format!("{want}: new label, same id"),
                format!("{other:?} in {shown}"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn steps_parse_and_reject_unknown_keys() {
        let f: FireTrigger = serde_yaml::from_str("{ key: k1, times: 3 }").unwrap();
        assert_eq!((f.times, f.kind), (3, FireKind::Scheduled));
        assert!(serde_yaml::from_str::<FireTrigger>("{ key: k, tims: 3 }").is_err());
        let b: BearerRequest =
            serde_yaml::from_str("{ path: /v1/runs, token: grant-secret, status: 401 }").unwrap();
        assert_eq!(
            (b.method.as_str(), b.token),
            ("post", TokenForm::GrantSecret)
        );
    }

    #[test]
    fn only_a_broker_holder_is_a_grant() {
        assert!(is_grant(&json!({ "holder": "broker:abc" })));
        assert!(!is_grant(&json!({ "holder": "http" })));
        assert!(!is_grant(&json!({ "keyId": "k" })));
    }

    #[test]
    fn a_token_never_reaches_a_message() {
        assert_eq!(hide("401 token=abc.def", "abc.def"), "401 token=<token>");
    }

    #[test]
    fn seeding_keeps_existing_files() {
        let tmp = std::env::temp_dir().join(format!("vam-e2e-seed-{}", std::process::id()));
        let (from, to) = (tmp.join("from"), tmp.join("to"));
        std::fs::create_dir_all(from.join("a")).unwrap();
        std::fs::create_dir_all(to.join("a")).unwrap();
        std::fs::write(from.join("a/x"), "new").unwrap();
        std::fs::write(from.join("a/y"), "new").unwrap();
        std::fs::write(to.join("a/x"), "old").unwrap();
        copy_missing(&from, &to).unwrap();
        assert_eq!(std::fs::read_to_string(to.join("a/x")).unwrap(), "old");
        assert_eq!(std::fs::read_to_string(to.join("a/y")).unwrap(), "new");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
