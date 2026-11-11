//! One canonical arguments digest for host and guest.
//!
//! The digest binds a capability token to the call that was authorized. Every
//! JSON-representable protobuf value is included, including nested objects,
//! arrays, null, and empty structs. Object keys are sorted. A non-finite
//! number or a value with no kind set cannot be encoded, so those calls fail
//! instead of being signed with the field omitted.

use prost_types::value::Kind;
use prost_types::{Struct, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArgsDigestError {
    NonFiniteNumber { path: String },
    MissingKind { path: String },
}

impl std::fmt::Display for ArgsDigestError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NonFiniteNumber { path } => write!(
                f,
                "non-finite number at {path} cannot be covered by the arguments digest"
            ),
            Self::MissingKind { path } => write!(
                f,
                "protobuf value at {path} has no kind and cannot be covered by the arguments digest"
            ),
        }
    }
}

impl std::error::Error for ArgsDigestError {}

/// Canonical JSON for `args`. `None` is JSON null. An empty struct is `{}`.
pub fn canonical_args_json(args: Option<&Struct>) -> Result<serde_json::Value, ArgsDigestError> {
    match args {
        None => Ok(serde_json::Value::Null),
        Some(fields) => struct_to_json(fields, "$"),
    }
}

/// SHA-256 hex of [`canonical_args_json`].
pub fn canonical_args_digest(args: Option<&Struct>) -> Result<String, ArgsDigestError> {
    let json = canonical_args_json(args)?;
    Ok(sha256_hex(json.to_string().as_bytes()))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    format!("{:x}", hasher.finalize())
}

fn struct_to_json(fields: &Struct, path: &str) -> Result<serde_json::Value, ArgsDigestError> {
    let mut map = serde_json::Map::new();
    for (key, value) in &fields.fields {
        let child = format!("{path}.{key}");
        map.insert(key.clone(), value_to_json(value, &child)?);
    }
    Ok(serde_json::Value::Object(map))
}

fn value_to_json(value: &Value, path: &str) -> Result<serde_json::Value, ArgsDigestError> {
    let kind = value
        .kind
        .as_ref()
        .ok_or_else(|| ArgsDigestError::MissingKind {
            path: path.to_string(),
        })?;
    match kind {
        Kind::NullValue(_) => Ok(serde_json::Value::Null),
        Kind::NumberValue(n) => {
            let number = serde_json::Number::from_f64(*n).ok_or_else(|| {
                ArgsDigestError::NonFiniteNumber {
                    path: path.to_string(),
                }
            })?;
            Ok(serde_json::Value::Number(number))
        }
        Kind::StringValue(s) => Ok(serde_json::Value::String(s.clone())),
        Kind::BoolValue(b) => Ok(serde_json::Value::Bool(*b)),
        Kind::StructValue(nested) => struct_to_json(nested, path),
        Kind::ListValue(list) => {
            let mut items = Vec::with_capacity(list.values.len());
            for (index, item) in list.values.iter().enumerate() {
                items.push(value_to_json(item, &format!("{path}[{index}]"))?);
            }
            Ok(serde_json::Value::Array(items))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::value::Kind;
    use prost_types::{ListValue, Struct, Value};

    fn string(s: &str) -> Value {
        Value {
            kind: Some(Kind::StringValue(s.to_string())),
        }
    }

    fn number(n: f64) -> Value {
        Value {
            kind: Some(Kind::NumberValue(n)),
        }
    }

    fn boolean(b: bool) -> Value {
        Value {
            kind: Some(Kind::BoolValue(b)),
        }
    }

    fn null() -> Value {
        Value {
            kind: Some(Kind::NullValue(0)),
        }
    }

    fn list(values: Vec<Value>) -> Value {
        Value {
            kind: Some(Kind::ListValue(ListValue { values })),
        }
    }

    fn structure(fields: Vec<(&str, Value)>) -> Value {
        Value {
            kind: Some(Kind::StructValue(struct_of(fields))),
        }
    }

    fn struct_of(fields: Vec<(&str, Value)>) -> Struct {
        let mut out = Struct::default();
        for (key, value) in fields {
            out.fields.insert(key.to_string(), value);
        }
        out
    }

    fn digest(fields: Vec<(&str, Value)>) -> String {
        canonical_args_digest(Some(&struct_of(fields))).expect("canonical digest")
    }

    #[test]
    fn covers_arrays_nested_objects_null_and_empty_struct() {
        let args = struct_of(vec![
            (
                "items",
                list(vec![
                    string("a"),
                    number(2.0),
                    boolean(false),
                    null(),
                    structure(vec![("id", number(1.0))]),
                ]),
            ),
            (
                "nested",
                structure(vec![("inner", structure(vec![("n", number(3.0))]))]),
            ),
            ("missing", null()),
            ("empty", structure(vec![])),
        ]);

        let json = canonical_args_json(Some(&args)).expect("canonical json");
        assert_eq!(
            json,
            serde_json::json!({
                "empty": {},
                "items": ["a", 2.0, false, null, {"id": 1.0}],
                "missing": null,
                "nested": {"inner": {"n": 3.0}}
            })
        );
        let encoded = json.to_string();
        assert_eq!(
            canonical_args_digest(Some(&args)).expect("digest"),
            sha256_hex(encoded.as_bytes())
        );
        assert!(encoded.contains("\"empty\":{}"));
        assert!(encoded.contains("\"missing\":null"));
        assert!(encoded.contains("[\"a\",2.0,false,null,{\"id\":1.0}]"));
    }

    #[test]
    fn null_args_and_empty_struct_differ() {
        let absent = canonical_args_digest(None).expect("null args");
        let empty = digest(vec![]);
        assert_eq!(canonical_args_json(None).unwrap(), serde_json::Value::Null);
        assert_eq!(
            canonical_args_json(Some(&Struct::default())).unwrap(),
            serde_json::json!({})
        );
        assert_ne!(absent, empty);
    }

    #[test]
    fn object_keys_are_sorted() {
        let json = canonical_args_json(Some(&struct_of(vec![
            ("b", string("2")),
            ("a", string("1")),
        ])))
        .unwrap();
        assert_eq!(json.to_string(), r#"{"a":"1","b":"2"}"#);
    }

    #[test]
    fn non_finite_numbers_and_missing_kinds_are_rejected() {
        let nan = canonical_args_digest(Some(&struct_of(vec![("n", number(f64::NAN))])));
        assert!(matches!(nan, Err(ArgsDigestError::NonFiniteNumber { .. })));

        let inf = canonical_args_digest(Some(&struct_of(vec![("n", number(f64::INFINITY))])));
        assert!(matches!(inf, Err(ArgsDigestError::NonFiniteNumber { .. })));

        let missing = canonical_args_digest(Some(&struct_of(vec![("gap", Value { kind: None })])));
        assert!(matches!(missing, Err(ArgsDigestError::MissingKind { .. })));
    }

    #[test]
    fn nested_list_non_finite_is_rejected_not_nulled() {
        let err = digest_result(vec![("items", list(vec![number(f64::NAN)]))]);
        assert!(
            matches!(err, Err(ArgsDigestError::NonFiniteNumber { path }) if path == "$.items[0]")
        );
    }

    fn digest_result(fields: Vec<(&str, Value)>) -> Result<String, ArgsDigestError> {
        canonical_args_digest(Some(&struct_of(fields)))
    }
}
