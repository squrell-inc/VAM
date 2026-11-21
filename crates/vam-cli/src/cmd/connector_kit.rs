//! `vam connector init`, `validate` and `test`: the developer kit, offline (no vamd, no token).
//!
//! `init <dir>` writes a marketplace root for one connector that passes `validate` and `test` as
//! written:
//!
//! ```text
//! marketplace.json
//! listings/<id>/connector/connector.json
//! listings/<id>/skills/<id>-read/{manifest.json, SKILLS.md, read.ts | read.py}   (REST only)
//! listings/<id>/agents/<id>-assistant/agent.json
//! listings/<id>/tests/connector-test.json
//! ```
//!
//! A REST connector (`--host`) gets a skill with one read tool whose shape is the binding host; an
//! MCP connector (`--mcp-url`) is the node's to call, so it gets no skill. The files are templates
//! under `crates/vam-cli/templates/connector/`. Before any file is written the connector is
//! checked with the marketplace's own rules (`parse_marketplace_connector`, so a reserved or
//! malformed id is refused in the node's words) and the whole tree is checked in a scratch
//! directory with the same check `validate` runs.
//!
//! `validate <dir>` takes a marketplace root or one listing (`listings/<id>` under a root) and runs
//! `check_marketplace_root`, listing rules included (`check_listing`): what an install would
//! refuse. Output is `vam catalog validate`'s, with `ok <kind> <urn>` lines; exit 6 on an error
//! (and on a warning with `--strict`).
//!
//! `test <dir>` runs each tool of a listing's skills against the mock its test file declares, behind
//! the egress gate's own screening, offline (see [`runner`]); `pass <tool>` or `fail <tool>: <the
//! gate's words>`, exit 6 on any failure.

use std::fs;
use std::path::Path;

use anyhow::Result;
use clap::{Args, ValueEnum};
use vam_spec::connector_kit::{ConnectorTestFile, TEST_FILE_PATH};
use vam_spec::connector_listing::parse_marketplace_connector;
use vam_spec::marketplace::{
    check_marketplace_listing, check_marketplace_root, MarketplaceReport, INDEX_FILE,
};

use super::catalog_validate::present;
use crate::output::{fail, ExitCode};

const MARKETPLACE_REST: &str = include_str!("../../templates/connector/marketplace.rest.json");
const MARKETPLACE_MCP: &str = include_str!("../../templates/connector/marketplace.mcp.json");
const CONNECTOR_API_KEY_REST: &str =
    include_str!("../../templates/connector/connector.api_key.rest.json");
const CONNECTOR_API_KEY_MCP: &str =
    include_str!("../../templates/connector/connector.api_key.mcp.json");
const CONNECTOR_OAUTH_REST: &str =
    include_str!("../../templates/connector/connector.oauth.rest.json");
const CONNECTOR_DISCOVER_MCP: &str =
    include_str!("../../templates/connector/connector.discover.mcp.json");
const SKILL_MANIFEST: &str = include_str!("../../templates/connector/skill.manifest.json");
const SKILL_MD: &str = include_str!("../../templates/connector/skill.SKILLS.md");
const SKILL_TOOL_PY: &str = include_str!("../../templates/connector/skill.tool.py");
const SKILL_TOOL_TS: &str = include_str!("../../templates/connector/skill.tool.ts");
const AGENT_REST: &str = include_str!("../../templates/connector/agent.rest.json");
const AGENT_MCP: &str = include_str!("../../templates/connector/agent.mcp.json");
const TEST_REST: &str = include_str!("../../templates/connector/connector-test.rest.json");
const TEST_MCP: &str = include_str!("../../templates/connector/connector-test.mcp.json");

/// How the member signs in to the connector.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Auth {
    /// The member types an API key (REST with `--host`, or MCP with `--mcp-url`).
    #[value(name = "api_key")]
    ApiKey,
    /// OAuth with fixed endpoints and a client the org's admin registers (REST, `--host`).
    #[value(name = "oauth")]
    Oauth,
    /// OAuth by discovery from a remote MCP server (`--mcp-url`).
    #[value(name = "discover")]
    Discover,
}

/// The language of the skill's tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Runtime {
    #[value(name = "bun")]
    Bun,
    #[value(name = "python3")]
    Python3,
}

