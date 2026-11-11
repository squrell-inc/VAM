//! Permission targets and [`AgentPermissions`] validation.
//!
//! Design: `design/agent-experience.md` §IV.2 and §IV.11. Locked decisions
//! 11, 13, 15, 22, and 24.
//!
//! # Canonical MCP server id
//!
//! [`mcp_server_id`] turns an `McpConnection.url` into the server id stored
//! in `mcp:` targets. It also accepts that URL with one leading `mcp:`
//! prefix, so an author-written server and the host computation match.
//!
//! * Scheme and host are lowercased. A missing scheme means `https`. Only
//!   `http` and `https` are accepted.
//! * The default port is dropped (`443` for https, `80` for http). Any
//!   other port is kept.
//! * A trailing slash is dropped. The path is case-sensitive. Empty
//!   segments, `.`, and `..` are rejected.
//! * Query (`?`), fragment (`#`), and userinfo (`@`) are rejected. They
//!   are not part of the server id.
//! * `https` is omitted from the id, so the form authors write stays
//!   `mcp.atlassian.com/v1`. `http` is written out. A non-default port is
//!   written out: `https://host:8443/v1` is `host:8443/v1`.
//! * Two servers on one host differ by path.
//! * IPv6 hosts are bracketed and compressed (`[::1]`).
//!
//! `https://MCP.Atlassian.com:443/v1/` and `mcp.atlassian.com/v1` are both
//! `mcp.atlassian.com/v1`.
//!
//! A tool target is `mcp:<server id>/<tool>` or `mcp:<server id>/*`. The
//! last path segment is the tool, not part of the server id.
//! `mcp:mcp.atlassian.com/v1/create_issue` is tool `create_issue` on server
//! `mcp.atlassian.com/v1`. Pass [`mcp_server_id`] the connection URL only;
//! it does not strip a tool name.
//!
//! # Other targets
//!
//! * `skill:<skill name>/<tool>` and `skill:<skill name>/*`. The skill
//!   name is a lowercase slug (`jira`, `jira-integration`).
//! * `builtin:<tool>` and `tools:*`. There is no `builtin:*`.
//! * A tool token is 1–128 characters: ASCII letters, digits, `_`, `-`,
//!   and `.`, starting and ending with a letter or digit. Case is kept,
//!   so `createIssue` and `create_issue` are different tools.
//! * Internet rules take an exact DNS name (`api.github.com`), one leading
//!   wildcard label (`*.pastebin.com`, not `foo.*.com` or `*.*.com`), an
//!   exact IPv4 address, or `internet:*`.
//!
//! # Host matching
//!
//! [`match_host`] is the same match as `vam_network::policy::match_domain`
//! (`crates/vam-network/src/policy.rs`). `vam-network` depends on this
//! crate, so the body is copied here. `*.example.com` matches
//! `api.example.com` and `sub.api.example.com`, and does not match the
//! apex `example.com`.
//!
//! # Specificity
//!
//! [`Target::specificity`] is the within-scope order the resolver in
//! AX-5.2b will use. Exact tool > source wildcard (`skill:<name>/*` or
//! `mcp:<server>/*`) > `tools:*`. Exact host > wildcard host, and a longer
//! wildcard suffix is more specific, > `internet:*`. Tool ranks and host
//! ranks do not compare with each other.
//!
//! # Defaults and strictness
//!
//! Unspecified defaults are valid on a stored spec.
//! [`AgentPermissions::effective_defaults`] applies decision 13: tools
//! `bySideEffect`, internet `allow`. [`Strictness`] orders decisions
//! `deny > ask > allow`, and tool defaults
//! `deny > ask > bySideEffect > allow`. Host rules reject `ask`
//! (decision 24).

use std::collections::HashMap;
use std::fmt;
use std::str::FromStr;

use thiserror::Error;

use crate::v1::{AgentPermissions, HostDefault, PermissionDecision, PermissionRule, ToolDefault};
use crate::validation::ValidationError;

/// How specific a [`Target`] is inside one scope.
///
/// Higher compares as more specific. Comparing a tool rank with a host
/// rank returns [`None`](Option).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Specificity {
    /// `tools:*`.
    ToolsAll,
    /// `skill:<name>/*` or `mcp:<server>/*`.
    SourceWildcard,
    /// One tool: `skill:<name>/<tool>`, `mcp:<server>/<tool>`, or `builtin:<tool>`.
    ExactTool,
    /// `internet:*`.
    InternetAll,
    /// `*.suffix`. `suffix_len` is the byte length of the suffix after `*.`.
    WildcardHost { suffix_len: usize },
    /// An exact host such as `api.github.com`.
    ExactHost,
}

impl Specificity {
    fn tool_rank(self) -> Option<u8> {
        match self {
            Specificity::ToolsAll => Some(0),
            Specificity::SourceWildcard => Some(1),
            Specificity::ExactTool => Some(2),
            Specificity::InternetAll
            | Specificity::WildcardHost { .. }
            | Specificity::ExactHost => None,
        }
    }

    fn host_rank(self) -> Option<(u8, usize)> {
        match self {
            Specificity::InternetAll => Some((0, 0)),
            Specificity::WildcardHost { suffix_len } => Some((1, suffix_len)),
            Specificity::ExactHost => Some((2, 0)),
            Specificity::ToolsAll | Specificity::SourceWildcard | Specificity::ExactTool => None,
        }
    }

    /// This rank orders tool targets.
    pub fn is_tool(self) -> bool {
        self.tool_rank().is_some()
    }

    /// This rank orders host targets.
    pub fn is_host(self) -> bool {
        self.host_rank().is_some()
    }
}

impl PartialOrd for Specificity {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        match (self.tool_rank(), other.tool_rank()) {
            (Some(left), Some(right)) => Some(left.cmp(&right)),
            (Some(_), None) | (None, Some(_)) => None,
            (None, None) => match (self.host_rank(), other.host_rank()) {
                (Some(left), Some(right)) => Some(left.cmp(&right)),
                _ => None,
            },
        }
    }
}

/// The tool half of a `skill:` or `mcp:` target.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ToolSelector {
    /// One tool name, in the spelling the author wrote.
    Exact(String),
    /// The source wildcard `/*`.
    All,
}

impl ToolSelector {
    fn as_str(&self) -> &str {
        match self {
            ToolSelector::Exact(name) => name,
            ToolSelector::All => "*",
        }
    }
}

/// A host pattern on an internet rule. `internet:*` is [`Target::InternetAll`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum HostPattern {
    /// `api.github.com` or an IPv4 address, already lowercased.
    Exact(String),
    /// `*.pastebin.com`. `suffix` is `pastebin.com`, with no leading dot.
    Wildcard { suffix: String },
}

/// A permission target in canonical form.
///
/// [`Display`](fmt::Display) writes the canonical spelling.
/// [`Target::parse`] accepts that spelling and the normalisations described
/// on this module.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Target {
    /// `skill:<name>/<tool>` or `skill:<name>/*`.
    Skill { name: String, tool: ToolSelector },
    /// `mcp:<server id>/<tool>` or `mcp:<server id>/*`.
    /// `server` is the canonical id from [`mcp_server_id`].
    Mcp { server: String, tool: ToolSelector },
    /// `builtin:<tool>`.
    Builtin { tool: String },
    /// `tools:*`.
    ToolsAll,
    /// An exact host or a single leading wildcard label.
    Host(HostPattern),
    /// `internet:*`.
    InternetAll,
}

