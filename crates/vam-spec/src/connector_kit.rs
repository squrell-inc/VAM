//! The connector kit's test file: what a publisher declares so a connector's tools can be run
//! against a mock that sends nothing off the machine.
//!
//! One file per listing, `listings/<id>/tests/connector-test.json` ([`TEST_FILE_PATH`] inside the
//! listing). It is not an artifact, so a node never imports it. `vam connector test` and the e2e
//! runner's `provider: declared` mock both read it, so a publisher's CI and VAM's own scenarios
//! judge one mock.
//!
//! ```json
//! {
//!   "hosts": ["api.notesdesk.example"],
//!   "routes": [
//!     { "method": "GET", "prefix": "/v1/notes", "status": 200, "body": { "notes": [] } },
//!     { "method": "POST", "path": "/v1/notes", "status": 201, "body": { "id": 2 } }
//!   ],
//!   "cases": [
//!     { "tool": "note_search", "input": { "query": "printer" }, "expect": "ok", "contains": "Printer" }
//!   ]
//! }
//! ```
//!
//! This module is types, validation and the route matcher only: no I/O, no network, no clock. The
//! mock server, the generated key and the screening of a tool's requests live with the callers.
//! Messages never quote a value from the file's bodies or inputs, only field names and counts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Where the file sits inside a listing directory.
pub const TEST_FILE_PATH: &str = "tests/connector-test.json";

/// The largest test file read (bytes).
pub const MAX_TEST_FILE_BYTES: usize = 256 * 1024;
/// The most mock hosts one file declares.
pub const MAX_HOSTS: usize = 8;
/// The most routes one file declares.
pub const MAX_ROUTES: usize = 64;
/// The most cases one file declares.
pub const MAX_CASES: usize = 64;

const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

/// The test file.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorTestFile {
    /// The hosts the mock answers for (lowercase DNS names, no scheme, port or wildcard). A
    /// request to any other host is not answered.
    pub hosts: Vec<String>,
    /// What the mock answers. A request no route matches is answered 404.
    #[serde(default)]
    pub routes: Vec<TestRoute>,
    /// What each tool is expected to do against the mock.
    #[serde(default)]
    pub cases: Vec<TestCase>,
}

/// One answer: a method and either an exact path or a path prefix, a status and a JSON body.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestRoute {
    /// `GET`, `POST`, `PUT`, `PATCH` or `DELETE`.
    pub method: String,
    /// An exact path (`/v1/notes`): the request names the same segments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// A path prefix (`/v1/notes`): the request names these segments and may name more.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// The HTTP status of the answer (100-599).
    pub status: u16,
    /// The JSON body of the answer (`null` when omitted).
    #[serde(default)]
    pub body: Value,
}

/// What a case expects of a tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Expect {
    /// The tool's request reaches the mock and its output is the mock's answer.
    #[default]
    Ok,
    /// The node's gate would refuse the tool's request (outside the declared shape, a
    /// placeholder where none is allowed); the mock sees nothing.
    Refused,
}

/// One tool case.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TestCase {
    /// The tool's name in its skill manifest.
    pub tool: String,
    /// The tool's input (a JSON object; `{}` when omitted).
    #[serde(default = "empty_object")]
    pub input: Value,
    /// `ok` (default) or `refused`.
    #[serde(default)]
    pub expect: Expect,
    /// Text the tool's output must contain (`ok`) or the refusal must name (`refused`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contains: Option<String>,
}

fn empty_object() -> Value {
    Value::Object(Default::default())
}

/// Why a test file is refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TestFileError {
    /// The text is too large to read.
    #[error("the test file is larger than {MAX_TEST_FILE_BYTES} bytes")]
    TooLarge,
    /// The text is not the file's JSON shape; the parser's words (field names, positions).
    #[error("the test file is not valid: {0}")]
    Json(String),
    /// A field holds a value the file does not allow.
    #[error("the test file's `{field}` {reason}")]
    Invalid {
        /// A dotted path to the field (`routes.1.path`).
        field: String,
        /// What is wrong, without the value.
        reason: String,
    },
}

