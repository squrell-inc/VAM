//! Pure helpers for the conversation a task starts from and keeps (AR-2.5,
//! AR-2.12).
//!
//! * [`restore_history`] turns a fetched thread context into the messages a
//!   follow-up begins with, and repairs a trailing assistant message whose
//!   calls have no result (a failed task, or a `task_complete` call).
//! * [`clear_old_results`] is batched clearing: every [`CLEAR_EVERY_ROUNDS`]
//!   rounds, if the old tool results are worth at least [`CLEAR_MIN_TOKENS`],
//!   they are replaced in one pass by a placeholder that names the file or the
//!   `tool_call_id`. It is never a per-round rewrite: one clear costs one
//!   prompt-cache break and one `rollback` in the host log.

use vam_spec::thread_context::{ThreadContext, WireMessage};
use vam_spec::v1::{ChatMessage, ToolCall};

/// Rounds between clearing passes.
pub const CLEAR_EVERY_ROUNDS: usize = 8;
/// Clear only when the clearable results hold at least this many tokens
/// (bytes divided by four).
pub const CLEAR_MIN_TOKENS: usize = 8_000;
/// The newest tool results are never cleared.
pub const KEEP_RECENT_RESULTS: usize = 6;
/// Results smaller than this are not worth clearing.
pub const MIN_CLEARABLE_BYTES: usize = 1024;
/// Marks a result that was already cleared.
pub const CLEARED_PREFIX: &str = "[Cleared ";

const DANGLING_RESULT: &str = "No result was recorded for this call (the task ended before it \
     finished). Check whether it took effect before retrying.";
const TASK_COMPLETE_RESULT: &str = "Task marked complete.";

