//! The steps that read what receipts add up to per agent version, and the two steps that move an
//! agent between versions: `expect: { agent-stats }`, `expect: { release-diff }`,
//! `edit-agent-draft`, `release-agent`, and the file form of `verify-receipt` (a receipt saved
//! earlier, with its pinned node keys).
//!
//! This module is a child of `engine`; the shared runner files only register the steps and the
//! expectations.

use super::{conn, field_problems, run_cli, tamper_receipt, verify_verdict, Engine, Fail};
use crate::api::seg;
use crate::scenario::{Class, Expect, Step};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// `agent-stats: { piece, version?, k?, fields }`: `GET /v1/agents/{urn}/outcomes[?version=&k=]`.
/// `version` is `X.Y.Z` or `draft`; `fields` are dotted camelCase paths into the answer, with
/// `present` / `absent`, numbers compared by value.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStatsExpect {
    pub piece: String,
    pub version: Option<String>,
    pub k: Option<u64>,
    pub fields: BTreeMap<String, serde_yaml::Value>,
}

/// `release-diff: { piece, version, k?, fields }`:
/// `GET /v1/agents/{urn}/outcomes/diff?version=`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseDiffExpect {
    pub piece: String,
    pub version: String,
    pub k: Option<u64>,
    pub fields: BTreeMap<String, serde_yaml::Value>,
}

/// `edit-agent-draft: { piece, set: { <dotted spec path>: value } }`: reads the piece's draft,
/// sets the paths, puts it back (`PUT /v1/agents`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EditAgentDraft {
    pub piece: String,
    pub set: BTreeMap<String, serde_yaml::Value>,
}

/// `release-agent: { piece, version, refused? }`: releases the draft as `version` (`X.Y.Z`).
/// With `refused` the step passes only when the node answers 4xx.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReleaseAgent {
    pub piece: String,
    pub version: String,
    #[serde(default)]
    pub refused: bool,
}

/// `X.Y.Z` as the release body's `version`.
pub fn parse_version(v: &str) -> Option<Value> {
    let mut it = v.split('.');
    let (a, b, c) = (it.next()?, it.next()?, it.next()?);
    if it.next().is_some() {
        return None;
    }
    let n = |s: &str| s.parse::<u64>().ok();
    Some(json!({ "major": n(a)?, "minor": n(b)?, "patch": n(c)? }))
}

/// A version the outcomes routes take: `draft` or `X.Y.Z`.
fn version_ok(v: &str) -> bool {
    v == "draft" || parse_version(v).is_some()
}

/// The outcomes path of an agent: `/v1/agents/{urn}/outcomes[/diff][?version=&k=]`.
pub fn outcomes_path(urn: &str, diff: bool, version: Option<&str>, k: Option<u64>) -> String {
    let mut p = format!(
        "/v1/agents/{}/outcomes{}",
        seg(urn),
        if diff { "/diff" } else { "" }
    );
    let mut q = Vec::new();
    if let Some(v) = version {
        q.push(format!("version={}", seg(v)));
    }
    if let Some(k) = k {
        q.push(format!("k={k}"));
    }
    if !q.is_empty() {
        p.push('?');
        p.push_str(&q.join("&"));
    }
    p
}

/// Sets `path` (dotted keys; a numeric segment indexes an array) in `doc`, making objects on
/// the way. `false` when the path runs through a scalar or past the end of an array.
pub fn set_path(doc: &mut Value, path: &str, value: Value) -> bool {
    let mut cur = doc;
    let segs: Vec<&str> = path.split('.').collect();
    for (i, s) in segs.iter().enumerate() {
        let last = i + 1 == segs.len();
        match cur {
            Value::Object(m) => {
                if last {
                    m.insert((*s).to_string(), value);
                    return true;
                }
                cur = m.entry((*s).to_string()).or_insert_with(|| json!({}));
            }
            Value::Array(a) => {
                let Some(slot) = s.parse::<usize>().ok().and_then(|n| a.get_mut(n)) else {
                    return false;
                };
                if last {
                    *slot = value;
                    return true;
                }
                cur = slot;
            }
            _ => return false,
        }
    }
    false
}

/// Numbers match by value (`1` matches `1.0`); anything else is the caller's exact compare.
pub fn numbers_match(got: &Value, want: &serde_yaml::Value) -> Option<bool> {
    let (Value::Number(g), Some(w)) = (got, want.as_f64()) else {
        return None;
    };
    if !want.is_number() {
        return None;
    }
    Some(g.as_f64() == Some(w))
}