impl Target {
    /// Parse a target. MCP server spelling is normalised; the [`Display`](fmt::Display)
    /// of the result is the canonical form.
    pub fn parse(input: &str) -> Result<Self, TargetParseError> {
        let input = input.trim();
        if input.is_empty() {
            return Err(target_error(input, "is empty"));
        }
        if let Some(body) = input.strip_prefix("skill:") {
            return parse_skill(body, input);
        }
        if let Some(body) = input.strip_prefix("mcp:") {
            return parse_mcp(body, input);
        }
        if let Some(body) = input.strip_prefix("builtin:") {
            return parse_builtin(body, input);
        }
        if let Some(body) = input.strip_prefix("tools:") {
            return parse_star_only(body, input, "tools:*", Target::ToolsAll);
        }
        if let Some(body) = input.strip_prefix("internet:") {
            return parse_star_only(body, input, "internet:*", Target::InternetAll);
        }
        parse_host_target(input).map_err(|reason| target_error(input, &reason))
    }

    /// Within-scope specificity. See the module docs.
    pub fn specificity(&self) -> Specificity {
        match self {
            Target::ToolsAll => Specificity::ToolsAll,
            Target::Skill {
                tool: ToolSelector::All,
                ..
            }
            | Target::Mcp {
                tool: ToolSelector::All,
                ..
            } => Specificity::SourceWildcard,
            Target::Skill { .. } | Target::Mcp { .. } | Target::Builtin { .. } => {
                Specificity::ExactTool
            }
            Target::InternetAll => Specificity::InternetAll,
            Target::Host(HostPattern::Wildcard { suffix }) => Specificity::WildcardHost {
                suffix_len: suffix.len(),
            },
            Target::Host(HostPattern::Exact(_)) => Specificity::ExactHost,
        }
    }

    /// Whether this host target matches `host`.
    ///
    /// Exact and wildcard hosts use [`match_host`]. `internet:*` matches
    /// every non-empty host. Tool targets do not match a host. This does
    /// not apply the platform IP floor; that check stays outside Cedar.
    pub fn matches_host(&self, host: &str) -> bool {
        match self {
            Target::Host(HostPattern::Exact(pattern)) => match_host(pattern, host),
            Target::Host(HostPattern::Wildcard { suffix }) => {
                match_host(&format!("*.{suffix}"), host)
            }
            Target::InternetAll => !host.is_empty(),
            Target::Skill { .. }
            | Target::Mcp { .. }
            | Target::Builtin { .. }
            | Target::ToolsAll => false,
        }
    }

    /// This target belongs on a tool rule.
    pub fn is_tool(&self) -> bool {
        self.specificity().is_tool()
    }

    /// This target belongs on an internet rule.
    pub fn is_host(&self) -> bool {
        self.specificity().is_host()
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Skill { name, tool } => write!(f, "skill:{name}/{}", tool.as_str()),
            Target::Mcp { server, tool } => write!(f, "mcp:{server}/{}", tool.as_str()),
            Target::Builtin { tool } => write!(f, "builtin:{tool}"),
            Target::ToolsAll => write!(f, "tools:*"),
            Target::Host(HostPattern::Exact(host)) => write!(f, "{host}"),
            Target::Host(HostPattern::Wildcard { suffix }) => write!(f, "*.{suffix}"),
            Target::InternetAll => write!(f, "internet:*"),
        }
    }
}

impl FromStr for Target {
    type Err = TargetParseError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        Target::parse(input)
    }
}

/// [`Target::parse`] failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct TargetParseError {
    /// The full sentence, including the target text.
    pub message: String,
}

/// [`mcp_server_id`] failed.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("{message}")]
pub struct McpServerIdError {
    /// The full sentence, including the URL text.
    pub message: String,
}

fn target_error(target: &str, reason: &str) -> TargetParseError {
    TargetParseError {
        message: format!("permission target '{target}' {reason}"),
    }
}

/// Canonical MCP server id for `url`.
///
/// `url` is an `McpConnection.url` (`https://MCP.Atlassian.com:443/v1/`)
/// or the same string with one leading `mcp:` prefix
/// (`mcp:mcp.atlassian.com/v1`). See the module docs for the canonical form.
pub fn mcp_server_id(url: &str) -> Result<String, McpServerIdError> {
    let trimmed = url.trim();
    let body = strip_one_mcp_prefix(trimmed);
    parse_server(body)
        .map(|parts| format_server_id(&parts))
        .map_err(|reason| McpServerIdError {
            message: format!("MCP server URL '{trimmed}' {reason}"),
        })
}

/// Same rules as `vam_network::policy::match_domain`.
///
/// Copied because `vam-network` depends on `vam-spec`. `*.example.com`
/// matches a subdomain and does not match the apex.
pub fn match_host(pattern: &str, target: &str) -> bool {
    let pattern = pattern.to_lowercase();
    let target = target.to_lowercase();
    if pattern == target {
        return true;
    }
    // `strip_prefix("*.")` is the same test as `starts_with("*.")` in
    // `vam_network::policy::match_domain`.
    if let Some(suffix) = pattern.strip_prefix("*.") {
        if target.ends_with(suffix) && target.len() > suffix.len() {
            let prefix_len = target.len() - suffix.len();
            if target.as_bytes()[prefix_len - 1] == b'.' {
                return true;
            }
        }
    }
    false
}

/// Defaults a stored spec actually enforces when a field was left unset.
///
/// Decision 13: tools `bySideEffect`, internet `allow`. An explicit value
/// is returned unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EffectiveDefaults {
    /// Never [`ToolDefault::Unspecified`].
    pub tools: ToolDefault,
    /// Never [`HostDefault::Unspecified`].
    pub internet: HostDefault,
}

impl EffectiveDefaults {
    /// Strictness of the tool default. Unspecified is not produced.
    pub fn tools_strictness(self) -> Strictness {
        match self.tools {
            ToolDefault::Deny => Strictness::DENY,
            ToolDefault::Ask => Strictness::ASK,
            ToolDefault::BySideEffect | ToolDefault::Unspecified => Strictness::BY_SIDE_EFFECT,
            ToolDefault::Allow => Strictness::ALLOW,
        }
    }

    /// Strictness of the internet default. Unspecified is not produced.
    pub fn internet_strictness(self) -> Strictness {
        match self.internet {
            HostDefault::Deny => Strictness::DENY,
            HostDefault::Allow | HostDefault::Unspecified => Strictness::ALLOW,
        }
    }
}