#[derive(Args, Debug, Clone)]
pub struct InitArgs {
    /// The directory to write the marketplace root into: new, empty, or holding only `.keep`.
    pub dir: String,
    /// The connector id (the last segment of its URN): a-z, 0-9, `-` or `_`.
    #[arg(long)]
    pub id: String,
    /// How the member signs in: `api_key`, `oauth` (REST) or `discover` (MCP).
    #[arg(long, value_enum)]
    pub auth: Auth,
    /// The REST API's host, like `api.example.com` (a company address: `{subdomain}.<suffix>`,
    /// with `--tenant-suffix`).
    #[arg(long)]
    pub host: Option<String>,
    /// The remote MCP server's URL, like `https://mcp.example.com/mcp`.
    #[arg(long)]
    pub mcp_url: Option<String>,
    /// The suffix of a company address connector: the host is `{subdomain}.<suffix>` and the
    /// admin enters the subdomain at Enable. Pass the host as `--host '{subdomain}.<suffix>'`.
    #[arg(long)]
    pub tenant_suffix: Option<String>,
    /// The domain the listing publishes under (`publisherDomain`; the URNs are under it).
    #[arg(long)]
    pub domain: String,
    /// The language of the skill's tool (REST only).
    #[arg(long, value_enum, default_value = "bun")]
    pub runtime: Runtime,
}

/// `vam connector init`.
pub fn init(args: &InitArgs) -> Result<()> {
    let files = plan(args)?;
    let dir = Path::new(&args.dir);
    check_target(dir)?;
    // The whole tree is checked as `validate` checks it before the target is touched.
    let scratch = tempfile::tempdir().map_err(|e| fail(ExitCode::Failure, e.to_string()))?;
    write_files(scratch.path(), &files)?;
    let report = check_marketplace_root(scratch.path());
    if !report.findings.is_empty() {
        let lines: Vec<String> = report
            .findings
            .iter()
            .map(|f| f.message.split_whitespace().collect::<Vec<_>>().join(" "))
            .collect();
        return Err(fail(ExitCode::Invalid, lines.join("; ")));
    }
    write_files(dir, &files)?;
    for (path, _) in &files {
        println!("wrote {path}");
    }
    println!(
        "next: vam connector validate {dir}   (then `vam connector test {dir}`)",
        dir = args.dir
    );
    Ok(())
}

/// `vam connector validate`.
pub fn validate(dir: &str, strict: bool) -> Result<()> {
    let path = Path::new(dir);
    let report = if path.join(INDEX_FILE).exists() {
        check_marketplace_root(path)
    } else {
        check_listing_dir(path, dir)?
    };
    present(&report, strict, "ok ")
}

/// `vam connector test`: the tools of a listing against the mock its test file declares (CN-7.5).
pub fn test(dir: &str, listing: Option<&str>, subdomain: Option<&str>) -> Result<()> {
    runner::test(dir, listing, subdomain)
}

/// `listings/<id>` under a marketplace root: that listing alone.
fn check_listing_dir(path: &Path, shown: &str) -> Result<MarketplaceReport> {
    let (root, id) = locate_listing(path, shown)?;
    Ok(check_marketplace_listing(&root, &id))
}

/// The marketplace root and the id of the listing directory `path` (`listings/<id>` under a root).
fn locate_listing(path: &Path, shown: &str) -> Result<(std::path::PathBuf, String)> {
    let not_a_root = || {
        fail(
            ExitCode::Invalid,
            format!(
                "`{shown}` is neither a marketplace root (no {INDEX_FILE}) nor a listing directory (listings/<id> under one)"
            ),
        )
    };
    let abs = fs::canonicalize(path).map_err(|_| not_a_root())?;
    let id = abs
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(not_a_root)?;
    let listings = abs.parent().ok_or_else(not_a_root)?;
    let root = listings.parent().ok_or_else(not_a_root)?;
    if listings.file_name().and_then(|n| n.to_str()) != Some("listings")
        || !root.join(INDEX_FILE).exists()
    {
        return Err(not_a_root());
    }
    Ok((root.to_path_buf(), id.to_string()))
}

