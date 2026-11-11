//! What the run's harness asks of one OpenAI-compatible request, resolved against the
//! catalog entry (H1 wave 2). Model-specific handling is data on the entry (locked
//! decision 7); this module only reads it.
//!
//! * `adapter.thinking`: `drop` (default) and `strip` keep reasoning out of the next
//!   request; `strip` also has the host keep the text in the thread log. `keep` has the
//!   broker re-attach the reasoning to the assistant message on the host. `off` merges
//!   the entry's `thinking_off` fragment into the body. `catalog` uses the entry's
//!   `thinking`; an entry with no adapter block treats it as `drop`.
//! * `adapter.toolChoice` is sent when it is not `auto`, the entry supports it and the
//!   request has tools. `adapter.effort` goes in the entry's `effort_field` when it is
//!   not `default`.
//! * Anything the run asked for that is not sent or applied is a note, recorded on
//!   `inference.completed`.

use crate::catalog::ModelCatalogEntry;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use vam_spec::harness::Adapter;
use vam_spec::v1::ChatMessage;

/// Largest reasoning text kept per reply (host side). The size recorded is the true size.
pub const MAX_KEPT_REASONING_BYTES: usize = 64 * 1024;
/// Replies whose reasoning the broker remembers for `keep`.
const KEEP_CAPACITY: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ThinkingMode {
    #[default]
    Drop,
    Strip,
    Keep,
    Off,
}

impl ThinkingMode {
    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingMode::Drop => "drop",
            ThinkingMode::Strip => "strip",
            ThinkingMode::Keep => "keep",
            ThinkingMode::Off => "off",
        }
    }
}

/// The resolved request options. `Default` is today's request: nothing added.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WirePlan {
    pub thinking: ThinkingMode,
    pub tool_choice: Option<String>,
    pub effort: Option<(String, String)>,
    pub thinking_off: Option<serde_json::Map<String, serde_json::Value>>,
    /// Field that carries re-attached reasoning (`keep` only).
    pub reasoning_field: Option<String>,
    pub notes: Vec<String>,
}

/// A plan plus the reasoning to re-attach, as `(index into request.messages, text)`.
#[derive(Debug, Clone, Default)]
pub struct WireOptions {
    pub plan: WirePlan,
    pub reattach: Vec<(usize, String)>,
}

pub fn plan_wire(entry: &ModelCatalogEntry, adapter: &Adapter, has_tools: bool) -> WirePlan {
    let spec = entry.adapter.as_ref();
    let mut plan = WirePlan::default();

    let requested = match adapter.thinking.as_str() {
        "catalog" => spec.map(|a| a.thinking.as_str()).unwrap_or("drop"),
        other => other,
    };
    plan.thinking = match requested {
        "strip" => ThinkingMode::Strip,
        "off" => ThinkingMode::Off,
        "keep" => ThinkingMode::Keep,
        _ => ThinkingMode::Drop,
    };
    match plan.thinking {
        ThinkingMode::Keep => match spec.filter(|a| !a.reasoning_field.is_empty()) {
            Some(a) => plan.reasoning_field = Some(a.reasoning_field.clone()),
            None => {
                plan.thinking = ThinkingMode::Drop;
                plan.notes.push("keep_unsupported".into());
            }
        },
        ThinkingMode::Off => {
            let fragment = spec
                .map(|a| a.thinking_off.trim())
                .filter(|f| !f.is_empty())
                .and_then(|f| serde_json::from_str::<serde_json::Value>(f).ok())
                .and_then(|v| match v {
                    serde_json::Value::Object(m) => Some(m),
                    _ => None,
                });
            match fragment {
                Some(m) => plan.thinking_off = Some(m),
                None => plan.notes.push("thinking_off_unsupported".into()),
            }
        }
        _ => {}
    }

    if adapter.tool_choice != "auto" {
        if spec.is_some_and(|a| a.tool_choice) && has_tools {
            plan.tool_choice = Some(adapter.tool_choice.clone());
        } else {
            plan.notes.push("tool_choice_unsupported".into());
        }
    }
    if adapter.effort != "default" {
        match spec
            .map(|a| a.effort_field.as_str())
            .filter(|f| !f.is_empty())
        {
            Some(field) => plan.effort = Some((field.to_string(), adapter.effort.clone())),
            None => plan.notes.push("effort_unsupported".into()),
        }
    }
    plan
}

