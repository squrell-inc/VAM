//! Check `vam run --input key=value` pairs against `PlaybookSpec.input_schema`.
//!
//! The schema is a JSON Schema object stored as a protobuf `Struct`. This
//! covers the object form playbooks use: `properties`, `required`, and
//! `additionalProperties`. Values are the strings from the command line,
//! coerced to the property's `type` (`string`, `integer`, `number`, `boolean`).

use prost_types::value::Kind;
use prost_types::{Struct, Value};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
#[error("{message}")]
pub struct PlaybookInputError {
    pub message: String,
}

impl PlaybookInputError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

/// Build run `inputs` from `KEY=VALUE` pairs.
///
/// An absent or empty schema stores every value as a string. A schema checks
/// required names, property types, and `additionalProperties: false`.
pub fn parse_playbook_inputs(
    schema: Option<&Struct>,
    pairs: &[(String, String)],
) -> Result<Struct, PlaybookInputError> {
    let schema_json = schema
        .map(struct_to_json)
        .unwrap_or(serde_json::Value::Null);
    let schema_obj = schema_json.as_object();
    let constrained = schema_obj.is_some_and(|obj| !obj.is_empty());

    if constrained {
        if let Some(kind) = schema_obj
            .and_then(|obj| obj.get("type"))
            .and_then(|value| value.as_str())
        {
            if kind != "object" {
                return Err(PlaybookInputError::new(
                    "playbook inputSchema.type must be \"object\"",
                ));
            }
        }
    }

    let properties = schema_obj
        .and_then(|obj| obj.get("properties"))
        .and_then(|value| value.as_object());
    let additional_ok = schema_obj
        .and_then(|obj| obj.get("additionalProperties"))
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let required = required_names(schema_obj);

    let mut fields = std::collections::BTreeMap::<String, Value>::new();
    for (key, raw) in pairs {
        if fields.contains_key(key) {
            return Err(PlaybookInputError::new(format!(
                "inputs.{key}: set more than once"
            )));
        }
        let declared = properties.and_then(|props| props.get(key));
        if constrained && declared.is_none() && !additional_ok {
            return Err(PlaybookInputError::new(format!(
                "inputs.{key}: not declared in inputSchema.properties"
            )));
        }
        let expected = declared
            .and_then(|prop| prop.get("type"))
            .and_then(|value| value.as_str());
        if let Some(format) = declared
            .and_then(|prop| prop.get("format"))
            .and_then(|value| value.as_str())
        {
            crate::schema_format::check_format(format, raw)
                .map_err(|err| PlaybookInputError::new(format!("inputs.{key}: {err}")))?;
        }
        fields.insert(key.clone(), coerce(key, raw, expected)?);
    }

    for name in required {
        if !fields.contains_key(&name) {
            return Err(PlaybookInputError::new(format!(
                "inputs.{name}: required by the playbook input schema"
            )));
        }
    }

    Ok(Struct {
        fields: fields.into_iter().collect(),
    })
}

/// Check an already-built inputs object against `PlaybookSpec.input_schema`.
///
/// An absent or empty schema accepts any object. A schema checks `type`,
/// `required`, property types, and `additionalProperties: false`. Values are
/// the typed fields of a protobuf `Struct` (not command-line strings).
pub fn validate_playbook_inputs(
    schema: Option<&Struct>,
    inputs: Option<&Struct>,
) -> Result<(), PlaybookInputError> {
    let inputs_json = inputs
        .map(struct_to_json)
        .unwrap_or_else(|| serde_json::Value::Object(serde_json::Map::new()));
    validate_object_against_schema(
        schema,
        &inputs_json,
        "inputs",
        "playbook inputSchema.type must be \"object\"",
        "playbook inputs must be an object",
        "playbook input schema",
    )
}

