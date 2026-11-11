//! `vam receipt show` and `vam receipt verify`.
//!
//! `show` prints the run's stored, signed receipt. The markdown form is a pure
//! function of that document: every line comes from a field the document has,
//! a missing field is a missing line, and nothing is computed or inferred.
//! `verify` is offline: it reads a file and checks the signature, and never
//! reads the host config or talks to a daemon.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::Value;
use vam_security::receipt::{trusted_keys_from_json, verify_receipt};

use crate::client::{self, Client};
use crate::config;
use crate::output::{self, ExitCode};

#[derive(Subcommand)]
pub enum ReceiptCmd {
    /// Print a finished run's receipt (markdown, or the signed document with --json)
    Show { id: String },
    /// Verify a receipt file offline: signature, model appendix digest, key id
    Verify {
        file: PathBuf,
        /// Keys from `vam node keys`. With it, a receipt signed by any other key fails.
        #[arg(long)]
        trusted_keys: Option<PathBuf>,
    },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: ReceiptCmd) -> Result<()> {
    match cmd {
        ReceiptCmd::Show { id } => {
            let cfg = config::resolve(host, token).await?;
            let c = Client::new(&cfg)?;
            show(&c, &id).await
        }
        ReceiptCmd::Verify { file, trusted_keys } => verify(&file, trusted_keys.as_deref()),
    }
}

/// `vam evidence receipt <id>`: the same output as `show`, with a notice.
pub async fn show_via_evidence_alias(c: &Client, id: &str) -> Result<()> {
    eprintln!(
        "[vam] `vam evidence receipt` is now `vam receipt show` (alias, removed after 0.1.0)"
    );
    show(c, id).await
}

pub async fn show(c: &Client, run_id: &str) -> Result<()> {
    let id = client::normalize_resource_id(run_id)?;
    let body = c
        .get(&format!(
            "/v1/runs/{}/receipt",
            client::encode_path_segment(&id)
        ))
        .await
        .with_context(|| format!("GET /v1/runs/{{id}}/receipt for {id}"))?;
    if output::is_json() {
        println!("{body}");
        return Ok(());
    }
    let doc: Value = serde_json::from_str(&body).context("parse receipt JSON")?;
    print!("{}", render_markdown(&doc));
    Ok(())
}

pub fn verify(file: &std::path::Path, trusted_keys: Option<&std::path::Path>) -> Result<()> {
    let bytes = std::fs::read(file).map_err(|e| {
        output::fail(
            ExitCode::Usage,
            format!("cannot read {}: {e}", file.display()),
        )
    })?;
    let trusted = match trusted_keys {
        Some(path) => {
            let keys = std::fs::read(path).map_err(|e| {
                output::fail(
                    ExitCode::Usage,
                    format!("cannot read {}: {e}", path.display()),
                )
            })?;
            trusted_keys_from_json(&keys)
                .map_err(|e| output::fail(ExitCode::Usage, format!("trusted keys: {e}")))?
        }
        None => Vec::new(),
    };
    let verified = verify_receipt(&bytes, &trusted)
        .map_err(|e| output::fail(ExitCode::VerifyFailed, e.to_string()))?;
    eprintln!("[vam] receipt verified: key {}", verified.key_id);
    if !verified.pinned {
        eprintln!(
            "[vam] key not pinned: this proves the receipt is unchanged since the key signed it, \
             not that the key is the node's (use --trusted-keys <file from `vam node keys`>)"
        );
    }
    if output::is_json() {
        println!(
            "{}",
            serde_json::json!({
                "verified": true,
                "keyId": verified.key_id,
                "publicKey": verified.public_key,
                "pinned": verified.pinned,
            })
        );
    }
    Ok(())
}

// ---- markdown -----------------------------------------------------------------------------