// ---- the files init writes -----------------------------------------------------------------------

/// The files of the kit, as (path under the root, text), after the connector passed the
/// marketplace's checks. Nothing is written here.
fn plan(a: &InitArgs) -> Result<Vec<(String, String)>> {
    let usage = |m: String| fail(ExitCode::Usage, m);
    let rest = match (&a.host, &a.mcp_url) {
        (Some(_), None) => true,
        (None, Some(_)) => false,
        _ => return Err(usage("give exactly one of --host and --mcp-url".into())),
    };
    match (a.auth, rest) {
        (Auth::Oauth, false) => {
            return Err(usage(
                "--auth oauth is for a REST API (--host); a remote MCP server uses --auth discover or api_key".into(),
            ))
        }
        (Auth::Discover, true) => {
            return Err(usage(
                "--auth discover is for a remote MCP server (--mcp-url); a REST API uses --auth oauth or api_key".into(),
            ))
        }
        _ => {}
    }
    let host = a.host.clone().unwrap_or_default();
    match (&a.tenant_suffix, rest) {
        (Some(_), false) => return Err(usage("--tenant-suffix is for a REST API (--host)".into())),
        (Some(sfx), true) if host != format!("{{subdomain}}.{sfx}") => {
            return Err(usage(format!(
                "--tenant-suffix `{sfx}` makes the host `{{subdomain}}.{sfx}`: pass that as --host"
            )))
        }
        (None, true) if host.contains('{') => {
            return Err(usage(
                "a host with `{subdomain}` needs --tenant-suffix (the part after `{subdomain}.`)"
                    .into(),
            ))
        }
        _ => {}
    }
    let mcp_url = a.mcp_url.clone().unwrap_or_default();

    let name = display_name(&a.id);
    let what = match (a.auth, rest) {
        (Auth::ApiKey, true) => "through its REST API and the API key the member enters",
        (Auth::ApiKey, false) => "through its remote MCP server and the API key the member enters",
        (Auth::Oauth, _) => "through its REST API and the member's own sign-in",
        (Auth::Discover, _) => "through its remote MCP server and the member's own sign-in",
    };
    let test_host = if rest {
        match &a.tenant_suffix {
            Some(sfx) => format!("demo.{sfx}"),
            None => host.clone(),
        }
    } else {
        url_host(&mcp_url)
    };
    let tenant = a.tenant_suffix.as_ref().map_or_else(String::new, |sfx| {
        format!(
            "\"tenant\": {{\n    \"name\": \"subdomain\",\n    \"suffix\": \"{}\",\n    \"label\": \"{} address\"\n  }},\n  ",
            json_escape(sfx),
            json_escape(&name)
        )
    });
    let skill = format!("{}-read", a.id);
    let (command, entry) = match a.runtime {
        Runtime::Bun => ("bun", "read.ts"),
        Runtime::Python3 => ("python3", "read.py"),
    };
    let vars = Vars(vec![
        ("id", a.id.clone()),
        ("name", name.clone()),
        ("domain", a.domain.clone()),
        (
            "description",
            format!("{name} items in the member's own account, {what}."),
        ),
        (
            "icon",
            a.id.replace('_', "-").chars().take(40).collect::<String>(),
        ),
        ("host", host),
        ("mcpUrl", mcp_url),
        ("testHost", test_host),
        ("skill", skill.clone()),
        ("agent", format!("{}-assistant", a.id)),
        ("tool", format!("{}_read", a.id.replace('-', "_"))),
        ("command", command.to_string()),
        ("entry", entry.to_string()),
    ]);

    // The connector first, by the marketplace's rules: a reserved or malformed id, a host or an
    // MCP URL it refuses, are said in the node's words before anything else is built.
    let connector_tpl = match (a.auth, rest) {
        (Auth::ApiKey, true) => CONNECTOR_API_KEY_REST,
        (Auth::ApiKey, false) => CONNECTOR_API_KEY_MCP,
        (Auth::Oauth, _) => CONNECTOR_OAUTH_REST,
        (Auth::Discover, _) => CONNECTOR_DISCOVER_MCP,
    };
    let connector = vars.render(&connector_tpl.replace("{{tenant}}", &tenant), true);
    parse_marketplace_connector(&connector)
        .map_err(|e| fail(ExitCode::Invalid, format!("connector `{}`: {e}", a.id)))?;

    let id = &a.id;
    let mut files = vec![
        (
            INDEX_FILE.to_string(),
            vars.render(
                if rest {
                    MARKETPLACE_REST
                } else {
                    MARKETPLACE_MCP
                },
                true,
            ),
        ),
        (format!("listings/{id}/connector/connector.json"), connector),
    ];
    if rest {
        let tool = match a.runtime {
            Runtime::Bun => SKILL_TOOL_TS,
            Runtime::Python3 => SKILL_TOOL_PY,
        };
        let dir = format!("listings/{id}/skills/{skill}");
        files.push((
            format!("{dir}/manifest.json"),
            vars.render(SKILL_MANIFEST, true),
        ));
        files.push((format!("{dir}/SKILLS.md"), vars.render(SKILL_MD, false)));
        files.push((format!("{dir}/{entry}"), vars.render(tool, false)));
    }
    files.push((
        format!("listings/{id}/agents/{id}-assistant/agent.json"),
        vars.render(if rest { AGENT_REST } else { AGENT_MCP }, true),
    ));
    let test_file = vars.render(if rest { TEST_REST } else { TEST_MCP }, true);
    ConnectorTestFile::parse(&test_file)
        .map_err(|e| fail(ExitCode::Invalid, format!("the test file: {e}")))?;
    files.push((format!("listings/{id}/{TEST_FILE_PATH}"), test_file));
    Ok(files)
}

