use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;
use vam_telemetry::{verify_evidence_pack_report, write_evidence_dir, EvidencePackResponse};

use crate::client::{self, Client};
use crate::config;

#[derive(Subcommand)]
pub enum EvidenceCmd {
    /// Export a finished run's evidence pack from a live vamd
    Export {
        id: String,
        #[arg(long)]
        out: PathBuf,
    },
    /// Verify an exported evidence directory offline (no daemon)
    Verify { dir: PathBuf },
    /// Alias of `vam receipt show` (removed after 0.1.0)
    Receipt { id: String },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: EvidenceCmd) -> Result<()> {
    match cmd {
        EvidenceCmd::Export { id, out } => {
            let cfg = config::resolve(host, token).await?;
            let c = Client::new(&cfg)?;
            export_to_dir(&c, &id, &out).await?;
        }
        EvidenceCmd::Verify { dir } => {
            verify_dir(&dir)?;
        }
        EvidenceCmd::Receipt { id } => {
            let cfg = config::resolve(host, token).await?;
            let c = Client::new(&cfg)?;
            crate::cmd::receipt::show_via_evidence_alias(&c, &id).await?;
        }
    }
    Ok(())
}

pub async fn export_to_dir(c: &Client, run_id: &str, out: &std::path::Path) -> Result<()> {
    let id = client::normalize_resource_id(run_id)?;
    let body = c
        .get(&format!(
            "/v1/runs/{}/evidence",
            client::encode_path_segment(&id)
        ))
        .await
        .with_context(|| format!("GET /v1/runs/{{id}}/evidence for {id}"))?;
    let pack: EvidencePackResponse =
        serde_json::from_str(&body).context("parse evidence pack JSON")?;
    write_evidence_dir(out, &pack.files).map_err(|e| anyhow::anyhow!(e))?;
    eprintln!(
        "[vam] wrote evidence pack for {} to {}",
        pack.run_id,
        out.display()
    );
    if crate::output::is_json() {
        println!(
            "{}",
            serde_json::json!({"runId": pack.run_id, "dir": out.display().to_string()})
        );
    }
    Ok(())
}

pub fn verify_dir(dir: &std::path::Path) -> Result<()> {
    let report = verify_evidence_pack_report(dir)
        .map_err(|e| crate::output::fail(crate::output::ExitCode::VerifyFailed, e.to_string()))?;
    for note in &report.notes {
        eprintln!("[vam] {note}");
    }
    eprintln!("[vam] evidence pack verified: {}", dir.display());
    if crate::output::is_json() {
        println!(
            "{}",
            serde_json::json!({"verified": true, "dir": dir.display().to_string()})
        );
    }
    Ok(())
}
