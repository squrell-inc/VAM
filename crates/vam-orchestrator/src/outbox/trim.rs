//! A tool card whose result is larger than a card may carry keeps the rows that fit.
//!
//! The card's IR binds one `List` to an array of the result (`props.items = {"$data": ptr}`).
//! When the result is over [`MAX_CARD_DATA_BYTES`], the card's data keeps the first rows of that
//! array that fit, and the IR shows "and N more" under the list. The tool result the model gets
//! is never changed; only the card's copy is cut.

use serde_json::{json, Map, Value};
use vam_spec::outbox::MAX_CARD_DATA_BYTES;

/// A result larger than this is not parsed for a card at all (`result_too_large`).
pub(super) const MAX_TRIM_SOURCE_BYTES: usize = 1024 * 1024;

/// The node id of the "and N more" text (another id if the skill's IR already uses it).
const MORE_NODE: &str = "more";
/// The node id of the stack that holds the list and the "more" text.
const TRIM_ROOT_NODE: &str = "trimmed";

/// A card cut to fit: the data and IR to send, and what was cut.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Trimmed {
    pub data: Value,
    pub ir: Value,
    /// The JSON pointer of the array that was cut (`/messages`).
    pub path: String,
    /// Rows in the tool result.
    pub total: usize,
    /// Rows the card keeps.
    pub kept: usize,
}

/// Cut `data` to fit [`MAX_CARD_DATA_BYTES`] by keeping the first rows of the array the IR's
/// `List` binds to. `None` when the IR binds no array of `data` with `$data`, or when not even
/// one row fits (the caller skips the card as `result_too_large`).
pub(super) fn trim_to_fit(ir: &Value, data: &Value) -> Option<Trimmed> {
    let path = list_pointer(ir)?;
    let rows = data.pointer(&path)?.as_array()?;
    let total = rows.len();
    let fits = |k: usize| -> bool {
        let mut d = data.clone();
        if let Some(Value::Array(a)) = d.pointer_mut(&path) {
            a.truncate(k);
        }
        serde_json::to_vec(&d).is_ok_and(|b| b.len() <= MAX_CARD_DATA_BYTES)
    };
    // The largest k in 1..total that fits (k = total does not fit, or the caller had not asked).
    let (mut lo, mut hi) = (0usize, total);
    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if fits(mid) {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    let kept = lo;
    if kept == 0 || !fits(kept) {
        return None;
    }
    let mut cut = data.clone();
    if let Some(Value::Array(a)) = cut.pointer_mut(&path) {
        a.truncate(kept);
    }
    Some(Trimmed {
        data: cut,
        ir: with_more_line(ir, total - kept)?,
        path,
        total,
        kept,
    })
}

/// The `$data` pointer of the first `List` reached from the root (children in order).
fn list_pointer(ir: &Value) -> Option<String> {
    let nodes = ir.get("nodes")?.as_object()?;
    let mut stack = vec![ir.get("root")?.as_str()?.to_string()];
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let node = nodes.get(&id)?;
        if node.get("type").and_then(Value::as_str) == Some("List") {
            let ptr = node.pointer("/props/items/$data")?.as_str()?;
            return Some(ptr.to_string());
        }
        if let Some(children) = node.get("children").and_then(Value::as_array) {
            for c in children.iter().rev().filter_map(Value::as_str) {
                stack.push(c.to_string());
            }
        }
    }
    None
}

/// The IR with its root wrapped in a vertical stack that ends with "and `more` more".
fn with_more_line(ir: &Value, more: usize) -> Option<Value> {
    let mut out = ir.clone();
    let obj = out.as_object_mut()?;
    let old_root = obj.get("root")?.as_str()?.to_string();
    let nodes = obj.get_mut("nodes")?.as_object_mut()?;
    let more_id = free_id(nodes, MORE_NODE);
    nodes.insert(
        more_id.clone(),
        json!({ "type": "Text", "props": { "text": format!("and {more} more"), "tone": "neutral" } }),
    );
    let root_id = free_id(nodes, TRIM_ROOT_NODE);
    nodes.insert(
        root_id.clone(),
        json!({
            "type": "Stack",
            "props": { "direction": "vertical", "gap": "small" },
            "children": [old_root, more_id],
        }),
    );
    obj.insert("root".into(), Value::String(root_id));
    Some(out)
}

