//! Cards a skill ships for its tools (`ToolSpec.ui`), checked once and kept per session.
//!
//! A card is a JSON IR file `jigs/<name>.json` in the skill package. [`check_package_cards`]
//! runs at publish (a bad card is a 400 naming tool, file and JSON pointer) and again when the
//! host seeds a session's tool catalog ([`seed_session`]). After that, a run reads only the
//! checked IRs kept here, never the materialized files, so a changed file cannot change a card.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{LazyLock, Mutex};

use serde_json::Value;
use vam_spec::v1::SkillManifest;

pub use vam_spec::skill_cards::{
    check_package_cards, skill_name, ToolCard, ToolCards, MAX_CARD_FILE_BYTES,
};

// ---- per-session map ---------------------------------------------------------------------------

static SESSIONS: LazyLock<Mutex<HashMap<String, HashMap<String, ToolCards>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));
const MAX_SESSIONS: usize = 512;

/// Check the cards of every materialized skill and keep them for the session. A skill whose
/// cards fail the check has none (its tools run without cards). Called once, at session start.
pub fn seed_session(session_id: &str, skills_dir: &Path) {
    let mut all: HashMap<String, ToolCards> = HashMap::new();
    if let Ok(entries) = fs::read_dir(skills_dir) {
        for entry in entries.flatten() {
            let root = entry.path();
            let Ok(raw) = fs::read_to_string(root.join("manifest.json")) else {
                continue;
            };
            let Ok(manifest) =
                vam_spec::encoding::json_to_proto::<SkillManifest>("vam.v1.SkillManifest", &raw)
            else {
                continue;
            };
            if manifest.tools.iter().all(|t| t.ui.is_none()) {
                continue;
            }
            match check_package_cards(&root, &manifest) {
                Ok(cards) => all.extend(cards),
                Err(why) => {
                    tracing::warn!("skill cards not offered: {why}");
                    for tool in manifest
                        .tools
                        .iter()
                        .filter(|t| t.ui.as_ref().is_some_and(|u| !u.show.is_empty()))
                    {
                        all.insert(
                            tool.name.clone(),
                            ToolCards {
                                declares_show: true,
                                ..Default::default()
                            },
                        );
                    }
                }
            }
        }
    }
    if let Ok(mut map) = SESSIONS.lock() {
        if map.len() >= MAX_SESSIONS {
            map.clear();
        }
        map.insert(session_id.to_string(), all);
    }
}

/// Drop the session's cards (called where the session's recorder is dropped, at its end).
pub fn forget_session(session_id: &str) {
    if let Ok(mut map) = SESSIONS.lock() {
        map.remove(session_id);
    }
}

/// The fields an ask card's submit buttons send (`props.action.with` of every `submit` action),
/// in first-seen order without repeats.
pub fn submit_fields(ir: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(nodes) = ir.get("nodes").and_then(Value::as_object) else {
        return out;
    };
    for node in nodes.values() {
        let Some(action) = node.pointer("/props/action") else {
            continue;
        };
        if action.get("event").and_then(Value::as_str) != Some("submit") {
            continue;
        }
        for name in action
            .get("with")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if !out.iter().any(|seen| seen == name) {
                out.push(name.to_string());
            }
        }
    }
    out
}

/// The checked cards of a tool, as kept at session start.
pub fn cards_for(session_id: &str, tool: &str) -> Option<ToolCards> {
    SESSIONS
        .lock()
        .ok()?
        .get(session_id)
        .and_then(|m| m.get(tool))
        .cloned()
}

/// The ask card for a call about to suspend: the session's checked ask card for the tool, bound
/// to the requested args (`bind_args` never sets `decision`). `None` for a tool without `ui.ask`,
/// one absent from the session's map, or args that are not an object: those suspend as before.
pub fn ask_approval_card(
    session_id: &str,
    tool: &str,
    args: &Value,
) -> Option<crate::hitl::AskApprovalCard> {
    let ask = cards_for(session_id, tool)?.ask?;
    if !args.is_object() {
        return None;
    }
    Some(crate::hitl::AskApprovalCard {
        jig: ask.jig,
        skill: ask.skill,
        ir: vam_spec::jig::bind_args(&ask.ir, args),
        data: args.clone(),
        submit: submit_fields(&ask.ir),
        input_schema: ask.schema,
    })
}

/// Keep `cards` for a session (tests that need an ask card without a skill package).
#[cfg(test)]
pub(crate) fn seed_for_test(session_id: &str, cards: HashMap<String, ToolCards>) {
    SESSIONS
        .lock()
        .unwrap()
        .insert(session_id.to_string(), cards);
}

#[cfg(test)]
mod tests;

/// Fixtures shared with the outbox tests.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::tests::skill_dir;

    /// A session seeded with a mail skill whose card is good (`good`) or broken.
    pub fn seeded(session: &str, good: bool) -> (String, tempfile::TempDir) {
        let card = good.then_some(super::tests::MAIL_LIST);
        let dir = skill_dir(Some(card.unwrap_or("{}")));
        super::seed_session(session, dir.path());
        (session.to_string(), dir)
    }
}

#[cfg(test)]
mod submit_fields_tests {
    use super::submit_fields;

    #[test]
    fn submit_fields_are_the_with_lists_of_submit_actions_once_each() {
        let ir = serde_json::json!({"nodes": {
            "a": {"type": "Button", "props": {"action": {"event": "submit", "with": ["subject", "cc"]}}},
            "b": {"type": "Button", "props": {"action": {"event": "submit", "with": ["cc", "body"]}}},
            "c": {"type": "Button", "props": {"action": {"event": "set", "with": ["to"]}}}
        }});
        let mut got = submit_fields(&ir);
        got.sort();
        assert_eq!(got, ["body", "cc", "subject"]);
        assert!(submit_fields(&serde_json::json!({})).is_empty());
    }
}
