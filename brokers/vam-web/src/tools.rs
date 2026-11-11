//! Tool definitions and argument parsing for `web_search` and `web_fetch`.

use serde::Deserialize;
use serde_json::{json, Value};
use vam_spec::v1::tool_spec::ExecutionProvider;
use vam_spec::v1::{HostExecSpec, SecretRequirement, SideEffectClass, ToolSpec};

use crate::error::{FetchError, ProviderError};
use crate::provider::brave::SECRET_NAME;
use crate::provider::{Recency, SearchQuery, DEFAULT_COUNT, MAX_COUNT, MAX_QUERY_CHARS};

pub const WEB_SEARCH: &str = "web_search";
pub const WEB_FETCH: &str = "web_fetch";
/// Most entries in a domain filter list.
pub const MAX_DOMAINS: usize = 20;
/// Longest URL `web_fetch` accepts.
pub const MAX_URL_CHARS: usize = 2048;
/// Host adapter id carried on the tool specs.
pub const ADAPTER_ID: &str = "vam-web";

/// JSON schema of `web_search` arguments.
pub fn search_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["query"],
        "properties": {
            "query": {
                "type": "string",
                "minLength": 1,
                "maxLength": MAX_QUERY_CHARS,
                "description": "What to search for. Plain keywords or a short question; no URLs (use web_fetch for a URL)."
            },
            "recency": {
                "type": "string",
                "enum": ["day", "week", "month", "year"],
                "description": "Only results from the last day, week, month or year."
            },
            "include_domains": {
                "type": "array",
                "maxItems": MAX_DOMAINS,
                "items": {"type": "string", "maxLength": 253},
                "description": "Only results from these domains, for example [\"docs.rs\"]."
            },
            "exclude_domains": {
                "type": "array",
                "maxItems": MAX_DOMAINS,
                "items": {"type": "string", "maxLength": 253},
                "description": "Leave out results from these domains."
            },
            "count": {
                "type": "integer",
                "minimum": 1,
                "maximum": MAX_COUNT,
                "default": DEFAULT_COUNT,
                "description": "How many results to return."
            }
        }
    })
}

/// JSON schema of `web_fetch` arguments.
pub fn fetch_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["url"],
        "properties": {
            "url": {
                "type": "string",
                "maxLength": MAX_URL_CHARS,
                "description": "A full http or https URL that already appeared in the conversation (the user's message or an earlier search or fetch result)."
            },
            "focus": {
                "type": "string",
                "maxLength": 200,
                "description": "Optional topic to look for on the page; the preview will favour matching sections."
            }
        }
    })
}

/// The tools `ListTools` returns.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: WEB_SEARCH.to_string(),
            description: "Search the web and get a list of results (title, URL, date, snippet). Search more than once with different queries when the first results do not answer the question, then read the best sources with web_fetch. Cite what you use.".to_string(),
            input_schema_json: search_schema().to_string(),
            execution_provider: Some(ExecutionProvider::HostExec(HostExecSpec {
                adapter_id: ADAPTER_ID.to_string(),
                config: Default::default(),
            })),
            required_secrets: vec![SecretRequirement {
                name: SECRET_NAME.to_string(),
                required: true,
                ..Default::default()
            }],
            side_effect: SideEffectClass::ReadOnly as i32,
            ..Default::default()
        },
        ToolSpec {
            name: WEB_FETCH.to_string(),
            description: "Read a web page you already have the URL for. The page is saved as a workspace file and you get a preview and the file path. Only URLs that appeared in the conversation are accepted. HTML and plain text only (no PDF). If the site redirects to another host you get redirect_to instead of a page; call web_fetch again with that URL if you still want it.".to_string(),
            input_schema_json: fetch_schema().to_string(),
            execution_provider: Some(ExecutionProvider::HostExec(HostExecSpec {
                adapter_id: ADAPTER_ID.to_string(),
                config: Default::default(),
            })),
            required_secrets: vec![],
            side_effect: SideEffectClass::ReadOnly as i32,
            ..Default::default()
        },
    ]
}

/// Convert a protobuf `Struct` to JSON.
pub fn struct_to_json(s: &prost_types::Struct) -> Value {
    Value::Object(
        s.fields
            .iter()
            .map(|(k, v)| (k.clone(), value_to_json(v)))
            .collect(),
    )
}

