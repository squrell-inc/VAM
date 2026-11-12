//! Scripted mock model for the E2E journeys (test only; see `e2e/README.md`).
//!
//! A script is one JSON file per journey. Its `id` must equal the file stem:
//!
//! ```json
//! { "id": "approval-resume",
//!   "rules": [
//!     { "when": {"step": "start", "stepStart": true},
//!       "reply": {"toolCall": {"name": "create_record", "args": {"title": "e2e"}}} },
//!     { "when": {"lastRole": "tool", "toolName": "create_record"},
//!       "reply": {"text": "Created the record."} },
//!     { "when": {"lastRole": "user"},
//!       "reply": {"text": "Turn answer.", "fillerBytes": 13000} } ] }
//! ```
//!
//! **Selection.** A request uses script `<id>` when a `system` message contains the marker
//! `mock-script:<id>`. When no system message has the marker, any message may carry it. A request
//! without a marker is not scripted: the content fixtures in `provider.rs` answer it as before.
//!
//! **Matching.** Stateless: the reply is a pure function of the request's messages and offered
//! tools, so it survives guest restarts, HITL resume and the playbook-finish loop. The first rule
//! whose `when` holds decides the reply. Conditions are ANDed, and an empty `when` always holds.
//! - `lastRole`: `user` or `tool`, the role of the last message.
//! - `contains`: a substring of the last message's content.
//! - `anyContains`: a substring of the `content` of any non-system message in the request. Tool-call
//!   arguments are not searched. Use it to see an earlier turn's tool result or summary.
//! - `step`: the latest user message that carries `e2e-step:<label>` has this label. When that message
//!   carries several markers (a flattened replay of earlier turns), the last one is its label. A scenario
//!   writes the label into its own message; messages the guest adds (reprompts, HITL notes, the
//!   empty-reply nudge) never carry one, so they cannot shift the script. Without a labelled
//!   message a rule that sets `step` does not hold.
//! - `stepStart`: `true` holds iff the last message is the latest labelled message, so no tool
//!   result or guest note has followed it yet. It never holds without a labelled message.
//! - `model`: the request is for this catalog model id (the model the broker resolved for the
//!   request). It tells two models of one script apart, for a failover journey.
//! - `toolName`: the last message is a tool result whose `tool_call_id` is the id of an assistant
//!   tool call with this name. Only messages after the labelled message are looked at.
//! - `afterTool` / `notAfterTool`: a tool result from this tool does (or does not) appear among the
//!   messages after the labelled message and before the last one. Without a labelled message the
//!   scope is the whole conversation.
//!
//! **Reply.** `text`, `toolCall {name, args}`, or both (text first). Or `fail: {status}`, alone: the
//! provider answers with that HTTP status and no reply at all (the mock has no retries of its own,
//! so the status is what a real provider shows after its retries ran out). `fillerBytes` appends exactly
//! that many bytes of deterministic ASCII to the text. `delayMs` streams the text in four pieces with
//! `delayMs / 4` between them. A tool call id is `mock_<script>_<rule index>_<step>_<n>`, with the
//! rule index counted from zero, `<step>` the active label (`nostep` when none) and `<n>` the number
//! of assistant tool calls already in the request, so repeated runs of one script produce the same
//! ids and no two calls of a thread share one.
//!
//! **Trace.** Each scripted request reports one `MockTrace` (rule matched, or why nothing matched)
//! to the callback set with `MockProvider::with_trace`; vamd turns it into a run event.
//!
//! **Failures** are error chunks that name the script, so a run shows them in its events: a tool the
//! request does not offer, a request that matches no rule, or a marker naming a script that is not
//! loaded.
//!
//! **Names and usage.** `MockProvider` receives the guest's canonical tool names. Wire aliasing
//! (`provider_wire_tool_name`) is applied only by the OpenAI and Anthropic adapters. Usage is
//! estimated: prompt tokens are message content bytes divided by 4, completion tokens are reply
//! bytes divided by 4.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use vam_spec::v1::{ChatMessage, ToolCall, ToolSpec};

/// The marker a request carries to select a script.
pub const MARKER: &str = "mock-script:";
/// The marker a scenario writes into its own user message to name the step.
pub const STEP_MARKER: &str = "e2e-step:";
/// The largest `fillerBytes` a reply may ask for.
pub const MAX_FILLER_BYTES: usize = 1024 * 1024;
/// The largest text delta in one chunk. A vsock frame carries at most 64 KiB.
pub const MAX_DELTA_BYTES: usize = 16 * 1024;
/// The number of pieces a `delayMs` reply is streamed in.
pub const DELAY_PIECES: usize = 4;
/// How much of the last message a no-match error quotes.
const PREVIEW_CHARS: usize = 80;

/// One journey's script, as written in its JSON file.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockScript {
    pub id: String,
    pub rules: Vec<MockRule>,
}

/// A condition and the reply it selects.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockRule {
    #[serde(default)]
    pub when: MockCondition,
    pub reply: MockReply,
}

/// The conditions of a rule. Every set field must hold.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockCondition {
    pub last_role: Option<String>,
    pub contains: Option<String>,
    /// A substring of the `content` of any non-system message (tool-call arguments excluded).
    pub any_contains: Option<String>,
    pub step: Option<String>,
    pub step_start: Option<bool>,
    /// The request is for this model id (see the module notes).
    pub model: Option<String>,
    pub tool_name: Option<String>,
    pub after_tool: Option<String>,
    pub not_after_tool: Option<String>,
    /// The request is labelled a resample (`ContextHint.reason = "resample"`).
    pub resampled: Option<bool>,
    /// An offered tool's description contains a text.
    pub tool_described: Option<MockToolDescribed>,
}

/// `toolDescribed`: the offered tool named `tool` has a description containing `contains`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockToolDescribed {
    pub tool: String,
    pub contains: String,
}

/// What a matched rule answers with.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockReply {
    /// The provider fails with this HTTP status (400 to 599) and sends nothing else.
    pub fail: Option<MockFail>,
    pub text: Option<String>,
    pub tool_call: Option<MockToolCall>,
    pub filler_bytes: Option<usize>,
    pub delay_ms: Option<u64>,
    /// Reasoning the model returns before its answer. The mock carries it in the plan; nothing
    /// streams it until the inference chunk has a reasoning field.
    pub reasoning: Option<String>,
}

/// `fail`: the HTTP status the provider answers with.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockFail {
    pub status: u16,
}

/// A tool call the reply makes. `args` must be a JSON object. `rawArgs` instead sends a string
/// verbatim as the call's arguments (it need not be JSON), for scenarios about malformed calls;
/// setting both is refused.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct MockToolCall {
    pub name: String,
    #[serde(default = "empty_object")]
    pub args: serde_json::Value,
    pub raw_args: Option<String>,
}