pub fn wire_to_chat(wire: &WireMessage) -> ChatMessage {
    ChatMessage {
        role: wire.role.clone(),
        content: wire.content.clone(),
        tool_calls: wire
            .tool_calls
            .iter()
            .map(|c| ToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .collect(),
        tool_call_id: wire.tool_call_id.clone(),
    }
}

pub fn chat_to_wire(message: &ChatMessage) -> WireMessage {
    WireMessage {
        role: message.role.clone(),
        content: message.content.clone(),
        tool_calls: message
            .tool_calls
            .iter()
            .map(|c| vam_spec::thread_context::WireToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .collect(),
        tool_call_id: message.tool_call_id.clone(),
    }
}

/// `text` cut to at most `max` bytes at a character boundary.
pub fn cap_text(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

/// The messages after the system prompt for a task that continues a thread:
/// stable blocks, history, per-task blocks, with unanswered calls closed.
pub fn restore_history(ctx: &ThreadContext) -> Vec<ChatMessage> {
    let mut messages: Vec<ChatMessage> = ctx.placed_messages().iter().map(wire_to_chat).collect();
    repair_dangling_calls(&mut messages);
    messages
}

/// The round the model had decided when a task's guest died (E10 Phase 4): its assistant
/// message, whose calls are acted on before the model is asked again.
#[derive(Debug, Clone, PartialEq)]
pub struct DecidedRound {
    /// What the assistant message said besides its calls.
    pub text: String,
    /// All the calls of the message, in the order the model made them.
    pub calls: Vec<ToolCall>,
    /// Results the log already holds for some of the calls, by call id (an approval resume
    /// that crashed mid-round). They are put back instead of running those calls again.
    pub answered: std::collections::HashMap<String, ChatMessage>,
}

/// [`restore_history`] for a task that resumes after a crash. A conversation that ends in a
/// decided round does not get its open calls closed: the round is returned apart, and the
/// guest runs its unanswered calls in order. The history before it is repaired as usual.
pub fn restore_history_for_crash(ctx: &ThreadContext) -> (Vec<ChatMessage>, Option<DecidedRound>) {
    let mut messages: Vec<ChatMessage> = ctx.placed_messages().iter().map(wire_to_chat).collect();
    let open = ctx
        .crash
        .as_ref()
        .is_some_and(|crash| !crash.unanswered.is_empty());
    let index = messages
        .iter()
        .rposition(|m| m.role == "assistant" && !m.tool_calls.is_empty());
    let (Some(index), true) = (index, open) else {
        repair_dangling_calls(&mut messages);
        return (messages, None);
    };
    let after = messages.split_off(index);
    repair_dangling_calls(&mut messages);
    let mut after = after.into_iter();
    let Some(decided) = after.next() else {
        return (messages, None);
    };
    let answered = after
        .filter(|m| m.role == "tool")
        .map(|m| (m.tool_call_id.clone(), m))
        .collect();
    (
        messages,
        Some(DecidedRound {
            text: decided.content,
            calls: decided.tool_calls,
            answered,
        }),
    )
}

/// The skills a conversation loaded, in order: the `skill` argument of each `load_skill` call
/// whose result is not an error. The guest that died held them in memory only.
pub fn loaded_skills_in(messages: &[ChatMessage]) -> Vec<String> {
    let mut ids: Vec<String> = Vec::new();
    for (index, message) in messages.iter().enumerate() {
        if message.role != "assistant" {
            continue;
        }
        for call in message.tool_calls.iter().filter(|c| c.name == "load_skill") {
            let ok = messages[index + 1..]
                .iter()
                .find(|m| m.role == "tool" && m.tool_call_id == call.id)
                .is_some_and(|m| !m.content.starts_with("tool_error"));
            let id = serde_json::from_str::<serde_json::Value>(&call.arguments)
                .ok()
                .and_then(|v| v.get("skill").and_then(|s| s.as_str().map(str::to_string)));
            if let (true, Some(id)) = (ok, id) {
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
    }
    ids
}

/// Adds a tool result after any assistant message whose calls are not all
/// answered, so the next request is valid for every provider.
pub fn repair_dangling_calls(messages: &mut Vec<ChatMessage>) {
    let mut i = 0;
    while i < messages.len() {
        if messages[i].role == "assistant" && !messages[i].tool_calls.is_empty() {
            let mut end = i + 1;
            while end < messages.len() && messages[end].role == "tool" {
                end += 1;
            }
            let missing: Vec<ChatMessage> = messages[i]
                .tool_calls
                .iter()
                .filter(|call| {
                    !messages[i + 1..end]
                        .iter()
                        .any(|m| m.tool_call_id == call.id)
                })
                .map(|call| ChatMessage {
                    role: "tool".to_string(),
                    content: if call.name == crate::skills::TASK_COMPLETE_TOOL_NAME {
                        TASK_COMPLETE_RESULT.to_string()
                    } else {
                        DANGLING_RESULT.to_string()
                    },
                    tool_calls: vec![],
                    tool_call_id: call.id.clone(),
                })
                .collect();
            let added = missing.len();
            for (offset, message) in missing.into_iter().enumerate() {
                messages.insert(end + offset, message);
            }
            i = end + added;
        } else {
            i += 1;
        }
    }
}

fn tool_name_for(messages: &[ChatMessage], before: usize, call_id: &str) -> String {
    messages[..before]
        .iter()
        .rev()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.tool_calls.iter())
        .find(|c| c.id == call_id)
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "tool".to_string())
}

/// The placeholder for a cleared result. It names the file when the output was
/// written to one, otherwise the `tool_call_id` that finds the thread-log record.
pub fn clear_placeholder(tool: &str, call_id: &str, bytes: usize, file: Option<&str>) -> String {
    match file {
        Some(path) => format!(
            "{CLEARED_PREFIX}{tool} result ({bytes} bytes, call {call_id}). Full output: read_file {path} if needed]"
        ),
        None => format!(
            "{CLEARED_PREFIX}{tool} result ({bytes} bytes, call {call_id}). Full text is in the thread log (record for call {call_id}); re-run the tool if needed]"
        ),
    }
}

/// Clears old tool results in one pass when they hold at least
/// [`CLEAR_MIN_TOKENS`]. The caller decides the cadence ([`CLEAR_EVERY_ROUNDS`]).
/// `file_of` finds the file a bulky output was written to. Returns how many
/// results were cleared.
pub fn clear_old_results(
    messages: &mut [ChatMessage],
    file_of: impl Fn(&str) -> Option<String>,
) -> usize {
    clear_old_results_from(messages, 0, file_of)
}

/// [`clear_old_results`] limited to messages at `floor` and after. After a
/// compaction the floor is the end of the kept tail, so a clearing pass never
/// rewrites a message the host recorded before the compaction.
pub fn clear_old_results_from(
    messages: &mut [ChatMessage],
    floor: usize,
    file_of: impl Fn(&str) -> Option<String>,
) -> usize {
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter(|(i, m)| *i >= floor && m.role == "tool")
        .map(|(i, _)| i)
        .collect();
    if tool_indices.len() <= KEEP_RECENT_RESULTS {
        return 0;
    }
    let old = &tool_indices[..tool_indices.len() - KEEP_RECENT_RESULTS];
    let eligible: Vec<usize> = old
        .iter()
        .copied()
        .filter(|&i| {
            messages[i].content.len() >= MIN_CLEARABLE_BYTES
                && !messages[i].content.starts_with(CLEARED_PREFIX)
        })
        .collect();
    let tokens: usize = eligible
        .iter()
        .map(|&i| messages[i].content.len() / 4)
        .sum();
    if eligible.is_empty() || tokens < CLEAR_MIN_TOKENS {
        return 0;
    }
    for &i in &eligible {
        let tool = tool_name_for(messages, i, &messages[i].tool_call_id);
        let file = file_of(&messages[i].content);
        let placeholder = clear_placeholder(
            &tool,
            &messages[i].tool_call_id,
            messages[i].content.len(),
            file.as_deref(),
        );
        messages[i].content = placeholder;
    }
    eligible.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::thread_context::{
        BlockKind, BlockPlacement, BlockTrust, ContextBlock, WireToolCall, THREAD_CONTEXT_FORMAT,
        THREAD_CONTEXT_VERSION,
    };

    fn chat(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    fn call(id: &str, name: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: name.into(),
            arguments: "{}".into(),
        }
    }

    fn assistant(calls: Vec<ToolCall>) -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: calls,
            tool_call_id: String::new(),
        }
    }

    fn tool(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "tool".into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: id.into(),
        }
    }

    #[test]
    fn cap_text_cuts_at_a_character_boundary() {
        assert_eq!(cap_text("abc", 10), "abc");
        assert_eq!(cap_text("héllo", 2), "h");
        assert_eq!(cap_text("héllo", 3), "hé");
    }

    #[test]
    fn restore_places_blocks_and_closes_a_trailing_call() {
        let ctx = ThreadContext {
            format: THREAD_CONTEXT_FORMAT.into(),
            version: THREAD_CONTEXT_VERSION,
            run_id: "r".into(),
            head_seq: 3,
            head_digest: "d".into(),
            system_sha256: None,
            tools_sha256: None,
            messages: vec![
                WireMessage {
                    role: "user".into(),
                    content: "go".into(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                },
                WireMessage {
                    role: "assistant".into(),
                    content: String::new(),
                    tool_calls: vec![WireToolCall {
                        id: "c1".into(),
                        name: "run".into(),
                        arguments: "{}".into(),
                    }],
                    tool_call_id: String::new(),
                },
            ],
            suspend: None,
            crash: None,
            blocks: vec![ContextBlock {
                source: "fake".into(),
                kind: BlockKind::AgentMemory,
                placement: BlockPlacement::Stable,
                trust: BlockTrust::Untrusted,
                text: "mem".into(),
            }],
        };
        let out = restore_history(&ctx);
        let roles: Vec<&str> = out.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(roles, ["user", "user", "assistant", "tool"]);
        assert_eq!(out[0].content, "mem");
        assert_eq!(out[3].tool_call_id, "c1");
        assert!(out[3].content.contains("Check whether it took effect"));
    }

    #[test]
    fn repair_names_task_complete_and_fills_only_the_missing_results() {
        let mut m = vec![
            chat("user", "u"),
            assistant(vec![call("a", "read_file"), call("b", "task_complete")]),
            tool("a", "ok"),
            chat("user", "next"),
        ];
        repair_dangling_calls(&mut m);
        assert_eq!(m.len(), 5);
        assert_eq!(m[2].content, "ok");
        assert_eq!(m[3].tool_call_id, "b");
        assert_eq!(m[3].content, "Task marked complete.");
        assert_eq!(m[4].content, "next");
        let before = m.clone();
        repair_dangling_calls(&mut m);
        assert_eq!(m, before, "a repaired list is stable");
    }

    fn conversation(results: usize, size: usize) -> Vec<ChatMessage> {
        let mut m = vec![chat("user", "go")];
        for i in 0..results {
            let id = format!("c{i}");
            m.push(assistant(vec![call(&id, "read_file")]));
            m.push(tool(&id, &"x".repeat(size)));
        }
        m
    }

    #[test]
    fn clearing_keeps_the_newest_results_and_clears_the_rest_in_one_pass() {
        let mut m = conversation(10, 10_000);
        let cleared = clear_old_results(&mut m, |_| None);
        assert_eq!(cleared, 4);
        let tools: Vec<&ChatMessage> = m.iter().filter(|x| x.role == "tool").collect();
        assert!(tools[..4]
            .iter()
            .all(|t| t.content.starts_with(CLEARED_PREFIX)));
        assert!(tools[0].content.contains("call c0"));
        assert!(tools[0].content.contains("read_file result (10000 bytes"));
        assert!(tools[4..].iter().all(|t| t.content.len() == 10_000));
        assert_eq!(clear_old_results(&mut m, |_| None), 0, "idempotent");
    }

    #[test]
    fn clearing_waits_for_enough_tokens_and_skips_small_results() {
        let mut m = conversation(10, 4_000);
        assert_eq!(
            clear_old_results(&mut m, |_| None),
            0,
            "4 x 1000 tokens < 8000"
        );
        let mut small = conversation(12, 500);
        assert_eq!(clear_old_results(&mut small, |_| None), 0);
    }

    #[test]
    fn the_placeholder_names_the_file_when_there_is_one() {
        let mut m = conversation(8, 20_000);
        clear_old_results(&mut m, |_| Some("notes/out.txt".into()));
        let first = m.iter().find(|x| x.role == "tool").unwrap();
        assert!(first.content.contains("read_file notes/out.txt"));
        assert!(!first.content.contains("thread log"));
    }
    #[test]
    fn clearing_never_touches_messages_below_the_floor() {
        let mut m = conversation(14, 10_000);
        let floor = m.len() - 2 * 6;
        let before: Vec<String> = m[..floor].iter().map(|x| x.content.clone()).collect();
        let cleared = clear_old_results_from(&mut m, floor, |_| None);
        // Only the results at or after the floor, outside the newest six, are cleared.
        assert!(cleared <= 14 - 6);
        let after: Vec<String> = m[..floor].iter().map(|x| x.content.clone()).collect();
        assert_eq!(before, after, "the protected part is untouched");
        let mut all = conversation(14, 10_000);
        assert!(clear_old_results_from(&mut all, 0, |_| None) > cleared);
    }
}
