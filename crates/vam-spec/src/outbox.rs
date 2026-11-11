//! The outbox tools `post_message` and `show` (AR-3.9): names, caps and the
//! one digest the guest and the host both use.
//!
//! The guest posts progress through the port-52 method `PostOutbox`; the host
//! checks everything again. These constants are the shared contract. Nothing
//! here depends on the jig catalog, so the static guest build stays small.

use prost_types::Struct;
use serde_json::Value;

/// Post a short plain-text progress message to the user.
pub const POST_MESSAGE_TOOL: &str = "post_message";
/// Show a built-in Show jig (a card) to the user.
pub const SHOW_TOOL: &str = "show";
/// Ask the user a question and wait for the answer (AR-3.10).
pub const ASK_USER_TOOL: &str = "ask_user";
/// The outbox tools. `ask_user` never posts: the host turns it into a question
/// approval and the run waits for input.
pub const OUTBOX_TOOLS: &[&str] = &[POST_MESSAGE_TOOL, SHOW_TOOL, ASK_USER_TOOL];
/// The built-in jigs `ask_user` may use. `vam.question` is always offered; the
/// others only when the run has `human.jig` (Q11).
pub const ASK_JIGS: &[&str] = &["vam.question", "vam.approve"];
/// The one Ask jig offered without `human.jig`.
pub const ASK_JIG_ALWAYS: &str = "vam.question";
/// Largest answer to a question, as JSON text, in bytes. Checked before the
/// answer is validated or stored.
pub const MAX_ANSWER_BYTES: usize = 16 * 1024;
/// Tool result when the user declines to answer.
pub const DECLINED_ANSWER: &str = "the user declined to answer";
/// The built-in jigs `show` may display. Ask jigs wait for an answer and belong
/// to `ask_user`.
pub const SHOW_JIGS: &[&str] = &["vam.progress"];

/// Longest `post_message` text, in bytes.
pub const MAX_TEXT_BYTES: usize = 4 * 1024;
/// Longest card data, as JSON text, in bytes.
pub const MAX_CARD_DATA_BYTES: usize = 16 * 1024;
/// Accepted posts per run in any 60 seconds.
pub const MAX_POSTS_PER_MINUTE: usize = 60;

/// Start of the tool result for card data that breaks the jig's schema.
pub const INVALID_DATA_PREFIX: &str = "invalid data for ";
/// Start of the tool result when the per-run rate is spent.
pub const RATE_LIMITED_PREFIX: &str = "rate limited";
/// The tool result of a successful post.
pub const POSTED: &str = "posted";

pub fn is_outbox_tool(name: &str) -> bool {
    OUTBOX_TOOLS.contains(&name)
}

/// Catalog digest of an outbox tool (the host catalog entry and the digest the
/// guest sends with the intent must agree).
pub fn tool_digest(name: &str) -> String {
    crate::validation::builtin_tool_digest(name)
}

/// The arguments of a `post_message` call as the intent signed them.
pub fn message_args(text: &str) -> Struct {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "text".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(text.to_string())),
        },
    );
    Struct { fields }
}

/// The arguments of a `show` call as the intent signed them: `jig` and `data`.
/// `None` unless `data` is a JSON object.
pub fn card_args(jig: &str, data: &Value) -> Option<Struct> {
    let data = crate::harness::json_to_struct(data)?;
    let mut fields = std::collections::BTreeMap::new();
    fields.insert(
        "jig".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(jig.to_string())),
        },
    );
    fields.insert(
        "data".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(data)),
        },
    );
    Some(Struct { fields })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args_digest::canonical_args_digest;
    use serde_json::json;

    /// What the guest signs: the model's arguments as a protobuf Struct. Built from
    /// JSON text through serde, like the guest does.
    fn guest_struct(args: &Value) -> Struct {
        crate::harness::json_to_struct(args).expect("object")
    }

    /// What the guest sends in `data_json`, and what the host re-parses.
    fn wire(args: &Struct) -> Value {
        let json = crate::harness::struct_to_json(args);
        serde_json::from_str(&json.to_string()).expect("round trip")
    }

    #[test]
    fn message_digest_is_equal_on_both_sides() {
        for text in [
            "hi",
            "",
            "Starting: 1 of 2",
            "caf\u{e9} \u{1f600} \"quoted\"\n\ttab",
        ] {
            let guest = guest_struct(&json!({ "text": text }));
            let host = message_args(text);
            assert_eq!(
                canonical_args_digest(Some(&guest)).unwrap(),
                canonical_args_digest(Some(&host)).unwrap(),
                "{text:?}"
            );
        }
    }

    #[test]
    fn card_digest_is_equal_for_integers_floats_and_nested_objects() {
        let cases = [
            json!({"label": "Checking", "done": 1, "total": 2}),
            json!({"label": "x", "done": 0, "total": 0, "message": "ok", "card_id": "c1"}),
            json!({"label": "x", "done": 1.5, "total": 2.25}),
            json!({"label": "x", "done": 1.0, "total": 2}),
            json!({"label": "x", "done": 9007199254740993_u64, "total": 1e16}),
            json!({"n": {"a": [1, 2.5, {"b": null, "c": true}], "d": {"e": {"f": -3}}}}),
            json!({"u": "\u{e9}\u{1f600}", "list": [], "empty": {}}),
        ];
        for data in cases {
            // The model's args, as the guest holds them.
            let args = json!({ "jig": "vam.progress", "data": data });
            let guest = guest_struct(&args);
            // The guest sends the data as JSON text; the host re-parses it.
            let sent = wire(&guest_struct(&data));
            let host = card_args("vam.progress", &sent).expect("object");
            assert_eq!(
                canonical_args_digest(Some(&guest)).unwrap(),
                canonical_args_digest(Some(&host)).unwrap(),
                "{data}"
            );
        }
    }

    #[test]
    fn a_changed_value_changes_the_digest() {
        let guest = guest_struct(&json!({"jig": "vam.progress", "data": {"done": 1}}));
        let host = card_args("vam.progress", &json!({"done": 2})).unwrap();
        assert_ne!(
            canonical_args_digest(Some(&guest)).unwrap(),
            canonical_args_digest(Some(&host)).unwrap()
        );
        assert!(card_args("vam.progress", &json!([1])).is_none());
    }

    #[test]
    fn names_and_digests() {
        assert!(is_outbox_tool("post_message") && is_outbox_tool("show"));
        assert!(!is_outbox_tool("run"));
        assert_ne!(tool_digest("show"), tool_digest("post_message"));
    }
}