fn invalid(field: impl Into<String>, reason: impl Into<String>) -> TestFileError {
    TestFileError::Invalid {
        field: field.into(),
        reason: reason.into(),
    }
}

impl ConnectorTestFile {
    /// Parses and checks a test file.
    pub fn parse(text: &str) -> Result<Self, TestFileError> {
        if text.len() > MAX_TEST_FILE_BYTES {
            return Err(TestFileError::TooLarge);
        }
        let file: Self =
            serde_json::from_str(text).map_err(|e| TestFileError::Json(e.to_string()))?;
        file.check()?;
        Ok(file)
    }

    /// The checks [`Self::parse`] runs.
    pub fn check(&self) -> Result<(), TestFileError> {
        if self.hosts.is_empty() || self.hosts.len() > MAX_HOSTS {
            return Err(invalid("hosts", format!("holds 1 to {MAX_HOSTS} hosts")));
        }
        for (i, h) in self.hosts.iter().enumerate() {
            if !valid_host(h) {
                return Err(invalid(
                    format!("hosts.{i}"),
                    "is not a lowercase DNS name (no scheme, port, path or wildcard)",
                ));
            }
            if self.hosts[..i].contains(h) {
                return Err(invalid(format!("hosts.{i}"), "repeats an earlier host"));
            }
        }
        if self.routes.len() > MAX_ROUTES {
            return Err(invalid(
                "routes",
                format!("holds at most {MAX_ROUTES} routes"),
            ));
        }
        for (i, r) in self.routes.iter().enumerate() {
            let at = |f: &str| format!("routes.{i}.{f}");
            if !METHODS.contains(&r.method.as_str()) {
                return Err(invalid(
                    at("method"),
                    "is GET, POST, PUT, PATCH or DELETE (upper case)",
                ));
            }
            match (&r.path, &r.prefix) {
                (Some(_), Some(_)) | (None, None) => {
                    return Err(invalid(
                        at("path"),
                        "names exactly one of `path` and `prefix`",
                    ))
                }
                (Some(p), None) if !valid_route_path(p) => {
                    return Err(invalid(at("path"), PATH_RULE))
                }
                (None, Some(p)) if !valid_route_path(p) => {
                    return Err(invalid(at("prefix"), PATH_RULE))
                }
                _ => {}
            }
            if !(100..=599).contains(&r.status) {
                return Err(invalid(at("status"), "is an HTTP status (100 to 599)"));
            }
        }
        if self.cases.len() > MAX_CASES {
            return Err(invalid("cases", format!("holds at most {MAX_CASES} cases")));
        }
        for (i, c) in self.cases.iter().enumerate() {
            if c.tool.is_empty() || c.tool.len() > 64 || c.tool.chars().any(char::is_whitespace) {
                return Err(invalid(
                    format!("cases.{i}.tool"),
                    "is a tool name (1 to 64 characters, no spaces)",
                ));
            }
            if !c.input.is_object() {
                return Err(invalid(format!("cases.{i}.input"), "is a JSON object"));
            }
            if c.contains.as_deref().is_some_and(str::is_empty) {
                return Err(invalid(format!("cases.{i}.contains"), "is not empty"));
            }
        }
        Ok(())
    }

    /// The route that answers a request, or `None` (the mock answers 404). `host` is compared
    /// without case; the query and fragment of `target` are ignored; an exact route beats a prefix
    /// route and the longest prefix beats a shorter one. A host the file does not declare matches
    /// nothing.
    pub fn route_for(&self, host: &str, method: &str, target: &str) -> Option<&TestRoute> {
        if !self.serves(host) {
            return None;
        }
        let path = target.split(['?', '#']).next().unwrap_or_default();
        let want = segments(path)?;
        let mut best: Option<(usize, &TestRoute)> = None;
        for r in self.routes.iter().filter(|r| r.method == method) {
            if let Some(exact) = &r.path {
                if segments(exact).as_deref() == Some(want.as_slice()) {
                    return Some(r);
                }
            } else if let Some(prefix) = &r.prefix {
                let Some(have) = segments(prefix) else {
                    continue;
                };
                let admits =
                    have.len() <= want.len() && have.iter().zip(&want).all(|(a, b)| a == b);
                if admits && best.is_none_or(|(n, _)| have.len() > n) {
                    best = Some((have.len(), r));
                }
            }
        }
        best.map(|(_, r)| r)
    }

