//! The scenario file format (`e2e/scenarios/<id>.yaml`).

use serde::Deserialize;
use std::collections::BTreeMap;

/// Area tags (design §3). Tags are areas, never phases.
pub const TAGS: &[&str] = &[
    "chat",
    "approval",
    "gate",
    "playbook",
    "release",
    "permissions",
    "workspace",
    "members",
    "egress",
    "web",
    "context",
    "triggers",
    "schedule",
    "notifications",
    "connectors",
    "slow",
];

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Scenario {
    pub id: String,
    pub title: String,
    /// Optional prose: what the scenario proves and how.
    pub description: Option<String>,
    pub tags: Vec<String>,
    pub needs: Needs,
    #[serde(default)]
    pub given: Vec<Given>,
    pub when: Vec<Step>,
    #[serde(default)]
    pub then: Vec<Step>,
    #[serde(default)]
    pub guards: Vec<String>,
    /// Inline mock rules, kept raw. `check` loads them as `vam_inference` mock rules.
    #[serde(default)]
    pub mock: Vec<serde_yaml::Value>,
    /// A benchmark task: `vam-e2e run --bench` runs this scenario k times (H-0.9).
    pub model: Option<ModelBlock>,
    pub status: Status,
    pub expect_fail: Option<ExpectFail>,
}

/// The `model` block of a benchmark task. The trial passes when every `then` holds.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelBlock {
    /// The task's name in the pinned task set.
    pub task: String,
    /// The task-set version the task belongs to (`e2e/task-sets.yaml`).
    pub version: String,
    /// Trials per model (benchmark.md section 2: 5).
    #[serde(default = "default_k")]
    pub k: u32,
    /// Only `on` exists until `machine off` does (B1).
    #[serde(default = "default_modes")]
    pub modes: Vec<String>,
    /// Per-trial timeout (300ms, 45s, 10m); the scenario budget when omitted.
    pub timeout: Option<String>,
    /// The catalog model a real run uses; `--mock` uses `mock-model` and the `mock` block.
    pub endpoint: Option<String>,
}

fn default_k() -> u32 {
    5
}

fn default_modes() -> Vec<String> {
    vec!["on".into()]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Needs {
    Process,
    Firecracker,
    Ui,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pending,
    Active,
    Retired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
    Product,
    Scenario,
    Harness,
    Env,
}

/// Where a `pending` scenario is expected to fail: `when.N` or `then.N` (0-based).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExpectFail {
    pub step: String,
    pub class: Class,
}

impl ExpectFail {
    /// `(section, index)` of the step, `None` when malformed.
    pub fn position(&self) -> Option<(&str, usize)> {
        let (sec, idx) = self.step.split_once('.')?;
        if sec != "when" && sec != "then" {
            return None;
        }
        Some((sec, idx.parse().ok()?))
    }
}

/// A piece named in `given`, optionally under an alias (`agent-with-builtins` twice).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Given {
    Name(String),
    Aliased(AliasedGiven),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AliasedGiven {
    pub piece: String,
    #[serde(rename = "as")]
    pub alias: String,
}

impl Given {
    pub fn piece(&self) -> &str {
        match self {
            Given::Name(n) => n,
            Given::Aliased(a) => &a.piece,
        }
    }
    pub fn alias(&self) -> &str {
        match self {
            Given::Name(n) => n,
            Given::Aliased(a) => &a.alias,
        }
    }
}

