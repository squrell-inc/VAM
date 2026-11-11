use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde::Deserialize;
use serde_json::json;

use crate::client::{self, Client};
use crate::config;
use crate::output::{self, ExitCode};

const MAX_UPLOAD_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Subcommand)]
pub enum WorkspaceCmd {
    /// List committed Drive9 HEAD files under workspace/
    Ls {
        /// Space id (`sp_…`); see `vam space ls`
        #[arg(long = "space", alias = "workspace")]
        workspace: String,
    },
    /// Download a workspace file (raw bytes to stdout or -o)
    Cat {
        /// Manifest path (`workspace/...` or shorthand `notes/a.md`)
        path: String,
        /// Space id (`sp_…`); see `vam space ls`
        #[arg(long = "space", alias = "workspace")]
        workspace: String,
        /// Write bytes to this file instead of stdout
        #[arg(short = 'o', long = "out")]
        out: Option<PathBuf>,
    },
    /// Upload a local file into committed Drive9 HEAD under workspace/
    Put {
        /// Manifest path (`workspace/...` or shorthand `notes/a.md`)
        path: String,
        /// Local file to upload
        file: PathBuf,
        /// Space id (`sp_…`); see `vam space ls`
        #[arg(long = "space", alias = "workspace")]
        workspace: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse {
    files: Vec<FileEntry>,
    truncated: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FileEntry {
    path: String,
    size_bytes: u64,
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: WorkspaceCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        WorkspaceCmd::Ls { workspace } => ls(&c, &workspace).await,
        WorkspaceCmd::Cat {
            path,
            workspace,
            out,
        } => cat(&c, &workspace, &path, out.as_deref()).await,
        WorkspaceCmd::Put {
            path,
            file,
            workspace,
        } => put(&c, &workspace, &path, &file).await,
    }
}

async fn ls(c: &Client, workspace: &str) -> Result<()> {
    super::space::require_space_id(workspace)?;
    let id = client::encode_path_segment(workspace.trim());
    let api_path = format!("/v1/workspaces/{id}/files");
    let body = c
        .get(&api_path)
        .await
        .with_context(|| format!("GET {api_path}"))?;

    if output::is_json() {
        client::print_json(&body);
        return Ok(());
    }

    let listing: ListResponse =
        serde_json::from_str(&body).context("parse workspace files list JSON")?;
    if listing.truncated {
        eprintln!("[vam] warning: listing truncated (max 10,000 files)");
    }
    for file in listing.files {
        println!("{}\t{}", file.path, file.size_bytes);
    }
    Ok(())
}

async fn cat(c: &Client, workspace: &str, path: &str, out: Option<&std::path::Path>) -> Result<()> {
    super::space::require_space_id(workspace)?;
    let normalized = normalize_cli_workspace_path(path)?;
    if output::is_json() && out.is_none() {
        return Err(output::fail(
            ExitCode::Usage,
            "Wrong: --json requires -o/--out for workspace cat.\nExpected: a file path for the download.\nFix: vam workspace cat <PATH> -o <FILE> --json.",
        ));
    }

    let id = client::encode_path_segment(workspace.trim());
    let encoded_path = encode_workspace_file_path(&normalized);
    let api_path = format!("/v1/workspaces/{id}/files/{encoded_path}");
    let bytes = c
        .get_bytes(&api_path)
        .await
        .with_context(|| format!("GET {api_path}"))?;

    if let Some(out_path) = out {
        if let Some(parent) = out_path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("create parent dirs for {}", out_path.display()))?;
            }
        }
        std::fs::write(out_path, &bytes)
            .with_context(|| format!("write {}", out_path.display()))?;
        if output::is_json() {
            println!(
                "{}",
                json!({
                    "workspaceId": workspace,
                    "path": normalized,
                    "sizeBytes": bytes.len() as u64,
                    "out": out_path.display().to_string(),
                })
            );
        } else {
            eprintln!(
                "[vam] wrote {} bytes to {}",
                bytes.len(),
                out_path.display()
            );
        }
    } else {
        let mut stdout = std::io::stdout().lock();
        stdout
            .write_all(&bytes)
            .context("write workspace file bytes to stdout")?;
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutResponse {
    workspace_id: String,
    path: String,
    head: String,
    size_bytes: u64,
}

async fn put(c: &Client, workspace: &str, path: &str, file: &std::path::Path) -> Result<()> {
    super::space::require_space_id(workspace)?;
    let normalized = normalize_cli_workspace_path(path)?;
    let bytes =
        std::fs::read(file).with_context(|| format!("read local file {}", file.display()))?;
    if bytes.len() as u64 > MAX_UPLOAD_BYTES {
        return Err(output::fail(
            ExitCode::Usage,
            format!(
                "Wrong: local file is {} bytes.\nExpected: at most {} bytes (32 MiB).\nFix: vam workspace put {} <SMALLER-FILE> -w {workspace}.",
                bytes.len(),
                MAX_UPLOAD_BYTES,
                normalized
            ),
        ));
    }

    let id = client::encode_path_segment(workspace.trim());
    let encoded_path = encode_workspace_file_path(&normalized);
    let api_path = format!("/v1/workspaces/{id}/files/{encoded_path}");
    let body = c
        .put_bytes(&api_path, &bytes)
        .await
        .with_context(|| format!("PUT {api_path}"))?;

    if output::is_json() {
        client::print_json(&body);
        return Ok(());
    }

    let resp: PutResponse =
        serde_json::from_str(&body).context("parse workspace file upload JSON")?;
    eprintln!(
        "[vam] uploaded {} ({} bytes) to workspace {} (head {})",
        resp.path, resp.size_bytes, resp.workspace_id, resp.head
    );
    Ok(())
}

/// Prepend `workspace/` when missing; reject empty, absolute, or `..` paths.
pub(crate) fn normalize_cli_workspace_path(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(output::fail(
            ExitCode::Usage,
            "Wrong: path is empty.\nExpected: a relative workspace path.\nFix: vam workspace cat notes/a.md.",
        ));
    }
    if trimmed.starts_with('/') {
        return Err(output::fail(
            ExitCode::Usage,
            "Wrong: absolute paths are not allowed.\nExpected: a relative path under workspace/.\nFix: vam workspace cat notes/a.md.",
        ));
    }
    if trimmed.split('/').any(|seg| seg == "..") {
        return Err(output::fail(
            ExitCode::Usage,
            "Wrong: path contains '..'.\nExpected: a relative path without traversal.\nFix: vam workspace cat notes/a.md.",
        ));
    }
    if trimmed == "workspace" || trimmed.starts_with("workspace/") {
        Ok(trimmed.to_string())
    } else {
        Ok(format!("workspace/{trimmed}"))
    }
}

