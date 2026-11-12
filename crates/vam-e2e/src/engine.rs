//! The engine: arranges world pieces and executes steps against a vamd over HTTP.

use crate::api::{seg, Api, ConnError, Reply};
use crate::events::bare;
use crate::runner::{piece_name, Arranged, RunTag, StepExecutor, StepOutcome, World};
use crate::scenario::{
    Chat, Class, DecideGate, DecideOpts, Needs, Scenario, Step, ThreadMode, ThreadSel,
};
use crate::vamd::Vamd;
use crate::world::{PieceKind, WorldPiece};
use serde_json::{json, Value};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::time::{Duration, Instant};

pub const MOCK_MODEL: &str = "mock-model";

#[derive(Debug, Clone)]
pub struct Cfg {
    pub tag: RunTag,
    pub step_budget: Duration,
    pub scenario_budget: Duration,
    /// How long after a run is terminal an expectation may still become true.
    pub settle: Duration,
    pub health_timeout: Duration,
    pub poll: Duration,
}

impl Cfg {
    pub fn new(tag: RunTag) -> Cfg {
        Cfg {
            tag,
            step_budget: Duration::from_secs(30),
            scenario_budget: Duration::from_secs(120),
            settle: Duration::from_secs(5),
            health_timeout: Duration::from_secs(30),
            poll: Duration::from_millis(100),
        }
    }
}

/// `15s`, `2m`, `300ms`.
pub fn parse_duration(s: &str) -> Option<Duration> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    match unit {
        "ms" => Some(Duration::from_millis(n)),
        "s" => Some(Duration::from_secs(n)),
        "m" => Some(Duration::from_secs(n * 60)),
        _ => None,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fail {
    pub class: Class,
    pub expected: String,
    pub observed: String,
}

impl Fail {
    pub fn new(class: Class, expected: impl Into<String>, observed: impl Into<String>) -> Fail {
        Fail {
            class,
            expected: expected.into(),
            observed: observed.into(),
        }
    }
}

fn conn(e: ConnError) -> Fail {
    Fail::new(
        Class::Env,
        "vamd answers",
        format!("connection failed: {}", e.0),
    )
}

pub fn is_terminal(state: &str) -> bool {
    matches!(
        state,
        "RUN_STATE_SUCCEEDED"
            | "RUN_STATE_FAILED"
            | "RUN_STATE_CANCELLED"
            | "RUN_STATE_TIMED_OUT"
            | "RUN_STATE_EXPIRED"
            | "RUN_STATE_REJECTED"
    )
}

/// A key that never prints: `Debug` hides it, and nothing writes it to a brief or summary.
#[derive(Clone)]
struct Secret(String);

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}

