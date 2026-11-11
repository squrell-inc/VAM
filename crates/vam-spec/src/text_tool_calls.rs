//! Tool calls a model wrote as JSON in its reply text instead of as native
//! calls. One parser for host and guest: the host recorder normalizes a logged
//! response exactly as the guest does, so the log equals the next request.

use crate::thread_context::WireToolCall;

pub fn extract_tool_calls_from_text(text: &str) -> Vec<WireToolCall> {
    let mut calls = Vec::new();
    let mut search_from = 0usize;
    let mut call_idx = 0usize;
    while search_from < text.len() {
        let Some(rel_start) = text[search_from..].find('{') else {
            break;
        };
        let start = search_from + rel_start;
        let Some((json_str, consumed)) = extract_balanced_json_object(&text[start..]) else {
            search_from = start + 1;
            continue;
        };
        if let Some(tc) = parse_text_tool_json(json_str, call_idx) {
            calls.push(tc);
            call_idx += 1;
        }
        search_from = start + consumed;
    }
    calls
}

fn extract_balanced_json_object(input: &str) -> Option<(&str, usize)> {
    let bytes = input.as_bytes();
    if *bytes.first()? != b'{' {
        return None;
    }
    let mut depth = 0i32;
    let mut in_string = false;
    let mut escape = false;
    for (i, &b) in bytes.iter().enumerate() {
        if in_string {
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&input[..=i], i + 1));
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_text_tool_json(json_str: &str, idx: usize) -> Option<WireToolCall> {
    let val: serde_json::Value = serde_json::from_str(json_str).ok()?;
    let obj = val.as_object()?;

    if let Some(cmd) = obj.get("cmd").and_then(|v| v.as_array()) {
        let name = cmd.first()?.as_str()?.to_string();
        let args = cmd_array_to_arguments(&name, &cmd[1..]);
        return Some(WireToolCall {
            id: format!("text_call_{idx}"),
            name,
            arguments: args,
        });
    }

    let tool = obj
        .get("tool")
        .or_else(|| obj.get("name"))
        .and_then(|v| v.as_str())?;
    let name = tool.to_string();

    if let Some(tool_input) = obj.get("tool_input") {
        let args = match tool_input {
            serde_json::Value::String(s) => {
                cmd_array_to_arguments(&name, &[serde_json::Value::String(s.clone())])
            }
            serde_json::Value::Object(_) => {
                let mut args = tool_input.clone();
                normalize_tool_arguments(&name, &mut args);
                args.to_string()
            }
            _ => "{}".to_string(),
        };
        return Some(WireToolCall {
            id: format!("text_call_{idx}"),
            name,
            arguments: args,
        });
    }

    let mut args = obj
        .get("arguments")
        .or_else(|| obj.get("args"))
        .or_else(|| obj.get("parameters"))
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    if !args.is_object() {
        args = serde_json::json!({});
    }
    normalize_tool_arguments(&name, &mut args);
    Some(WireToolCall {
        id: format!("text_call_{idx}"),
        name,
        arguments: args.to_string(),
    })
}

pub fn normalize_tool_arguments(tool: &str, args: &mut serde_json::Value) {
    let Some(obj) = args.as_object_mut() else {
        return;
    };
    if tool == "load_skill" {
        if let Some(catalog_id) = obj.remove("catalog_id") {
            obj.entry("skill".to_string()).or_insert(catalog_id);
        }
        if let Some(skill_id) = obj.remove("skill_id") {
            obj.entry("skill".to_string()).or_insert(skill_id);
        }
    }
}

pub fn cmd_array_to_arguments(tool: &str, tail: &[serde_json::Value]) -> String {
    match tool {
        "load_skill" => {
            let skill = tail.first().and_then(|v| v.as_str()).unwrap_or_default();
            serde_json::json!({ "skill": skill }).to_string()
        }
        "read_file" | "glob" | "grep" | "delete_file" => {
            let path = tail.first().and_then(|v| v.as_str()).unwrap_or_default();
            serde_json::json!({ "path": path }).to_string()
        }
        "write_file" | "edit_file" => {
            let path = tail.first().and_then(|v| v.as_str()).unwrap_or_default();
            let content = tail.get(1).and_then(|v| v.as_str()).unwrap_or_default();
            serde_json::json!({ "path": path, "content": content }).to_string()
        }
        _ => {
            if tail.is_empty() {
                "{}".to_string()
            } else if tail.len() == 1 {
                serde_json::json!({ "input": tail[0] }).to_string()
            } else {
                serde_json::json!({ "args": tail }).to_string()
            }
        }
    }
}
