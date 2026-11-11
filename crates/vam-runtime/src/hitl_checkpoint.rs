//! Guest-side HITL continuation.
//!
//! A tool-level suspend uploads a [`vam_spec::thread_context::SuspendTail`]: only
//! the results of calls that ran after the last model request, plus loop state.
//! The host records the conversation itself (the thread log, decision 23),
//! validates the tail, and on resume serves the assembled context, which
//! [`continuation_from_context`] turns back into a [`HitlContinuation`]. The
//! guest writes no conversation file in the workspace.
//!
//! Schema 1/2 [`HitlContinuation`] checkpoints (full conversation, no system
//! prompt) are still read for runs parked before the switch, and are the
//! fallback upload when the host refuses a tail.
//!
//! The PEP still re-evaluates Cedar on every tool call; a tampered history
//! grants no new authority.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vam_spec::v1::{ChatMessage, ToolCall};

pub use crate::pep::{CHECKPOINT_CHUNK_BYTES, MAX_CHECKPOINT_BYTES};

/// Checkpoint schema written by this runtime. `messages` excludes the system prompt.
pub const SCHEMA_VERSION: u32 = 2;
/// Schema 1 kept the system prompt at `messages[0]`. It is still accepted.
pub const SCHEMA_VERSION_V1: u32 = 1;

