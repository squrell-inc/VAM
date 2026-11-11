//! The marketplace index (`marketplace.json`) and the offline check of a marketplace root.
//!
//! A marketplace is a Git repository (design §9.1). At its root, `marketplace.json` lists
//! listings; each listing names the artifact directories (relative to the root) it installs and
//! the tag (`ref`) it is released at:
//!
//! ```json
//! { "name": "Squrell", "publisherDomain": "hub.squrell.cloud",
//!   "listings": [ { "id": "microsoft-365", "displayName": "Microsoft 365",
//!     "description": "…", "connector": "urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365",
//!     "artifacts": [ { "kind": "skill", "path": "listings/microsoft-365/skills/m365-mail" } ],
//!     "ref": "microsoft-365-v1.0.0", "requiresNode": ">=0.1.0" } ] }
//! ```
//!
//! Parsing and the rules here need no node: marketplace CI, `vam catalog validate` and vamd (when
//! it refreshes a marketplace, CN-4.4) call the same functions. What depends on the node (which
//! domains it admits, whether a marketplace is Squrell's) is checked at import (CN-4.5).

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::package::{
    check_dir, read_package_file, ArtifactKind, CapabilitySummary, CheckedPackage, PackageError,
};
use crate::validation::{is_registry, is_slug};