impl AgentPermissions {
    /// Fill unspecified tool and internet defaults (decision 13).
    pub fn effective_defaults(&self) -> EffectiveDefaults {
        let tools = self
            .tools
            .as_ref()
            .map(|tools| tools.default)
            .unwrap_or(ToolDefault::Unspecified as i32);
        let internet = self
            .internet
            .as_ref()
            .map(|internet| internet.default)
            .unwrap_or(HostDefault::Unspecified as i32);
        EffectiveDefaults {
            tools: match ToolDefault::try_from(tools) {
                Ok(ToolDefault::Allow) => ToolDefault::Allow,
                Ok(ToolDefault::Ask) => ToolDefault::Ask,
                Ok(ToolDefault::Deny) => ToolDefault::Deny,
                Ok(ToolDefault::BySideEffect) | Ok(ToolDefault::Unspecified) | Err(_) => {
                    ToolDefault::BySideEffect
                }
            },
            internet: match HostDefault::try_from(internet) {
                Ok(HostDefault::Deny) => HostDefault::Deny,
                Ok(HostDefault::Allow) | Ok(HostDefault::Unspecified) | Err(_) => {
                    HostDefault::Allow
                }
            },
        }
    }
}

/// `deny > ask > bySideEffect > allow`. Higher is stricter.
///
/// Decisions use `deny`, `ask`, and `allow`. Tool defaults also use
/// `bySideEffect`, which sits between `ask` and `allow`. Release diff
/// (AX-5.7a) and the resolver compare these ranks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Strictness(u8);

impl Strictness {
    /// Least strict.
    pub const ALLOW: Self = Self(1);
    /// Tool default only. Stricter than `allow`, looser than `ask`.
    pub const BY_SIDE_EFFECT: Self = Self(2);
    /// Stricter than `bySideEffect` and `allow`.
    pub const ASK: Self = Self(3);
    /// Strictest.
    pub const DENY: Self = Self(4);
}

/// `deny > ask > allow`. Unspecified has no rank.
pub fn decision_strictness(decision: PermissionDecision) -> Option<Strictness> {
    match decision {
        PermissionDecision::Deny => Some(Strictness::DENY),
        PermissionDecision::Ask => Some(Strictness::ASK),
        PermissionDecision::Allow => Some(Strictness::ALLOW),
        PermissionDecision::Unspecified => None,
    }
}

/// `deny > ask > bySideEffect > allow`. Unspecified has no rank.
pub fn tool_default_strictness(default: ToolDefault) -> Option<Strictness> {
    match default {
        ToolDefault::Deny => Some(Strictness::DENY),
        ToolDefault::Ask => Some(Strictness::ASK),
        ToolDefault::BySideEffect => Some(Strictness::BY_SIDE_EFFECT),
        ToolDefault::Allow => Some(Strictness::ALLOW),
        ToolDefault::Unspecified => None,
    }
}

/// `deny > allow`. Host defaults have no `ask` (decision 24).
pub fn host_default_strictness(default: HostDefault) -> Option<Strictness> {
    match default {
        HostDefault::Deny => Some(Strictness::DENY),
        HostDefault::Allow => Some(Strictness::ALLOW),
        HostDefault::Unspecified => None,
    }
}

/// Check tool targets, host targets, decisions, and duplicate targets.
///
/// Field paths are rooted at `permissions`. `advanced` policy refs are
/// checked by agent validation, with the other policy refs.
pub fn validate_agent_permissions(permissions: &AgentPermissions) -> Result<(), ValidationError> {
    let mut errors = ValidationError::empty();
    if let Some(tools) = &permissions.tools {
        if ToolDefault::try_from(tools.default).is_err() {
            errors.push(
                "permissions.tools.default",
                "tool default must be allow, ask, deny, or bySideEffect",
                "tool_default",
            );
        }
        validate_rules(&tools.rules, RuleList::Tools, &mut errors);
    }
    if let Some(internet) = &permissions.internet {
        if HostDefault::try_from(internet.default).is_err() {
            errors.push(
                "permissions.internet.default",
                "internet default must be allow or deny",
                "host_default",
            );
        }
        validate_rules(&internet.rules, RuleList::Internet, &mut errors);
    }
    if errors.field_violations.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

#[derive(Clone, Copy)]
enum RuleList {
    Tools,
    Internet,
}

impl RuleList {
    fn path(self) -> &'static str {
        match self {
            RuleList::Tools => "permissions.tools.rules",
            RuleList::Internet => "permissions.internet.rules",
        }
    }
}

fn validate_rules(rules: &[PermissionRule], list: RuleList, errors: &mut ValidationError) {
    let mut seen: HashMap<Target, usize> = HashMap::new();
    for (index, rule) in rules.iter().enumerate() {
        let target_path = format!("{}[{index}].target", list.path());
        let decision_path = format!("{}[{index}].decision", list.path());
        match Target::parse(&rule.target) {
            Ok(target) => {
                let canonical = target.to_string();
                let kind_ok = match list {
                    RuleList::Tools => target.is_tool(),
                    RuleList::Internet => target.is_host(),
                };
                if !kind_ok {
                    let expected = match list {
                        RuleList::Tools => {
                            "tool rules take a skill, mcp, or builtin target, or tools:*"
                        }
                        RuleList::Internet => "internet rules take a host pattern or internet:*",
                    };
                    errors.push(
                        &target_path,
                        format!("{expected}; got '{canonical}'"),
                        match list {
                            RuleList::Tools => "tool_target",
                            RuleList::Internet => "host_target",
                        },
                    );
                }
                if let Some(_first) = seen.insert(target, index) {
                    errors.push(
                        &target_path,
                        format!("duplicate target '{canonical}'"),
                        "unique",
                    );
                }
                validate_decision(rule.decision, list, &canonical, &decision_path, errors);
            }
            Err(err) => errors.push(&target_path, err.message, "target_grammar"),
        }
    }
}

fn validate_decision(
    raw: i32,
    list: RuleList,
    target: &str,
    path: &str,
    errors: &mut ValidationError,
) {
    match (list, PermissionDecision::try_from(raw)) {
        (_, Ok(PermissionDecision::Allow | PermissionDecision::Deny)) => {}
        (RuleList::Tools, Ok(PermissionDecision::Ask)) => {}
        (RuleList::Internet, Ok(PermissionDecision::Ask)) => errors.push(
            path,
            format!("host rules are allow or deny; decision 'ask' is not valid for '{target}'"),
            "host_decision",
        ),
        (RuleList::Tools, Ok(PermissionDecision::Unspecified) | Err(_)) => errors.push(
            path,
            "tool rule decision must be allow, ask, or deny",
            "decision",
        ),
        (RuleList::Internet, Ok(PermissionDecision::Unspecified) | Err(_)) => {
            errors.push(path, "host rule decision must be allow or deny", "decision")
        }
    }
}

fn parse_skill(body: &str, original: &str) -> Result<Target, TargetParseError> {
    let Some((name, tool)) = body.split_once('/') else {
        return Err(target_error(
            original,
            "must look like skill:<skill name>/<tool> or skill:<skill name>/*",
        ));
    };
    if name.contains('/') || tool.contains('/') {
        return Err(target_error(
            original,
            "must look like skill:<skill name>/<tool> or skill:<skill name>/*",
        ));
    }
    if !is_skill_name(name) {
        return Err(target_error(
            original,
            "has an invalid skill name (lowercase letters, digits, '.', '_', '-')",
        ));
    }
    Ok(Target::Skill {
        name: name.to_string(),
        tool: parse_selector(tool, original)?,
    })
}

