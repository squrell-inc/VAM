//! `vam fix request <decisionRef>`: ask the person who can apply the fix a refusal named. The
//! reference comes from the `Ask:` line the refused command printed; the server resolves it,
//! checks it is yours, and works out who to tell. Nothing about the fix is sent from here.

use anyhow::{Context, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::{self, Client};
use crate::config;
use crate::output::{self, ExitCode};

#[derive(Subcommand)]
pub enum FixCmd {
    /// Ask the right person to apply the fix a refusal named
    Request {
        /// The reference from the `Ask:` line of the refusal (`dnl_...`)
        reference: String,
    },
}

/// `dnl_` plus 26 base32 characters (a-z, 2-7), the shape of a stored decision's id.
fn is_decision_ref(value: &str) -> bool {
    value.strip_prefix("dnl_").is_some_and(|rest| {
        rest.len() == 26
            && rest
                .bytes()
                .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b))
    })
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: FixCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        FixCmd::Request { reference } => request(&c, &reference).await,
    }
}

async fn request(c: &Client, reference: &str) -> Result<()> {
    let reference = reference.trim();
    if !is_decision_ref(reference) {
        return Err(output::fail(
            ExitCode::Usage,
            "Wrong: that is not a decision reference.\nExpected: dnl_ followed by 26 letters and digits.\nFix: copy it from the Ask: line of the refusal, as the same user.".to_string(),
        ));
    }
    let body = json!({ "decisionRef": reference }).to_string();
    // A fresh key per command; the server also answers a repeat within 24 hours with the first
    // request, so asking twice never tells anyone twice.
    let key = client::new_idempotency_key();
    let resp = c
        .post_raw_json("/v1/fix-requests", &body, &key)
        .await
        .context("POST /v1/fix-requests")?;
    if output::is_json() {
        client::print_json(&resp);
        return Ok(());
    }
    let v: Value = serde_json::from_str(&resp).context("parse fix request JSON")?;
    println!("{}", render(&v));
    Ok(())
}

/// What the person is told: the request id and who was asked. Both come from the host.
fn render(v: &Value) -> String {
    let text = |k: &str| {
        v[k].as_str()
            .unwrap_or("")
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
    };
    let asked = match text("asked") {
        a if a.is_empty() => "the right person".to_string(),
        a => a,
    };
    let mut out = format!("Asked {asked}. Request {}.", text("id"));
    if v["existing"].as_bool() == Some(true) {
        out.push_str("\nYou already asked about this today; nobody was notified again.");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reference_has_the_shape_of_a_stored_id() {
        assert!(is_decision_ref("dnl_k3q7xw2m5b4n6a7c3d2e3f4g5h"));
        assert!(!is_decision_ref("dnl_short"));
        assert!(!is_decision_ref("sp_k3q7xw2m5b4n6a7c3d2e3f4g5h"));
        assert!(!is_decision_ref("dnl_K3Q7XW2M5B4N6A7C3D2E3F4G5H"));
    }

    #[test]
    fn the_answer_names_the_request_and_who_was_asked() {
        let first = json!({"id": "frq_abc", "asked": "an org admin", "existing": false});
        assert_eq!(render(&first), "Asked an org admin. Request frq_abc.");
        let again = json!({"id": "frq_abc", "asked": "an org admin", "existing": true});
        assert!(render(&again).contains("nobody was notified again"));
        let odd = json!({"id": "frq_a\u{1b}[2J", "asked": ""});
        assert_eq!(render(&odd), "Asked the right person. Request frq_a[2J.");
    }
}