/// `n` random bytes as hex: the OS source, else a per-process random hasher seeded with the clock.
fn secret_hex(n: usize) -> String {
    use std::io::Read;
    let mut buf = vec![0u8; n];
    let from_os = std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .is_ok();
    if !from_os {
        use std::hash::{BuildHasher, Hasher};
        for chunk in buf.chunks_mut(8) {
            let mut h = std::collections::hash_map::RandomState::new().build_hasher();
            h.write_u128(
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_nanos())
                    .unwrap_or(0),
            );
            let v = h.finish().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// The suffix of trial `n` (from 1) on `model`: `k<n><4 hex of the scenario id and model name>`. Short, because it
/// lands in resource names that vamd limits.
pub fn trial_suffix(scenario: &str, model: &str, n: u32) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    scenario.hash(&mut h);
    model.hash(&mut h);
    format!("k{n}{:04x}", h.finish() as u16)
}

/// The user name of a created identity: `name` when vamd accepts it (at most 64 characters),
/// else the alias cut short with a hash of the full name and the run tag.
fn identity_name(name: &str, alias: &str, tag: &str) -> String {
    if name.len() <= 64 {
        return name.to_string();
    }
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    let hash = format!("{:08x}", h.finish() as u32);
    let room = 64usize.saturating_sub(hash.len() + tag.len() + 2);
    let prefix: String = alias.chars().take(room).collect();
    format!("{prefix}-{hash}-{tag}")
}

#[derive(Debug, Clone)]
struct PieceRec {
    kind: PieceKind,
    /// The world piece this alias names.
    piece: String,
    urn: String,
    /// The run-unique name the piece was made under (`{{name:<piece>}}`).
    name: String,
}

#[derive(Debug, Clone, Default)]
pub struct RunRec {
    pub agent: String,
    /// Alias of the `org` piece the run was started as, when given.
    pub identity: Option<String>,
    pub run_id: String,
    pub session_id: String,
    pub workspace_id: String,
    /// The thread's label as the node reports it (`threadRef`).
    pub thread_ref: String,
    /// The Space and thread ids the node reports; follow-ups and thread reads use them.
    pub space_id: String,
    pub thread_id: String,
    /// The agent resource the run was started on.
    pub agent_urn: String,
    pub state: String,
    pub steps: Vec<Value>,
    pub error: String,
    pub events: Vec<Value>,
    cursor: i64,
    terminal_at: Option<Instant>,
}

#[derive(Debug, Default)]
struct Ctx {
    pieces: HashMap<String, PieceRec>,
    order: Vec<String>,
    runs: Vec<RunRec>,
    last_refusal: Option<String>,
    /// A wait ended on the step or scenario budget (not on the product failing).
    budget_timeout: bool,
    threads: u32,
    /// Keys of the identities that `org` pieces created, by piece alias.
    identities: HashMap<String, Secret>,
    /// Username, user id, passwords and sessions of those identities (`identity.rs`).
    accounts: HashMap<String, identity::Account>,
    /// Values of `secret` pieces, by alias. Never printed.
    secrets: HashMap<String, Secret>,
    /// Ids of the org MCP connections that `mcp-connection` pieces made; removed at cleanup.
    mcp_connections: Vec<String>,
    /// What the latest `flood` step saw.
    flood: Option<FloodSeen>,
    /// Test brokers and the latest fire (`triggers.rs`).
    trig: triggers::TrigState,
    /// The schedule broker's gateway socket (`schedule.rs`).
    sched: schedule::SchedState,
    /// Connector steps and mock providers (`connectors.rs`).
    conn: connectors::ConnState,
    /// The scratch tree the latest `cli` step ran on (`root: { same: true }` reuses it).
    cli_root: Option<std::path::PathBuf>,
    /// The secret a `cli` step captured (`{{secret}}` in later `cli` steps). Never printed.
    cli_secret: Option<String>,
    /// Marketplace steps (`marketplace.rs`).
    mkt: marketplace::MktState,
}

type Check = Box<dyn Fn(&mut Engine) -> Result<Probe, Fail>>;

fn probe(met: bool) -> Probe {
    if met {
        Probe::Met
    } else {
        Probe::Pending
    }
}

enum Probe {
    Met,
    Pending,
    Impossible(String),
}

enum WaitEnd {
    Fail(Fail),
    Timeout { scenario_budget: bool },
    Impossible(String),
}

#[path = "channels.rs"]
pub mod channels;
#[path = "cli.rs"]
pub mod cli;
#[path = "connectors.rs"]
pub mod connectors;
#[path = "effects.rs"]
pub mod effects;
#[path = "identity.rs"]
pub mod identity;
#[path = "marketplace.rs"]
pub mod marketplace;
#[path = "notifications.rs"]
pub mod notifications;
#[path = "outcomes.rs"]
pub mod outcomes;
#[path = "schedule.rs"]
pub mod schedule;
#[path = "steer.rs"]
pub mod steer;
#[path = "triggers.rs"]
pub mod triggers;

pub struct Engine {
    key_seq: u32,
    pub api: Api,
    pub vamd: Box<dyn Vamd>,
    pub cfg: Cfg,
    pieces: HashMap<String, WorldPiece>,
    ctx: Ctx,
    deadline: Instant,
    prepare_error: Option<Fail>,
    node_mode: Option<Option<String>>,
    /// The model each run is pinned to (`inputs.model`). `mock-model` unless a bench trial sets one.
    model: String,
    /// The harness profile every chat names (`inputs.harness.profile`); none means `base`.
    profile: Option<String>,
    /// Real models the node allows besides `mock-model` (a bench selection).
    extra_models: Vec<String>,
    /// A `set-node` step changed the node's model settings; the next scenario restores them.
    node_models_touched: bool,
    /// A scenario named a default gateway; the next scenario puts `vercel` back.
    node_gateway_touched: bool,
    /// The `model_provider` input a bench run sends (`vercel` routes through the AI Gateway).
    model_provider: Option<String>,
    /// The guest mode every scenario needs, in place of its own `needs` (a bench on the vm node).
    mode_override: Option<&'static str>,
    /// Bench trial identity appended to the run tag: empty for every non-bench run.
    trial_suffix: String,
    next_trial: Option<String>,
    /// The runner-hosted mock providers, when the run needs them.
    connector_mocks: Option<std::sync::Arc<connectors::Mocks>>,
    /// Where marketplace pieces make their repositories.
    marketplace_root: Option<std::path::PathBuf>,
}

impl Engine {
    pub fn new(api: Api, vamd: Box<dyn Vamd>, cfg: Cfg, pieces: Vec<WorldPiece>) -> Engine {
        Engine {
            api,
            vamd,
            deadline: Instant::now() + cfg.scenario_budget,
            cfg,
            pieces: pieces.into_iter().map(|p| (p.piece.clone(), p)).collect(),
            ctx: Ctx::default(),
            key_seq: 0,
            prepare_error: None,
            node_mode: None,
            model: MOCK_MODEL.to_string(),
            profile: None,
            extra_models: Vec::new(),
            node_models_touched: false,
            node_gateway_touched: false,
            model_provider: None,
            mode_override: None,
            trial_suffix: String::new(),
            next_trial: None,
            connector_mocks: None,
            marketplace_root: None,
        }
    }

    /// The mock providers `mock-provider` pieces use (started by the runner before vamd).
    pub fn set_connector_mocks(&mut self, m: std::sync::Arc<connectors::Mocks>) {
        self.connector_mocks = Some(m);
    }

    /// Real models the node should allow (call before `prepare`).
    pub fn allow_models(&mut self, models: &[String]) {
        self.extra_models = models
            .iter()
            .filter(|m| m.as_str() != MOCK_MODEL)
            .cloned()
            .collect();
    }

    /// Every scenario runs on a node of this guest mode (`"vm"`), whatever its `needs` says.
    pub fn set_node_mode(&mut self, mode: &'static str) {
        self.mode_override = Some(mode);
    }

    /// Sends `inputs.model_provider` with real-model runs (the mock never gets it).
    pub fn set_model_provider(&mut self, provider: &str) {
        self.model_provider = Some(provider.to_string());
    }

    /// Names this harness profile in every run's `inputs.harness`.
    pub fn set_profile(&mut self, profile: &str) {
        self.profile = Some(profile.to_string());
    }

    /// Pins the following runs to this model.
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
    }

    /// What the node says about itself, for the bench gates: `guestMode` and the binding ids in
    /// its org secret store (metadata only; no value is read).
    pub fn node_facts(&mut self) -> Result<(Option<String>, Vec<String>), String> {
        let node = self.api.get("/v1/node").map_err(|e| e.0)?;
        if !node.ok() {
            return Err(format!("GET /v1/node answered {}", node.status));
        }
        let mode = node.json()["status"]["guestMode"]
            .as_str()
            .map(String::from);
        let secrets = self.api.get("/v1/secrets?scope=org").map_err(|e| e.0)?;
        if !secrets.ok() {
            return Err(format!(
                "GET /v1/secrets?scope=org answered {} (the runner's key must be allowed to list secrets)",
                secrets.status
            ));
        }
        let ids = secrets.json()["secrets"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|m| m["bindingId"].as_str().or(m["binding_id"].as_str()))
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        Ok((mode, ids))
    }

    /// Starts nothing: waits for health and allows the mock model. A failure is kept and reported
    /// as class `env` at the first step of every scenario.
    pub fn prepare(&mut self) {
        self.prepare_error = self.prepare_inner().err();
    }

    fn prepare_inner(&mut self) -> Result<(), Fail> {
        self.wait_healthy()?;
        self.put_node_models(&self.base_models(), None, None)
    }

    /// The models the runner allows on the node: the mock model and any real ones it was given.
    fn base_models(&self) -> Vec<String> {
        let mut allowed = vec![MOCK_MODEL.to_string()];
        allowed.extend(self.extra_models.iter().cloned());
        allowed
    }

    /// `PUT /v1/node/allowed-models`; `default` is sent only when set (`""` clears it).
    fn put_node_models(
        &self,
        models: &[String],
        default: Option<&str>,
        gateway: Option<&str>,
    ) -> Result<(), Fail> {
        let mut body = json!({ "models": models });
        if let Some(d) = default {
            body["defaultModel"] = json!(d);
        }
        if let Some(g) = gateway {
            body["defaultGateway"] = json!(g);
        }
        let r = self
            .api
            .put("/v1/node/allowed-models", &body)
            .map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Env,
                "mock model allowed on the node",
                format!("PUT /v1/node/allowed-models -> {} {}", r.status, r.body),
            ));
        }
        Ok(())
    }

    fn set_node(&mut self, n: &crate::scenario::SetNode) -> Result<(), Fail> {
        let mut body = json!({
            "models": n.allowed_models,
            "defaultModel": n.default_model.clone().unwrap_or_default(),
        });
        if let Some(g) = &n.default_gateway {
            body["defaultGateway"] = json!(g);
        }
        let r = self
            .api
            .put("/v1/node/allowed-models", &body)
            .map_err(conn)?;
        if r.ok() {
            self.node_models_touched = true;
            self.node_gateway_touched |= n.default_gateway.is_some();
            return Ok(());
        }
        Err(Fail::new(
            Class::Product,
            "the node takes its model settings",
            format!("PUT /v1/node/allowed-models -> {} {}", r.status, r.body),
        ))
    }

    pub fn wait_healthy(&mut self) -> Result<(), Fail> {
        let start = Instant::now();
        loop {
            if self.api.healthy() {
                return Ok(());
            }
            if !self.vamd.alive() {
                return Err(Fail::new(
                    Class::Env,
                    "vamd healthy",
                    format!("vamd exited. Log tail:\n{}", self.vamd.log_tail(15)),
                ));
            }
            if start.elapsed() > self.cfg.health_timeout {
                return Err(Fail::new(
                    Class::Env,
                    "vamd healthy",
                    format!("no healthy answer in {:?}", self.cfg.health_timeout),
                ));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Resets per-scenario state and starts the scenario clock.
    /// Names the next scenario run as bench trial `n` of a model, or as a plain run (`None`).
    /// Call before `run_scenario`, which then starts the scenario.
    pub fn set_trial(&mut self, scenario: &str, t: Option<(&str, u32)>) {
        self.next_trial = t.map(|(m, n)| trial_suffix(scenario, m, n));
    }

    pub fn begin(&mut self) {
        self.trial_suffix = self.next_trial.take().unwrap_or_default();
        self.begin_inner();
    }

    /// The run tag with the trial suffix: the tag itself outside a bench.
    fn eff_tag(&self) -> RunTag {
        if self.trial_suffix.is_empty() {
            self.cfg.tag.clone()
        } else {
            RunTag(format!("{}-{}", self.cfg.tag.0, self.trial_suffix))
        }
    }

    /// The name of thread `n` of this scenario.
    fn thread_name(&self, n: u32) -> String {
        if self.trial_suffix.is_empty() {
            format!("t{n}")
        } else {
            format!("t{n}-{}", self.trial_suffix)
        }
    }

    fn begin_inner(&mut self) {
        if self.node_models_touched {
            // A failure here shows at the scenario's first step as an env failure.
            let gateway = self.node_gateway_touched.then_some("vercel");
            self.prepare_error = self
                .put_node_models(&self.base_models(), Some(""), gateway)
                .err();
            self.node_models_touched = false;
            self.node_gateway_touched = false;
        }
        self.ctx = Ctx::default();
        self.deadline = Instant::now() + self.cfg.scenario_budget;
    }

    pub fn set_prepare_error(&mut self, f: Fail) {
        self.prepare_error = Some(f);
    }

    pub fn prepare_failure(&self) -> Option<&Fail> {
        self.prepare_error.as_ref()
    }

    /// Runs of the current scenario, for `brief.md`.
    pub fn runs(&self) -> &[RunRec] {
        &self.ctx.runs
    }

    /// True when a step or the scenario ran out of its budget.
    pub fn budget_timed_out(&self) -> bool {
        self.ctx.budget_timeout
    }

    pub fn last_refusal(&self) -> Option<&str> {
        self.ctx.last_refusal.as_deref()
    }

    // ---- world -------------------------------------------------------------------------------

    fn apply_piece(&mut self, scenario: &Scenario, piece: &str, alias: &str) -> Result<(), Fail> {
        if self.ctx.pieces.contains_key(alias) {
            return Ok(());
        }
        let wp = self.pieces.get(piece).cloned().ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                format!("world piece `{piece}`"),
                "unknown piece",
            )
        })?;
        for req in &wp.requires {
            self.apply_piece(scenario, req, req)?;
        }
        // A skill's name is the id the model passes to `load_skill`, which a mock script spells
        // out, so it carries no run tag (vamd is throwaway; the scenario id keeps it unique).
        let name = if wp.kind == PieceKind::Skill {
            format!("{alias}-{}", scenario.id)
        } else {
            piece_name(alias, &scenario.id, &self.eff_tag())
        };
        let mut urn = String::new();
        if let Some(body) = &wp.body {
            let text = self.subst(body.to_string(), &name);
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            urn = body
                .pointer("/spec/resourceId/urn")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let coll = match wp.kind {
                PieceKind::Agent => "agents",
                PieceKind::Playbook => "playbooks",
                PieceKind::Org
                | PieceKind::Secret
                | PieceKind::McpConnection
                | PieceKind::Skill
                | PieceKind::Broker
                | PieceKind::WorkspaceSeed
                | PieceKind::MockProvider
                | PieceKind::GitMarketplace => "",
            };
            if !coll.is_empty() {
                let r = self.api.put(&format!("/v1/{coll}"), &body).map_err(conn)?;
                if !r.ok() {
                    return Err(Fail::new(
                        Class::Harness,
                        format!("PUT /v1/{coll} for piece `{piece}`"),
                        format!("{} {}", r.status, r.body),
                    ));
                }
                if wp.release && !wp.deferred {
                    let r = self
                        .api
                        .post(
                            &format!("/v1/{coll}/{}/release", seg(&urn)),
                            &json!({ "version": { "major": 1, "minor": 0, "patch": 0 } }),
                        )
                        .map_err(conn)?;
                    if !r.ok() {
                        return Err(Fail::new(
                            Class::Harness,
                            format!("release of piece `{piece}`"),
                            format!("{} {}", r.status, r.body),
                        ));
                    }
                }
            }
        }
        if wp.kind == PieceKind::Org {
            self.make_identity(alias, wp.role.as_deref().unwrap_or("Owner"), &name)?;
        }
        if wp.kind == PieceKind::Secret {
            self.put_secret_piece(alias, &name, &wp)?;
        }
        if wp.kind == PieceKind::McpConnection {
            self.put_mcp_connection_piece(alias, &wp, &name)?;
        }
        if wp.kind == PieceKind::Skill && !wp.deferred {
            self.publish_skill_piece(&wp, &name, &urn)?;
        }
        if wp.kind == PieceKind::Broker {
            self.start_broker_piece(&wp, &name)?;
        }
        if wp.kind == PieceKind::WorkspaceSeed {
            self.seed_workspace_piece(&wp)?;
        }
        if wp.kind == PieceKind::MockProvider {
            self.start_mock_provider_piece(alias, &wp)?;
        }
        if wp.kind == PieceKind::GitMarketplace {
            self.start_git_marketplace_piece(&wp)?;
        }
        self.ctx.pieces.insert(
            alias.to_string(),
            PieceRec {
                kind: wp.kind,
                piece: piece.to_string(),
                urn,
                name,
            },
        );
        self.ctx.order.push(alias.to_string());
        Ok(())
    }

    /// An `org` piece with a role other than Owner is a second identity: a user with a random
    /// password and one minted key. The bootstrap key stays the Owner.
    fn make_identity(&mut self, alias: &str, role: &str, name: &str) -> Result<(), Fail> {
        let norm: String = role
            .chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_lowercase();
        let wire = match norm.as_str() {
            "owner" | "operator" => return Ok(()),
            "member" => "member",
            "publisher" => "publisher",
            "orgadmin" => "org_admin",
            other => {
                return Err(Fail::new(
                    Class::Scenario,
                    "an org role of Owner, Member, Publisher or OrgAdmin",
                    format!("role `{other}` of org piece `{alias}`"),
                ))
            }
        };
        let username = identity_name(name, alias, &self.eff_tag().0);
        let password = secret_hex(24);
        let r = self
            .api
            .post(
                "/v1/users",
                &json!({ "username": username, "password": password, "role": wire }),
            )
            .map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                format!("POST /v1/users for org piece `{alias}`"),
                format!("{} {}", r.status, r.body),
            ));
        }
        let user_id = r.json()["user"]["userId"]
            .as_str()
            .filter(|u| !u.is_empty())
            .unwrap_or(&username)
            .to_string();
        let r = self
            .api
            .post(
                &format!("/v1/users/{}/keys", seg(&user_id)),
                &json!({ "name": "e2e" }),
            )
            .map_err(conn)?;
        // The success body carries the secret: it is never printed.
        let secret = r.json()["secret"].as_str().unwrap_or_default().to_string();
        if !r.ok() || secret.is_empty() {
            return Err(Fail::new(
                Class::Harness,
                format!("POST /v1/users/<id>/keys for org piece `{alias}`"),
                if r.ok() {
                    "200 without a secret".to_string()
                } else {
                    format!("{} {}", r.status, r.body)
                },
            ));
        }
        self.ctx
            .identities
            .insert(alias.to_string(), Secret(secret));
        self.ctx.accounts.insert(
            alias.to_string(),
            identity::Account {
                username,
                user_id,
                current: Secret(password),
                previous: None,
                session: None,
                previous_session: None,
            },
        );
        Ok(())
    }

    /// The key to act as: `None` is the bootstrap key. `alias` must name an `org` piece.
    fn key_of(&self, alias: Option<&str>) -> Result<Option<String>, Fail> {
        let Some(a) = alias else { return Ok(None) };
        match self.ctx.pieces.get(a) {
            Some(p) if p.kind == PieceKind::Org => {
                Ok(self.ctx.identities.get(a).map(|s| s.0.clone()))
            }
            _ => Err(Fail::new(
                Class::Scenario,
                format!("org alias `{a}`"),
                "not an org piece in `given`",
            )),
        }
    }

    /// The key a run acts as, for its own reads and its decisions.
    fn run_key(&self, i: usize) -> Option<String> {
        self.ctx.runs[i]
            .identity
            .as_deref()
            .and_then(|a| self.ctx.identities.get(a))
            .map(|s| s.0.clone())
    }

    fn arrange_inner(&mut self, s: &Scenario) -> Result<Arranged, Fail> {
        if self.prepare_error.is_some() {
            return Ok(Arranged::default());
        }
        for g in &s.given {
            self.apply_piece(s, g.piece(), g.alias())?;
        }
        Ok(Arranged {
            names: self.ctx.order.clone(),
        })
    }

    /// Cancels the scenario's runs and ends their sessions. Catalog entries stay: their names are
    /// run-unique and the vamd is throwaway.
    fn cleanup_inner(&mut self) {
        let runs = self.ctx.runs.clone();
        self.stop_brokers();
        self.cleanup_marketplaces();
        self.cleanup_connectors();
        // The connection's credentials and host must not reach a later scenario's session.
        for id in std::mem::take(&mut self.ctx.mcp_connections) {
            let _ = self
                .api
                .delete(&format!("/v1/mcp/connections/{}?scope=org", seg(&id)));
        }
        for (i, r) in runs.into_iter().enumerate() {
            // As the run's own identity: a member's run is not the bootstrap key's to cancel.
            let key = self.run_key(i);
            if !is_terminal(&r.state) {
                let _ = self.api.post_as(
                    key.as_deref(),
                    &format!("/v1/runs/{}/cancel", seg(&r.run_id)),
                    &json!({}),
                );
            }
            if !r.session_id.is_empty() {
                let _ = self.api.post_as(
                    key.as_deref(),
                    &format!("/v1/sessions/{}/terminate", seg(&r.session_id)),
                    &json!({}),
                );
            }
        }
    }

    // ---- polling -----------------------------------------------------------------------------

    fn refresh(&mut self, i: usize) -> Result<(), Fail> {
        let run_id = self.ctx.runs[i].run_id.clone();
        let key = self.run_key(i);
        let r = self
            .api
            .get_as(key.as_deref(), &format!("/v1/runs/{}", seg(&run_id)))
            .map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                "run status readable",
                format!("GET run -> {} {}", r.status, r.body),
            ));
        }
        let v = r.json();
        let st = &v["status"];
        let rec = &mut self.ctx.runs[i];
        rec.state = st["state"].as_str().unwrap_or_default().to_string();
        rec.steps = st["stepStatuses"].as_array().cloned().unwrap_or_default();
        rec.error = st["error"].to_string();
        if let Some(s) = st["sessionId"].as_str().filter(|s| !s.is_empty()) {
            rec.session_id = s.to_string();
        }
        if is_terminal(&rec.state) {
            rec.terminal_at.get_or_insert_with(Instant::now);
        } else {
            rec.terminal_at = None;
        }
        let cursor = rec.cursor;
        let r = self
            .api
            .get_as(
                key.as_deref(),
                &format!("/v1/runs/{}/events?after={cursor}", seg(&run_id)),
            )
            .map_err(conn)?;
        if r.ok() {
            let v = r.json();
            let rec = &mut self.ctx.runs[i];
            for e in v["events"].as_array().cloned().unwrap_or_default() {
                if let Some(seq) = e["sequence"]
                    .as_i64()
                    .or_else(|| e["sequence"].as_str().and_then(|x| x.parse().ok()))
                {
                    rec.cursor = rec.cursor.max(seq);
                }
                rec.events.push(e);
            }
        }
        Ok(())
    }

    fn wait_for(
        &mut self,
        budget: Duration,
        watch: Option<usize>,
        mut probe: impl FnMut(&mut Engine) -> Result<Probe, Fail>,
    ) -> Result<(), WaitEnd> {
        let start = Instant::now();
        let mut poll = self.cfg.poll;
        loop {
            if let Some(i) = watch {
                self.refresh(i).map_err(WaitEnd::Fail)?;
            }
            match probe(self).map_err(WaitEnd::Fail)? {
                Probe::Met => return Ok(()),
                Probe::Impossible(m) => return Err(WaitEnd::Impossible(m)),
                Probe::Pending => {}
            }
            if let Some(i) = watch {
                if let Some(t) = self.ctx.runs[i].terminal_at {
                    if t.elapsed() >= self.cfg.settle {
                        let r = &self.ctx.runs[i];
                        return Err(WaitEnd::Impossible(format!(
                            "run ended {} (error {})",
                            r.state, r.error
                        )));
                    }
                }
            }
            let scenario_over = Instant::now() >= self.deadline;
            if scenario_over || start.elapsed() >= budget {
                return Err(WaitEnd::Timeout {
                    scenario_budget: scenario_over,
                });
            }
            std::thread::sleep(poll);
            if start.elapsed() > Duration::from_secs(2) {
                poll = poll.max(Duration::from_millis(250));
            }
        }
    }

    /// Turns a wait that did not succeed into a failure with the right class.
    fn wait_fail(&mut self, e: WaitEnd, what: &str, watch: Option<usize>) -> Fail {
        match e {
            WaitEnd::Fail(f) => f,
            WaitEnd::Timeout { scenario_budget } => {
                self.ctx.budget_timeout = true;
                let mut observed = match watch {
                    Some(i) => format!(
                        "run still {} when the {} ran out",
                        self.ctx.runs[i].state,
                        if scenario_budget {
                            "scenario budget"
                        } else {
                            "step budget"
                        }
                    ),
                    None => "budget ran out".to_string(),
                };
                let class = self.cause_class(watch, Class::Harness, &mut observed);
                Fail::new(class, what, observed)
            }
            WaitEnd::Impossible(m) => {
                let mut observed = m;
                let default = Class::Product;
                let class = self.cause_class(watch, default, &mut observed);
                Fail::new(class, what, observed)
            }
        }
    }

    /// `env` if vamd is gone, `scenario` if the mock found no rule, else `default`.
    fn cause_class(
        &mut self,
        watch: Option<usize>,
        default: Class,
        observed: &mut String,
    ) -> Class {
        if !self.vamd.alive() || !self.api.healthy() {
            observed.push_str("; vamd is not answering");
            return Class::Env;
        }
        if let Some(i) = watch {
            if let Some(e) = self.ctx.runs[i]
                .events
                .iter()
                .find(|e| bare(e["eventType"].as_str().unwrap_or_default()) == "mock.unmatched")
            {
                observed.push_str(&format!("; the mock matched no rule: {}", e["payload"]));
                // A script that calls a tool the product does not offer yet is a product gap.
                if e["payload"].to_string().contains("tool_not_offered") {
                    return Class::Product;
                }
                return Class::Scenario;
            }
        }
        default
    }

    // ---- steps -------------------------------------------------------------------------------

    fn preflight(&mut self, s: &Scenario) -> Result<(), Fail> {
        if let Some(f) = &self.prepare_error {
            return Err(f.clone());
        }
        let want = match (self.mode_override, s.needs) {
            (Some(m), _) => m,
            (None, Needs::Process) => "process",
            (None, Needs::Firecracker) => "vm",
            _ => return Ok(()),
        };
        if self.node_mode.is_none() {
            let r = self.api.get("/v1/node").map_err(conn)?;
            let mode = r.json()["status"]["guestMode"].as_str().map(String::from);
            self.node_mode = Some(mode);
        }
        match self.node_mode.clone().flatten() {
            Some(m) if m == want => Ok(()),
            other => Err(Fail::new(
                Class::Env,
                format!("{want} guest available (GET /v1/node guestMode: {want})"),
                format!(
                    "{want} guest not available (guestMode: {})",
                    other.unwrap_or_else(|| "absent".into())
                ),
            )),
        }
    }

    fn agent_of(&self, alias: Option<&str>) -> Result<(String, String), Fail> {
        let agents: Vec<&String> = self
            .ctx
            .order
            .iter()
            .filter(|a| self.ctx.pieces[*a].kind == PieceKind::Agent)
            .collect();
        // A literal URN names an agent the scenario did not make (one a marketplace installed).
        if let Some(u) = alias.filter(|a| a.starts_with("urn:vam:agent:")) {
            return Ok((u.to_string(), u.to_string()));
        }
        let a = match alias {
            Some(a) => a.to_string(),
            None if agents.len() == 1 => agents[0].clone(),
            None => {
                return Err(Fail::new(
                    Class::Scenario,
                    "one agent piece, or an `agent:` alias",
                    format!("{} agent pieces given", agents.len()),
                ))
            }
        };
        match self.ctx.pieces.get(&a) {
            Some(p) if p.kind == PieceKind::Agent => Ok((a, p.urn.clone())),
            _ => Err(Fail::new(
                Class::Scenario,
                format!("agent alias `{a}`"),
                "not an agent in `given`",
            )),
        }
    }

    fn playbook_of(&self, alias: &str) -> Result<String, Fail> {
        match self.ctx.pieces.get(alias) {
            Some(p) if p.kind == PieceKind::Playbook => Ok(p.urn.clone()),
            _ => Err(Fail::new(
                Class::Scenario,
                format!("playbook alias `{alias}`"),
                "not a playbook in `given`",
            )),
        }
    }

    fn next_key(&mut self) -> String {
        // Unique across scenarios: vamd replays a run for a repeated idempotency key.
        self.key_seq += 1;
        format!("e2e-{}-{}", self.eff_tag().0, self.key_seq)
    }

    /// POSTs a run, retrying while the agent's workspace is still held by its previous run.
    fn post_run(
        &mut self,
        body: &Value,
        budget: Duration,
        key: Option<&str>,
    ) -> Result<Reply, Fail> {
        let start = Instant::now();
        loop {
            let r = self.api.post_as(key, "/v1/runs", body).map_err(conn)?;
            let busy = !r.ok() && {
                let b = r.body.to_lowercase();
                b.contains("running on workspace") || b.contains("reserved by parked run")
            };
            if !busy || start.elapsed() >= budget || Instant::now() >= self.deadline {
                return Ok(r);
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    fn run_body(
        &mut self,
        urn: &str,
        slug: &str,
        thread: &str,
        prompt: &str,
        playbook: Option<(&str, &Value)>,
        draft: Option<bool>,
    ) -> Value {
        let mut inputs = serde_json::Map::new();
        inputs.insert("prompt".into(), json!(prompt));
        inputs.insert("model".into(), json!(self.model));
        if let (Some(p), true) = (&self.model_provider, self.model != MOCK_MODEL) {
            inputs.insert("model_provider".into(), json!(p));
        }
        let mut spec = json!({
            "agentRef": { "resourceId": { "urn": urn } },
            "workspaceRefUri": format!("volume://{slug}"),
            "threadRef": thread,
            "idempotencyKey": self.next_key(),
        });
        match draft {
            Some(true) => {
                spec["agentRef"]["version"] = json!({ "major": 1, "prerelease": "draft" });
            }
            // The release a world piece makes.
            Some(false) => {
                spec["agentRef"]["version"] = json!({ "major": 1, "minor": 0, "patch": 0 });
            }
            None => {}
        }
        if let Some((pb, extra)) = playbook {
            spec["playbookRef"] = json!({ "resourceId": { "urn": pb } });
            if let Some(o) = extra.as_object() {
                for (k, v) in o {
                    inputs.insert(k.clone(), v.clone());
                }
            }
        }
        spec["inputs"] = Value::Object(inputs);
        json!({ "spec": spec })
    }

    fn start_and_wait(
        &mut self,
        alias: &str,
        identity: Option<&str>,
        body: Value,
        budget: Duration,
        refusal_ok: bool,
        wait: bool,
    ) -> Result<(), Fail> {
        let key = self.key_of(identity)?;
        let r = self.post_run(&body, budget, key.as_deref())?;
        if !r.ok() {
            if refusal_ok && (400..500).contains(&r.status) {
                self.ctx.last_refusal = Some(r.body);
                return Ok(());
            }
            return Err(Fail::new(
                Class::Product,
                "the run starts",
                format!("POST /v1/runs -> {} {}", r.status, r.body),
            ));
        }
        self.ctx.last_refusal = None;
        let v = r.json();
        let rec = RunRec {
            agent: alias.to_string(),
            identity: identity.map(String::from),
            // A message queued behind a busy workspace names the run it will start.
            run_id: steer::queued_run_id(&v).unwrap_or_else(|| {
                v["status"]["runId"]["urn"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string()
            }),
            session_id: v["sessionId"].as_str().unwrap_or_default().to_string(),
            workspace_id: v["workspaceId"].as_str().unwrap_or_default().to_string(),
            thread_ref: v["threadRef"].as_str().unwrap_or_default().to_string(),
            space_id: first_str(&[&v["spaceId"], &v["status"]["spaceId"]]),
            thread_id: first_str(&[&v["threadId"], &v["status"]["threadId"]]),
            agent_urn: body["spec"]["agentRef"]["resourceId"]["urn"]
                .as_str()
                .unwrap_or_default()
                .to_string(),
            ..Default::default()
        };
        if rec.run_id.is_empty() {
            return Err(Fail::new(
                Class::Harness,
                "run id in the create response",
                r.body,
            ));
        }
        self.ctx.runs.push(rec);
        let i = self.ctx.runs.len() - 1;
        if !wait {
            return Ok(());
        }
        let what = "the run is terminal or parked";
        self.wait_for(budget, Some(i), |e| {
            let st = e.ctx.runs[i].state.as_str();
            Ok(if is_terminal(st) || st == "RUN_STATE_PAUSED" {
                Probe::Met
            } else {
                Probe::Pending
            })
        })
        .map_err(|e| self.wait_fail(e, what, Some(i)))
    }

    /// The workspace name of an agent's runs: a created identity gets its own, and so do draft
    /// runs (vamd binds a workspace to one agent version and refuses a release run on a
    /// workspace a draft run used).
    fn workspace_slug(&self, urn: &str, identity: Option<&str>, draft: Option<bool>) -> String {
        let mut slug = urn.rsplit('/').next().unwrap_or(urn).to_string();
        if let Some(a) = identity.filter(|a| self.ctx.identities.contains_key(*a)) {
            slug = format!("{slug}-{a}");
        }
        if draft == Some(true) {
            slug.push_str("-draft");
        }
        slug
    }

    fn chat(&mut self, c: &Chat, s: &Scenario) -> Result<(), Fail> {
        let (alias, urn) = self.agent_of(c.agent())?;
        let identity = c.identity().map(String::from);
        self.key_of(identity.as_deref())?;
        let (mode, budget) = match c {
            Chat::Text(_) => (ThreadSel::Mode(ThreadMode::New), self.cfg.step_budget),
            Chat::Full(f) => (
                f.thread.clone(),
                f.budget
                    .as_deref()
                    .and_then(parse_duration)
                    .unwrap_or(self.cfg.step_budget),
            ),
        };
        let slug = self.workspace_slug(&urn, identity.as_deref(), c.draft());
        // A follow-up names its Space and thread by the ids the node reported; a new thread
        // sends only a label (names are labels, never lookups).
        let mut place: Option<(String, String)> = None;
        let thread = match mode {
            ThreadSel::ById(_) => {
                let id = self.latest_thread_id()?;
                let space = self.ctx.runs[self.last_run()?].space_id.clone();
                place = Some((space, id));
                String::new()
            }
            ThreadSel::Mode(ThreadMode::New) => {
                self.ctx.threads += 1;
                self.thread_name(self.ctx.threads)
            }
            ThreadSel::Label(ref l) => l.name.clone(),
            ThreadSel::Mode(ThreadMode::Continue) => match self
                .ctx
                .runs
                .iter()
                .rev()
                .find(|r| r.agent == alias && r.identity == identity)
            {
                Some(r) => {
                    place = Some((r.space_id.clone(), r.thread_id.clone()));
                    String::new()
                }
                None => {
                    return Err(Fail::new(
                        Class::Scenario,
                        "an earlier chat with this agent to continue",
                        "none",
                    ))
                }
            },
        };
        let prompt = self.chat_prompt(c.text(), s);
        let mut body = self.run_body(&urn, &slug, &thread, &prompt, None, c.draft());
        if let Some((space, thread_id)) = place {
            body["spec"]["spaceId"] = json!(space);
            body["spec"]["threadId"] = json!(thread_id);
        } else if let Some(space) = self.created_space() {
            // A `create-space` step made a Space: a new thread goes into it.
            body["spec"]["spaceId"] = json!(space);
        }
        put_harness(&mut body, c.harness(), self.profile.as_deref());
        put_model(&mut body, c.model());
        self.start_and_wait(
            &alias,
            identity.as_deref(),
            body,
            budget,
            c.refused(),
            c.wait(),
        )
    }

    /// The user text of a chat step. The mock reads the `e2e-step:<label>` marker and the
    /// `mock-script:<id>` suffix; a real model gets the text without either.
    fn chat_prompt(&self, text: &str, s: &Scenario) -> String {
        let text = &self.expand_schedule_id(text);
        if self.model == MOCK_MODEL {
            format!("{} mock-script:{}", text, s.id)
        } else {
            strip_step_marker(text)
        }
    }

    fn start_playbook(
        &mut self,
        sp: &crate::scenario::StartPlaybook,
        s: &Scenario,
    ) -> Result<(), Fail> {
        let (alias, urn) = self.agent_of(sp.agent.as_deref())?;
        let pb = self.playbook_of(&sp.playbook)?;
        self.key_of(sp.as_.as_deref())?;
        let slug = self.workspace_slug(&urn, sp.as_.as_deref(), sp.draft);
        self.ctx.threads += 1;
        let thread = self.thread_name(self.ctx.threads);
        let prompt = match (&sp.label, self.model == MOCK_MODEL) {
            (Some(l), true) => format!("e2e-step:{l} Start the playbook mock-script:{}", s.id),
            (None, true) => format!("Start the playbook mock-script:{}", s.id),
            // A real model sees neither the step marker nor the mock script id.
            (_, false) => "Start the playbook".to_string(),
        };
        let inputs: Value = serde_json::to_value(&sp.inputs).unwrap_or(Value::Null);
        let mut body = self.run_body(
            &urn,
            &slug,
            &thread,
            &prompt,
            Some((&pb, &inputs)),
            sp.draft,
        );
        put_harness(&mut body, sp.harness.as_ref(), self.profile.as_deref());
        self.start_and_wait(
            &alias,
            sp.as_.as_deref(),
            body,
            self.cfg.step_budget,
            true,
            true,
        )
    }

    fn last_run(&self) -> Result<usize, Fail> {
        if self.ctx.runs.is_empty() {
            Err(Fail::new(
                Class::Scenario,
                "an earlier run in this scenario",
                "no run started yet",
            ))
        } else {
            Ok(self.ctx.runs.len() - 1)
        }
    }

    /// Pending approvals of the run (tool calls, or gates when `gate`), oldest first, listed as
    /// the identity `key` (approvals are listed per owner).
    fn pending_approvals(
        &self,
        run_id: &str,
        gate: bool,
        key: Option<&str>,
    ) -> Result<Vec<Value>, Fail> {
        Ok(self
            .listed_approvals(run_id, key)?
            .into_iter()
            .filter(|c| (c["kind"].as_str() == Some("APPROVAL_KIND_GATE")) == gate)
            .collect())
    }

    fn listed_approvals(&self, run_id: &str, key: Option<&str>) -> Result<Vec<Value>, Fail> {
        let mut found = Vec::new();
        for v in self.approval_pages(key)? {
            for a in v["approvals"].as_array().cloned().unwrap_or_default() {
                let claims = if a.get("claims").is_some() {
                    &a["claims"]
                } else {
                    &a
                };
                if claims["runId"]["urn"].as_str() == Some(run_id) {
                    found.push(claims.clone());
                }
            }
        }
        Ok(found)
    }

    /// What a parked run waits on, when it has a pending approval or gate.
    fn parked_on(&self, run_id: &str, key: Option<&str>) -> Result<Option<String>, Fail> {
        Ok(self.listed_approvals(run_id, key)?.last().map(|c| {
            if c["kind"].as_str() == Some("APPROVAL_KIND_GATE") {
                format!("gate step `{}`", c["stepId"].as_str().unwrap_or_default())
            } else {
                c["targetToolName"]
                    .as_str()
                    .unwrap_or("a tool call")
                    .to_string()
            }
        }))
    }

    fn decide(
        &mut self,
        gate: bool,
        decision: &str,
        response: Option<Value>,
        step: Option<&str>,
        opts: &DecideOpts,
    ) -> Result<(), Fail> {
        let i = self.last_run()?;
        let run_id = self.ctx.runs[i].run_id.clone();
        let budget = self.cfg.step_budget;
        let key = match opts.as_.as_deref() {
            Some(a) => self.key_of(Some(a))?,
            None => self.run_key(i),
        };
        let mut found: Option<Value> = None;
        self.wait_for(budget, Some(i), |e| {
            let a = e
                .pending_approvals(&run_id, gate, key.as_deref())?
                .into_iter()
                .rfind(|c| step.is_none_or(|st| c["stepId"].as_str() == Some(st)));
            Ok(match a {
                Some(c) => {
                    found = Some(c);
                    Probe::Met
                }
                None => Probe::Pending,
            })
        })
        .map_err(|e| {
            self.wait_fail(
                e,
                if gate {
                    "a pending gate for the run"
                } else {
                    "a pending approval for the run"
                },
                Some(i),
            )
        })?;
        let claims = found.expect("probe met");
        let decision = if opts.amend.is_some() {
            "HITL_DECISION_AMENDED"
        } else {
            decision
        };
        let req_id = claims["requestId"]["urn"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let mut body = decide_body(&req_id, decision, response, opts.always);
        if let Some(a) = &opts.amend {
            body["amendedArgs"] = serde_json::to_value(a).unwrap_or(Value::Null);
        }
        let r = match opts.via {
            Some(via) => self.decide_via(via, key.as_deref(), &req_id, decision, &body)?,
            None => self
                .api
                .post_as(
                    key.as_deref(),
                    &format!("/v1/approvals/{}/decide", seg(&req_id)),
                    &body,
                )
                .map_err(conn)?,
        };
        if opts.refused {
            if !(400..500).contains(&r.status) {
                return Err(Fail::new(
                    Class::Product,
                    "the decision is refused (a 4xx answer)",
                    format!("{} {}", r.status, r.body),
                ));
            }
            let still = self
                .pending_approvals(&run_id, gate, key.as_deref())?
                .iter()
                .any(|c| c["requestId"]["urn"].as_str() == Some(req_id.as_str()));
            return if still {
                Ok(())
            } else {
                Err(Fail::new(
                    Class::Product,
                    "the refused approval stays pending",
                    format!("refused with {} but no longer pending", r.status),
                ))
            };
        }
        if !r.ok() {
            return Err(Fail::new(
                Class::Product,
                "the decision is accepted",
                format!("{} {}", r.status, r.body),
            ));
        }
        // Leave PAUSED: the decision resumes the run (or ends it).
        self.wait_for(budget, Some(i), |e| {
            Ok(if e.ctx.runs[i].state != "RUN_STATE_PAUSED" {
                Probe::Met
            } else {
                Probe::Pending
            })
        })
        .map_err(|e| self.wait_fail(e, "the run resumes after the decision", Some(i)))
    }

    fn decide_gate(&mut self, g: &DecideGate) -> Result<(), Fail> {
        let opts = DecideOpts {
            always: false,
            refused: g.refused,
            as_: g.as_.clone(),
            via: g.via,
            amend: None,
        };
        self.decide(
            true,
            "HITL_DECISION_SUBMITTED",
            Some(json!({ "option": g.option })),
            g.gate.as_deref(),
            &opts,
        )
    }

    fn stop_session(&mut self, agent: Option<&str>) -> Result<(), Fail> {
        let idx = match agent {
            Some(a) => self.ctx.runs.iter().rposition(|r| r.agent == a),
            None => self.ctx.runs.len().checked_sub(1),
        }
        .ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                "a run whose session to stop",
                "no run started yet",
            )
        })?;
        self.refresh(idx)?;
        let sid = self.ctx.runs[idx].session_id.clone();
        let r = self
            .api
            .post(&format!("/v1/sessions/{}/terminate", seg(&sid)), &json!({}))
            .map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Product,
                "the session terminates",
                format!("{} {}", r.status, r.body),
            ));
        }
        let budget = self.cfg.step_budget;
        self.wait_for(budget, Some(idx), |e| {
            Ok(if is_terminal(&e.ctx.runs[idx].state) {
                Probe::Met
            } else {
                Probe::Pending
            })
        })
        .map_err(|e| self.wait_fail(e, "the run ends after its session is stopped", Some(idx)))
    }

    fn restart_vamd(&mut self) -> Result<(), Fail> {
        self.vamd
            .restart()
            .map_err(|e| Fail::new(Class::Env, "vamd restarts", e))?;
        self.wait_healthy()?;
        // Let the runs that were open catch up.
        for i in 0..self.ctx.runs.len() {
            self.refresh(i)?;
        }
        Ok(())
    }

    // ---- expectations ------------------------------------------------------------------------

    /// `expect: { node: { fields } }`: `GET /v1/node`, fields as dotted paths from its body.
    fn expect_node(&mut self, n: &crate::scenario::NodeExpect) -> Result<(), Fail> {
        let r = self.api.get("/v1/node").map_err(conn)?;
        let body = r.json();
        let mut bad = field_problems(&body, &n.fields);
        if !r.ok() {
            bad.insert(0, format!("GET /v1/node answered {}", r.status));
        }
        if bad.is_empty() {
            Ok(())
        } else {
            Err(Fail::new(
                Class::Product,
                format!(
                    "node reports {}",
                    serde_json::to_string(&n.fields).unwrap_or_default()
                ),
                bad.join("; "),
            ))
        }
    }

    /// The latest run's receipt: `GET /v1/runs/{id}/receipt`. A non-2xx answer is `product`.
    fn fetch_receipt(&mut self, what: &str) -> Result<(String, Value), Fail> {
        let i = self.last_run()?;
        let run_id = self.ctx.runs[i].run_id.clone();
        let path = format!("/v1/runs/{}/receipt", seg(&run_id));
        // A 409 means the receipt is not issued yet (the node issues it as the last
        // step of finishing a run): ask again within the step budget.
        let (budget, deadline) = (self.cfg.step_budget, self.deadline);
        // Runs are the caller's own: read as the identity that started this one.
        let key = self.run_key(i);
        let api = &self.api;
        let r = poll_while_pending(
            || api.get_as(key.as_deref(), &path),
            budget,
            RECEIPT_POLL,
            deadline,
        )
        .map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Product,
                what.to_string(),
                format!("GET {path} answered {} {}", r.status, r.body),
            ));
        }
        Ok((run_id, r.json()))
    }

    /// `expect: { receipt: { fields } }`: fields are dotted camelCase paths over
    /// `{receipt, modelAppendix, signature}`.
    fn expect_receipt(&mut self, x: &crate::scenario::ReceiptExpect) -> Result<(), Fail> {
        let what = format!(
            "receipt reports {}",
            serde_json::to_string(&x.fields).unwrap_or_default()
        );
        let (_, body) = self.fetch_receipt(&what)?;
        let bad = field_problems(&body, &x.fields);
        if bad.is_empty() {
            Ok(())
        } else {
            Err(Fail::new(Class::Product, what, bad.join("; ")))
        }
    }

    /// `verify-receipt: { tamper, pack }`: write the receipt (or the evidence pack) under a
    /// scratch directory and run the built `vam` on it with no host and no key.
    fn verify_receipt(&mut self, v: &crate::scenario::VerifyReceipt) -> Result<(), Fail> {
        if v.file.is_some() {
            return self.verify_receipt_file(v);
        }
        let what = match (v.pack, v.tamper) {
            (true, _) => "the evidence pack verifies offline",
            (false, false) => "the receipt verifies offline",
            (false, true) => "a receipt with one changed byte fails verification",
        };
        let envf = |e: String| Fail::new(Class::Env, what, e);
        let (run_id, body) = self.fetch_receipt(what)?;
        let dir = std::env::temp_dir().join(format!(
            "vam-e2e-receipt-{}-{}",
            self.cfg.tag.0,
            run_id.rsplit('/').next().unwrap_or("run")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| envf(e.to_string()))?;
        let cli = std::env::var("VAM_E2E_CLI").ok();
        let (ok, out) = if v.pack {
            let r = self
                .api
                .get(&format!("/v1/runs/{}/evidence", seg(&run_id)))
                .map_err(conn)?;
            if !r.ok() {
                return Err(Fail::new(
                    Class::Product,
                    what,
                    format!("GET evidence answered {} {}", r.status, r.body),
                ));
            }
            let files = r.json()["files"].as_object().cloned().unwrap_or_default();
            if !files.contains_key("receipt.json") {
                return Err(Fail::new(
                    Class::Product,
                    what,
                    "the evidence pack holds no receipt.json",
                ));
            }
            for (name, content) in &files {
                if name.contains('/') || name.contains('\\') || name.contains("..") {
                    return Err(Fail::new(
                        Class::Product,
                        what,
                        format!("unsafe file name {name}"),
                    ));
                }
                std::fs::write(dir.join(name), content.as_str().unwrap_or_default())
                    .map_err(|e| envf(e.to_string()))?;
            }
            run_cli(
                cli.as_deref(),
                &["evidence", "verify", &dir.to_string_lossy()],
            )?
        } else {
            let doc = if v.tamper {
                tamper_receipt(&body).ok_or_else(|| {
                    Fail::new(Class::Product, what, "the receipt holds no text to change")
                })?
            } else {
                body
            };
            let file = dir.join("receipt.json");
            std::fs::write(&file, serde_json::to_vec(&doc).unwrap_or_default())
                .map_err(|e| envf(e.to_string()))?;
            run_cli(
                cli.as_deref(),
                &["receipt", "verify", &file.to_string_lossy()],
            )?
        };
        verify_verdict(v.tamper && !v.pack, ok, &out)
            .map_err(|m| Fail::new(Class::Product, what, m))
    }

    /// `expect: { plain-http: refused }`: no 2xx and no API body on plain HTTP, no key sent.
    fn expect_plain_http(&mut self) -> Result<(), Fail> {
        match self.api.probe_plain("/v1/node") {
            Err(_) => Ok(()),
            Ok(r) if !r.ok() && !r.json().is_object() => Ok(()),
            Ok(r) => Err(Fail::new(
                Class::Product,
                "no API answer on plain HTTP (no 2xx, no API body)",
                format!(
                    "plain HTTP answered {}: {}",
                    r.status,
                    r.body.chars().take(200).collect::<String>()
                ),
            )),
        }
    }

    /// `expect: { vmm: jailed }`: the Firecracker process, read from the host's `/proc`.
    fn expect_vmm(&mut self, v: &crate::scenario::VmmExpect) -> Result<(), Fail> {
        use crate::vmm_probe::{judge_seccomp, own_ns, Verdict};
        let under = v.under();
        let own = own_ns();
        let want = format!(
            "Firecracker process jailed under {under}, non-root, own pid and mnt namespaces{}",
            if v.wants_seccomp() {
                ", under a seccomp filter"
            } else {
                ""
            }
        );
        let end = Instant::now() + self.cfg.step_budget;
        loop {
            match judge_seccomp(
                std::path::Path::new("/proc"),
                &under,
                &own,
                v.wants_seccomp(),
            ) {
                Verdict::Jailed(_) if v.is_jailed() => return Ok(()),
                Verdict::Jailed(_) => {
                    return Err(Fail::new(
                        Class::Product,
                        "an unjailed VMM",
                        "the VMM is jailed",
                    ))
                }
                Verdict::Unjailed(p) if !v.is_jailed() => {
                    let _ = p;
                    return Ok(());
                }
                Verdict::Unjailed(p) => return Err(Fail::new(Class::Product, want, p.join("; "))),
                Verdict::Unreadable(m) => return Err(Fail::new(Class::Env, want, m)),
                Verdict::NoProcess if Instant::now() >= end => {
                    return Err(Fail::new(
                        Class::Product,
                        want,
                        "no firecracker process on the host",
                    ))
                }
                Verdict::NoProcess => std::thread::sleep(self.cfg.poll),
            }
        }
    }

    fn expect(&mut self, e: &crate::scenario::Expect) -> Result<(), Fail> {
        if let Some(n) = &e.node {
            return self.expect_node(n);
        }
        if let Some(r) = &e.receipt {
            return self.expect_receipt(r);
        }
        if e.plain_http.is_some() {
            return self.expect_plain_http();
        }
        if let Some(v) = &e.vmm {
            return self.expect_vmm(v);
        }
        if let Some(f) = &e.flood {
            return self.expect_flood(f);
        }
        if let Some(x) = &e.secret_absent {
            return self.expect_secret_absent(x);
        }
        if let Some(x) = &e.grant {
            return self.expect_grant(x);
        }
        if let Some(x) = &e.fire {
            return self.expect_fire(x);
        }
        if let Some(x) = &e.spaces {
            return self.expect_spaces(x);
        }
        if let Some(x) = &e.threads {
            return self.expect_threads(x);
        }
        if let Some(x) = &e.runs {
            return self.expect_runs(x);
        }
        if let Some(x) = &e.scheduled_runs {
            return self.expect_scheduled_runs(x);
        }
        if let Some(x) = &e.ledger {
            return self.expect_ledger(x);
        }
        if let Some(x) = &e.notifications {
            return self.expect_notifications(x);
        }
        if let Some(x) = &e.channel_messages {
            return self.expect_channel_messages(x);
        }
        if let Some(x) = &e.broker_state {
            return self.expect_broker_state(x);
        }
        if let Some(x) = &e.connectors {
            return self.expect_connectors(x);
        }
        if let Some(x) = &e.mock_requests {
            return self.expect_mock_requests(x);
        }
        if let Some(x) = &e.agent_stats {
            return self.expect_agent_stats(x);
        }
        if let Some(x) = &e.release_diff {
            return self.expect_release_diff(x);
        }
        if let Some(x) = &e.marketplaces {
            return self.expect_marketplaces(x);
        }
        if let Some(x) = &e.listings {
            return self.expect_listings(x);
        }
        if let Some(x) = &e.catalog {
            return self.expect_catalog(x);
        }
        if let Some(x) = &e.notification_summary {
            return self.expect_notification_summary(x);
        }
        let i = self.last_run();
        if let Some(true) = e.refused {
            return match &self.ctx.last_refusal {
                Some(body) => match &e.contains {
                    Some(want) if !body.contains(want.as_str()) => Err(Fail::new(
                        Class::Product,
                        format!("the refusal says `{want}`"),
                        body.clone(),
                    )),
                    _ => Ok(()),
                },
                None => Err(Fail::new(
                    Class::Product,
                    "the start is refused",
                    "the start was accepted",
                )),
            };
        }
        let i = i?;
        let budget = self.cfg.step_budget;
        if let Some(want) = &e.run {
            let want_state = format!("RUN_STATE_{}", want.to_uppercase());
            let w = want_state.clone();
            let settle = self.cfg.settle;
            let mut paused_at: Option<Instant> = None;
            let r = self.wait_for(budget, Some(i), |en| {
                let st = en.ctx.runs[i].state.clone();
                if st == w {
                    return Ok(Probe::Met);
                }
                if is_terminal(&st) {
                    return Ok(Probe::Impossible(format!(
                        "run ended {st} (error {})",
                        en.ctx.runs[i].error
                    )));
                }
                // A run parked on a pending approval or gate will not reach another state by
                // waiting: fail now instead of running out the step budget.
                if st == "RUN_STATE_PAUSED" {
                    let since = *paused_at.get_or_insert_with(Instant::now);
                    if since.elapsed() >= settle {
                        let run_id = en.ctx.runs[i].run_id.clone();
                        let key = en.run_key(i);
                        if let Some(on) = en.parked_on(&run_id, key.as_deref())? {
                            return Ok(Probe::Impossible(format!(
                                "run parked on approval for {on}"
                            )));
                        }
                    }
                } else {
                    paused_at = None;
                }
                Ok(Probe::Pending)
            });
            return r.map_err(|x| self.wait_fail(x, &format!("run {want_state}"), Some(i)));
        }
        if let (Some(name), Some(true)) = (&e.event, e.absent) {
            let (name, tool) = (name.clone(), e.tool.clone());
            let fields = e.fields.clone().unwrap_or_default();
            let settle = self.cfg.settle;
            let what = format!(
                "event {name}{} absent",
                tool.as_ref()
                    .map(|t| format!(" tool={t}"))
                    .unwrap_or_default()
            );
            let mut parked_since: Option<Instant> = None;
            let r = self.wait_for(budget, Some(i), |en| {
                let run = &en.ctx.runs[i];
                let settled = match run.terminal_at {
                    Some(t) => t.elapsed() >= settle,
                    None if run.state == "RUN_STATE_PAUSED" => {
                        parked_since.get_or_insert_with(Instant::now).elapsed() >= settle
                    }
                    None => {
                        parked_since = None;
                        false
                    }
                };
                Ok(absent_verdict(
                    &run.events,
                    &name,
                    tool.as_deref(),
                    &fields,
                    settled,
                ))
            });
            return r.map_err(|x| self.wait_fail(x, &what, Some(i)));
        }
        let seen = Rc::new(RefCell::new(String::new()));
        let (what, check): (String, Check) = if let Some(name) = &e.event {
            let name = name.clone();
            let tool = e.tool.clone();
            let fields = e.fields.clone().unwrap_or_default();
            let settle = self.cfg.settle;
            let parked_since: RefCell<Option<Instant>> = RefCell::new(None);
            (
                format!(
                    "event {name}{}{}",
                    tool.as_ref()
                        .map(|t| format!(" tool={t}"))
                        .unwrap_or_default(),
                    if fields.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " fields={}",
                            serde_json::to_string(&fields).unwrap_or_default()
                        )
                    }
                ),
                Box::new(move |en| {
                    let run = &en.ctx.runs[i];
                    if run
                        .events
                        .iter()
                        .any(|ev| event_matches(ev, &name, tool.as_deref(), &fields))
                    {
                        return Ok(Probe::Met);
                    }
                    // A run parked on an approval or gate emits nothing more by waiting: once
                    // it has stayed parked for the settle time, the event is absent (product).
                    if run.state == "RUN_STATE_PAUSED" {
                        let since = *parked_since.borrow_mut().get_or_insert_with(Instant::now);
                        if since.elapsed() >= settle {
                            return Ok(Probe::Impossible(
                                "the run is parked and the event has not appeared".to_string(),
                            ));
                        }
                    } else {
                        *parked_since.borrow_mut() = None;
                    }
                    Ok(Probe::Pending)
                }),
            )
        } else if e.approval.is_some() || e.gate.is_some() {
            let gate = e.gate.is_some();
            let state = e
                .gate
                .clone()
                .or_else(|| e.approval.clone())
                .unwrap_or_default();
            let family = if gate { "gate" } else { "approval" };
            let tool = e.tool.clone();
            let step = if gate { e.step.clone() } else { None };
            let fields = e.fields.clone().unwrap_or_default();
            let seen = seen.clone();
            let settle = self.cfg.settle;
            let mismatch_since: RefCell<Option<Instant>> = RefCell::new(None);
            (
                format!(
                    "{family} {state}{}{}{}",
                    tool.as_ref()
                        .map(|t| format!(" tool={t}"))
                        .unwrap_or_default(),
                    step.as_ref()
                        .map(|t| format!(" step={t}"))
                        .unwrap_or_default(),
                    if fields.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " fields={}",
                            serde_json::to_string(&fields).unwrap_or_default()
                        )
                    }
                ),
                Box::new(move |en| {
                    if state != "pending" {
                        return Err(Fail::new(
                            Class::Scenario,
                            format!("{family}: pending"),
                            format!("unsupported {family} state `{state}`"),
                        ));
                    }
                    let run_id = en.ctx.runs[i].run_id.clone();
                    let key = en.run_key(i);
                    let all = en.pending_approvals(&run_id, gate, key.as_deref())?;
                    *seen.borrow_mut() = all
                        .last()
                        .map(|c| c.to_string())
                        .unwrap_or_else(|| "(nothing pending)".into());
                    if all
                        .iter()
                        .any(|c| claims_match(c, tool.as_deref(), step.as_deref(), &fields))
                    {
                        return Ok(Probe::Met);
                    }
                    // Claims are fixed when the approval is raised: one that is there and does
                    // not match will not start to. Fail then, not at the end of the budget.
                    if all.is_empty() {
                        *mismatch_since.borrow_mut() = None;
                        return Ok(Probe::Pending);
                    }
                    let since = *mismatch_since.borrow_mut().get_or_insert_with(Instant::now);
                    Ok(if since.elapsed() >= settle {
                        Probe::Impossible(format!(
                            "a {family} is pending but does not match: {}",
                            seen.borrow()
                        ))
                    } else {
                        Probe::Pending
                    })
                }),
            )
        } else if let Some(step) = &e.step {
            let step = step.clone();
            let state = e.state.clone().unwrap_or_default();
            (
                format!("step {step} {state}"),
                Box::new(move |en| {
                    Ok(probe(en.ctx.runs[i].steps.iter().any(|s| {
                        s["stepId"].as_str() == Some(&step) && step_matches(&state, s)
                    })))
                }),
            )
        } else if let Some(f) = &e.file {
            return self.expect_file(f, i);
        } else if let Some(t) = &e.thread {
            return self.expect_thread(t, i);
        } else {
            return Ok(());
        };
        let r = self.wait_for(budget, Some(i), |en| check(en));
        r.map_err(|x| {
            let mut f = self.wait_fail(x, &what, Some(i));
            let last = seen.borrow();
            if !last.is_empty() {
                f.observed = format!("{}; last pending: {last}", f.observed);
            }
            f
        })
    }

    /// A message of the latest run's thread, read through the thread API of the
    /// run's workspace. While the run is live, or settling after it ended, a miss
    /// is retried; after the settle grace it is a product failure (the endpoint
    /// is missing, or the thread does not hold the message).
    fn expect_thread(&mut self, t: &crate::scenario::ThreadExpect, idx: usize) -> Result<(), Fail> {
        let ws = self.ctx.runs[idx].workspace_id.clone();
        let thread = self.ctx.runs[idx].thread_id.clone();
        let (role, contains) = (t.role.clone(), t.contains.clone());
        let max_bytes = t.max_bytes;
        let mut what = format!("thread {thread} has a {role} message containing {contains:?}");
        if let Some(m) = max_bytes {
            what.push_str(&format!(" of at most {m} bytes"));
        }
        let budget = self.cfg.step_budget;
        let mut seen = String::from("(not read)");
        let r = self.wait_for(budget, Some(idx), |en| {
            // Threads are the caller's own: read as the identity that started the run.
            let key = en.run_key(idx);
            let r = en
                .api
                .get_as(
                    key.as_deref(),
                    &format!(
                        "/v1/workspaces/{}/threads/{}/messages?view=all&limit=500",
                        seg(&ws),
                        seg(&thread)
                    ),
                )
                .map_err(conn)?;
            if !r.ok() {
                seen = format!("GET thread messages -> {} {}", r.status, r.body);
                return Ok(Probe::Pending);
            }
            let body = r.json();
            seen = format!(
                "{} records; integrity {}",
                body["records"].as_array().map(Vec::len).unwrap_or(0),
                body["integrity"]
            );
            if let Some(max) = max_bytes {
                let sizes = thread_message_sizes(&body, &role, &contains);
                let all = thread_message_sizes(&body, &role, "");
                seen.push_str(&format!(
                    "; matching message sizes {sizes:?}; all {role} message sizes {all:?}"
                ));
                return Ok(probe(sizes.iter().any(|n| *n <= max)));
            }
            if t.absent == Some(true) {
                return Ok(if thread_has_message(&body, &role, &contains) {
                    Probe::Impossible(format!("a {role} message contains {contains:?}"))
                } else {
                    Probe::Met
                });
            }
            Ok(if thread_has_message(&body, &role, &contains) {
                Probe::Met
            } else {
                Probe::Pending
            })
        });
        r.map_err(|x| {
            let mut f = self.wait_fail(x, &what, Some(idx));
            f.observed = format!("{}; last read: {seen}", f.observed);
            f
        })
    }

    fn expect_file(
        &mut self,
        f: &crate::scenario::FileExpect,
        default_run: usize,
    ) -> Result<(), Fail> {
        let idx = match f.agent.as_deref() {
            Some(a) => self.ctx.runs.iter().rposition(|r| r.agent == a),
            None => Some(default_run),
        }
        .ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                "a run of that agent",
                "no run for the agent",
            )
        })?;
        let ws = self.ctx.runs[idx].workspace_id.clone();
        // Scenario paths are relative to the agent's workspace directory; the files
        // API addresses the manifest, where that directory is `workspace/`.
        let path = if f.path.starts_with("workspace/") {
            f.path.clone()
        } else {
            format!("workspace/{}", f.path)
        };
        let want_absent = f.absent == Some(true);
        let equals = f.equals.clone();
        let what = if want_absent {
            format!("file {path} absent")
        } else if let Some(eq) = &equals {
            format!("file {path} equals {eq:?}")
        } else {
            format!("file {path} present")
        };
        let budget = self.cfg.step_budget;
        let mut seen = String::from("(not read)");
        let r = self.wait_for(budget, Some(idx), |en| {
            // An absence can only be judged once the run's workspace is committed.
            // Workspaces are the caller's own: read as the identity that ran.
            let key = en.run_key(idx);
            let r = en
                .api
                .get_as(
                    key.as_deref(),
                    &format!("/v1/workspaces/{}/files/{}", seg(&ws), path),
                )
                .map_err(conn)?;
            let committed = is_terminal(&en.ctx.runs[idx].state);
            let present = r.ok();
            seen = if present {
                format!("present: {:?}", r.body)
            } else {
                format!("{} {}", r.status, r.body)
            };
            Ok(if want_absent {
                if !present && committed {
                    Probe::Met
                } else if present {
                    Probe::Impossible(format!("file exists: {:?}", r.body))
                } else {
                    Probe::Pending
                }
            } else if let Some(eq) = &equals {
                if present && r.body == *eq {
                    Probe::Met
                } else {
                    Probe::Pending
                }
            } else if present {
                Probe::Met
            } else {
                Probe::Pending
            })
        });
        r.map_err(|x| {
            let mut f = self.wait_fail(x, &what, Some(idx));
            f.observed = format!("{}; last read: {seen}", f.observed);
            f
        })
    }

    fn step(&mut self, s: &Scenario, st: &Step) -> Result<(), Fail> {
        let r = self.step_dispatch(s, st);
        if r.is_ok() {
            self.notify_observe(st);
            self.channel_observe(st);
        }
        r
    }

    fn step_dispatch(&mut self, s: &Scenario, st: &Step) -> Result<(), Fail> {
        match st {
            Step::Chat(c) => self.chat(c, s),
            Step::Approve(d) => self.decide(false, "HITL_DECISION_APPROVED", None, None, &d.opts()),
            Step::Deny(d) => self.decide(false, "HITL_DECISION_REJECTED", None, None, &d.opts()),
            Step::DecideGate(g) => self.decide_gate(g),
            Step::StartPlaybook(sp) => self.start_playbook(sp, s),
            Step::StopSession(r) => self.stop_session(r.agent.as_deref()),
            Step::RestartVamd(_) => self.restart_vamd(),
            Step::Expect(e) => self.expect(e),
            Step::VerifyReceipt(v) => self.verify_receipt(v),
            Step::Send(m) => self.send(m, &s.id),
            Step::Stop(x) => self.stop(x),
            Step::Answer(a) => self.answer(a),
            Step::DecideTogether(d) => self.decide_together(d),
            Step::KillGuest(k) => self.kill_guest(k),
            Step::Flood(f) => self.flood(f),
            Step::SetNode(n) => self.set_node(n),
            Step::FireTrigger(f) => self.fire_trigger(f),
            Step::NodeNotify(x) => self.node_notify(x),
            Step::LinkContact(x) => self.link_contact(x),
            Step::UnlinkContact(x) => self.unlink_contact(x),
            Step::NotifyPrefs(x) => self.notify_prefs(x),
            Step::ChannelClick(x) => self.channel_click(x),
            Step::RevokeGrant(r) => self.revoke_grant(r),
            Step::BearerRequest(b) => self.bearer_request(b, &s.id),
            Step::CreateSpace(c) => self.create_space(c),
            Step::RenameSpace(x) => self.rename_space(x),
            Step::RenameThread(x) => self.rename_thread(x),
            Step::WaitScheduledRun(w) => self.wait_scheduled_run(w),
            Step::StopBroker(x) => self.stop_broker(x),
            Step::StartBroker(x) => self.start_broker(x),
            Step::NotificationMark(m) => self.notification_mark(m),
            Step::ConnectorNode(x) => self.connector_node(x),
            Step::PublishSkill(p) => self.publish_skill_step(p),
            Step::Cli(x) => self.cli_step(x),
            Step::ConnectorConnect(x) => self.connector_connect(x),
            Step::ConnectorCheck(x) => self.connector_check(x),
            Step::MockProvider(x) => self.mock_provider(x),
            Step::EditAgentDraft(x) => self.edit_agent_draft(x),
            Step::ReleaseAgent(x) => self.release_agent(x),
            Step::GitTag(x) => self.git_tag(x),
            Step::MarketplaceAdd(x) => self.marketplace_add(x),
            Step::MarketplaceRefresh(x) => self.marketplace_act("refresh", x),
            Step::MarketplaceInstall(x) => self.marketplace_act("install", x),
            Step::MarketplaceUpdate(x) => self.marketplace_act("update", x),
            Step::CatalogWrite(x) => self.catalog_write(x),
            Step::Login(x) => self.login(x),
            Step::ChangePassword(x) => self.change_password(x),
            Step::IdentityRequest(x) => self.identity_request(x),
        }
    }
}