/// Digest of an assistant message as the broker saw it: text and calls.
pub fn assistant_digest(content: &str, calls: &[(String, String, String)]) -> String {
    let mut h = Sha256::new();
    h.update(content.as_bytes());
    for (id, name, args) in calls {
        for part in [id, name, args] {
            h.update([0u8]);
            h.update(part.as_bytes());
        }
    }
    format!("{:x}", h.finalize())
}

fn message_digest(m: &ChatMessage) -> String {
    let calls: Vec<_> = m
        .tool_calls
        .iter()
        .map(|c| (c.id.clone(), c.name.clone(), c.arguments.clone()))
        .collect();
    assistant_digest(&m.content, &calls)
}

/// Reasoning remembered for `keep`, per session (one broker per session). Keyed by the
/// digest of the reply and by each tool-call id, so a guest that rewrites the text but
/// keeps the calls still matches. Bounded; the oldest entry goes first.
#[derive(Debug, Default)]
pub struct ReasoningStore {
    map: HashMap<String, String>,
    order: VecDeque<String>,
}

impl ReasoningStore {
    pub fn remember(&mut self, content: &str, calls: &[(String, String, String)], text: &str) {
        if text.is_empty() {
            return;
        }
        let mut keys = vec![format!("d:{}", assistant_digest(content, calls))];
        keys.extend(
            calls
                .iter()
                .filter(|(id, _, _)| !id.is_empty())
                .map(|(id, _, _)| format!("i:{id}")),
        );
        for key in keys {
            if self.map.insert(key.clone(), text.to_string()).is_none() {
                self.order.push_back(key);
            }
        }
        while self.order.len() > KEEP_CAPACITY {
            if let Some(old) = self.order.pop_front() {
                self.map.remove(&old);
            }
        }
    }

    fn lookup(&self, m: &ChatMessage) -> Option<&String> {
        self.map
            .get(&format!("d:{}", message_digest(m)))
            .or_else(|| {
                m.tool_calls
                    .iter()
                    .find_map(|c| self.map.get(&format!("i:{}", c.id)))
            })
    }

    /// Reasoning for the assistant messages of `messages` that have a remembered reply,
    /// and how many assistant messages with tool calls had none.
    pub fn reattachments(&self, messages: &[ChatMessage]) -> (Vec<(usize, String)>, usize) {
        let mut found = Vec::new();
        let mut missing = 0;
        for (i, m) in messages.iter().enumerate() {
            if m.role != "assistant" || m.tool_calls.is_empty() {
                continue;
            }
            match self.lookup(m) {
                Some(t) => found.push((i, t.clone())),
                None => missing += 1,
            }
        }
        (found, missing)
    }
}

