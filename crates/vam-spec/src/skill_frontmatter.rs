//! Publish-time check that `SKILLS.md` frontmatter does not disagree with
//! `manifest.json`. The manifest is the machine contract. The markdown body is
//! what `load_skill` injects. A `tools` list in the frontmatter is optional; when
//! it is present, every restated field has to match the manifest.

use std::collections::BTreeSet;

use crate::v1::tool_spec::ExecutionProvider;
use crate::v1::SkillManifest;
use prost_types::Duration;
use serde_json::Value;

struct DeclaredTool {
    name: String,
    command: Option<String>,
    entrypoint: Option<String>,
    default_args: Option<Vec<String>>,
    required_secrets: Option<Vec<String>>,
    side_effect: Option<i32>,
    timeout: Option<Duration>,
    input_schema: Option<Value>,
    has_exec: bool,
}

pub fn ensure_frontmatter_matches_manifest(
    frontmatter: &str,
    manifest: &SkillManifest,
) -> Result<(), String> {
    let Some(declared) = parse_frontmatter_tools(frontmatter)? else {
        return Ok(());
    };

    let manifest_names: BTreeSet<&str> = manifest
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    let declared_names: BTreeSet<&str> = declared.iter().map(|tool| tool.name.as_str()).collect();
    if manifest_names != declared_names {
        return Err(format!(
            "SKILLS.md frontmatter tools [{declared}] must match manifest.json tools [{manifest}]",
            declared = declared_names.into_iter().collect::<Vec<_>>().join(", "),
            manifest = manifest_names.into_iter().collect::<Vec<_>>().join(", "),
        ));
    }

    for declared_tool in &declared {
        let manifest_tool = manifest
            .tools
            .iter()
            .find(|tool| tool.name == declared_tool.name)
            .ok_or_else(|| {
                format!(
                    "SKILLS.md frontmatter tool '{}' is not in manifest.json",
                    declared_tool.name
                )
            })?;

        if declared_tool.has_exec {
            match &manifest_tool.execution_provider {
                Some(ExecutionProvider::LocalExec(local)) => {
                    if let Some(command) = &declared_tool.command {
                        if command != &local.command {
                            return Err(format!(
                                "SKILLS.md frontmatter tool '{}' command '{command}' must match manifest.json command '{}'",
                                declared_tool.name, local.command
                            ));
                        }
                    }
                    if let Some(entrypoint) = &declared_tool.entrypoint {
                        if entrypoint != &local.entrypoint {
                            return Err(format!(
                                "SKILLS.md frontmatter tool '{}' entrypoint '{entrypoint}' must match manifest.json entrypoint '{}'",
                                declared_tool.name, local.entrypoint
                            ));
                        }
                    }
                    if let Some(default_args) = &declared_tool.default_args {
                        if default_args != &local.default_args {
                            return Err(format!(
                                "SKILLS.md frontmatter tool '{}' default_args must match manifest.json",
                                declared_tool.name
                            ));
                        }
                    }
                }
                _ => {
                    return Err(format!(
                        "SKILLS.md frontmatter tool '{}' restates exec, but manifest.json does not declare localExec",
                        declared_tool.name
                    ));
                }
            }
        }

        if let Some(secrets) = &declared_tool.required_secrets {
            let declared_set: BTreeSet<&str> = secrets.iter().map(String::as_str).collect();
            let manifest_set: BTreeSet<&str> = manifest_tool
                .required_secrets
                .iter()
                .map(|secret| secret.name.as_str())
                .collect();
            if declared_set != manifest_set {
                return Err(format!(
                    "SKILLS.md frontmatter tool '{}' required_secrets must match manifest.json",
                    declared_tool.name
                ));
            }
        }

        if let Some(side_effect) = declared_tool.side_effect {
            if side_effect != manifest_tool.side_effect {
                return Err(format!(
                    "SKILLS.md frontmatter tool '{}' side_effect must match manifest.json",
                    declared_tool.name
                ));
            }
        }

        if let Some(timeout) = &declared_tool.timeout {
            match &manifest_tool.timeout {
                Some(expected)
                    if expected.seconds == timeout.seconds && expected.nanos == timeout.nanos => {}
                _ => {
                    return Err(format!(
                        "SKILLS.md frontmatter tool '{}' timeout must match manifest.json",
                        declared_tool.name
                    ));
                }
            }
        }

        if let Some(schema) = &declared_tool.input_schema {
            let manifest_schema = if manifest_tool.input_schema_json.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&manifest_tool.input_schema_json).map_err(|err| {
                    format!(
                        "manifest.json tool '{}' input_schema_json is not JSON: {err}",
                        declared_tool.name
                    )
                })?
            };
            if schema != &manifest_schema {
                return Err(format!(
                    "SKILLS.md frontmatter tool '{}' input_schema must match manifest.json inputSchemaJson",
                    declared_tool.name
                ));
            }
        }
    }

    Ok(())
}

