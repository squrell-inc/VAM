use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::{self, Client};
use crate::config;

#[derive(Subcommand)]
pub enum ConnectorsCmd {
    /// GET /v1/connectors: each connector with the node state and your own state.
    List,
    /// PUT /v1/connectors/<id>/node enabled (org admin). `--tenant` records the Microsoft admin
    /// consent for a connector that needs one (a tenant GUID).
    Enable {
        id: String,
        /// The consented Microsoft tenant id (a GUID).
        #[arg(long)]
        tenant: Option<String>,
    },
    /// PUT /v1/connectors/<id>/node disabled (org admin). Consent and members' connections stay.
    Disable { id: String },
    /// DELETE /v1/connectors/<id>/connection: remove your own Microsoft 365 connection (its
    /// tokens and account). Reconnecting is done in the app.
    Disconnect { id: String },
    /// POST /v1/connectors/<id>/connection/check: one refresh at the consented tenant. Prints
    /// `connected`, or `needsReconnect` when Microsoft refused the saved grant.
    Check { id: String },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: ConnectorsCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        ConnectorsCmd::List => {
            let body = c.get("/v1/connectors").await?;
            if !crate::output::is_json() {
                for line in list_lines(&body) {
                    println!("{line}");
                }
            }
            client::print_json(&body);
        }
        ConnectorsCmd::Enable { id, tenant } => {
            let (path, body) = node_request(&id, true, tenant.as_deref())?;
            let key = client::resolve_idempotency_key(None);
            let out = c.put_raw_json(&path, &body.to_string(), Some(&key)).await?;
            client::print_json(&out);
        }
        ConnectorsCmd::Disable { id } => {
            let (path, body) = node_request(&id, false, None)?;
            let key = client::resolve_idempotency_key(None);
            let out = c.put_raw_json(&path, &body.to_string(), Some(&key)).await?;
            client::print_json(&out);
        }
        ConnectorsCmd::Disconnect { id } => {
            let path = connection_path(&id, false)?;
            let key = client::resolve_idempotency_key(None);
            let out = c.delete(&path, Some(&key)).await?;
            print_state_then_json(&out);
        }
        ConnectorsCmd::Check { id } => {
            let path = connection_path(&id, true)?;
            let key = client::resolve_idempotency_key(None);
            let out = c.post_raw_json(&path, "{}", &key).await?;
            print_state_then_json(&out);
        }
    }
    Ok(())
}

fn print_state_then_json(body: &str) {
    if !crate::output::is_json() {
        if let Some(line) = state_line(body) {
            println!("{line}");
        }
    }
    client::print_json(body);
}

/// `/v1/connectors/{id}/connection` or `.../connection/check`. The same id rule as the node
/// route; `vamd` refuses an MCP connector (use `vam mcp disconnect`).
pub fn connection_path(id: &str, check: bool) -> Result<String> {
    let (base, _) = node_request(id, true, None)?;
    let base = base.trim_end_matches("/node");
    Ok(if check {
        format!("{base}/connection/check")
    } else {
        format!("{base}/connection")
    })
}

/// `microsoft-365: connected (member@contoso.com)` from one connector entry the connection
/// routes answer with.
pub fn state_line(body: &str) -> Option<String> {
    let v: Value = serde_json::from_str(body).ok()?;
    let s = |k: &str| v[k].as_str().map(str::to_string);
    Some(format!("{}: {} ({})", s("id")?, s("state")?, s("label")?))
}

/// The path and body of `PUT /v1/connectors/{id}/node`. The id is one path segment of
/// `[a-z0-9-]`; `vamd` checks the tenant, so a wrong one prints its refusal.
pub fn node_request(id: &str, enabled: bool, tenant: Option<&str>) -> Result<(String, Value)> {
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(anyhow!("connector id `{id}` is not a connector id"));
    }
    let mut body = json!({ "enabled": enabled });
    if let Some(t) = tenant {
        body["consent"] = json!({ "tenantId": t });
    }
    Ok((format!("/v1/connectors/{id}/node"), body))
}

