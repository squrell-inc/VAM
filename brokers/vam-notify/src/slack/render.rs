//! Slack Block Kit from the neutral rendering (NT-2.1, NT-2.10 broker half).
//!
//! Rules:
//! - Host title and body are ours; the agent's words sit in their own fenced, labelled block.
//! - A button's value is the opaque `delivery_id|action_id`, never a handle.
//! - Inputs come from `jig_ir` only, as input blocks in the message (never a modal: a modal needs
//!   the bot token at click time). When the IR is absent or holds a node Slack cannot show, no
//!   field is drawn and no form-submit button is offered.
//! - "Open in VAM" is a link in a context block (not a button) whenever `open_url` is set; the
//!   CLI line stands in when it is not.

use serde_json::{json, Map, Value};
use vam_spec::harness::struct_to_json;
use vam_spec::v1::{ActionStyle, NotificationRendering};

use crate::adapter::{Capabilities, Offered};

pub const HEADER_MAX: usize = 150;
pub const SECTION_MAX: usize = 3000;
pub const BUTTON_MAX: usize = 75;
pub const OPTION_MAX: usize = 75;
const MAX_ACTIONS: usize = 25;
const MAX_DEPTH: usize = 8;

/// A message ready for `chat.postMessage` or `chat.update`.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    /// The notification fallback text.
    pub text: String,
    pub blocks: Vec<Value>,
    /// The actions the blocks carry, for validating a click later.
    pub offered: Vec<Offered>,
    /// Whether the jig's fields are drawn in the message.
    pub native: bool,
}

/// Slack's mrkdwn control characters.
pub fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}

fn plain_text(s: &str, max: usize) -> Value {
    json!({ "type": "plain_text", "text": clip(s, max), "emoji": false })
}

fn mrkdwn(s: &str) -> Value {
    json!({ "type": "mrkdwn", "text": clip(s, SECTION_MAX) })
}

/// Whether the IR draws natively: every node in the adapter's set, every select within the
/// option cap, every button action a submit.
pub fn renders_natively(ir: &Value, caps: &Capabilities) -> bool {
    let Some(nodes) = ir.get("nodes").and_then(Value::as_object) else {
        return false;
    };
    if nodes.is_empty() {
        return false;
    }
    nodes.values().all(|n| {
        let ty = n.get("type").and_then(Value::as_str).unwrap_or("");
        if !caps.native_primitives.iter().any(|p| p == ty) {
            return false;
        }
        if ty == "Select"
            && n.pointer("/props/options")
                .and_then(Value::as_array)
                .is_some_and(|o| o.len() > caps.max_select_options)
        {
            return false;
        }
        match n.pointer("/props/action") {
            None => true,
            Some(a) => a.get("event").and_then(Value::as_str) == Some("submit"),
        }
    })
}

pub fn render(r: &NotificationRendering, delivery_id: &str, caps: &Capabilities) -> Rendered {
    let ir = r.jig_ir.as_ref().map(struct_to_json);
    let native_ir = ir.filter(|ir| renders_natively(ir, caps));
    let native = native_ir.is_some();

    let mut blocks: Vec<Value> = Vec::new();
    if !r.title.trim().is_empty() {
        blocks.push(json!({ "type": "header", "text": plain_text(&r.title, HEADER_MAX) }));
    }
    if !r.body.trim().is_empty() {
        blocks.push(json!({ "type": "section", "text": mrkdwn(&esc(&r.body)) }));
    }
    if let Some(a) = &r.agent_text {
        // Fenced and labelled: the reader sees whose words these are. A fence in the text is
        // defused here too, whatever `vamd` already did.
        let text = esc(&a.text).replace("```", "'''");
        blocks.push(json!({
            "type": "section",
            "text": mrkdwn(&format!("*{}*\n```{}```", esc(&a.label), text)),
        }));
    }
    if let Some(ir) = &native_ir {
        blocks.extend(native_blocks(ir, caps));
    }

    // Buttons come from the actions vamd offered; the IR's own buttons are not drawn twice. A
    // form submit is offered only when its form is drawn.
    let mut offered = Vec::new();
    let mut buttons = Vec::new();
    for a in &r.actions {
        let submit_only = a.decision == "SUBMITTED" && a.option.is_empty();
        if a.id.is_empty() || a.label.is_empty() || (submit_only && !native) {
            continue;
        }
        if buttons.len() >= MAX_ACTIONS {
            break;
        }
        let mut b = json!({
            "type": "button",
            "action_id": format!("vam_act:{}", a.id),
            "text": plain_text(&a.label, BUTTON_MAX),
            "value": format!("{delivery_id}|{}", a.id),
        });
        match ActionStyle::try_from(a.style) {
            Ok(ActionStyle::Primary) => b["style"] = json!("primary"),
            Ok(ActionStyle::Danger) => b["style"] = json!("danger"),
            _ => {}
        }
        buttons.push(b);
        offered.push(Offered {
            id: a.id.clone(),
            decision: if a.option.is_empty() {
                a.decision.clone()
            } else {
                a.option.clone()
            },
        });
    }
    if !buttons.is_empty() {
        blocks.push(json!({ "type": "actions", "block_id": "vam_actions", "elements": buttons }));
    }

    if let Some(link) = open_link(&r.open_url) {
        blocks.push(
            json!({ "type": "context", "elements": [mrkdwn(&format!("<{link}|Open in VAM>"))] }),
        );
    } else if !r.cli_line.trim().is_empty() {
        let line = r.cli_line.replace('`', "'");
        blocks.push(json!({ "type": "context", "elements": [mrkdwn(&format!("In a terminal: `{}`", esc(&line)))] }));
    }

    let text = [&r.title, &r.body]
        .into_iter()
        .find(|s| !s.trim().is_empty())
        .map(|s| clip(s, 200))
        .unwrap_or_else(|| "VAM notification".to_string());
    Rendered {
        text,
        blocks,
        offered,
        native,
    }
}

