//! Offline package checks, one per artifact kind (skill, agent, playbook), and the capability
//! summary of what a package can do.
//!
//! This is the check vamd runs when a skill, agent or playbook is published, moved here so that
//! `vam catalog validate`, marketplace CI and the catalog import (CN-4.5) run the same code and
//! give the same messages. Nothing here reads a database, a registry or the network.
//!
//! Layout of a package directory, which is what [`detect_kind`] looks for:
//!
//! | Kind | Marker file | Contents |
//! |---|---|---|
//! | skill | `SKILLS.md` | `manifest.json`, `SKILLS.md`, tool entry points, `jigs/*.json` cards |
//! | agent | `agent.json` | `{ "spec": <AgentSpec> }`, the body of `PUT /v1/agents` |
//! | playbook | `playbook.json` | `{ "spec": <PlaybookSpec> }`, the body of `PUT /v1/playbooks` |
//! | connector | `connector.json` | the connector file (`spec/connectors/*.json` format), CN-4.11 |
//!
//! Node policy is not checked here: which registry domains a node admits is the node's rule, and
//! vamd applies it between [`load_skill_manifest`] and [`check_skills_md`].

use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::connector::Connector;
use crate::encoding::json_to_proto;
use crate::v1::{
    AgentPermissions, AgentSpec, HostDefault, InternetPermissions, PermissionDecision,
    PlaybookSpec, PutAgentRequest, PutPlaybookRequest, SideEffectClass, SkillManifest, ToolDefault,
    ToolPermissions,
};
use crate::validation::{parse_skill_urn, Validate};

/// Largest JSON or Markdown file a package check reads.
pub const MAX_PACKAGE_FILE_BYTES: u64 = 1024 * 1024;

/// Marker files, one per artifact kind.
pub const SKILL_MARKER: &str = "SKILLS.md";
/// The name community skills use for the same file. Read wherever `SKILLS.md` is read.
pub const SKILL_MARKER_COMMUNITY: &str = "SKILL.md";

/// The skill instructions file in `dir`: `SKILLS.md`, else `SKILL.md`, else `None`.
///
/// Precedence: when both exist, `SKILLS.md` wins and `SKILL.md` is ignored (it is then just
/// another file in the package). `SKILLS.md` is VAM's own name, so a package that carries both
/// keeps exactly the behaviour, frontmatter check and digest it had before `SKILL.md` was read.
/// Only regular files count; a symlink or directory with either name is not a skill file.
pub fn skill_doc(dir: &Path) -> Option<std::path::PathBuf> {
    [SKILL_MARKER, SKILL_MARKER_COMMUNITY]
        .into_iter()
        .map(|name| dir.join(name))
        .find(|path| is_file(path))
}
pub const AGENT_FILE: &str = "agent.json";
pub const PLAYBOOK_FILE: &str = "playbook.json";
pub const CONNECTOR_FILE: &str = "connector.json";

/// What a package holds. Import code is generic over this: a kind is a row of
/// [`ArtifactKind::ALL`] with its marker file, its check and its summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ArtifactKind {
    Skill,
    Agent,
    Playbook,
    /// A connector file shipped in a listing (CN-4.11); see [`crate::connector_listing`].
    Connector,
}

impl ArtifactKind {
    pub const ALL: [ArtifactKind; 4] = [
        ArtifactKind::Skill,
        ArtifactKind::Agent,
        ArtifactKind::Playbook,
        ArtifactKind::Connector,
    ];