/// Check step `output` JSON against `PlaybookStep.output_schema`.
///
/// An absent or empty schema accepts any JSON object (or omitted output).
/// A constrained schema applies the same `properties` / `required` /
/// `additionalProperties` rules as [`validate_playbook_inputs`].
pub fn validate_playbook_step_output(
    schema: Option<&Struct>,
    output_json: Option<&str>,
) -> Result<(), PlaybookInputError> {
    let schema_json = schema
        .map(struct_to_json)
        .unwrap_or(serde_json::Value::Null);
    let constrained = schema_json.as_object().is_some_and(|obj| !obj.is_empty());
    if !constrained {
        return Ok(());
    }

    let output_value = match output_json {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(raw) => {
            let value: serde_json::Value = serde_json::from_str(raw)
                .map_err(|_| PlaybookInputError::new("step output must be a JSON object"))?;
            if !value.is_object() {
                return Err(PlaybookInputError::new("step output must be a JSON object"));
            }
            value
        }
    };

    validate_object_against_schema(
        schema,
        &output_value,
        "output",
        "playbook outputSchema.type must be \"object\"",
        "step output must be a JSON object",
        "playbook step output schema",
    )
}

/// JSON object checked against `PlaybookSpec.output_schema` at run finish.
///
/// An answer that is itself a JSON object is the run output. Any other answer
/// is wrapped as `{ "answer": ... }`, which fails a schema that requires
/// different fields.
pub fn playbook_run_output_json(answer: &str) -> String {
    let trimmed = answer.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if value.is_object() {
            return value.to_string();
        }
    }
    serde_json::json!({ "answer": answer }).to_string()
}

/// Check the run's final output against `PlaybookSpec.output_schema`.
///
/// An absent or empty schema accepts any JSON object. A constrained schema
/// uses the same `properties` / `required` / `additionalProperties` rules as
/// [`validate_playbook_step_output`]. Step completion does not call this.
pub fn validate_playbook_run_output(
    schema: Option<&Struct>,
    output_json: Option<&str>,
) -> Result<(), PlaybookInputError> {
    let schema_json = schema
        .map(struct_to_json)
        .unwrap_or(serde_json::Value::Null);
    let constrained = schema_json.as_object().is_some_and(|obj| !obj.is_empty());
    if !constrained {
        return Ok(());
    }

    let output_value = match output_json {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(raw) => {
            let value: serde_json::Value = serde_json::from_str(raw)
                .map_err(|_| PlaybookInputError::new("run output must be a JSON object"))?;
            if !value.is_object() {
                return Err(PlaybookInputError::new("run output must be a JSON object"));
            }
            value
        }
    };

    validate_object_against_schema(
        schema,
        &output_value,
        "output",
        "playbook outputSchema.type must be \"object\"",
        "run output must be a JSON object",
        "playbook output schema",
    )
}

fn validate_object_against_schema(
    schema: Option<&Struct>,
    value: &serde_json::Value,
    field_prefix: &str,
    bad_schema_type_msg: &str,
    bad_object_msg: &str,
    required_by_msg: &str,
) -> Result<(), PlaybookInputError> {
    let schema_json = schema
        .map(struct_to_json)
        .unwrap_or(serde_json::Value::Null);
    let schema_obj = schema_json.as_object();
    let constrained = schema_obj.is_some_and(|obj| !obj.is_empty());
    if !constrained {
        return Ok(());
    }

    if let Some(kind) = schema_obj
        .and_then(|obj| obj.get("type"))
        .and_then(|value| value.as_str())
    {
        if kind != "object" {
            return Err(PlaybookInputError::new(bad_schema_type_msg));
        }
    }

    let properties = schema_obj
        .and_then(|obj| obj.get("properties"))
        .and_then(|value| value.as_object());
    let additional_ok = schema_obj
        .and_then(|obj| obj.get("additionalProperties"))
        .and_then(|value| value.as_bool())
        .unwrap_or(true);
    let required = required_names(schema_obj);
    let Some(fields) = value.as_object() else {
        return Err(PlaybookInputError::new(bad_object_msg));
    };

    for (key, field_value) in fields {
        let declared = properties.and_then(|props| props.get(key));
        if constrained && declared.is_none() && !additional_ok {
            return Err(PlaybookInputError::new(format!(
                "{field_prefix}.{key}: not declared in schema properties"
            )));
        }
        if let (Some(format), Some(text)) = (
            declared
                .and_then(|prop| prop.get("format"))
                .and_then(|value| value.as_str()),
            field_value.as_str(),
        ) {
            crate::schema_format::check_format(format, text)
                .map_err(|err| PlaybookInputError::new(format!("{field_prefix}.{key}: {err}")))?;
        }
        let Some(expected) = declared
            .and_then(|prop| prop.get("type"))
            .and_then(|value| value.as_str())
        else {
            continue;
        };
        if !value_matches(expected, field_value) {
            return Err(PlaybookInputError::new(format!(
                "{field_prefix}.{key}: expected {expected}, got '{}'",
                describe_value(field_value)
            )));
        }
    }

    for name in required {
        if !fields.contains_key(&name) {
            return Err(PlaybookInputError::new(format!(
                "{field_prefix}.{name}: required by the {required_by_msg}"
            )));
        }
    }

    Ok(())
}

