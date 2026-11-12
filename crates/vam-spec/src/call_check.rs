//! Honest tool-call errors (docs: runtime/agent).
//!
//! The guest checks a model's tool arguments before anything runs. Unparsable or
//! non-object arguments are never turned into `{}`; a schema violation is a typed
//! error naming the field and the fix. The checker covers the subset the builtin
//! schemas use (`type`, `required`, `properties`, `enum`, `items`); every other
//! keyword passes, so a call that is valid today stays valid.
//!
//! This is separate from the host's check of MCP arguments and gate responses
//! (`vam-orchestrator`), which is unchanged.
//!
//! The constants are the stable prefixes the host-recorded thread is counted by
//! (H-0.8): tool messages starting [`INVALID_ARGS_PREFIX`] are repairs, user
//! messages starting [`LOOP_OBSERVATION_PREFIX`] are loop warnings, and a run error
//! starting [`LOOP_STOP_PREFIX`] is a loop stop.

use serde_json::Value;

/// Start of every typed tool-call error sent to the model.
pub const INVALID_ARGS_PREFIX: &str = "invalid arguments for ";
/// Start of the one observation a loop detector injects.
pub const LOOP_OBSERVATION_PREFIX: &str = "Loop detected:";
/// Start of the run error when a loop stops the task.
pub const LOOP_STOP_PREFIX: &str = "loop_detected:";
/// Start of the run error when the repair budget is spent.
pub const REPAIR_BUDGET_PREFIX: &str = "repair_budget_exhausted:";

/// Tools whose `path` is also accepted as `filepath` (the builtins accept both).
const PATH_ALIAS_TOOLS: &[&str] = &["read_file", "write_file", "edit_file", "delete_file"];

/// A typed tool-call error: which tool, which field, what was expected, what came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallError {
    pub tool: String,
    /// Empty when the error is about the whole argument string.
    pub field: String,
    pub expected: String,
    pub got: String,
    message: String,
}

impl CallError {
    /// The text sent to the model as the tool result.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// An error for a rule the schema subset cannot express (an unexpected field,
    /// a size cap). Same prefix and shape as every other typed error.
    pub fn custom(tool: &str, field: &str, expected: &str, got: &str, detail: &str) -> Self {
        Self::new(tool, field, expected, got, detail.to_string())
    }

    fn new(tool: &str, field: &str, expected: &str, got: &str, detail: String) -> Self {
        Self {
            tool: tool.to_string(),
            field: field.to_string(),
            expected: expected.to_string(),
            got: got.to_string(),
            message: format!("{INVALID_ARGS_PREFIX}{tool}: {detail}"),
        }
    }
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CallError {}

fn kind_of(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

fn type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "number" => value.is_number(),
        // Protobuf numbers are doubles: a whole number is an integer.
        "integer" => value
            .as_f64()
            .is_some_and(|n| n.is_finite() && n.fract() == 0.0),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// Parse the model's argument string into a JSON object. An empty string is `{}`
/// (some providers send it for a call with no arguments); anything else that is
/// not one JSON object is an error, never `{}`.
pub fn parse_args(tool: &str, raw: &str) -> Result<Value, CallError> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(serde_json::Map::new()));
    }
    match serde_json::from_str::<Value>(raw) {
        Err(_) => Err(CallError::new(
            tool,
            "",
            "a JSON object",
            "not valid JSON",
            "not valid JSON. Send the arguments as one complete JSON object.".to_string(),
        )),
        Ok(value @ Value::Object(_)) => Ok(value),
        Ok(other) => {
            let got = kind_of(&other);
            Err(CallError::new(
                tool,
                "",
                "a JSON object",
                got,
                format!(
                    "arguments must be a JSON object, got {got}. Send the arguments as one \
                     JSON object."
                ),
            ))
        }
    }
}

/// Check parsed arguments against a tool's input schema. `waive_required` skips
/// the top-level `required` list (`task_complete` falls back to the last answer).
/// A schema that is empty or not valid JSON accepts everything.
pub fn check_args(
    tool: &str,
    args: &Value,
    schema_json: &str,
    waive_required: bool,
) -> Result<(), CallError> {
    let Ok(schema) = serde_json::from_str::<Value>(schema_json) else {
        return Ok(());
    };
    if !waive_required {
        if let (Some(required), Some(fields)) = (
            schema.get("required").and_then(Value::as_array),
            args.as_object(),
        ) {
            for name in required.iter().filter_map(Value::as_str) {
                let present = |key: &str| fields.get(key).is_some_and(|v| !v.is_null());
                let aliased =
                    name == "path" && PATH_ALIAS_TOOLS.contains(&tool) && present("filepath");
                if !present(name) && !aliased {
                    let expected = schema
                        .get("properties")
                        .and_then(|p| p.get(name))
                        .and_then(|p| p.get("type"))
                        .and_then(Value::as_str)
                        .unwrap_or("value");
                    return Err(CallError::new(
                        tool,
                        name,
                        expected,
                        "missing",
                        format!(
                            "missing required field '{name}'. Call {tool} again with \
                             \"{name}\" ({expected})."
                        ),
                    ));
                }
            }
        }
    }
    check_value(tool, "", args, &schema, true)
}