fn parse_mcp(body: &str, original: &str) -> Result<Target, TargetParseError> {
    if body.is_empty() {
        return Err(target_error(
            original,
            "needs a server and a tool name or '*'",
        ));
    }
    // The trailing slash is not part of the server id. Drop it before the
    // tool is split off, so `mcp:host/v1/create_issue/` is that tool and
    // `*` is never fed to path canonicalisation.
    let body = body.trim_end_matches('/');
    let Some((server_body, tool)) = body.rsplit_once('/') else {
        return Err(target_error(
            original,
            "needs a tool name or '*' after the server",
        ));
    };
    if server_body.is_empty() || tool.is_empty() {
        return Err(target_error(
            original,
            "needs a tool name or '*' after the server",
        ));
    }
    let parts = parse_server(server_body).map_err(|reason| target_error(original, &reason))?;
    let tool = if tool == "*" {
        ToolSelector::All
    } else {
        parse_tool_token(tool).map_err(|reason| target_error(original, &reason))?
    };
    Ok(Target::Mcp {
        server: format_server_id(&parts),
        tool,
    })
}

fn parse_builtin(body: &str, original: &str) -> Result<Target, TargetParseError> {
    if body == "*" || body.contains('/') {
        return Err(target_error(
            original,
            "must look like builtin:<tool> (there is no builtin:*)",
        ));
    }
    let tool = parse_tool_token(body).map_err(|reason| target_error(original, &reason))?;
    let ToolSelector::Exact(tool) = tool else {
        return Err(target_error(original, "must look like builtin:<tool>"));
    };
    Ok(Target::Builtin { tool })
}

fn parse_star_only(
    body: &str,
    original: &str,
    spelling: &str,
    target: Target,
) -> Result<Target, TargetParseError> {
    if body == "*" {
        Ok(target)
    } else {
        Err(target_error(
            original,
            &format!("must be exactly {spelling}"),
        ))
    }
}

fn parse_selector(token: &str, original: &str) -> Result<ToolSelector, TargetParseError> {
    if token == "*" {
        Ok(ToolSelector::All)
    } else {
        parse_tool_token(token).map_err(|reason| target_error(original, &reason))
    }
}

fn parse_tool_token(token: &str) -> Result<ToolSelector, String> {
    let bytes = token.as_bytes();
    let shape_ok = !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes[bytes.len() - 1].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(*byte, b'_' | b'-' | b'.'));
    if !shape_ok {
        return Err(format!(
            "has an invalid tool name '{token}' (letters, digits, '_', '-', '.', 1-128 chars)"
        ));
    }
    // Wider than `validation::is_tool_name`: skill manifests stay lowercase
    // snake, while MCP vendors also ship camelCase (`createIssue`) and dotted
    // names (`echo.ping`). Case is significant.
    Ok(ToolSelector::Exact(token.to_string()))
}

fn parse_host_target(input: &str) -> Result<Target, String> {
    if input.contains('/') || input.contains(':') || input.contains('?') || input.contains('#') {
        return Err(
            "is not a tool target (skill:, mcp:, builtin:, tools:*) or a host pattern (a DNS name, *.suffix, or internet:*)"
                .to_string(),
        );
    }
    if let Some(suffix) = input.strip_prefix("*.") {
        if suffix.is_empty() || suffix.contains('*') {
            return Err("host wildcard must be a single leading '*' label".to_string());
        }
        let suffix = normalize_hostname(suffix)?;
        if is_ipv4_literal(&suffix) {
            return Err("host wildcard suffix must be a DNS name".to_string());
        }
        return Ok(Target::Host(HostPattern::Wildcard { suffix }));
    }
    if input.contains('*') {
        return Err("host wildcard must be a single leading '*' label".to_string());
    }
    let host = normalize_hostname(input)?;
    Ok(Target::Host(HostPattern::Exact(host)))
}

/// Skill names match `validation`'s slug rule: lowercase letters, digits,
/// `.`, `_`, and `-`, starting and ending with a letter or digit.
fn is_skill_name(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'_' | b'-')
        })
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphanumeric)
        && value
            .as_bytes()
            .last()
            .is_some_and(u8::is_ascii_alphanumeric)
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scheme {
    Http,
    Https,
}

struct ServerParts {
    scheme: Scheme,
    host: String,
    port: Option<u16>,
    path: Vec<String>,
}

fn strip_one_mcp_prefix(input: &str) -> &str {
    let Some(rest) = input.get(4..) else {
        return input;
    };
    if input.as_bytes()[..4].eq_ignore_ascii_case(b"mcp:") && !rest.starts_with("//") {
        rest
    } else {
        input
    }
}

fn parse_server(input: &str) -> Result<ServerParts, String> {
    if input.is_empty() {
        return Err("is empty".to_string());
    }
    if input
        .chars()
        .any(|ch| ch.is_whitespace() || ch.is_control())
    {
        return Err("must not contain whitespace".to_string());
    }
    if input.contains('?') {
        return Err("must not contain a query".to_string());
    }
    if input.contains('#') {
        return Err("must not contain a fragment".to_string());
    }
    if input.contains('@') {
        return Err("must not contain userinfo".to_string());
    }
    if input.contains('\\') {
        return Err("must not contain a backslash".to_string());
    }
    let (scheme, rest) = split_scheme(input)?;
    if rest.is_empty() || rest.starts_with('/') {
        return Err("is missing a host".to_string());
    }
    let (authority, path) = match rest.split_once('/') {
        Some((authority, path)) => (authority, path),
        None => (rest, ""),
    };
    if authority.is_empty() {
        return Err("is missing a host".to_string());
    }
    let (host, port) = split_authority(authority)?;
    let port = drop_default_port(scheme, port);
    let path = path.trim_end_matches('/');
    let mut segments = Vec::new();
    if !path.is_empty() {
        for segment in path.split('/') {
            segments.push(canonicalize_segment(segment)?);
        }
    }
    Ok(ServerParts {
        scheme,
        host,
        port,
        path: segments,
    })
}

fn split_scheme(input: &str) -> Result<(Scheme, &str), String> {
    let Some((scheme, rest)) = input.split_once("://") else {
        return Ok((Scheme::Https, input));
    };
    if scheme.eq_ignore_ascii_case("https") {
        Ok((Scheme::Https, rest))
    } else if scheme.eq_ignore_ascii_case("http") {
        Ok((Scheme::Http, rest))
    } else if scheme.is_empty() {
        Err("is missing a scheme".to_string())
    } else {
        Err(format!(
            "has scheme '{scheme}'; only http and https are accepted"
        ))
    }
}