fn parse_frontmatter_tools(frontmatter: &str) -> Result<Option<Vec<DeclaredTool>>, String> {
    let lines: Vec<&str> = frontmatter.lines().collect();
    let Some(start) = lines.iter().position(|line| is_top_level_tools(line)) else {
        return Ok(None);
    };
    let header = lines[start].trim();
    if header == "tools: []" {
        return Ok(Some(Vec::new()));
    }
    let mut block = Vec::new();
    for line in lines.iter().skip(start + 1) {
        if !line.is_empty() && !line.starts_with(' ') {
            break;
        }
        if line.contains('\t') {
            return Err("SKILLS.md frontmatter tools must be indented with spaces".to_string());
        }
        block.push(*line);
    }
    parse_tool_items(&block).map(Some)
}

fn is_top_level_tools(line: &str) -> bool {
    let trimmed = line.trim_end();
    (trimmed == "tools:" || trimmed == "tools: []") && !line.starts_with(' ')
}

fn parse_tool_items(lines: &[&str]) -> Result<Vec<DeclaredTool>, String> {
    let mut tools = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if lines[index].trim().is_empty() {
            index += 1;
            continue;
        }
        let trimmed = lines[index].trim();
        let Some(rest) = trimmed.strip_prefix("- ") else {
            return Err(format!(
                "SKILLS.md frontmatter tools entry must start with '- name:', got '{trimmed}'"
            ));
        };
        let (key, value) = split_key(rest).ok_or_else(|| {
            format!("SKILLS.md frontmatter tools entry must start with name, got '{trimmed}'")
        })?;
        if key != "name" {
            return Err(format!(
                "SKILLS.md frontmatter tools entry must start with name, got '{key}'"
            ));
        }
        let name = scalar_string(value);
        if name.is_empty() {
            return Err("SKILLS.md frontmatter tool name is empty".to_string());
        }
        let item_indent = indent_of(lines[index]);
        index += 1;
        let mut tool = DeclaredTool {
            name,
            command: None,
            entrypoint: None,
            default_args: None,
            required_secrets: None,
            side_effect: None,
            timeout: None,
            input_schema: None,
            has_exec: false,
        };
        while index < lines.len() {
            if lines[index].trim().is_empty() {
                index += 1;
                continue;
            }
            if indent_of(lines[index]) <= item_indent {
                break;
            }
            let key_indent = indent_of(lines[index]);
            let (key, value) = split_key(lines[index].trim()).ok_or_else(|| {
                format!(
                    "SKILLS.md frontmatter tool '{}' has an unreadable field '{}'",
                    tool.name,
                    lines[index].trim()
                )
            })?;
            index += 1;
            match key {
                "exec" => {
                    tool.has_exec = true;
                    if value.is_empty() {
                        parse_exec(&mut tool, lines, &mut index, key_indent)?;
                    } else {
                        return Err(format!(
                            "SKILLS.md frontmatter tool '{}' exec must be a nested mapping",
                            tool.name
                        ));
                    }
                }
                "required_secrets" => {
                    tool.required_secrets =
                        Some(parse_string_list(value, lines, &mut index, key_indent)?);
                }
                "side_effect" => {
                    tool.side_effect =
                        Some(parse_side_effect(&scalar_string(value)).map_err(|err| {
                            format!("SKILLS.md frontmatter tool '{}' {err}", tool.name)
                        })?);
                }
                "timeout" => {
                    tool.timeout = Some(parse_timeout(&scalar_string(value)).map_err(|err| {
                        format!("SKILLS.md frontmatter tool '{}' {err}", tool.name)
                    })?);
                }
                "input_schema" => {
                    tool.input_schema = Some(if value.is_empty() {
                        parse_nested(lines, &mut index, key_indent)?
                    } else {
                        parse_inline_value(value)?
                    });
                }
                "default_args" => {
                    tool.default_args =
                        Some(parse_string_list(value, lines, &mut index, key_indent)?);
                }
                _ => {
                    if value.is_empty() {
                        let _ = parse_nested(lines, &mut index, key_indent)?;
                    }
                }
            }
        }
        tools.push(tool);
    }
    Ok(tools)
}

