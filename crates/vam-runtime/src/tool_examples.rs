//! One worked example per guest-defined tool (`tools.examples`, H-1.7).
//!
//! With the switch on, the description of each builtin, the `task_complete`
//! intrinsic and the playbook tools ends with `Example: name({json})`. MCP and
//! host tools are unchanged. With the switch off nothing here runs and the
//! descriptions, and so the cache prefix, are byte-identical.

use std::borrow::Cow;
use vam_spec::v1::ToolSpec;

/// The example arguments of a tool this crate defines, as JSON text.
pub fn example_args(tool: &str) -> Option<&'static str> {
    Some(match tool {
        "read_file" => r#"{"path":"src/main.rs","limit":50}"#,
        "write_file" => r#"{"path":"notes/todo.txt","content":"first line\n"}"#,
        "edit_file" => r#"{"path":"src/lib.rs","old_string":"foo()","new_string":"bar()"}"#,
        "delete_file" => r#"{"path":"tmp/old.log"}"#,
        "glob" => r#"{"pattern":"src/**/*.rs"}"#,
        "grep" => r#"{"pattern":"fn main","path":"src"}"#,
        "load_skill" => r#"{"skill":"assistant-tools"}"#,
        "run" => r#"{"command":"ls -la"}"#,
        "todo_write" => r#"{"todos":[{"id":"1","content":"Read the file","status":"pending"}]}"#,
        "task_complete" => r#"{"summary":"Saved the note to notes.txt."}"#,
        "start_playbook" => r#"{"urn":"urn:vam:playbook:example"}"#,
        "step_complete" => r#"{"step_id":"s1","outcome":"done"}"#,
        "run_step" => r#"{"step_id":"s1"}"#,
        _ => return None,
    })
}

/// The description with its example appended; unchanged for a tool without one.
pub fn describe(tool: &str, description: &str) -> String {
    match example_args(tool) {
        Some(args) => format!("{description}\nExample: {tool}({args})"),
        None => description.to_string(),
    }
}

/// The tool list as the model should see it. Borrowed untouched when `on` is false.
pub fn for_request(tools: &[ToolSpec], on: bool) -> Cow<'_, [ToolSpec]> {
    if !on {
        return Cow::Borrowed(tools);
    }
    Cow::Owned(
        tools
            .iter()
            .map(|t| {
                let mut t = t.clone();
                t.description = describe(&t.name, &t.description);
                t
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills;

    fn every_example_tool() -> Vec<ToolSpec> {
        let mut tools = crate::primitives::builtin_tool_specs();
        tools.push(skills::task_complete_tool_spec());
        tools.push(skills::start_playbook_tool_spec());
        tools.push(skills::step_complete_tool_spec());
        tools.push(skills::run_step_tool_spec());
        tools
    }

    #[test]
    fn every_guest_tool_has_one_example_that_is_valid_against_its_own_schema() {
        for spec in every_example_tool() {
            let args =
                example_args(&spec.name).unwrap_or_else(|| panic!("{} has no example", spec.name));
            let value: serde_json::Value = serde_json::from_str(args)
                .unwrap_or_else(|e| panic!("{}: example is not JSON: {e}", spec.name));
            assert!(value.is_object(), "{}", spec.name);
            vam_spec::call_check::check_args(&spec.name, &value, &spec.input_schema_json, false)
                .unwrap_or_else(|e| panic!("{}: {}", spec.name, e.message()));
            // An unknown property would be a typo in the example.
            let schema: serde_json::Value = serde_json::from_str(&spec.input_schema_json).unwrap();
            let props = schema["properties"].as_object().unwrap();
            for key in value.as_object().unwrap().keys() {
                assert!(
                    props.contains_key(key),
                    "{}: unknown field {key}",
                    spec.name
                );
            }
        }
    }

    #[test]
    fn on_appends_exactly_one_example_and_off_is_byte_identical() {
        let tools = every_example_tool();
        let off = for_request(&tools, false);
        assert!(matches!(off, Cow::Borrowed(_)));
        assert_eq!(&*off, &tools[..]);
        let on = for_request(&tools, true);
        for (before, after) in tools.iter().zip(on.iter()) {
            assert_eq!(after.name, before.name);
            assert_eq!(after.input_schema_json, before.input_schema_json);
            assert_eq!(after.description.matches("Example:").count(), 1);
            assert!(after.description.starts_with(&before.description));
        }
    }

    #[test]
    fn tools_without_an_example_are_left_alone() {
        let host = ToolSpec {
            name: "some_mcp_tool".into(),
            description: "does a thing".into(),
            ..Default::default()
        };
        let on = for_request(std::slice::from_ref(&host), true);
        assert_eq!(on[0], host);
    }
}