/// Problems with the shape of the steps, for `check`: aliases that are not in `given`,
/// versions that are not `X.Y.Z` or `draft`, empty `fields` or `set`.
pub fn check_expect(e: &Expect, given: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    let mut piece = |what: &str, p: &str| {
        if !given.contains(p) {
            out.push(format!("`{what}.piece: {p}` is not in `given`"));
        }
    };
    let mut bad = Vec::new();
    if let Some(x) = &e.agent_stats {
        piece("agent-stats", &x.piece);
        if x.fields.is_empty() {
            bad.push("`agent-stats` needs at least one field".to_string());
        }
        if x.version.as_deref().is_some_and(|v| !version_ok(v)) {
            bad.push("`agent-stats.version` is `draft` or `X.Y.Z`".to_string());
        }
        if x.k == Some(0) {
            bad.push("`agent-stats.k` must be at least 1".to_string());
        }
    }
    if let Some(x) = &e.release_diff {
        piece("release-diff", &x.piece);
        if x.fields.is_empty() {
            bad.push("`release-diff` needs at least one field".to_string());
        }
        if !version_ok(&x.version) {
            bad.push("`release-diff.version` is `draft` or `X.Y.Z`".to_string());
        }
        if x.k == Some(0) {
            bad.push("`release-diff.k` must be at least 1".to_string());
        }
    }
    out.extend(bad);
    out
}

/// Problems with `edit-agent-draft`, `release-agent` and `verify-receipt`'s file form.
pub fn check_step(st: &Step, given: &BTreeSet<String>) -> Vec<String> {
    let mut out = Vec::new();
    match st {
        Step::EditAgentDraft(x) => {
            if !given.contains(&x.piece) {
                out.push(format!(
                    "`edit-agent-draft.piece: {}` is not in `given`",
                    x.piece
                ));
            }
            if x.set.is_empty() {
                out.push("`edit-agent-draft` needs at least one path in `set`".to_string());
            }
        }
        Step::ReleaseAgent(x) => {
            if !given.contains(&x.piece) {
                out.push(format!(
                    "`release-agent.piece: {}` is not in `given`",
                    x.piece
                ));
            }
            if parse_version(&x.version).is_none() {
                out.push("`release-agent.version` must look like `1.1.0`".to_string());
            }
        }
        Step::VerifyReceipt(v) => {
            if v.file.is_some() && v.pack {
                out.push("`file` and `pack` are separate checks, not both".to_string());
            }
            if v.trusted_keys.is_some() && v.file.is_none() {
                out.push("`trustedKeys` goes with `file`".to_string());
            }
        }
        _ => {}
    }
    out
}