/// One line per connector of a `GET /v1/connectors` body: id, name, node state, my state, label.
/// Empty for a body that is not that answer.
pub fn list_lines(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(list) = v["connectors"].as_array() else {
        return Vec::new();
    };
    list.iter()
        .map(|e| {
            let s = |k: &str| e[k].as_str().unwrap_or("-").to_string();
            let node = if e["node"]["enabled"].as_bool().unwrap_or(false) {
                match e["node"]["consent"]["tenantId"].as_str() {
                    Some(t) => format!("enabled (consent {t})"),
                    None => "enabled".to_string(),
                }
            } else if e["node"]["configured"].as_bool() == Some(false) {
                "off (not configured)".to_string()
            } else {
                "off".to_string()
            };
            // A connector a marketplace installed says who vouches for it.
            let name = match e["trust"].as_str() {
                Some("unverified") => format!("{} [unverified]", s("displayName")),
                _ => s("displayName"),
            };
            format!(
                "{:<14} {:<14} node: {:<44} me: {} ({})",
                s("id"),
                name,
                node,
                s("state"),
                s("label")
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{connection_path, list_lines, node_request, state_line};

    #[test]
    fn enable_and_disable_request_shapes() {
        let (p, b) = node_request("linear", true, None).unwrap();
        assert_eq!(p, "/v1/connectors/linear/node");
        assert_eq!(b, serde_json::json!({ "enabled": true }));
        let t = "11111111-1111-4111-8111-111111111111";
        let (p, b) = node_request("microsoft-365", true, Some(t)).unwrap();
        assert_eq!(p, "/v1/connectors/microsoft-365/node");
        assert_eq!(b["consent"]["tenantId"], t);
        let (_, b) = node_request("linear", false, None).unwrap();
        assert_eq!(b, serde_json::json!({ "enabled": false }));
        for bad in ["", "../x", "Linear", "a/b", "a b"] {
            assert!(node_request(bad, true, None).is_err(), "{bad}");
        }
    }

    #[test]
    fn disconnect_and_check_request_shapes() {
        assert_eq!(
            connection_path("microsoft-365", false).unwrap(),
            "/v1/connectors/microsoft-365/connection"
        );
        assert_eq!(
            connection_path("microsoft-365", true).unwrap(),
            "/v1/connectors/microsoft-365/connection/check"
        );
        for bad in ["", "../x", "Linear", "a/b"] {
            assert!(connection_path(bad, true).is_err(), "{bad}");
        }
        let body = r#"{"id":"microsoft-365","state":"connected","label":"member@e2e.example"}"#;
        assert_eq!(
            state_line(body).unwrap(),
            "microsoft-365: connected (member@e2e.example)"
        );
        assert!(state_line("{}").is_none());
    }

    #[test]
    fn list_lines_show_node_and_my_state() {
        let body = r#"{"connectors":[
          {"id":"linear","displayName":"Linear","state":"askAdmin","label":"Ask your admin","node":{"enabled":false,"configured":true}},
          {"id":"microsoft-365","displayName":"Microsoft 365","state":"connect","label":"Connect","node":{"enabled":true,"configured":true,"consent":{"tenantId":"T"}}},
          {"id":"x","displayName":"X","state":"askAdmin","label":"Ask your admin","node":{"enabled":false,"configured":false}},
          {"id":"bugwatch","displayName":"Bugwatch","trust":"unverified","state":"askAdmin","label":"Ask your admin","node":{"enabled":false,"configured":true}}]}"#;
        let l = list_lines(body);
        assert_eq!(l.len(), 4);
        assert!(l[3].contains("Bugwatch [unverified]"), "{}", l[3]);
        assert!(!l[0].contains("unverified"));
        assert!(
            l[0].contains("off") && l[0].contains("askAdmin (Ask your admin)"),
            "{}",
            l[0]
        );
        assert!(l[1].contains("enabled (consent T)") && l[1].contains("connect (Connect)"));
        assert!(l[2].contains("not configured"));
        assert!(list_lines("not json").is_empty());
    }
}