/// `{{name}}` tokens and their values. A template that leaves a token unreplaced is a bug here, not
/// the user's, and a test catches it.
struct Vars(Vec<(&'static str, String)>);

impl Vars {
    /// `json`: the values go inside JSON strings, so they are escaped.
    fn render(&self, template: &str, json: bool) -> String {
        let mut out = template.to_string();
        for (k, v) in &self.0 {
            let v = if json { json_escape(v) } else { v.clone() };
            out = out.replace(&format!("{{{{{k}}}}}"), &v);
        }
        out
    }
}

fn json_escape(s: &str) -> String {
    let quoted = serde_json::Value::String(s.to_string()).to_string();
    quoted[1..quoted.len() - 1].to_string()
}

/// `notesdesk` is `Notesdesk`, `my_api-2` is `My Api 2`.
fn display_name(id: &str) -> String {
    id.split(['-', '_'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut c = w.chars();
            c.next()
                .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                .unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// The host of an `https://host[:port]/path` URL as written (the connector check has the last
/// word on whether it is allowed).
fn url_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    authority
        .split_once(':')
        .map_or(authority, |(h, _)| h)
        .to_string()
}

/// `dir` is new, empty, or holds only `.keep`.
fn check_target(dir: &Path) -> Result<()> {
    match fs::metadata(dir) {
        Err(_) => Ok(()),
        Ok(m) if !m.is_dir() => Err(fail(
            ExitCode::Conflict,
            format!("`{}` exists and is not a directory", dir.display()),
        )),
        Ok(_) => {
            let entries = fs::read_dir(dir).map_err(|e| {
                fail(
                    ExitCode::Failure,
                    format!("cannot read `{}`: {e}", dir.display()),
                )
            })?;
            let busy = entries
                .filter_map(Result::ok)
                .any(|e| e.file_name() != ".keep");
            if busy {
                return Err(fail(
                    ExitCode::Conflict,
                    format!(
                        "`{}` is not empty: `vam connector init` writes into a new or empty directory (one holding only `.keep` counts as empty)",
                        dir.display()
                    ),
                ));
            }
            Ok(())
        }
    }
}

fn write_files(root: &Path, files: &[(String, String)]) -> Result<()> {
    for (rel, text) in files {
        let path = root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                fail(
                    ExitCode::Failure,
                    format!("cannot create `{}`: {e}", parent.display()),
                )
            })?;
        }
        fs::write(&path, text).map_err(|e| {
            fail(
                ExitCode::Failure,
                format!("cannot write `{}`: {e}", path.display()),
            )
        })?;
    }
    Ok(())
}

mod runner;

#[cfg(test)]
mod tests;