    /// The word used in the index (`"kind"`) and in the URN (`urn:vam:<word>:…`).
    pub fn as_str(self) -> &'static str {
        match self {
            ArtifactKind::Skill => "skill",
            ArtifactKind::Agent => "agent",
            ArtifactKind::Playbook => "playbook",
            ArtifactKind::Connector => "connector",
        }
    }

    pub fn parse(word: &str) -> Option<ArtifactKind> {
        ArtifactKind::ALL.into_iter().find(|k| k.as_str() == word)
    }

    /// Whether `dir` holds this kind's marker (for a skill, `SKILLS.md` or `SKILL.md`).
    pub fn marker_in(self, dir: &Path) -> bool {
        match self {
            ArtifactKind::Skill => skill_doc(dir).is_some(),
            other => is_file(&dir.join(other.marker_file())),
        }
    }

    /// The file whose presence says a directory holds this kind.
    pub fn marker_file(self) -> &'static str {
        match self {
            ArtifactKind::Skill => SKILL_MARKER,
            ArtifactKind::Agent => AGENT_FILE,
            ArtifactKind::Playbook => PLAYBOOK_FILE,
            ArtifactKind::Connector => CONNECTOR_FILE,
        }
    }
}

/// A package that does not pass its check. The message is the whole story and is shown as is
/// (at publish it is the body of a 400).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PackageError(pub String);

fn err<T>(message: impl Into<String>) -> Result<T, PackageError> {
    Err(PackageError(message.into()))
}

/// The parsed, validated spec of a checked package. One value per package, built once and read;
/// boxing the agent would only add an indirection to every match.
#[derive(Debug, Clone, PartialEq)]
#[allow(clippy::large_enum_variant)]
pub enum CheckedSpec {
    Skill(SkillManifest),
    Agent(AgentSpec),
    Playbook(PlaybookSpec),
    Connector(Box<Connector>),
}

/// A package that passed its check.
#[derive(Debug, Clone, PartialEq)]
pub struct CheckedPackage {
    pub kind: ArtifactKind,
    pub urn: String,
    /// `X.Y.Z` for a skill; empty for an agent or playbook (the catalog assigns the release).
    pub version: String,
    pub spec: CheckedSpec,
    pub summary: CapabilitySummary,
    /// Publish warnings; the package is still accepted.
    pub warnings: Vec<String>,
}

// ---- reading files -----------------------------------------------------------------------------

/// Read a regular file of at most [`MAX_PACKAGE_FILE_BYTES`]. A symlink is refused, so a package
/// cannot make a check read outside itself.
pub fn read_package_file(path: &Path) -> Result<String, PackageError> {
    let shown = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let meta =
        fs::symlink_metadata(path).map_err(|_| PackageError(format!("{shown} is missing")))?;
    if !meta.is_file() {
        return err(format!("{shown} must be a regular file (no symlink)"));
    }
    if meta.len() > MAX_PACKAGE_FILE_BYTES {
        return err(format!(
            "{shown} is larger than {MAX_PACKAGE_FILE_BYTES} bytes"
        ));
    }
    fs::read_to_string(path).map_err(|e| PackageError(format!("{shown} is unreadable: {e}")))
}

fn is_file(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|m| m.is_file())
}

// ---- kind detection ----------------------------------------------------------------------------