fn empty_object() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Every script in a directory, keyed by id.
#[derive(Debug, Clone, Default)]
pub struct MockScripts {
    by_id: BTreeMap<String, MockScript>,
}

/// Why a script directory or a script was rejected.
#[derive(Debug, thiserror::Error)]
pub enum MockScriptError {
    #[error("cannot read mock script directory {path}: {source}")]
    Directory {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("cannot read mock script {file}: {source}")]
    Read {
        file: String,
        #[source]
        source: std::io::Error,
    },
    #[error("mock script {file} is not a valid script: {reason}")]
    Parse { file: String, reason: String },
    #[error("mock script {file} declares id {id}; the id must be the file name")]
    IdMismatch { file: String, id: String },
    #[error("mock script id {id} must use only letters, digits, '-' and '_'")]
    InvalidId { id: String },
    #[error("mock script {id} is defined more than once")]
    DuplicateId { id: String },
    #[error("mock script {id} has no rules")]
    NoRules { id: String },
    #[error("mock script {id} rule {index}: {reason}")]
    InvalidRule {
        id: String,
        index: usize,
        reason: String,
    },
}

/// Why a request got no scripted reply. The text names the script for the run's events.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MockMatchError {
    #[error("mock script {id}: not loaded")]
    NotLoaded { id: String },
    #[error("mock script {id}: tool {name} not offered (offered: {offered})")]
    ToolNotOffered {
        id: String,
        name: String,
        offered: String,
    },
    #[error("mock script {id}: no rule matched (last role {role}, content \"{preview}\")")]
    NoRuleMatched {
        id: String,
        role: String,
        preview: String,
    },
}

impl MockMatchError {
    /// The script the error names.
    pub fn script(&self) -> &str {
        match self {
            Self::NotLoaded { id }
            | Self::ToolNotOffered { id, .. }
            | Self::NoRuleMatched { id, .. } => id,
        }
    }

    /// A stable word for the trace event.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::NotLoaded { .. } => "not_loaded",
            Self::ToolNotOffered { .. } => "tool_not_offered",
            Self::NoRuleMatched { .. } => "no_rule",
        }
    }
}

/// What one scripted request resolved to, for the run's event stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MockTrace {
    RuleMatched {
        script: String,
        rule: usize,
        step: Option<String>,
    },
    Unmatched {
        script: String,
        reason: &'static str,
        last_role: String,
        /// At most `PREVIEW_CHARS` characters of the last message, control characters blanked.
        preview: String,
        step: Option<String>,
    },
}

/// Receives one `MockTrace` per scripted request.
pub type MockTraceCallback = std::sync::Arc<dyn Fn(&MockTrace) + Send + Sync>;

/// Resolves a request to its plan or error, and the trace that describes the outcome.
pub fn plan_with_trace(
    script: &MockScript,
    tools: &[ToolSpec],
    messages: &[ChatMessage],
    resampled: bool,
) -> (Result<MockPlan, MockMatchError>, MockTrace) {
    plan_with_trace_for(script, tools, messages, resampled, None)
}

/// As [`plan_with_trace`], for a request that is for `model` (what a `model` condition reads).
pub fn plan_with_trace_for(
    script: &MockScript,
    tools: &[ToolSpec],
    messages: &[ChatMessage],
    resampled: bool,
    model: Option<&str>,
) -> (Result<MockPlan, MockMatchError>, MockTrace) {
    let planned = plan_reply_for(script, tools, messages, resampled, model);
    let trace = match &planned {
        Ok(plan) => MockTrace::RuleMatched {
            script: script.id.clone(),
            rule: plan.rule_index,
            step: plan.step.clone(),
        },
        Err(err) => unmatched_trace(err, messages),
    };
    (planned, trace)
}

/// The trace for a request that got no scripted reply.
pub fn unmatched_trace(err: &MockMatchError, messages: &[ChatMessage]) -> MockTrace {
    let last = messages.last();
    MockTrace::Unmatched {
        script: err.script().to_string(),
        reason: err.reason(),
        last_role: last.map(|m| m.role.clone()).unwrap_or_default(),
        preview: preview(last.map_or("", |m| m.content.as_str())),
        step: History::new(messages).step.map(str::to_string),
    }
}

/// The reply a matched rule produces, ready to stream.
#[derive(Debug, Clone, PartialEq)]
pub struct MockPlan {
    /// The index of the rule that matched, counted from zero.
    pub rule_index: usize,
    /// The active step label, when the request carries one.
    pub step: Option<String>,
    /// The full reply text, filler included.
    pub text: String,
    pub tool_call: Option<ToolCall>,
    pub delay_ms: u64,
    /// The provider fails with this HTTP status instead of replying.
    pub fail_status: Option<u16>,
    /// The reply's reasoning, when it has one. Not streamed yet.
    pub reasoning: Option<String>,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
}

impl MockScripts {
    /// Loads every `*.json` file in `dir`. A file's `id` must equal its file stem.
    pub fn load_dir(dir: &Path) -> Result<Self, MockScriptError> {
        let shown = dir.display().to_string();
        let entries = std::fs::read_dir(dir).map_err(|source| MockScriptError::Directory {
            path: shown.clone(),
            source,
        })?;
        let mut files = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| MockScriptError::Directory {
                path: shown.clone(),
                source,
            })?;
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) == Some("json") {
                files.push(path);
            }
        }
        files.sort();
        let scripts = files
            .iter()
            .map(|path| read_script(path))
            .collect::<Result<Vec<_>, _>>()?;
        Self::from_scripts(scripts)
    }

    /// Builds the registry from parsed scripts, checking each one and rejecting duplicate ids.
    pub fn from_scripts(scripts: Vec<MockScript>) -> Result<Self, MockScriptError> {
        let mut by_id = BTreeMap::new();
        for script in scripts {
            validate_script(&script)?;
            if by_id.contains_key(&script.id) {
                return Err(MockScriptError::DuplicateId { id: script.id });
            }
            by_id.insert(script.id.clone(), script);
        }
        Ok(Self { by_id })
    }

    pub fn get(&self, id: &str) -> Option<&MockScript> {
        self.by_id.get(id)
    }

    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.by_id.keys().map(String::as_str)
    }
}

fn read_script(path: &Path) -> Result<MockScript, MockScriptError> {
    let file = path.display().to_string();
    let text = std::fs::read_to_string(path).map_err(|source| MockScriptError::Read {
        file: file.clone(),
        source,
    })?;
    let script: MockScript = serde_json::from_str(&text).map_err(|err| MockScriptError::Parse {
        file: file.clone(),
        reason: err.to_string(),
    })?;
    let stem = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default();
    if script.id != stem {
        return Err(MockScriptError::IdMismatch {
            file,
            id: script.id,
        });
    }
    Ok(script)
}

