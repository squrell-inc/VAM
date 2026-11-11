//! Shared pieces of context compaction (AR-2.10).
//!
//! The guest decides to compact and asks the model for a summary; the host
//! records the exchange and the `compaction` record. Both sides must build the
//! same summary message byte for byte, because the next request has to extend
//! the host's record. They do it with these functions only.

use serde_json::json;

use crate::thread_context::{canonical_json, sha256_hex};
use crate::v1::ChatMessage;

/// First line of the instruction message that ends a summary request.
pub const INSTRUCTION_PREFIX: &str = "[context compaction]";
/// First line of the summary message that replaces the summarised history.
pub const SUMMARY_HEADER: &str = "Summary of the earlier conversation:";
/// Log lane that holds the summary exchange.
pub const SUMMARY_LANE: &str = "summary";
/// Log lane that holds the harness notes (the todo recitation) a model saw.
pub const NOTE_LANE: &str = "note";
/// Log lane that holds the reasoning text of a reply, kept only under
/// `adapter.thinking: strip`. Host-only; assembly never reads it.
pub const REASONING_LANE: &str = "reasoning";
/// Log lane that holds the gaps of refused resample labels. Host-only.
pub const RESAMPLE_LANE: &str = "resample";
/// First line of the todo recitation note: the host accepts a note hint only
/// for a last message that starts with it.
pub const NOTE_LABEL: &str =
    "[Harness reminder: your current todo list. Not a user message; do not reply to it.]\n";
/// Most bytes of the plan in a recitation note.
pub const NOTE_PLAN_CAP: usize = 4 * 1024;

pub const REASON_TOKENS: &str = "tokens";
pub const REASON_BYTES: &str = "bytes";
pub const REASON_PROVIDER_LIMIT: &str = "provider_limit";
/// Written by the host when a request does not extend the recorded head of a
/// compacted context: a fresh compaction record, never an un-hiding rollback.
pub const REASON_REBASE: &str = "rebase";

/// The `ContextHint.reason` of a request that is the one resample of an invalid tool
/// call (`adapter.resampleOnInvalid`). Valid on the main or the note lane.
pub const REASON_RESAMPLE: &str = "resample";

/// Is this hint the resample label?
pub fn is_resample_hint(hint: Option<&crate::v1::ContextHint>) -> bool {
    hint.is_some_and(|h| h.reason == REASON_RESAMPLE)
}

/// The reasons a guest may declare in a hint.
pub fn is_declarable_reason(reason: &str) -> bool {
    matches!(reason, REASON_TOKENS | REASON_BYTES | REASON_PROVIDER_LIMIT)
}

/// Cap on the model's summary text inside the summary message.
pub const MAX_SUMMARY_TEXT_BYTES: usize = 64 * 1024;
/// Cap on the verbatim user-message block of a summary message.
pub const MAX_VERBATIM_BYTES: usize = 32 * 1024;
/// Cap on one user message inside the verbatim block.
pub const MAX_VERBATIM_ENTRY_BYTES: usize = 8 * 1024;

const VERBATIM_HEADER: &str = "User messages in the summarised part (verbatim):";
const ENTRY_MARKER: &str = "\n[user message]\n";
const TRAILER: &str =
    "Plan and notes: read PLAN.md and notes/ in the workspace before you continue.";
const TRUNCATED: &str = "\n[truncated]";

/// The instruction that ends a summary request. `plan` is the agent's current
/// todo list, if it has one; the model carries it into the summary.
pub fn instruction_message(plan: Option<&str>) -> ChatMessage {
    let mut text = format!(
        "{INSTRUCTION_PREFIX}\nWrite a summary of the conversation above for yourself, so you can \
         continue the task without it. Use this structure:\n\
         1. Goal: what the user wants and any correction they made.\n\
         2. Every user message, verbatim.\n\
         3. Decisions made and why.\n\
         4. All ids, file paths, URLs, names and numbers that may still matter.\n\
         5. Files touched.\n\
         6. Open todos and the next step.\n\
         7. Pending effects: calls that were started or may have taken effect and need a check.\n\
         Do not invent anything. Plain text, at most 2,000 words. Do not call tools."
    );
    if let Some(plan) = plan.map(str::trim).filter(|p| !p.is_empty()) {
        text.push_str("\n\nCurrent todo list (carry it into section 6):\n");
        text.push_str(&cap_str(plan, 4 * 1024));
    }
    user(text)
}

