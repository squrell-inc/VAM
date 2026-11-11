//! `vam-e2e check --docs`: the docs status field and the claim lint.
//!
//! Every page says `status: shipped | planned | stale` and, when it makes an isolation claim,
//! names what proves it in `proves`. Pages that fail today are listed in
//! `<root>/docs-baseline.txt`; the list can only shrink (a listed page that now passes is an
//! error), and the exit is an empty list.

use crate::catalog::{Catalog, CheckError};
use crate::scenario::Status;
use regex::Regex;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The claim word list shipped with the binary; `<root>/docs-claims.yaml` overrides it.
const DEFAULT_CLAIMS: &str = include_str!("../../../e2e/docs-claims.yaml");
pub const CLAIMS_FILE: &str = "docs-claims.yaml";
pub const BASELINE_FILE: &str = "docs-baseline.txt";
pub const FEATURES_FILE: &str = "docs-features.yaml";
/// The redirect map, relative to the repository root (the parent of the catalog root).
pub const REDIRECTS_FILE: &str = "docs/redirects.json";

#[derive(serde::Deserialize)]
struct ClaimsFile {
    patterns: Vec<String>,
}

fn claim_patterns(root: &Path, errors: &mut Vec<CheckError>) -> Vec<Regex> {
    let f = root.join(CLAIMS_FILE);
    let (text, name) = match std::fs::read_to_string(&f) {
        Ok(t) => (t, f.display().to_string()),
        Err(_) => (
            DEFAULT_CLAIMS.to_string(),
            "docs-claims.yaml (built in)".to_string(),
        ),
    };
    let parsed: ClaimsFile = match serde_yaml::from_str(&text) {
        Ok(p) => p,
        Err(e) => {
            errors.push(CheckError::new(name, "schema", e.to_string()));
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for p in parsed.patterns {
        match Regex::new(&format!("(?i){p}")) {
            Ok(r) => out.push(r),
            Err(e) => errors.push(CheckError::new(
                name.clone(),
                "schema",
                format!("bad pattern `{p}`: {e}"),
            )),
        }
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            walk(&p, out);
        } else if p.extension().is_some_and(|x| x == "md" || x == "mdx") {
            out.push(p);
        }
    }
}

/// Splits a page into (frontmatter YAML, prose). A README keeps its keys in a leading
/// `<!-- docs: ... -->` comment.
fn split(text: &str, readme: bool) -> (Option<String>, String) {
    if readme {
        if let Some(rest) = text.strip_prefix("<!-- docs:") {
            if let Some(end) = rest.find("-->") {
                return (Some(rest[..end].to_string()), rest[end + 3..].to_string());
            }
        }
        return (None, text.to_string());
    }
    let t = text.trim_start_matches('\u{feff}');
    if let Some(rest) = t
        .strip_prefix("---\n")
        .or_else(|| t.strip_prefix("---\r\n"))
    {
        if let Some(i) = rest.find("\n---") {
            let after = &rest[i + 4..];
            let after = after.split_once('\n').map(|x| x.1).unwrap_or("");
            return (Some(rest[..i].to_string()), after.to_string());
        }
    }
    (None, t.to_string())
}

fn prose(body: &str) -> String {
    let fence = Regex::new(r"(?s)```.*?```|~~~.*?~~~").unwrap();
    let comment = Regex::new(r"(?s)<!--.*?-->").unwrap();
    let s = fence.replace_all(body, "");
    comment.replace_all(&s, "").into_owned()
}

fn is_test_ref(r: &str) -> bool {
    r.contains("::") || r.contains('/') || r.ends_with(".rs")
}

/// True when `fn name` is in `src` and an attribute `#[test]`, `#[tokio::test]` or
/// `#[tokio::test(...)]` sits directly above it (other attributes and doc comments may be between).
fn is_test_fn(src: &str, name: &str) -> bool {
    let decl = Regex::new(&format!(
        r"^\s*(pub\s+)?(async\s+)?fn {}\b",
        regex::escape(name)
    ))
    .unwrap();
    let test_attr = Regex::new(r"^\s*#\[(tokio::)?test(\(.*\))?\]\s*$").unwrap();
    let lines: Vec<&str> = src.lines().collect();
    lines.iter().enumerate().any(|(i, l)| {
        decl.is_match(l)
            && lines[..i]
                .iter()
                .rev()
                .take_while(|p| {
                    let t = p.trim_start();
                    t.starts_with("#[") || t.starts_with("///")
                })
                .any(|p| test_attr.is_match(p))
    })
}

/// Why a `proves` entry is not a proof, or None when it holds. `allow_pending` is true for a
/// `planned` page, the only kind that may cite a pending scenario.
fn proof_problem(cx: &Ctx, r: &str, allow_pending: bool) -> Option<String> {
    if is_test_ref(r) {
        let (file, func) = match r.split_once("::") {
            Some((f, rest)) => (f, rest.rsplit("::").next()),
            None => (r, None),
        };
        let Ok(src) = std::fs::read_to_string(cx.repo.join(file)) else {
            return Some(format!("test file `{file}` does not exist"));
        };
        return match func {
            // A whole file is a proof only when it is a crate's integration test.
            None if file.contains("/tests/") => None,
            None => Some(format!(
                "`{file}` is not a test file: cite a file under `tests/` or `{file}::test_fn`"
            )),
            Some(f) if is_test_fn(&src, f) => None,
            Some(f) => Some(format!(
                "`{file}` has no test `fn {f}` (a `#[test]` or `#[tokio::test]` attribute must precede it)"
            )),
        };
    }
    match cx.scenarios.get(r) {
        None => Some(format!("no scenario `{r}`")),
        Some(Status::Retired) => Some(format!("scenario `{r}` is retired")),
        Some(Status::Pending) if !allow_pending => Some(format!(
            "scenario `{r}` is pending: not proven yet (only a planned page may cite it)"
        )),
        Some(_) => None,
    }
}

/// Absolute link targets in a page body (`](/a/b/)` and `href="/a/b/"`), fragment and query
/// dropped, with one trailing slash.
fn links(body: &str) -> Vec<String> {
    let re = Regex::new(r##"\]\((/[^)\s#?]*)|href="(/[^"#?]*)"##).unwrap();
    re.captures_iter(&prose(body))
        .filter_map(|c| c.get(1).or_else(|| c.get(2)))
        .map(|m| format!("/{}/", m.as_str().trim_matches('/')).replace("//", "/"))
        .collect()
}

struct Ctx {
    redirects: BTreeMap<String, String>,
    repo: PathBuf,
    scenarios: BTreeMap<String, Status>,
    claims: Vec<Regex>,
    admonition: Regex,
}

fn page_errors(cx: &Ctx, path: &str, text: &str, readme: bool) -> Vec<CheckError> {
    let mut errs = Vec::new();
    let (fm, body) = split(text, readme);
    let fm: serde_yaml::Value = match fm {
        None => serde_yaml::Value::Null,
        Some(y) => match serde_yaml::from_str(&y) {
            Ok(v) => v,
            Err(e) => {
                errs.push(CheckError::new(
                    path,
                    "docs-status",
                    format!("frontmatter: {e}"),
                ));
                serde_yaml::Value::Null
            }
        },
    };
    let status = fm
        .get("status")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    match status.as_deref() {
        Some("shipped" | "planned" | "stale") => {}
        Some(other) => errs.push(CheckError::new(
            path,
            "docs-status",
            format!("unknown status `{other}` (shipped, planned or stale)"),
        )),
        None => errs.push(CheckError::new(
            path,
            "docs-status",
            "missing `status` (shipped, planned or stale)",
        )),
    }
    let proves: Vec<String> = fm
        .get("proves")
        .and_then(|v| v.as_sequence())
        .map(|s| {
            s.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();

    if cx.admonition.is_match(&body) {
        errs.push(CheckError::new(
            path,
            "docs-admonition",
            "hand-written `:::note[Status` / `:::caution[Status` note: use the `status` field",
        ));
    }
    let prose = prose(&body);
    if status.as_deref() == Some("shipped") && proves.is_empty() {
        if let Some(m) = cx.claims.iter().find_map(|r| r.find(&prose)) {
            errs.push(CheckError::new(
                path,
                "docs-proof",
                format!(
                    "shipped page makes an isolation claim (`{}`) and `proves` is empty",
                    m.as_str()
                ),
            ));
        }
    }
    for r in &proves {
        if let Some(msg) = proof_problem(cx, r, status.as_deref() == Some("planned")) {
            errs.push(CheckError::new(path, "docs-proof-ref", msg));
        }
    }
    for t in links(&body) {
        if let Some(to) = cx.redirects.get(&t) {
            errs.push(CheckError::new(
                path,
                "docs-redirect",
                format!("links to `{t}`, which redirects to `{to}`: link the target"),
            ));
        }
    }
    errs
}

#[derive(serde::Deserialize)]
struct Redirect {
    to: String,
    kind: String,
}

/// A page slug (`network/overview`) from a URL path (`/network/overview/`).
fn slug_of(url: &str) -> String {
    url.trim_matches('/').to_string()
}

fn page_exists(keys: &BTreeSet<String>, slug: &str) -> bool {
    let slug = if slug.is_empty() { "index" } else { slug };
    ["md", "mdx"].iter().any(|e| {
        keys.contains(&format!("{slug}.{e}")) || keys.contains(&format!("{slug}/index.{e}"))
    })
}

/// Reads `<repo>/docs/redirects.json`: old URL path to {to, kind}. Absent file: no redirects.
fn load_redirects(root: &Path, errors: &mut Vec<CheckError>) -> BTreeMap<String, (String, String)> {
    let f = root.join("..").join(REDIRECTS_FILE);
    let Ok(text) = std::fs::read_to_string(&f) else {
        return BTreeMap::new();
    };
    let name = f.display().to_string();
    match serde_json::from_str::<BTreeMap<String, Redirect>>(&text) {
        Ok(m) => m
            .into_iter()
            .map(|(k, v)| (format!("/{}/", slug_of(&k)), (v.to, v.kind)))
            .collect(),
        Err(e) => {
            errors.push(CheckError::new(
                name,
                "docs-redirect",
                format!("schema: {e}"),
            ));
            BTreeMap::new()
        }
    }
}

#[derive(serde::Deserialize)]
struct Feature {
    id: String,
    #[serde(default)]
    dev: Vec<String>,
    #[serde(default)]
    arch: Vec<String>,
    #[serde(default)]
    proves: Vec<String>,
    #[serde(default)]
    unproven: Option<String>,
    #[serde(default)]
    pending_page: bool,
}

#[derive(serde::Deserialize)]
struct FeaturesFile {
    features: Vec<Feature>,
}

fn load_features(root: &Path, errors: &mut Vec<CheckError>) -> Vec<Feature> {
    let f = root.join(FEATURES_FILE);
    let Ok(text) = std::fs::read_to_string(&f) else {
        return Vec::new();
    };
    match serde_yaml::from_str::<FeaturesFile>(&text) {
        Ok(p) => p.features,
        Err(e) => {
            errors.push(CheckError::new(
                f.display().to_string(),
                "schema",
                e.to_string(),
            ));
            Vec::new()
        }
    }
}

/// (features, with a proof, with a developer page) in `<root>/docs-features.yaml`, or None when
/// the register is absent.
pub fn feature_counts(root: &Path) -> Option<(usize, usize, usize)> {
    let mut e = Vec::new();
    if !root.join(FEATURES_FILE).is_file() {
        return None;
    }
    let fs = load_features(root, &mut e);
    Some((
        fs.len(),
        fs.iter().filter(|f| !f.proves.is_empty()).count(),
        fs.iter().filter(|f| !f.dev.is_empty()).count(),
    ))
}

/// Lints the docs under `docs` (and `readme` when given). `root` is the catalog root
/// (`e2e/`); the repository root is its parent, where test paths resolve.
pub fn check_docs(root: &Path, docs: &Path, readme: Option<&Path>) -> Vec<CheckError> {
    let mut errors = Vec::new();
    let cat = Catalog::load(root);
    let cx = Ctx {
        repo: root.join(".."),
        scenarios: cat
            .scenarios
            .iter()
            .map(|(_, s)| (s.id.clone(), s.status))
            .collect(),
        redirects: BTreeMap::new(),
        claims: claim_patterns(root, &mut errors),
        admonition: Regex::new(r"(?m)^:::(note|caution)\[Status").unwrap(),
    };

    // (key used in the baseline, path shown, text, is_readme)
    let mut pages: Vec<(String, String, String, bool)> = Vec::new();
    let mut files = Vec::new();
    walk(docs, &mut files);
    files.sort();
    for f in files {
        let key = f
            .strip_prefix(docs)
            .unwrap_or(&f)
            .to_string_lossy()
            .replace('\\', "/");
        match std::fs::read_to_string(&f) {
            Ok(t) => pages.push((key, f.display().to_string(), t, false)),
            Err(e) => errors.push(CheckError::new(
                f.display().to_string(),
                "io",
                e.to_string(),
            )),
        }
    }
    if let Some(r) = readme {
        match std::fs::read_to_string(r) {
            Ok(t) => pages.push(("README.md".into(), r.display().to_string(), t, true)),
            Err(e) => errors.push(CheckError::new(
                r.display().to_string(),
                "io",
                e.to_string(),
            )),
        }
    }

    let bf = root.join(BASELINE_FILE);
    let mut baseline = BTreeSet::new();
    if let Ok(t) = std::fs::read_to_string(&bf) {
        for l in t
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
        {
            if !baseline.insert(l.to_string()) {
                errors.push(CheckError::new(
                    bf.display().to_string(),
                    "docs-baseline",
                    format!("`{l}` is listed twice"),
                ));
            }
        }
    }
    let redirects = load_redirects(root, &mut errors);
    let rd_file = root.join("..").join(REDIRECTS_FILE).display().to_string();
    let keys: BTreeSet<String> = pages.iter().map(|p| p.0.clone()).collect();
    for (old, (to, kind)) in &redirects {
        if kind != "retired" && kind != "design" {
            errors.push(CheckError::new(
                rd_file.clone(),
                "docs-redirect",
                format!("`{old}`: kind `{kind}` is not `retired` or `design`"),
            ));
        }
        if page_exists(&keys, &slug_of(old)) {
            errors.push(CheckError::new(
                rd_file.clone(),
                "docs-redirect",
                format!(
                    "`{old}` is redirected but its page still exists: delete the page or the entry"
                ),
            ));
        }
        let target = to.split(['#', '?']).next().unwrap_or("");
        if !target.starts_with('/') || !page_exists(&keys, &slug_of(target)) {
            errors.push(CheckError::new(
                rd_file.clone(),
                "docs-redirect",
                format!("`{old}`: target `{to}` is not a page"),
            ));
        } else if redirects.contains_key(&format!("/{}/", slug_of(target))) {
            errors.push(CheckError::new(
                rd_file.clone(),
                "docs-redirect",
                format!("`{old}`: target `{to}` is itself redirected"),
            ));
        }
    }
    let cx = Ctx {
        redirects: redirects
            .iter()
            .map(|(k, v)| (k.clone(), v.0.clone()))
            .collect(),
        ..cx
    };
    let mut seen = BTreeSet::new();
    for (key, shown, text, is_readme) in &pages {
        seen.insert(key.clone());
        let errs = page_errors(&cx, shown, text, *is_readme);
        if baseline.contains(key) {
            if errs.is_empty() {
                errors.push(CheckError::new(
                    shown.clone(),
                    "docs-baseline",
                    format!("`{key}` passes now: delete its line from {BASELINE_FILE}"),
                ));
            }
        } else {
            errors.extend(errs);
        }
    }
    let page_meta: BTreeMap<String, (Option<String>, Vec<String>)> = pages
        .iter()
        .map(|(key, _, text, readme)| {
            let (fm, _) = split(text, *readme);
            let fm: serde_yaml::Value = fm
                .and_then(|y| serde_yaml::from_str(&y).ok())
                .unwrap_or(serde_yaml::Value::Null);
            let st = fm
                .get("status")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let pr = fm
                .get("proves")
                .and_then(|v| v.as_sequence())
                .map(|s| {
                    s.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            (key.clone(), (st, pr))
        })
        .collect();
    let ff = root.join(FEATURES_FILE).display().to_string();
    let mut ids = BTreeSet::new();
    for ft in load_features(root, &mut errors) {
        let mut found: Vec<String> = Vec::new();
        if !ids.insert(ft.id.clone()) {
            found.push("listed twice".into());
        }
        let has_reason = ft.unproven.as_deref().is_some_and(|u| !u.trim().is_empty());
        match (ft.proves.is_empty(), has_reason) {
            (true, false) => found.push("needs `proves` or `unproven: <reason>`".into()),
            (false, true) => found.push("has both `proves` and `unproven`".into()),
            _ => {}
        }
        for r in &ft.proves {
            if let Some(m) = proof_problem(&cx, r, false) {
                found.push(format!("proof: {m}"));
            }
        }
        let mut all_exist = true;
        for slug in ft.dev.iter().chain(ft.arch.iter()) {
            if !page_exists(&keys, slug) {
                all_exist = false;
                if !ft.pending_page {
                    found.push(format!(
                        "page `{slug}` does not exist (set `pending_page: true` until it is written)"
                    ));
                }
                continue;
            }
            let key = ["md", "mdx"]
                .iter()
                .flat_map(|e| [format!("{slug}.{e}"), format!("{slug}/index.{e}")])
                .find(|k| keys.contains(k))
                .unwrap();
            if baseline.contains(&key) {
                continue;
            }
            let (st, pr) = &page_meta[&key];
            if st.as_deref() != Some("shipped") {
                found.push(format!(
                    "page `{slug}` has left the baseline and must be `shipped`"
                ));
            } else if !ft.proves.is_empty() && !ft.proves.iter().any(|p| pr.contains(p)) {
                found.push(format!(
                    "page `{slug}` is shipped but its `proves` names none of the feature's proofs ({})",
                    ft.proves.join(", ")
                ));
            }
        }
        if ft.pending_page && all_exist && !ft.dev.is_empty() && !ft.arch.is_empty() {
            found.push("`pending_page` is set but every page exists: delete the flag".into());
        }
        for m in found {
            errors.push(CheckError::new(
                ff.clone(),
                "docs-feature",
                format!("`{}`: {m}", ft.id),
            ));
        }
    }
    for b in &baseline {
        if !seen.contains(b) {
            errors.push(CheckError::new(
                bf.display().to_string(),
                "docs-baseline",
                format!("`{b}` is listed but there is no such page"),
            ));
        }
    }
    errors
}

/// Number of pages listed in the baseline under `root` (0 when the file is absent).
pub fn baseline_len(root: &Path) -> usize {
    std::fs::read_to_string(root.join(BASELINE_FILE))
        .map(|t| {
            t.lines()
                .map(str::trim)
                .filter(|l| !l.is_empty() && !l.starts_with('#'))
                .count()
        })
        .unwrap_or(0)
}