fn validate_script(script: &MockScript) -> Result<(), MockScriptError> {
    if !is_valid_id(&script.id) {
        return Err(MockScriptError::InvalidId {
            id: script.id.clone(),
        });
    }
    if script.rules.is_empty() {
        return Err(MockScriptError::NoRules {
            id: script.id.clone(),
        });
    }
    for (index, rule) in script.rules.iter().enumerate() {
        validate_rule(rule).map_err(|reason| MockScriptError::InvalidRule {
            id: script.id.clone(),
            index,
            reason,
        })?;
    }
    Ok(())
}

fn validate_rule(rule: &MockRule) -> Result<(), String> {
    let when = &rule.when;
    if let Some(role) = &when.last_role {
        if role != "user" && role != "tool" {
            return Err(format!(
                "lastRole must be \"user\" or \"tool\", not \"{role}\""
            ));
        }
    }
    if let Some(step) = &when.step {
        if step.is_empty() || !step.chars().all(is_id_char) {
            return Err("step must be a label of letters, digits, '-' and '_'".to_string());
        }
    }
    for name in [&when.tool_name, &when.after_tool, &when.not_after_tool]
        .into_iter()
        .flatten()
    {
        if name.is_empty() {
            return Err("tool names in conditions must not be empty".to_string());
        }
    }
    if let Some(d) = &when.tool_described {
        if d.tool.is_empty() || d.contains.is_empty() {
            return Err("toolDescribed needs a tool and a non-empty contains".to_string());
        }
    }
    let reply = &rule.reply;
    if let Some(model) = &when.model {
        if model.trim().is_empty() {
            return Err("model must not be empty".to_string());
        }
    }
    if let Some(fail) = &reply.fail {
        if !(400..=599).contains(&fail.status) {
            return Err(format!(
                "fail.status must be an HTTP error status (400 to 599), not {}",
                fail.status
            ));
        }
        if reply.text.is_some()
            || reply.tool_call.is_some()
            || reply.filler_bytes.is_some()
            || reply.reasoning.is_some()
            || reply.delay_ms.is_some()
        {
            return Err(
                "fail is the whole reply; it cannot be combined with other keys".to_string(),
            );
        }
        return Ok(());
    }
    if reply.reasoning.as_deref() == Some("") {
        return Err("reasoning must not be empty".to_string());
    }
    if reply.text.is_none() && reply.tool_call.is_none() && reply.filler_bytes.is_none() {
        return Err("reply sets no text, toolCall or fillerBytes".to_string());
    }
    if let Some(call) = &reply.tool_call {
        if call.name.is_empty() {
            return Err("toolCall.name must not be empty".to_string());
        }
        if !call.args.is_object() {
            return Err("toolCall.args must be a JSON object".to_string());
        }
        if call.raw_args.is_some() && call.args.as_object().is_some_and(|o| !o.is_empty()) {
            return Err("toolCall sets both args and rawArgs".to_string());
        }
    }
    if let Some(bytes) = reply.filler_bytes {
        if bytes > MAX_FILLER_BYTES {
            return Err(format!(
                "fillerBytes {bytes} is over the limit of {MAX_FILLER_BYTES}"
            ));
        }
    }
    Ok(())
}

fn is_valid_id(id: &str) -> bool {
    !id.is_empty() && id.chars().all(is_id_char)
}

fn is_id_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}

/// The script a request asks for: the first `mock-script:<id>` in a system message, else in any
/// message.
pub fn requested_script(messages: &[ChatMessage]) -> Option<String> {
    messages
        .iter()
        .filter(|message| message.role == "system")
        .chain(messages.iter())
        .find_map(|message| marker_id(&message.content))
}

fn marker_id(content: &str) -> Option<String> {
    let (_, rest) = content.split_once(MARKER)?;
    let id: String = rest.chars().take_while(|c| is_id_char(*c)).collect();
    (!id.is_empty()).then_some(id)
}

/// Chooses the reply for a request: the first rule whose `when` holds, built from its `reply`.
pub fn plan_reply(
    script: &MockScript,
    tools: &[ToolSpec],
    messages: &[ChatMessage],
) -> Result<MockPlan, MockMatchError> {
    plan_reply_labelled(script, tools, messages, false)
}

/// As [`plan_reply`], for a request that is (`resampled`) or is not labelled a resample.
pub fn plan_reply_labelled(
    script: &MockScript,
    tools: &[ToolSpec],
    messages: &[ChatMessage],
    resampled: bool,
) -> Result<MockPlan, MockMatchError> {
    plan_reply_for(script, tools, messages, resampled, None)
}

/// As [`plan_reply_labelled`], for a request that is for `model`.
pub fn plan_reply_for(
    script: &MockScript,
    tools: &[ToolSpec],
    messages: &[ChatMessage],
    resampled: bool,
    model: Option<&str>,
) -> Result<MockPlan, MockMatchError> {
    let mut history = History::new(messages);
    history.resampled = resampled;
    history.model = model;
    let last = messages.last();
    for (index, rule) in script.rules.iter().enumerate() {
        if condition_holds(&rule.when, &history, messages, tools, last) {
            return build_plan(&script.id, index, &rule.reply, tools, &history, messages);
        }
    }
    Err(MockMatchError::NoRuleMatched {
        id: script.id.clone(),
        role: last.map(|message| message.role.clone()).unwrap_or_default(),
        preview: preview(last.map_or("", |message| message.content.as_str())),
    })
}

/// What the conditions read from a request, computed once.
struct History<'a> {
    /// The label of the latest labelled user message.
    step: Option<&'a str>,
    /// The last message is the latest labelled message.
    at_step_start: bool,
    /// Assistant tool calls in the whole request.
    tool_calls: usize,
    /// The tool that produced the last message, when the last message is a tool result.
    last_tool: Option<&'a str>,
    /// The tools whose results appear after the labelled message and before the last message.
    earlier_tools: HashSet<&'a str>,
    /// The request carries the resample label.
    resampled: bool,
    /// The model the request is for, when the caller knows it.
    model: Option<&'a str>,
}

impl<'a> History<'a> {
    fn new(messages: &'a [ChatMessage]) -> Self {
        let mut call_names: HashMap<&'a str, &'a str> = HashMap::new();
        let mut tool_calls = 0;
        for message in messages {
            for call in &message.tool_calls {
                call_names.insert(call.id.as_str(), call.name.as_str());
                tool_calls += 1;
            }
        }
        let labelled = messages.iter().enumerate().rev().find_map(|(index, m)| {
            (m.role == "user")
                .then(|| step_label(&m.content))
                .flatten()
                .map(|label| (index, label))
        });
        let scope_start = labelled.map_or(0, |(index, _)| index + 1);
        let last = messages.last();
        let earlier = messages
            .get(scope_start..messages.len().saturating_sub(1))
            .unwrap_or_default();
        let earlier_tools = earlier
            .iter()
            .filter(|message| message.role == "tool")
            .filter_map(|message| call_names.get(message.tool_call_id.as_str()).copied())
            .collect();
        let last_tool = last
            .filter(|message| message.role == "tool")
            .and_then(|message| call_names.get(message.tool_call_id.as_str()).copied());
        Self {
            step: labelled.map(|(_, label)| label),
            at_step_start: labelled.is_some_and(|(index, _)| index + 1 == messages.len()),
            tool_calls,
            last_tool,
            earlier_tools,
            resampled: false,
            model: None,
        }
    }
}

