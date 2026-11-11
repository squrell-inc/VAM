//! `vam marketplace`: the Git marketplaces this node trusts (org admin). The node reads them; an
//! install is `vam catalog import` or the app. See `GET /v1/marketplaces` in the API reference.

use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde_json::{json, Value};

use crate::client::{self, Client};
use crate::config;

#[derive(Subcommand)]
pub enum MarketplaceCmd {
    /// GET /v1/marketplaces: each trusted marketplace with its trust, last read and error.
    Ls,
    /// POST /v1/marketplaces: trust a Git marketplace. The node reads it once; `--publisher-domain`
    /// is the domain you confirm the index declares (it is refused if it differs, or if it is a
    /// domain the node reserves: Squrell's, this org's, a local one, or another marketplace's).
    Add {
        /// An https:// URL of a Git repository (no user name, no port, a public host).
        url: String,
        #[arg(long = "publisher-domain")]
        publisher_domain: String,
    },
    /// PUT /v1/marketplaces/<id> enabled: read it again on its timer and show its listings.
    Enable { id: String },
    /// PUT /v1/marketplaces/<id> disabled: hide its listings (nothing already installed changes).
    Disable { id: String },
    /// DELETE /v1/marketplaces/<id>: remove an added marketplace (the preconfigured one can only be
    /// disabled).
    Rm { id: String },
    /// POST /v1/marketplaces/<id>/refresh: read it now (index, tags, what each listing installs).
    /// Reads only; installs nothing.
    Refresh { id: String },
    /// GET /v1/marketplaces/<id>/listings: what each listing installs and can reach.
    Listings { id: String },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: MarketplaceCmd) -> Result<()> {
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        MarketplaceCmd::Ls => {
            let body = c.get("/v1/marketplaces").await?;
            if !crate::output::is_json() {
                for line in list_lines(&body) {
                    println!("{line}");
                }
            }
            client::print_json(&body);
        }
        MarketplaceCmd::Add {
            url,
            publisher_domain,
        } => {
            let key = client::resolve_idempotency_key(None);
            let body = add_body(&url, &publisher_domain);
            let out = c
                .post_raw_json("/v1/marketplaces", &body.to_string(), &key)
                .await?;
            client::print_json(&out);
        }
        MarketplaceCmd::Enable { id } => put_enabled(&c, &id, true).await?,
        MarketplaceCmd::Disable { id } => put_enabled(&c, &id, false).await?,
        MarketplaceCmd::Rm { id } => {
            let path = id_path(&id, "")?;
            let key = client::resolve_idempotency_key(None);
            client::print_json(&c.delete(&path, Some(&key)).await?);
        }
        MarketplaceCmd::Refresh { id } => {
            let path = id_path(&id, "/refresh")?;
            let key = client::resolve_idempotency_key(None);
            client::print_json(&c.post_raw_json(&path, "{}", &key).await?);
        }
        MarketplaceCmd::Listings { id } => {
            let path = id_path(&id, "/listings")?;
            let body = c.get(&path).await?;
            if !crate::output::is_json() {
                for line in listing_lines(&body) {
                    println!("{line}");
                }
            }
            client::print_json(&body);
        }
    }
    Ok(())
}

async fn put_enabled(c: &Client, id: &str, enabled: bool) -> Result<()> {
    let path = id_path(id, "")?;
    let key = client::resolve_idempotency_key(None);
    let out = c
        .put_raw_json(
            &path,
            &json!({ "enabled": enabled }).to_string(),
            Some(&key),
        )
        .await?;
    client::print_json(&out);
    Ok(())
}

/// `/v1/marketplaces/<id><suffix>`; the id is one path segment of `[a-z0-9-]`.
pub fn id_path(id: &str, suffix: &str) -> Result<String> {
    if id.is_empty()
        || id.len() > 40
        || id.starts_with('-')
        || !id
            .chars()
            .all(|ch| ch.is_ascii_lowercase() || ch.is_ascii_digit() || ch == '-')
    {
        return Err(anyhow!(
            "`{id}` is not a marketplace id (see `vam marketplace ls`)"
        ));
    }
    Ok(format!("/v1/marketplaces/{id}{suffix}"))
}