/// Puts a step's `harness:` switches into the run's `inputs.harness`; leaves the body as is
/// when the step sets none.
/// `text` without its `e2e-step:<label>` token(s).
pub fn strip_step_marker(text: &str) -> String {
    let marker = vam_inference::mock_script::STEP_MARKER;
    text.split_whitespace()
        .filter(|w| !w.starts_with(marker))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sets a step's `model:` as the run's `inputs.model`; `none` removes it. Leaves the body as is
/// when the step names none.
fn put_model(body: &mut Value, model: Option<&str>) {
    let Some(m) = model else { return };
    if let Some(inputs) = body["spec"]["inputs"].as_object_mut() {
        if m == "none" {
            inputs.remove("model");
        } else {
            inputs.insert("model".into(), json!(m));
        }
    }
}

fn put_harness(body: &mut Value, harness: Option<&serde_yaml::Value>, profile: Option<&str>) {
    let mut value = match harness {
        Some(h) => serde_json::to_value(h).unwrap_or(Value::Null),
        None if profile.is_some() => json!({}),
        None => return,
    };
    // The run's profile; a step that names its own keeps it.
    if let (Some(p), Some(o)) = (profile, value.as_object_mut()) {
        o.entry("profile").or_insert_with(|| json!(p));
    }
    if let Some(inputs) = body["spec"]["inputs"].as_object_mut() {
        inputs.insert("harness".into(), value);
    }
}

/// The text a message part carries: text, tool-call arguments, and a tool
/// result's inner text.
fn part_text(part: &Value, out: &mut String) {
    match part["type"].as_str() {
        Some("text") => out.push_str(part["text"].as_str().unwrap_or_default()),
        Some("tool_use") => out.push_str(part["arguments"].as_str().unwrap_or_default()),
        Some("tool_result") => {
            for inner in part["content"].as_array().into_iter().flatten() {
                part_text(inner, out);
            }
        }
        _ => {}
    }
}

/// Does a thread messages response (`{records:[{kind, message:{role, parts[]}}]}`)
/// hold a message of `role` whose text contains `contains`?
pub fn thread_has_message(body: &Value, role: &str, contains: &str) -> bool {
    body["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["kind"].as_str() == Some("message"))
        .filter(|r| r["message"]["role"].as_str() == Some(role))
        .any(|r| {
            let mut text = String::new();
            for part in r["message"]["parts"].as_array().into_iter().flatten() {
                part_text(part, &mut text);
            }
            text.contains(contains)
        })
}

/// The text sizes, in bytes, of the thread messages of `role` whose text contains `contains`.
pub fn thread_message_sizes(body: &Value, role: &str, contains: &str) -> Vec<usize> {
    body["records"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|r| r["kind"].as_str() == Some("message"))
        .filter(|r| r["message"]["role"].as_str() == Some(role))
        .filter_map(|r| {
            let mut text = String::new();
            for part in r["message"]["parts"].as_array().into_iter().flatten() {
                part_text(part, &mut text);
            }
            text.contains(contains).then_some(text.len())
        })
        .collect()
}

/// Maps a scenario's state word to a playbook step status.
pub fn step_matches(want: &str, st: &Value) -> bool {
    let state = st["state"].as_str().unwrap_or_default();
    let outcome = st["outcome"].as_str().unwrap_or_default();
    match want {
        "waiting" => matches!(state, "STEP_STATE_PAUSED" | "STEP_STATE_AVAILABLE"),
        "decided" => state == "STEP_STATE_SUCCESS" && !outcome.is_empty(),
        "succeeded" => state == "STEP_STATE_SUCCESS",
        other => state == format!("STEP_STATE_{}", other.to_uppercase()),
    }
}

/// The value at the dotted key path `path` (`data.outcomePreview`), by exact key names.
/// A numeric segment indexes an array. Nothing is searched for: a wrong path finds nothing.
fn get_path<'a>(v: &'a Value, path: &str) -> Option<&'a Value> {
    let mut cur = v;
    for seg in path.split('.') {
        cur = match cur {
            Value::Object(m) => m.get(seg)?,
            // `key=value` picks the element of an array whose `key` is the string `value`
            // (`status.base.runtimes.name=python.version`).
            Value::Array(a) => match seg.split_once('=') {
                Some((k, want)) => a
                    .iter()
                    .find(|e| e.get(k).and_then(Value::as_str) == Some(want))?,
                None => a.get(seg.parse::<usize>().ok()?)?,
            },
            _ => return None,
        };
    }
    Some(cur)
}