/// The label after the last `e2e-step:` in a message, if any. The last one, because the
/// orchestrator folds earlier turns into the new user message ("Previous conversation ...").
fn step_label(content: &str) -> Option<&str> {
    let (_, rest) = content.rsplit_once(STEP_MARKER)?;
    let end = rest.find(|c: char| !is_id_char(c)).unwrap_or(rest.len());
    (end > 0).then(|| &rest[..end])
}

fn condition_holds(
    when: &MockCondition,
    history: &History<'_>,
    messages: &[ChatMessage],
    tools: &[ToolSpec],
    last: Option<&ChatMessage>,
) -> bool {
    let Some(last) = last else {
        return false;
    };
    when.last_role
        .as_deref()
        .is_none_or(|role| last.role == role)
        && when
            .contains
            .as_deref()
            .is_none_or(|needle| last.content.contains(needle))
        && when.any_contains.as_deref().is_none_or(|needle| {
            messages
                .iter()
                .any(|m| m.role != "system" && m.content.contains(needle))
        })
        && when
            .step
            .as_deref()
            .is_none_or(|label| history.step == Some(label))
        && when
            .step_start
            .is_none_or(|wanted| history.step.is_some() && history.at_step_start == wanted)
        && when
            .tool_name
            .as_deref()
            .is_none_or(|name| history.last_tool == Some(name))
        && when
            .after_tool
            .as_deref()
            .is_none_or(|name| history.earlier_tools.contains(name))
        && when
            .not_after_tool
            .as_deref()
            .is_none_or(|name| !history.earlier_tools.contains(name))
        && when
            .model
            .as_deref()
            .is_none_or(|wanted| history.model == Some(wanted))
        && when
            .resampled
            .is_none_or(|wanted| history.resampled == wanted)
        && when.tool_described.as_ref().is_none_or(|want| {
            tools
                .iter()
                .any(|t| t.name == want.tool && t.description.contains(&want.contains))
        })
}

fn build_plan(
    id: &str,
    index: usize,
    reply: &MockReply,
    tools: &[ToolSpec],
    history: &History<'_>,
    messages: &[ChatMessage],
) -> Result<MockPlan, MockMatchError> {
    if let Some(fail) = &reply.fail {
        return Ok(MockPlan {
            rule_index: index,
            step: history.step.map(str::to_string),
            text: String::new(),
            tool_call: None,
            delay_ms: 0,
            fail_status: Some(fail.status),
            reasoning: None,
            prompt_tokens: 0,
            completion_tokens: 0,
        });
    }
    let mut text = reply.text.clone().unwrap_or_default();
    text.push_str(&filler(reply.filler_bytes.unwrap_or(0)));
    let tool_call = match &reply.tool_call {
        None => None,
        Some(call) => {
            if !tools.iter().any(|tool| tool.name == call.name) {
                return Err(MockMatchError::ToolNotOffered {
                    id: id.to_string(),
                    name: call.name.clone(),
                    offered: offered(tools),
                });
            }
            Some(ToolCall {
                id: format!(
                    "mock_{id}_{index}_{}_{}",
                    history.step.unwrap_or("nostep"),
                    history.tool_calls
                ),
                name: call.name.clone(),
                arguments: match &call.raw_args {
                    Some(raw) => raw.clone(),
                    None => serde_json::to_string(&expand_last(&call.args, messages))
                        .unwrap_or_else(|_| "{}".to_string()),
                },
            })
        }
    };
    let prompt_bytes: usize = messages
        .iter()
        .map(|message| {
            message.content.len()
                + message
                    .tool_calls
                    .iter()
                    .map(|call| call.arguments.len())
                    .sum::<usize>()
        })
        .sum();
    let reply_bytes = text.len() + tool_call.as_ref().map_or(0, |call| call.arguments.len());
    Ok(MockPlan {
        rule_index: index,
        step: history.step.map(str::to_string),
        text,
        tool_call,
        delay_ms: reply.delay_ms.unwrap_or(0),
        fail_status: None,
        reasoning: reply.reasoning.clone(),
        prompt_tokens: tokens(prompt_bytes),
        completion_tokens: tokens(reply_bytes),
    })
}

