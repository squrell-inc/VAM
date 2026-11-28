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
//!     "ref": "microsoft-365-v1.0.0", "requiresNode": ">=0.1.0",
//!     "access": { "default": { "visibility": "org", "mode": "open" } } } ] }
//! ```
//!
//! `access` is optional. It is the access an agent of the listing starts with on its first install
//! (visibility `org` or `restricted`, mode `everyone`, `open` or `approval`); without it a first
//! install starts as `org` + `approval`. A `restricted` agent cannot be open to `everyone`.
//!
//! Parsing and the rules here need no node: marketplace CI, `vam catalog validate` and vamd (when
//! it refreshes a marketplace, CN-4.4) call the same functions. What depends on the node (which
//! domains it admits, whether a marketplace is Squrell's) is checked at import (CN-4.5).

use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::connector::{
    connectors_from_files, templated_suffix, Connector, BUILTIN_CONNECTOR_FILES,
};
use crate::connector_kit::TEST_FILE_PATH;
use crate::connector_listing::{connector_id_of_urn, placeholders_overlap};
use crate::package::{
    check_dir, read_package_file, ArtifactKind, CapabilitySummary, CheckedPackage, CheckedSpec,
    PackageError,
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
    /// The access the agents of the listing start with on their first install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub access: Option<ListingAccess>,
}

/// The `access` object of a listing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ListingAccess {
    pub default: AccessDefault,
}