/// Where a fixture path is read from: as given (the repo root is the working directory under
/// `scripts/e2e/run.sh`), else under the repo this crate was built from.
fn fixture_path(rel: &str) -> PathBuf {
    let p = PathBuf::from(rel);
    if p.is_absolute() || p.exists() {
        return p;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

impl Engine {
    /// The urn of an agent piece in `given`, by alias.
    fn agent_urn(&self, alias: &str) -> Result<String, Fail> {
        use crate::world::PieceKind;
        match self.ctx.pieces.get(alias) {
            Some(p) if p.kind == PieceKind::Agent && !p.urn.is_empty() => Ok(p.urn.clone()),
            _ => Err(Fail::new(
                Class::Scenario,
                format!("agent alias `{alias}`"),
                "not an agent in `given`",
            )),
        }
    }

    /// One GET of an outcomes route, fields checked; a non-2xx is `product`.
    fn read_outcomes(
        &mut self,
        what: String,
        path: String,
        fields: &BTreeMap<String, serde_yaml::Value>,
    ) -> Result<(), Fail> {
        let r = self.api.get(&path).map_err(conn)?;
        if !r.ok() {
            return Err(Fail::new(
                Class::Product,
                what,
                format!("GET {path} answered {} {}", r.status, r.body),
            ));
        }
        let bad = field_problems(&r.json(), fields);
        if bad.is_empty() {
            Ok(())
        } else {
            Err(Fail::new(Class::Product, what, bad.join("; ")))
        }
    }

    /// `expect: { agent-stats }`.
    pub(super) fn expect_agent_stats(&mut self, x: &AgentStatsExpect) -> Result<(), Fail> {
        let urn = self.agent_urn(&x.piece)?;
        let what = format!(
            "agent `{}`{} reports {}",
            x.piece,
            x.version
                .as_ref()
                .map(|v| format!(" version {v}"))
                .unwrap_or_default(),
            serde_json::to_string(&x.fields).unwrap_or_default()
        );
        let path = outcomes_path(&urn, false, x.version.as_deref(), x.k);
        self.read_outcomes(what, path, &x.fields)
    }

    /// `expect: { release-diff }`.
    pub(super) fn expect_release_diff(&mut self, x: &ReleaseDiffExpect) -> Result<(), Fail> {
        let urn = self.agent_urn(&x.piece)?;
        let what = format!(
            "release diff of `{}` at {} reports {}",
            x.piece,
            x.version,
            serde_json::to_string(&x.fields).unwrap_or_default()
        );
        let path = outcomes_path(&urn, true, Some(&x.version), x.k);
        self.read_outcomes(what, path, &x.fields)
    }

    /// `edit-agent-draft`: GET the draft, set the paths under `spec`, PUT it back.
    pub(super) fn edit_agent_draft(&mut self, x: &EditAgentDraft) -> Result<(), Fail> {
        let urn = self.agent_urn(&x.piece)?;
        let harness = |what: String, r: &crate::api::Reply| {
            Fail::new(Class::Harness, what, format!("{} {}", r.status, r.body))
        };
        let get = format!("/v1/agents/{}", seg(&urn));
        let r = self.api.get(&get).map_err(conn)?;
        if !r.ok() {
            return Err(harness(format!("GET {get} for piece `{}`", x.piece), &r));
        }
        let mut spec = r.json()["spec"].clone();
        if !spec.is_object() {
            return Err(Fail::new(
                Class::Harness,
                format!("GET {get} holds a spec"),
                r.body.chars().take(200).collect::<String>(),
            ));
        }
        for (path, v) in &x.set {
            let v = serde_json::to_value(v).unwrap_or(Value::Null);
            if !set_path(&mut spec, path, v) {
                return Err(Fail::new(
                    Class::Scenario,
                    format!("`set` path `{path}` reaches into the draft"),
                    "the path runs through a value that is not an object or list",
                ));
            }
        }
        let r = self
            .api
            .put("/v1/agents", &json!({ "spec": spec }))
            .map_err(conn)?;
        if !r.ok() {
            return Err(harness(
                format!("PUT /v1/agents for piece `{}`", x.piece),
                &r,
            ));
        }
        Ok(())
    }

    /// `release-agent`: `POST /v1/agents/{urn}/release`.
    pub(super) fn release_agent(&mut self, x: &ReleaseAgent) -> Result<(), Fail> {
        let urn = self.agent_urn(&x.piece)?;
        let version = parse_version(&x.version).ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                "a version like 1.1.0",
                format!("`{}`", x.version),
            )
        })?;
        let path = format!("/v1/agents/{}/release", seg(&urn));
        let r = self
            .api
            .post(&path, &json!({ "version": version }))
            .map_err(conn)?;
        match (x.refused, r.ok()) {
            (false, true) => Ok(()),
            (false, false) => Err(Fail::new(
                Class::Harness,
                format!("release of piece `{}` as {}", x.piece, x.version),
                format!("{} {}", r.status, r.body),
            )),
            (true, false) if r.status / 100 == 4 => {
                self.ctx.last_refusal = Some(r.body);
                Ok(())
            }
            (true, _) => Err(Fail::new(
                Class::Product,
                format!("the release of {} is refused with a 4xx", x.version),
                format!("{} {}", r.status, r.body),
            )),
        }
    }

    /// `verify-receipt: { file, trustedKeys?, tamper? }`: the built `vam` offline over a receipt
    /// file saved earlier, pinned to a node key list when `trustedKeys` is given.
    pub(super) fn verify_receipt_file(
        &mut self,
        v: &crate::scenario::VerifyReceipt,
    ) -> Result<(), Fail> {
        let what = if v.tamper {
            "a saved receipt with one changed byte fails verification"
        } else {
            "a saved receipt verifies offline"
        };
        let envf = |e: String| Fail::new(Class::Env, what, e);
        let rel = v.file.as_deref().unwrap_or_default();
        let src = fixture_path(rel);
        let bytes = std::fs::read(&src)
            .map_err(|e| Fail::new(Class::Harness, what, format!("{}: {e}", src.display())))?;
        let dir = std::env::temp_dir().join(format!("vam-e2e-receipt-file-{}", self.cfg.tag.0));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| envf(e.to_string()))?;
        let file = dir.join("receipt.json");
        if v.tamper {
            let doc: Value = serde_json::from_slice(&bytes)
                .map_err(|e| Fail::new(Class::Harness, what, format!("{rel}: {e}")))?;
            let doc = tamper_receipt(&doc)
                .ok_or_else(|| Fail::new(Class::Harness, what, "the receipt holds no text"))?;
            std::fs::write(&file, serde_json::to_vec(&doc).unwrap_or_default())
        } else {
            std::fs::write(&file, &bytes)
        }
        .map_err(|e| envf(e.to_string()))?;
        let file_s = file.to_string_lossy().to_string();
        let mut args = vec!["receipt", "verify", file_s.as_str()];
        let keys = v
            .trusted_keys
            .as_deref()
            .map(|k| fixture_path(k).to_string_lossy().to_string());
        if let Some(k) = &keys {
            args.push("--trusted-keys");
            args.push(k);
        }
        let cli = std::env::var("VAM_E2E_CLI").ok();
        let (ok, out) = run_cli(cli.as_deref(), &args)?;
        verify_verdict(v.tamper, ok, &out).map_err(|m| Fail::new(Class::Product, what, m))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_and_paths() {
        assert_eq!(
            parse_version("1.1.0"),
            Some(json!({"major": 1, "minor": 1, "patch": 0}))
        );
        assert_eq!(parse_version("1.1"), None);
        assert_eq!(parse_version("draft"), None);
        assert!(version_ok("draft") && version_ok("2.0.1") && !version_ok("latest"));
        assert_eq!(
            outcomes_path("urn:vam:agent:a/b", false, Some("draft"), Some(2)),
            "/v1/agents/urn%3Avam%3Aagent%3Aa%2Fb/outcomes?version=draft&k=2"
        );
        assert_eq!(
            outcomes_path("u", true, Some("1.1.0"), None),
            "/v1/agents/u/outcomes/diff?version=1.1.0"
        );
        assert_eq!(
            outcomes_path("u", false, None, None),
            "/v1/agents/u/outcomes"
        );
    }

    #[test]
    fn set_path_makes_objects_and_indexes_lists() {
        let mut d = json!({"a": {"b": 1}, "l": [{"x": 1}]});
        assert!(set_path(&mut d, "a.c.d", json!(2)));
        assert!(set_path(&mut d, "l.0.x", json!(9)));
        assert!(set_path(&mut d, "description", json!("t")));
        assert!(!set_path(&mut d, "a.b.z", json!(1)));
        assert!(!set_path(&mut d, "l.4.x", json!(1)));
        assert_eq!(d["a"]["c"]["d"], 2);
        assert_eq!(d["l"][0]["x"], 9);
        assert_eq!(d["description"], "t");
    }

    #[test]
    fn numbers_compare_by_value() {
        let one = serde_yaml::Value::from(1);
        assert_eq!(numbers_match(&json!(1.0), &one), Some(true));
        assert_eq!(numbers_match(&json!(1), &one), Some(true));
        assert_eq!(numbers_match(&json!(2), &one), Some(false));
        assert_eq!(numbers_match(&json!("1"), &one), None);
        assert_eq!(
            numbers_match(&json!(1), &serde_yaml::Value::from("1")),
            None
        );
    }

    #[test]
    fn the_steps_parse() {
        let s: Step = serde_yaml::from_str(
            "edit-agent-draft: { piece: agent-with-effect-tools, set: { description: second } }",
        )
        .unwrap();
        assert!(matches!(s, Step::EditAgentDraft(_)));
        let s: Step = serde_yaml::from_str(
            "release-agent: { piece: agent-with-effect-tools, version: 1.1.0 }",
        )
        .unwrap();
        assert!(matches!(s, Step::ReleaseAgent(r) if !r.refused));
        let s: Step = serde_yaml::from_str(
            "expect: { agent-stats: { piece: p, version: draft, k: 2, fields: { runs: 1 } } }",
        )
        .unwrap();
        match s {
            Step::Expect(e) => {
                assert_eq!(e.families(), 1);
                assert_eq!(e.agent_stats.unwrap().k, Some(2));
            }
            _ => panic!("not an expect"),
        }
    }
}