/// The kind of the package in `dir`, from its marker file. A directory with none, or with the
/// markers of two kinds, is an error.
pub fn detect_kind(dir: &Path) -> Result<ArtifactKind, PackageError> {
    if !dir.is_dir() {
        return err(format!("{} is not a directory", dir.display()));
    }
    let found: Vec<ArtifactKind> = ArtifactKind::ALL
        .into_iter()
        .filter(|k| k.marker_in(dir))
        .collect();
    match found.as_slice() {
        [] => err(format!(
            "{} holds no artifact: expected {}, {}, {} or {}",
            dir.display(),
            SKILL_MARKER,
            AGENT_FILE,
            PLAYBOOK_FILE,
            CONNECTOR_FILE
        )),
        [one] => Ok(*one),
        many => err(format!(
            "{} holds more than one artifact ({}): one directory, one artifact",
            dir.display(),
            many.iter()
                .map(|k| k.marker_file())
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// Check the package in `dir`. `expect` is the kind the caller was told (a marketplace index
/// names it); a directory of another kind is an error. Without it the kind is detected.
pub fn check_dir(dir: &Path, expect: Option<ArtifactKind>) -> Result<CheckedPackage, PackageError> {
    let kind = match expect {
        Some(kind) => {
            if !kind.marker_in(dir) {
                return err(format!(
                    "expected a {} but {} has no {}",
                    kind.as_str(),
                    dir.display(),
                    kind.marker_file()
                ));
            }
            kind
        }
        None => detect_kind(dir)?,
    };
    match kind {
        ArtifactKind::Skill => check_skill_dir(dir),
        ArtifactKind::Agent => check_agent_dir(dir),
        ArtifactKind::Playbook => check_playbook_dir(dir),
        ArtifactKind::Connector => check_connector_dir(dir),
    }
}

// ---- skill -------------------------------------------------------------------------------------

/// The skill's `manifest.json`: parsed, validated (manifest rules, tool and egress shapes,
/// `requiresConnector` shape) and with a well-formed skill URN. The same function runs at publish.
pub fn load_skill_manifest(root: &Path) -> Result<SkillManifest, PackageError> {
    let manifest_path = root.join("manifest.json");
    if !is_file(&manifest_path) {
        return err("manifest.json is required in the skill package");
    }
    let raw = read_package_file(&manifest_path)?;
    let manifest: SkillManifest =
        json_to_proto("vam.v1.SkillManifest", &raw).map_err(PackageError)?;
    manifest
        .validate()
        .map_err(|e| PackageError(e.to_string()))?;
    let urn = manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .filter(|urn| !urn.is_empty())
        .ok_or_else(|| PackageError("manifest.json resource_id.urn is required".into()))?;
    parse_skill_urn(urn).map_err(|e| PackageError(e.to_string()))?;
    Ok(manifest)
}

/// `SKILLS.md`: frontmatter present, body non-empty, id and version equal to the manifest's, and
/// any restated tools equal to the manifest's.
pub fn check_skills_md(root: &Path, manifest: &SkillManifest) -> Result<(), PackageError> {
    let content = skill_doc(root)
        .ok_or_else(|| {
            PackageError("SKILLS.md (or SKILL.md) is required in the skill package".into())
        })
        .and_then(|path| {
            read_package_file(&path).map_err(|_| {
                PackageError("SKILLS.md (or SKILL.md) is required in the skill package".into())
            })
        })?;
    let (frontmatter, body) = split_frontmatter(&content).ok_or_else(|| {
        PackageError("SKILLS.md must begin with YAML frontmatter delimited by ---".into())
    })?;
    if body.trim().is_empty() {
        return err("SKILLS.md markdown body after frontmatter must be non-empty");
    }
    let frontmatter_id = frontmatter_field(frontmatter, "id")
        .ok_or_else(|| PackageError("SKILLS.md frontmatter id is required".into()))?;
    let frontmatter_version = frontmatter_field(frontmatter, "version")
        .ok_or_else(|| PackageError("SKILLS.md frontmatter version is required".into()))?;

    let manifest_urn = manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .unwrap_or_default();
    if frontmatter_id != manifest_urn {
        return err(format!(
            "SKILLS.md frontmatter id '{frontmatter_id}' must match manifest.json resource_id.urn '{manifest_urn}'"
        ));
    }
    let manifest_version = manifest
        .version
        .as_ref()
        .ok_or_else(|| PackageError("manifest.json version is required".into()))?;
    let expected = format!(
        "{}.{}.{}",
        manifest_version.major, manifest_version.minor, manifest_version.patch
    );
    if frontmatter_version != expected {
        return err(format!(
            "SKILLS.md frontmatter version '{frontmatter_version}' must match manifest.json version '{expected}'"
        ));
    }
    crate::skill_frontmatter::ensure_frontmatter_matches_manifest(frontmatter, manifest)
        .map_err(PackageError)
}

/// Every offline rule for a skill directory: manifest, `SKILLS.md`, cards against the tool
/// schemas. What vamd adds on top is node policy (the registry domain) and storage.
pub fn check_skill_dir(root: &Path) -> Result<CheckedPackage, PackageError> {
    let manifest = load_skill_manifest(root)?;
    check_skills_md(root, &manifest)?;
    crate::skill_cards::check_package_cards(root, &manifest).map_err(PackageError)?;
    let warnings = crate::validation::skill_manifest_warnings(&manifest);
    let urn = manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.clone())
        .unwrap_or_default();
    let version = manifest
        .version
        .as_ref()
        .map(|v| format!("{}.{}.{}", v.major, v.minor, v.patch))
        .unwrap_or_default();
    let summary = summarize_skill(&manifest);
    Ok(CheckedPackage {
        kind: ArtifactKind::Skill,
        urn,
        version,
        summary,
        warnings,
        spec: CheckedSpec::Skill(manifest),
    })
}

fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let trimmed = content.strip_prefix("---")?;
    let rest = trimmed
        .strip_prefix('\n')
        .or_else(|| trimmed.strip_prefix("\r\n"))?;
    let end = rest.find("\n---")?;
    let frontmatter = &rest[..end];
    let body = rest[end + 4..]
        .strip_prefix('\n')
        .unwrap_or(&rest[end + 4..]);
    Some((frontmatter, body))
}

fn frontmatter_field(frontmatter: &str, key: &str) -> Option<String> {
    let prefix = format!("{key}:");
    frontmatter.lines().find_map(|line| {
        let line = line.trim();
        line.strip_prefix(&prefix)
            .map(|value| parse_yaml_scalar(value.trim()))
    })
}

fn parse_yaml_scalar(value: &str) -> String {
    let value = value.trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value[1..value.len() - 1].to_string()
    } else {
        value.to_string()
    }
}

// ---- agent -------------------------------------------------------------------------------------

/// The permissions an agent without any gets at publish: tools by side effect, internet allowed.
/// A legacy draft (capability or HITL policy) keeps what it has.
pub fn apply_publish_permission_defaults(spec: &mut AgentSpec) {
    if spec.permissions.is_some() || spec.capability_policy.is_some() || spec.hitl_policy.is_some()
    {
        return;
    }
    spec.permissions = Some(AgentPermissions {
        tools: Some(ToolPermissions {
            default: ToolDefault::BySideEffect as i32,
            rules: Vec::new(),
        }),
        internet: Some(InternetPermissions {
            default: HostDefault::Allow as i32,
            rules: Vec::new(),
        }),
        ..Default::default()
    });
}

/// What an agent draft must pass to be stored: publish defaults, then the spec rules.
pub fn agent_ready_for_publish(spec: &AgentSpec) -> Result<AgentSpec, PackageError> {
    let mut spec = spec.clone();
    apply_publish_permission_defaults(&mut spec);
    spec.validate().map_err(|e| PackageError(e.to_string()))?;
    Ok(spec)
}

/// What a playbook draft must pass to be stored: migrated to spec v2, then the spec rules.
pub fn playbook_ready_for_publish(spec: &PlaybookSpec) -> Result<PlaybookSpec, PackageError> {
    let mut spec = spec.clone();
    crate::playbook::migrate_playbook_spec(&mut spec);
    spec.validate().map_err(|e| PackageError(e.to_string()))?;
    Ok(spec)
}

/// A release is the publish step: on top of the draft checks it requires `activation.when`.
pub fn playbook_ready_for_release(spec: &PlaybookSpec) -> Result<PlaybookSpec, PackageError> {
    let spec = playbook_ready_for_publish(spec)?;
    spec.validate_for_publish()
        .map_err(|e| PackageError(e.to_string()))?;
    Ok(spec)
}

fn urn_of(resource_id: &Option<crate::v1::ResourceId>) -> String {
    resource_id
        .as_ref()
        .map(|r| r.urn.clone())
        .unwrap_or_default()
}

fn check_urn_kind(urn: &str, kind: ArtifactKind) -> Result<(), PackageError> {
    if urn.is_empty() {
        return err(format!("the {} resource_id.urn is required", kind.as_str()));
    }
    if !urn.starts_with(&format!("urn:vam:{}:", kind.as_str())) {
        return err(format!(
            "the URN '{urn}' is not a {} URN (urn:vam:{}:<domain>/<namespace>/<name>)",
            kind.as_str(),
            kind.as_str()
        ));
    }
    Ok(())
}

/// An agent directory: `agent.json` holding `{ "spec": … }`, checked as a release would be.
pub fn check_agent_dir(root: &Path) -> Result<CheckedPackage, PackageError> {
    let raw = read_package_file(&root.join(AGENT_FILE))?;
    let req: PutAgentRequest = json_to_proto("vam.v1.PutAgentRequest", &raw)
        .map_err(|e| PackageError(format!("{AGENT_FILE}: {e}")))?;
    let spec = req
        .spec
        .ok_or_else(|| PackageError(format!("{AGENT_FILE}: spec is required")))?;
    let spec = agent_ready_for_publish(&spec)?;
    let urn = urn_of(&spec.resource_id);
    check_urn_kind(&urn, ArtifactKind::Agent)?;
    let summary = summarize_agent(&spec);
    Ok(CheckedPackage {
        kind: ArtifactKind::Agent,
        urn,
        version: String::new(),
        summary,
        warnings: Vec::new(),
        spec: CheckedSpec::Agent(spec),
    })
}

/// A playbook directory: `playbook.json` holding `{ "spec": … }`, checked as a release would be.
pub fn check_playbook_dir(root: &Path) -> Result<CheckedPackage, PackageError> {
    let raw = read_package_file(&root.join(PLAYBOOK_FILE))?;
    let req: PutPlaybookRequest = json_to_proto("vam.v1.PutPlaybookRequest", &raw)
        .map_err(|e| PackageError(format!("{PLAYBOOK_FILE}: {e}")))?;
    let spec = req
        .spec
        .ok_or_else(|| PackageError(format!("{PLAYBOOK_FILE}: spec is required")))?;
    let spec = playbook_ready_for_release(&spec)?;
    let urn = urn_of(&spec.resource_id);
    check_urn_kind(&urn, ArtifactKind::Playbook)?;
    let warnings = crate::validation::playbook_enforcement_warnings(&spec);
    let summary = summarize_playbook(&spec);
    Ok(CheckedPackage {
        kind: ArtifactKind::Playbook,
        urn,
        version: String::new(),
        summary,
        warnings,
        spec: CheckedSpec::Playbook(spec),
    })
}

/// A connector directory: `connector.json` in the format of `spec/connectors/*.json`, held to the
/// limits of a connector from a marketplace ([`crate::connector_listing`]).
pub fn check_connector_dir(root: &Path) -> Result<CheckedPackage, PackageError> {
    let raw = read_package_file(&root.join(CONNECTOR_FILE))?;
    let c = crate::connector_listing::parse_marketplace_connector(&raw)
        .map_err(|e| PackageError(format!("{CONNECTOR_FILE}: {e}")))?;
    check_urn_kind(&c.urn, ArtifactKind::Connector)?;
    let summary = summarize_connector(&c);
    Ok(CheckedPackage {
        kind: ArtifactKind::Connector,
        urn: c.urn.clone(),
        version: c.version.clone(),
        summary,
        warnings: Vec::new(),
        spec: CheckedSpec::Connector(Box::new(c)),
    })
}

// ---- capability summary ------------------------------------------------------------------------

/// What a package can do, in words an admin can read before installing it. Serialized as the
/// `summary` of a marketplace listing (CN-4.4, CN-4.5) and printed by `vam catalog validate`.
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySummary {
    pub kind: String,
    pub urn: String,
    pub display_name: String,
    pub description: String,
    /// Skill: the connector whose connection its egress uses. Empty when none.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub requires_connector: String,
    /// Skill: every tool with its side effect, approval and egress.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolSummary>,
    /// Agent: the skills it uses. Playbook: the skills it requires.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    /// Agent: the playbooks it may start.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub playbooks: Vec<String>,
    /// Agent: may start any playbook.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_any_playbook: bool,
    /// Agent: the default for tools and each rule.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_default: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleSummary>,
    /// Playbook: the number of steps and the tools its steps are bound to.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub has_steps: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub bound_tools: Vec<String>,
    /// Connector: who publishes it, how members sign in, which server it reaches and with which
    /// vetted tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub connector: Option<ConnectorSummary>,
}

/// What an admin approves when they enable a connector from a marketplace. Everything the node
/// acts on is here: the publisher the file claims, the sign-in profile and client id, the scopes,
/// the one server (`mcpUrl`, its `hosts`) and the vetted tools with their risk class. An update
/// whose summary differs from the installed one needs Enable again.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectorSummary {
    pub publisher: String,
    /// `oauth2` (the only kind a marketplace connector can be today).
    pub provider: String,
    pub profile: String,
    /// The public client id the publisher declares, when it declares one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub client_secret: String,
    pub org_consent: String,
    pub scopes: Vec<String>,
    pub mcp_url: String,
    pub mcp_host: String,
    /// The hosts the connector reaches with a member's token: the MCP server's.
    pub hosts: Vec<String>,
    pub tools: Vec<McpToolSummary>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct McpToolSummary {
    pub remote: String,
    /// `read_only | mutable_local | mutable_external`.
    pub side_effect: String,
    /// `allow | ask | block`, when the file sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolSummary {
    pub name: String,
    pub description: String,
    /// `read_only | mutable_local | mutable_external | unspecified`.
    pub side_effect: String,
    pub approval_required: bool,
    /// The tool shows a card for its result (`show`) or its approval (`ask`).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub show_card: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub ask_card: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<EgressSummary>,
}

/// One host a tool may reach through the member's connection.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EgressSummary {
    pub binding: String,
    pub host: String,
    pub methods: Vec<String>,
    pub paths: Vec<PathReach>,
    /// A method other than GET or HEAD is declared.
    pub can_write: bool,
    /// A method other than GET or HEAD is declared together with a path prefix, which also
    /// admits every path beneath it (a write should be declared by exact path).
    pub writes_by_prefix: bool,
    /// One line for the admin, e.g. `GET on graph.microsoft.com: /v1.0/me/messages`.
    pub text: String,
}