    /// Whether `host` is one of the file's hosts (without case).
    pub fn serves(&self, host: &str) -> bool {
        self.hosts.iter().any(|h| h.eq_ignore_ascii_case(host))
    }
}

const PATH_RULE: &str =
    "starts with `/` and holds no query, fragment, empty or dot segment, space or `%`";

/// The segments of a path: `/a/b/` is `["a", "b"]` and `/` is none. `None` for a path that does not
/// start with `/` or holds an empty or dot segment.
fn segments(path: &str) -> Option<Vec<&str>> {
    let rest = path.strip_prefix('/')?;
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    if rest.is_empty() {
        return Some(Vec::new());
    }
    let parts: Vec<&str> = rest.split('/').collect();
    if parts
        .iter()
        .any(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return None;
    }
    Some(parts)
}

fn valid_route_path(p: &str) -> bool {
    p.starts_with('/')
        && !p.contains(['?', '#', '%', ' ', '\\'])
        && p.bytes().all(|b| b > 0x20 && b < 0x7f)
        && segments(p).is_some()
}

fn valid_host(h: &str) -> bool {
    h.len() <= 253
        && h.contains('.')
        && h.split('.').all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{
      "hosts": ["api.notesdesk.example", "eu.api.notesdesk.example"],
      "routes": [
        { "method": "GET", "prefix": "/v1/notes", "status": 200, "body": { "notes": [{ "id": 1, "title": "Printer notes" }] } },
        { "method": "GET", "path": "/v1/notes/export", "status": 403, "body": { "error": "no" } },
        { "method": "POST", "path": "/v1/notes", "status": 201, "body": { "id": 2 } }
      ],
      "cases": [
        { "tool": "note_search", "input": { "query": "printer" }, "contains": "Printer" },
        { "tool": "note_wander", "expect": "refused", "contains": "declared shape" }
      ]
    }"#;

    fn good() -> ConnectorTestFile {
        ConnectorTestFile::parse(GOOD).unwrap()
    }

    #[test]
    fn a_good_file_parses_with_defaults() {
        let f = good();
        assert_eq!(f.hosts.len(), 2);
        assert_eq!(f.cases[0].expect, Expect::Ok);
        assert_eq!(f.cases[1].expect, Expect::Refused);
        assert!(f.cases[1].input.as_object().is_some_and(|o| o.is_empty()));
        let again = ConnectorTestFile::parse(&serde_json::to_string(&f).unwrap()).unwrap();
        assert_eq!(again, f);
    }

    #[test]
    fn the_matcher_follows_the_gates_segment_rules() {
        let f = good();
        let hit = |h: &str, m: &str, p: &str| f.route_for(h, m, p).map(|r| r.status);
        // prefix: the named segments and any beneath them, never a longer first segment
        assert_eq!(hit("api.notesdesk.example", "GET", "/v1/notes"), Some(200));
        assert_eq!(
            hit("api.notesdesk.example", "GET", "/v1/notes/7"),
            Some(200)
        );
        assert_eq!(hit("api.notesdesk.example", "GET", "/v1/notes/"), Some(200));
        assert_eq!(hit("api.notesdesk.example", "GET", "/v1/notesx"), None);
        assert_eq!(hit("api.notesdesk.example", "GET", "/v1"), None);
        // the query is not part of the path; the host is compared without case
        assert_eq!(
            hit("API.notesdesk.example", "GET", "/v1/notes?q=a/b"),
            Some(200)
        );
        // exact beats prefix, and an exact route admits no deeper path
        assert_eq!(
            hit("api.notesdesk.example", "GET", "/v1/notes/export"),
            Some(403)
        );
        assert_eq!(hit("api.notesdesk.example", "POST", "/v1/notes"), Some(201));
        assert_eq!(hit("api.notesdesk.example", "POST", "/v1/notes/7"), None);
        // another method, another host, a path that is not one
        assert_eq!(hit("api.notesdesk.example", "DELETE", "/v1/notes"), None);
        assert_eq!(hit("api.elsewhere.example", "GET", "/v1/notes"), None);
        assert_eq!(hit("api.notesdesk.example", "GET", "/v1/../v1/notes"), None);
        assert_eq!(hit("api.notesdesk.example", "GET", "v1/notes"), None);
        assert_eq!(
            hit("eu.api.notesdesk.example", "GET", "/v1/notes/1"),
            Some(200)
        );
    }

    #[test]
    fn the_longest_prefix_wins() {
        let f = ConnectorTestFile::parse(
            r#"{"hosts":["a.example"],"routes":[
              {"method":"GET","prefix":"/v1","status":200},
              {"method":"GET","prefix":"/v1/notes","status":201}]}"#,
        )
        .unwrap();
        assert_eq!(
            f.route_for("a.example", "GET", "/v1/notes/1")
                .unwrap()
                .status,
            201
        );
        assert_eq!(
            f.route_for("a.example", "GET", "/v1/other").unwrap().status,
            200
        );
        assert!(f.routes[0].body.is_null());
    }

    fn refused(edit: impl Fn(&str) -> String) -> String {
        ConnectorTestFile::parse(&edit(GOOD))
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_bad_file_is_refused_naming_the_field() {
        let e = refused(|g| g.replace("api.notesdesk.example", "https://api.notesdesk.example"));
        assert!(e.contains("hosts.0"), "{e}");
        let e = refused(|g| g.replace("\"eu.api.notesdesk.example\"", "\"api.notesdesk.example\""));
        assert!(e.contains("hosts.1") && e.contains("repeats"), "{e}");
        let e = refused(|g| g.replace("\"GET\", \"prefix\"", "\"get\", \"prefix\""));
        assert!(e.contains("routes.0.method"), "{e}");
        let e = refused(|g| g.replace("\"prefix\": \"/v1/notes\"", "\"prefix\": \"v1/notes\""));
        assert!(e.contains("routes.0.prefix"), "{e}");
        let e = refused(|g| g.replace("\"status\": 201", "\"status\": 99"));
        assert!(e.contains("routes.2.status"), "{e}");
        let e = refused(|g| {
            g.replace(
                "\"path\": \"/v1/notes/export\"",
                "\"path\": \"/v1/notes/export\", \"prefix\": \"/v1\"",
            )
        });
        assert!(
            e.contains("routes.1.path") && e.contains("exactly one"),
            "{e}"
        );
        let e = refused(|g| g.replace("\"input\": { \"query\": \"printer\" }", "\"input\": [1]"));
        assert!(e.contains("cases.0.input"), "{e}");
        let e = refused(|g| g.replace("\"expect\": \"refused\"", "\"expect\": \"maybe\""));
        assert!(e.contains("not valid"), "{e}");
        let e = refused(|g| g.replace("\"hosts\"", "\"servers\""));
        assert!(e.contains("not valid"), "{e}");
        assert!(ConnectorTestFile::parse(r#"{"hosts":[]}"#).is_err());
        let big = format!(
            r#"{{"hosts":["a.example"],"routes":[],"pad":"{}"}}"#,
            "x".repeat(MAX_TEST_FILE_BYTES)
        );
        assert_eq!(ConnectorTestFile::parse(&big), Err(TestFileError::TooLarge));
    }

    #[test]
    fn a_refusal_never_quotes_a_body_or_an_input() {
        let text = GOOD
            .replace("Printer notes", "SECRET-BODY-VALUE")
            .replace("\"status\": 201", "\"status\": 7");
        let e = ConnectorTestFile::parse(&text).unwrap_err().to_string();
        assert!(!e.contains("SECRET-BODY-VALUE"), "{e}");
    }
}