/// The todo recitation note: the label and the plan capped at
/// [`NOTE_PLAN_CAP`] bytes, as a user message appended to one request.
pub fn recitation_note(plan: &str) -> ChatMessage {
    user(format!("{NOTE_LABEL}{}", cap_str(plan, NOTE_PLAN_CAP)))
}

/// Whether `m` is shaped like a recitation note: a user message that starts
/// with the label and holds at most [`NOTE_PLAN_CAP`] bytes after it, and
/// carries no tool calls.
pub fn is_recitation_note(m: &ChatMessage) -> bool {
    m.role == "user"
        && m.tool_calls.is_empty()
        && m.tool_call_id.is_empty()
        && m.content
            .strip_prefix(NOTE_LABEL)
            .is_some_and(|plan| plan.len() <= NOTE_PLAN_CAP)
}

fn user(content: String) -> ChatMessage {
    ChatMessage {
        role: "user".to_string(),
        content,
        tool_calls: vec![],
        tool_call_id: String::new(),
    }
}

pub fn is_instruction(m: &ChatMessage) -> bool {
    m.role == "user" && m.content.starts_with(INSTRUCTION_PREFIX)
}

pub fn is_summary_message(m: &ChatMessage) -> bool {
    m.role == "user" && m.content.starts_with(SUMMARY_HEADER)
}

/// `text` cut to at most `max` bytes at a character boundary.
pub fn cap_str(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_string();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_string()
}

fn capped_with_marker(text: &str, max: usize) -> String {
    if text.len() <= max {
        text.to_string()
    } else {
        format!("{}{TRUNCATED}", cap_str(text, max))
    }
}

/// User messages already carried by an earlier summary message.
fn carried_entries(summary: &ChatMessage) -> Vec<String> {
    let Some((_, rest)) = summary.content.split_once(VERBATIM_HEADER) else {
        return vec![];
    };
    let block = match rest.rsplit_once(&format!("\n\n{TRAILER}")) {
        Some((block, _)) => block,
        None => rest,
    };
    block
        .split(ENTRY_MARKER)
        .skip(1)
        .map(str::to_string)
        .collect()
}

/// The verbatim block entries for `summarised`: what earlier summaries already
/// carry, then each user message that is neither a summary nor an instruction.
/// Over the cap, the first entry and the newest ones that fit are kept.
fn verbatim_entries(summarised: &[ChatMessage]) -> Vec<String> {
    let mut entries: Vec<String> = Vec::new();
    for m in summarised {
        if is_summary_message(m) {
            entries.extend(carried_entries(m));
        } else if m.role == "user" && !is_instruction(m) {
            entries.push(capped_with_marker(&m.content, MAX_VERBATIM_ENTRY_BYTES));
        }
    }
    let total: usize = entries.iter().map(|e| e.len() + ENTRY_MARKER.len()).sum();
    if total <= MAX_VERBATIM_BYTES || entries.len() <= 1 {
        return entries;
    }
    let first = entries[0].clone();
    let mut budget = MAX_VERBATIM_BYTES.saturating_sub(first.len() + ENTRY_MARKER.len() + 64);
    let mut newest: Vec<String> = Vec::new();
    for e in entries[1..].iter().rev() {
        let cost = e.len() + ENTRY_MARKER.len();
        if cost > budget {
            break;
        }
        budget -= cost;
        newest.push(e.clone());
    }
    newest.reverse();
    let omitted = entries.len() - 1 - newest.len();
    let mut out = vec![first];
    if omitted > 0 {
        out.push(format!("[{omitted} earlier user messages omitted]"));
    }
    out.extend(newest);
    out
}

/// The message that replaces the summarised history: the model's summary text,
/// every user message of the summarised part verbatim, and a pointer to the
/// plan and notes in the workspace. Pure; guest and host call it on the same
/// inputs.
pub fn summary_message(model_text: &str, summarised: &[ChatMessage]) -> ChatMessage {
    let text = capped_with_marker(model_text.trim(), MAX_SUMMARY_TEXT_BYTES);
    let mut content = format!("{SUMMARY_HEADER}\n{text}");
    let entries = verbatim_entries(summarised);
    if !entries.is_empty() {
        content.push_str("\n\n");
        content.push_str(VERBATIM_HEADER);
        for e in &entries {
            content.push_str(ENTRY_MARKER);
            content.push_str(e);
        }
    }
    content.push_str("\n\n");
    content.push_str(TRAILER);
    user(content)
}

