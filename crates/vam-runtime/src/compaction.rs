//! Context compaction by summary (AR-2.10), guest side: when to compact and
//! where to cut. The model call and the glue live in `agent.rs`.
//!
//! * **One trigger, one place.** [`decide`] runs before every model call: at the
//!   start of a follow-up and after every tool round. The estimate is the
//!   provider's reported prompt tokens for the last call plus the bytes added
//!   since, divided by four. The trigger is [`TRIGGER_PERCENT`] of the task
//!   model's `context_window`; a configurable trigger would replace this one
//!   function. Byte limits are guards only.
//! * **The cut** is at a round boundary: the kept tail never starts with a tool
//!   result, so a tool call is never split from its result. It holds the
//!   current user message and up to [`KEEP_ROUNDS`] rounds, within
//!   [`TAIL_PERCENT`] of the window; what does not fit is summarised. The
//!   summary template asks for pending effects and open todos, so work in
//!   flight survives a cut in the middle of a tool loop.
//! * The host verifies the declared kept tail against its own record; nothing
//!   here is trusted by it.

use vam_spec::compaction as spec;
use vam_spec::v1::ChatMessage;

/// Error prefixes the host and broker use (stable tokens).
pub const CONTEXT_TOO_LARGE: &str = "context_too_large";
pub const CONTEXT_TOO_LONG: &str = "context_too_long";
pub const COMPACTION_REFUSED: &str = "compaction_refused";

/// Compact when the estimated context reaches this share of the window.
pub const TRIGGER_PERCENT: u64 = 70;
/// Guard: compact on bytes alone at this size, well under the 16 MiB limit.
pub const BYTES_TRIGGER: usize = 12 * 1024 * 1024;
/// Guard: the host refuses a request over this size.
pub const GUARD_BYTES: usize = 16 * 1024 * 1024;
/// Rounds kept verbatim after a compaction, at most.
pub const KEEP_ROUNDS: usize = 3;
/// The kept tail stays within this share of the window.
pub const TAIL_PERCENT: u64 = 25;
/// Rounds to wait after a failed compaction before trying again (the trigger
/// is still met; asking every round would double the cost of a bad model).
pub const RETRY_AFTER_ROUNDS: usize = 4;

/// What the guest knows about the size of the next request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ContextEstimate {
    /// Prompt tokens the provider reported for the last call, if any call was
    /// made since the context last changed shape.
    pub reported_prompt_tokens: Option<u64>,
    /// Bytes of messages added after that call.
    pub bytes_since_report: usize,
    /// System prompt, tool definitions and every message, in bytes.
    pub total_bytes: usize,
    /// Bytes of the messages alone (the byte guard counts these).
    pub message_bytes: usize,
}

impl ContextEstimate {
    pub fn tokens(&self) -> u64 {
        match self.reported_prompt_tokens {
            Some(reported) => reported + (self.bytes_since_report / 4) as u64,
            None => (self.total_bytes / 4) as u64,
        }
    }
}

/// Why to compact now, if at all.
pub fn decide(
    estimate: &ContextEstimate,
    window: Option<u64>,
    trigger_percent: u64,
) -> Option<&'static str> {
    if let Some(window) = window.filter(|w| *w > 0) {
        if estimate.tokens().saturating_mul(100) >= window.saturating_mul(trigger_percent) {
            return Some(spec::REASON_TOKENS);
        }
    }
    if estimate.message_bytes >= BYTES_TRIGGER {
        return Some(spec::REASON_BYTES);
    }
    None
}

/// Tracks the provider-reported size of the last call.
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageTracker {
    reported: Option<(u64, usize)>,
}

impl UsageTracker {
    /// The last call carried `message_count` messages and cost `prompt_tokens`.
    pub fn note(&mut self, prompt_tokens: i64, message_count: usize) {
        self.reported = (prompt_tokens > 0).then_some((prompt_tokens as u64, message_count));
    }

    /// The messages were rewritten (a compaction or a clearing pass): the
    /// reported number no longer describes them.
    pub fn reset(&mut self) {
        self.reported = None;
    }

    pub fn estimate(&self, messages: &[ChatMessage], fixed_bytes: usize) -> ContextEstimate {
        let message_bytes = spec::approx_bytes_all(messages);
        match self.reported {
            Some((tokens, count)) if count <= messages.len() => ContextEstimate {
                reported_prompt_tokens: Some(tokens),
                bytes_since_report: spec::approx_bytes_all(&messages[count..]),
                total_bytes: fixed_bytes + message_bytes,
                message_bytes,
            },
            _ => ContextEstimate {
                reported_prompt_tokens: None,
                bytes_since_report: 0,
                total_bytes: fixed_bytes + message_bytes,
                message_bytes,
            },
        }
    }
}

/// A planned cut: the first kept message, as an index into the full message
/// list (system message at 0).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cut {
    pub keep_from: usize,
}