/// The index file at the root of a marketplace repository.
pub const INDEX_FILE: &str = "marketplace.json";
/// Largest index a node reads.
pub const MAX_INDEX_BYTES: usize = 256 * 1024;
pub const MAX_LISTINGS: usize = 200;
pub const MAX_ARTIFACTS_PER_LISTING: usize = 32;
const MAX_TEXT: usize = 2000;
const MAX_NAME: usize = 120;
const MAX_REF: usize = 128;
const MAX_PATH: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct MarketplaceIndex {
    pub name: String,
    pub publisher_domain: String,
    pub listings: Vec<Listing>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Listing {
    pub id: String,
    pub display_name: String,
    pub description: String,
    /// The connector the listing needs enabled (a URN). A listing from an Unverified marketplace
    /// may name only a connector it ships itself (an artifact of kind `connector`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    pub artifacts: Vec<ListingArtifact>,
    /// The Git tag the listing is installed at.
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// Node versions that can run the listing, e.g. `>=0.1.0`.
    pub requires_node: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct ListingArtifact {
    pub kind: ArtifactKind,
    /// Directory of the artifact, relative to the repository root.
    pub path: String,
}

/// A problem with the index. `listing` names the listing it is about, when it is about one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IndexProblem {
    pub listing: Option<String>,
    pub message: String,
}

impl std::fmt::Display for IndexProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.listing {
            Some(l) => write!(f, "listing '{l}': {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// Parse `marketplace.json` text and check every rule that needs no files. All problems are
/// returned, not the first. Text over [`MAX_INDEX_BYTES`] or not JSON is one problem.
pub fn parse_index(raw: &str) -> Result<MarketplaceIndex, Vec<IndexProblem>> {
    let problem = |message: String| {
        vec![IndexProblem {
            listing: None,
            message,
        }]
    };
    if raw.len() > MAX_INDEX_BYTES {
        return Err(problem(format!(
            "{INDEX_FILE} is larger than {MAX_INDEX_BYTES} bytes"
        )));
    }
    let index: MarketplaceIndex = serde_json::from_str(raw)
        .map_err(|e| problem(format!("{INDEX_FILE} is not a valid index: {e}")))?;
    let problems = validate_index(&index);
    if problems.is_empty() {
        Ok(index)
    } else {
        Err(problems)
    }
}

/// Every rule of the index that needs no files.
pub fn validate_index(index: &MarketplaceIndex) -> Vec<IndexProblem> {
    let mut out: Vec<IndexProblem> = Vec::new();
    let mut top = |message: String| {
        out.push(IndexProblem {
            listing: None,
            message,
        })
    };
    if index.name.trim().is_empty() || index.name.len() > MAX_NAME {
        top(format!("name must be 1 to {MAX_NAME} characters"));
    }
    if !is_registry(&index.publisher_domain) {
        top(format!(
            "publisherDomain '{}' is not a domain: lowercase labels of letters, digits and '-' separated by '.'",
            index.publisher_domain
        ));
    }
    if index.listings.len() > MAX_LISTINGS {
        top(format!("at most {MAX_LISTINGS} listings"));
    }
    let mut seen: Vec<&str> = Vec::new();
    for l in &index.listings {
        if seen.contains(&l.id.as_str()) {
            top(format!("listing id '{}' appears twice", l.id));
        }
        seen.push(&l.id);
    }
    for l in &index.listings {
        for message in listing_problems(l) {
            out.push(IndexProblem {
                listing: Some(l.id.clone()),
                message,
            });
        }
    }
    out
}

fn listing_problems(l: &Listing) -> Vec<String> {
    let mut out = Vec::new();
    if !is_slug(&l.id) || l.id.len() > 64 {
        out.push(format!(
            "id '{}' must be a lowercase slug (letters, digits, '.', '_', '-'; at most 64)",
            l.id
        ));
    }
    if l.display_name.trim().is_empty() || l.display_name.len() > MAX_NAME {
        out.push(format!("displayName must be 1 to {MAX_NAME} characters"));
    }
    if l.description.trim().is_empty() || l.description.len() > MAX_TEXT {
        out.push(format!("description must be 1 to {MAX_TEXT} characters"));
    }
    if let Some(c) = &l.connector {
        if !crate::connector::valid_urn(c) {
            out.push(format!(
                "connector '{c}' must be urn:vam:connector:<domain>/<publisher>/<name>"
            ));
        }
    }
    if l.artifacts.is_empty() {
        out.push("artifacts must list at least one artifact".into());
    }
    if l.artifacts.len() > MAX_ARTIFACTS_PER_LISTING {
        out.push(format!(
            "at most {MAX_ARTIFACTS_PER_LISTING} artifacts per listing"
        ));
    }
    let mut paths: Vec<&str> = Vec::new();
    for a in &l.artifacts {
        if let Err(why) = check_relative_path(&a.path) {
            out.push(format!("artifact path '{}': {why}", a.path));
        }
        if paths.contains(&a.path.as_str()) {
            out.push(format!("artifact path '{}' appears twice", a.path));
        }
        paths.push(&a.path);
    }
    if let Err(why) = check_git_ref(&l.git_ref) {
        out.push(format!("ref '{}': {why}", l.git_ref));
    }
    if let Err(why) = NodeRequirement::parse(&l.requires_node) {
        out.push(format!("requiresNode '{}': {why}", l.requires_node));
    }
    out
}

/// A path inside the repository: relative, `/`-separated, no `.`/`..`/empty segment, no
/// backslash, no control character.
pub fn check_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.len() > MAX_PATH {
        return Err(format!("must be 1 to {MAX_PATH} characters"));
    }
    if path.starts_with('/') || path.contains('\\') || path.chars().any(char::is_control) {
        return Err("must be a relative path with '/' separators".into());
    }
    if path
        .split('/')
        .any(|seg| seg.is_empty() || seg == "." || seg == "..")
    {
        return Err("must not hold an empty, '.' or '..' segment".into());
    }
    Ok(())
}

/// A tag name that is safe to hand to Git: no leading '-', no `..`, no whitespace or ref
/// metacharacters, no `.lock` ending, not a bare commit id.
pub fn check_git_ref(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_REF {
        return Err(format!("must be 1 to {MAX_REF} characters"));
    }
    let ok_chars = name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b"._-/".contains(&b));
    if !ok_chars {
        return Err("may hold only letters, digits, '.', '_', '-' and '/'".into());
    }
    if name.starts_with(['-', '/', '.'])
        || name.ends_with(['/', '.'])
        || name.contains("..")
        || name.contains("//")
        || name.contains("/.")
        || name.ends_with(".lock")
    {
        return Err("is not a valid tag name".into());
    }
    if name.len() == 40 && name.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err("must be a tag, not a commit id".into());
    }
    Ok(())
}