/// One path the tool may reach. `exact` is a declared exact path (it admits that path only); a
/// prefix covers everything beneath it, and the prefix `/` covers the whole host (`any_path`).
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PathReach {
    pub path: String,
    pub exact: bool,
    pub any_path: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuleSummary {
    pub target: String,
    pub decision: String,
}

/// A path scope in words: `/` is "any path on <host>".
pub fn describe_reach(host: &str, reach: &PathReach) -> String {
    if reach.any_path {
        format!("any path on {host}")
    } else if reach.exact {
        format!("exactly {}", reach.path)
    } else {
        format!("{} and beneath", reach.path)
    }
}

fn side_effect_word(value: i32) -> &'static str {
    match SideEffectClass::try_from(value) {
        Ok(SideEffectClass::ReadOnly) => "read_only",
        Ok(SideEffectClass::MutableLocal) => "mutable_local",
        Ok(SideEffectClass::MutableExternal) => "mutable_external",
        _ => "unspecified",
    }
}

fn egress_summary(e: &crate::v1::ToolEgress) -> EgressSummary {
    let paths: Vec<PathReach> = e
        .path_prefixes
        .iter()
        .map(|p| PathReach {
            path: p.clone(),
            exact: false,
            any_path: p == "/",
        })
        .chain(e.paths.iter().map(|p| PathReach {
            path: p.clone(),
            exact: true,
            any_path: false,
        }))
        .collect();
    let can_write = e.methods.iter().any(|m| m != "GET" && m != "HEAD");
    let scope: Vec<String> = paths.iter().map(|p| describe_reach(&e.host, p)).collect();
    let text = format!(
        "{} on {}: {}",
        e.methods.join("/"),
        e.host,
        scope.join(", ")
    );
    EgressSummary {
        binding: e.binding.clone(),
        host: e.host.clone(),
        methods: e.methods.clone(),
        paths,
        can_write,
        writes_by_prefix: crate::skill_egress::writes_by_prefix(e),
        text,
    }
}

