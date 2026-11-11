use std::fs;
use std::io::{copy, Cursor};
use std::path::Path;

use anyhow::{Context, Result};
use clap::Subcommand;
use flate2::write::GzEncoder;
use flate2::Compression;
use prost::Message;
use vam_spec::v1::{
    AgentSpec, GetAgentRequest, GetPlaybookRequest, GetPolicyRequest, GetSkillRequest,
    PlaybookSpec, PutAgentRequest, PutPlaybookRequest, PutPolicyRequest, PutSkillRequest,
    ReleaseCatalogRequest, ReleaseSkillPackageRequest, SkillManifest,
};

use super::agent_check;
use super::playbook_local;
use super::release_suggest;
use crate::client::{self, encode_path_segment, Client};
use crate::config;
use crate::output::{fail, ExitCode};

#[derive(Subcommand)]
pub enum AgentCmd {
    Put {
        path: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Get {
        urn: String,
    },
    /// Promote the current draft to released for X.Y.Z.
    Release {
        urn: String,
        /// X.Y.Z. Omit to accept the suggested version (asks on a terminal).
        #[arg(long)]
        version: Option<String>,
        /// Release note, at most 500 characters.
        #[arg(long)]
        note: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// List versions: your draft first, then releases newest first.
    Versions {
        urn: String,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Print the suggested next version for the current draft. Does not release.
    SuggestVersion {
        urn: String,
    },
    /// Show what the agent's playbooks require and whether the agent attaches it.
    /// Informational: exits 0 even when skills are missing.
    Check {
        urn: String,
        /// Check this released version. Default: the latest release, else your draft.
        #[arg(long, conflicts_with = "draft")]
        version: Option<String>,
        /// Check your draft.
        #[arg(long)]
        draft: bool,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// What the agent's stored receipts add up to: success rate, pass^k, cost per success,
    /// effects. `--diff` compares the version with the release before it.
    Stats {
        agent: String,
        /// `X.Y.Z` or `draft`. Omit to list versions that have runs.
        #[arg(long)]
        version: Option<String>,
        /// pass^k size (server default).
        #[arg(long)]
        k: Option<u32>,
        /// Compare with the previous release (the draft, unless `--version` is given).
        #[arg(long)]
        diff: bool,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Show or replace draft permissions, or print the effective table.
    Permissions {
        #[command(subcommand)]
        command: super::permissions::PermissionsCmd,
    },
    /// Convert a legacy draft (capability policy or monitored tools) into permissions.
    Migrate {
        agent: String,
        /// Drop guest tool names that map to no target.
        #[arg(long)]
        force: bool,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
        /// Print the raw JSON report.
        #[arg(long)]
        json: bool,
    },
    Ls,
}

#[derive(Subcommand)]
pub enum PlaybookCmd {
    Put {
        path: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Get {
        urn: String,
    },
    /// Promote the current draft to released for X.Y.Z.
    Release {
        urn: String,
        /// X.Y.Z. Omit to accept the suggested version (asks on a terminal).
        #[arg(long)]
        version: Option<String>,
        /// Release note, at most 500 characters.
        #[arg(long)]
        note: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// List versions: your draft first, then releases newest first.
    Versions {
        urn: String,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Print the suggested next version for the current draft. Does not release.
    SuggestVersion {
        urn: String,
    },
    Ls,
    /// Write a starter v2 playbook file. Local only: does not contact vamd.
    Init {
        /// Path to write.
        #[arg(long, default_value = "playbook.json")]
        out: String,
        /// Overwrite the file if it already exists.
        #[arg(long)]
        force: bool,
    },
    /// Print a playbook file's steps, gates, guards and mode ratio. Local only: does not contact vamd.
    Explain {
        path: String,
    },
    /// Run a scripted scenario against a playbook's host rules, once per trial. Posts to vamd.
    Test {
        path: String,
        /// Scenario file: `actions` to apply and `expect` to check.
        #[arg(long)]
        scenario: String,
        /// Number of trials (1 to 30). Reports pass^k.
        #[arg(long, default_value_t = 1)]
        repeat: u32,
    },
}

#[derive(Subcommand)]
pub enum PolicyCmd {
    /// Upload Cedar policy text to the catalog.
    Put {
        path: String,
        #[arg(long)]
        urn: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Get {
        urn: String,
    },
    /// Promote the current draft to released for X.Y.Z.
    Release {
        urn: String,
        /// X.Y.Z. Omit to accept the suggested version (asks on a terminal).
        #[arg(long)]
        version: Option<String>,
        /// Release note, at most 500 characters.
        #[arg(long)]
        note: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// List versions: your draft first, then releases newest first.
    Versions {
        urn: String,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Print the suggested next version for the current draft. Does not release.
    SuggestVersion {
        urn: String,
    },
    /// Show which rule decides a tool or a host.
    Explain {
        agent: String,
        /// Guest tool name or target.
        #[arg(conflicts_with = "host")]
        tool: Option<String>,
        /// Explain this host instead of a tool.
        #[arg(long, conflicts_with = "tool")]
        host: Option<String>,
        /// Release X.Y.Z. Default: your draft if you own one, otherwise the latest release.
        #[arg(long)]
        version: Option<String>,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    Ls,
}

#[derive(Subcommand)]
pub enum SkillCmd {
    /// Upsert JSON SkillManifest metadata (catalog table).
    Put {
        path: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Fetch skill metadata, or a package row when --version/--channel is set.
    Get {
        urn: String,
        #[arg(long)]
        version: Option<String>,
        #[arg(long, value_parser = ["draft", "released"])]
        channel: Option<String>,
    },
    /// Publish a skill directory as a draft package (tar.gz multipart upload).
    Publish {
        dir: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Promote the current draft package to released for X.Y.Z.
    Release {
        urn: String,
        /// X.Y.Z. Omit to accept the suggested version (asks on a terminal).
        #[arg(long)]
        version: Option<String>,
        /// Release note, at most 500 characters.
        #[arg(long)]
        note: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// List versions: your draft first, then releases newest first.
    Versions {
        urn: String,
        /// Print the raw JSON response.
        #[arg(long)]
        json: bool,
    },
    /// Print the suggested next version for the current draft. Does not release.
    SuggestVersion { urn: String },
    /// List published skill packages.
    Ls,
}

/// `vam catalog …`: operations on catalog content that are not one artifact kind.
#[derive(Subcommand)]
pub enum CatalogCmd {
    /// Check a skill, agent or playbook directory, or a marketplace root, offline. No vamd.
    ///
    /// A marketplace root is a directory with marketplace.json: every listing's artifacts are
    /// checked. One line per finding; exit code 6 when there is an error.
    Validate {
        /// An artifact directory, or a marketplace root.
        dir: String,
        /// Treat warnings as failures (marketplace CI).
        #[arg(long)]
        strict: bool,
        /// The artifact kind to expect in an artifact directory: skill, agent or playbook.
        #[arg(long)]
        kind: Option<String>,
    },
    /// Install a listing of a trusted marketplace at its tag (org admin). The same import as the
    /// Marketplace page: the node reads the tag's commit, checks every package, and writes
    /// read-only released rows with their origin. The URL must be a marketplace the node already
    /// trusts (`vam marketplace ls`); --ref must be the tag its index names for the listing.
    Import {
        /// The marketplace's Git URL, as `vam marketplace ls` shows it.
        url: String,
        /// The listing's directory in the repository, e.g. listings/microsoft-365.
        #[arg(long)]
        path: String,
        /// The tag to install, e.g. microsoft-365-v1.0.0. An older tag of the listing is updated.
        #[arg(long = "ref")]
        git_ref: String,
    },
}

pub async fn run_catalog(
    host: Option<String>,
    token: Option<String>,
    cmd: CatalogCmd,
) -> Result<()> {
    match cmd {
        CatalogCmd::Validate { dir, strict, kind } => {
            super::catalog_validate::run(&dir, strict, kind.as_deref())
        }
        CatalogCmd::Import { url, path, git_ref } => {
            super::catalog_import::run(host, token, &url, &path, &git_ref).await
        }
    }
}

pub async fn run_agent(host: Option<String>, token: Option<String>, cmd: AgentCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        AgentCmd::Put {
            path,
            idempotency_key,
        } => {
            let spec = load_json_proto::<AgentSpec>(&path, "vam.v1.AgentSpec")?;
            let req = PutAgentRequest {
                spec: Some(spec),
                ..Default::default()
            };
            let body = c
                .put_json(
                    "/v1/agents",
                    "vam.v1.PutAgentRequest",
                    &req,
                    idempotency_key.as_deref(),
                )
                .await?;
            client::print_json(&body);
        }
        AgentCmd::Get { urn } => {
            let path = format!("/v1/agents/{}", urlencoding(&urn));
            let body = c.get(&path).await?;
            client::print_json(&body);
        }
        AgentCmd::Release {
            urn,
            version,
            note,
            idempotency_key,
        } => {
            let version = release_suggest::version_for_release(&c, "agents", &urn, version).await?;
            // The draft against the release before it, on stderr; never blocks the release.
            match super::outcomes::release_preface(&c, &urn).await {
                Ok(text) => eprint!("{text}"),
                Err(e) => eprintln!("warning: could not fetch the outcomes diff: {e}"),
            }
            let body = release_catalog(
                &c,
                "agents",
                &urn,
                &version,
                note.as_deref(),
                idempotency_key.as_deref(),
            )
            .await?;
            client::print_json(&body);
        }
        AgentCmd::Versions { urn, json } => print_versions(&c, "agents", &urn, json).await?,
        AgentCmd::SuggestVersion { urn } => {
            release_suggest::suggest_version(&c, "agents", &urn).await?;
        }
        AgentCmd::Check {
            urn,
            version,
            draft,
            json,
        } => agent_check::run(&c, &urn, version.as_deref(), draft, json).await?,
        AgentCmd::Stats {
            agent,
            version,
            k,
            diff,
            json,
        } => super::outcomes::run(&c, &agent, version.as_deref(), k, diff, json).await?,
        AgentCmd::Permissions { command } => {
            super::permissions::run_permissions(&c, command).await?
        }
        AgentCmd::Migrate {
            agent,
            force,
            idempotency_key,
            json,
        } => {
            super::permissions::run_migrate(&c, &agent, force, idempotency_key.as_deref(), json)
                .await?
        }
        AgentCmd::Ls => {
            let body = c.get("/v1/agents").await?;
            client::print_json(&body);
        }
    }
    Ok(())
}

pub async fn run_playbook(
    host: Option<String>,
    token: Option<String>,
    cmd: PlaybookCmd,
) -> Result<()> {
    // Local subcommands run before config resolution, so they work without a configured host.
    if let PlaybookCmd::Init { out, force } = &cmd {
        return playbook_local::init(out, *force);
    }
    if let PlaybookCmd::Explain { path } = &cmd {
        return playbook_local::explain(path);
    }
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        PlaybookCmd::Init { .. } | PlaybookCmd::Explain { .. } => {
            unreachable!("local playbook subcommands return before config resolution")
        }
        PlaybookCmd::Put {
            path,
            idempotency_key,
        } => {
            let spec = load_json_proto::<PlaybookSpec>(&path, "vam.v1.PlaybookSpec")?;
            let mut migrated = spec.clone();
            vam_spec::playbook::migrate_playbook_spec(&mut migrated);
            let warnings = vam_spec::validation::playbook_enforcement_warnings(&migrated);
            let req = PutPlaybookRequest {
                spec: Some(spec),
                ..Default::default()
            };
            let body = c
                .put_json(
                    "/v1/playbooks",
                    "vam.v1.PutPlaybookRequest",
                    &req,
                    idempotency_key.as_deref(),
                )
                .await?;
            // Warnings go to stderr after success. Stdout stays the vamd body.
            for warning in &warnings {
                eprintln!("warning: {warning}");
            }
            client::print_json(&body);
        }
        PlaybookCmd::Get { urn } => {
            let path = format!("/v1/playbooks/{}", urlencoding(&urn));
            let body = c.get(&path).await?;
            client::print_json(&body);
        }
        PlaybookCmd::Release {
            urn,
            version,
            note,
            idempotency_key,
        } => {
            let version =
                release_suggest::version_for_release(&c, "playbooks", &urn, version).await?;
            let body = release_catalog(
                &c,
                "playbooks",
                &urn,
                &version,
                note.as_deref(),
                idempotency_key.as_deref(),
            )
            .await?;
            client::print_json(&body);
        }
        PlaybookCmd::Versions { urn, json } => print_versions(&c, "playbooks", &urn, json).await?,
        PlaybookCmd::SuggestVersion { urn } => {
            release_suggest::suggest_version(&c, "playbooks", &urn).await?;
        }
        PlaybookCmd::Ls => {
            let body = c.get("/v1/playbooks").await?;
            client::print_json(&body);
        }
        PlaybookCmd::Test {
            path,
            scenario,
            repeat,
        } => {
            let spec: PlaybookSpec = load_json_proto(&path, "vam.v1.PlaybookSpec")
                .map_err(|e| fail(ExitCode::Invalid, format!("{path}: {e:#}")))?;
            let spec_json = vam_spec::encoding::proto_to_json("vam.v1.PlaybookSpec", &spec)
                .map_err(anyhow::Error::msg)?;
            let scenario_raw =
                fs::read_to_string(&scenario).with_context(|| format!("read {scenario:?}"))?;
            let scenario_value: serde_json::Value = serde_json::from_str(&scenario_raw)
                .map_err(|e| fail(ExitCode::Invalid, format!("{scenario}: {e}")))?;
            let body = serde_json::json!({
                "spec": serde_json::from_str::<serde_json::Value>(&spec_json)?,
                "scenario": scenario_value,
                "repeat": repeat,
            });
            let resp = c
                .post_raw_json("/v1/playbook-tests", &body.to_string(), "playbook-test")
                .await?;
            playbook_local::print_test_report(&resp)?;
        }
    }
    Ok(())
}

/// The vamd URL for a policy command. `vam policy explain <agent> --host <site>` shares the name of the
/// global `--host` (the vamd URL), and clap hands the same value to both, so for that form the value is
/// the site and the server comes from config or `VAM_HOST`.
pub fn policy_server_host(global_host: Option<String>, cmd: &PolicyCmd) -> Option<String> {
    match cmd {
        PolicyCmd::Explain { host: Some(_), .. } => None,
        _ => global_host,
    }
}

pub async fn run_policy(host: Option<String>, token: Option<String>, cmd: PolicyCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        PolicyCmd::Put {
            path,
            urn,
            idempotency_key,
        } => {
            let cedar = fs::read_to_string(&path).with_context(|| format!("read {:?}", path))?;
            let req = PutPolicyRequest {
                urn,
                cedar,
                ..Default::default()
            };
            let body = c
                .put_json(
                    "/v1/policies",
                    "vam.v1.PutPolicyRequest",
                    &req,
                    idempotency_key.as_deref(),
                )
                .await?;
            client::print_json(&body);
        }
        PolicyCmd::Get { urn } => {
            let path = format!("/v1/policies/{}", urlencoding(&urn));
            let body = c.get(&path).await?;
            client::print_json(&body);
        }
        PolicyCmd::Release {
            urn,
            version,
            note,
            idempotency_key,
        } => {
            let version =
                release_suggest::version_for_release(&c, "policies", &urn, version).await?;
            let body = release_catalog(
                &c,
                "policies",
                &urn,
                &version,
                note.as_deref(),
                idempotency_key.as_deref(),
            )
            .await?;
            client::print_json(&body);
        }
        PolicyCmd::Versions { urn, json } => print_versions(&c, "policies", &urn, json).await?,
        PolicyCmd::SuggestVersion { urn } => {
            release_suggest::suggest_version(&c, "policies", &urn).await?;
        }
        PolicyCmd::Explain {
            agent,
            tool,
            host,
            version,
            json,
        } => {
            super::permissions::run_explain(
                &c,
                &agent,
                tool.as_deref(),
                host.as_deref(),
                version.as_deref(),
                json,
            )
            .await?
        }
        PolicyCmd::Ls => {
            let body = c.get("/v1/policies").await?;
            client::print_json(&body);
        }
    }
    Ok(())
}

pub async fn run_skill(host: Option<String>, token: Option<String>, cmd: SkillCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        SkillCmd::Put {
            path,
            idempotency_key,
        } => {
            let spec = load_json_proto::<SkillManifest>(&path, "vam.v1.SkillManifest")?;
            let req = PutSkillRequest {
                spec: Some(spec),
                ..Default::default()
            };
            let body = c
                .put_json(
                    "/v1/skills",
                    "vam.v1.PutSkillRequest",
                    &req,
                    idempotency_key.as_deref(),
                )
                .await?;
            client::print_json(&body);
        }
        SkillCmd::Get {
            urn,
            version,
            channel,
        } => {
            if version.is_some() || channel.is_some() {
                let mut path = format!("/v1/skills/packages/{}", encode_path_segment(&urn));
                let mut query = Vec::new();
                if let Some(channel) = channel {
                    query.push(format!("channel={channel}"));
                }
                if let Some(version) = version {
                    query.push(format!("version={version}"));
                }
                if !query.is_empty() {
                    path.push('?');
                    path.push_str(&query.join("&"));
                }
                let body = c.get(&path).await?;
                client::print_json(&body);
            } else {
                let path = format!("/v1/skills/{}", urlencoding(&urn));
                let body = c.get(&path).await?;
                client::print_json(&body);
            }
        }
        SkillCmd::Publish {
            dir,
            idempotency_key,
        } => {
            let archive = tar_gz_directory(Path::new(&dir))?;
            let body = c
                .post_multipart(
                    "/v1/skills/packages",
                    "file",
                    "skill-package.tar.gz",
                    "application/gzip",
                    archive,
                    idempotency_key.as_deref(),
                )
                .await?;
            client::print_json(&body);
        }
        SkillCmd::Release {
            urn,
            version,
            note,
            idempotency_key,
        } => {
            let version = release_suggest::version_for_release(&c, "skills", &urn, version).await?;
            let req = ReleaseSkillPackageRequest {
                urn: urn.clone(),
                version: Some(parse_version_triplet(&version)?),
                note: note.unwrap_or_default(),
                ..Default::default()
            };
            let path = format!("/v1/skills/packages/{}/release", encode_path_segment(&urn));
            let body = c
                .post_json(
                    &path,
                    "vam.v1.ReleaseSkillPackageRequest",
                    &req,
                    idempotency_key.as_deref(),
                )
                .await?;
            client::print_json(&body);
        }
        SkillCmd::Versions { urn, json } => {
            print_versions(&c, "skills/packages", &urn, json).await?
        }
        SkillCmd::SuggestVersion { urn } => {
            release_suggest::suggest_version(&c, "skills", &urn).await?;
        }
        SkillCmd::Ls => {
            let body = c.get("/v1/skills/packages").await?;
            client::print_json(&body);
        }
    }
    Ok(())
}

async fn release_catalog(
    c: &Client,
    collection: &str,
    urn: &str,
    version: &str,
    note: Option<&str>,
    idempotency_key: Option<&str>,
) -> Result<String> {
    let req = ReleaseCatalogRequest {
        version: Some(parse_version_triplet(version)?),
        note: note.unwrap_or_default().to_string(),
        ..Default::default()
    };
    let path = format!("/v1/{collection}/{}/release", encode_path_segment(urn));
    c.post_json(&path, "vam.v1.ReleaseCatalogRequest", &req, idempotency_key)
        .await
}

/// `GET /v1/<collection>/<urn>/versions`. A table by default; the raw JSON with
/// `--json` or the global JSON output mode.
async fn print_versions(c: &Client, collection: &str, urn: &str, json: bool) -> Result<()> {
    let path = format!("/v1/{collection}/{}/versions", encode_path_segment(urn));
    let body = c.get(&path).await?;
    if json || crate::output::is_json() {
        client::print_json(&body);
        return Ok(());
    }
    let parsed: serde_json::Value = serde_json::from_str(&body)?;
    println!(
        "{:<12} {:<10} {:<24} {:<26} NOTE",
        "VERSION", "CHANNEL", "RELEASED BY", "RELEASED AT"
    );
    for entry in parsed["versions"].as_array().into_iter().flatten() {
        let version = &entry["version"];
        let is_draft = version["prerelease"].as_str() == Some("draft");
        let label = if is_draft {
            "draft".to_string()
        } else {
            format!(
                "{}.{}.{}",
                version["major"].as_i64().unwrap_or(0),
                version["minor"].as_i64().unwrap_or(0),
                version["patch"].as_i64().unwrap_or(0),
            )
        };
        let channel = if is_draft { "draft" } else { "released" };
        let released_by = entry["releasedBy"].as_str().unwrap_or("-");
        let when = entry["releasedAt"]
            .as_str()
            .or_else(|| entry["updatedAt"].as_str())
            .unwrap_or("-");
        let note = entry["note"].as_str().unwrap_or("");
        println!("{label:<12} {channel:<10} {released_by:<24} {when:<26} {note}");
    }
    Ok(())
}

fn tar_gz_directory(dir: &Path) -> Result<Vec<u8>> {
    if !dir.is_dir() {
        anyhow::bail!("publish path must be a directory: {}", dir.display());
    }
    let mut tar_buf = Vec::new();
    {
        let mut builder = tar::Builder::new(&mut tar_buf);
        append_dir_to_tar(&mut builder, dir, dir)?;
        builder.finish().context("finish tar archive")?;
    }
    let mut gz = Vec::new();
    {
        let mut encoder = GzEncoder::new(&mut gz, Compression::default());
        copy(&mut Cursor::new(&tar_buf), &mut encoder).context("gzip tar archive")?;
        encoder.finish().context("finish gzip archive")?;
    }
    Ok(gz)
}

fn append_dir_to_tar(
    builder: &mut tar::Builder<&mut Vec<u8>>,
    base: &Path,
    current: &Path,
) -> Result<()> {
    for entry in fs::read_dir(current).with_context(|| format!("read dir {}", current.display()))? {
        let entry = entry?;
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .with_context(|| format!("strip prefix for {}", path.display()))?;
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if path.is_dir() {
            append_dir_to_tar(builder, base, &path)?;
        } else {
            let mut file =
                fs::File::open(&path).with_context(|| format!("open {}", path.display()))?;
            let mut header = tar::Header::new_gnu();
            let len = file.metadata()?.len();
            header.set_size(len);
            header.set_mode(0o644);
            header.set_cksum();
            builder
                .append_data(&mut header, rel_str, &mut file)
                .with_context(|| format!("append {}", path.display()))?;
        }
    }
    Ok(())
}

pub(super) fn parse_version_triplet(raw: &str) -> Result<vam_spec::v1::Version> {
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 3 {
        anyhow::bail!("version must be X.Y.Z, got '{raw}'");
    }
    Ok(vam_spec::v1::Version {
        major: parts[0].parse().context("invalid major version")?,
        minor: parts[1].parse().context("invalid minor version")?,
        patch: parts[2].parse().context("invalid patch version")?,
        prerelease: String::new(),
        build: String::new(),
    })
}

pub(super) fn load_json_proto<T: Message + Default>(path: &str, proto_name: &str) -> Result<T> {
    let raw = fs::read_to_string(path).with_context(|| format!("read {:?}", path))?;
    let val: serde_json::Value = serde_json::from_str(&raw)?;
    let spec_val = val.get("spec").cloned().unwrap_or(val);
    let json = serde_json::to_string(&spec_val)?;
    vam_spec::encoding::json_to_proto(proto_name, &json).map_err(|e| anyhow::anyhow!(e))
}

fn urlencoding(s: &str) -> String {
    s.replace('/', "%2F").replace(':', "%3A")
}

#[allow(dead_code)]
fn _unused_imports() {
    let _ = GetAgentRequest::default();
    let _ = GetPlaybookRequest::default();
    let _ = GetPolicyRequest::default();
    let _ = GetSkillRequest::default();
}

#[cfg(test)]
mod flag_tests {
    use super::*;
    use clap::Parser;

    fn key_of(value: &Option<String>) -> Option<&str> {
        value.as_deref()
    }

    #[test]
    fn playbook_local_commands_parse_their_flags() {
        #[derive(Parser)]
        struct PlaybookWrap {
            #[command(subcommand)]
            cmd: PlaybookCmd,
        }

        let init =
            PlaybookWrap::try_parse_from(["vam", "init", "--out", "x.json", "--force"]).unwrap();
        assert!(matches!(
            init.cmd,
            PlaybookCmd::Init { ref out, force: true } if out == "x.json"
        ));

        let default_init = PlaybookWrap::try_parse_from(["vam", "init"]).unwrap();
        assert!(matches!(
            default_init.cmd,
            PlaybookCmd::Init { ref out, force: false } if out == "playbook.json"
        ));

        let explain = PlaybookWrap::try_parse_from(["vam", "explain", "playbook.json"]).unwrap();
        assert!(matches!(
            explain.cmd,
            PlaybookCmd::Explain { ref path } if path == "playbook.json"
        ));

        let test = PlaybookWrap::try_parse_from([
            "vam",
            "test",
            "playbook.json",
            "--scenario",
            "scenario.json",
            "--repeat",
            "4",
        ])
        .unwrap();
        assert!(matches!(
            test.cmd,
            PlaybookCmd::Test { ref path, ref scenario, repeat: 4 }
                if path == "playbook.json" && scenario == "scenario.json"
        ));
        let default_repeat = PlaybookWrap::try_parse_from([
            "vam",
            "test",
            "playbook.json",
            "--scenario",
            "scenario.json",
        ])
        .unwrap();
        assert!(matches!(
            default_repeat.cmd,
            PlaybookCmd::Test { repeat: 1, .. }
        ));
    }

    #[test]
    fn mutating_catalog_commands_accept_idempotency_key() {
        #[derive(Parser)]
        struct AgentWrap {
            #[command(subcommand)]
            cmd: AgentCmd,
        }
        #[derive(Parser)]
        struct PlaybookWrap {
            #[command(subcommand)]
            cmd: PlaybookCmd,
        }
        #[derive(Parser)]
        struct PolicyWrap {
            #[command(subcommand)]
            cmd: PolicyCmd,
        }
        #[derive(Parser)]
        struct SkillWrap {
            #[command(subcommand)]
            cmd: SkillCmd,
        }

        let agent = AgentWrap::try_parse_from([
            "vam",
            "put",
            "agent.json",
            "--idempotency-key",
            "agent-put",
        ])
        .unwrap();
        assert!(matches!(
            agent.cmd,
            AgentCmd::Put { ref idempotency_key, .. } if key_of(idempotency_key) == Some("agent-put")
        ));
        let release = AgentWrap::try_parse_from([
            "vam",
            "release",
            "urn:vam:agent:ex/a",
            "--version",
            "1.2.3",
            "--idempotency-key",
            "agent-rel",
        ])
        .unwrap();
        assert!(matches!(
            release.cmd,
            AgentCmd::Release { ref idempotency_key, .. } if key_of(idempotency_key) == Some("agent-rel")
        ));

        let playbook = PlaybookWrap::try_parse_from([
            "vam",
            "put",
            "playbook.json",
            "--idempotency-key",
            "pb",
        ])
        .unwrap();
        assert!(matches!(
            playbook.cmd,
            PlaybookCmd::Put { ref idempotency_key, .. } if key_of(idempotency_key) == Some("pb")
        ));
        let playbook_rel = PlaybookWrap::try_parse_from([
            "vam",
            "release",
            "urn:vam:playbook:ex/p",
            "--version",
            "1.0.0",
            "--idempotency-key",
            "pb-rel",
        ])
        .unwrap();
        assert!(matches!(
            playbook_rel.cmd,
            PlaybookCmd::Release { ref idempotency_key, .. } if key_of(idempotency_key) == Some("pb-rel")
        ));

        let policy = PolicyWrap::try_parse_from([
            "vam",
            "put",
            "policy.cedar",
            "--urn",
            "urn:vam:policy:ex/p",
            "--idempotency-key",
            "pol",
        ])
        .unwrap();
        assert!(matches!(
            policy.cmd,
            PolicyCmd::Put { ref idempotency_key, .. } if key_of(idempotency_key) == Some("pol")
        ));
        let policy_rel = PolicyWrap::try_parse_from([
            "vam",
            "release",
            "urn:vam:policy:ex/p",
            "--version",
            "1.0.0",
            "--idempotency-key",
            "pol-rel",
        ])
        .unwrap();
        assert!(matches!(
            policy_rel.cmd,
            PolicyCmd::Release { ref idempotency_key, .. } if key_of(idempotency_key) == Some("pol-rel")
        ));

        let skill =
            SkillWrap::try_parse_from(["vam", "put", "manifest.json", "--idempotency-key", "sk"])
                .unwrap();
        assert!(matches!(
            skill.cmd,
            SkillCmd::Put { ref idempotency_key, .. } if key_of(idempotency_key) == Some("sk")
        ));
        let publish =
            SkillWrap::try_parse_from(["vam", "publish", "./skill", "--idempotency-key", "pub"])
                .unwrap();
        assert!(matches!(
            publish.cmd,
            SkillCmd::Publish { ref idempotency_key, .. } if key_of(idempotency_key) == Some("pub")
        ));
        let skill_rel = SkillWrap::try_parse_from([
            "vam",
            "release",
            "urn:vam:skill:ex/s",
            "--version",
            "1.0.0",
            "--idempotency-key",
            "sk-rel",
        ])
        .unwrap();
        assert!(matches!(
            skill_rel.cmd,
            SkillCmd::Release { ref idempotency_key, .. } if key_of(idempotency_key) == Some("sk-rel")
        ));
    }

    #[test]
    fn policy_explain_and_agent_permissions_parse() {
        use super::super::permissions::PermissionsCmd;

        #[derive(Parser)]
        struct AgentWrap {
            #[command(subcommand)]
            cmd: AgentCmd,
        }
        #[derive(Parser)]
        struct PolicyWrap {
            #[command(subcommand)]
            cmd: PolicyCmd,
        }

        let explain = PolicyWrap::try_parse_from([
            "vam",
            "explain",
            "jira-assistant",
            "jira_create_issue",
            "--version",
            "1.3.0",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            explain.cmd,
            PolicyCmd::Explain {
                ref agent,
                ref tool,
                host: None,
                ref version,
                json: true,
            } if agent == "jira-assistant"
                && tool.as_deref() == Some("jira_create_issue")
                && version.as_deref() == Some("1.3.0")
        ));

        let host = PolicyWrap::try_parse_from([
            "vam",
            "explain",
            "jira-assistant",
            "--host",
            "pastebin.com",
        ])
        .unwrap();
        assert!(matches!(
            host.cmd,
            PolicyCmd::Explain {
                ref host,
                tool: None,
                version: None,
                json: false,
                ..
            } if host.as_deref() == Some("pastebin.com")
        ));

        assert!(PolicyWrap::try_parse_from([
            "vam",
            "explain",
            "jira-assistant",
            "jira_create_issue",
            "--host",
            "pastebin.com",
        ])
        .is_err());

        let get = AgentWrap::try_parse_from([
            "vam",
            "permissions",
            "get",
            "jira-assistant",
            "--effective",
            "--version",
            "1.2.3",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            get.cmd,
            AgentCmd::Permissions {
                command: PermissionsCmd::Get {
                    ref agent,
                    ref version,
                    effective: true,
                    json: true,
                }
            } if agent == "jira-assistant" && version.as_deref() == Some("1.2.3")
        ));

        let set = AgentWrap::try_parse_from([
            "vam",
            "permissions",
            "set",
            "jira-assistant",
            "-f",
            "-",
            "--idempotency-key",
            "perm-set",
        ])
        .unwrap();
        assert!(matches!(
            set.cmd,
            AgentCmd::Permissions {
                command: PermissionsCmd::Set {
                    ref file,
                    ref idempotency_key,
                    ..
                }
            } if file == "-" && key_of(idempotency_key) == Some("perm-set")
        ));

        let presets = AgentWrap::try_parse_from(["vam", "permissions", "presets"]).unwrap();
        assert!(matches!(
            presets.cmd,
            AgentCmd::Permissions {
                command: PermissionsCmd::Presets { json: false }
            }
        ));

        let migrate =
            AgentWrap::try_parse_from(["vam", "migrate", "jira-assistant", "--force", "--json"])
                .unwrap();
        assert!(matches!(
            migrate.cmd,
            AgentCmd::Migrate {
                ref agent,
                force: true,
                json: true,
                ..
            } if agent == "jira-assistant"
        ));
    }
}

#[cfg(test)]
mod policy_host_tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Probe {
        #[arg(long, global = true)]
        host: Option<String>,
        #[command(subcommand)]
        command: PolicyCmd,
    }

    #[test]
    fn explain_host_is_the_site_not_the_server() {
        let probe = Probe::parse_from(["vam", "explain", "p5-new", "--host", "paste.pastebin.com"]);
        // clap copies the value to the global flag too.
        assert_eq!(probe.host.as_deref(), Some("paste.pastebin.com"));
        assert_eq!(policy_server_host(probe.host, &probe.command), None);
    }

    #[test]
    fn other_policy_commands_keep_the_server_host() {
        let probe = Probe::parse_from(["vam", "--host", "http://127.0.0.1:8080", "ls"]);
        assert_eq!(
            policy_server_host(probe.host, &probe.command).as_deref(),
            Some("http://127.0.0.1:8080")
        );
    }
}