/// `{ present: true }` / `{ absent: true }`: the one-key presence matchers, any `fields` family.
fn presence_want(want: &serde_yaml::Value) -> Option<(&'static str, bool)> {
    let map = want.as_mapping()?;
    let mut it = map.iter();
    let (k, v) = it.next()?;
    if it.next().is_some() {
        return None;
    }
    let name = match k.as_str()? {
        "present" => "present",
        "absent" => "absent",
        _ => return None,
    };
    Some((name, v.as_bool()?))
}

/// Does `got` (the value at the path, if any) satisfy `want`? `{ absent: true }` wants no
/// value (or `null`), `{ present: true }` wants one; `false` flips each.
fn field_matches(got: Option<&Value>, want: &serde_yaml::Value) -> bool {
    if let Some((kind, on)) = presence_want(want) {
        let there = got.is_some_and(|g| !g.is_null());
        return there == ((kind == "present") == on);
    }
    got.is_some_and(|g| value_matches(g, want))
}

/// One line per field of `fields` that `body` does not satisfy.
fn field_problems(
    body: &Value,
    fields: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> Vec<String> {
    let mut bad = Vec::new();
    for (path, want) in fields {
        let got = get_path(body, path);
        if field_matches(got, want) {
            continue;
        }
        match got {
            Some(g) => bad.push(format!("{path}: {g} (want {})", yaml_text(want))),
            None => bad.push(format!("{path}: absent (want {})", yaml_text(want))),
        }
    }
    bad
}

/// Flip one byte inside the signed `receipt` object: the first string leaf gets its last
/// character changed. `None` when the receipt holds no string.
fn tamper_receipt(body: &Value) -> Option<Value> {
    fn flip(v: &mut Value) -> bool {
        match v {
            Value::String(s) if !s.is_empty() => {
                let last = s.pop().unwrap_or('a');
                s.push(if last == 'x' { 'y' } else { 'x' });
                true
            }
            Value::Array(a) => a.iter_mut().any(flip),
            Value::Object(m) => m.values_mut().any(flip),
            _ => false,
        }
    }
    let mut out = body.clone();
    if flip(out.get_mut("receipt")?) {
        Some(out)
    } else {
        None
    }
}

/// Run the built `vam` (path in `cli`) with `args`; `(exit ok, stdout + stderr)`. A missing
/// variable or binary is `env`.
fn run_cli(cli: Option<&str>, args: &[&str]) -> Result<(bool, String), Fail> {
    let want = "the built vam runs (VAM_E2E_CLI)";
    let cli = cli.filter(|c| !c.is_empty()).ok_or_else(|| {
        Fail::new(
            Class::Env,
            want,
            "VAM_E2E_CLI is not set (scripts/e2e/run.sh exports it)",
        )
    })?;
    let out = std::process::Command::new(cli)
        .args(args)
        .output()
        .map_err(|e| Fail::new(Class::Env, want, format!("{cli}: {e}")))?;
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    Ok((out.status.success(), text.trim().to_string()))
}

/// The verdict of `verify-receipt` from the CLI's exit: a plain verify passes on success; a
/// tampered one passes only on failure whose output names the signature.
fn verify_verdict(tamper: bool, ok: bool, out: &str) -> Result<(), String> {
    match (tamper, ok) {
        (false, true) => Ok(()),
        (false, false) => Err(format!("verify failed: {out}")),
        (true, true) => Err("verify passed on a receipt with one changed byte".into()),
        (true, false) if out.to_lowercase().contains("signature") => Ok(()),
        (true, false) => Err(format!("verify failed without naming the signature: {out}")),
    }
}

fn yaml_text(v: &serde_yaml::Value) -> String {
    serde_json::to_string(v).unwrap_or_default()
}

/// Exact JSON equality. The one looser form is `{contains: "text"}`: a string value that
/// holds the text.
fn value_matches(got: &Value, want: &serde_yaml::Value) -> bool {
    if let Some(same) = outcomes::numbers_match(got, want) {
        return same;
    }
    if let Some(map) = want.as_mapping() {
        let mut it = map.iter();
        if let (Some((k, v)), None) = (it.next(), it.next()) {
            if k.as_str() == Some("contains") {
                return match (got.as_str(), v.as_str()) {
                    (Some(g), Some(w)) => g.contains(w),
                    _ => false,
                };
            }
        }
    }
    serde_json::to_value(want)
        .map(|w| *got == w)
        .unwrap_or(false)
}

/// Does a pending approval's claims match `expect: {approval|gate, tool, step, fields}`?
/// `fields` are exact dotted camelCase paths into the claims, as for events.
pub fn claims_match(
    claims: &Value,
    tool: Option<&str>,
    step: Option<&str>,
    fields: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> bool {
    tool.is_none_or(|t| claims["targetToolName"].as_str() == Some(t))
        && step.is_none_or(|st| claims["stepId"].as_str() == Some(st))
        && fields
            .iter()
            .all(|(path, want)| field_matches(get_path(claims, path), want))
}

/// Does run event `ev` (`{eventType, payload}`) match an `expect: {event, tool, fields}`?
/// `event: .., absent: true`: a matching event is a failure now; with none, the step holds once
/// the run has ended or parked and settled (`settled`), and waits until then.
fn absent_verdict(
    events: &[Value],
    name: &str,
    tool: Option<&str>,
    fields: &std::collections::BTreeMap<String, serde_yaml::Value>,
    settled: bool,
) -> Probe {
    if let Some(ev) = events
        .iter()
        .find(|ev| event_matches(ev, name, tool, fields))
    {
        // Name the event itself: a CI failure is often the only evidence.
        Probe::Impossible(format!("an event {name} appeared: {}", event_evidence(ev)))
    } else if settled {
        Probe::Met
    } else {
        Probe::Pending
    }
}

/// One event as failure evidence: its sequence, type and payload, cut to 600 characters.
fn event_evidence(ev: &Value) -> String {
    let payload = match &ev["payload"] {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let payload: String = payload.chars().take(600).collect();
    format!(
        "sequence {} {} {}",
        ev["sequence"],
        ev["eventType"].as_str().unwrap_or_default(),
        payload
    )
}

pub fn event_matches(
    ev: &Value,
    name: &str,
    tool: Option<&str>,
    fields: &std::collections::BTreeMap<String, serde_yaml::Value>,
) -> bool {
    if bare(ev["eventType"].as_str().unwrap_or_default()) != bare(name) {
        return false;
    }
    let payload = match &ev["payload"] {
        Value::String(s) => serde_json::from_str(s).unwrap_or(Value::String(s.clone())),
        other => other.clone(),
    };
    if let Some(t) = tool {
        let ok = ["tool_name", "toolName", "tool"]
            .iter()
            .any(|k| get_path(&payload, k).and_then(Value::as_str) == Some(t));
        if !ok {
            return false;
        }
    }
    fields
        .iter()
        .all(|(path, want)| field_matches(get_path(&payload, path), want))
}

/// The body of a decision. vamd takes `remember` only with the decision it implies (Always
/// allow with approve, Always deny with reject) and requires a `rejectionReason` on every
/// reject that is not remembered (a remembered one gets its reason from vamd).
fn decide_body(req_id: &str, decision: &str, response: Option<Value>, always: bool) -> Value {
    let mut body = json!({ "requestId": { "urn": req_id }, "decision": decision });
    if let Some(r) = response {
        body["response"] = r;
    }
    if always {
        body["remember"] = json!(match decision {
            "HITL_DECISION_APPROVED" => "REMEMBER_ALWAYS_ALLOW",
            _ => "REMEMBER_ALWAYS_DENY",
        });
    } else if decision == "HITL_DECISION_REJECTED" {
        body["rejectionReason"] = json!("denied by scenario");
    }
    body
}

impl World for Engine {
    fn arrange(&mut self, scenario: &Scenario, _tag: &RunTag) -> Result<Arranged, String> {
        self.arrange_inner(scenario).map_err(|f| f.observed)
    }
    fn cleanup(&mut self, _arranged: Arranged) -> Result<(), String> {
        self.cleanup_inner();
        Ok(())
    }
}

impl StepExecutor for Engine {
    fn exec(&mut self, scenario: &Scenario, position: &str) -> StepOutcome {
        let Some((_, step)) = scenario.steps().find(|(p, _)| p == position) else {
            return StepOutcome::Failed {
                class: Class::Harness,
                expected: format!("step {position}"),
                observed: "no such step".into(),
            };
        };
        let step = step.clone();
        let r = if position == "when.0" {
            self.preflight(scenario)
                .and_then(|_| self.step(scenario, &step))
        } else {
            self.step(scenario, &step)
        };
        match r {
            Ok(()) => StepOutcome::Ok,
            Err(f) => StepOutcome::Failed {
                class: f.class,
                expected: f.expected,
                observed: f.observed,
            },
        }
    }
}

impl Engine {
    /// Arrange, for the driver, with the full failure.
    pub fn arrange_full(&mut self, s: &Scenario) -> Result<Arranged, Fail> {
        self.arrange_inner(s)
    }
    pub fn cleanup_full(&mut self) {
        self.cleanup_inner();
    }

    /// The receipt and the thread of each run of the scenario, for usage (H-0.8). Best effort:
    /// a run that has not ended has no receipt, and a read that fails is `None`; this never
    /// fails a scenario. Call it before `cleanup_full`.
    pub fn measure_runs(&mut self) -> Vec<crate::usage::RunExtra> {
        let mut out = Vec::new();
        for i in 0..self.ctx.runs.len() {
            let mut extra = crate::usage::RunExtra::default();
            if self.prepare_error.is_none() && !self.ctx.runs[i].run_id.is_empty() {
                let _ = self.refresh(i);
                let key = self.run_key(i);
                let (ws, th, run_id) = {
                    let r = &self.ctx.runs[i];
                    (
                        r.workspace_id.clone(),
                        r.thread_id.clone(),
                        r.run_id.clone(),
                    )
                };
                if is_terminal(&self.ctx.runs[i].state) {
                    let path = format!("/v1/runs/{}/receipt", seg(&run_id));
                    let api = &self.api;
                    let r = poll_while_pending(
                        || api.get_as(key.as_deref(), &path),
                        RECEIPT_MEASURE_BUDGET,
                        RECEIPT_POLL,
                        Instant::now() + RECEIPT_MEASURE_BUDGET,
                    );
                    extra.receipt = r.ok().filter(|r| r.ok()).map(|r| r.json());
                }
                if !ws.is_empty() && !th.is_empty() {
                    let r = self.api.get(&format!(
                        "/v1/workspaces/{}/threads/{}/messages?view=all&limit=500",
                        seg(&ws),
                        seg(&th)
                    ));
                    extra.thread = r.ok().filter(|r| r.ok()).map(|r| r.json());
                }
            }
            out.push(extra);
        }
        out
    }
    pub fn vamd_log_tail(&self, n: usize) -> String {
        self.vamd.log_tail(n)
    }
    pub fn guest_logs(&self) -> Vec<std::path::PathBuf> {
        self.vamd.guest_logs()
    }
}

/// How long usage measurement waits for a receipt that is not issued yet.
const RECEIPT_MEASURE_BUDGET: Duration = Duration::from_secs(3);

/// How often a pending receipt is asked for again.
const RECEIPT_POLL: Duration = Duration::from_millis(250);

// ---- secrets, skills, floods, paging and secret-absent -----------------------------------------

/// What a `flood` step saw: answers by status, and the refusal headers.
#[derive(Debug, Clone, Default)]
struct FloodSeen {
    path: String,
    /// The key the requests used (`None`: the bootstrap key). Never printed.
    key: Option<Secret>,
    by_status: std::collections::BTreeMap<u16, u32>,
    /// `Retry-After` of every 429, as sent (None when the header was missing).
    retry_after: Vec<Option<String>>,
    reasons: std::collections::BTreeSet<String>,
}

impl FloodSeen {
    fn limited(&self) -> u32 {
        self.by_status.get(&429).copied().unwrap_or(0)
    }
    fn counts(&self) -> String {
        let parts: Vec<String> = self
            .by_status
            .iter()
            .map(|(s, n)| {
                format!(
                    "{n} x {}",
                    if *s == 0 {
                        "no answer".into()
                    } else {
                        s.to_string()
                    }
                )
            })
            .collect();
        format!("GET {}: {}", self.path, parts.join(", "))
    }
}

/// Seconds a `Retry-After` value asks for (whole seconds; a date form is not used by vamd).
fn retry_seconds(v: &str) -> Option<u64> {
    v.trim().parse().ok()
}

impl Engine {
    /// `{{name}}` is the piece's own name; `{{urn:<piece>}}` and `{{name:<piece>}}` are the URN
    /// and run-unique name of a piece it `requires`.
    fn subst(&self, text: String, name: &str) -> String {
        let mut text = text.replace("{{name}}", name);
        for (alias, rec) in &self.ctx.pieces {
            text = text
                .replace(&format!("{{{{urn:{alias}}}}}"), &rec.urn)
                .replace(&format!("{{{{name:{alias}}}}}"), &rec.name);
        }
        text
    }

    /// The value never leaves `Secret`; an error text from vamd is scrubbed of it.
    fn scrub(&self, text: &str) -> String {
        let mut out = text.to_string();
        for v in self.ctx.secrets.values() {
            if !v.0.is_empty() {
                out = out.replace(&v.0, "<secret>");
            }
        }
        out
    }

    /// A `secret` piece: an org secret with a random value made here and kept in memory. The
    /// value carries characters that percent and JSON encodings change.
    fn put_secret_piece(&mut self, alias: &str, name: &str, wp: &WorldPiece) -> Result<(), Fail> {
        let value = format!("tok-{}/+=&\"z", secret_hex(12));
        // Two spellings of an egress secret: `egress: {bindingId, allowedDestinations}` (the
        // binding id is used as written) or `secretKind: egress` with `bindingSuffix` and
        // `allowedDestinations` (the binding id is the run-unique name plus the suffix).
        let (binding_id, kind, destinations) = match (&wp.egress, wp.secret_kind.as_deref()) {
            (Some(spec), _) => (
                spec.binding_id.clone(),
                2,
                spec.allowed_destinations.clone(),
            ),
            (None, kind) => {
                let kind = match kind {
                    None | Some("inference") => 1,
                    Some("egress") => 2,
                    Some(other) => {
                        return Err(Fail::new(
                            Class::Scenario,
                            "a secret kind of inference or egress",
                            format!("kind `{other}` of secret piece `{alias}`"),
                        ))
                    }
                };
                (
                    format!("{name}{}", wp.binding_suffix.as_deref().unwrap_or("")),
                    kind,
                    wp.allowed_destinations.clone(),
                )
            }
        };
        let r = self
            .api
            .put(
                "/v1/secrets",
                &json!({
                    "bindingId": binding_id,
                    "value": value,
                    "kind": kind,
                    "scope": 1,
                    "allowedDestinations": destinations,
                }),
            )
            .map_err(conn)?;
        self.ctx.secrets.insert(alias.to_string(), Secret(value));
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                format!("PUT /v1/secrets for piece `{alias}`"),
                format!("{} {}", r.status, self.scrub(&r.body)),
            ));
        }
        Ok(())
    }

    /// An `mcp-connection` piece: an org OAuth connection registered through
    /// `PUT /v1/mcp/connections` (no live server is contacted for an OAuth connection without
    /// headers), then random access and refresh tokens upserted through
    /// `PUT /v1/mcp/connections/{id}/oauth/tokens`. The access token is kept under the piece's
    /// alias for `secret-absent`; every value is scrubbed from error text.
    fn put_mcp_connection_piece(
        &mut self,
        alias: &str,
        wp: &WorldPiece,
        name: &str,
    ) -> Result<(), Fail> {
        let text = self.subst(
            wp.body.as_ref().map(Value::to_string).unwrap_or_default(),
            name,
        );
        let mut body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
        let id = body["connectionId"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        body["scope"] = json!("org");
        let r = self.api.put("/v1/mcp/connections", &body).map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                format!("PUT /v1/mcp/connections for piece `{alias}`"),
                format!("{} {}", r.status, self.scrub(&r.body)),
            ));
        }
        self.ctx.mcp_connections.push(id.clone());
        let access = format!("mcpa-{}", secret_hex(16));
        let refresh = format!("mcpr-{}", secret_hex(16));
        let client = format!("mcpc-{}", secret_hex(16));
        self.ctx
            .secrets
            .insert(alias.to_string(), Secret(access.clone()));
        self.ctx
            .secrets
            .insert(format!("{alias}#refresh"), Secret(refresh.clone()));
        self.ctx
            .secrets
            .insert(format!("{alias}#client"), Secret(client.clone()));
        let r = self
            .api
            .put(
                &format!("/v1/mcp/connections/{}/oauth/tokens?scope=org", seg(&id)),
                &json!({ "accessToken": access, "refreshToken": refresh, "clientSecret": client }),
            )
            .map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                format!("PUT /v1/mcp/connections/{id}/oauth/tokens for piece `{alias}`"),
                format!("{} {}", r.status, self.scrub(&r.body)),
            ));
        }
        Ok(())
    }

    /// A `skill` piece: `body.spec` is the manifest. It is packed with a `SKILLS.md`, published
    /// as a package and released as 1.0.0, so an agent's `skillRefs` can name it.
    fn publish_skill_piece(&mut self, wp: &WorldPiece, name: &str, urn: &str) -> Result<(), Fail> {
        let piece = &wp.piece;
        let archive = self.skill_package(wp, name, urn)?;
        let r = self
            .api
            .post_file("/v1/skills/packages", "skill.tar.gz", archive)
            .map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                format!("POST /v1/skills/packages for piece `{piece}`"),
                format!("{} {}", r.status, r.body),
            ));
        }
        self.release_skill_piece(piece, urn)
    }

    /// `publish-skill: { piece, refused, contains }`: publishes and releases a `deferred` skill
    /// piece as a step, so a refusal at publish is a product result.
    fn publish_skill_step(&mut self, p: &crate::scenario::PublishSkill) -> Result<(), Fail> {
        let rec = self.ctx.pieces.get(&p.piece).cloned().ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                format!("world piece `{}`", p.piece),
                "not in `given`",
            )
        })?;
        let wp = self.pieces.get(&rec.piece).cloned().ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                format!("world piece `{}`", rec.piece),
                "unknown piece",
            )
        })?;
        let archive = self.skill_package(&wp, &rec.name, &rec.urn)?;
        let r = self
            .api
            .post_file("/v1/skills/packages", "skill.tar.gz", archive)
            .map_err(conn)?;
        let shown = format!(
            "{} {}",
            r.status,
            self.scrub(&r.body.chars().take(400).collect::<String>())
        );
        if p.refused {
            if !(400..500).contains(&r.status) {
                return Err(Fail::new(
                    Class::Product,
                    "publishing the skill is refused (a 4xx answer)",
                    shown,
                ));
            }
            for c in &p.contains {
                if !r.body.contains(c.as_str()) {
                    return Err(Fail::new(
                        Class::Product,
                        format!("the refusal contains `{c}`"),
                        shown,
                    ));
                }
            }
            return Ok(());
        }
        if !r.ok() {
            return Err(Fail::new(
                Class::Product,
                "publishing the skill is accepted",
                shown,
            ));
        }
        self.release_skill_piece(&p.piece, &rec.urn)?;
        // An agent piece marked `deferred` that requires this skill is released now.
        let agents: Vec<(String, String)> = self
            .ctx
            .pieces
            .values()
            .filter(|r| r.kind == PieceKind::Agent)
            .filter_map(|r| {
                let a = self.pieces.get(&r.piece)?;
                (a.deferred && a.release && a.requires.contains(&p.piece))
                    .then(|| (r.piece.clone(), r.urn.clone()))
            })
            .collect();
        for (piece, urn) in agents {
            let r = self
                .api
                .post(
                    &format!("/v1/agents/{}/release", seg(&urn)),
                    &json!({ "version": { "major": 1, "minor": 0, "patch": 0 } }),
                )
                .map_err(conn)?;
            if !r.ok() {
                return Err(Fail::new(
                    Class::Harness,
                    format!("release of piece `{piece}`"),
                    format!("{} {}", r.status, r.body),
                ));
            }
        }
        Ok(())
    }

    fn release_skill_piece(&mut self, piece: &str, urn: &str) -> Result<(), Fail> {
        let r = self
            .api
            .post(
                &format!("/v1/skills/packages/{}/release", seg(urn)),
                &json!({ "urn": urn, "version": { "major": 1, "minor": 0, "patch": 0 } }),
            )
            .map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Harness,
                format!("release of skill piece `{piece}`"),
                format!("{} {}", r.status, r.body),
            ));
        }
        Ok(())
    }

    fn skill_package(&mut self, wp: &WorldPiece, name: &str, urn: &str) -> Result<Vec<u8>, Fail> {
        let piece = &wp.piece;
        let spec = wp
            .body
            .as_ref()
            .and_then(|b| b.get("spec"))
            .cloned()
            .unwrap_or(Value::Null);
        let spec: Value =
            serde_json::from_str(&self.subst(spec.to_string(), name)).unwrap_or(Value::Null);
        let mut files = Vec::new();
        for (dest, src) in &wp.files {
            let base = wp.fixture_dir.clone().unwrap_or_default();
            let bytes = std::fs::read(base.join(src)).map_err(|e| {
                Fail::new(
                    Class::Harness,
                    format!("read fixture file `{src}` of piece `{piece}`"),
                    e.to_string(),
                )
            })?;
            files.push((dest.clone(), bytes));
        }
        let archive = crate::forms::skill_archive(&spec, urn, name, &files)
            .map_err(|e| Fail::new(Class::Harness, format!("pack skill piece `{piece}`"), e))?;
        Ok(archive)
    }

    /// Every page of `GET /v1/approvals` for `key`: the first page, then each
    /// `pagination.nextPageToken` until it is empty (a node that does not page answers one page).
    fn approval_pages(&self, key: Option<&str>) -> Result<Vec<Value>, Fail> {
        let mut pages = Vec::new();
        let mut token = String::new();
        for _ in 0..200 {
            let path = if token.is_empty() {
                "/v1/approvals".to_string()
            } else {
                format!("/v1/approvals?pageToken={}", seg(&token))
            };
            let v = self.api.get_as(key, &path).map_err(conn)?.json();
            token = v["pagination"]["nextPageToken"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            pages.push(v);
            if token.is_empty() {
                break;
            }
        }
        Ok(pages)
    }

    /// `flood: { as, path, count }`: quick GETs with one identity's key. It records what
    /// came back; `expect: { flood }` judges it.
    fn flood(&mut self, f: &crate::scenario::Flood) -> Result<(), Fail> {
        let key = self.key_of(f.as_.as_deref())?;
        let answers = self
            .api
            .flood_get(key.as_deref(), &f.path, f.count)
            .map_err(conn)?;
        let mut seen = FloodSeen {
            path: f.path.clone(),
            key: key.map(Secret),
            ..Default::default()
        };
        for a in &answers {
            *seen.by_status.entry(a.status).or_default() += 1;
            if a.status == 429 {
                seen.retry_after.push(a.retry_after.clone());
            }
            if let Some(r) = &a.reason {
                seen.reasons.insert(r.clone());
            }
        }
        let silent = seen.by_status.get(&0).copied().unwrap_or(0);
        if silent as usize == answers.len() && !answers.is_empty() {
            return Err(Fail::new(
                Class::Env,
                format!("vamd answers GET {}", f.path),
                "no request was answered".to_string(),
            ));
        }
        self.ctx.flood = Some(seen);
        Ok(())
    }

    /// `expect: { flood: { rateLimited, retryAfter, recovers } }` over the latest `flood` step.
    fn expect_flood(&mut self, x: &crate::scenario::FloodExpect) -> Result<(), Fail> {
        let what = format!(
            "flood: {}",
            [
                x.rate_limited
                    .as_ref()
                    .map(|a| format!("at least {} answered 429", a.at_least)),
                x.retry_after
                    .as_ref()
                    .map(|_| "429 carries Retry-After".to_string()),
                x.recovers
                    .filter(|r| *r)
                    .map(|_| "answered again after Retry-After".to_string()),
            ]
            .into_iter()
            .flatten()
            .collect::<Vec<_>>()
            .join("; ")
        );
        let Some(seen) = self.ctx.flood.clone() else {
            return Err(Fail::new(
                Class::Scenario,
                what,
                "no `flood` step ran before it",
            ));
        };
        let reasons = if seen.reasons.is_empty() {
            "no X-VAM-Rate-Limited-Reason".to_string()
        } else {
            format!(
                "X-VAM-Rate-Limited-Reason {}",
                seen.reasons.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        };
        if let Some(a) = &x.rate_limited {
            if seen.limited() < a.at_least {
                return Err(Fail::new(
                    Class::Product,
                    what,
                    format!(
                        "{} answered 429, {}; {}",
                        seen.limited(),
                        seen.counts(),
                        reasons
                    ),
                ));
            }
        }
        if let Some(want) = &x.retry_after {
            if want != "present" {
                return Err(Fail::new(
                    Class::Scenario,
                    "retryAfter: present",
                    format!("`retryAfter: {want}` is not a form"),
                ));
            }
            let missing = seen
                .retry_after
                .iter()
                .filter(|v| v.as_deref().and_then(retry_seconds).is_none())
                .count();
            if seen.retry_after.is_empty() || missing > 0 {
                return Err(Fail::new(
                    Class::Product,
                    what,
                    format!(
                        "{} of {} 429 answers carry no Retry-After in seconds ({})",
                        if seen.retry_after.is_empty() {
                            0
                        } else {
                            missing
                        },
                        seen.retry_after.len(),
                        seen.counts()
                    ),
                ));
            }
        }
        if x.recovers == Some(true) {
            let wait = seen
                .retry_after
                .iter()
                .flatten()
                .filter_map(|v| retry_seconds(v))
                .max()
                .unwrap_or(1)
                .min(30);
            std::thread::sleep(Duration::from_secs(wait) + Duration::from_millis(200));
            let key = seen.key.as_ref().map(|k| k.0.as_str());
            let a = self.api.get_answer(key, &seen.path).map_err(conn)?;
            if !(200..300).contains(&a.status) {
                return Err(Fail::new(
                    Class::Product,
                    what,
                    format!(
                        "after waiting {wait}s the same key was answered {} ({})",
                        a.status,
                        seen.counts()
                    ),
                ));
            }
        }
        Ok(())
    }

    /// `expect: { secret-absent: { secret } }`: no form of the secret's value is in the latest
    /// run's thread, run events, receipt (with its model appendix) or workspace files. A hit
    /// names the form and the place; the value is never put in the failure.
    fn expect_secret_absent(&mut self, x: &crate::scenario::SecretAbsent) -> Result<(), Fail> {
        let what = format!(
            "no form of secret `{}` in what the run left behind",
            x.secret
        );
        let value = match self.ctx.secrets.get(&x.secret) {
            Some(v) => v.0.clone(),
            None => {
                return Err(Fail::new(
                    Class::Scenario,
                    what,
                    format!("`{}` is not a secret piece in `given`", x.secret),
                ))
            }
        };
        // A piece may hold companion values (`<alias>#refresh`, `<alias>#client`): all are judged.
        let companions: Vec<String> = self
            .ctx
            .secrets
            .iter()
            .filter(|(k, _)| k.starts_with(&format!("{}#", x.secret)))
            .map(|(_, v)| v.0.clone())
            .collect();
        let values: Vec<String> = std::iter::once(value.clone()).chain(companions).collect();
        let i = self.last_run()?;
        let _ = self.refresh(i);
        let key = self.run_key(i);
        let ws = self.ctx.runs[i].workspace_id.clone();
        let thread = self.ctx.runs[i].thread_id.clone();
        let leak = |place: &str, v: &Value| -> Option<Fail> {
            let mut strings = Vec::new();
            crate::forms::strings_of(v, &mut strings);
            strings.iter().find_map(|t| {
                values
                    .iter()
                    .find_map(|value| crate::forms::find_form(value, t))
                    .map(|form| {
                        Fail::new(
                            Class::Product,
                            what.clone(),
                            format!("the {form} form is in the {place}"),
                        )
                    })
            })
        };

        // Thread messages (every view).
        let r = self
            .api
            .get_as(
                key.as_deref(),
                &format!(
                    "/v1/workspaces/{}/threads/{}/messages?view=all&limit=500",
                    seg(&ws),
                    seg(&thread)
                ),
            )
            .map_err(conn)?;
        if r.ok() {
            if let Some(f) = leak("thread messages", &r.json()) {
                return Err(f);
            }
        }
        // Run events.
        let events = Value::Array(self.ctx.runs[i].events.clone());
        if let Some(f) = leak("run events", &events) {
            return Err(f);
        }
        // The receipt and its model appendix.
        let (_, receipt) = self.fetch_receipt(&what)?;
        if let Some(f) = leak("receipt", &receipt) {
            return Err(f);
        }
        // Workspace files.
        let listing = self
            .api
            .get_as(
                key.as_deref(),
                &format!("/v1/workspaces/{}/files", seg(&ws)),
            )
            .map_err(conn)?;
        if listing.ok() {
            let paths: Vec<String> = listing.json()["files"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| f["path"].as_str().map(String::from))
                .take(200)
                .collect();
            for path in paths {
                let r = self
                    .api
                    .get_as(
                        key.as_deref(),
                        &format!("/v1/workspaces/{}/files/{}", seg(&ws), path),
                    )
                    .map_err(conn)?;
                if !r.ok() {
                    continue;
                }
                if let Some(form) = values
                    .iter()
                    .find_map(|v| crate::forms::find_form(v, &r.body))
                {
                    return Err(Fail::new(
                        Class::Product,
                        what,
                        format!("the {form} form is in the workspace file {path}"),
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Calls `get` until it answers something other than 409, or the budget or the
/// scenario deadline is spent; then returns the last answer.
fn poll_while_pending<E>(
    mut get: impl FnMut() -> Result<Reply, E>,
    budget: Duration,
    poll: Duration,
    deadline: Instant,
) -> Result<Reply, E> {
    let start = Instant::now();
    loop {
        let r = get()?;
        if r.status != 409 || start.elapsed() >= budget || Instant::now() >= deadline {
            return Ok(r);
        }
        std::thread::sleep(poll);
    }
}

/// The first non-empty string among the values.
fn first_str(values: &[&Value]) -> String {
    values
        .iter()
        .filter_map(|v| v.as_str())
        .find(|s| !s.is_empty())
        .unwrap_or_default()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoVamd;
    impl crate::vamd::Vamd for NoVamd {
        fn start(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn stop(&mut self) {}
        fn restart(&mut self) -> Result<(), String> {
            Ok(())
        }
        fn base_url(&self) -> String {
            "http://127.0.0.1:1".into()
        }
        fn alive(&mut self) -> bool {
            false
        }
        fn log_tail(&self, _: usize) -> String {
            String::new()
        }
    }

    fn engine() -> Engine {
        Engine::new(
            Api::new("http://127.0.0.1:1", "k", "tag"),
            Box::new(NoVamd),
            Cfg::new(RunTag("abc123".into())),
            Vec::new(),
        )
    }

    /// The names a scenario run uses: piece name, thread names, idempotency key.
    fn names(e: &mut Engine) -> (String, String, String, String) {
        e.begin();
        let piece = piece_name("agent-plain", "some-task", &e.eff_tag());
        let key = e.next_key();
        (piece, e.thread_name(1), e.thread_name(2), key)
    }

    #[test]
    fn plain_runs_keep_todays_names() {
        let mut e = engine();
        let (piece, t1, t2, key) = names(&mut e);
        assert_eq!(piece, "agent-plain-some-task-abc123");
        assert_eq!((t1.as_str(), t2.as_str()), ("t1", "t2"));
        assert_eq!(key, "e2e-abc123-1");
        e.set_trial("some-task", None);
        assert_eq!(names(&mut e).0, "agent-plain-some-task-abc123");
    }

    #[test]
    fn k_trials_of_one_scenario_share_no_name() {
        let mut e = engine();
        let mut seen = Vec::new();
        for scenario in ["some-task", "other-task"] {
            for model in ["mock-model", "other/model"] {
                for n in 1..=3 {
                    e.set_trial(scenario, Some((model, n)));
                    seen.push(names(&mut e));
                }
            }
        }
        for (a, x) in seen.iter().enumerate() {
            for y in &seen[a + 1..] {
                assert_ne!(x.0, y.0, "piece names");
                assert_ne!(x.1, y.1, "first thread names");
                assert_ne!(x.2, y.2, "second thread names");
                assert_ne!(x.3, y.3, "idempotency keys");
            }
        }
        e.set_trial("some-task", None);
        assert_eq!(names(&mut e).1, "t1");
    }

    #[test]
    fn trial_names_fit_the_64_character_limit() {
        let mut e = engine();
        e.set_trial("some-task", Some(("anthropic/claude-haiku-4.5", 10)));
        e.begin();
        let alias = "org-admin-with-a-very-long-alias";
        let long = piece_name(
            alias,
            "a-scenario-id-that-is-long-enough-to-matter-here",
            &e.eff_tag(),
        );
        let user = identity_name(&long, alias, &e.eff_tag().0);
        assert!(user.len() <= 64, "{user}");
        assert!(user.ends_with(&e.eff_tag().0), "{user}");
    }

    #[test]
    fn absent_event_holds_when_settled_and_fails_when_one_appears() {
        let none = std::collections::BTreeMap::new();
        let ev = vec![
            json!({ "eventType": "vam.v1.tool.invoked", "payload": { "tool_name": "write_file" } }),
        ];
        let v = absent_verdict(&ev, "tool.invoked", Some("load_skill"), &none, true);
        assert!(matches!(v, Probe::Met));
        let v = absent_verdict(&ev, "tool.invoked", Some("load_skill"), &none, false);
        assert!(matches!(v, Probe::Pending));
        let v = absent_verdict(&ev, "tool.invoked", Some("write_file"), &none, true);
        assert!(matches!(v, Probe::Impossible(_)));
    }

    /// The failure names the event that should not be there, payload included.
    #[test]
    fn an_absent_event_that_appears_is_named_in_the_failure() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "data.disposition".to_string(),
            serde_yaml::Value::String("ALLOW".into()),
        );
        let ev = vec![json!({
            "sequence": 41,
            "eventType": "vam.v1.egress.decided",
            "payload": r#"{"data":{"domain":"leak.example","port":443,"disposition":"ALLOW","policyId":"gen-1"}}"#
        })];
        let Probe::Impossible(m) = absent_verdict(&ev, "egress.decided", None, &fields, true)
        else {
            panic!("an ALLOW must fail the absent check");
        };
        assert!(m.contains("sequence 41"), "{m}");
        assert!(m.contains("leak.example") && m.contains("gen-1"), "{m}");
    }

    #[test]
    fn harness_goes_into_the_run_inputs_and_is_absent_when_unset() {
        let base = json!({ "spec": { "inputs": { "prompt": "p", "model": "m" } } });
        let mut unset = base.clone();
        put_harness(&mut unset, None, None);
        assert_eq!(unset, base);
        let h: serde_yaml::Value = serde_yaml::from_str("{ loop: { detect: true } }").unwrap();
        let mut set = base.clone();
        put_harness(&mut set, Some(&h), None);
        assert_eq!(
            set["spec"]["inputs"]["harness"],
            json!({ "loop": { "detect": true } })
        );
        assert_eq!(set["spec"]["inputs"]["prompt"], json!("p"));
    }

    #[test]
    fn the_profile_is_sent_on_every_chat_beside_step_switches() {
        let base = json!({ "spec": { "inputs": { "prompt": "p" } } });
        let mut b = base.clone();
        put_harness(&mut b, None, Some("open-small"));
        assert_eq!(
            b["spec"]["inputs"]["harness"],
            json!({ "profile": "open-small" })
        );
        let h: serde_yaml::Value = serde_yaml::from_str("{ loop: { detect: true } }").unwrap();
        let mut b = base.clone();
        put_harness(&mut b, Some(&h), Some("open-small"));
        assert_eq!(
            b["spec"]["inputs"]["harness"],
            json!({ "loop": { "detect": true }, "profile": "open-small" })
        );
        let own: serde_yaml::Value = serde_yaml::from_str("{ profile: base }").unwrap();
        let mut b = base;
        put_harness(&mut b, Some(&own), Some("open-small"));
        assert_eq!(b["spec"]["inputs"]["harness"], json!({ "profile": "base" }));
    }

    #[test]
    fn decision_bodies() {
        let b = decide_body("r", "HITL_DECISION_REJECTED", None, false);
        assert_eq!(b["rejectionReason"], "denied by scenario");
        assert!(b.get("remember").is_none());
        let b = decide_body("r", "HITL_DECISION_REJECTED", None, true);
        assert_eq!(b["remember"], "REMEMBER_ALWAYS_DENY");
        assert!(b.get("rejectionReason").is_none());
        let b = decide_body("r", "HITL_DECISION_APPROVED", None, false);
        assert!(b.get("rejectionReason").is_none());
    }

    #[test]
    fn receipt_fields_match_a_canned_body() {
        let body = json!({"receipt":{"toolCalls":{"counts":{"denied":1},"lines":[{"tool":"write_file"}]},
            "model":[{"name":"mock-model"}],"note":"all good"},"signature":{"keyId":"k"}});
        let f = |y: &str| -> std::collections::BTreeMap<String, serde_yaml::Value> {
            serde_yaml::from_str(y).unwrap()
        };
        assert!(field_problems(&body, &f("receipt.toolCalls.lines.0.tool: write_file\nreceipt.model.0.name: mock-model\nreceipt.note: { contains: good }\nreceipt.toolCalls.counts.denied: 1")).is_empty());
        let bad = field_problems(
            &body,
            &f("receipt.toolCalls.lines.0.tool: read_file\nreceipt.nope: 1"),
        );
        assert_eq!(bad.len(), 2, "{bad:?}");
        assert!(bad.iter().any(|b| b.contains("absent")), "{bad:?}");
    }

    #[test]
    fn absent_and_present_matchers() {
        let body = json!({"a":{"b":1,"n":null},"s":"x"});
        let f = |y: &str| -> std::collections::BTreeMap<String, serde_yaml::Value> {
            serde_yaml::from_str(y).unwrap()
        };
        assert!(field_problems(&body, &f("a.b: { present: true }\na.c: { absent: true }\na.n: { absent: true }\ns: { present: true }")).is_empty());
        assert_eq!(field_problems(&body, &f("a.b: { absent: true }")).len(), 1);
        assert_eq!(field_problems(&body, &f("a.c: { present: true }")).len(), 1);
        assert!(field_problems(&body, &f("a.c: { present: false }")).is_empty());
        // Event/claim matching shares the matcher.
        let claims = json!({"stepId":"c"});
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "other".to_string(),
            serde_yaml::from_str("{ absent: true }").unwrap(),
        );
        assert!(claims_match(&claims, None, Some("c"), &m));
    }

    #[test]
    fn tamper_changes_one_byte_inside_the_receipt_only() {
        let body = json!({"receipt":{"n":1,"id":"abc"},"signature":{"sig":"zzz"}});
        let t = tamper_receipt(&body).unwrap();
        assert_eq!(t["signature"], body["signature"]);
        assert_ne!(t["receipt"], body["receipt"]);
        assert_eq!(t["receipt"]["id"], "abx");
        assert!(tamper_receipt(&json!({"receipt":{"n":1}})).is_none());
    }

    #[cfg(unix)]
    fn stub(dir: &std::path::Path, body: &str) -> String {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("vam");
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p.to_string_lossy().into_owned()
    }

    #[cfg(unix)]
    #[test]
    fn verify_passes_and_tamper_is_detected() {
        let d = tempfile::tempdir().unwrap();
        let ok = stub(d.path(), "echo verified; exit 0");
        let (good, out) = run_cli(Some(&ok), &["receipt", "verify", "f"]).unwrap();
        assert!(verify_verdict(false, good, &out).is_ok());
        assert!(
            verify_verdict(true, good, &out).is_err(),
            "pass on tamper is wrong"
        );
        let sig = stub(d.path(), "echo 'bad Signature' >&2; exit 3");
        let (good, out) = run_cli(Some(&sig), &["receipt", "verify", "f"]).unwrap();
        assert!(verify_verdict(true, good, &out).is_ok());
        assert!(verify_verdict(false, good, &out).is_err());
        let other = stub(d.path(), "echo boom >&2; exit 1");
        let (good, out) = run_cli(Some(&other), &[]).unwrap();
        assert!(verify_verdict(true, good, &out)
            .unwrap_err()
            .contains("signature"));
    }

    #[test]
    fn a_missing_cli_is_env() {
        for cli in [None, Some(""), Some("/nonexistent/vam")] {
            let e = run_cli(cli, &["receipt", "verify"]).unwrap_err();
            assert_eq!(e.class, Class::Env);
        }
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("300ms"), Some(Duration::from_millis(300)));
        assert_eq!(parse_duration("2m"), Some(Duration::from_secs(120)));
        assert_eq!(parse_duration("45s"), Some(Duration::from_secs(45)));
        assert_eq!(parse_duration("x"), None);
    }

    #[test]
    fn step_words() {
        let s = json!({"stepId":"c","state":"STEP_STATE_SUCCESS","outcome":"go"});
        assert!(step_matches("decided", &s) && step_matches("succeeded", &s));
        assert!(!step_matches("waiting", &s));
        assert!(step_matches(
            "waiting",
            &json!({"state":"STEP_STATE_PAUSED"})
        ));
    }

    #[test]
    fn identity_names_fit_vamd_and_secrets_do_not_print() {
        assert_eq!(
            identity_name("org-member-s-ab12", "org-member", "ab12"),
            "org-member-s-ab12"
        );
        let long = format!("org-member-{}-ab12cd", "x".repeat(60));
        let n = identity_name(&long, "org-member", "ab12cd");
        assert!(
            n.len() <= 64 && n.ends_with("-ab12cd") && n.starts_with("org-member-"),
            "{n}"
        );
        assert_eq!(n, identity_name(&long, "org-member", "ab12cd"), "stable");
        assert_eq!(format!("{:?}", Secret("hunter2".into())), "<redacted>");
        let (a, b) = (secret_hex(24), secret_hex(24));
        assert_eq!((a.len(), b.len()), (48, 48));
        assert_ne!(a, b, "random per call");
    }

    #[test]
    fn claims_match_tool_step_and_exact_paths() {
        let c = json!({"targetToolName":"write_file","stepId":"confirm","alwaysOffered":true,
            "requestedArgs":{"options":["go","stop"]}});
        let none = Default::default();
        assert!(claims_match(&c, Some("write_file"), None, &none));
        assert!(!claims_match(&c, Some("read_file"), None, &none));
        assert!(claims_match(&c, None, Some("confirm"), &none));
        assert!(!claims_match(&c, None, Some("other"), &none));
        assert!(claims_match(
            &c,
            None,
            None,
            &fields(&[("requestedArgs.options.1", "stop")])
        ));
        assert!(!claims_match(
            &c,
            None,
            None,
            &fields(&[("requestedargs.options.1", "stop")])
        ));
    }

    fn yaml(s: &str) -> serde_yaml::Value {
        serde_yaml::from_str(s).unwrap()
    }

    fn fields(pairs: &[(&str, &str)]) -> std::collections::BTreeMap<String, serde_yaml::Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), yaml(v)))
            .collect()
    }

    fn inference_event() -> Value {
        json!({"eventType":"vam.v1.inference.completed",
            "payload":"{\"data\":{\"outcomePreview\":\"Hello from the mock.\",\"n\":3,\"list\":[{\"a\":\"x\"}]}}"})
    }

    #[test]
    fn fields_match_an_exact_key_path_and_an_exact_value() {
        let ev = inference_event();
        let m = |pairs: &[(&str, &str)]| {
            event_matches(&ev, "inference.completed", None, &fields(pairs))
        };
        assert!(m(&[("data.outcomePreview", "Hello from the mock.")]));
        assert!(m(&[("data.n", "3")]));
        assert!(
            m(&[("data.list.0.a", "x")]),
            "numeric segment indexes an array"
        );
        // a key that exists only deeper does not match at the top level
        assert!(!m(&[("outcomePreview", "Hello from the mock.")]));
        // no case or underscore folding
        assert!(!m(&[("data.outcome_preview", "Hello from the mock.")]));
        assert!(!m(&[("data.OutcomePreview", "Hello from the mock.")]));
        // a substring is not a match by default
        assert!(!m(&[("data.outcomePreview", "Hello")]));
        assert!(!m(&[("data.outcomePreview", "hello from the mock.")]));
        // a missing path, a path through a scalar and a wrong type are clean misses
        assert!(!m(&[("data.nope", "x")]));
        assert!(!m(&[("data.n.x", "3")]));
        assert!(!m(&[("data.n", "\"3\"")]));
        assert!(!m(&[("data.list.9.a", "x")]));
    }

    #[test]
    fn contains_is_the_explicit_substring_form() {
        let ev = inference_event();
        let m = |y: &str| {
            let mut f = std::collections::BTreeMap::new();
            f.insert("data.outcomePreview".to_string(), yaml(y));
            event_matches(&ev, "inference.completed", None, &f)
        };
        assert!(m("{contains: Hello}"));
        assert!(m("{contains: the mock}"));
        assert!(!m("{contains: goodbye}"));
        assert!(!m("{contains: 3}"), "contains needs a string");
        assert!(!m("{contains: Hello, other: 1}"), "only the one-key form");
        // contains on a non-string value never matches
        let mut f = std::collections::BTreeMap::new();
        f.insert("data.n".to_string(), yaml("{contains: 3}"));
        assert!(!event_matches(&ev, "inference.completed", None, &f));
    }

    #[test]
    fn event_name_and_tool_are_matched_exactly() {
        let ev = inference_event();
        assert!(!event_matches(
            &ev,
            "tool.failed",
            None,
            &Default::default()
        ));
        let tf =
            json!({"eventType":"vam.v1.tool.failed","payload":"{\"tool_name\":\"read_file\"}"});
        assert!(event_matches(
            &tf,
            "tool.failed",
            Some("read_file"),
            &Default::default()
        ));
        assert!(!event_matches(
            &tf,
            "tool.failed",
            Some("write_file"),
            &Default::default()
        ));
        // the tool name is a top-level key, not searched for at depth
        let deep = json!({"eventType":"vam.v1.tool.failed","payload":"{\"x\":{\"tool_name\":\"read_file\"}}"});
        assert!(!event_matches(
            &deep,
            "tool.failed",
            Some("read_file"),
            &Default::default()
        ));
    }

    fn reply(status: u16) -> Reply {
        Reply {
            status,
            body: String::new(),
        }
    }

    #[test]
    fn a_pending_receipt_is_asked_for_again_until_it_is_served() {
        let mut answers = vec![200u16, 409, 409];
        let mut calls = 0;
        let r = poll_while_pending::<()>(
            || {
                calls += 1;
                Ok(reply(answers.pop().unwrap()))
            },
            Duration::from_secs(5),
            Duration::from_millis(1),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!((r.status, calls), (200, 3));
    }

    #[test]
    fn a_receipt_that_stays_pending_ends_with_the_last_answer_at_the_budget() {
        let mut calls = 0;
        let r = poll_while_pending::<()>(
            || {
                calls += 1;
                Ok(reply(409))
            },
            Duration::from_millis(30),
            Duration::from_millis(5),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!(r.status, 409);
        assert!(calls > 1, "{calls}");
    }

    #[test]
    fn other_answers_are_not_retried() {
        let mut calls = 0;
        let r = poll_while_pending::<()>(
            || {
                calls += 1;
                Ok(reply(404))
            },
            Duration::from_secs(5),
            Duration::from_millis(1),
            Instant::now() + Duration::from_secs(10),
        )
        .unwrap();
        assert_eq!((r.status, calls), (404, 1));
    }
}