fn summarize_skill(m: &SkillManifest) -> CapabilitySummary {
    let urn = urn_of(&m.resource_id);
    let tools = m
        .tools
        .iter()
        .map(|t| ToolSummary {
            name: t.name.clone(),
            description: t.description.clone(),
            side_effect: side_effect_word(t.side_effect).to_string(),
            approval_required: t.approval_required,
            show_card: t.ui.as_ref().map(|u| u.show.clone()).unwrap_or_default(),
            ask_card: t.ui.as_ref().map(|u| u.ask.clone()).unwrap_or_default(),
            egress: t.egress.iter().map(egress_summary).collect(),
        })
        .collect();
    CapabilitySummary {
        kind: ArtifactKind::Skill.as_str().into(),
        display_name: urn.rsplit('/').next().unwrap_or_default().to_string(),
        description: m.description.clone(),
        urn,
        requires_connector: m.requires_connector.clone(),
        tools,
        ..Default::default()
    }
}

/// The capability summary of a connector definition.
pub fn summarize_connector(c: &Connector) -> CapabilitySummary {
    use crate::connector::{OAuthProfile, SideEffect, ToolDefault as McpDefault};
    let (provider, profile, client_id, client_secret, org_consent, scopes) = match &c.provider {
        crate::connector::Provider::Oauth2(p) => (
            "oauth2",
            match p.profile {
                OAuthProfile::Entra => "entra",
                OAuthProfile::Discover => "discover",
                OAuthProfile::Google => "google",
                OAuthProfile::Slack => "slack",
            },
            p.client_id.clone(),
            match p.client_secret {
                crate::connector::ClientSecret::None => "none",
                crate::connector::ClientSecret::SqurellRelay => "squrell_relay",
            },
            match p.org_consent {
                crate::connector::OrgConsent::None => "none",
                crate::connector::OrgConsent::AdminRequired => "admin_required",
            },
            p.scopes.clone(),
        ),
        crate::connector::Provider::ApiKey(_) => ("api_key", "", None, "none", "none", Vec::new()),
    };
    let (mcp_url, mcp_host, tools) = match &c.mcp {
        Some(m) => (
            m.url.clone(),
            crate::connector_listing::check_mcp_server_url(&m.url).unwrap_or_default(),
            m.tools
                .iter()
                .map(|t| McpToolSummary {
                    remote: t.remote.clone(),
                    side_effect: match t.side_effect {
                        SideEffect::ReadOnly => "read_only",
                        SideEffect::MutableLocal => "mutable_local",
                        SideEffect::MutableExternal => "mutable_external",
                    }
                    .to_string(),
                    default: t.default.map(|d| {
                        match d {
                            McpDefault::Allow => "allow",
                            McpDefault::Ask => "ask",
                            McpDefault::Block => "block",
                        }
                        .to_string()
                    }),
                })
                .collect(),
        ),
        None => (String::new(), String::new(), Vec::new()),
    };
    let hosts = if mcp_host.is_empty() {
        Vec::new()
    } else {
        vec![mcp_host.clone()]
    };
    CapabilitySummary {
        kind: ArtifactKind::Connector.as_str().into(),
        urn: c.urn.clone(),
        display_name: c.display_name.clone(),
        description: c.description.clone(),
        connector: Some(ConnectorSummary {
            publisher: c.publisher.clone(),
            provider: provider.into(),
            profile: profile.into(),
            client_id,
            client_secret: client_secret.into(),
            org_consent: org_consent.into(),
            scopes,
            mcp_url,
            mcp_host,
            hosts,
            tools,
        }),
        ..Default::default()
    }
}