/// One step. Exactly one key per step.
// Public type; boxing the variants would change every constructor and match.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone)]
pub enum Step {
    Chat(Chat),
    Expect(Expect),
    /// `approve: latest`, or `approve: {always, refused, as}`
    Approve(Decide),
    /// `deny: latest`, or `deny: {always, refused, as}`
    Deny(Decide),
    DecideGate(DecideGate),
    StartPlaybook(StartPlaybook),
    /// `stop-session: {}`: terminates the session of the latest run (of `agent`, when given)
    /// and waits for the run to end. Pairs with `chat { wait: false }` to stop a running task.
    StopSession(Reserved),
    /// `restart-vamd: {}`: kills vamd and starts it again on the same port and state, then
    /// waits until it is healthy; durable state must survive.
    RestartVamd(Reserved),
    /// `verify-receipt: { tamper, pack, file, trustedKeys }`: runs the built `vam` (`VAM_E2E_CLI`) offline over the
    /// latest run's receipt, or over its evidence pack with `pack: true`.
    VerifyReceipt(VerifyReceipt),
    /// `send: { text, mode, refused, as }`: a message to the running task (steer) or the next one (queue).
    Send(crate::engine::steer::Send),
    /// `stop: { refused }`: ends the latest run through its stop route and waits for it to end.
    Stop(crate::engine::steer::Stop),
    /// `answer: { response, refused, as }`: answers the latest pending question.
    Answer(crate::engine::steer::Answer),
    /// `decide-together: { option, answer, as }`: the pending gate and the pending question are
    /// decided in parallel.
    DecideTogether(crate::engine::steer::DecideTogether),
    /// `kill-guest: {}`: SIGKILLs the latest run's guest (a crash, never a stop).
    KillGuest(crate::engine::effects::KillGuest),
    /// `flood: { as, path, count }`: `count` quick GETs of `path` with one identity's key.
    Flood(Flood),
    /// `set-node: { allowedModels, defaultModel, defaultGateway }`: the node's model settings for this scenario
    /// (`PUT /v1/node/allowed-models`); the runner restores its own at the next scenario.
    SetNode(SetNode),
    /// `fire-trigger: { key, times, kind, label }`: the test broker fires its grant.
    FireTrigger(crate::engine::triggers::FireTrigger),
    /// `revoke-grant: { via }`: the user revokes the latest grant.
    RevokeGrant(crate::engine::triggers::RevokeGrant),
    /// `bearer-request: { method, path, token, status }`: one request with the grant as bearer.
    BearerRequest(crate::engine::triggers::BearerRequest),
    /// `create-space: { label, agent }`: a new Space; later chats go into it.
    CreateSpace(crate::engine::triggers::CreateSpace),
    /// `rename-space: { label }`: renames the Space in place.
    RenameSpace(crate::engine::triggers::RenameSpace),
    /// `rename-thread: { label }`: renames the latest run's thread in place.
    RenameThread(crate::engine::triggers::RenameThread),
    /// `wait-scheduled-run: { within, label }`: waits for a run the schedule broker started.
    WaitScheduledRun(crate::engine::schedule::WaitScheduledRun),
    /// `stop-broker: {}`: stops the schedule broker process.
    StopBroker(crate::engine::schedule::BrokerStep),
    /// `start-broker: {}`: starts it again with the same dir and sockets.
    StartBroker(crate::engine::schedule::BrokerStep),
    /// `notification-mark: { action, kind, as, of, status, refused, contains }`: marks a notification read or acted.
    NotificationMark(crate::engine::notifications::Mark),
    /// `node-notify: { slack: { botToken, appToken }, appUrl }`: enables Slack for the node.
    NodeNotify(crate::engine::channels::NodeNotify),
    /// `link-contact: { channel, account, as, verify }`: adds a contact and confirms it from the account.
    LinkContact(crate::engine::channels::LinkContact),
    /// `unlink-contact: { channel, as }`.
    UnlinkContact(crate::engine::channels::UnlinkContact),
    /// `notify-prefs: { quietHours, always, as }`.
    NotifyPrefs(crate::engine::channels::NotifyPrefs),
    /// `channel-click: { account, message, action, handle, via-socket, status }`.
    ChannelClick(crate::engine::channels::ChannelClick),
    /// `connector-node: { id, enabled, consent: { tenant }, as, refused }`: `PUT /v1/connectors/{id}/node`.
    ConnectorNode(crate::engine::connectors::ConnectorNode),
    /// `connector-connect: { id, url, piece, tenant, upn, as, refused }`: connects as the app would.
    ConnectorConnect(crate::engine::connectors::ConnectorConnect),
    /// `connector-check: { id, as }`: `POST /v1/connectors/{id}/connection/check`.
    ConnectorCheck(crate::engine::connectors::ConnectorCheck),
    /// `publish-skill: { piece, refused, contains }`: publishes and releases a `deferred` skill piece.
    PublishSkill(PublishSkill),
    /// `mock-provider: { piece, revoke }`: acts on a runner-hosted mock provider.
    MockProvider(crate::engine::connectors::MockProviderStep),
    /// `cli: { args, root, exit, contains, absent }`: runs the built `vam` (`VAM_E2E_CLI`) on a
    /// scratch copy of a fixture tree, with no host and no key, and judges its exit and output.
    Cli(CliStep),
    /// `edit-agent-draft: { piece, set }`: changes paths of the piece's draft.
    EditAgentDraft(crate::engine::outcomes::EditAgentDraft),
    /// `release-agent: { piece, version, refused }`: releases the draft as a version.
    ReleaseAgent(crate::engine::outcomes::ReleaseAgent),
    /// `git-tag: { piece, tag, move, overlay }`: adds or moves a tag in a marketplace repo.
    GitTag(crate::engine::marketplace::GitTag),
    /// `marketplace-add: { piece, as, refused, contains }`: `POST /v1/marketplaces`.
    MarketplaceAdd(crate::engine::marketplace::MarketplaceAdd),
    /// `marketplace-refresh: { marketplace, as, refused }`: `POST /v1/marketplaces/{id}/refresh`.
    MarketplaceRefresh(crate::engine::marketplace::MarketplaceAct),
    /// `marketplace-install: { marketplace, listing, as, refused, contains }`.
    MarketplaceInstall(crate::engine::marketplace::MarketplaceAct),
    /// `marketplace-update: { marketplace, listing, as, refused }`.
    MarketplaceUpdate(crate::engine::marketplace::MarketplaceAct),
    /// `catalog-write: { kind, urn, action, as, refused }`: a write to an (installed) artifact.
    CatalogWrite(crate::engine::marketplace::CatalogWrite),
    /// `login: { as, password, refused }`: `POST /v1/login` as an `org` identity.
    Login(crate::engine::identity::Login),
    /// `change-password: { as, via, current, new, idempotencyKey, status, contains }`:
    /// `POST /v1/me/password` with the identity's session or key.
    ChangePassword(crate::engine::identity::ChangePassword),
    /// `identity-request: { as, credential, method, path, status }`: one request with one of the
    /// identity's credentials, judged by status.
    IdentityRequest(crate::engine::identity::IdentityRequest),
}