fn text(v: &Value) -> Option<String> {
    match v {
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// The journaled effects section: the two headline numbers, the counts, one line per effect,
/// then what the journal cannot see. Nothing is computed here; every number is as stored.
fn render_effects(out: &mut String, e: &Value) {
    if e.get("journaled").and_then(Value::as_bool) != Some(true) {
        return;
    }
    field(out, "Duplicates", get(e, "duplicates"));
    field(out, "Unknown", get(e, "unknown"));
    if let Some(c) = e.get("counts") {
        let parts: Vec<String> = [
            ("confirmed", "confirmed"),
            ("reconciled", "reconciled"),
            ("failed", "failed"),
            ("not sent", "notSent"),
            ("identical repeats", "identicalRepeats"),
            ("reads", "reads"),
        ]
        .iter()
        .filter_map(|(label, key)| get(c, key).map(|v| format!("{label} {v}")))
        .collect();
        if !parts.is_empty() {
            out.push_str(&format!("- **Counts:** {}\n", parts.join(", ")));
        }
    }
    let lines = e.get("lines").and_then(Value::as_array);
    if lines.is_some_and(|l| !l.is_empty()) {
        out.push('\n');
    }
    for l in lines.into_iter().flatten() {
        let mut line = format!(
            "- #{} {} ({}, {}): {}",
            get(l, "seq").unwrap_or_default(),
            get(l, "tool").unwrap_or_default(),
            get(l, "adapter").unwrap_or_default(),
            get(l, "class").unwrap_or_default(),
            get(l, "state").unwrap_or_default(),
        );
        for (label, key) in [
            ("attempts", "attempts"),
            ("checked by", "checkedBy"),
            ("repeat of #", "repeatOf"),
            ("parent #", "parentSeq"),
            ("key", "keyDigest"),
            ("args", "argsDigest"),
            ("result", "resultDigest"),
            ("at", "at"),
        ] {
            if let Some(v) = get(l, key) {
                let sep = if label.ends_with('#') { "" } else { " " };
                line.push_str(&format!(", {label}{sep}{v}"));
            }
        }
        if l.get("hostDerivedId").and_then(Value::as_bool) == Some(true) {
            line.push_str(", call id derived by the host");
        }
        out.push_str(&format!("{line}\n"));
    }
    if let Some(scope) = get(e, "scope") {
        out.push_str(&format!("\n_{scope}_\n"));
    }
}

fn get(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(text)
}

/// `- **label:** value`, only when the field exists.
fn field(out: &mut String, label: &str, value: Option<String>) {
    if let Some(value) = value {
        out.push_str(&format!("- **{label}:** {value}\n"));
    }
}

/// "<user>, surface <s> — <ref> via grant <id> (<broker>), fire time <RFC 3339>"; a missing part is left out.
fn trigger_line(r: &Value, t: &Value) -> String {
    let mut line = String::new();
    if let Some(user) = get(r, "requestedBy") {
        line.push_str(&user);
    }
    if let Some(surface) = get(r, "requestSurface") {
        if !line.is_empty() {
            line.push_str(", ");
        }
        line.push_str(&format!("surface {surface}"));
    }
    let mut what = String::new();
    if let Some(tr) = get(t, "triggerRef") {
        what.push_str(&tr);
    }
    if let Some(grant) = get(t, "grantId") {
        if !what.is_empty() {
            what.push(' ');
        }
        what.push_str(&format!("via grant {grant}"));
    }
    if let Some(broker) = get(t, "brokerId") {
        if !what.is_empty() {
            what.push(' ');
        }
        what.push_str(&format!("({broker})"));
    }
    if let Some(ms) = t.get("fireAtMs").and_then(Value::as_i64) {
        if !what.is_empty() {
            what.push_str(", ");
        }
        what.push_str(&format!("fire time {}", rfc3339(ms / 1000)));
    }
    if !what.is_empty() {
        if !line.is_empty() {
            line.push_str(" — ");
        }
        line.push_str(&what);
    }
    line
}

/// Unix seconds as `YYYY-MM-DDTHH:MM:SSZ` (civil-from-days).
fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn pin_line(p: &Value) -> String {
    let mut parts = Vec::new();
    for key in ["urn", "version", "digest"] {
        if let Some(s) = get(p, key) {
            parts.push(s);
        }
    }
    parts.join(" ")
}

/// `base v0.1.0`, then `(overrides: loop.detect=true)` when the run changed any switch.
fn harness_line(h: &Value) -> String {
    let mut line = get(h, "profile").unwrap_or_default();
    if let Some(v) = get(h, "version") {
        line.push_str(&format!(" v{v}"));
    }
    let mut leaves = Vec::new();
    if let Some(o) = h.get("overrides") {
        flatten_overrides(o, "", &mut leaves);
    }
    if !leaves.is_empty() {
        line.push_str(&format!(" (overrides: {})", leaves.join(", ")));
    }
    line
}

fn flatten_overrides(v: &Value, prefix: &str, out: &mut Vec<String>) {
    match v {
        Value::Object(map) => {
            for (k, child) in map {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_overrides(child, &path, out);
            }
        }
        other => out.push(format!("{prefix}={other}")),
    }
}

/// The receipt document as markdown. The signed host facts come first; the
/// model appendix is last, under its label, and is the only place model text
/// appears.
pub fn render_markdown(doc: &Value) -> String {
    let empty = Value::Null;
    let r = doc.get("receipt").unwrap_or(&empty);
    let mut out = String::from("# Run receipt\n\n");
    out.push_str(
        "_Host-observed facts, signed by the node. Verify with `vam receipt verify`._\n\n",
    );
    field(&mut out, "Run", get(r, "runId"));
    field(&mut out, "State", get(r, "state"));
    if let Some(t) = r.get("requestTrigger").filter(|t| t.is_object()) {
        field(&mut out, "Requested by", Some(trigger_line(r, t)));
    } else {
        field(&mut out, "Requested by", get(r, "requestedBy"));
        field(&mut out, "Request surface", get(r, "requestSurface"));
    }
    field(&mut out, "Started (unix s)", get(r, "startedAt"));
    field(&mut out, "Ended (unix s)", get(r, "endedAt"));

    if let Some(pins) = r.get("pins") {
        out.push_str("\n## Pins\n\n");
        for (label, key) in [("Agent", "agent"), ("Playbook", "playbook")] {
            if let Some(p) = pins.get(key).filter(|p| p.is_object()) {
                field(&mut out, label, Some(pin_line(p)));
            }
        }
        for p in pins
            .get("other")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            field(&mut out, "Pinned", Some(pin_line(p)));
        }
    }

    if let Some(h) = r.get("harness").filter(|h| h.is_object()) {
        out.push_str("\n## Harness\n\n");
        field(&mut out, "Harness", Some(harness_line(h)));
        field(&mut out, "Harness digest", get(h, "digest"));
    }

    if let Some(models) = r.get("model").and_then(Value::as_array) {
        if !models.is_empty() {
            out.push_str("\n## Model and endpoint\n\n");
            for m in models {
                let mut line = get(m, "name").unwrap_or_default();
                for (label, key) in [
                    ("provider", "provider"),
                    ("endpoint", "endpointHost"),
                    ("calls", "calls"),
                    ("prompt tokens", "promptTokens"),
                    ("completion tokens", "completionTokens"),
                ] {
                    if let Some(s) = get(m, key) {
                        line.push_str(&format!(", {label} {s}"));
                    }
                }
                out.push_str(&format!("- {line}\n"));
            }
        }
    }

    if let Some(iso) = r.get("isolation") {
        out.push_str("\n## Isolation\n\n");
        // As stored.
        if let Some(summary) = get(iso, "summary") {
            out.push_str(&format!("{summary}\n"));
        }
        let has_generations = iso
            .get("generations")
            .and_then(Value::as_array)
            .is_some_and(|g| !g.is_empty());
        let has_image = r.get("image").is_some();
        if has_generations || has_image {
            out.push('\n');
        }
        for g in iso
            .get("generations")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            out.push_str(&format!(
                "- generation {}: {}\n",
                get(g, "generation").unwrap_or_default(),
                get(g, "summary").unwrap_or_default()
            ));
        }
        if let Some(image) = r.get("image") {
            field(&mut out, "Kernel digest", get(image, "kernelDigest"));
            field(&mut out, "Rootfs digest", get(image, "rootfsDigest"));
            if let Some(s) = get(image, "statement") {
                out.push_str(&format!("{s}\n"));
            }
        }
    }

    if let Some(ws) = r.get("workspace") {
        if get(ws, "headBefore").is_some() || get(ws, "headAfter").is_some() {
            out.push_str("\n## Workspace\n\n");
            field(&mut out, "HEAD before", get(ws, "headBefore"));
            field(&mut out, "HEAD after", get(ws, "headAfter"));
        }
    }

    if let Some(tc) = r.get("toolCalls") {
        out.push_str("\n## Tool calls\n\n");
        if let Some(c) = tc.get("counts") {
            out.push_str(&format!(
                "allowed {}, asked {}, denied {}\n\n",
                get(c, "allowed").unwrap_or_default(),
                get(c, "asked").unwrap_or_default(),
                get(c, "denied").unwrap_or_default()
            ));
        }
        if let Some(s) = get(tc, "statement") {
            out.push_str(&format!("{s}\n\n"));
        }
        for l in tc
            .get("lines")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let mut line = format!(
                "- #{} {} {}",
                get(l, "seq").unwrap_or_default(),
                get(l, "tool").unwrap_or_default(),
                get(l, "decision").unwrap_or_default()
            );
            for (label, key) in [
                ("by", "decidedBy"),
                ("scope", "scope"),
                ("rule", "rule"),
                ("constraint", "constraint"),
                ("gate step", "gateStep"),
                ("resolution", "resolution"),
                ("approved by", "approvedBy"),
                ("surface", "surface"),
                ("approved at", "approvedAt"),
                ("args", "argsDigest"),
            ] {
                if let Some(s) = get(l, key) {
                    line.push_str(&format!(", {label} {s}"));
                }
            }
            out.push_str(&format!("{line}\n"));
        }
    }

    if let Some(steps) = r
        .pointer("/procedure/steps")
        .and_then(Value::as_array)
        .filter(|s| !s.is_empty())
    {
        out.push_str("\n## Procedure\n\n");
        for s in steps {
            let mut line = format!(
                "- {}: {}",
                get(s, "id").unwrap_or_default(),
                get(s, "state").unwrap_or_default()
            );
            if let Some(o) = get(s, "outcome") {
                line.push_str(&format!(", outcome {o}"));
            }
            out.push_str(&format!("{line}\n"));
            if let Some(g) = s.get("gate") {
                let mut gl = String::from("  - gate");
                for (label, key) in [
                    ("decision", "decision"),
                    ("decided by", "decidedBy"),
                    ("surface", "surface"),
                    ("at", "decidedAt"),
                ] {
                    if let Some(v) = get(g, key) {
                        gl.push_str(&format!(", {label} {v}"));
                    }
                }
                if let Some(resp) = g.get("response") {
                    gl.push_str(&format!(", response {resp}"));
                }
                out.push_str(&format!("{gl}\n"));
            }
        }
    }

    if let Some(m) = r.get("metering") {
        out.push_str("\n## Metering\n\n");
        field(&mut out, "Prompt tokens", get(m, "promptTokens"));
        field(&mut out, "Completion tokens", get(m, "completionTokens"));
        field(&mut out, "Approvals", get(m, "approvals"));
        field(&mut out, "Turns", get(m, "turns"));
    }

    if let Some(e) = r.get("effects") {
        out.push_str("\n## Effects\n\n");
        if let Some(s) = get(e, "statement") {
            out.push_str(&format!("{s}\n"));
        }
        render_effects(&mut out, e);
    }

    out.push_str("\n## Signature\n\n");
    field(&mut out, "Trajectory digest", get(r, "trajectoryDigest"));
    field(
        &mut out,
        "Model appendix digest",
        get(r, "modelAppendixDigest"),
    );
    if let Some(sig) = doc.get("signature") {
        field(&mut out, "Algorithm", get(sig, "algorithm"));
        field(&mut out, "Key id", get(sig, "keyId"));
        field(&mut out, "Receipt digest", get(sig, "receiptDigest"));
    }

    if let Some(a) = doc.get("modelAppendix") {
        out.push_str("\n---\n\n## Model appendix\n\n");
        out.push_str(
            "_Written by the model, not verified by the host. Bound to the receipt by digest only._\n\n",
        );
        if let Some(reply) = get(a, "finalReply") {
            out.push_str(&format!("**Final reply:** {reply}\n\n"));
        }
        for s in a
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            out.push_str(&format!(
                "- {}: {}\n",
                get(s, "id").unwrap_or_default(),
                get(s, "summary").unwrap_or_default()
            ));
        }
    }
    out
}