pub fn add_body(url: &str, publisher_domain: &str) -> Value {
    json!({ "url": url, "publisherDomain": publisher_domain })
}

/// One line per marketplace of a `GET /v1/marketplaces` body.
pub fn list_lines(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(list) = v["marketplaces"].as_array() else {
        return Vec::new();
    };
    let mut lines: Vec<String> = list
        .iter()
        .map(|m| {
            let s = |k: &str| m[k].as_str().unwrap_or("-").to_string();
            let state = if m["enabled"].as_bool().unwrap_or(false) {
                "enabled"
            } else {
                "disabled"
            };
            let read = match (m["lastError"].as_str(), m["lastCheckedAt"].as_i64()) {
                (Some(e), _) => format!("error: {e}"),
                (None, Some(_)) => "read ok".to_string(),
                (None, None) => "not read yet".to_string(),
            };
            format!(
                "{:<14} {:<12} {:<10} {:<9} {}  {}",
                s("id"),
                s("name"),
                s("trust"),
                state,
                s("url"),
                read
            )
        })
        .collect();
    if v["gitAvailable"].as_bool() == Some(false) {
        lines.push("git is not installed on this node: marketplaces cannot be read".to_string());
    }
    lines
}

/// One line per listing of a `GET /v1/marketplaces/<id>/listings` body.
pub fn listing_lines(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let Some(list) = v["listings"].as_array() else {
        return Vec::new();
    };
    list.iter()
        .map(|l| {
            let s = |k: &str| l[k].as_str().unwrap_or("-").to_string();
            let status = match l["error"].as_str() {
                Some(e) => format!("cannot install: {e}"),
                None => "ok".to_string(),
            };
            format!("{:<20} {:<28} {}", s("id"), s("ref"), status)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_one_plain_segment() {
        assert_eq!(
            id_path("squrell", "/refresh").unwrap(),
            "/v1/marketplaces/squrell/refresh"
        );
        assert_eq!(
            id_path("m-0a1b2c3d4e", "").unwrap(),
            "/v1/marketplaces/m-0a1b2c3d4e"
        );
        for bad in ["", "../x", "A", "a/b", "a b", "-x", "a?b"] {
            assert!(id_path(bad, "").is_err(), "{bad}");
        }
    }

    #[test]
    fn add_sends_the_url_and_the_confirmed_domain() {
        let b = add_body("https://git.example.com/a/b", "a.example.com");
        assert_eq!(b["url"], "https://git.example.com/a/b");
        assert_eq!(b["publisherDomain"], "a.example.com");
    }

    #[test]
    fn the_list_shows_trust_state_and_errors() {
        let body = r#"{"gitAvailable":false,"marketplaces":[
          {"id":"squrell","name":"Squrell","url":"https://github.com/squrell-inc/vam-marketplace","trust":"squrell","enabled":true,"lastCheckedAt":5},
          {"id":"m-1","name":"Community","url":"https://git.example.com/a/b","trust":"unverified","enabled":false,"lastError":"index is not JSON","lastCheckedAt":6}]}"#;
        let l = list_lines(body);
        assert_eq!(l.len(), 3);
        assert!(
            l[0].contains("squrell") && l[0].contains("enabled") && l[0].contains("read ok"),
            "{}",
            l[0]
        );
        assert!(
            l[1].contains("unverified")
                && l[1].contains("disabled")
                && l[1].contains("error: index is not JSON"),
            "{}",
            l[1]
        );
        assert!(l[2].contains("git is not installed"));
        assert!(list_lines("{}").is_empty());
    }

    #[test]
    fn listings_show_what_cannot_be_installed() {
        let body =
            r#"{"listings":[{"id":"a","ref":"a-v1"},{"id":"b","ref":"b-v1","error":"tag moved"}]}"#;
        let l = listing_lines(body);
        assert!(l[0].ends_with("ok") && l[1].contains("cannot install: tag moved"));
    }
}