// ---- requiresNode ------------------------------------------------------------------------------

/// A node version requirement: comma-separated comparators (`>=`, `>`, `<=`, `<`, `=`) each with
/// `X`, `X.Y` or `X.Y.Z`, e.g. `>=0.1.0` or `>=0.2, <1`. Pre-release and build suffixes of the
/// node version are ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeRequirement {
    parts: Vec<(Op, [u64; 3])>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
}

impl NodeRequirement {
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut parts = Vec::new();
        for raw in text.split(',') {
            let raw = raw.trim();
            let (op, rest) = if let Some(r) = raw.strip_prefix(">=") {
                (Op::Ge, r)
            } else if let Some(r) = raw.strip_prefix("<=") {
                (Op::Le, r)
            } else if let Some(r) = raw.strip_prefix('>') {
                (Op::Gt, r)
            } else if let Some(r) = raw.strip_prefix('<') {
                (Op::Lt, r)
            } else if let Some(r) = raw.strip_prefix('=') {
                (Op::Eq, r)
            } else {
                return Err(
                    "each comparator starts with >=, >, <=, < or = (for example >=0.1.0)".into(),
                );
            };
            parts.push((op, parse_version(rest.trim())?));
        }
        if parts.is_empty() {
            return Err("is empty".into());
        }
        Ok(Self { parts })
    }

    /// True when `node_version` (`X.Y.Z`, optionally with a `-pre` or `+build` suffix) satisfies
    /// every comparator. A version that does not parse satisfies nothing.
    pub fn matches(&self, node_version: &str) -> bool {
        let core = node_version
            .split(['-', '+'])
            .next()
            .unwrap_or(node_version);
        let Ok(v) = parse_version(core) else {
            return false;
        };
        self.parts.iter().all(|(op, want)| match op {
            Op::Ge => v >= *want,
            Op::Gt => v > *want,
            Op::Le => v <= *want,
            Op::Lt => v < *want,
            Op::Eq => v == *want,
        })
    }
}

fn parse_version(text: &str) -> Result<[u64; 3], String> {
    let nums: Vec<&str> = text.split('.').collect();
    if nums.is_empty() || nums.len() > 3 {
        return Err(format!("'{text}' is not a version (X, X.Y or X.Y.Z)"));
    }
    let mut out = [0u64; 3];
    for (i, n) in nums.iter().enumerate() {
        if n.is_empty() || n.len() > 9 || !n.bytes().all(|b| b.is_ascii_digit()) {
            return Err(format!("'{text}' is not a version (X, X.Y or X.Y.Z)"));
        }
        out[i] = n.parse::<u64>().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

// ---- the check of a marketplace root ------------------------------------------------------------

/// How serious a finding is. `vam catalog validate` exits non-zero on an error, and on a warning
/// only with `--strict`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Error,
    Warning,
}

/// One line of a report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    pub level: Level,
    /// The listing id, when the finding is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub listing: Option<String>,
    /// The artifact directory (relative to the root), when the finding is about one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub message: String,
}

impl Finding {
    pub fn error(listing: Option<&str>, path: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Error,
            listing: listing.map(str::to_string),
            path: path.map(str::to_string),
            message: message.into(),
        }
    }

    pub fn warning(listing: Option<&str>, path: Option<&str>, message: impl Into<String>) -> Self {
        Self {
            level: Level::Warning,
            ..Self::error(listing, path, message)
        }
    }
}

/// One artifact that passed.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CheckedArtifact {
    pub listing: String,
    pub path: String,
    pub kind: ArtifactKind,
    pub urn: String,
    pub summary: CapabilitySummary,
}