fn value_matches(expected: &str, value: &serde_json::Value) -> bool {
    match expected {
        "string" => value.is_string(),
        // Protobuf structs store JSON numbers as f64, so `14` arrives as `14.0`.
        "integer" => value
            .as_f64()
            .is_some_and(|number| number.is_finite() && number.fract() == 0.0),
        "number" => value.as_f64().is_some_and(|number| number.is_finite()),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        _ => false,
    }
}

fn describe_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn required_names(schema_obj: Option<&serde_json::Map<String, serde_json::Value>>) -> Vec<String> {
    let Some(list) = schema_obj
        .and_then(|obj| obj.get("required"))
        .and_then(|value| value.as_array())
    else {
        return Vec::new();
    };
    list.iter()
        .filter_map(|value| value.as_str().map(str::to_string))
        .collect()
}

fn coerce(key: &str, raw: &str, expected: Option<&str>) -> Result<Value, PlaybookInputError> {
    match expected {
        None | Some("string") => Ok(string_value(raw)),
        Some("integer") => {
            let text = raw.trim();
            let number: i64 = text.parse().map_err(|_| {
                PlaybookInputError::new(format!("inputs.{key}: expected integer, got '{raw}'"))
            })?;
            Ok(number_value(number as f64))
        }
        Some("number") => {
            let number: f64 = raw.trim().parse().map_err(|_| {
                PlaybookInputError::new(format!("inputs.{key}: expected number, got '{raw}'"))
            })?;
            if !number.is_finite() {
                return Err(PlaybookInputError::new(format!(
                    "inputs.{key}: expected number, got '{raw}'"
                )));
            }
            Ok(number_value(number))
        }
        Some("boolean") => match raw.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(bool_value(true)),
            "false" => Ok(bool_value(false)),
            _ => Err(PlaybookInputError::new(format!(
                "inputs.{key}: expected boolean, got '{raw}'"
            ))),
        },
        Some(other) => Err(PlaybookInputError::new(format!(
            "inputs.{key}: inputSchema type \"{other}\" is not a key=value scalar (string, integer, number, boolean)"
        ))),
    }
}

fn string_value(text: &str) -> Value {
    Value {
        kind: Some(Kind::StringValue(text.to_string())),
    }
}

fn number_value(number: f64) -> Value {
    Value {
        kind: Some(Kind::NumberValue(number)),
    }
}

fn bool_value(value: bool) -> Value {
    Value {
        kind: Some(Kind::BoolValue(value)),
    }
}

fn struct_to_json(value: &Struct) -> serde_json::Value {
    let mut map = serde_json::Map::new();
    for (key, field) in &value.fields {
        map.insert(key.clone(), value_to_json(field));
    }
    serde_json::Value::Object(map)
}