/// A `cli` step. `{{root}}` (the scratch tree) and `{{vamd}}` (the built `vamd`) are replaced in
/// `args` and in `contains` and `absent`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct CliStep {
    /// The arguments after `vam`.
    pub args: Vec<String>,
    /// The tree the command runs against.
    pub root: CliRoot,
    /// `ok` (exit 0) or `fail` (non-zero exit).
    pub exit: CliExit,
    /// Text the combined stdout and stderr must contain.
    #[serde(default)]
    pub contains: Vec<String>,
    /// Text that must appear in no output and in no file under the scratch tree.
    #[serde(default)]
    pub absent: Vec<String>,
    /// Text that must appear in no file under the scratch tree (the output may show it).
    #[serde(default)]
    pub absent_in_files: Vec<String>,
    /// The tree is byte-for-byte what it was before the command ran.
    #[serde(default)]
    pub unchanged: bool,
    /// The output holds a line `<prefix><secret>`; the secret is kept (never printed) for
    /// `{{secret}}` in later `contains`, `absent` and `absentInFiles` of this and later steps.
    pub capture: Option<String>,
}

/// Where a `cli` step runs: a fresh copy of a fixture directory (`from`, under
/// `e2e/world/fixtures`), or the tree the previous `cli` step left (`same`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CliRoot {
    pub from: Option<String>,
    #[serde(default)]
    pub same: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CliExit {
    #[default]
    Ok,
    Fail,
}