fn parse_exec(
    tool: &mut DeclaredTool,
    lines: &[&str],
    index: &mut usize,
    parent_indent: usize,
) -> Result<(), String> {
    while *index < lines.len() {
        if lines[*index].trim().is_empty() {
            *index += 1;
            continue;
        }
        if indent_of(lines[*index]) <= parent_indent {
            break;
        }
        let key_indent = indent_of(lines[*index]);
        let (key, value) = split_key(lines[*index].trim()).ok_or_else(|| {
            format!(
                "SKILLS.md frontmatter tool '{}' exec field '{}' is unreadable",
                tool.name,
                lines[*index].trim()
            )
        })?;
        *index += 1;
        match key {
            "command" => tool.command = Some(scalar_string(value)),
            "entrypoint" => tool.entrypoint = Some(scalar_string(value)),
            "default_args" => {
                tool.default_args = Some(parse_string_list(value, lines, index, key_indent)?);
            }
            _ => {
                if value.is_empty() {
                    let _ = parse_nested(lines, index, key_indent)?;
                }
            }
        }
    }
    Ok(())
}

fn parse_string_list(
    inline: &str,
    lines: &[&str],
    index: &mut usize,
    parent_indent: usize,
) -> Result<Vec<String>, String> {
    if !inline.is_empty() {
        let value = parse_inline_value(inline)?;
        return string_array(value);
    }
    let value = parse_nested(lines, index, parent_indent)?;
    string_array(value)
}

fn string_array(value: Value) -> Result<Vec<String>, String> {
    let Value::Array(items) = value else {
        return Err("SKILLS.md frontmatter expected a list of strings".to_string());
    };
    items
        .into_iter()
        .map(|item| match item {
            Value::String(text) => Ok(text),
            other => Err(format!(
                "SKILLS.md frontmatter expected a string list entry, got {other}"
            )),
        })
        .collect()
}

fn parse_nested(lines: &[&str], index: &mut usize, parent_indent: usize) -> Result<Value, String> {
    while *index < lines.len() && lines[*index].trim().is_empty() {
        *index += 1;
    }
    if *index >= lines.len() || indent_of(lines[*index]) <= parent_indent {
        return Ok(Value::Null);
    }
    let indent = indent_of(lines[*index]);
    parse_yaml_at(lines, index, indent)
}

fn parse_yaml_at(lines: &[&str], index: &mut usize, indent: usize) -> Result<Value, String> {
    while *index < lines.len() && lines[*index].trim().is_empty() {
        *index += 1;
    }
    if *index >= lines.len() {
        return Ok(Value::Null);
    }
    if lines[*index].trim().starts_with("- ") {
        return parse_sequence(lines, index, indent);
    }
    parse_mapping(lines, index, indent)
}

fn parse_sequence(lines: &[&str], index: &mut usize, indent: usize) -> Result<Value, String> {
    let mut items = Vec::new();
    while *index < lines.len() {
        if lines[*index].trim().is_empty() {
            *index += 1;
            continue;
        }
        if indent_of(lines[*index]) != indent || !lines[*index].trim().starts_with("- ") {
            break;
        }
        let rest = lines[*index].trim().strip_prefix("- ").unwrap_or("");
        *index += 1;
        if rest.is_empty() {
            items.push(parse_nested(lines, index, indent)?);
        } else if split_key(rest).is_some() {
            items.push(parse_mapping_item(rest, lines, index, indent)?);
        } else {
            items.push(parse_inline_value(rest)?);
        }
    }
    Ok(Value::Array(items))
}

fn parse_mapping_item(
    first: &str,
    lines: &[&str],
    index: &mut usize,
    seq_indent: usize,
) -> Result<Value, String> {
    let mut map = serde_json::Map::new();
    insert_mapping_entry(&mut map, first, lines, index, seq_indent)?;
    while *index < lines.len() {
        if lines[*index].trim().is_empty() {
            *index += 1;
            continue;
        }
        let line_indent = indent_of(lines[*index]);
        if line_indent <= seq_indent
            || lines[*index].trim().starts_with("- ") && line_indent == seq_indent
        {
            break;
        }
        let raw = lines[*index].trim().to_string();
        *index += 1;
        insert_mapping_entry(&mut map, &raw, lines, index, line_indent)?;
    }
    Ok(Value::Object(map))
}

fn parse_mapping(lines: &[&str], index: &mut usize, indent: usize) -> Result<Value, String> {
    let mut map = serde_json::Map::new();
    while *index < lines.len() {
        if lines[*index].trim().is_empty() {
            *index += 1;
            continue;
        }
        if indent_of(lines[*index]) != indent || lines[*index].trim().starts_with("- ") {
            break;
        }
        let raw = lines[*index].trim().to_string();
        *index += 1;
        insert_mapping_entry(&mut map, &raw, lines, index, indent)?;
    }
    Ok(Value::Object(map))
}