/// `base`, or `base_2`, `base_3`, ... if the IR already has that node id.
fn free_id(nodes: &Map<String, Value>, base: &str) -> String {
    if !nodes.contains_key(base) {
        return base.to_string();
    }
    (2..)
        .map(|n| format!("{base}_{n}"))
        .find(|id| !nodes.contains_key(id))
        .unwrap_or_else(|| base.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mail_ir() -> Value {
        json!({
            "version": 1, "catalog": "vam/1", "root": "root", "state": {},
            "nodes": {
                "root": { "type": "List", "props": { "items": { "$data": "/messages" }, "empty": "No messages" }, "children": ["row"] },
                "row": { "type": "Stack", "props": { "direction": "horizontal", "gap": "small" }, "children": ["f0", "f1"] },
                "f0": { "type": "Text", "props": { "text": { "$item": "/from" } } },
                "f1": { "type": "Text", "props": { "text": { "$item": "/subject" } } }
            },
            "inputSchema": {
                "type": "object", "required": ["messages"],
                "properties": { "messages": { "type": "array", "items": { "type": "object",
                    "properties": { "from": { "type": "string" }, "subject": { "type": "string" }, "preview": { "type": "string" } } } } }
            }
        })
    }

    fn mailbox(n: usize, preview: usize) -> Value {
        let rows: Vec<Value> = (1..=n)
            .map(|i| json!({ "from": format!("s{i}@e2e.example"), "subject": format!("Message {i}"), "preview": "p".repeat(preview) }))
            .collect();
        json!({ "messages": rows, "other": "kept" })
    }

    #[test]
    fn an_oversize_list_keeps_the_rows_that_fit_and_says_how_many_more() {
        let data = mailbox(40, 560);
        assert!(serde_json::to_vec(&data).unwrap().len() > MAX_CARD_DATA_BYTES);
        let t = trim_to_fit(&mail_ir(), &data).expect("trimmed");
        assert_eq!(t.path, "/messages");
        assert_eq!(t.total, 40);
        assert!(t.kept > 0 && t.kept < 40, "kept {}", t.kept);
        let size = serde_json::to_vec(&t.data).unwrap().len();
        assert!(size <= MAX_CARD_DATA_BYTES, "{size}");
        // One more row would not have fitted.
        let mut one_more = data.clone();
        one_more["messages"]
            .as_array_mut()
            .unwrap()
            .truncate(t.kept + 1);
        assert!(serde_json::to_vec(&one_more).unwrap().len() > MAX_CARD_DATA_BYTES);
        assert_eq!(t.data["messages"][0]["subject"], "Message 1");
        assert_eq!(t.data["other"], "kept");
        assert_eq!(
            t.ir["nodes"]["more"]["props"]["text"],
            format!("and {} more", 40 - t.kept)
        );
        assert_eq!(t.ir["root"], "trimmed");
        assert_eq!(
            t.ir["nodes"]["trimmed"]["children"],
            json!(["root", "more"])
        );
        // The cut IR is a valid card for the tool's schema, and the cut data fits it.
        vam_spec::jig::check_card(&t.ir, &t.ir["inputSchema"]).expect("cut IR checks");
        vam_spec::jig::check_data_against(&t.ir["inputSchema"], &t.data).expect("cut data fits");
    }

    #[test]
    fn no_bound_list_or_one_row_over_the_limit_cannot_be_trimmed() {
        let mut ir = mail_ir();
        ir["nodes"]["root"] = json!({ "type": "Text", "props": { "text": "x" } });
        assert!(trim_to_fit(&ir, &mailbox(40, 560)).is_none());
        let huge = mailbox(3, MAX_CARD_DATA_BYTES + 1);
        assert!(trim_to_fit(&mail_ir(), &huge).is_none());
    }

    #[test]
    fn node_ids_the_skill_already_uses_are_not_overwritten() {
        let mut ir = mail_ir();
        ir["nodes"]["more"] = json!({ "type": "Text", "props": { "text": "mine" } });
        ir["nodes"]["root"]["children"] = json!(["row"]);
        let t = trim_to_fit(&ir, &mailbox(40, 560)).expect("trimmed");
        assert_eq!(t.ir["nodes"]["more"]["props"]["text"], "mine");
        assert!(t.ir["nodes"]["more_2"]["props"]["text"]
            .as_str()
            .unwrap()
            .ends_with(" more"));
    }

    #[test]
    fn a_list_under_a_stack_is_found() {
        let mut ir = mail_ir();
        let list = ir["nodes"]["root"].clone();
        ir["nodes"]["list"] = list;
        ir["nodes"]["title"] = json!({ "type": "Text", "props": { "text": "Mail" } });
        ir["nodes"]["root"] = json!({ "type": "Stack", "props": { "direction": "vertical" }, "children": ["title", "list"] });
        let t = trim_to_fit(&ir, &mailbox(40, 560)).expect("trimmed");
        assert_eq!(t.path, "/messages");
        vam_spec::jig::check_card(&t.ir, &t.ir["inputSchema"]).expect("cut IR checks");
    }
}