fn one_or_many<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum V {
        One(String),
        Many(Vec<String>),
    }
    Ok(match V::deserialize(d)? {
        V::One(s) => vec![s],
        V::Many(v) => v,
    })
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishSkill {
    /// The alias of a `skill` world piece.
    pub piece: String,
    /// The step passes only when publishing answers 4xx.
    #[serde(default)]
    pub refused: bool,
    /// With `refused`: the answer body contains this text (one string, or a list: all of them).
    #[serde(default, deserialize_with = "one_or_many")]
    pub contains: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerifyReceipt {
    /// Flip one byte inside the signed `receipt` object; verify must then fail naming the
    /// signature.
    #[serde(default)]
    pub tamper: bool,
    /// Verify the run's evidence pack (`vam evidence verify`) instead of the receipt file.
    #[serde(default)]
    pub pack: bool,
    /// Verify this receipt file (a path from the repo root) instead of the latest run's.
    pub file: Option<String>,
    /// With `file`: a node key list (`vam node keys` output) passed as `--trusted-keys`.
    #[serde(rename = "trustedKeys")]
    pub trusted_keys: Option<String>,
}

// serde_yaml 0.9 reads externally tagged enums from `!tag` only, so `Step` maps a one-key
// mapping by hand.
impl<'de> Deserialize<'de> for Step {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let map = BTreeMap::<String, serde_yaml::Value>::deserialize(d)?;
        if map.len() != 1 {
            return Err(D::Error::custom(format!(
                "a step has exactly one key, found {}",
                map.len()
            )));
        }
        let (key, v) = map.into_iter().next().expect("one entry");
        fn val<T: serde::de::DeserializeOwned, E: Error>(v: serde_yaml::Value) -> Result<T, E> {
            serde_yaml::from_value(v).map_err(E::custom)
        }
        Ok(match key.as_str() {
            "chat" => Step::Chat(val(v)?),
            "expect" => Step::Expect(val(v)?),
            "approve" => Step::Approve(val(v)?),
            "publish-skill" => Step::PublishSkill(val(v)?),
            "cli" => Step::Cli(val(v)?),
            "deny" => Step::Deny(val(v)?),
            "decide-gate" => Step::DecideGate(val(v)?),
            "start-playbook" => Step::StartPlaybook(val(v)?),
            "stop-session" => Step::StopSession(val(v)?),
            "restart-vamd" => Step::RestartVamd(val(v)?),
            "verify-receipt" => Step::VerifyReceipt(val(v)?),
            "send" => Step::Send(val(v)?),
            "stop" => Step::Stop(val(v)?),
            "answer" => Step::Answer(val(v)?),
            "decide-together" => Step::DecideTogether(val(v)?),
            "kill-guest" => Step::KillGuest(val(v)?),
            "flood" => Step::Flood(val(v)?),
            "set-node" => Step::SetNode(val(v)?),
            "fire-trigger" => Step::FireTrigger(val(v)?),
            "revoke-grant" => Step::RevokeGrant(val(v)?),
            "bearer-request" => Step::BearerRequest(val(v)?),
            "create-space" => Step::CreateSpace(val(v)?),
            "rename-space" => Step::RenameSpace(val(v)?),
            "rename-thread" => Step::RenameThread(val(v)?),
            "wait-scheduled-run" => Step::WaitScheduledRun(val(v)?),
            "stop-broker" => Step::StopBroker(val(v)?),
            "start-broker" => Step::StartBroker(val(v)?),
            "notification-mark" => Step::NotificationMark(val(v)?),
            "node-notify" => Step::NodeNotify(val(v)?),
            "link-contact" => Step::LinkContact(val(v)?),
            "unlink-contact" => Step::UnlinkContact(val(v)?),
            "notify-prefs" => Step::NotifyPrefs(val(v)?),
            "channel-click" => Step::ChannelClick(val(v)?),
            "connector-node" => Step::ConnectorNode(val(v)?),
            "connector-connect" => Step::ConnectorConnect(val(v)?),
            "connector-check" => Step::ConnectorCheck(val(v)?),
            "mock-provider" => Step::MockProvider(val(v)?),
            "edit-agent-draft" => Step::EditAgentDraft(val(v)?),
            "release-agent" => Step::ReleaseAgent(val(v)?),
            "git-tag" => Step::GitTag(val(v)?),
            "marketplace-add" => Step::MarketplaceAdd(val(v)?),
            "marketplace-refresh" => Step::MarketplaceRefresh(val(v)?),
            "marketplace-install" => Step::MarketplaceInstall(val(v)?),
            "marketplace-update" => Step::MarketplaceUpdate(val(v)?),
            "catalog-write" => Step::CatalogWrite(val(v)?),
            "login" => Step::Login(val(v)?),
            "change-password" => Step::ChangePassword(val(v)?),
            "identity-request" => Step::IdentityRequest(val(v)?),
            other => return Err(D::Error::custom(format!("unknown step `{other}`"))),
        })
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Latest {
    Latest,
}

/// The decision of an `approve` / `deny` step: `latest`, or the map form.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Decide {
    Latest(Latest),
    Opts(DecideOpts),
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideOpts {
    /// Also save the decision as a rule on the agent's draft (`remember`): "Always allow" on
    /// `approve`, "Always deny" on `deny`.
    #[serde(default)]
    pub always: bool,
    /// The step passes only when vamd answers 4xx and the approval stays pending.
    #[serde(default)]
    pub refused: bool,
    /// Alias of the `org` piece whose identity decides; the run's identity when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    /// `cli` or `app`: the surface the decision is sent from (the API with no surface header when omitted).
    pub via: Option<crate::engine::notifications::Via>,
    /// On `approve`: decide AMENDED with these argument values (`amendedArgs`).
    pub amend: Option<BTreeMap<String, serde_yaml::Value>>,
}

impl Decide {
    pub fn opts(&self) -> DecideOpts {
        match self {
            Decide::Latest(_) => DecideOpts::default(),
            Decide::Opts(o) => o.clone(),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reserved {
    pub agent: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThreadMode {
    #[default]
    New,
    Continue,
}

/// Which thread a chat goes to: a mode word (`new`, `continue`: the thread the latest run
/// used, sent by the ids the node reported), `{ id: latest }` (the same, with the id read from
/// the thread list), or `{ name: "label" }` (a new thread with that label, as written).
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ThreadSel {
    Mode(ThreadMode),
    ById(ThreadById),
    Label(ThreadLabel),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadLabel {
    pub name: String,
}

impl Default for ThreadSel {
    fn default() -> Self {
        ThreadSel::Mode(ThreadMode::New)
    }
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadById {
    pub id: Latest,
}

/// A chat message: a bare string, or the full form.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Chat {
    Text(String),
    Full(ChatFull),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatFull {
    pub text: String,
    /// Alias of the agent piece to talk to; the only agent when omitted.
    pub agent: Option<String>,
    #[serde(default)]
    pub thread: ThreadSel,
    pub budget: Option<String>,
    /// `false` returns once the run is created, without waiting for it to end or park.
    #[serde(default = "yes")]
    pub wait: bool,
    /// `true`: run the owner's draft of the agent (`agentRef.version` prerelease `draft`).
    /// `false`: pin the release the world piece made (1.0.0), so a draft the owner has since
    /// saved is not followed. Omitted: no version, which follows the owner's draft once one
    /// exists and the latest release before.
    pub draft: Option<bool>,
    /// Alias of the `org` piece whose identity runs it; the bootstrap key when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    /// Harness switches for this run, sent as the run's `inputs.harness`; none when omitted.
    pub harness: Option<serde_yaml::Value>,
    /// The `inputs.model` this run names. Omitted: the runner's model. `none`: no model at all,
    /// so the node picks one.
    pub model: Option<String>,
    /// `true`: a 4xx answer to the start is the expected outcome (`expect: { refused: true }`
    /// then checks it) instead of a failed step.
    #[serde(default)]
    pub refused: bool,
}

fn yes() -> bool {
    true
}

impl Chat {
    pub fn text(&self) -> &str {
        match self {
            Chat::Text(t) => t,
            Chat::Full(f) => &f.text,
        }
    }
    pub fn agent(&self) -> Option<&str> {
        match self {
            Chat::Text(_) => None,
            Chat::Full(f) => f.agent.as_deref(),
        }
    }
    /// Whether the step waits for the run to end or park (`wait: false` does not).
    pub fn wait(&self) -> bool {
        match self {
            Chat::Text(_) => true,
            Chat::Full(f) => f.wait,
        }
    }
    pub fn draft(&self) -> Option<bool> {
        match self {
            Chat::Text(_) => None,
            Chat::Full(f) => f.draft,
        }
    }
    pub fn identity(&self) -> Option<&str> {
        match self {
            Chat::Text(_) => None,
            Chat::Full(f) => f.as_.as_deref(),
        }
    }
    pub fn harness(&self) -> Option<&serde_yaml::Value> {
        match self {
            Chat::Text(_) => None,
            Chat::Full(f) => f.harness.as_ref(),
        }
    }
    pub fn model(&self) -> Option<&str> {
        match self {
            Chat::Text(_) => None,
            Chat::Full(f) => f.model.as_deref(),
        }
    }
    /// Whether a 4xx answer to the start is expected (checked by `expect: { refused }`).
    pub fn refused(&self) -> bool {
        matches!(self, Chat::Full(f) if f.refused)
    }
    /// The `e2e-step:<label>` label the message carries.
    pub fn label(&self) -> Option<String> {
        label_of(self.text())
    }
}

/// The label of the last `e2e-step:<label>` in `text` (the mock reads the same one).
pub fn label_of(text: &str) -> Option<String> {
    let marker = vam_inference::mock_script::STEP_MARKER;
    let rest = &text[text.rfind(marker)? + marker.len()..];
    let label: String = rest.chars().take_while(|c| !c.is_whitespace()).collect();
    (!label.is_empty()).then_some(label)
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideGate {
    pub option: String,
    pub gate: Option<String>,
    /// The step passes only when vamd answers 4xx and the gate stays pending.
    #[serde(default)]
    pub refused: bool,
    /// Alias of the `org` piece whose identity decides; the run's identity when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    /// As for `approve`: `cli` or `app`.
    pub via: Option<crate::engine::notifications::Via>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartPlaybook {
    /// Alias of the playbook piece.
    pub playbook: String,
    /// Alias of the agent piece; the only agent when omitted.
    pub agent: Option<String>,
    /// The `e2e-step` label the run's first message carries (the engine writes the marker).
    pub label: Option<String>,
    #[serde(default)]
    pub inputs: BTreeMap<String, serde_yaml::Value>,
    /// As for `chat`: `true` runs the owner's draft, `false` pins the release.
    pub draft: Option<bool>,
    /// Alias of the `org` piece whose identity runs it; the bootstrap key when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    /// As for `chat`: harness switches sent as the run's `inputs.harness`.
    pub harness: Option<serde_yaml::Value>,
}

/// An assertion. Exactly one family: `run`, `event`, `approval`, `gate`, `step`, `file`,
/// `thread` or `refused`. `step` is a family of its own (with `state`) unless `gate` is set,
/// where it picks the gate's step.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    pub run: Option<String>,
    pub event: Option<String>,
    /// With `event`: no matching event may appear. Holds once the run has ended or parked and
    /// settled; fails as soon as one appears.
    pub absent: Option<bool>,
    pub tool: Option<String>,
    pub fields: Option<BTreeMap<String, serde_yaml::Value>>,
    pub approval: Option<String>,
    /// `pending`: a gate of the run waits for a decision (`step` picks it, `fields` match its
    /// claims).
    pub gate: Option<String>,
    /// A playbook step id; pairs with `state` (or with `gate`).
    pub step: Option<String>,
    pub state: Option<String>,
    pub file: Option<FileExpect>,
    pub refused: Option<bool>,
    /// With `refused`: the refusal's answer body contains this text.
    pub contains: Option<String>,
    /// `node: { fields }`: `GET /v1/node` fields (dotted paths from the response body).
    pub node: Option<NodeExpect>,
    /// `plain-http: refused`: a plain-HTTP request to vamd's address gets no API answer.
    #[serde(rename = "plain-http")]
    pub plain_http: Option<String>,
    /// `vmm: jailed` (or `{ jailed: true, under: <dir> }`): the Firecracker process on the host.
    pub vmm: Option<VmmExpect>,
    pub thread: Option<ThreadExpect>,
    /// `receipt: { fields }`: the latest run's `GET /v1/runs/{id}/receipt` body.
    pub receipt: Option<ReceiptExpect>,
    /// `secret-absent: { secret }`: no form of the named `secret` piece's value is in anything
    /// the latest run left behind.
    #[serde(rename = "secret-absent")]
    pub secret_absent: Option<SecretAbsent>,
    /// `flood: { rateLimited, retryAfter, recovers }`: what the latest `flood` step saw.
    pub flood: Option<FloodExpect>,
    /// `grant: { tool, fields, count, absent }`: the user's grants (`GET /v1/keys?holder=broker`).
    pub grant: Option<Box<crate::engine::triggers::GrantExpect>>,
    /// `fire: { outcomes, reason, sameRun }`: what the latest `fire-trigger` step saw.
    pub fire: Option<Box<crate::engine::triggers::FireExpect>>,
    /// `spaces: { workspace, fields }`: the Space `GET /v1/spaces` lists for a workspace.
    pub spaces: Option<Box<crate::engine::triggers::SpacesExpect>>,
    /// `threads: { workspace, fields }`: the workspace's thread list entry for a thread.
    pub threads: Option<Box<crate::engine::triggers::ThreadsExpect>>,
    /// `runs: { count }`: how many runs the node lists for the scenario's agent.
    pub runs: Option<Box<crate::engine::triggers::RunsExpect>>,
    /// `ledger: { piece, tool, title?, count }`: lines in the effects test broker's own ledger.
    pub ledger: Option<Box<crate::engine::effects::LedgerExpect>>,
    /// `scheduled-runs: { count, within }`: runs the schedule broker started in this scenario.
    #[serde(rename = "scheduled-runs")]
    pub scheduled_runs: Option<Box<crate::engine::schedule::ScheduledRunsExpect>>,
    /// `notifications: { kind, count, run, unread, state, fields, absent, as, within }`: the user's list.
    pub notifications: Option<Box<crate::engine::notifications::NotificationsExpect>>,
    /// `notification-summary: { action, warn, unread, as, within }`: the badge counts.
    #[serde(rename = "notification-summary")]
    pub notification_summary: Option<Box<crate::engine::notifications::SummaryExpect>>,
    /// `channel-messages: { to, kind, count, buttons, openInVam, inputs, updated, mentions, absent, within }`.
    #[serde(rename = "channel-messages")]
    pub channel_messages: Option<Box<crate::engine::channels::ChannelMessagesExpect>>,
    /// `broker-state: { piece, lacks }`: the notification broker keeps no credential.
    #[serde(rename = "broker-state")]
    pub broker_state: Option<Box<crate::engine::channels::BrokerStateExpect>>,
    /// `connectors: { id, as, fields, absent }`: one connector in `GET /v1/connectors`.
    pub connectors: Option<Box<crate::engine::connectors::ConnectorsExpect>>,
    /// `mock-requests: { piece, path, has, lacks, count }`: what a mock provider saw.
    #[serde(rename = "mock-requests")]
    pub mock_requests: Option<Box<crate::engine::connectors::MockRequestsExpect>>,
    /// `agent-stats: { piece, version, k, fields }`: `GET /v1/agents/{urn}/outcomes`.
    #[serde(rename = "agent-stats")]
    pub agent_stats: Option<Box<crate::engine::outcomes::AgentStatsExpect>>,
    /// `release-diff: { piece, version, k, fields }`: `GET /v1/agents/{urn}/outcomes/diff`.
    #[serde(rename = "release-diff")]
    pub release_diff: Option<Box<crate::engine::outcomes::ReleaseDiffExpect>>,
    /// `marketplaces: { marketplace, as, fields, absent }`: one entry of `GET /v1/marketplaces`.
    pub marketplaces: Option<Box<crate::engine::marketplace::MarketplacesExpect>>,
    /// `listings: { marketplace, listing, as, fields, absent }`: one marketplace listing.
    pub listings: Option<Box<crate::engine::marketplace::ListingsExpect>>,
    /// `catalog: { kind, urnPrefix, as, count, fields }`: catalog rows under a URN prefix.
    pub catalog: Option<Box<crate::engine::marketplace::CatalogExpect>>,
}

impl Expect {
    /// How many families are set (`tool` belongs to `event`/`approval`, `fields` to `event`/`approval`/`gate`).
    pub fn families(&self) -> usize {
        [
            self.run.is_some(),
            self.event.is_some(),
            self.approval.is_some(),
            self.gate.is_some(),
            self.step.is_some() && self.gate.is_none(),
            self.file.is_some(),
            self.refused.is_some(),
            self.node.is_some(),
            self.plain_http.is_some(),
            self.vmm.is_some(),
            self.thread.is_some(),
            self.receipt.is_some(),
            self.secret_absent.is_some(),
            self.flood.is_some(),
            self.grant.is_some(),
            self.fire.is_some(),
            self.spaces.is_some(),
            self.threads.is_some(),
            self.runs.is_some(),
            self.scheduled_runs.is_some(),
            self.ledger.is_some(),
            self.notifications.is_some(),
            self.notification_summary.is_some(),
            self.channel_messages.is_some(),
            self.broker_state.is_some(),
            self.connectors.is_some(),
            self.mock_requests.is_some(),
            self.agent_stats.is_some(),
            self.release_diff.is_some(),
            self.marketplaces.is_some(),
            self.listings.is_some(),
            self.catalog.is_some(),
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }
}

/// A message of the latest run's thread, as the thread API lists it: some
/// message of `role` whose text contains `contains`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ThreadExpect {
    pub role: String,
    pub contains: String,
    /// The matched message's text is at most this many bytes.
    #[serde(rename = "maxBytes")]
    pub max_bytes: Option<usize>,
    /// `true`: no message of `role` contains the text (read once the run has ended).
    pub absent: Option<bool>,
}

/// Dotted camelCase paths over `{receipt, modelAppendix, signature}`; a value is exact,
/// `{ contains: text }`, `{ present: true }` or `{ absent: true }`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReceiptExpect {
    pub fields: BTreeMap<String, serde_yaml::Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeExpect {
    pub fields: BTreeMap<String, serde_yaml::Value>,
}

/// `vmm: jailed`, or the map form with the jail's base directory.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum VmmExpect {
    Word(String),
    Opts(VmmOpts),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VmmOpts {
    pub jailed: bool,
    pub under: Option<String>,
    /// `filter`: the process also runs under a seccomp filter (`Seccomp: 2` in its status).
    pub seccomp: Option<String>,
}

impl VmmExpect {
    /// The jail base directory the process root must sit under.
    pub fn under(&self) -> String {
        match self {
            VmmExpect::Opts(VmmOpts { under: Some(u), .. }) => u.clone(),
            _ => "/srv/jailer".to_string(),
        }
    }
    /// Whether the process must also run under a seccomp filter.
    pub fn wants_seccomp(&self) -> bool {
        matches!(self, VmmExpect::Opts(VmmOpts { seccomp: Some(s), .. }) if s == "filter")
    }
    pub fn is_jailed(&self) -> bool {
        match self {
            VmmExpect::Word(w) => w == "jailed",
            VmmExpect::Opts(o) => o.jailed,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileExpect {
    pub agent: Option<String>,
    pub path: String,
    pub present: Option<bool>,
    pub absent: Option<bool>,
    pub equals: Option<String>,
}

impl Scenario {
    /// All steps in order, `when` then `then`, with their `when.N` / `then.N` position.
    pub fn steps(&self) -> impl Iterator<Item = (String, &Step)> {
        let w = self
            .when
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("when.{i}"), s));
        let t = self
            .then
            .iter()
            .enumerate()
            .map(|(i, s)| (format!("then.{i}"), s));
        w.chain(t)
    }
}

// ---- secret-absent and flood -----------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretAbsent {
    /// Alias of a `secret` piece of `given`.
    pub secret: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetNode {
    pub allowed_models: Vec<String>,
    pub default_model: Option<String>,
    /// `vercel` or `openrouter`: where a slash model id the catalog does not know goes.
    pub default_gateway: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Flood {
    /// Alias of the `org` piece whose key sends the requests; the bootstrap key when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    pub path: String,
    pub count: u32,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FloodExpect {
    /// At least this many requests were answered 429.
    pub rate_limited: Option<AtLeast>,
    /// `present`: the 429 answers carry a `Retry-After` header.
    pub retry_after: Option<String>,
    /// `true`: after sleeping `Retry-After`, one request with the same key is answered 2xx.
    pub recovers: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AtLeast {
    pub at_least: u32,
}

#[cfg(test)]
mod harness_tests {
    use super::*;

    #[test]
    fn thread_is_a_mode_word_or_an_id_of_the_latest() {
        let thread = |yaml: &str| -> ThreadSel {
            let Step::Chat(Chat::Full(f)) = serde_yaml::from_str::<Step>(yaml).unwrap() else {
                panic!("a full chat")
            };
            f.thread
        };
        assert!(matches!(
            thread("chat: { text: hi }"),
            ThreadSel::Mode(ThreadMode::New)
        ));
        assert!(matches!(
            thread("chat: { text: hi, thread: continue }"),
            ThreadSel::Mode(ThreadMode::Continue)
        ));
        assert!(matches!(
            thread("chat: { text: hi, thread: { id: latest } }"),
            ThreadSel::ById(_)
        ));
        assert!(matches!(
            thread("chat: { text: hi, thread: { name: \"my notes\" } }"),
            ThreadSel::Label(l) if l.name == "my notes"
        ));
        for bad in ["thread: forever", "thread: { id: other }"] {
            assert!(serde_yaml::from_str::<Step>(&format!("chat: {{ text: hi, {bad} }}")).is_err());
        }
    }

    #[test]
    fn harness_parses_on_chat_and_start_playbook() {
        let c: Step =
            serde_yaml::from_str("chat: { text: hi, harness: { loop: { detect: true } } }")
                .unwrap();
        let Step::Chat(c) = c else { panic!("a chat") };
        assert!(c.harness().is_some());
        let p: Step = serde_yaml::from_str(
            "start-playbook: { playbook: p, harness: { context: { reciteTodo: true } } }",
        )
        .unwrap();
        let Step::StartPlaybook(p) = p else {
            panic!("a playbook start")
        };
        assert!(p.harness.is_some());
        let plain: Step = serde_yaml::from_str("chat: hi").unwrap();
        let Step::Chat(plain) = plain else {
            panic!("a chat")
        };
        assert!(plain.harness().is_none());
        assert!(serde_yaml::from_str::<Step>("chat: { text: hi, harnes: {} }").is_err());
    }
}
