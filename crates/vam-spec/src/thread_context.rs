//! The context a host hands a guest task by digest (AR-2.4/2.5).
//!
//! The host assembles it from ordered context sources (today only the thread
//! log), writes the bytes beside the session and grants their SHA-256 digest to
//! one run. The guest fetches the bytes over `FetchRunCheckpoint`, checks the
//! digest, rebuilds its own system prompt and tools, and places `blocks` and
//! `messages` itself. The payload never carries a system prompt, a thread id or
//! a workspace id.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const THREAD_CONTEXT_FORMAT: &str = "vam.thread-context";
pub const THREAD_CONTEXT_VERSION: u32 = 1;
/// Largest payload either side accepts.
pub const MAX_THREAD_CONTEXT_BYTES: usize = 16 * 1024 * 1024;
/// Task-input key naming the digest of the assembled context (set by the host).
pub const THREAD_HEAD_DIGEST_INPUT: &str = "thread_head_digest";
/// Task-input key with the log head seq of the assembled context, as a string.
pub const THREAD_HEAD_SEQ_INPUT: &str = "thread_head_seq";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A message in the plain chat wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<WireToolCall>,
    #[serde(default)]
    pub tool_call_id: String,
}

/// Where the guest puts a block.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockPlacement {
    /// Right after the system prompt, before the history.
    Stable,
    /// After the history, before the new user message.
    PerTask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockKind {
    History,
    UserMemory,
    AgentMemory,
    Knowledge,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockTrust {
    Trusted,
    /// Text from a broker. The host has already fenced it.
    Untrusted,
}

/// Context text injected by a source other than the history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextBlock {
    pub source: String,
    pub kind: BlockKind,
    pub placement: BlockPlacement,
    pub trust: BlockTrust,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadContext {
    pub format: String,
    pub version: u32,
    pub run_id: String,
    pub head_seq: u64,
    pub head_digest: String,
    #[serde(default)]
    pub system_sha256: Option<String>,
    #[serde(default)]
    pub tools_sha256: Option<String>,
    pub messages: Vec<WireMessage>,
    #[serde(default)]
    pub blocks: Vec<ContextBlock>,
    /// Set when the task resumes a suspended tool call (approval resume).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suspend: Option<SuspendResume>,
    /// Set when the task resumes after a guest crash or a `vamd` restart (E10 Phase 4).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crash: Option<CrashResume>,
}

/// Schema of the suspend tail a guest uploads.
pub const SUSPEND_TAIL_SCHEMA: u32 = 3;
/// Longest `last_answer_candidate` carried in the loop state.
pub const MAX_LAST_ANSWER_BYTES: usize = 256 * 1024;

/// The tool call a suspend waits on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingCall {
    pub tool_call_id: String,
    pub tool_name: String,
    pub args_json: String,
}

/// The guest's loop state at a suspend (it is not conversation).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct LoopState {
    #[serde(default)]
    pub loaded_skill_ids: Vec<String>,
    #[serde(default)]
    pub iterations_used: usize,
    #[serde(default)]
    pub max_iterations: usize,
    #[serde(default)]
    pub prompt_tokens: i32,
    #[serde(default)]
    pub completion_tokens: i32,
    #[serde(default)]
    pub cached_tokens: i32,
    #[serde(default)]
    pub total_tokens: i32,
    #[serde(default)]
    pub nudge_count: usize,
    #[serde(default)]
    pub skill_tool_called: bool,
    #[serde(default)]
    pub claim_reprompted: bool,
    #[serde(default)]
    pub last_answer_candidate: String,
    /// Last inbox seq applied to the conversation (AR-3.6). A resume asks the
    /// host for messages after it. Absent in older tails and omitted at 0.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub inbox_seq: i64,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

/// What the guest sends at an approval suspend: only what the host cannot see,
/// the results of calls that ran after the last model request, plus loop state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspendTail {
    pub schema_version: u32,
    pub run_id: String,
    pub lifecycle_generation: i64,
    pub kind: String,
    /// Role `tool` messages only.
    pub messages: Vec<WireMessage>,
    pub pending: PendingCall,
    pub state: LoopState,
}

/// The suspend as the host serves it back on resume.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SuspendResume {
    pub lifecycle_generation: i64,
    pub pending: PendingCall,
    /// Calls of the suspended message with no result yet, computed by the host.
    pub unanswered: Vec<String>,
    pub state: LoopState,
}