/// Cuts `text` to at most `max` bytes on a char boundary.
pub fn truncate_utf8(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::AdapterSpec;
    use vam_spec::v1::ToolCall;

    fn entry(spec: Option<AdapterSpec>) -> ModelCatalogEntry {
        ModelCatalogEntry {
            model_name: "fixture".into(),
            adapter: spec,
            ..Default::default()
        }
    }

    fn spec(thinking: &str) -> AdapterSpec {
        AdapterSpec {
            reasoning_field: "reasoning_content".into(),
            thinking: thinking.into(),
            thinking_off: r#"{"thinking":{"type":"disabled"}}"#.into(),
            tool_choice: true,
            effort_field: "reasoning_effort".into(),
        }
    }

    fn adapter(f: impl FnOnce(&mut Adapter)) -> Adapter {
        let mut a = Adapter::default();
        f(&mut a);
        a
    }

    #[test]
    fn defaults_add_nothing_with_or_without_an_adapter_block() {
        for e in [entry(None), entry(Some(spec("keep")))] {
            assert_eq!(
                plan_wire(&e, &Adapter::default(), true),
                WirePlan::default()
            );
        }
    }

    #[test]
    fn catalog_uses_the_entry_and_falls_back_to_drop_without_a_block() {
        let a = adapter(|a| a.thinking = "catalog".into());
        assert_eq!(
            plan_wire(&entry(None), &a, true).thinking,
            ThinkingMode::Drop
        );
        for (t, want) in [
            ("strip", ThinkingMode::Strip),
            ("keep", ThinkingMode::Keep),
            ("off", ThinkingMode::Off),
        ] {
            assert_eq!(plan_wire(&entry(Some(spec(t))), &a, true).thinking, want);
        }
    }

    #[test]
    fn keep_needs_a_field_and_off_needs_a_fragment() {
        let keep = adapter(|a| a.thinking = "keep".into());
        let p = plan_wire(&entry(None), &keep, true);
        assert_eq!(p.thinking, ThinkingMode::Drop);
        assert_eq!(p.notes, ["keep_unsupported"]);
        let p = plan_wire(&entry(Some(spec("strip"))), &keep, true);
        assert_eq!(p.reasoning_field.as_deref(), Some("reasoning_content"));

        let off = adapter(|a| a.thinking = "off".into());
        let p = plan_wire(&entry(Some(spec("strip"))), &off, true);
        assert_eq!(p.thinking, ThinkingMode::Off);
        assert!(p.thinking_off.unwrap().contains_key("thinking"));
        let mut s = spec("strip");
        s.thinking_off = String::new();
        let p = plan_wire(&entry(Some(s)), &off, true);
        assert_eq!(p.thinking, ThinkingMode::Off);
        assert!(p.thinking_off.is_none());
        assert_eq!(p.notes, ["thinking_off_unsupported"]);
    }

    #[test]
    fn tool_choice_and_effort_follow_the_entry() {
        let a = adapter(|a| {
            a.tool_choice = "required".into();
            a.effort = "high".into();
        });
        let p = plan_wire(&entry(Some(spec("strip"))), &a, true);
        assert_eq!(p.tool_choice.as_deref(), Some("required"));
        assert_eq!(p.effort, Some(("reasoning_effort".into(), "high".into())));
        assert!(p.notes.is_empty());
        // No tools in the request: not sent.
        let p = plan_wire(&entry(Some(spec("strip"))), &a, false);
        assert!(p.tool_choice.is_none());
        assert_eq!(p.notes, ["tool_choice_unsupported"]);
        // Unsupported entry.
        let mut s = spec("strip");
        s.tool_choice = false;
        s.effort_field = String::new();
        let p = plan_wire(&entry(Some(s)), &a, true);
        assert!(p.tool_choice.is_none() && p.effort.is_none());
        assert_eq!(p.notes, ["tool_choice_unsupported", "effort_unsupported"]);
        let p = plan_wire(&entry(None), &a, true);
        assert_eq!(p.notes.len(), 2);
    }

    fn call(id: &str) -> ToolCall {
        ToolCall {
            id: id.into(),
            name: "t".into(),
            arguments: "{}".into(),
        }
    }

    #[test]
    fn the_store_matches_by_digest_or_call_id_and_is_bounded() {
        let calls = vec![("c1".to_string(), "t".to_string(), "{}".to_string())];
        let mut store = ReasoningStore::default();
        store.remember("", &calls, "because");
        let msg = |content: &str| ChatMessage {
            role: "assistant".into(),
            content: content.into(),
            tool_calls: vec![call("c1")],
            tool_call_id: String::new(),
        };
        // Exact reply, and a reply the guest rewrote but whose call id is the same.
        for m in [msg(""), msg("rewritten")] {
            let (found, missing) = store.reattachments(&[m]);
            assert_eq!((found, missing), (vec![(0, "because".to_string())], 0));
        }
        let other = ChatMessage {
            tool_calls: vec![call("zz")],
            ..msg("")
        };
        assert_eq!(store.reattachments(&[other]), (vec![], 1));
        for i in 0..100 {
            store.remember("", &[(format!("n{i}"), "t".into(), "{}".into())], "x");
        }
        assert!(store.map.len() <= KEEP_CAPACITY);
        assert_eq!(store.reattachments(&[msg("")]).1, 1);
    }

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_utf8("héllo", 2), "h");
        assert_eq!(truncate_utf8("abc", 10), "abc");
    }
}