const SIBLING_PLACEHOLDER: &str = "This sibling tool call was not executed because another tool \
     in the same turn required human approval. Continue the task with any remaining tools you \
     still need.";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerdeToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SerdeChatMessage {
    pub role: String,
    pub content: String,
    #[serde(default)]
    pub tool_calls: Vec<SerdeToolCall>,
    #[serde(default)]
    pub tool_call_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HitlContinuation {
    pub schema_version: u32,
    pub run_id: String,
    pub lifecycle_generation: i64,
    /// The conversation without the system prompt (schema 2).
    pub messages: Vec<SerdeChatMessage>,
    pub loaded_skill_ids: Vec<String>,
    pub iterations_used: usize,
    pub max_iterations: usize,
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub cached_tokens: i32,
    pub total_tokens: i32,
    pub nudge_count: usize,
    /// Retired same-tool heuristic field. Kept so existing checkpoints deserialize.
    #[serde(default)]
    pub last_tool_name: String,
    /// Retired same-tool heuristic field. Kept so existing checkpoints deserialize.
    #[serde(default)]
    pub same_tool_rounds: usize,
    pub skill_tool_called: bool,
    pub last_answer_candidate: String,
    pub pending_tool_call_id: String,
    pub pending_tool_name: String,
    pub pending_tool_args_json: String,
    pub unanswered_tool_call_ids: Vec<String>,
    #[serde(default)]
    pub claim_reprompted: bool,
    /// Last inbox seq applied before the suspend (AR-3.6).
    #[serde(default)]
    pub inbox_seq: i64,
}

impl From<&ToolCall> for SerdeToolCall {
    fn from(value: &ToolCall) -> Self {
        Self {
            id: value.id.clone(),
            name: value.name.clone(),
            arguments: value.arguments.clone(),
        }
    }
}

impl From<SerdeToolCall> for ToolCall {
    fn from(value: SerdeToolCall) -> Self {
        Self {
            id: value.id,
            name: value.name,
            arguments: value.arguments,
        }
    }
}

impl From<&ChatMessage> for SerdeChatMessage {
    fn from(value: &ChatMessage) -> Self {
        Self {
            role: value.role.clone(),
            content: value.content.clone(),
            tool_calls: value.tool_calls.iter().map(SerdeToolCall::from).collect(),
            tool_call_id: value.tool_call_id.clone(),
        }
    }
}

impl From<SerdeChatMessage> for ChatMessage {
    fn from(value: SerdeChatMessage) -> Self {
        Self {
            role: value.role,
            content: value.content,
            tool_calls: value.tool_calls.into_iter().map(ToolCall::from).collect(),
            tool_call_id: value.tool_call_id,
        }
    }
}

/// The continuation a resume restores, from the host-assembled context of a
/// suspended run. `None` when the context carries no suspend.
pub fn continuation_from_context(
    ctx: &vam_spec::thread_context::ThreadContext,
) -> Option<HitlContinuation> {
    let suspend = ctx.suspend.as_ref()?;
    let state = &suspend.state;
    Some(HitlContinuation {
        schema_version: SCHEMA_VERSION,
        run_id: ctx.run_id.clone(),
        lifecycle_generation: suspend.lifecycle_generation,
        messages: ctx
            .placed_messages()
            .iter()
            .map(|m| SerdeChatMessage::from(&crate::context_window::wire_to_chat(m)))
            .collect(),
        loaded_skill_ids: state.loaded_skill_ids.clone(),
        iterations_used: state.iterations_used,
        max_iterations: state.max_iterations,
        prompt_tokens: state.prompt_tokens,
        completion_tokens: state.completion_tokens,
        cached_tokens: state.cached_tokens,
        total_tokens: state.total_tokens,
        nudge_count: state.nudge_count,
        last_tool_name: String::new(),
        same_tool_rounds: 0,
        skill_tool_called: state.skill_tool_called,
        last_answer_candidate: state.last_answer_candidate.clone(),
        pending_tool_call_id: suspend.pending.tool_call_id.clone(),
        pending_tool_name: suspend.pending.tool_name.clone(),
        pending_tool_args_json: suspend.pending.args_json.clone(),
        unanswered_tool_call_ids: suspend.unanswered.clone(),
        claim_reprompted: state.claim_reprompted,
        inbox_seq: state.inbox_seq,
    })
}

/// Serializable history for a checkpoint: every message except a leading system prompt.
///
/// The live list keeps the system prompt at index 0. Resume rebuilds it, so the
/// checkpoint does not store it.
pub fn serde_messages_without_system(messages: &[ChatMessage]) -> Vec<SerdeChatMessage> {
    let skip = usize::from(messages.first().is_some_and(|m| m.role == "system"));
    messages
        .iter()
        .skip(skip)
        .map(SerdeChatMessage::from)
        .collect()
}

/// Encode the continuation as compact JSON and return `(bytes, sha256_hex)`.
///
/// The digest covers these exact bytes. Resume must send them back unchanged.
pub fn encode_hitl_continuation(
    continuation: &HitlContinuation,
) -> Result<(Vec<u8>, String), String> {
    let json = serde_json::to_vec(continuation)
        .map_err(|e| format!("failed to encode HITL continuation: {e}"))?;
    if json.len() > MAX_CHECKPOINT_BYTES {
        return Err(format!(
            "HITL continuation is {} bytes; max is {MAX_CHECKPOINT_BYTES}",
            json.len()
        ));
    }
    let digest = format!("{:x}", Sha256::digest(&json));
    Ok((json, digest))
}

/// Restore a continuation the host sent on resume.
///
/// `checkpoint_json` must be the exact stored bytes, and `expected_digest` must
/// be their SHA-256 hex. This function does not read the workspace.
pub fn continuation_from_host_payload(
    checkpoint_json: &str,
    expected_run_id: &str,
    expected_digest: &str,
) -> Option<HitlContinuation> {
    let actual = format!("{:x}", Sha256::digest(checkpoint_json.as_bytes()));
    if actual != expected_digest {
        return None;
    }
    parse_checkpoint(checkpoint_json.as_bytes(), expected_run_id)
}

/// Parse checkpoint bytes and normalize them to the current schema.
///
/// Schema 1 keeps the system prompt at `messages[0]`, so that message is dropped.
/// A schema 2 checkpoint must not carry one. Other schema versions are rejected.
fn parse_checkpoint(bytes: &[u8], expected_run_id: &str) -> Option<HitlContinuation> {
    let mut parsed: HitlContinuation = serde_json::from_slice(bytes).ok()?;
    let leads_with_system = parsed.messages.first().is_some_and(|m| m.role == "system");
    match parsed.schema_version {
        SCHEMA_VERSION_V1 => {
            if leads_with_system {
                parsed.messages.remove(0);
            }
        }
        SCHEMA_VERSION => {
            if leads_with_system {
                return None;
            }
        }
        _ => return None,
    }
    if parsed.run_id != expected_run_id {
        return None;
    }
    parsed.schema_version = SCHEMA_VERSION;
    Some(parsed)
}

/// Tool-call ids in the latest assistant message that have no matching tool response
/// and are not the pending (just-suspended) call.
pub fn unanswered_sibling_ids(messages: &[ChatMessage], pending_tool_call_id: &str) -> Vec<String> {
    let Some(assistant) = messages
        .iter()
        .rev()
        .find(|m| m.role == "assistant" && !m.tool_calls.is_empty())
    else {
        return Vec::new();
    };
    assistant
        .tool_calls
        .iter()
        .filter(|tc| tc.id != pending_tool_call_id)
        .filter(|tc| {
            !messages
                .iter()
                .any(|m| m.role == "tool" && m.tool_call_id == tc.id)
        })
        .map(|tc| tc.id.clone())
        .collect()
}

/// Append the approved tool result and placeholder responses for unanswered siblings.
pub fn append_approved_tool_and_placeholders(
    messages: &mut Vec<ChatMessage>,
    pending_tool_call_id: &str,
    observation: String,
    unanswered_tool_call_ids: &[String],
) {
    messages.push(ChatMessage {
        role: "tool".to_string(),
        content: observation,
        tool_calls: vec![],
        tool_call_id: pending_tool_call_id.to_string(),
    });
    for id in unanswered_tool_call_ids {
        if id == pending_tool_call_id {
            continue;
        }
        if messages
            .iter()
            .any(|m| m.role == "tool" && m.tool_call_id == *id)
        {
            continue;
        }
        messages.push(ChatMessage {
            role: "tool".to_string(),
            content: SIBLING_PLACEHOLDER.to_string(),
            tool_calls: vec![],
            tool_call_id: id.clone(),
        });
    }
}

/// Rebuild the conversation for a resume: the system prompt, the stored history,
/// the approved tool's observation (with placeholders for siblings), then the
/// resume nudge. `system_prompt` is rebuilt from the reloaded skills by the caller.
pub fn restore_messages_from_checkpoint(
    checkpoint: &HitlContinuation,
    system_prompt: &str,
    observation: String,
    resume_nudge: &str,
) -> Vec<ChatMessage> {
    let mut messages = vec![ChatMessage {
        role: "system".to_string(),
        content: system_prompt.to_string(),
        tool_calls: vec![],
        tool_call_id: String::new(),
    }];
    messages.extend(checkpoint.messages.iter().cloned().map(ChatMessage::from));
    append_approved_tool_and_placeholders(
        &mut messages,
        &checkpoint.pending_tool_call_id,
        observation,
        &checkpoint.unanswered_tool_call_ids,
    );
    // An empty nudge leaves the observation last (a question's answer reads as the
    // tool result of `ask_user`, with nothing after it).
    if !resume_nudge.is_empty() {
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: resume_nudge.to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        });
    }
    messages
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_continuation(run_id: &str) -> HitlContinuation {
        HitlContinuation {
            schema_version: SCHEMA_VERSION,
            run_id: run_id.to_string(),
            lifecycle_generation: 2,
            messages: vec![
                SerdeChatMessage {
                    role: "user".to_string(),
                    content: "Finish the plan.".to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                },
                SerdeChatMessage {
                    role: "assistant".to_string(),
                    content: String::new(),
                    tool_calls: vec![
                        SerdeToolCall {
                            id: "call_pending".to_string(),
                            name: "slack_post_message".to_string(),
                            arguments: r#"{"channel":"ops"}"#.to_string(),
                        },
                        SerdeToolCall {
                            id: "call_sibling".to_string(),
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"notes.md"}"#.to_string(),
                        },
                    ],
                    tool_call_id: String::new(),
                },
            ],
            loaded_skill_ids: vec!["slack-notify".to_string()],
            iterations_used: 3,
            max_iterations: 9,
            prompt_tokens: 100,
            completion_tokens: 40,
            cached_tokens: 0,
            total_tokens: 140,
            nudge_count: 0,
            last_tool_name: "load_skill".to_string(),
            same_tool_rounds: 1,
            skill_tool_called: false,
            last_answer_candidate: "working".to_string(),
            pending_tool_call_id: "call_pending".to_string(),
            pending_tool_name: "slack_post_message".to_string(),
            pending_tool_args_json: r#"{"channel":"ops"}"#.to_string(),
            unanswered_tool_call_ids: vec!["call_sibling".to_string()],
            claim_reprompted: false,
            inbox_seq: 0,
        }
    }

    #[test]
    fn checkpoint_round_trips_through_serde_mirrors() {
        let original = sample_continuation("urn:vam:run:local.vam.dev/acme/run-1");
        let proto_messages: Vec<ChatMessage> = original
            .messages
            .iter()
            .cloned()
            .map(ChatMessage::from)
            .collect();
        let mirrored: Vec<SerdeChatMessage> =
            proto_messages.iter().map(SerdeChatMessage::from).collect();
        assert_eq!(mirrored, original.messages);
    }

    #[test]
    fn unanswered_siblings_skip_pending_and_already_answered() {
        let messages = vec![
            ChatMessage {
                role: "assistant".to_string(),
                content: String::new(),
                tool_calls: vec![
                    ToolCall {
                        id: "a".to_string(),
                        name: "one".to_string(),
                        arguments: "{}".to_string(),
                    },
                    ToolCall {
                        id: "b".to_string(),
                        name: "two".to_string(),
                        arguments: "{}".to_string(),
                    },
                    ToolCall {
                        id: "c".to_string(),
                        name: "three".to_string(),
                        arguments: "{}".to_string(),
                    },
                ],
                tool_call_id: String::new(),
            },
            ChatMessage {
                role: "tool".to_string(),
                content: "done a".to_string(),
                tool_calls: vec![],
                tool_call_id: "a".to_string(),
            },
        ];
        assert_eq!(
            unanswered_sibling_ids(&messages, "b"),
            vec!["c".to_string()]
        );
    }

    #[test]
    fn restore_puts_rebuilt_prompt_first_then_observation_and_sibling_placeholders() {
        let checkpoint = sample_continuation("urn:vam:run:local.vam.dev/acme/run-1");
        let messages = restore_messages_from_checkpoint(
            &checkpoint,
            "REBUILT PROMPT",
            "posted to ops".to_string(),
            "Continue the original task.",
        );
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "REBUILT PROMPT");
        assert_eq!(messages.iter().filter(|m| m.role == "system").count(), 1);
        let tool_msgs: Vec<&ChatMessage> = messages.iter().filter(|m| m.role == "tool").collect();
        assert_eq!(tool_msgs.len(), 2);
        assert_eq!(tool_msgs[0].tool_call_id, "call_pending");
        assert_eq!(tool_msgs[0].content, "posted to ops");
        assert_eq!(tool_msgs[1].tool_call_id, "call_sibling");
        assert!(tool_msgs[1].content.contains("sibling tool call"));
        assert_eq!(messages.last().map(|m| m.role.as_str()), Some("user"));
        assert!(messages
            .last()
            .is_some_and(|m| m.content.contains("Continue the original task")));
    }

    #[test]
    fn serde_messages_without_system_drops_only_a_leading_system_prompt() {
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "prompt".to_string(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            },
        ];
        let stored = serde_messages_without_system(&messages);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].role, "user");
        assert_eq!(serde_messages_without_system(&messages[1..]).len(), 1);
    }

    #[test]
    fn one_mib_conversation_encodes_compact_under_the_cap_without_system_prompt() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-big";
        let mut continuation = sample_continuation(run_id);
        continuation.messages[0].content = "research notes ".repeat(75_000);

        let (bytes, digest) = encode_hitl_continuation(&continuation).expect("encode");
        assert!(bytes.len() > 1024 * 1024, "fixture should exceed 1 MiB");
        assert!(bytes.len() <= MAX_CHECKPOINT_BYTES);
        assert!(!bytes.contains(&b'\n'), "checkpoint JSON is compact");
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(
            !text.contains("\"role\":\"system\""),
            "the system prompt is not stored"
        );
        let restored = continuation_from_host_payload(text, run_id, &digest).expect("verifies");
        assert_eq!(restored, continuation);
    }

    #[test]
    fn schema_one_checkpoint_drops_stored_system_prompt_and_restores_with_rebuilt_prompt() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-v1";
        let v1 = serde_json::json!({
            "schema_version": 1,
            "run_id": run_id,
            "lifecycle_generation": 2,
            "messages": [
                {"role": "system", "content": "OLD PROMPT", "tool_calls": [], "tool_call_id": ""},
                {"role": "user", "content": "Finish the plan.", "tool_calls": [], "tool_call_id": ""},
                {"role": "assistant", "content": "", "tool_calls": [
                    {"id": "call_pending", "name": "slack_post_message", "arguments": "{}"}
                ], "tool_call_id": ""}
            ],
            "loaded_skill_ids": [],
            "iterations_used": 1,
            "max_iterations": 5,
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "cached_tokens": 0,
            "total_tokens": 0,
            "nudge_count": 0,
            "skill_tool_called": false,
            "last_answer_candidate": "",
            "pending_tool_call_id": "call_pending",
            "pending_tool_name": "slack_post_message",
            "pending_tool_args_json": "{}",
            "unanswered_tool_call_ids": []
        })
        .to_string();
        let digest = format!("{:x}", Sha256::digest(v1.as_bytes()));

        let restored =
            continuation_from_host_payload(&v1, run_id, &digest).expect("schema 1 verifies");
        assert_eq!(restored.schema_version, SCHEMA_VERSION);
        assert_eq!(
            restored.messages.len(),
            2,
            "the stored system prompt is dropped"
        );
        assert_eq!(restored.messages[0].role, "user");

        let messages = restore_messages_from_checkpoint(
            &restored,
            "REBUILT PROMPT",
            "posted".to_string(),
            "Continue.",
        );
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "REBUILT PROMPT");
        assert_eq!(messages.iter().filter(|m| m.role == "system").count(), 1);
        assert!(messages.iter().any(|m| m.role == "tool"
            && m.tool_call_id == "call_pending"
            && m.content == "posted"));
    }

    #[test]
    fn schema_two_with_a_system_prompt_and_unknown_schemas_are_rejected() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-bad";
        let mut with_system = sample_continuation(run_id);
        with_system.messages.insert(
            0,
            SerdeChatMessage {
                role: "system".to_string(),
                content: "stored prompt".to_string(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            },
        );
        let (bytes, digest) = encode_hitl_continuation(&with_system).expect("encode");
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(continuation_from_host_payload(text, run_id, &digest).is_none());

        let mut future = sample_continuation(run_id);
        future.schema_version = 3;
        let (bytes, digest) = encode_hitl_continuation(&future).expect("encode");
        let text = std::str::from_utf8(&bytes).expect("utf8");
        assert!(continuation_from_host_payload(text, run_id, &digest).is_none());
    }

    #[test]
    fn checkpoint_deserializes_without_retired_same_tool_fields() {
        let json = r#"{
            "schema_version": 1,
            "run_id": "urn:vam:run:local.vam.dev/acme/run-1",
            "lifecycle_generation": 2,
            "messages": [],
            "loaded_skill_ids": [],
            "iterations_used": 1,
            "max_iterations": 5,
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "cached_tokens": 0,
            "total_tokens": 0,
            "nudge_count": 0,
            "skill_tool_called": false,
            "last_answer_candidate": "",
            "pending_tool_call_id": "call_pending",
            "pending_tool_name": "read_file",
            "pending_tool_args_json": "{}",
            "unanswered_tool_call_ids": []
        }"#;
        let loaded: HitlContinuation = serde_json::from_str(json).expect("legacy shape");
        assert_eq!(loaded.last_tool_name, "");
        assert_eq!(loaded.same_tool_rounds, 0);
        assert_eq!(loaded.pending_tool_name, "read_file");
    }

    #[test]
    fn checkpoint_deserializes_legacy_same_tool_fields() {
        let json = r#"{
            "schema_version": 1,
            "run_id": "urn:vam:run:local.vam.dev/acme/run-1",
            "lifecycle_generation": 2,
            "messages": [],
            "loaded_skill_ids": [],
            "iterations_used": 1,
            "max_iterations": 5,
            "prompt_tokens": 0,
            "completion_tokens": 0,
            "cached_tokens": 0,
            "total_tokens": 0,
            "nudge_count": 0,
            "last_tool_name": "grep",
            "same_tool_rounds": 9,
            "skill_tool_called": false,
            "last_answer_candidate": "",
            "pending_tool_call_id": "call_pending",
            "pending_tool_name": "grep",
            "pending_tool_args_json": "{}",
            "unanswered_tool_call_ids": []
        }"#;
        let loaded: HitlContinuation = serde_json::from_str(json).expect("legacy fields");
        assert_eq!(loaded.last_tool_name, "grep");
        assert_eq!(loaded.same_tool_rounds, 9);
    }

    #[test]
    fn host_payload_rejects_digest_mismatch_and_other_run() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-1";
        let (bytes, digest) =
            encode_hitl_continuation(&sample_continuation(run_id)).expect("encode");
        let json = std::str::from_utf8(&bytes).expect("utf8");
        assert!(continuation_from_host_payload(json, run_id, "deadbeef").is_none());
        assert!(continuation_from_host_payload(
            json,
            "urn:vam:run:local.vam.dev/acme/run-other",
            &digest
        )
        .is_none());
        assert!(continuation_from_host_payload(json, run_id, &digest).is_some());
    }
}