fn encode_workspace_file_path(path: &str) -> String {
    path.split('/')
        .map(client::encode_path_segment)
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_prepends_workspace_prefix() {
        assert_eq!(
            normalize_cli_workspace_path("notes/a.md").unwrap(),
            "workspace/notes/a.md"
        );
        assert_eq!(
            normalize_cli_workspace_path("workspace/notes/a.md").unwrap(),
            "workspace/notes/a.md"
        );
        assert_eq!(
            normalize_cli_workspace_path("workspace").unwrap(),
            "workspace"
        );
    }

    #[test]
    fn normalize_rejects_empty_absolute_and_dotdot() {
        assert_eq!(
            output::classify(&normalize_cli_workspace_path("").unwrap_err()).0,
            ExitCode::Usage
        );
        assert_eq!(
            output::classify(&normalize_cli_workspace_path("/etc/passwd").unwrap_err()).0,
            ExitCode::Usage
        );
        assert_eq!(
            output::classify(&normalize_cli_workspace_path("a/../b").unwrap_err()).0,
            ExitCode::Usage
        );
    }

    #[test]
    fn encode_keeps_slashes_encodes_segments() {
        assert_eq!(
            encode_workspace_file_path("workspace/notes/a md.md"),
            "workspace/notes/a%20md.md"
        );
    }
}