fn value_to_json(value: &Value) -> serde_json::Value {
    match &value.kind {
        Some(Kind::NullValue(_)) | None => serde_json::Value::Null,
        Some(Kind::NumberValue(number)) => serde_json::Number::from_f64(*number)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null),
        Some(Kind::StringValue(text)) => serde_json::Value::String(text.clone()),
        Some(Kind::BoolValue(flag)) => serde_json::Value::Bool(*flag),
        Some(Kind::StructValue(nested)) => struct_to_json(nested),
        Some(Kind::ListValue(list)) => {
            serde_json::Value::Array(list.values.iter().map(value_to_json).collect())
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn date_and_file_formats_are_checked_for_inputs_and_cli_pairs() {
        let schema = json_schema(
            r#"{"type":"object","properties":{
                "due":{"type":"string","format":"date"},
                "doc":{"type":"string","format":"vam-workspace-file"}}}"#,
        );
        let ok = parse_playbook_inputs(
            Some(&schema),
            &[
                ("due".into(), "2026-12-01".into()),
                ("doc".into(), "a/b.pdf".into()),
            ],
        )
        .expect("valid");
        assert!(validate_playbook_inputs(Some(&schema), Some(&ok)).is_ok());
        assert!(
            parse_playbook_inputs(Some(&schema), &[("due".into(), "2026-02-30".into())]).is_err()
        );
        let bad = json_to_struct(&serde_json::json!({"doc": "../x"}));
        assert!(validate_playbook_inputs(Some(&schema), Some(&bad)).is_err());
    }

    use super::*;

    fn schema() -> Struct {
        json_schema(
            r#"{
                "type": "object",
                "additionalProperties": false,
                "required": ["project_key", "cutoff_days"],
                "properties": {
                    "project_key": {"type": "string"},
                    "cutoff_days": {"type": "integer"},
                    "dry": {"type": "boolean"}
                }
            }"#,
        )
    }

    fn json_schema(json: &str) -> Struct {
        let value: serde_json::Value = serde_json::from_str(json).unwrap();
        json_to_struct(&value)
    }

    fn json_to_struct(value: &serde_json::Value) -> Struct {
        let serde_json::Value::Object(map) = value else {
            panic!("schema object");
        };
        let mut fields = std::collections::BTreeMap::new();
        for (key, field) in map {
            fields.insert(key.clone(), json_to_value(field));
        }
        Struct { fields }
    }

    fn json_to_value(value: &serde_json::Value) -> Value {
        let kind = match value {
            serde_json::Value::Null => Kind::NullValue(0),
            serde_json::Value::Bool(flag) => Kind::BoolValue(*flag),
            serde_json::Value::Number(number) => Kind::NumberValue(number.as_f64().unwrap()),
            serde_json::Value::String(text) => Kind::StringValue(text.clone()),
            serde_json::Value::Array(items) => Kind::ListValue(prost_types::ListValue {
                values: items.iter().map(json_to_value).collect(),
            }),
            serde_json::Value::Object(_) => Kind::StructValue(json_to_struct(value)),
        };
        Value { kind: Some(kind) }
    }

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items
            .iter()
            .map(|(key, value)| (key.to_string(), value.to_string()))
            .collect()
    }

    #[test]
    fn coerces_string_and_integer_and_rejects_a_bad_integer() {
        let inputs = parse_playbook_inputs(
            Some(&schema()),
            &pairs(&[("project_key", "PROJ"), ("cutoff_days", "14")]),
        )
        .unwrap();
        assert_eq!(
            inputs
                .fields
                .get("project_key")
                .and_then(|v| v.kind.clone()),
            Some(Kind::StringValue("PROJ".into()))
        );
        assert_eq!(
            inputs
                .fields
                .get("cutoff_days")
                .and_then(|v| v.kind.clone()),
            Some(Kind::NumberValue(14.0))
        );

        let err = parse_playbook_inputs(
            Some(&schema()),
            &pairs(&[("project_key", "PROJ"), ("cutoff_days", "14.5")]),
        )
        .unwrap_err();
        assert!(err.message.contains("inputs.cutoff_days"));
        assert!(err.message.contains("integer"));
    }

    #[test]
    fn required_and_unknown_keys_fail() {
        let missing =
            parse_playbook_inputs(Some(&schema()), &pairs(&[("project_key", "PROJ")])).unwrap_err();
        assert!(missing.message.contains("inputs.cutoff_days"));
        assert!(missing.message.contains("required"));

        let extra = parse_playbook_inputs(
            Some(&schema()),
            &pairs(&[
                ("project_key", "PROJ"),
                ("cutoff_days", "1"),
                ("extra", "nope"),
            ]),
        )
        .unwrap_err();
        assert!(extra.message.contains("inputs.extra"));
    }

    #[test]
    fn empty_schema_keeps_strings() {
        let inputs = parse_playbook_inputs(None, &pairs(&[("note", "hello")])).unwrap();
        assert_eq!(
            inputs.fields.get("note").and_then(|v| v.kind.clone()),
            Some(Kind::StringValue("hello".into()))
        );
    }

    #[test]
    fn validate_accepts_coerced_values_and_rejects_a_string_integer() {
        let inputs = parse_playbook_inputs(
            Some(&schema()),
            &pairs(&[("project_key", "PROJ"), ("cutoff_days", "14")]),
        )
        .unwrap();
        validate_playbook_inputs(Some(&schema()), Some(&inputs)).unwrap();

        let mut wrong = inputs.clone();
        wrong
            .fields
            .insert("cutoff_days".to_string(), string_value("14"));
        let err = validate_playbook_inputs(Some(&schema()), Some(&wrong)).unwrap_err();
        assert!(err.message.contains("inputs.cutoff_days"));
        assert!(err.message.contains("integer"));
    }

    #[test]
    fn validate_reports_missing_required_and_unknown_keys() {
        let missing = parse_playbook_inputs(None, &pairs(&[("project_key", "PROJ")])).unwrap();
        let err = validate_playbook_inputs(Some(&schema()), Some(&missing)).unwrap_err();
        assert!(err.message.contains("inputs.cutoff_days"));
        assert!(err.message.contains("required"));

        let mut extra = parse_playbook_inputs(
            Some(&schema()),
            &pairs(&[("project_key", "PROJ"), ("cutoff_days", "1")]),
        )
        .unwrap();
        extra
            .fields
            .insert("extra".to_string(), string_value("nope"));
        let err = validate_playbook_inputs(Some(&schema()), Some(&extra)).unwrap_err();
        assert!(err.message.contains("inputs.extra"));
    }

    #[test]
    fn boolean_property() {
        let inputs = parse_playbook_inputs(
            Some(&schema()),
            &pairs(&[
                ("project_key", "PROJ"),
                ("cutoff_days", "1"),
                ("dry", "true"),
            ]),
        )
        .unwrap();
        assert_eq!(
            inputs.fields.get("dry").and_then(|v| v.kind.clone()),
            Some(Kind::BoolValue(true))
        );
    }

    #[test]
    fn run_output_uses_a_json_answer_and_rejects_a_missing_required_field() {
        assert_eq!(
            playbook_run_output_json("finished"),
            r#"{"answer":"finished"}"#
        );
        let object = playbook_run_output_json(r#"{"report_file_path":"notes/report.md"}"#);
        assert!(object.contains("report_file_path"));
        assert!(!object.contains("\"answer\""));

        let schema = json_schema(
            r#"{"type":"object","additionalProperties":false,"required":["report_file_path"],"properties":{"report_file_path":{"type":"string"}}}"#,
        );
        let missing = validate_playbook_run_output(
            Some(&schema),
            Some(&playbook_run_output_json("finished")),
        )
        .expect_err("prose answer is not the schema");
        assert!(
            missing.message.contains("report_file_path") || missing.message.contains("answer"),
            "{}",
            missing.message
        );
        validate_playbook_run_output(
            Some(&schema),
            Some(&playbook_run_output_json(
                r#"{"report_file_path":"notes/report.md"}"#,
            )),
        )
        .expect("matching run output");
        validate_playbook_step_output(Some(&schema), Some(r#"{"ticket":"nope"}"#))
            .expect_err("step schema checks stay independent of the run-output helper");
    }
}