fn insert_mapping_entry(
    map: &mut serde_json::Map<String, Value>,
    raw: &str,
    lines: &[&str],
    index: &mut usize,
    key_indent: usize,
) -> Result<(), String> {
    let (key, value) = split_key(raw)
        .ok_or_else(|| format!("SKILLS.md frontmatter field '{raw}' is unreadable"))?;
    let parsed = if value.is_empty() {
        parse_nested(lines, index, key_indent)?
    } else {
        parse_inline_value(value)?
    };
    map.insert(key.to_string(), parsed);
    Ok(())
}

fn parse_inline_value(raw: &str) -> Result<Value, String> {
    let raw = raw.trim();
    if raw.starts_with('{') || raw.starts_with('[') {
        return serde_json::from_str(raw)
            .map_err(|err| format!("SKILLS.md frontmatter value '{raw}' is not JSON: {err}"));
    }
    Ok(scalar_value(raw))
}

fn scalar_value(raw: &str) -> Value {
    let text = scalar_string(raw);
    if text == "true" {
        return Value::Bool(true);
    }
    if text == "false" {
        return Value::Bool(false);
    }
    if text == "null" || text == "~" {
        return Value::Null;
    }
    if let Ok(number) = text.parse::<i64>() {
        return Value::Number(number.into());
    }
    if let Ok(number) = text.parse::<u64>() {
        return Value::Number(number.into());
    }
    Value::String(text)
}

fn scalar_string(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 {
        let bytes = raw.as_bytes();
        if (bytes[0] == b'"' && bytes[raw.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[raw.len() - 1] == b'\'')
        {
            return raw[1..raw.len() - 1].to_string();
        }
    }
    raw.to_string()
}

fn split_key(raw: &str) -> Option<(&str, &str)> {
    let mut quote = None;
    for (index, ch) in raw.char_indices() {
        if let Some(open) = quote {
            if ch == open {
                quote = None;
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch == ':' {
            let key = raw[..index].trim();
            if key.is_empty() {
                return None;
            }
            return Some((key, raw[index + 1..].trim()));
        }
    }
    None
}

fn indent_of(line: &str) -> usize {
    line.chars().take_while(|ch| *ch == ' ').count()
}

fn parse_side_effect(raw: &str) -> Result<i32, String> {
    match raw {
        "UNSPECIFIED" | "SIDE_EFFECT_CLASS_UNSPECIFIED" => Ok(0),
        "PURE" | "READ_ONLY" | "SIDE_EFFECT_CLASS_READ_ONLY" => Ok(1),
        "MUTABLE_LOCAL" | "SIDE_EFFECT_CLASS_MUTABLE_LOCAL" => Ok(2),
        "MUTABLE_EXTERNAL" | "SIDE_EFFECT_CLASS_MUTABLE_EXTERNAL" => Ok(3),
        other => other
            .parse::<i32>()
            .map_err(|_| format!("side_effect '{other}' does not match a manifest side effect")),
    }
}

fn parse_timeout(raw: &str) -> Result<Duration, String> {
    if let Some(millis) = raw.strip_suffix("ms") {
        let millis: i64 = millis
            .parse()
            .map_err(|_| format!("timeout '{raw}' is not a duration"))?;
        return Ok(Duration {
            seconds: millis / 1000,
            nanos: ((millis % 1000) * 1_000_000) as i32,
        });
    }
    if let Some(seconds) = raw.strip_suffix('s') {
        let seconds: i64 = seconds
            .parse()
            .map_err(|_| format!("timeout '{raw}' is not a duration"))?;
        return Ok(Duration { seconds, nanos: 0 });
    }
    Err(format!("timeout '{raw}' is not a duration"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_schema_parses_to_manifest_json() {
        let frontmatter = r#"
tools:
  - name: "echo_tool"
    input_schema:
      type: "object"
      properties:
        q:
          type: "string"
      required:
        - "q"
"#;
        let tools = parse_frontmatter_tools(frontmatter)
            .expect("parse")
            .expect("tools");
        let schema = tools[0].input_schema.as_ref().expect("schema");
        let expected: Value = serde_json::from_str(
            r#"{"type":"object","properties":{"q":{"type":"string"}},"required":["q"]}"#,
        )
        .expect("json");
        assert_eq!(schema, &expected);
    }
}