impl Cut {
    /// The index the host's hint uses: among the messages after the system one.
    pub fn hint_index(&self) -> u32 {
        (self.keep_from - 1) as u32
    }
}

/// Where to cut `messages` (system message first). `task_user` is the index of
/// the current user message, if known. `window` is in tokens. `None` when
/// there is nothing to summarise.
pub fn plan_cut(
    messages: &[ChatMessage],
    task_user: Option<usize>,
    window: Option<u64>,
    keep_rounds: usize,
) -> Option<Cut> {
    if messages.len() < 3 || messages.first().map(|m| m.role.as_str()) != Some("system") {
        return None;
    }
    // Valid boundaries: not a tool result, and something before it to summarise.
    let boundaries: Vec<usize> = (2..messages.len())
        .filter(|i| messages[*i].role != "tool")
        .collect();
    if boundaries.is_empty() {
        return None;
    }
    let tail_bytes = |from: usize| spec::approx_bytes_all(&messages[from..]);
    let budget = window
        .filter(|w| *w > 0)
        .map(|w| (w.saturating_mul(TAIL_PERCENT) / 100).saturating_mul(4) as usize)
        .unwrap_or(BYTES_TRIGGER / 4);

    // Start of the last KEEP_ROUNDS rounds (a round starts at an assistant message).
    let assistants: Vec<usize> = boundaries
        .iter()
        .copied()
        .filter(|i| messages[*i].role == "assistant")
        .collect();
    let rounds_start = if assistants.len() > keep_rounds {
        assistants[assistants.len() - keep_rounds]
    } else {
        boundaries[0]
    };
    // Prefer keeping the current user message too, when the tail still fits.
    let mut start = match task_user.filter(|u| boundaries.contains(u)) {
        Some(user) if user < rounds_start && tail_bytes(user) <= budget => user,
        _ => rounds_start,
    };
    // Shrink to later boundaries until the tail fits the budget.
    while tail_bytes(start) > budget {
        match boundaries.iter().copied().find(|b| *b > start) {
            Some(next) => start = next,
            None => break,
        }
    }
    Some(Cut { keep_from: start })
}

/// The tool results of one round together may take at most this share of the
/// window. With the 70% trigger this keeps the estimate under about 90% after
/// any round (plus a reply of at most 4,096 tokens), so the summary request,
/// which carries the whole context plus the instruction, always fits.
pub const ROUND_RESULTS_PERCENT: u64 = 15;
/// A result is never cut below this many bytes.
const MIN_RESULT_BYTES: usize = 2 * 1024;

/// Trims the tool results a round just added (`round`: the messages after its
/// assistant message) so together they stay within [`ROUND_RESULTS_PERCENT`] of
/// the window. Each oversized result keeps its first bytes and says it was
/// cut; the thread log then records exactly what the model saw. Returns how
/// many results were cut. Does nothing without a window.
pub fn cap_round_results(round: &mut [ChatMessage], window: Option<u64>) -> usize {
    let Some(window) = window.filter(|w| *w > 0) else {
        return 0;
    };
    let budget = (window
        .saturating_mul(4)
        .saturating_mul(ROUND_RESULTS_PERCENT)
        / 100) as usize;
    let results: Vec<usize> = round
        .iter()
        .enumerate()
        .filter(|(_, m)| m.role == "tool")
        .map(|(i, _)| i)
        .collect();
    let total: usize = results.iter().map(|i| round[*i].content.len()).sum();
    if results.is_empty() || total <= budget {
        return 0;
    }
    let share = (budget / results.len()).max(MIN_RESULT_BYTES);
    let mut cut = 0;
    for i in results {
        let len = round[i].content.len();
        if len > share {
            let kept = spec::cap_str(&round[i].content, share);
            round[i].content = format!(
                "{kept}\n[truncated: kept {} of {len} bytes to fit the context window; \
                 narrow the request and run it again]",
                kept.len()
            );
            cut += 1;
        }
    }
    cut
}

/// The messages after a compaction: the system message, the summary, the tail.
pub fn compacted_messages(
    messages: &[ChatMessage],
    cut: Cut,
    summary_text: &str,
) -> Vec<ChatMessage> {
    let summary = spec::summary_message(summary_text, &messages[1..cut.keep_from]);
    let mut out = Vec::with_capacity(messages.len() - cut.keep_from + 2);
    out.push(messages[0].clone());
    out.push(summary);
    out.extend_from_slice(&messages[cut.keep_from..]);
    out
}