fn split_authority(authority: &str) -> Result<(String, Option<u16>), String> {
    if let Some(rest) = authority.strip_prefix('[') {
        let Some(end) = rest.find(']') else {
            return Err("has an unclosed IPv6 address".to_string());
        };
        let inner = &rest[..end];
        let after = &rest[end + 1..];
        let port = if after.is_empty() {
            None
        } else if let Some(raw) = after.strip_prefix(':') {
            if raw.is_empty() {
                return Err("has an invalid port".to_string());
            }
            Some(parse_port(raw)?)
        } else {
            return Err("has trailing junk after an IPv6 address".to_string());
        };
        return Ok((normalize_ipv6(inner)?, port));
    }
    if let Some((host, raw_port)) = authority.rsplit_once(':') {
        if host.is_empty() || raw_port.is_empty() {
            return Err("has an invalid port".to_string());
        }
        return Ok((normalize_hostname(host)?, Some(parse_port(raw_port)?)));
    }
    Ok((normalize_hostname(authority)?, None))
}

fn parse_port(raw: &str) -> Result<u16, String> {
    if raw.is_empty()
        || raw.len() > 5
        || (raw.len() > 1 && raw.starts_with('0'))
        || !raw.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(format!("has an invalid port '{raw}'"));
    }
    let value: u32 = match raw.parse() {
        Ok(value) => value,
        Err(_) => return Err(format!("has an invalid port '{raw}'")),
    };
    if value == 0 || value > 65535 {
        return Err(format!("has an invalid port '{raw}'"));
    }
    Ok(value as u16)
}

fn drop_default_port(scheme: Scheme, port: Option<u16>) -> Option<u16> {
    match (scheme, port) {
        (Scheme::Https, Some(443)) | (Scheme::Http, Some(80)) => None,
        (_, other) => other,
    }
}

fn format_server_id(parts: &ServerParts) -> String {
    let mut out = String::new();
    if parts.scheme == Scheme::Http {
        out.push_str("http://");
    }
    out.push_str(&parts.host);
    if let Some(port) = parts.port {
        out.push(':');
        out.push_str(&port.to_string());
    }
    for segment in &parts.path {
        out.push('/');
        out.push_str(segment);
    }
    out
}

fn canonicalize_segment(segment: &str) -> Result<String, String> {
    if segment.is_empty() {
        return Err("has an empty path segment".to_string());
    }
    if segment == "." || segment == ".." {
        return Err(format!("has a '{segment}' path segment"));
    }
    let bytes = segment.as_bytes();
    let mut out = String::with_capacity(segment.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if is_path_byte(byte) {
            out.push(byte as char);
            index += 1;
            continue;
        }
        if byte == b'%'
            && index + 2 < bytes.len()
            && bytes[index + 1].is_ascii_hexdigit()
            && bytes[index + 2].is_ascii_hexdigit()
        {
            out.push('%');
            out.push((bytes[index + 1] as char).to_ascii_lowercase());
            out.push((bytes[index + 2] as char).to_ascii_lowercase());
            index += 3;
            continue;
        }
        return Err(format!("has an invalid path segment '{segment}'"));
    }
    Ok(out)
}

fn is_path_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'-' | b'.'
                | b'_'
                | b'~'
                | b'!'
                | b'$'
                | b'&'
                | b'\''
                | b'('
                | b')'
                | b'+'
                | b','
                | b';'
                | b'='
        )
}

fn normalize_hostname(host: &str) -> Result<String, String> {
    if host.is_empty() || host.len() > 253 || host.starts_with('.') || host.ends_with('.') {
        return Err("is not a host name".to_string());
    }
    let lower = host.to_ascii_lowercase();
    if is_ipv4_shape(&lower) {
        return normalize_ipv4(&lower);
    }
    for label in lower.split('.') {
        if !is_dns_label(label) {
            return Err("is not a host name".to_string());
        }
    }
    Ok(lower)
}

fn is_ipv4_shape(host: &str) -> bool {
    host.split('.').count() == 4
        && host
            .bytes()
            .all(|byte| byte.is_ascii_digit() || byte == b'.')
}

fn is_ipv4_literal(host: &str) -> bool {
    is_ipv4_shape(host)
}

fn is_dns_label(label: &str) -> bool {
    let bytes = label.as_bytes();
    if bytes.is_empty() || bytes.len() > 63 {
        return false;
    }
    if !bytes[0].is_ascii_alphanumeric() || !bytes[bytes.len() - 1].is_ascii_alphanumeric() {
        return false;
    }
    bytes
        .iter()
        .all(|byte| byte.is_ascii_alphanumeric() || *byte == b'-')
}

fn normalize_ipv4(host: &str) -> Result<String, String> {
    let mut octets = [0u8; 4];
    let parts: Vec<&str> = host.split('.').collect();
    if parts.len() != 4 {
        return Err("is not a host name".to_string());
    }
    for (index, part) in parts.iter().enumerate() {
        if part.is_empty() || part.len() > 3 || (part.len() > 1 && part.starts_with('0')) {
            return Err("is not an IPv4 address".to_string());
        }
        if !part.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("is not an IPv4 address".to_string());
        }
        let value: u16 = match part.parse() {
            Ok(value) => value,
            Err(_) => return Err("is not an IPv4 address".to_string()),
        };
        if value > 255 {
            return Err("is not an IPv4 address".to_string());
        }
        octets[index] = value as u8;
    }
    Ok(format!(
        "{}.{}.{}.{}",
        octets[0], octets[1], octets[2], octets[3]
    ))
}

fn normalize_ipv6(inner: &str) -> Result<String, String> {
    let groups = expand_ipv6(inner)?;
    Ok(format!("[{}]", compress_ipv6(&groups)))
}

fn expand_ipv6(input: &str) -> Result<[u16; 8], String> {
    let lower = input.to_ascii_lowercase();
    if lower.is_empty() {
        return Err("has an empty IPv6 address".to_string());
    }
    let compressions = lower.matches("::").count();
    if compressions > 1 {
        return Err("has an invalid IPv6 address".to_string());
    }
    let has_compress = compressions == 1;
    let (left, right) = if has_compress {
        match lower.split_once("::") {
            Some(pair) => pair,
            None => return Err("has an invalid IPv6 address".to_string()),
        }
    } else {
        (lower.as_str(), "")
    };
    let left_groups = parse_ipv6_side(left)?;
    let right_groups = if has_compress {
        parse_ipv6_side(right)?
    } else {
        Vec::new()
    };
    let mut groups = [0u16; 8];
    if !has_compress {
        if left_groups.len() != 8 {
            return Err("has an invalid IPv6 address".to_string());
        }
        for (index, group) in left_groups.iter().enumerate() {
            groups[index] = *group;
        }
        return Ok(groups);
    }
    if left_groups.len() + right_groups.len() >= 8 {
        return Err("has an invalid IPv6 address".to_string());
    }
    for (index, group) in left_groups.iter().enumerate() {
        groups[index] = *group;
    }
    let right_start = 8 - right_groups.len();
    for (index, group) in right_groups.iter().enumerate() {
        groups[right_start + index] = *group;
    }
    Ok(groups)
}

fn parse_ipv6_side(side: &str) -> Result<Vec<u16>, String> {
    if side.is_empty() {
        return Ok(Vec::new());
    }
    let mut groups = Vec::new();
    for part in side.split(':') {
        if part.is_empty() || part.len() > 4 || !part.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("has an invalid IPv6 address".to_string());
        }
        let group = match u16::from_str_radix(part, 16) {
            Ok(group) => group,
            Err(_) => return Err("has an invalid IPv6 address".to_string()),
        };
        groups.push(group);
    }
    Ok(groups)
}