/// Digest of a message in the canonical wire form the thread log uses
/// (`role`, `content`, `tool_calls`, `tool_call_id`).
pub fn message_digest(m: &ChatMessage) -> String {
    let calls: Vec<serde_json::Value> = m
        .tool_calls
        .iter()
        .map(|c| json!({"id": c.id, "name": c.name, "arguments": c.arguments}))
        .collect();
    let value = json!({
        "role": m.role,
        "content": m.content,
        "tool_calls": calls,
        "tool_call_id": m.tool_call_id,
    });
    sha256_hex(canonical_json(&value).as_bytes())
}

/// Rough size of a message in bytes: the text a provider would count.
pub fn approx_bytes(m: &ChatMessage) -> usize {
    m.content.len()
        + m.tool_call_id.len()
        + m.tool_calls
            .iter()
            .map(|c| c.id.len() + c.name.len() + c.arguments.len() + 16)
            .sum::<usize>()
        + 16
}

pub fn approx_bytes_all(messages: &[ChatMessage]) -> usize {
    messages.iter().map(approx_bytes).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.to_string(),
            content: content.to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    #[test]
    fn summary_is_deterministic_and_carries_every_user_message() {
        let history = vec![
            msg("user", "use bravo.txt"),
            msg("assistant", "ok"),
            msg("user", "and keep it short"),
        ];
        let a = summary_message("Summary: done.", &history);
        let b = summary_message("Summary: done.", &history);
        assert_eq!(a, b);
        assert!(is_summary_message(&a));
        assert!(a.content.contains("use bravo.txt"));
        assert!(a.content.contains("and keep it short"));
        assert!(!a.content.contains("\nok\n"));
        assert!(a.content.ends_with(TRAILER));
    }

    #[test]
    fn a_second_summary_carries_the_first_ones_user_messages() {
        let first = summary_message("one", &[msg("user", "rule A")]);
        let second = summary_message(
            "two",
            &[first, msg("user", "rule B"), msg("assistant", "x")],
        );
        assert!(second.content.contains("rule A"));
        assert!(second.content.contains("rule B"));
        assert_eq!(second.content.matches("rule A").count(), 1);
    }

    #[test]
    fn instructions_and_summaries_are_not_user_messages() {
        let history = vec![instruction_message(Some("- [x] a")), msg("user", "real")];
        let s = summary_message("t", &history);
        assert!(s.content.contains("real"));
        assert!(!s.content.contains("Write a summary of the conversation"));
    }

    #[test]
    fn the_verbatim_block_is_capped_and_keeps_first_and_newest() {
        let history: Vec<ChatMessage> = (0..100)
            .map(|i| msg("user", &format!("m{i:03} {}", "x".repeat(2000))))
            .collect();
        let s = summary_message("t", &history);
        assert!(s.content.len() < MAX_VERBATIM_BYTES + 2000 + 1024);
        assert!(s.content.contains("m000 "));
        assert!(s.content.contains("m099 "));
        assert!(s.content.contains("earlier user messages omitted"));
    }

    #[test]
    fn long_summary_text_is_cut() {
        let s = summary_message(&"y".repeat(MAX_SUMMARY_TEXT_BYTES * 2), &[]);
        assert!(s.content.contains("[truncated]"));
        assert!(s.content.len() < MAX_SUMMARY_TEXT_BYTES + 512);
    }

    #[test]
    fn digest_ignores_nothing_it_should_see() {
        let a = message_digest(&msg("user", "a"));
        assert_eq!(a, message_digest(&msg("user", "a")));
        assert_ne!(a, message_digest(&msg("user", "b")));
        assert_ne!(a, message_digest(&msg("assistant", "a")));
    }

    #[test]
    fn declarable_reasons_exclude_rebase() {
        assert!(is_declarable_reason("tokens") && is_declarable_reason("bytes"));
        assert!(is_declarable_reason("provider_limit"));
        assert!(!is_declarable_reason("rebase") && !is_declarable_reason(""));
    }
}