impl CapabilitySummary {
    /// True when any tool of the package can write through a connection.
    pub fn can_write(&self) -> bool {
        self.tools
            .iter()
            .any(|t| t.egress.iter().any(|e| e.can_write))
    }

    /// True when some tool declares a write method by path prefix (it should use exact paths).
    pub fn writes_by_prefix(&self) -> bool {
        self.tools
            .iter()
            .any(|t| t.egress.iter().any(|e| e.writes_by_prefix))
    }

    /// Every egress host the package can reach, sorted, without repeats.
    pub fn egress_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = self
            .tools
            .iter()
            .flat_map(|t| t.egress.iter().map(|e| e.host.clone()))
            .collect();
        hosts.sort();
        hosts.dedup();
        hosts
    }

    /// True when some tool reaches any path on a host (the `/` prefix).
    pub fn reaches_any_path(&self) -> bool {
        self.tools
            .iter()
            .any(|t| t.egress.iter().any(|e| e.paths.iter().any(|p| p.any_path)))
    }
}

fn decision_word(value: i32) -> &'static str {
    match PermissionDecision::try_from(value) {
        Ok(PermissionDecision::Allow) => "allow",
        Ok(PermissionDecision::Ask) => "ask",
        Ok(PermissionDecision::Deny) => "deny",
        _ => "unspecified",
    }
}