fn compress_ipv6(groups: &[u16; 8]) -> String {
    let mut best: Option<(usize, usize)> = None;
    let mut index = 0;
    while index < 8 {
        if groups[index] == 0 {
            let start = index;
            while index < 8 && groups[index] == 0 {
                index += 1;
            }
            let len = index - start;
            if len >= 2 && best.map(|(_, best_len)| len > best_len).unwrap_or(true) {
                best = Some((start, len));
            }
        } else {
            index += 1;
        }
    }
    let mut out = String::new();
    let mut index = 0;
    while index < 8 {
        if let Some((start, len)) = best {
            if index == start {
                if out.ends_with(':') {
                    out.push(':');
                } else {
                    out.push_str("::");
                }
                index += len;
                continue;
            }
        }
        if !out.is_empty() && !out.ends_with(':') {
            out.push(':');
        }
        out.push_str(&format!("{:x}", groups[index]));
        index += 1;
    }
    if out.is_empty() {
        "::".to_string()
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoding::{json_to_proto, proto_to_json};
    use crate::v1::{
        agent_spec::InstructionSource, AgentSpec, CapabilityPolicyRef, Digest, HitlPolicyRef,
        InternetPermissions, ResourceId, ResourceRef, ToolPermissions, Version,
    };
    use crate::validation::Validate;

    fn assert_canonical(raw: &str, canonical: &str) {
        let parsed = Target::parse(raw).unwrap_or_else(|err| panic!("{raw}: {err}"));
        assert_eq!(parsed.to_string(), canonical, "display of {raw}");
        let again =
            Target::parse(canonical).unwrap_or_else(|err| panic!("reparse {canonical}: {err}"));
        assert_eq!(again, parsed);
        assert_eq!(again.to_string(), canonical);
        assert_eq!(canonical.parse::<Target>().unwrap(), parsed);
    }

    fn reject(raw: &str, fragment: &str) {
        let err = Target::parse(raw).expect_err(raw);
        assert!(
            err.message.contains(fragment),
            "target '{raw}' error '{}' should mention '{fragment}'",
            err.message
        );
    }

    #[test]
    fn every_grammar_form_round_trips_canonically() {
        assert_canonical("skill:jira/create_issue", "skill:jira/create_issue");
        assert_canonical("skill:jira-integration/*", "skill:jira-integration/*");
        assert_canonical(
            "mcp:mcp.atlassian.com/create_issue",
            "mcp:mcp.atlassian.com/create_issue",
        );
        assert_canonical("mcp:mcp.atlassian.com/v1/*", "mcp:mcp.atlassian.com/v1/*");
        assert_canonical(
            "mcp:https://MCP.Atlassian.com:443/v1/create_issue",
            "mcp:mcp.atlassian.com/v1/create_issue",
        );
        assert_canonical(
            "mcp:http://MCP.Atlassian.com:80/v1/create_issue",
            "mcp:http://mcp.atlassian.com/v1/create_issue",
        );
        assert_canonical(
            "mcp:https://mcp.atlassian.com:8443/v1/create_issue",
            "mcp:mcp.atlassian.com:8443/v1/create_issue",
        );
        assert_canonical(
            "mcp:https://MCP.Atlassian.com/V1/createIssue",
            "mcp:mcp.atlassian.com/V1/createIssue",
        );
        assert_canonical("builtin:run", "builtin:run");
        assert_canonical("builtin:echo.ping", "builtin:echo.ping");
        assert_canonical("tools:*", "tools:*");
        assert_canonical("api.github.com", "api.github.com");
        assert_canonical("API.GitHub.com", "api.github.com");
        assert_canonical("*.pastebin.com", "*.pastebin.com");
        assert_canonical("*.Pastebin.COM", "*.pastebin.com");
        assert_canonical("*.api.github.com", "*.api.github.com");
        assert_canonical("internet:*", "internet:*");
        assert_canonical("  skill:jira/create_issue  ", "skill:jira/create_issue");
    }

    #[test]
    fn j5_mcp_server_ids_match_across_spellings() {
        let from_url = mcp_server_id("https://MCP.Atlassian.com:443/v1/").unwrap();
        let from_prefixed = mcp_server_id("mcp:https://MCP.Atlassian.com:443/v1/").unwrap();
        let from_author = mcp_server_id("mcp:mcp.atlassian.com/v1").unwrap();
        let from_bare = mcp_server_id("mcp.atlassian.com/v1").unwrap();
        assert_eq!(from_url, "mcp.atlassian.com/v1");
        assert_eq!(from_prefixed, from_url);
        assert_eq!(from_author, from_url);
        assert_eq!(from_bare, from_url);
        assert_ne!(
            mcp_server_id("https://mcp.atlassian.com/v1").unwrap(),
            mcp_server_id("https://mcp.atlassian.com/v2").unwrap()
        );
        assert_eq!(
            mcp_server_id("http://MCP.Atlassian.com:80/v1/").unwrap(),
            "http://mcp.atlassian.com/v1"
        );
        assert_eq!(
            mcp_server_id("https://mcp.atlassian.com:8443/v1").unwrap(),
            "mcp.atlassian.com:8443/v1"
        );
        assert_eq!(
            mcp_server_id("https://[0:0:0:0:0:0:0:1]:443/mcp").unwrap(),
            mcp_server_id("https://[::1]/mcp").unwrap()
        );
        assert_eq!(
            mcp_server_id("http://[::1]:80/mcp").unwrap(),
            "http://[::1]/mcp"
        );
    }

    #[test]
    fn mcp_server_id_rejects_query_fragment_and_userinfo() {
        for url in [
            "https://mcp.atlassian.com/v1?token=1",
            "https://mcp.atlassian.com/v1#frag",
            "https://user:pass@mcp.atlassian.com/v1",
            "ftp://mcp.atlassian.com/v1",
            "https://mcp.atlassian.com/v1/../secret",
            "",
        ] {
            assert!(mcp_server_id(url).is_err(), "{url} should fail");
        }
    }

    #[test]
    fn rejected_grammar_names_the_problem() {
        reject("skill:Jira/create_issue", "skill name");
        reject("skill:jira", "skill:");
        reject("skill:jira/", "tool name");
        reject("builtin:*", "builtin:*");
        reject("builtin:", "tool name");
        reject("builtin:has space", "tool name");
        reject("mcp:*", "tool");
        reject("tools:run", "tools:*");
        reject("tools:**", "tools:*");
        reject("internet:allow", "internet:*");
        reject("foo.*.com", "wildcard");
        reject("*.*.pastebin.com", "wildcard");
        reject("*pastebin.com", "wildcard");
        reject("*.", "wildcard");
        reject("", "empty");
        reject("https://api.github.com", "host pattern");
    }

    #[test]
    fn specificity_orders_tools_and_hosts_separately() {
        let exact = Target::parse("skill:jira/create_issue").unwrap();
        let source = Target::parse("skill:jira/*").unwrap();
        let mcp_exact = Target::parse("mcp:mcp.atlassian.com/v1/create_issue").unwrap();
        let mcp_source = Target::parse("mcp:mcp.atlassian.com/*").unwrap();
        let builtin = Target::parse("builtin:run").unwrap();
        let all = Target::parse("tools:*").unwrap();
        assert!(exact.specificity() > source.specificity());
        assert!(source.specificity() > all.specificity());
        assert!(exact.specificity() > all.specificity());
        assert_eq!(exact.specificity(), Specificity::ExactTool);
        assert_eq!(mcp_exact.specificity(), Specificity::ExactTool);
        assert_eq!(builtin.specificity(), Specificity::ExactTool);
        assert_eq!(source.specificity(), Specificity::SourceWildcard);
        assert_eq!(mcp_source.specificity(), Specificity::SourceWildcard);

        let host = Target::parse("api.github.com").unwrap();
        let long = Target::parse("*.api.github.com").unwrap();
        let short = Target::parse("*.github.com").unwrap();
        let shorter = Target::parse("*.com").unwrap();
        let every = Target::parse("internet:*").unwrap();
        assert!(host.specificity() > long.specificity());
        assert!(long.specificity() > short.specificity());
        assert!(short.specificity() > shorter.specificity());
        assert!(shorter.specificity() > every.specificity());
        assert_eq!(
            Target::parse("*.aa.com").unwrap().specificity(),
            Target::parse("*.bb.com").unwrap().specificity()
        );
        assert_eq!(exact.specificity().partial_cmp(&host.specificity()), None);
        assert!(!exact.matches_host("jira"));
        assert!(host.matches_host("API.GitHub.com"));
        assert!(!host.matches_host("other.github.com"));
        let wild = Target::parse("*.pastebin.com").unwrap();
        assert!(wild.matches_host("evil.pastebin.com"));
        assert!(wild.matches_host("a.b.pastebin.com"));
        assert!(!wild.matches_host("pastebin.com"));
        assert!(!wild.matches_host("notpastebin.com"));
        assert!(every.matches_host("anywhere.example"));
        assert!(!every.matches_host(""));
    }

    #[test]
    fn match_host_follows_network_match_domain() {
        assert!(match_host("*.example.com", "api.example.com"));
        assert!(match_host("*.example.com", "sub.api.example.com"));
        assert!(!match_host("*.example.com", "example.com"));
        assert!(!match_host("*.example.com", "fake-example.com"));
        assert!(match_host("example.com", "example.com"));
        assert!(match_host("Example.COM", "example.com"));
    }

    #[test]
    fn strictness_orders_deny_above_ask_above_side_effect_above_allow() {
        let deny = decision_strictness(PermissionDecision::Deny).unwrap();
        let ask = decision_strictness(PermissionDecision::Ask).unwrap();
        let allow = decision_strictness(PermissionDecision::Allow).unwrap();
        assert!(deny > ask);
        assert!(ask > allow);
        assert!(decision_strictness(PermissionDecision::Unspecified).is_none());

        let tool_deny = tool_default_strictness(ToolDefault::Deny).unwrap();
        let tool_ask = tool_default_strictness(ToolDefault::Ask).unwrap();
        let side_effect = tool_default_strictness(ToolDefault::BySideEffect).unwrap();
        let tool_allow = tool_default_strictness(ToolDefault::Allow).unwrap();
        assert!(tool_deny > tool_ask);
        assert!(tool_ask > side_effect);
        assert!(side_effect > tool_allow);
        assert!(tool_default_strictness(ToolDefault::Unspecified).is_none());
        assert_eq!(host_default_strictness(HostDefault::Deny), Some(deny));
        assert_eq!(host_default_strictness(HostDefault::Allow), Some(allow));
        assert!(host_default_strictness(HostDefault::Unspecified).is_none());
    }

    fn bare_agent() -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: "urn:vam:agent:hub.vam.dev/acme/perms".to_string(),
            }),
            display_name: "Perms".to_string(),
            role: "assistant".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions("Go.".to_string())),
            ..Default::default()
        }
    }

    fn policy_ref(urn: &str, version: bool, digest: bool) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            version: version.then(|| Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: digest.then(|| Digest {
                algorithm: 1,
                hex_hash: "ab".to_string(),
            }),
            locators: Vec::new(),
        }
    }

    fn descriptions(spec: &AgentSpec) -> Vec<String> {
        spec.validate()
            .err()
            .map(|err| err.field_violations)
            .unwrap_or_default()
            .into_iter()
            .map(|violation| format!("{}: {}", violation.field_path, violation.description))
            .collect()
    }

    const SECTION_IV_2: &str = r#"{
      "tools": {
        "default": "bySideEffect",
        "rules": [
          { "target": "skill:jira/create_issue", "decision": "ask" },
          { "target": "mcp:mcp.atlassian.com/*", "decision": "allow" },
          { "target": "builtin:run", "decision": "deny" }
        ]
      },
      "internet": {
        "default": "allow",
        "rules": [ { "target": "*.pastebin.com", "decision": "deny" } ]
      },
      "advanced": []
    }"#;

    #[test]
    fn section_iv_2_json_validates() {
        let permissions: AgentPermissions =
            json_to_proto("vam.v1.AgentPermissions", SECTION_IV_2).expect("decode §IV.2");
        validate_agent_permissions(&permissions).expect("§IV.2 permissions");
        let defaults = permissions.effective_defaults();
        assert_eq!(defaults.tools, ToolDefault::BySideEffect);
        assert_eq!(defaults.internet, HostDefault::Allow);
        assert_eq!(defaults.tools_strictness(), Strictness::BY_SIDE_EFFECT);
        assert_eq!(defaults.internet_strictness(), Strictness::ALLOW);

        let mut spec = bare_agent();
        spec.permissions = Some(permissions);
        spec.validate().expect("agent with §IV.2 permissions");

        let encoded = proto_to_json(
            "vam.v1.AgentPermissions",
            spec.permissions.as_ref().unwrap(),
        )
        .unwrap();
        assert!(encoded.contains("\"default\":\"bySideEffect\""));
        let again: AgentPermissions = json_to_proto("vam.v1.AgentPermissions", &encoded).unwrap();
        validate_agent_permissions(&again).unwrap();
    }

    #[test]
    fn j1_omitted_defaults_resolve_to_by_side_effect_and_allow() {
        for json in [
            r#"{"tools":{},"internet":{}}"#,
            r#"{}"#,
            r#"{"tools":{"rules":[]}}"#,
        ] {
            let permissions: AgentPermissions = json_to_proto("vam.v1.AgentPermissions", json)
                .unwrap_or_else(|err| {
                    panic!("{json}: {err}");
                });
            let defaults = permissions.effective_defaults();
            assert_eq!(defaults.tools, ToolDefault::BySideEffect, "{json}");
            assert_eq!(defaults.internet, HostDefault::Allow, "{json}");
            validate_agent_permissions(&permissions).unwrap_or_else(|err| panic!("{json}: {err}"));
        }

        let explicit: AgentPermissions = json_to_proto(
            "vam.v1.AgentPermissions",
            r#"{"tools":{"default":"deny"},"internet":{"default":"deny"}}"#,
        )
        .unwrap();
        let defaults = explicit.effective_defaults();
        assert_eq!(defaults.tools, ToolDefault::Deny);
        assert_eq!(defaults.internet, HostDefault::Deny);
        assert!(defaults.tools_strictness() > Strictness::BY_SIDE_EFFECT);
    }

    #[test]
    fn j7_ask_on_a_host_rule_is_rejected() {
        let permissions: AgentPermissions = json_to_proto(
            "vam.v1.AgentPermissions",
            r#"{"internet":{"rules":[{"target":"*.pastebin.com","decision":"ask"}]}}"#,
        )
        .unwrap();
        let err = validate_agent_permissions(&permissions).unwrap_err();
        let violation = &err.field_violations[0];
        assert_eq!(
            violation.field_path,
            "permissions.internet.rules[0].decision"
        );
        assert_eq!(violation.constraint, "host_decision");
        assert!(
            violation.description.contains("ask")
                && violation.description.contains("allow or deny")
                && violation.description.contains("*.pastebin.com"),
            "{}",
            violation.description
        );
    }

    #[test]
    fn each_validation_error_names_the_problem() {
        let mut permissions = AgentPermissions {
            tools: Some(ToolPermissions {
                default: 99,
                rules: vec![
                    PermissionRule {
                        target: "skill:jira/create_issue".to_string(),
                        decision: PermissionDecision::Ask as i32,
                    },
                    PermissionRule {
                        target: "skill:jira/create_issue".to_string(),
                        decision: PermissionDecision::Allow as i32,
                    },
                    PermissionRule {
                        target: "*.pastebin.com".to_string(),
                        decision: PermissionDecision::Deny as i32,
                    },
                    PermissionRule {
                        target: "skill:nope".to_string(),
                        decision: 0,
                    },
                    PermissionRule {
                        target: "mcp:https://MCP.Atlassian.com:443/v1/*".to_string(),
                        decision: PermissionDecision::Allow as i32,
                    },
                    PermissionRule {
                        target: "mcp:mcp.atlassian.com/v1/*".to_string(),
                        decision: PermissionDecision::Deny as i32,
                    },
                ],
            }),
            internet: Some(InternetPermissions {
                default: 99,
                rules: vec![
                    PermissionRule {
                        target: "skill:jira/create_issue".to_string(),
                        decision: PermissionDecision::Allow as i32,
                    },
                    PermissionRule {
                        target: "api.github.com".to_string(),
                        decision: 0,
                    },
                ],
            }),
            advanced: Vec::new(),
            web: Default::default(),
        };
        let err = validate_agent_permissions(&permissions).unwrap_err();
        let text = err.to_string();
        assert!(
            text.contains("tool default must be allow, ask, deny, or bySideEffect"),
            "{text}"
        );
        assert!(
            text.contains("duplicate target 'skill:jira/create_issue'"),
            "{text}"
        );
        assert!(text.contains("tool rules take a skill"), "{text}");
        assert!(text.contains("skill:nope"), "{text}");
        assert!(
            text.contains("duplicate target 'mcp:mcp.atlassian.com/v1/*'"),
            "{text}"
        );
        assert!(
            text.contains("internet default must be allow or deny"),
            "{text}"
        );
        assert!(
            text.contains("internet rules take a host pattern"),
            "{text}"
        );
        assert!(
            text.contains("host rule decision must be allow or deny"),
            "{text}"
        );

        permissions.tools.as_mut().unwrap().default = ToolDefault::Allow as i32;
        permissions.tools.as_mut().unwrap().rules.clear();
        permissions.internet.as_mut().unwrap().default = HostDefault::Allow as i32;
        permissions.internet.as_mut().unwrap().rules.clear();
        validate_agent_permissions(&permissions).unwrap();
    }

    #[test]
    fn permissions_and_legacy_policy_fields_are_exclusive() {
        let mut legacy = bare_agent();
        legacy.capability_policy = Some(CapabilityPolicyRef {
            policy_ref: Some(policy_ref(
                "urn:vam:policy:hub.vam.dev/acme/egress",
                false,
                false,
            )),
            ..Default::default()
        });
        legacy.hitl_policy = Some(HitlPolicyRef {
            monitored_tools: vec!["jira_delete_issue".to_string()],
            ..Default::default()
        });
        legacy
            .validate()
            .expect("legacy fields alone still validate");

        legacy.permissions = Some(AgentPermissions::default());
        let both = descriptions(&legacy).join("\n");
        assert!(
            both.contains("use `permissions.advanced` for hand-written Cedar"),
            "{both}"
        );

        let mut hitl_only = bare_agent();
        hitl_only.hitl_policy = Some(HitlPolicyRef::default());
        hitl_only.permissions = Some(AgentPermissions::default());
        let hitl = descriptions(&hitl_only).join("\n");
        assert!(hitl.contains("permissions.advanced"), "{hitl}");

        let mut capability_only = bare_agent();
        capability_only.capability_policy = Some(CapabilityPolicyRef::default());
        capability_only.permissions = Some(AgentPermissions::default());
        assert!(descriptions(&capability_only)
            .join("\n")
            .contains("permissions.advanced"));
    }

    #[test]
    fn advanced_refs_follow_the_policy_ref_rule() {
        let mut spec = bare_agent();
        spec.permissions = Some(AgentPermissions {
            advanced: vec![policy_ref(
                "urn:vam:policy:hub.vam.dev/acme/cedar",
                false,
                false,
            )],
            ..Default::default()
        });
        spec.validate().expect("floating policy ref");

        spec.permissions.as_mut().unwrap().advanced = vec![policy_ref(
            "urn:vam:skill:hub.vam.dev/acme/jira",
            false,
            false,
        )];
        let wrong_kind = descriptions(&spec).join("\n");
        assert!(wrong_kind.contains("policy"), "{wrong_kind}");

        spec.permissions.as_mut().unwrap().advanced = vec![policy_ref(
            "urn:vam:policy:hub.vam.dev/acme/cedar",
            false,
            true,
        )];
        let digest = descriptions(&spec).join("\n");
        assert!(digest.contains("digest requires a version"), "{digest}");
    }

    #[test]
    fn permissions_example_agent_validates() {
        let raw = include_str!("../../../examples/permissions-assistant/agent.json");
        let spec: AgentSpec = json_to_proto("vam.v1.AgentSpec", raw).expect("decode example");
        spec.validate().expect("example agent");
        assert!(spec.capability_policy.is_none());
        assert!(spec.hitl_policy.is_none());
        let permissions = spec.permissions.expect("permissions");
        validate_agent_permissions(&permissions).unwrap();
        let tools = permissions.tools.expect("tools");
        assert_eq!(tools.rules.len(), 3);
        for rule in &tools.rules {
            Target::parse(&rule.target).unwrap_or_else(|err| panic!("{}: {err}", rule.target));
        }
        let internet = permissions.internet.expect("internet");
        assert_eq!(
            Target::parse(&internet.rules[0].target)
                .unwrap()
                .to_string(),
            "*.pastebin.com"
        );
    }
}