/// The access an installed agent starts with: who can see it and how a member gets to use it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessDefault {
    pub visibility: AccessVisibility,
    pub mode: AccessMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessVisibility {
    Org,
    Restricted,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessMode {
    Everyone,
    Open,
    Approval,
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
    if let Some(a) = &l.access {
        if a.default.visibility == AccessVisibility::Restricted
            && a.default.mode == AccessMode::Everyone
        {
            out.push(
                "access.default: a restricted agent cannot be open to everyone: use mode 'open' or 'approval'"
                    .into(),
            );
        }
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
/// check of its kind, then each listing as a whole ([`check_listing`]). One listing's problem never
/// hides another's. Domain consistency (an artifact URN outside `publisherDomain`, which a node
/// refuses at install) is a warning here.
pub fn check_marketplace_root(root: &Path) -> MarketplaceReport {
    check_marketplace(root, None)
}

/// Like [`check_marketplace_root`], for the one listing `only` of the index (`vam connector
/// validate` on a listing directory). A listing the index does not name is an error.
pub fn check_marketplace_listing(root: &Path, only: &str) -> MarketplaceReport {
    check_marketplace(root, Some(only))
}

fn check_marketplace(root: &Path, only: Option<&str>) -> MarketplaceReport {
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
    if let Some(id) = only {
        if !index.listings.iter().any(|l| l.id == id) {
            report.findings.push(Finding::error(
                None,
                None,
                format!("the marketplace index names no listing `{id}`"),
            ));
            return report;
        }
    }
    for listing in index
        .listings
        .iter()
        .filter(|l| only.is_none_or(|id| l.id == id))
    {
        let before = report.findings.len();
        let mut checked = Vec::new();
        for a in &listing.artifacts {
            if let Some(c) = check_listing_artifact(root, &index, listing, a, &mut report) {
                checked.push(c);
            }
        }
        let artifact_errors = report.findings[before..]
            .iter()
            .any(|f| f.level == Level::Error);
        // The listing's own rules need every one of its artifacts: with one missing they would
        // report what that artifact's own error already says.
        if !artifact_errors {
            report.findings.extend(check_listing(&listing.id, &checked));
            if checked.iter().any(|c| c.kind == ArtifactKind::Connector)
                && !test_file_exists(root, &listing.id)
            {
                report.findings.push(Finding::warning(
                    Some(&listing.id),
                    None,
                    format!(
                        "the listing ships a connector but no listings/{}/{TEST_FILE_PATH}: `vam connector test` has nothing to run",
                        listing.id
                    ),
                ));
            }
        }
    }
    report
}

fn test_file_exists(root: &Path, listing_id: &str) -> bool {
    fs::symlink_metadata(root.join("listings").join(listing_id).join(TEST_FILE_PATH))
        .is_ok_and(|m| m.is_file())
}

fn check_listing_artifact(
    root: &Path,
    index: &MarketplaceIndex,
    listing: &Listing,
    a: &ListingArtifact,
    report: &mut MarketplaceReport,
) -> Option<CheckedPackage> {
    let lid = Some(listing.id.as_str());
    let path = Some(a.path.as_str());
    let dir = match resolve_artifact_dir(root, &a.path) {
        Ok(dir) => dir,
        Err(why) => {
            report
                .findings
                .push(Finding::error(lid, path, format!("artifact path {why}")));
            return None;
        }
    };
    let checked: CheckedPackage = match check_dir(&dir, Some(a.kind)) {
        Ok(c) => c,
        Err(PackageError(msg)) => {
            report.findings.push(Finding::error(lid, path, msg));
            return None;
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
        urn: checked.urn.clone(),
        summary: checked.summary.clone(),
    });
    Some(checked)
}

// ---- the listing as a whole ---------------------------------------------------------------------

/// The rules that look at a listing's artifacts together, once each passed its own check. One
/// function for `vam connector validate` ([`check_marketplace_root`]) and for vamd's import, so an
/// offline pass is not an install refusal. Errors come in the order import refuses them (the first
/// error is the one an install names); a warning is for what only a node can decide.
///
/// 1. A skill's company address host belongs to the connector it requires.
/// 2. No two connectors have binding placeholders the gate cannot tell apart.
/// 3. One URN is one artifact.
/// 4. No two connectors have the same id.
/// 5. Each tool's `egress[].binding` names a binding of the skill's `requiresConnector`, and
///    `egress[].host` is one of that binding's hosts. Checked when that connector ships in the
///    listing or is built in; otherwise a warning (the gate decides at call time).
pub fn check_listing(listing_id: &str, packages: &[CheckedPackage]) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut err = |message: String| out.push(Finding::error(Some(listing_id), None, message));
    check_templated_skill_hosts(packages, listing_id, &mut err);
    check_listing_placeholders(packages, listing_id, &mut err);
    let mut seen = std::collections::BTreeSet::new();
    for p in packages {
        if !seen.insert(p.urn.as_str()) {
            err(format!("listing `{listing_id}` names {} twice", p.urn));
        }
    }
    check_listing_connector_ids(packages, listing_id, &mut err);
    let mut warnings = Vec::new();
    check_skill_egress_bindings(packages, listing_id, &mut err, &mut warnings);
    out.extend(
        warnings
            .into_iter()
            .map(|w| Finding::warning(Some(listing_id), None, w)),
    );
    out
}

/// A skill that names a company address host (`{subdomain}.<suffix>`) must require a connector this
/// listing ships whose own company address has that suffix: a listing cannot aim a skill at a host
/// its connector does not own (T1, T2).
fn check_templated_skill_hosts(
    packages: &[CheckedPackage],
    listing_id: &str,
    err: &mut impl FnMut(String),
) {
    for p in packages.iter().filter(|p| p.kind == ArtifactKind::Skill) {
        let hosts: Vec<&str> = p
            .summary
            .tools
            .iter()
            .flat_map(|t| t.egress.iter().map(|e| e.host.as_str()))
            .filter(|h| h.contains('{'))
            .collect();
        if hosts.is_empty() {
            continue;
        }
        let required = &p.summary.requires_connector;
        let suffix = packages.iter().find_map(|c| match &c.spec {
            CheckedSpec::Connector(def) if &c.urn == required => {
                def.tenant.as_ref().map(|t| t.suffix.as_str())
            }
            _ => None,
        });
        for h in hosts {
            let ok = suffix.is_some_and(|sfx| templated_suffix(h).is_some_and(|got| got == sfx));
            if !ok {
                err(format!(
                    "listing `{listing_id}`: skill {} names the host `{h}`, which is not the company address ({}) of the connector it requires: a templated host must be `{{subdomain}}.<suffix>` of the connector the listing ships",
                    p.urn,
                    suffix.map_or_else(
                        || "no connector of this listing with a company address".to_string(),
                        |sfx| format!("suffix `{sfx}`")
                    )
                ));
            }
        }
    }
}

/// No two connectors of one listing declare binding placeholders the egress gate cannot tell apart
/// (the same one, or one inside the other: [`placeholders_overlap`]). Each is compared with the
/// installed connectors at write time (`admit_connector_id`), but the whole listing is planned
/// before any of it is inserted, so two of its own connectors would pass that check together.
fn check_listing_placeholders(
    packages: &[CheckedPackage],
    listing_id: &str,
    err: &mut impl FnMut(String),
) {
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for p in packages {
        let CheckedSpec::Connector(def) = &p.spec else {
            continue;
        };
        for b in def.bindings.iter().flatten() {
            // A connector's own bindings are compared by the file check.
            if let Some((ph, first)) = seen
                .iter()
                .find(|(ph, urn)| *urn != def.urn && placeholders_overlap(ph, &b.placeholder))
            {
                err(format!(
                    "listing `{listing_id}`: the connectors {first} and {} declare the binding placeholders `{ph}` and `{}`, which the gate cannot tell apart: a placeholder must name one binding",
                    def.urn, b.placeholder
                ));
                return;
            }
        }
        for b in def.bindings.iter().flatten() {
            seen.push((b.placeholder.as_str(), def.urn.as_str()));
        }
    }
}

/// No two connectors of one listing have the same id (the last segment of the URN): an id names one
/// connector on the node. Each is compared with the installed connectors at write time
/// (`admit_connector_id`), but neither of two new ones from one listing is installed yet.
fn check_listing_connector_ids(
    packages: &[CheckedPackage],
    listing_id: &str,
    err: &mut impl FnMut(String),
) {
    let mut ids: std::collections::BTreeMap<&str, &str> = std::collections::BTreeMap::new();
    for p in packages {
        if !matches!(p.spec, CheckedSpec::Connector(_)) {
            continue;
        }
        let urn = p.urn.as_str();
        if let Some(first) = ids.insert(connector_id_of_urn(urn), urn) {
            err(format!(
                "listing `{listing_id}`: the connectors {first} and {urn} have the same id `{}`: an id names one connector",
                connector_id_of_urn(urn)
            ));
            return;
        }
    }
}

/// Each tool's egress names a binding of the connector its skill requires, and one of that
/// binding's hosts. A templated host (`{subdomain}.<suffix>`) is compared as written with the
/// binding's, when the listing ships the connector with that company address; any other templated
/// host is [`check_templated_skill_hosts`]'s.
fn check_skill_egress_bindings(
    packages: &[CheckedPackage],
    listing_id: &str,
    err: &mut impl FnMut(String),
    warnings: &mut Vec<String>,
) {
    let mut builtin: Option<Vec<Connector>> = None;
    for p in packages.iter().filter(|p| p.kind == ArtifactKind::Skill) {
        let required = p.summary.requires_connector.as_str();
        if required.is_empty() || p.summary.tools.iter().all(|t| t.egress.is_empty()) {
            continue;
        }
        let shipped = packages.iter().find_map(|c| match &c.spec {
            CheckedSpec::Connector(def) if c.urn == required => Some(def.as_ref()),
            _ => None,
        });
        let def = match shipped {
            Some(def) => def,
            None => {
                let all = builtin.get_or_insert_with(|| {
                    connectors_from_files(BUILTIN_CONNECTOR_FILES).unwrap_or_default()
                });
                match all.iter().find(|c| c.urn == required) {
                    Some(found) => found,
                    None => {
                        warnings.push(format!(
                            "skill {} requires the connector {required}, which this listing does not ship and no node builds in: its egress bindings and hosts cannot be checked offline",
                            p.urn
                        ));
                        continue;
                    }
                }
            }
        };
        let bindings = def.bindings.as_deref().unwrap_or_default();
        for t in &p.summary.tools {
            for e in &t.egress {
                if e.host.contains('{') {
                    let ours = shipped.is_some()
                        && def.tenant.as_ref().is_some_and(|tn| {
                            templated_suffix(&e.host) == Some(tn.suffix.as_str())
                        });
                    if !ours {
                        continue;
                    }
                }
                let Some(b) = bindings.iter().find(|b| b.name == e.binding) else {
                    let declared = if bindings.is_empty() {
                        "it declares no bindings".to_string()
                    } else {
                        format!(
                            "its bindings: {}",
                            bindings
                                .iter()
                                .map(|b| format!("`{}`", b.name))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    };
                    err(format!(
                        "listing `{listing_id}`: skill {} tool `{}` names the binding `{}` for the host `{}`, which the connector it requires ({required}) does not declare ({declared}): a tool's egress must name a binding of its connector",
                        p.urn, t.name, e.binding, e.host
                    ));
                    continue;
                };
                if !b.hosts.contains(&e.host) {
                    err(format!(
                        "listing `{listing_id}`: skill {} tool `{}` names the host `{}` on the binding `{}`, which that binding of {required} does not list (its hosts: {}): a tool's egress host must be one of its binding's hosts",
                        p.urn,
                        t.name,
                        e.host,
                        e.binding,
                        b.hosts
                            .iter()
                            .map(|h| format!("`{h}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