/// A task picked up again after its guest died or `vamd` restarted. The conversation in
/// [`ThreadContext::messages`] ends where the host last recorded it: with the model's decided
/// round (an assistant message whose calls were not all answered) or with the last result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrashResume {
    /// `guest_crashed` or `vamd_restarted`.
    pub reason: String,
    /// How many times this task has been picked up, this one included.
    pub resume: u32,
    /// Calls of the last assistant message that have no result yet, in the order the model made
    /// them, computed by the host. Empty when the log ends with a result (the next step is a
    /// model request).
    pub unanswered: Vec<String>,
    /// Loop counters derived by the host (rounds from the log, spend from metering, inbox seq
    /// from what was logged). `loaded_skill_ids` is empty: the guest re-derives it from the
    /// history.
    pub state: LoopState,
}

impl ThreadContext {
    /// The message list a task starts from, after the system prompt: stable
    /// blocks, the history, per-task blocks. A block becomes a user message.
    pub fn placed_messages(&self) -> Vec<WireMessage> {
        let block_msg = |b: &ContextBlock| WireMessage {
            role: "user".to_string(),
            content: b.text.clone(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        };
        let mut out: Vec<WireMessage> = self
            .blocks
            .iter()
            .filter(|b| b.placement == BlockPlacement::Stable)
            .map(block_msg)
            .collect();
        out.extend(self.messages.iter().cloned());
        out.extend(
            self.blocks
                .iter()
                .filter(|b| b.placement == BlockPlacement::PerTask)
                .map(block_msg),
        );
        out
    }

    /// Parses and checks bytes whose SHA-256 must equal `expected_digest`.
    pub fn from_verified_bytes(bytes: &[u8], expected_digest: &str) -> Result<Self, String> {
        if bytes.len() > MAX_THREAD_CONTEXT_BYTES {
            return Err("thread context is over the size guard".into());
        }
        if sha256_hex(bytes) != expected_digest {
            return Err("thread context digest mismatch".into());
        }
        let ctx: ThreadContext =
            serde_json::from_slice(bytes).map_err(|e| format!("thread context invalid: {e}"))?;
        if ctx.format != THREAD_CONTEXT_FORMAT {
            return Err("not a thread context".into());
        }
        if ctx.version != THREAD_CONTEXT_VERSION {
            return Err(format!(
                "thread context version {} unsupported",
                ctx.version
            ));
        }
        Ok(ctx)
    }
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Compact JSON with object keys sorted.
pub fn canonical_json(value: &Value) -> String {
    match value {
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical_json).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| {
                    let key = serde_json::to_string(k).unwrap_or_default();
                    format!(
                        "{key}:{}",
                        map.get(k).map(canonical_json).unwrap_or_default()
                    )
                })
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

/// Digest of a turn's tool-result tail: the messages a round appends to the
/// conversation after its assistant message. The guest sends it with
/// `CommitTurn`; the host takes the same digest over the matching tail of the
/// next model request. Both sides use this one function.
pub fn tail_digest(tail: &[WireMessage]) -> String {
    let values: Vec<Value> = tail
        .iter()
        .map(|m| {
            let calls: Vec<Value> = m
                .tool_calls
                .iter()
                .map(|c| serde_json::json!({"id": c.id, "name": c.name, "arguments": c.arguments}))
                .collect();
            serde_json::json!({
                "role": m.role,
                "content": m.content,
                "tool_calls": calls,
                "tool_call_id": m.tool_call_id,
            })
        })
        .collect();
    sha256_hex(canonical_json(&Value::Array(values)).as_bytes())
}

/// The trailing run of tool results right after the last assistant message of
/// a request's messages (system message excluded by the caller). The run ends
/// at the first message that is not a tool result; an empty result means the
/// request carries no tail.
pub fn trailing_tool_results(messages: &[WireMessage]) -> &[WireMessage] {
    let Some(last_assistant) = messages.iter().rposition(|m| m.role == "assistant") else {
        return &[];
    };
    let after = &messages[last_assistant + 1..];
    let run = after.iter().take_while(|m| m.role == "tool").count();
    &after[..run]
}

/// Whether a tool may write files in the workspace (open question 3). A round
/// that ran one takes a turn commit; the host uses the same list to decide
/// which results a stop rolls back. Unknown names count as writers.
pub fn tool_may_write_files(name: &str) -> bool {
    !matches!(
        name,
        "read_file" | "glob" | "grep" | "load_skill" | "task_complete" | "post_message" | "show"
    )
}

/// Canonical bytes of tool definitions, in the request's order. The host
/// recorder and the guest both hash this, so the logged digest is comparable.
pub fn tools_blob(tools: &[crate::v1::ToolSpec]) -> Option<String> {
    let mut values = Vec::with_capacity(tools.len());
    for tool in tools {
        let json = crate::encoding::proto_to_json("vam.v1.ToolSpec", tool).ok()?;
        values.push(serde_json::from_str::<Value>(&json).ok()?);
    }
    Some(canonical_json(&Value::Array(values)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> WireMessage {
        WireMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    fn ctx() -> ThreadContext {
        ThreadContext {
            format: THREAD_CONTEXT_FORMAT.into(),
            version: THREAD_CONTEXT_VERSION,
            run_id: "r".into(),
            head_seq: 3,
            head_digest: "d".into(),
            system_sha256: None,
            tools_sha256: None,
            messages: vec![msg("user", "hi"), msg("assistant", "yo")],
            blocks: vec![],
            suspend: None,
            crash: None,
        }
    }

    fn tool_msg(id: &str, text: &str) -> WireMessage {
        WireMessage {
            role: "tool".into(),
            content: text.into(),
            tool_calls: vec![],
            tool_call_id: id.into(),
        }
    }

    #[test]
    fn tail_digest_is_stable_and_sensitive() {
        let a = tail_digest(&[tool_msg("c1", "ok")]);
        assert_eq!(a, tail_digest(&[tool_msg("c1", "ok")]));
        assert_ne!(a, tail_digest(&[tool_msg("c1", "ok!")]));
        assert_ne!(a, tail_digest(&[tool_msg("c2", "ok")]));
        assert_ne!(a, tail_digest(&[]));
    }

    #[test]
    fn the_trailing_run_stops_at_the_first_non_tool_message() {
        let msgs = vec![
            msg("user", "go"),
            msg("assistant", "calling"),
            tool_msg("a", "r"),
            tool_msg("b", "r"),
            msg("user", "nudge"),
        ];
        assert_eq!(trailing_tool_results(&msgs).len(), 2);
        let msgs = vec![msg("user", "x"), msg("assistant", "y")];
        assert!(trailing_tool_results(&msgs).is_empty());
        assert!(trailing_tool_results(&[]).is_empty());
    }

    #[test]
    fn read_only_tools_do_not_write_and_unknown_tools_do() {
        for name in [
            "read_file",
            "glob",
            "grep",
            "load_skill",
            "task_complete",
            "post_message",
            "show",
        ] {
            assert!(!tool_may_write_files(name), "{name}");
        }
        for name in [
            "write_file",
            "edit_file",
            "delete_file",
            "run",
            "todo_write",
            "x",
        ] {
            assert!(tool_may_write_files(name), "{name}");
        }
    }

    #[test]
    fn blocks_are_placed_by_placement() {
        let mut c = ctx();
        let block = |p, t: &str| ContextBlock {
            source: "fake".into(),
            kind: BlockKind::UserMemory,
            placement: p,
            trust: BlockTrust::Untrusted,
            text: t.into(),
        };
        c.blocks = vec![
            block(BlockPlacement::PerTask, "late"),
            block(BlockPlacement::Stable, "early"),
        ];
        let texts: Vec<String> = c.placed_messages().into_iter().map(|m| m.content).collect();
        assert_eq!(texts, ["early", "hi", "yo", "late"]);
    }

    #[test]
    fn verified_bytes_round_trip_and_reject_tampering() {
        let c = ctx();
        let bytes = serde_json::to_vec(&c).unwrap();
        let digest = sha256_hex(&bytes);
        assert_eq!(
            ThreadContext::from_verified_bytes(&bytes, &digest).unwrap(),
            c
        );
        assert!(ThreadContext::from_verified_bytes(&bytes, &"0".repeat(64)).is_err());
    }

    #[test]
    fn unknown_fields_and_missing_blocks_are_tolerated() {
        let json = r#"{"format":"vam.thread-context","version":1,"run_id":"r","head_seq":1,
            "head_digest":"d","messages":[],"future":true}"#;
        let bytes = json.as_bytes();
        let c = ThreadContext::from_verified_bytes(bytes, &sha256_hex(bytes)).unwrap();
        assert!(c.blocks.is_empty());
    }

    #[test]
    fn canonical_json_sorts_keys() {
        let v: Value = serde_json::from_str(r#"{"b":1,"a":[{"d":1,"c":2}]}"#).unwrap();
        assert_eq!(canonical_json(&v), r#"{"a":[{"c":2,"d":1}],"b":1}"#);
    }
}