fn value_to_json(v: &prost_types::Value) -> Value {
    use prost_types::value::Kind;
    match &v.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(Kind::StringValue(s)) => Value::String(s.clone()),
        Some(Kind::BoolValue(b)) => Value::Bool(*b),
        Some(Kind::StructValue(s)) => struct_to_json(s),
        Some(Kind::ListValue(l)) => Value::Array(l.values.iter().map(value_to_json).collect()),
    }
}

/// Convert JSON to a protobuf `Struct` (non-object values give an empty one).
pub fn json_to_struct(v: &Value) -> prost_types::Struct {
    match v {
        Value::Object(map) => prost_types::Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        },
        _ => prost_types::Struct::default(),
    }
}

fn json_to_value(v: &Value) -> prost_types::Value {
    use prost_types::value::Kind;
    prost_types::Value {
        kind: Some(match v {
            Value::Null => Kind::NullValue(0),
            Value::Bool(b) => Kind::BoolValue(*b),
            Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => Kind::StringValue(s.clone()),
            Value::Array(a) => Kind::ListValue(prost_types::ListValue {
                values: a.iter().map(json_to_value).collect(),
            }),
            Value::Object(_) => Kind::StructValue(json_to_struct(v)),
        }),
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    recency: Option<String>,
    #[serde(default)]
    include_domains: Vec<String>,
    #[serde(default)]
    exclude_domains: Vec<String>,
    count: Option<f64>,
}

fn invalid(detail: impl Into<String>) -> ProviderError {
    ProviderError::InvalidQuery(detail.into())
}

fn valid_domain(d: &str) -> bool {
    !d.is_empty()
        && d.len() <= 253
        && d.split('.').all(|label| {
            !label.is_empty()
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        })
}

fn clean_domains(field: &str, list: Vec<String>) -> Result<Vec<String>, ProviderError> {
    if list.len() > MAX_DOMAINS {
        return Err(invalid(format!(
            "{field} has more than {MAX_DOMAINS} entries; use fewer domains"
        )));
    }
    list.into_iter()
        .map(|d| {
            let d = d.trim().trim_start_matches("*.").to_ascii_lowercase();
            if valid_domain(&d) {
                Ok(d)
            } else {
                Err(invalid(format!(
                    "{field} must hold bare domain names such as \"example.com\""
                )))
            }
        })
        .collect()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    url: String,
    focus: Option<String>,
}

/// Validated `web_fetch` arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest {
    pub url: String,
    pub focus: Option<String>,
}

/// Longest `focus` kept.
pub const MAX_FOCUS_CHARS: usize = 200;

/// Validate `web_fetch` arguments. The URL itself is checked by the fetcher.
pub fn parse_fetch_args(args: &Value) -> Result<FetchRequest, FetchError> {
    let parsed: FetchArgs = serde_json::from_value(args.clone()).map_err(|_| {
        FetchError::InvalidUrl(
            "arguments must be an object with a string \"url\" and an optional \"focus\""
                .to_string(),
        )
    })?;
    let focus = parsed
        .focus
        .map(|f| f.trim().chars().take(MAX_FOCUS_CHARS).collect::<String>())
        .filter(|f| !f.is_empty());
    Ok(FetchRequest {
        url: parsed.url,
        focus,
    })
}