fn check_value(
    tool: &str,
    path: &str,
    value: &Value,
    schema: &Value,
    top: bool,
) -> Result<(), CallError> {
    if !top {
        if let Some(options) = schema.get("enum").and_then(Value::as_array) {
            if !options.iter().any(|candidate| candidate == value) {
                let list = options
                    .iter()
                    .map(Value::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(CallError::new(
                    tool,
                    path,
                    &format!("one of [{list}]"),
                    &value.to_string(),
                    format!("field '{path}' must be one of [{list}]"),
                ));
            }
        }
        if let Some(expected) = schema.get("type").and_then(Value::as_str) {
            if !type_matches(expected, value) {
                let got = kind_of(value);
                return Err(CallError::new(
                    tool,
                    path,
                    expected,
                    got,
                    format!("field '{path}' must be {expected}, got {got}"),
                ));
            }
        }
    }
    if let (Some(fields), Some(props)) = (
        value.as_object(),
        schema.get("properties").and_then(Value::as_object),
    ) {
        for (key, field) in fields {
            // Null means absent, as in the host's check.
            if field.is_null() {
                continue;
            }
            if let Some(child) = props.get(key) {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                check_nested_required(tool, &child_path, field, child)?;
                check_value(tool, &child_path, field, child, false)?;
            }
        }
    }
    if let (Some(items), Some(item_schema)) = (value.as_array(), schema.get("items")) {
        for (index, item) in items.iter().enumerate() {
            let child_path = format!("{path}[{index}]");
            check_nested_required(tool, &child_path, item, item_schema)?;
            check_value(tool, &child_path, item, item_schema, false)?;
        }
    }
    Ok(())
}

/// `required` of a nested object schema.
fn check_nested_required(
    tool: &str,
    path: &str,
    value: &Value,
    schema: &Value,
) -> Result<(), CallError> {
    if let (Some(required), Some(fields)) = (
        schema.get("required").and_then(Value::as_array),
        value.as_object(),
    ) {
        for name in required.iter().filter_map(Value::as_str) {
            if fields.get(name).is_none_or(Value::is_null) {
                let field = format!("{path}.{name}");
                return Err(CallError::new(
                    tool,
                    &field,
                    "value",
                    "missing",
                    format!("missing required field '{field}'"),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const READ: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"offset":{"type":"integer"},"limit":{"type":"integer"}},"required":["path"]}"#;
    const WRITE: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"content":{"type":"string"}},"required":["path","content"]}"#;
    const EDIT: &str = r#"{"type":"object","properties":{"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["path","old_string","new_string"]}"#;
    const GLOB: &str = r#"{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]}"#;
    const RUN: &str =
        r#"{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}"#;
    const TODO: &str = r#"{"type":"object","properties":{"todos":{"type":"array","items":{"type":"object","properties":{"id":{"type":"string"},"content":{"type":"string"},"status":{"type":"string"}},"required":["id","content","status"]}}},"required":["todos"]}"#;

    fn check(tool: &str, schema: &str, args: Value) -> Result<(), String> {
        check_args(tool, &args, schema, false).map_err(|e| e.message().to_string())
    }

    #[test]
    fn unparsable_arguments_are_typed_errors() {
        for raw in [r#"{"path": "#, "nope", r#"{"a":1"#] {
            let err = parse_args("read_file", raw).unwrap_err();
            assert!(err
                .message()
                .starts_with("invalid arguments for read_file: not valid JSON"));
            assert_eq!(err.tool, "read_file");
            assert_eq!(err.field, "");
        }
    }

    #[test]
    fn non_object_json_is_a_typed_error() {
        for (raw, got) in [
            ("[]", "array"),
            ("5", "integer"),
            ("\"x\"", "string"),
            ("null", "null"),
            ("true", "boolean"),
        ] {
            let err = parse_args("run", raw).unwrap_err();
            assert_eq!(err.got, got);
            assert!(err
                .message()
                .starts_with("invalid arguments for run: arguments must be a JSON object"));
        }
    }

    #[test]
    fn empty_string_is_an_empty_object_and_objects_pass_through() {
        assert_eq!(parse_args("t", "").unwrap(), json!({}));
        assert_eq!(parse_args("t", "  ").unwrap(), json!({}));
        assert_eq!(
            parse_args("t", r#"{"b":1,"a":2}"#).unwrap(),
            json!({"a":2,"b":1})
        );
    }

    #[test]
    fn missing_required_field_names_the_field_and_the_fix() {
        let msg = check("read_file", READ, json!({})).unwrap_err();
        assert!(msg.starts_with("invalid arguments for read_file: missing required field 'path'"));
        assert!(msg.contains("Call read_file again"));
        let msg = check("write_file", WRITE, json!({"path":"a"})).unwrap_err();
        assert!(msg.contains("missing required field 'content'"));
        let msg = check("edit_file", EDIT, json!({"path":"a","old_string":"x"})).unwrap_err();
        assert!(msg.contains("'new_string'"));
        assert!(check("glob", GLOB, json!({}))
            .unwrap_err()
            .contains("'pattern'"));
        assert!(check("run", RUN, json!({}))
            .unwrap_err()
            .contains("'command'"));
        assert!(check("todo_write", TODO, json!({}))
            .unwrap_err()
            .contains("'todos'"));
    }

    #[test]
    fn null_counts_as_absent() {
        assert!(check("read_file", READ, json!({"path": null})).is_err());
        assert!(check("read_file", READ, json!({"path":"a","offset":null})).is_ok());
    }

    #[test]
    fn wrong_type_is_named() {
        let msg = check("read_file", READ, json!({"path":"a","offset":"5"})).unwrap_err();
        assert_eq!(
            msg,
            "invalid arguments for read_file: field 'offset' must be integer, got string"
        );
        assert!(check("read_file", READ, json!({"path": 3})).is_err());
        assert!(check(
            "edit_file",
            EDIT,
            json!({"path":"a","old_string":"x","new_string":"y","replace_all":"yes"})
        )
        .is_err());
        assert!(check("read_file", READ, json!({"path":"a","offset":1.5})).is_err());
    }

    #[test]
    fn whole_number_floats_are_integers() {
        assert!(check(
            "read_file",
            READ,
            json!({"path":"a","offset":2.0,"limit":10})
        )
        .is_ok());
    }

    #[test]
    fn array_items_are_checked() {
        let ok = json!({"todos":[{"id":"1","content":"c","status":"open"}]});
        assert!(check("todo_write", TODO, ok).is_ok());
        let bad = json!({"todos":[{"id":"1","content":"c"}]});
        let msg = check("todo_write", TODO, bad).unwrap_err();
        assert!(msg.contains("'todos[0].status'"), "{msg}");
        let msg = check("todo_write", TODO, json!({"todos":"x"})).unwrap_err();
        assert!(msg.contains("field 'todos' must be array, got string"));
    }

    #[test]
    fn enum_values_are_checked() {
        let schema =
            r#"{"type":"object","properties":{"mode":{"type":"string","enum":["a","b"]}}}"#;
        assert!(check("t", schema, json!({"mode":"a"})).is_ok());
        let msg = check("t", schema, json!({"mode":"c"})).unwrap_err();
        assert!(
            msg.contains("field 'mode' must be one of [\"a\", \"b\"]"),
            "{msg}"
        );
    }

    #[test]
    fn filepath_alias_satisfies_path_for_path_tools_only() {
        assert!(check("read_file", READ, json!({"filepath":"a"})).is_ok());
        assert!(check("write_file", WRITE, json!({"filepath":"a","content":"c"})).is_ok());
        assert!(check("glob", r#"{"required":["path"]}"#, json!({"filepath":"a"})).is_err());
    }

    #[test]
    fn unknown_keywords_extra_fields_and_bad_schemas_pass() {
        let schema = r#"{"type":"object","properties":{"a":{"type":"string","format":"uri","minLength":9}},"additionalProperties":false}"#;
        assert!(check("t", schema, json!({"a":"x","other":1})).is_ok());
        assert!(check_args("t", &json!({}), "not json", false).is_ok());
        assert!(check_args("t", &json!({}), "", false).is_ok());
    }

    #[test]
    fn waive_required_still_checks_types() {
        let schema = r#"{"type":"object","properties":{"summary":{"type":"string"}},"required":["summary"]}"#;
        assert!(check_args("task_complete", &json!({}), schema, true).is_ok());
        assert!(check_args("task_complete", &json!({"summary":3}), schema, true).is_err());
    }
}