/// The summary request: the messages unchanged plus the instruction.
pub fn summary_request_messages(messages: &[ChatMessage], plan: Option<&str>) -> Vec<ChatMessage> {
    let mut out = messages.to_vec();
    out.push(spec::instruction_message(plan));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::ToolCall;

    fn m(role: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    fn call(id: &str) -> ChatMessage {
        ChatMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: "read_file".into(),
                arguments: "{}".into(),
            }],
            tool_call_id: String::new(),
        }
    }

    fn result(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "tool".into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: id.into(),
        }
    }

    fn est(reported: Option<u64>, since: usize, total: usize) -> ContextEstimate {
        ContextEstimate {
            reported_prompt_tokens: reported,
            bytes_since_report: since,
            total_bytes: total,
            message_bytes: total,
        }
    }

    #[test]
    fn the_trigger_is_seventy_percent_of_the_window_everywhere() {
        let window = Some(1000);
        assert_eq!(decide(&est(Some(699), 0, 0), window, TRIGGER_PERCENT), None);
        assert_eq!(
            decide(&est(Some(700), 0, 0), window, TRIGGER_PERCENT),
            Some("tokens")
        );
        // New bytes count at four per token on top of the reported number.
        assert_eq!(
            decide(&est(Some(600), 400, 0), window, TRIGGER_PERCENT),
            Some("tokens")
        );
        assert_eq!(
            decide(&est(Some(600), 396, 0), window, TRIGGER_PERCENT),
            None
        );
        // Without a report (the start of a follow-up) the whole context is estimated.
        assert_eq!(
            decide(&est(None, 0, 2800), window, TRIGGER_PERCENT),
            Some("tokens")
        );
        assert_eq!(decide(&est(None, 0, 2796), window, TRIGGER_PERCENT), None);
    }

    #[test]
    fn without_a_window_only_the_byte_guard_triggers() {
        assert_eq!(
            decide(&est(None, 0, 10_000_000), None, TRIGGER_PERCENT),
            None
        );
        assert_eq!(
            decide(&est(None, 0, BYTES_TRIGGER), None, TRIGGER_PERCENT),
            Some("bytes")
        );
    }

    #[test]
    fn tracker_estimates_from_the_last_report_and_forgets_on_rewrite() {
        let msgs = vec![m("system", "s"), m("user", "u"), m("assistant", "a")];
        let mut t = UsageTracker::default();
        let before = t.estimate(&msgs, 100);
        assert_eq!(before.reported_prompt_tokens, None);
        t.note(500, 2);
        let after = t.estimate(&msgs, 100);
        assert_eq!(after.reported_prompt_tokens, Some(500));
        assert_eq!(after.bytes_since_report, spec::approx_bytes(&msgs[2]));
        t.reset();
        assert_eq!(t.estimate(&msgs, 100).reported_prompt_tokens, None);
    }

    #[test]
    fn the_cut_keeps_the_last_three_rounds_and_never_starts_on_a_result() {
        let mut msgs = vec![m("system", "s"), m("user", "task")];
        for i in 0..5 {
            msgs.push(call(&format!("c{i}")));
            msgs.push(result(&format!("c{i}"), "ok"));
        }
        let cut = plan_cut(&msgs, Some(1), Some(100_000), KEEP_ROUNDS).unwrap();
        // Five rounds: the cut is at the third from last assistant message.
        assert_eq!(cut.keep_from, 2 + 2 * 2);
        assert_ne!(msgs[cut.keep_from].role, "tool");
        assert_eq!(cut.hint_index() as usize, cut.keep_from - 1);
    }

    #[test]
    fn the_current_user_message_is_kept_when_it_fits() {
        let msgs = vec![
            m("system", "s"),
            m("user", "old"),
            call("c0"),
            result("c0", "ok"),
            m("assistant", "done"),
            m("user", "now"),
        ];
        let cut = plan_cut(&msgs, Some(5), Some(100_000), KEEP_ROUNDS).unwrap();
        assert!(cut.keep_from <= 5);
        assert!(msgs[cut.keep_from..].iter().any(|x| x.content == "now"));
    }

    #[test]
    fn a_tail_over_the_budget_shrinks_to_the_user_message() {
        // Window 1,000 tokens: the tail budget is 250 tokens = 1,000 bytes.
        let msgs = vec![
            m("system", "s"),
            m("user", "task"),
            call("c0"),
            result("c0", &"x".repeat(4000)),
            m("assistant", &"y".repeat(4000)),
            m("user", "next"),
        ];
        let cut = plan_cut(&msgs, Some(5), Some(1000), KEEP_ROUNDS).unwrap();
        assert_eq!(cut.keep_from, 5);
    }

    #[test]
    fn a_cut_in_the_middle_of_a_tool_loop_keeps_the_open_round() {
        let mut msgs = vec![m("system", "s"), m("user", "task")];
        for i in 0..6 {
            msgs.push(call(&format!("c{i}")));
            msgs.push(result(&format!("c{i}"), &"z".repeat(2000)));
        }
        // The last round is a call with its result: both stay in the tail.
        let cut = plan_cut(&msgs, Some(1), Some(100_000), KEEP_ROUNDS).unwrap();
        let last_call = msgs.len() - 2;
        assert!(cut.keep_from <= last_call);
        assert_eq!(msgs[last_call].role, "assistant");
        assert_eq!(msgs[last_call + 1].role, "tool");
        // The summarised part holds whole rounds only.
        for i in 2..cut.keep_from {
            if msgs[i].role == "assistant" {
                assert_eq!(msgs[i + 1].tool_call_id, msgs[i].tool_calls[0].id);
            }
        }
    }

    #[test]
    fn one_huge_result_is_cut_to_the_round_budget() {
        // Window 10,000 tokens: 15% is 1,500 tokens = 6,000 bytes for the round.
        let mut round = vec![call("c0"), result("c0", &"x".repeat(2_000_000))];
        assert_eq!(cap_round_results(&mut round, Some(10_000)), 1);
        assert!(round[1].content.len() < 6_200, "{}", round[1].content.len());
        assert!(round[1].content.contains("[truncated: kept"));
        assert!(round[1].content.contains("of 2000000 bytes"));
        // After a round of at most this size the estimate stays under 95%.
        let added_tokens = round[1].content.len() as u64 / 4;
        assert!((TRIGGER_PERCENT * 100 + added_tokens) < 9_500);
    }

    #[test]
    fn several_results_share_the_budget_and_small_ones_are_untouched() {
        let mut round = vec![
            call("a"),
            result("a", &"x".repeat(100_000)),
            result("b", "tiny"),
            result("c", &"y".repeat(100_000)),
        ];
        assert_eq!(cap_round_results(&mut round, Some(10_000)), 2);
        assert_eq!(round[2].content, "tiny");
        let total: usize = round.iter().map(|m| m.content.len()).sum();
        assert!(total < 6_000 + 1024, "{total}");
    }

    #[test]
    fn a_round_within_budget_or_without_a_window_is_left_alone() {
        let mut round = vec![call("a"), result("a", &"x".repeat(1_000))];
        assert_eq!(cap_round_results(&mut round, Some(10_000)), 0);
        let mut big = vec![call("a"), result("a", &"x".repeat(1_000_000))];
        assert_eq!(cap_round_results(&mut big, None), 0);
        assert_eq!(big[1].content.len(), 1_000_000);
    }

    #[test]
    fn a_changed_threshold_changes_the_trigger() {
        let window = Some(1000);
        let e = est(Some(500), 0, 0);
        assert_eq!(decide(&e, window, 70), None);
        assert_eq!(decide(&e, window, 50), Some("tokens"));
        assert_eq!(decide(&est(Some(899), 0, 0), window, 90), None);
        assert_eq!(decide(&est(Some(900), 0, 0), window, 90), Some("tokens"));
    }

    #[test]
    fn keep_rounds_changes_the_cut() {
        let mut msgs = vec![m("system", "s"), m("user", "task")];
        for i in 0..5 {
            msgs.push(call(&format!("c{i}")));
            msgs.push(result(&format!("c{i}"), "r"));
        }
        let one = plan_cut(&msgs, None, Some(100_000), 1).unwrap();
        let three = plan_cut(&msgs, None, Some(100_000), 3).unwrap();
        assert!(one.keep_from > three.keep_from);
    }

    #[test]
    fn nothing_to_summarise_means_no_cut() {
        assert!(plan_cut(
            &[m("system", "s"), m("user", "u")],
            Some(1),
            Some(1000),
            KEEP_ROUNDS
        )
        .is_none());
        assert!(plan_cut(
            &[m("user", "u"), m("assistant", "a"), m("user", "b")],
            None,
            None,
            KEEP_ROUNDS
        )
        .is_none());
    }

    #[test]
    fn compacted_messages_are_system_summary_and_tail() {
        let msgs = vec![
            m("system", "s"),
            m("user", "use bravo.txt"),
            m("assistant", "ok"),
            m("user", "now"),
        ];
        let cut = Cut { keep_from: 3 };
        let out = compacted_messages(&msgs, cut, "Summary: ok.");
        assert_eq!(out.len(), 3);
        assert_eq!(out[0].role, "system");
        assert!(spec::is_summary_message(&out[1]));
        assert!(out[1].content.contains("use bravo.txt"));
        assert_eq!(out[2].content, "now");
    }

    #[test]
    fn the_summary_request_is_the_prefix_plus_one_instruction() {
        let msgs = vec![m("system", "s"), m("user", "u"), m("assistant", "a")];
        let req = summary_request_messages(&msgs, Some("- [ ] a"));
        assert_eq!(&req[..3], &msgs[..]);
        assert_eq!(req.len(), 4);
        assert!(spec::is_instruction(&req[3]));
        assert!(req[3].content.contains("- [ ] a"));
    }
}