/// Validate `web_search` arguments.
pub fn parse_search_args(args: &Value) -> Result<SearchQuery, ProviderError> {
    let parsed: SearchArgs = serde_json::from_value(args.clone()).map_err(|_| {
        invalid("arguments must be an object with a string \"query\" and optional \"recency\", \"include_domains\", \"exclude_domains\" and \"count\"")
    })?;
    let query = parsed.query.trim().to_string();
    if query.is_empty() {
        return Err(invalid("query is empty; give search keywords"));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(invalid(format!(
            "query is longer than {MAX_QUERY_CHARS} characters; shorten it"
        )));
    }
    let recency = match parsed.recency.as_deref() {
        None => None,
        Some("day") => Some(Recency::Day),
        Some("week") => Some(Recency::Week),
        Some("month") => Some(Recency::Month),
        Some("year") => Some(Recency::Year),
        Some(_) => return Err(invalid("recency must be one of day, week, month, year")),
    };
    let count = match parsed.count {
        None => DEFAULT_COUNT,
        Some(n) if n.fract() == 0.0 && n >= 1.0 && n <= f64::from(MAX_COUNT) => n as u8,
        Some(_) => {
            return Err(invalid(format!(
                "count must be a whole number from 1 to {MAX_COUNT}"
            )))
        }
    };
    Ok(SearchQuery {
        query,
        recency,
        include_domains: clean_domains("include_domains", parsed.include_domains)?,
        exclude_domains: clean_domains("exclude_domains", parsed.exclude_domains)?,
        count,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schemas_parse_and_have_limits() {
        let specs = tool_specs();
        assert_eq!(specs.len(), 2);
        let search: Value = serde_json::from_str(&specs[0].input_schema_json).expect("json");
        assert_eq!(search["properties"]["query"]["maxLength"], 400);
        assert_eq!(search["properties"]["count"]["maximum"], 20);
        assert_eq!(search["properties"]["count"]["default"], 10);
        assert_eq!(
            search["properties"]["recency"]["enum"],
            json!(["day", "week", "month", "year"])
        );
        let fetch: Value = serde_json::from_str(&specs[1].input_schema_json).expect("json");
        assert_eq!(fetch["required"], json!(["url"]));
        assert!(fetch["properties"].get("focus").is_some());
        for spec in &specs {
            assert_eq!(spec.side_effect, SideEffectClass::ReadOnly as i32);
        }
        assert_eq!(specs[0].required_secrets[0].name, "brave-api-key");
        assert!(specs[1].required_secrets.is_empty());
    }

    #[test]
    fn parses_valid_args() {
        let q = parse_search_args(&json!({
            "query": " rust ", "recency": "month", "include_domains": ["Docs.rs", "*.rust-lang.org"],
            "exclude_domains": ["x.com"], "count": 7
        }))
        .expect("ok");
        assert_eq!(q.query, "rust");
        assert_eq!(q.recency, Some(Recency::Month));
        assert_eq!(q.include_domains, vec!["docs.rs", "rust-lang.org"]);
        assert_eq!(q.count, 7);
        let d = parse_search_args(&json!({"query": "a"})).expect("ok");
        assert_eq!(d.count, 10);
    }

    #[test]
    fn rejects_bad_args() {
        for bad in [
            json!({}),
            json!({"query": ""}),
            json!({"query": "a", "count": 0}),
            json!({"query": "a", "count": 21}),
            json!({"query": "a", "count": 1.5}),
            json!({"query": "a", "recency": "decade"}),
            json!({"query": "a", "extra": 1}),
            json!({"query": "a", "include_domains": ["a b.com"]}),
            json!({"query": "a", "include_domains": ["x.com OR site:evil.com"]}),
            json!({"query": "a".repeat(401)}),
        ] {
            let err = parse_search_args(&bad).expect_err("should fail");
            assert_eq!(err.code(), "invalid_query", "{bad}");
        }
    }

    #[test]
    fn fetch_args() {
        let r = parse_fetch_args(&json!({"url": "https://a.test/", "focus": "  pricing "}))
            .expect("ok");
        assert_eq!(r.url, "https://a.test/");
        assert_eq!(r.focus.as_deref(), Some("pricing"));
        let r = parse_fetch_args(&json!({"url": "https://a.test/", "focus": "   "})).expect("ok");
        assert_eq!(r.focus, None);
        let r = parse_fetch_args(&json!({"url": "u", "focus": "x".repeat(500)})).expect("ok");
        assert_eq!(r.focus.expect("f").chars().count(), 200);
        for bad in [json!({}), json!({"url": 1}), json!({"url": "u", "x": 1})] {
            assert_eq!(
                parse_fetch_args(&bad).expect_err("bad").code(),
                "invalid_url"
            );
        }
    }

    #[test]
    fn struct_round_trip() {
        let v = json!({"a": [1.0, "x", {"b": true}], "n": null});
        assert_eq!(struct_to_json(&json_to_struct(&v)), v);
    }
}