/// The result of checking a marketplace root.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
pub struct MarketplaceReport {
    pub findings: Vec<Finding>,
    pub artifacts: Vec<CheckedArtifact>,
}

impl MarketplaceReport {
    pub fn errors(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.level == Level::Error)
            .count()
    }
    pub fn warnings(&self) -> usize {
        self.findings
            .iter()
            .filter(|f| f.level == Level::Warning)
            .count()
    }
}

/// The URN domain of `urn:vam:<kind>:<domain>/<namespace>/<name>`.
pub fn urn_domain(urn: &str) -> Option<&str> {
    let rest = urn.strip_prefix("urn:vam:")?;
    let (_kind, rest) = rest.split_once(':')?;
    let (domain, _) = rest.split_once('/')?;
    Some(domain)
}

/// Resolve `rel` under `root` without following a symlink on the way: every component must be a
/// real directory. Returns the directory, or the reason it cannot be used.
pub fn resolve_artifact_dir(root: &Path, rel: &str) -> Result<PathBuf, String> {
    check_relative_path(rel)?;
    let mut cur = root.to_path_buf();
    for comp in Path::new(rel).components() {
        let Component::Normal(seg) = comp else {
            return Err("must be a plain relative path".into());
        };
        cur.push(seg);
        let meta = fs::symlink_metadata(&cur).map_err(|_| "does not exist".to_string())?;
        if meta.file_type().is_symlink() {
            return Err("must not pass through a symlink".into());
        }
        if !meta.is_dir() {
            return Err("is not a directory".into());
        }
    }
    Ok(cur)
}

/// Check a marketplace root: the index, then every artifact of every listing with the package
/// check of its kind. One listing's problem never hides another's. Domain consistency (an
/// artifact URN outside `publisherDomain`, which a node refuses at install) is a warning here.
pub fn check_marketplace_root(root: &Path) -> MarketplaceReport {
    let mut report = MarketplaceReport::default();
    let index_path = root.join(INDEX_FILE);
    let raw = match read_package_file(&index_path) {
        Ok(raw) => raw,
        Err(PackageError(msg)) => {
            report.findings.push(Finding::error(None, None, msg));
            return report;
        }
    };
    let index = match parse_index(&raw) {
        Ok(index) => index,
        Err(problems) => {
            for p in problems {
                report
                    .findings
                    .push(Finding::error(p.listing.as_deref(), None, p.message));
            }
            return report;
        }
    };
    for listing in &index.listings {
        for a in &listing.artifacts {
            check_listing_artifact(root, &index, listing, a, &mut report);
        }
    }
    report
}

fn check_listing_artifact(
    root: &Path,
    index: &MarketplaceIndex,
    listing: &Listing,
    a: &ListingArtifact,
    report: &mut MarketplaceReport,
) {
    let lid = Some(listing.id.as_str());
    let path = Some(a.path.as_str());
    let dir = match resolve_artifact_dir(root, &a.path) {
        Ok(dir) => dir,
        Err(why) => {
            report
                .findings
                .push(Finding::error(lid, path, format!("artifact path {why}")));
            return;
        }
    };
    let checked: CheckedPackage = match check_dir(&dir, Some(a.kind)) {
        Ok(c) => c,
        Err(PackageError(msg)) => {
            report.findings.push(Finding::error(lid, path, msg));
            return;
        }
    };
    for w in &checked.warnings {
        report.findings.push(Finding::warning(lid, path, w.clone()));
    }
    if urn_domain(&checked.urn) != Some(index.publisher_domain.as_str()) {
        report.findings.push(Finding::warning(
            lid,
            path,
            format!(
                "URN '{}' is outside publisherDomain '{}': a node refuses it at install",
                checked.urn, index.publisher_domain
            ),
        ));
    }
    report.artifacts.push(CheckedArtifact {
        listing: listing.id.clone(),
        path: a.path.clone(),
        kind: checked.kind,
        urn: checked.urn,
        summary: checked.summary,
    });
}

#[cfg(test)]
mod tests;