/// Replaces `{{last:<prefix>}}` in the string values of a tool call's args with the newest word
/// that starts with `<prefix>` in the request's non-system messages (user text and tool
/// results, newest message first). A script loads at start and cannot hold an id a run makes
/// later; this lets a scripted call pass an id the model was just given. Mock only. With no
/// such word the placeholder stays as it is.
fn expand_last(args: &serde_json::Value, messages: &[ChatMessage]) -> serde_json::Value {
    use serde_json::Value;
    match args {
        Value::String(s) if s.contains("{{last:") => Value::String(expand_last_in(s, messages)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| expand_last(v, messages)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), expand_last(v, messages)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn expand_last_in(text: &str, messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{last:") {
        let after = &rest[start + "{{last:".len()..];
        let Some(end) = after.find("}}") else { break };
        let prefix = &after[..end];
        out.push_str(&rest[..start]);
        match newest_word(prefix, messages) {
            Some(word) => out.push_str(&word),
            None => out.push_str(&rest[start..start + "{{last:".len() + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// The last word starting with `prefix` in the newest message that has one.
fn newest_word(prefix: &str, messages: &[ChatMessage]) -> Option<String> {
    if prefix.is_empty() {
        return None;
    }
    messages
        .iter()
        .rev()
        .filter(|m| m.role != "system")
        .find_map(|m| {
            m.content
                .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'))
                .rfind(|w| w.starts_with(prefix))
                .map(str::to_string)
        })
}

fn tokens(bytes: usize) -> i32 {
    i32::try_from(bytes / 4).unwrap_or(i32::MAX)
}

fn offered(tools: &[ToolSpec]) -> String {
    if tools.is_empty() {
        "none".to_string()
    } else {
        tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

fn preview(content: &str) -> String {
    content
        .chars()
        .take(PREVIEW_CHARS)
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect()
}

/// Exactly `bytes` bytes of deterministic ASCII: `filler 000000 filler 000001 ...` cut at `bytes`.
pub fn filler(bytes: usize) -> String {
    let mut out = String::with_capacity(bytes);
    let mut block = 0usize;
    while out.len() < bytes {
        out.push_str(&format!("filler {block:06} "));
        block += 1;
    }
    out.truncate(bytes);
    out
}

/// Splits reply text into the deltas a provider streams. Each piece fits one vsock frame, and a
/// delayed reply is cut into at least `DELAY_PIECES` pieces. Pieces start and end on char boundaries.
pub fn stream_pieces(text: &str, delayed: bool) -> Vec<&str> {
    let by_size = text.len().div_ceil(MAX_DELTA_BYTES);
    let parts = if delayed {
        by_size.max(DELAY_PIECES)
    } else {
        by_size.max(1)
    };
    let mut pieces = Vec::with_capacity(parts);
    let mut start = 0;
    for part in 1..=parts {
        let mut end = if part == parts {
            text.len()
        } else {
            text.len() * part / parts
        };
        while end < text.len() && !text.is_char_boundary(end) {
            end += 1;
        }
        if end > start {
            pieces.push(&text[start..end]);
            start = end;
        }
    }
    pieces
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    fn assistant_call(call_id: &str, name: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".to_string(),
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: call_id.to_string(),
                name: name.to_string(),
                arguments: "{}".to_string(),
            }],
            tool_call_id: String::new(),
        }
    }

    fn tool_result(call_id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "tool".to_string(),
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: call_id.to_string(),
        }
    }

    fn tools(names: &[&str]) -> Vec<ToolSpec> {
        names
            .iter()
            .map(|name| ToolSpec {
                name: name.to_string(),
                ..Default::default()
            })
            .collect()
    }

    fn script(json: &str) -> MockScript {
        serde_json::from_str(json).expect("script parses")
    }

    fn text_of(script: &MockScript, messages: &[ChatMessage]) -> String {
        plan_reply(script, &tools(&[]), messages)
            .expect("a rule matches")
            .text
    }

    #[test]
    fn last_role_and_contains_look_at_the_last_message() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {"lastRole": "tool", "contains": "ok"}, "reply": {"text": "matched"}}]}"#,
        );
        let hit = [message("user", "go"), tool_result("x", "ok")];
        assert_eq!(text_of(&s, &hit), "matched");
        let wrong_content = [message("user", "go"), tool_result("x", "bad")];
        assert!(plan_reply(&s, &tools(&[]), &wrong_content).is_err());
        let wrong_role = [message("tool", "ok"), message("user", "go")];
        assert!(plan_reply(&s, &tools(&[]), &wrong_role).is_err());
    }

    const STEP_SCRIPT: &str = r#"{"id": "s", "rules": [
        {"when": {"step": "start", "stepStart": true}, "reply": {"text": "first"}},
        {"when": {"step": "start", "lastRole": "user"}, "reply": {"text": "start-note"}},
        {"when": {"step": "start", "toolName": "t"}, "reply": {"text": "start-tool"}},
        {"when": {"step": "next", "stepStart": true}, "reply": {"text": "second"}},
        {"when": {"step": "next", "notAfterTool": "t", "toolName": "t"}, "reply": {"text": "never"}},
        {"when": {"step": "next", "toolName": "t"}, "reply": {"text": "next-tool"}}]}"#;

    #[test]
    fn step_label_is_the_last_marker_of_a_folded_history_message() {
        let s = script(STEP_SCRIPT);
        let folded = [message(
            "user",
            "Previous conversation (most recent last):\nUser: e2e-step:start hi\nAssistant: ok\n\ne2e-step:next go",
        )];
        assert_eq!(text_of(&s, &folded), "second");
    }

    #[test]
    fn step_label_is_read_from_user_messages_only() {
        let s = script(STEP_SCRIPT);
        let labelled = [
            message("system", "mock-script:s e2e-step:next"),
            message("user", "please go e2e-step:start."),
        ];
        assert_eq!(text_of(&s, &labelled), "first");
        let latest = [
            message("user", "e2e-step:start"),
            message("assistant", "ok"),
            message("user", "e2e-step:next"),
        ];
        assert_eq!(text_of(&s, &latest), "second");
        // A marker in a tool result or assistant message is not a label.
        let unlabelled = [
            message("user", "hello"),
            message("assistant", "e2e-step:start"),
        ];
        assert!(plan_reply(&s, &tools(&[]), &unlabelled).is_err());
    }

    #[test]
    fn guest_messages_do_not_shift_the_script() {
        let s = script(STEP_SCRIPT);
        for note in [
            "Your last response was empty. Continue the assigned task now.",
            "The task is not marked complete. Call task_complete.",
            "The approval was granted; continue.",
            "The approval was denied; finish up.",
        ] {
            let messages = [
                message("user", "go e2e-step:start"),
                message("assistant", "text"),
                message("user", note),
            ];
            let plan = plan_reply(&s, &tools(&[]), &messages).expect("matches");
            // stepStart no longer holds, and the answer does not depend on how many notes came.
            assert_eq!(plan.text, "start-note", "{note}");
            let twice = [
                messages[0].clone(),
                messages[1].clone(),
                messages[2].clone(),
                message("assistant", "more"),
                message("user", note),
            ];
            assert_eq!(text_of(&s, &twice), "start-note");
        }
    }

    #[test]
    fn last_placeholder_takes_the_newest_word_with_the_prefix() {
        let messages = [
            message("user", "old sch_aaa11 text"),
            message("tool", "{\"scheduleId\":\"sch_bbb22\"}"),
            message("user", "delete it"),
        ];
        let args = serde_json::json!({"id": "{{last:sch_}}", "n": ["x-{{last:sch_}}"], "k": 3});
        let got = expand_last(&args, &messages);
        assert_eq!(got["id"], "sch_bbb22");
        assert_eq!(got["n"][0], "x-sch_bbb22");
        assert_eq!(got["k"], 3);
        // No such word: the placeholder is left as it is.
        let none = expand_last(&serde_json::json!({"id": "{{last:rg_}}"}), &messages);
        assert_eq!(none["id"], "{{last:rg_}}");
        // System text is not read.
        let sys = [message("system", "sch_zzz"), message("user", "go")];
        let kept = expand_last(&serde_json::json!({"id": "{{last:sch_}}"}), &sys);
        assert_eq!(kept["id"], "{{last:sch_}}");
    }

    #[test]
    fn any_contains_reads_any_non_system_message_but_not_call_args() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {"anyContains": "kiwi"}, "reply": {"text": "seen"}},
                {"when": {}, "reply": {"text": "unseen"}}]}"#,
        );
        let result = [
            message("user", "go"),
            assistant_call("c1", "t"),
            tool_result("c1", "kiwi-42"),
            message("user", "again"),
        ];
        assert_eq!(text_of(&s, &result), "seen");
        assert_eq!(text_of(&s, &[message("user", "go")]), "unseen");
        let system = [message("system", "kiwi"), message("user", "go")];
        assert_eq!(text_of(&s, &system), "unseen");
        let mut call = assistant_call("c1", "t");
        call.tool_calls[0].arguments = "{\"v\":\"kiwi\"}".to_string();
        assert_eq!(text_of(&s, &[call, message("user", "go")]), "unseen");
    }

    #[test]
    fn the_last_marker_of_a_flattened_message_is_its_label() {
        let s = script(STEP_SCRIPT);
        let flattened = [message(
            "user",
            "User: e2e-step:start a\nAssistant: x\n\nUser: e2e-step:next b",
        )];
        assert_eq!(text_of(&s, &flattened), "second");
        assert_eq!(step_label("e2e-step:only"), Some("only"));
    }

    #[test]
    fn step_start_never_holds_without_a_labelled_message() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {"stepStart": true}, "reply": {"text": "start"}},
                {"when": {"stepStart": false}, "reply": {"text": "not-start"}}]}"#,
        );
        assert!(plan_reply(&s, &tools(&[]), &[message("user", "hi")]).is_err());
        assert_eq!(text_of(&s, &[message("user", "e2e-step:a")]), "start");
    }

    #[test]
    fn a_second_labelled_message_starts_clean() {
        let s = script(STEP_SCRIPT);
        let thread = [
            message("user", "e2e-step:start"),
            assistant_call("c1", "t"),
            tool_result("c1", "{}"),
            message("assistant", "done"),
            message("user", "e2e-step:next"),
            assistant_call("c2", "t"),
            tool_result("c2", "{}"),
        ];
        // The earlier result of `t` is before the second label, so `notAfterTool` still holds.
        assert_eq!(text_of(&s, &thread[..3]), "start-tool");
        let plan = plan_reply(&s, &tools(&[]), &thread).expect("matches");
        assert_eq!(plan.rule_index, 4);
        assert_eq!(plan.step.as_deref(), Some("next"));
        let after = script(
            r#"{"id": "s", "rules": [
                {"when": {"step": "next", "afterTool": "t"}, "reply": {"text": "leak"}},
                {"when": {"step": "next", "stepStart": true}, "reply": {"text": "clean"}}]}"#,
        );
        assert_eq!(text_of(&after, &thread[..5]), "clean");
    }

    #[test]
    fn unlabelled_requests_keep_whole_conversation_scope() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {"afterTool": "t"}, "reply": {"text": "after"}},
                {"when": {}, "reply": {"text": "plain"}}]}"#,
        );
        let messages = [
            message("user", "go"),
            assistant_call("c1", "t"),
            tool_result("c1", "{}"),
            message("user", "next"),
        ];
        assert_eq!(text_of(&s, &messages), "after");
    }

    #[test]
    fn a_script_that_still_uses_user_turn_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "old.json",
            r#"{"id": "old", "rules": [{"when": {"userTurn": 1}, "reply": {"text": "x"}}]}"#,
        );
        let err = MockScripts::load_dir(dir.path()).expect_err("userTurn is retired");
        let text = err.to_string();
        assert!(
            text.contains("old.json") && text.contains("userTurn"),
            "{text}"
        );
    }

    #[test]
    fn traces_name_the_rule_or_the_reason() {
        let s = script(STEP_SCRIPT);
        let (planned, trace) =
            plan_with_trace(&s, &tools(&[]), &[message("user", "e2e-step:next")], false);
        assert!(planned.is_ok());
        assert_eq!(
            trace,
            MockTrace::RuleMatched {
                script: "s".to_string(),
                rule: 3,
                step: Some("next".to_string())
            }
        );
        let (planned, trace) = plan_with_trace(&s, &tools(&[]), &[message("user", "x\ny")], false);
        assert!(planned.is_err());
        assert_eq!(
            trace,
            MockTrace::Unmatched {
                script: "s".to_string(),
                reason: "no_rule",
                last_role: "user".to_string(),
                preview: "x y".to_string(),
                step: None
            }
        );
    }

    #[test]
    fn tool_name_matches_the_call_that_produced_the_result() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {"toolName": "create_record"}, "reply": {"text": "created"}}]}"#,
        );
        let made = [
            message("user", "go"),
            assistant_call("c1", "create_record"),
            tool_result("c1", "{}"),
        ];
        assert_eq!(text_of(&s, &made), "created");
        let other = [
            message("user", "go"),
            assistant_call("c1", "list_records"),
            tool_result("c1", "{}"),
        ];
        assert!(plan_reply(&s, &tools(&[]), &other).is_err());
        let not_last = [
            message("user", "go"),
            assistant_call("c1", "create_record"),
            tool_result("c1", "{}"),
            message("user", "next"),
        ];
        assert!(plan_reply(&s, &tools(&[]), &not_last).is_err());
    }

    #[test]
    fn after_tool_looks_only_at_earlier_results() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {"afterTool": "create_record"}, "reply": {"text": "after"}},
                {"when": {"notAfterTool": "create_record"}, "reply": {"text": "before"}}]}"#,
        );
        let later_turn = [
            message("user", "go"),
            assistant_call("c1", "create_record"),
            tool_result("c1", "{}"),
            message("user", "next"),
        ];
        assert_eq!(text_of(&s, &later_turn), "after");
        let fresh = [message("user", "start")];
        assert_eq!(text_of(&s, &fresh), "before");
        // The last message is the result itself, so it is not an earlier result.
        let just_called = [
            message("user", "go"),
            assistant_call("c1", "create_record"),
            tool_result("c1", "{}"),
        ];
        let plan = plan_reply(&s, &tools(&[]), &just_called).expect("a rule matches");
        assert_eq!(plan.rule_index, 1);
    }

    #[test]
    fn first_matching_rule_wins() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {"lastRole": "user"}, "reply": {"text": "first"}},
                {"when": {}, "reply": {"text": "second"}}]}"#,
        );
        let plan = plan_reply(&s, &tools(&[]), &[message("user", "go")]).expect("matches");
        assert_eq!(plan.rule_index, 0);
        assert_eq!(plan.text, "first");
    }

    #[test]
    fn empty_when_always_holds() {
        let s = script(r#"{"id": "s", "rules": [{"reply": {"text": "any"}}]}"#);
        assert_eq!(text_of(&s, &[message("user", "go")]), "any");
    }

    #[test]
    fn filler_has_exact_size_and_is_deterministic() {
        for size in [0usize, 1, 13_000, 65_536] {
            let first = filler(size);
            assert_eq!(first.len(), size, "filler of {size} bytes");
            assert_eq!(first, filler(size), "filler is deterministic");
            assert!(first.is_ascii());
        }
        assert_eq!(filler(20), "filler 000000 filler");
        let s = script(
            r#"{"id": "s", "rules": [{"when": {}, "reply": {"text": "t", "fillerBytes": 13000}}]}"#,
        );
        let plan = plan_reply(&s, &tools(&[]), &[message("user", "go")]).expect("matches");
        assert_eq!(plan.text.len(), 13_001);
        assert!(plan.text.starts_with("tfiller 000000 "));
    }

    #[test]
    fn tool_call_ids_are_deterministic_and_args_are_compact_json() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {"step": "c", "stepStart": true},
                 "reply": {"toolCall": {"name": "echo", "args": {"query": "hello"}}}}]}"#,
        );
        let messages = [
            message("user", "a"),
            assistant_call("x1", "echo"),
            tool_result("x1", "{}"),
            message("user", "e2e-step:c"),
        ];
        let first = plan_reply(&s, &tools(&["echo"]), &messages).expect("matches");
        let again = plan_reply(&s, &tools(&["echo"]), &messages).expect("matches");
        assert_eq!(first, again);
        let call = first.tool_call.expect("a tool call");
        assert_eq!(call.id, "mock_s_0_c_1");
        assert_eq!(call.name, "echo");
        assert_eq!(call.arguments, r#"{"query":"hello"}"#);
    }

    #[test]
    fn raw_args_are_sent_verbatim() {
        let s = script(
            r#"{"id": "s", "rules": [
                {"when": {}, "reply": {"toolCall": {"name": "echo", "rawArgs": "{\"query\": "}}}]}"#,
        );
        let plan = plan_reply(&s, &tools(&["echo"]), &[message("user", "go")]).expect("matches");
        assert_eq!(
            plan.tool_call.expect("a tool call").arguments,
            r#"{"query": "#
        );
    }

    #[test]
    fn raw_args_with_args_is_refused() {
        let json = r#"{"id": "s", "rules": [
            {"when": {}, "reply": {"toolCall": {"name": "echo", "args": {"a": 1}, "rawArgs": "x"}}}]}"#;
        let parsed: MockScript = serde_json::from_str(json).expect("parses");
        let err = crate::MockScripts::from_scripts(vec![parsed]).expect_err("both set");
        assert!(err.to_string().contains("rawArgs"), "{err}");
    }

    #[test]
    fn tool_not_offered_is_an_error_that_names_the_script() {
        let s = script(
            r#"{"id": "s", "rules": [{"when": {}, "reply": {"toolCall": {"name": "echo"}}}]}"#,
        );
        let err = plan_reply(&s, &tools(&["search"]), &[message("user", "go")])
            .expect_err("echo is not offered");
        assert_eq!(
            err.to_string(),
            "mock script s: tool echo not offered (offered: search)"
        );
        let none =
            plan_reply(&s, &tools(&[]), &[message("user", "go")]).expect_err("nothing is offered");
        assert_eq!(
            none.to_string(),
            "mock script s: tool echo not offered (offered: none)"
        );
    }

    #[test]
    fn no_rule_matched_names_the_script_and_the_last_message() {
        let s = script(
            r#"{"id": "s", "rules": [{"when": {"lastRole": "tool"}, "reply": {"text": "x"}}]}"#,
        );
        let err = plan_reply(&s, &tools(&[]), &[message("user", "hello\nthere")])
            .expect_err("no rule matches");
        assert_eq!(
            err.to_string(),
            "mock script s: no rule matched (last role user, content \"hello there\")"
        );
    }

    #[test]
    fn requested_script_prefers_system_messages() {
        let both = [
            message("user", "mock-script:b"),
            message("system", "rules mock-script:a"),
        ];
        assert_eq!(requested_script(&both).as_deref(), Some("a"));
        let user_only = [message("user", "mock-script:b")];
        assert_eq!(requested_script(&user_only).as_deref(), Some("b"));
        let none = [message("system", "no marker"), message("user", "hi")];
        assert_eq!(requested_script(&none), None);
        let empty_id = [message("system", "mock-script: next")];
        assert_eq!(requested_script(&empty_id), None);
    }

    #[test]
    fn stream_pieces_fit_a_frame_and_keep_the_text() {
        let long = "é".repeat(20_000);
        let pieces = stream_pieces(&long, false);
        assert!(pieces
            .iter()
            .all(|piece| piece.len() <= MAX_DELTA_BYTES + 3));
        assert_eq!(pieces.concat(), long);
        assert_eq!(stream_pieces("abcd", true), vec!["a", "b", "c", "d"]);
        assert_eq!(stream_pieces("", true), Vec::<&str>::new());
        assert_eq!(stream_pieces("abc", false), vec!["abc"]);
    }

    fn write(dir: &Path, name: &str, body: &str) {
        std::fs::write(dir.join(name), body).expect("write script file");
    }

    const ONE_RULE: &str = r#"{"id": "NAME", "rules": [{"when": {}, "reply": {"text": "ok"}}]}"#;

    #[test]
    fn loader_reads_scripts_named_by_their_file_stem() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "approval.json",
            &ONE_RULE.replace("NAME", "approval"),
        );
        write(dir.path(), "notes.txt", "not a script");
        let scripts = MockScripts::load_dir(dir.path()).expect("loads");
        assert!(scripts.get("approval").is_some());
        assert_eq!(scripts.ids().collect::<Vec<_>>(), vec!["approval"]);
    }

    #[test]
    fn loader_rejects_an_id_that_is_not_the_file_name() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(dir.path(), "a.json", &ONE_RULE.replace("NAME", "b"));
        let err = MockScripts::load_dir(dir.path()).expect_err("id differs from stem");
        assert!(matches!(err, MockScriptError::IdMismatch { .. }), "{err}");
    }

    #[test]
    fn loader_rejects_unknown_fields_and_bad_json() {
        let dir = tempfile::tempdir().expect("tempdir");
        write(
            dir.path(),
            "a.json",
            r#"{"id": "a", "rules": [{"when": {}, "reply": {"text": "x"}}], "extra": 1}"#,
        );
        let err = MockScripts::load_dir(dir.path()).expect_err("unknown field");
        assert!(err.to_string().contains("unknown field"), "{err}");
        std::fs::remove_file(dir.path().join("a.json")).expect("remove");
        write(dir.path(), "b.json", "{");
        let err = MockScripts::load_dir(dir.path()).expect_err("bad json");
        assert!(matches!(err, MockScriptError::Parse { .. }), "{err}");
    }

    #[test]
    fn loader_rejects_a_missing_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let err = MockScripts::load_dir(&dir.path().join("absent")).expect_err("no directory");
        assert!(matches!(err, MockScriptError::Directory { .. }), "{err}");
    }

    #[test]
    fn registry_rejects_duplicate_ids_and_empty_rules() {
        let one = script(&ONE_RULE.replace("NAME", "same"));
        let err = MockScripts::from_scripts(vec![one.clone(), one]).expect_err("duplicate");
        assert!(matches!(err, MockScriptError::DuplicateId { .. }), "{err}");
        let empty = script(r#"{"id": "empty", "rules": []}"#);
        let err = MockScripts::from_scripts(vec![empty]).expect_err("no rules");
        assert!(matches!(err, MockScriptError::NoRules { .. }), "{err}");
    }

    #[test]
    fn registry_rejects_bad_rules_with_their_index() {
        let cases = [
            (
                r#"{"when": {"lastRole": "system"}, "reply": {"text": "x"}}"#,
                "lastRole",
            ),
            (
                r#"{"when": {"step": ""}, "reply": {"text": "x"}}"#,
                "step must be a label",
            ),
            (r#"{"reply": {}}"#, "no text, toolCall or fillerBytes"),
            (
                r#"{"reply": {"toolCall": {"name": "", "args": {}}}}"#,
                "must not be empty",
            ),
            (
                r#"{"reply": {"toolCall": {"name": "t", "args": [1]}}}"#,
                "JSON object",
            ),
            (
                r#"{"reply": {"text": "x", "fillerBytes": 2000000}}"#,
                "over the limit",
            ),
        ];
        for (rule, expected) in cases {
            let json = format!(r#"{{"id": "bad", "rules": [{rule}]}}"#);
            let err = MockScripts::from_scripts(vec![script(&json)]).expect_err(rule);
            match err {
                MockScriptError::InvalidRule { index, reason, .. } => {
                    assert_eq!(index, 0);
                    assert!(reason.contains(expected), "{reason} vs {expected}");
                }
                other => panic!("expected InvalidRule for {rule}, got {other}"),
            }
        }
    }

    #[test]
    fn registry_rejects_ids_that_the_marker_cannot_carry() {
        let bad = script(&ONE_RULE.replace("NAME", "has space"));
        let err = MockScripts::from_scripts(vec![bad]).expect_err("invalid id");
        assert!(matches!(err, MockScriptError::InvalidId { .. }), "{err}");
    }

    #[test]
    fn reasoning_rides_in_the_plan_and_changes_nothing_else() {
        let ok = script(
            r#"{"id":"r","rules":[{"reply":{"reasoning":"why","toolCall":{"name":"read_file"}}}]}"#,
        );
        let plan = plan_reply(&ok, &tools(&["read_file"]), &[message("user", "hi")]).unwrap();
        assert_eq!(plan.reasoning.as_deref(), Some("why"));
        assert!(plan.tool_call.is_some());
        assert_eq!(plan.text, "");
        let bad = script(r#"{"id":"r","rules":[{"reply":{"reasoning":"","text":"x"}}]}"#);
        assert!(MockScripts::from_scripts(vec![bad]).is_err());
    }

    #[test]
    fn tool_described_reads_the_offered_description() {
        let s = script(
            r#"{"id":"d","rules":[
              {"when":{"toolDescribed":{"tool":"read_file","contains":"Example:"}},"reply":{"text":"has"}},
              {"reply":{"text":"none"}}]}"#,
        );
        let mut offered = tools(&["read_file"]);
        let msgs = [message("user", "hi")];
        assert_eq!(plan_reply(&s, &offered, &msgs).unwrap().text, "none");
        offered[0].description = "Reads a file. Example: {}".to_string();
        assert_eq!(plan_reply(&s, &offered, &msgs).unwrap().text, "has");
    }

    #[test]
    fn resampled_follows_the_request_label() {
        let s = script(
            r#"{"id":"s","rules":[
              {"when":{"resampled":true},"reply":{"text":"resample"}},
              {"when":{"resampled":false},"reply":{"text":"first"}}]}"#,
        );
        let msgs = [message("user", "hi")];
        assert_eq!(plan_reply(&s, &[], &msgs).unwrap().text, "first");
        assert_eq!(
            plan_reply_labelled(&s, &[], &msgs, false).unwrap().text,
            "first"
        );
        assert_eq!(
            plan_reply_labelled(&s, &[], &msgs, true).unwrap().text,
            "resample"
        );
    }

    #[test]
    fn a_model_condition_reads_the_model_the_request_is_for() {
        let s = script(
            r#"{"id":"m","rules":[
              {"when":{"model":"first"},"reply":{"text":"one"}},
              {"when":{"model":"second"},"reply":{"text":"two"}}]}"#,
        );
        let msgs = [message("user", "hi")];
        let text = |model: Option<&str>| {
            plan_reply_for(&s, &[], &msgs, false, model).map(|plan| plan.text)
        };
        assert_eq!(text(Some("first")).unwrap(), "one");
        assert_eq!(text(Some("second")).unwrap(), "two");
        assert!(text(Some("third")).is_err());
        // A caller that does not know the model never satisfies the condition.
        assert!(text(None).is_err());
        assert!(plan_reply(&s, &[], &msgs).is_err());
    }

    #[test]
    fn a_failing_reply_carries_the_status_and_nothing_else() {
        let s = script(
            r#"{"id":"f","rules":[
              {"when":{"model":"down"},"reply":{"fail":{"status":503}}},
              {"reply":{"text":"fine"}}]}"#,
        );
        MockScripts::from_scripts(vec![s.clone()]).expect("valid");
        let msgs = [message("user", "hi")];
        let down = plan_reply_for(&s, &[], &msgs, false, Some("down")).unwrap();
        assert_eq!(down.fail_status, Some(503));
        assert!(down.text.is_empty() && down.tool_call.is_none());
        let up = plan_reply_for(&s, &[], &msgs, false, Some("up")).unwrap();
        assert_eq!((up.fail_status, up.text.as_str()), (None, "fine"));
    }

    #[test]
    fn a_failing_reply_must_be_an_error_status_and_the_whole_reply() {
        for bad in [
            r#"{"fail":{"status":200}}"#,
            r#"{"fail":{"status":302}}"#,
            r#"{"fail":{"status":600}}"#,
            r#"{"fail":{"status":503},"text":"x"}"#,
        ] {
            let s = script(&format!(r#"{{"id":"b","rules":[{{"reply":{bad}}}]}}"#));
            assert!(MockScripts::from_scripts(vec![s]).is_err(), "{bad}");
        }
        let empty = script(r#"{"id":"b","rules":[{"when":{"model":" "},"reply":{"text":"x"}}]}"#);
        assert!(MockScripts::from_scripts(vec![empty]).is_err());
        // Unknown keys stay refused.
        assert!(serde_json::from_str::<MockScript>(
            r#"{"id":"b","rules":[{"reply":{"fail":{"status":503,"after":2}}}]}"#
        )
        .is_err());
    }
}