/// An http(s) URL that cannot break out of the mrkdwn link.
fn open_link(url: &str) -> Option<String> {
    let u = url.trim();
    let ok = (u.starts_with("https://") || u.starts_with("http://"))
        && !u.chars().any(|c| c.is_whitespace() || c.is_control())
        && !u.contains(['<', '>', '|']);
    ok.then(|| u.to_string())
}

fn str_prop<'a>(node: &'a Value, key: &str) -> Option<&'a str> {
    node.pointer(&format!("/props/{key}"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

fn state_key(node: &Value) -> Option<&str> {
    node.pointer("/props/value/$state")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The block id and action id of a field's input block.
pub const FIELD_PREFIX: &str = "vam_in:";

fn native_blocks(ir: &Value, caps: &Capabilities) -> Vec<Value> {
    let mut out = Vec::new();
    let (Some(root), Some(nodes)) = (
        ir.get("root").and_then(Value::as_str),
        ir.get("nodes").and_then(Value::as_object),
    ) else {
        return out;
    };
    walk(root, nodes, caps, 0, &mut out);
    out.truncate(40);
    out
}

fn walk(
    id: &str,
    nodes: &Map<String, Value>,
    caps: &Capabilities,
    depth: usize,
    out: &mut Vec<Value>,
) {
    let Some(node) = nodes.get(id) else { return };
    if depth > MAX_DEPTH {
        return;
    }
    match node.get("type").and_then(Value::as_str).unwrap_or("") {
        "Stack" => {
            for c in node
                .get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
            {
                walk(c, nodes, caps, depth + 1, out);
            }
        }
        "Text" => {
            if let Some(t) = str_prop(node, "text") {
                let t = esc(t);
                let t = if str_prop(node, "emphasis") == Some("strong") {
                    format!("*{t}*")
                } else {
                    t
                };
                out.push(json!({ "type": "section", "text": mrkdwn(&t) }));
            }
        }
        "Badge" => {
            if let Some(t) = str_prop(node, "label").or_else(|| str_prop(node, "text")) {
                out.push(json!({ "type": "context", "elements": [mrkdwn(&esc(t))] }));
            }
        }
        "KeyValue" => {
            if let (Some(k), Some(v)) = (str_prop(node, "label"), str_prop(node, "value")) {
                out.push(json!({ "type": "section", "text": mrkdwn(&format!("*{}*: {}", esc(k), esc(v))) }));
            }
        }
        ty @ ("Input" | "TextArea" | "Select" | "MultiSelect" | "Toggle" | "DateInput") => {
            if let Some(b) = field_block(ty, node, caps) {
                out.push(b);
            }
        }
        // Button: drawn from the offered actions. Anything else is not native and was refused
        // before this point.
        _ => {}
    }
}

fn options_of(node: &Value, caps: &Capabilities) -> Vec<Value> {
    node.pointer("/props/options")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|o| match o {
            Value::String(s) => Some((s.clone(), s.clone())),
            Value::Object(m) => {
                let value = m.get("value").and_then(Value::as_str)?.to_string();
                let label = m
                    .get("label")
                    .and_then(Value::as_str)
                    .unwrap_or(&value)
                    .to_string();
                Some((label, value))
            }
            _ => None,
        })
        .take(caps.max_select_options)
        .map(|(label, value)| {
            json!({ "text": plain_text(&label, OPTION_MAX), "value": clip(&value, OPTION_MAX) })
        })
        .collect()
}

fn field_block(ty: &str, node: &Value, caps: &Capabilities) -> Option<Value> {
    let key = state_key(node)?;
    let id = format!("{FIELD_PREFIX}{key}");
    let label = str_prop(node, "label").unwrap_or(key);
    let element = match ty {
        "Input" => json!({ "type": "plain_text_input", "action_id": id }),
        "TextArea" => json!({ "type": "plain_text_input", "action_id": id, "multiline": true }),
        "Select" => {
            json!({ "type": "static_select", "action_id": id, "options": options_of(node, caps) })
        }
        "MultiSelect" => {
            json!({ "type": "multi_static_select", "action_id": id, "options": options_of(node, caps) })
        }
        "Toggle" => json!({
            "type": "checkboxes", "action_id": id,
            "options": [{ "text": plain_text(label, OPTION_MAX), "value": "true" }],
        }),
        "DateInput" => json!({ "type": "datepicker", "action_id": id }),
        _ => return None,
    };
    // A select with no options is not a field.
    if matches!(ty, "Select" | "MultiSelect")
        && element["options"].as_array().is_none_or(|a| a.is_empty())
    {
        return None;
    }
    Some(json!({
        "type": "input",
        "block_id": id,
        "optional": true,
        "label": plain_text(label, 2000),
        "element": element,
    }))
}

/// A submit's fields from the `state.values` of a Slack `block_actions` payload: only the
/// fields this broker drew (`vam_in:` blocks). Wave 6 feeds the live payload through this.
pub fn response_from_state(values: &Value) -> Value {
    let mut out = Map::new();
    let Some(blocks) = values.as_object() else {
        return Value::Object(out);
    };
    for (block_id, actions) in blocks {
        let Some(key) = block_id.strip_prefix(FIELD_PREFIX) else {
            continue;
        };
        let Some(el) = actions.as_object().and_then(|a| a.values().next()) else {
            continue;
        };
        let v = match el.get("type").and_then(Value::as_str).unwrap_or("") {
            "plain_text_input" => el.get("value").cloned().unwrap_or(Value::Null),
            "static_select" => el
                .pointer("/selected_option/value")
                .cloned()
                .unwrap_or(Value::Null),
            "multi_static_select" => Value::Array(
                el.get("selected_options")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|o| o.get("value").cloned())
                    .collect(),
            ),
            "checkboxes" => Value::Bool(
                el.get("selected_options")
                    .and_then(Value::as_array)
                    .is_some_and(|a| !a.is_empty()),
            ),
            "datepicker" => el.get("selected_date").cloned().unwrap_or(Value::Null),
            _ => continue,
        };
        out.insert(key.to_string(), v);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::harness::json_to_struct;
    use vam_spec::v1::{AgentText, RenderedAction};

    fn caps() -> Capabilities {
        crate::slack::capabilities()
    }

    fn act(
        id: &str,
        label: &str,
        decision: &str,
        option: &str,
        style: ActionStyle,
    ) -> RenderedAction {
        RenderedAction {
            id: id.into(),
            label: label.into(),
            decision: decision.into(),
            option: option.into(),
            style: style as i32,
        }
    }

    fn count(v: &Value, ty: &str) -> usize {
        match v {
            Value::Object(m) => {
                usize::from(m.get("type").and_then(Value::as_str) == Some(ty))
                    + m.values().map(|x| count(x, ty)).sum::<usize>()
            }
            Value::Array(a) => a.iter().map(|x| count(x, ty)).sum(),
            _ => 0,
        }
    }

    fn all(v: &Value) -> String {
        v.to_string()
    }

    fn approval(open_url: &str) -> NotificationRendering {
        NotificationRendering {
            title: "Approve write_file?".into(),
            body: "A run waits for you.".into(),
            agent_text: Some(AgentText {
                label: "written by researcher, not verified by the host".into(),
                text: "{\"path\":\"a.txt\"}".into(),
            }),
            actions: vec![
                act("approve", "Approve", "APPROVED", "", ActionStyle::Primary),
                act("deny", "Deny", "REJECTED", "", ActionStyle::Danger),
            ],
            open_url: open_url.into(),
            cli_line: "vam approve decide ap1".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_tool_approval_shows_approve_and_deny_only() {
        let r = render(
            &approval("https://vam.example.test/notifications/nt_1"),
            "nd_1",
            &caps(),
        );
        let blocks = Value::Array(r.blocks.clone());
        assert_eq!(count(&blocks, "button"), 2);
        let s = all(&blocks);
        assert!(!s.to_lowercase().contains("always"));
        assert!(s.contains("Approve") && s.contains("Deny"));
        assert!(s.contains("Open in VAM") && !s.contains("In a terminal"));
        assert_eq!(
            r.offered
                .iter()
                .map(|o| (o.id.as_str(), o.decision.as_str()))
                .collect::<Vec<_>>(),
            vec![("approve", "APPROVED"), ("deny", "REJECTED")]
        );
    }

    #[test]
    fn button_values_are_the_delivery_id_and_action_never_a_handle() {
        let r = render(&approval(""), "nd_9", &caps());
        let s = all(&Value::Array(r.blocks));
        assert!(s.contains("nd_9|approve") && s.contains("nd_9|deny"));
        assert!(!s.contains("vam_na_"));
    }

    #[test]
    fn the_cli_line_stands_in_when_there_is_no_open_url() {
        let r = render(&approval(""), "nd_1", &caps());
        let s = all(&Value::Array(r.blocks));
        assert!(!s.contains("Open in VAM"));
        assert!(s.contains("vam approve decide ap1"));
        for bad in ["javascript:alert(1)", "https://x/y|z", "https://x/>y"] {
            let r = render(&approval(bad), "nd_1", &caps());
            assert!(
                !all(&Value::Array(r.blocks)).contains("Open in VAM"),
                "{bad}"
            );
        }
    }

    #[test]
    fn agent_text_is_fenced_labelled_and_cannot_close_the_fence() {
        let mut n = approval("");
        n.agent_text = Some(AgentText {
            label: "written by <x>".into(),
            text: "``` @here <!channel> & more".into(),
        });
        let r = render(&n, "nd_1", &caps());
        let section = r
            .blocks
            .iter()
            .find(|b| {
                b["text"]["text"]
                    .as_str()
                    .is_some_and(|t| t.contains("written by"))
            })
            .unwrap();
        let t = section["text"]["text"].as_str().unwrap();
        assert!(t.starts_with("*written by &lt;x&gt;*\n```"));
        assert_eq!(t.matches("```").count(), 2, "only our own fence: {t}");
        assert!(t.contains("&lt;!channel&gt;") && t.contains("&amp;"));
    }

    fn option_gate() -> NotificationRendering {
        NotificationRendering {
            title: "A gate opened".into(),
            body: "Go on?".into(),
            actions: vec![
                act("opt_0", "go", "SUBMITTED", "go", ActionStyle::Default),
                act("opt_1", "stop", "SUBMITTED", "stop", ActionStyle::Default),
            ],
            open_url: "https://vam.example.test/notifications/nt_2".into(),
            ..Default::default()
        }
    }

    #[test]
    fn an_option_gate_is_one_button_per_option_and_tells_vamd_the_option() {
        let r = render(&option_gate(), "nd_2", &caps());
        let blocks = Value::Array(r.blocks);
        assert_eq!(count(&blocks, "button"), 2);
        assert_eq!(count(&blocks, "input"), 0);
        assert_eq!(r.offered[0].decision, "go");
        assert!(all(&blocks).contains("Open in VAM"));
    }

    fn simple_gate() -> NotificationRendering {
        let ir = json!({
            "version": 1, "catalog": "vam/1", "jig": "vam.gate_default", "root": "root",
            "nodes": {
                "root": { "type": "Stack", "children": ["f_name", "f_kind", "f_ok", "f_when", "send"] },
                "f_name": { "type": "Input", "props": { "label": "name", "value": { "$state": "name" } } },
                "f_kind": { "type": "Select", "props": { "label": "kind", "value": { "$state": "kind" }, "options": ["a", "b"] } },
                "f_ok": { "type": "Toggle", "props": { "label": "ok", "value": { "$state": "ok" } } },
                "f_when": { "type": "DateInput", "props": { "label": "when", "value": { "$state": "when" } } },
                "send": { "type": "Button", "props": { "label": "Submit", "action": { "event": "submit", "with": ["name"] } } }
            }
        });
        NotificationRendering {
            title: "A gate opened".into(),
            body: "Fill this in.".into(),
            jig_ir: json_to_struct(&ir),
            actions: vec![act(
                "submit",
                "Submit",
                "SUBMITTED",
                "",
                ActionStyle::Primary,
            )],
            open_url: "https://vam.example.test/notifications/nt_3".into(),
            ..Default::default()
        }
    }

    #[test]
    fn a_simple_gate_renders_its_fields_natively_as_input_blocks() {
        let r = render(&simple_gate(), "nd_3", &caps());
        assert!(r.native);
        let blocks = Value::Array(r.blocks);
        assert_eq!(count(&blocks, "input"), 4);
        assert_eq!(count(&blocks, "plain_text_input"), 1);
        assert_eq!(count(&blocks, "static_select"), 1);
        assert_eq!(count(&blocks, "checkboxes"), 1);
        assert_eq!(count(&blocks, "datepicker"), 1);
        assert_eq!(
            count(&blocks, "button"),
            1,
            "the IR's own button is not drawn twice"
        );
        assert!(all(&blocks).contains("Open in VAM"));
        assert!(!all(&blocks).contains("\"modal\"") && !all(&blocks).contains("trigger_id"));
        assert_eq!(r.offered.len(), 1);
    }

    #[test]
    fn a_file_gate_renders_no_fields_and_an_open_in_vam_link() {
        // vamd sends no IR and no actions for it; the broker also refuses an IR it cannot draw.
        let file = NotificationRendering {
            title: "A gate opened".into(),
            body: "Which file?".into(),
            open_url: "https://vam.example.test/notifications/nt_4".into(),
            ..Default::default()
        };
        let r = render(&file, "nd_4", &caps());
        let blocks = Value::Array(r.blocks);
        for ty in [
            "button",
            "input",
            "plain_text_input",
            "static_select",
            "file_input",
        ] {
            assert_eq!(count(&blocks, ty), 0, "{ty}");
        }
        assert!(all(&blocks).contains("Open in VAM"));
        assert!(r.offered.is_empty() && !r.native);

        let mut with_ir = simple_gate();
        with_ir.jig_ir = json_to_struct(&json!({
            "root": "root",
            "nodes": {
                "root": { "type": "Stack", "children": ["f"] },
                "f": { "type": "FileInput", "props": { "label": "file", "value": { "$state": "file" } } }
            }
        }));
        let r = render(&with_ir, "nd_5", &caps());
        let blocks = Value::Array(r.blocks);
        assert_eq!(count(&blocks, "input"), 0);
        assert_eq!(
            count(&blocks, "button"),
            0,
            "a submit without its form is not offered"
        );
        assert!(all(&blocks).contains("Open in VAM"));
        assert!(r.offered.is_empty());
    }

    #[test]
    fn a_select_over_the_cap_is_not_native() {
        let options: Vec<String> = (0..101).map(|i| format!("o{i}")).collect();
        let ir = json!({ "root": "s", "nodes": { "s": { "type": "Select", "props": { "options": options, "value": { "$state": "x" } } } } });
        assert!(!renders_natively(&ir, &caps()));
        let ok: Vec<String> = (0..100).map(|i| format!("o{i}")).collect();
        let ir = json!({ "root": "s", "nodes": { "s": { "type": "Select", "props": { "options": ok, "value": { "$state": "x" } } } } });
        assert!(renders_natively(&ir, &caps()));
    }

    #[test]
    fn a_non_submit_button_is_not_native() {
        let ir = json!({ "root": "b", "nodes": { "b": { "type": "Button", "props": { "label": "x", "action": { "event": "query" } } } } });
        assert!(!renders_natively(&ir, &caps()));
    }

    #[test]
    fn state_values_become_the_response_and_only_ours() {
        let values = json!({
            "vam_in:name": { "vam_in:name": { "type": "plain_text_input", "value": "Ann" } },
            "vam_in:kind": { "vam_in:kind": { "type": "static_select", "selected_option": { "value": "a" } } },
            "vam_in:ok": { "vam_in:ok": { "type": "checkboxes", "selected_options": [{ "value": "true" }] } },
            "vam_in:many": { "vam_in:many": { "type": "multi_static_select", "selected_options": [{ "value": "x" }, { "value": "y" }] } },
            "vam_in:when": { "vam_in:when": { "type": "datepicker", "selected_date": "2026-11-04" } },
            "other": { "x": { "type": "plain_text_input", "value": "ignored" } }
        });
        assert_eq!(
            response_from_state(&values),
            json!({ "name": "Ann", "kind": "a", "ok": true, "many": ["x", "y"], "when": "2026-11-04" })
        );
    }

    #[test]
    fn long_text_is_clipped_to_slack_limits() {
        let mut n = approval("");
        n.title = "t".repeat(400);
        n.body = "b".repeat(5000);
        let r = render(&n, "nd_1", &caps());
        assert!(
            r.blocks[0]["text"]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= HEADER_MAX
        );
        assert!(
            r.blocks[1]["text"]["text"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= SECTION_MAX
        );
        assert!(r.text.chars().count() <= 200);
    }
}