fn tool_default_word(value: i32) -> &'static str {
    match ToolDefault::try_from(value) {
        Ok(ToolDefault::Allow) => "allow",
        Ok(ToolDefault::Ask) => "ask",
        Ok(ToolDefault::Deny) => "deny",
        Ok(ToolDefault::BySideEffect) => "bySideEffect",
        _ => "unspecified",
    }
}

fn summarize_agent(spec: &AgentSpec) -> CapabilitySummary {
    let tools = spec.permissions.as_ref().and_then(|p| p.tools.as_ref());
    let mut playbooks: Vec<String> = spec
        .allowed_playbook_refs
        .iter()
        .chain(spec.default_playbook_ref.iter())
        .map(|r| urn_of(&r.resource_id))
        .filter(|u| !u.is_empty())
        .collect();
    playbooks.sort();
    playbooks.dedup();
    CapabilitySummary {
        kind: ArtifactKind::Agent.as_str().into(),
        urn: urn_of(&spec.resource_id),
        display_name: spec.display_name.clone(),
        description: spec.description.clone(),
        skills: spec
            .skill_refs
            .iter()
            .map(|r| urn_of(&r.resource_id))
            .collect(),
        playbooks,
        allow_any_playbook: spec.allow_any_playbook,
        tool_default: tools.map(|t| tool_default_word(t.default).to_string()),
        rules: tools
            .map(|t| {
                t.rules
                    .iter()
                    .map(|r| RuleSummary {
                        target: r.target.clone(),
                        decision: decision_word(r.decision).to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default(),
        ..Default::default()
    }
}

fn summarize_playbook(spec: &PlaybookSpec) -> CapabilitySummary {
    let mut bound: Vec<String> = spec
        .steps
        .iter()
        .filter_map(|s| s.binding.as_ref())
        .map(|b| b.tool_name.clone())
        .filter(|n| !n.is_empty())
        .collect();
    bound.sort();
    bound.dedup();
    CapabilitySummary {
        kind: ArtifactKind::Playbook.as_str().into(),
        urn: urn_of(&spec.resource_id),
        display_name: spec.display_name.clone(),
        description: spec.description.clone(),
        skills: crate::playbook::required_skill_refs(spec)
            .iter()
            .map(|r| urn_of(&r.resource_id))
            .collect(),
        has_steps: !spec.steps.is_empty(),
        bound_tools: bound,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests;
