use anyhow::Result;
use clap::Subcommand;

use crate::client::{self, Client};
use crate::config;

#[derive(Subcommand)]
pub enum NodeCmd {
    /// GET /v1/node
    Status,
    /// The node's receipt-signing public keys as JSON (`vam receipt verify --trusted-keys` reads it)
    Keys,
    /// Prepare this host to run a node. `--check` lists what the host lacks, each with its fix,
    /// and changes nothing; it needs no host and no key.
    Init {
        /// Only check: print one line per requirement and exit 1 if any is missing or wrong.
        #[arg(long)]
        check: bool,
        /// Look every host path up under this directory instead of `/` (tests and packaging).
        #[arg(long, value_name = "DIR")]
        root: Option<std::path::PathBuf>,
        /// The `vamd` binary that supplies the jailer rules (`vamd --preflight`). Default: the
        /// `vamd` beside `vam`.
        #[arg(long, value_name = "PATH")]
        vamd: Option<std::path::PathBuf>,
        /// Install without writing a systemd unit (containers, packaging).
        #[arg(long)]
        no_systemd: bool,
        /// Write the unit but do not enable or start the service.
        #[arg(long)]
        no_start: bool,
        /// Use this base image (a path or URL; its `.base.json` manifest sits beside it)
        /// instead of building one from the checkout. Needs `--base-sha256`.
        #[arg(long, value_name = "PATH|URL")]
        base: Option<String>,
        /// The sha256 the `--base` image must have.
        #[arg(long, value_name = "SHA256")]
        base_sha256: Option<String>,
        /// The name clients use for this node; added to the TLS certificate names.
        #[arg(long, value_name = "NAME")]
        host_name: Option<String>,
        /// The repository checkout to build the base image from (default: the current
        /// directory, or the checkout the binary was built in).
        #[arg(long, value_name = "DIR")]
        checkout: Option<std::path::PathBuf>,
    },
    /// The notification channels this node offers (operator): show them, or set the adapter, the
    /// vault secret names it reads its tokens from, the org's kind defaults and the app URL.
    Notify {
        #[command(subcommand)]
        command: Option<NotifyCmd>,
    },
}

#[derive(Subcommand)]
pub enum NotifyCmd {
    /// GET /v1/node/notify
    Show,
    /// PUT /v1/node/notify (what you do not name stays as it is). Values are vault secret
    /// **names** (`vam secrets set --scope org ...` first); a token itself is never sent here.
    Set {
        /// Turn the Slack adapter on or off.
        #[arg(long, value_parser = ["on", "off"])]
        slack: Option<String>,
        /// Org vault secret holding the Slack bot token (for example notify/slack/bot-token).
        #[arg(long)]
        bot_token_ref: Option<String>,
        /// Org vault secret holding the Slack app-level token.
        #[arg(long)]
        app_token_ref: Option<String>,
        /// The https URL of the VAM app, for the Open in VAM link.
        #[arg(long)]
        app_url: Option<String>,
        /// Org default for a kind, `kind=immediate|off` (repeatable).
        #[arg(long = "kind", value_name = "KIND=MODE")]
        kinds: Vec<String>,
    },
}

pub async fn run(host: Option<String>, token: Option<String>, cmd: NodeCmd) -> Result<()> {
    // `init` runs on a host that has no node yet: no host, no key, no config.
    if let NodeCmd::Init {
        check,
        root,
        vamd,
        no_systemd,
        no_start,
        base,
        base_sha256,
        host_name,
        checkout,
    } = cmd
    {
        if check {
            return super::node_init::run_check(root, vamd);
        }
        return super::node_install::run_install(super::node_install::Options {
            root: root.unwrap_or_else(|| std::path::PathBuf::from("/")),
            vamd,
            no_systemd,
            no_start,
            base,
            base_sha256,
            host_name,
            checkout,
        })
        .await;
    }
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match cmd {
        NodeCmd::Status => {
            let body = c.get("/v1/node").await?;
            if !crate::output::is_json() {
                if let Some(line) = isolation_line(&body) {
                    println!("isolation: {line}");
                }
                if let Some(line) = base_line(&body) {
                    println!("{line}");
                }
                // An older node, or a key without `node:read`, simply prints no models.
                if let Ok(models) = c.get("/v1/node/allowed-models").await {
                    for line in models_lines(&models) {
                        println!("{line}");
                    }
                }
            }
            client::print_json(&body);
        }
        NodeCmd::Keys => {
            let body = c.get("/v1/node").await?;
            println!("{}", keys_document(&body)?);
        }
        NodeCmd::Init { .. } => anyhow::bail!("node init is handled before the config is resolved"),
        NodeCmd::Notify { command } => match command.unwrap_or(NotifyCmd::Show) {
            NotifyCmd::Show => {
                let body = c.get("/v1/node/notify").await?;
                if !crate::output::is_json() {
                    for line in notify_lines(&body) {
                        println!("{line}");
                    }
                }
                client::print_json(&body);
            }
            NotifyCmd::Set {
                slack,
                bot_token_ref,
                app_token_ref,
                app_url,
                kinds,
            } => {
                let current = c.get("/v1/node/notify").await?;
                let next = merge_notify(
                    &current,
                    slack.as_deref(),
                    bot_token_ref.as_deref(),
                    app_token_ref.as_deref(),
                    app_url.as_deref(),
                    &kinds,
                )?;
                let key = client::resolve_idempotency_key(None);
                let body = c
                    .put_raw_json("/v1/node/notify", &next.to_string(), Some(&key))
                    .await?;
                client::print_json(&body);
            }
        },
    }
    Ok(())
}

/// The `{"signingKeys":[{keyId,publicKey,state}]}` document of a `GET /v1/node` body.
/// Only those three public fields are copied; nothing else of the response is printed.
pub fn keys_document(body: &str) -> Result<String> {
    let v: serde_json::Value = serde_json::from_str(body)?;
    let listed = v["status"]["signingKeys"]
        .as_array()
        .filter(|a| !a.is_empty())
        .ok_or_else(|| anyhow::anyhow!("this node does not publish signing keys"))?;
    let mut keys = Vec::with_capacity(listed.len());
    for k in listed {
        let field = |name: &str| {
            k[name]
                .as_str()
                .map(String::from)
                .ok_or_else(|| anyhow::anyhow!("signing key without {name}"))
        };
        keys.push(serde_json::json!({
            "keyId": field("keyId")?,
            "publicKey": field("publicKey")?,
            "state": field("state")?,
        }));
    }
    Ok(serde_json::to_string_pretty(
        &serde_json::json!({ "signingKeys": keys }),
    )?)
}

/// The node's isolation line from a `GET /v1/node` body; `None` for an older node.
pub fn isolation_line(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    v["status"]["isolation"]["summary"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// The `base:` line of a `GET /v1/node` body, for example
/// `base: vam/base:2 (sha256:0123456789ab, verified) python 3.13.5, git 2.47.3`; `None` for a
/// node that reports no base.
pub fn base_line(body: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(body).ok()?;
    let base = v["status"]["base"].as_object()?;
    let text = |k: &str| base.get(k).and_then(|x| x.as_str()).unwrap_or_default();
    let name = text("name");
    let digest = text("digest");
    let digest = digest.strip_prefix("sha256:").unwrap_or(digest);
    let short: String = digest.chars().take(12).collect();
    let state = if base.get("verified").and_then(|x| x.as_bool()) == Some(true) {
        "verified"
    } else {
        "unverified"
    };
    let runtimes: Vec<String> = base
        .get("runtimes")
        .and_then(|r| r.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|r| {
                    Some(format!(
                        "{} {}",
                        r["name"].as_str()?,
                        r["version"].as_str()?
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let tail = if runtimes.is_empty() {
        "no runtimes".to_string()
    } else {
        runtimes.join(", ")
    };
    let head = if name.is_empty() {
        format!("base: none ({state})")
    } else if short.is_empty() {
        format!("base: {name} ({state})")
    } else {
        format!("base: {name} (sha256:{short}, {state})")
    };
    Some(format!("{head} {tail}"))
}

/// The `models:` lines of a `GET /v1/node/allowed-models` body: the default a run gets when
/// nothing names a model, and the allowed list. Empty for a body that is not that answer.
pub fn models_lines(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let listed: Vec<&str> = v["models"]
        .as_array()
        .map(|a| a.iter().filter_map(|m| m.as_str()).collect())
        .unwrap_or_default();
    let default = match (
        v["defaultModel"].as_str(),
        v["effectiveDefaultModel"].as_str(),
    ) {
        (Some(d), _) if !d.is_empty() => format!("{d} (set by the operator)"),
        (_, Some(e)) if !e.is_empty() => format!("{e} (none set)"),
        _ => return Vec::new(),
    };
    let allowed = if listed.is_empty() {
        "any (the operator has not set a list)".to_string()
    } else {
        listed.join(", ")
    };
    let mut lines = vec![
        format!("default model: {default}"),
        format!("allowed models: {allowed}"),
    ];
    if let Some(g) = v["defaultGateway"].as_str().filter(|g| !g.is_empty()) {
        lines.push(format!(
            "default gateway: {g} (for a provider/model id the catalog does not know)"
        ));
    }
    lines
}

/// The body for `PUT /v1/node/notify`: the current settings with the named changes laid over them.
/// Only the fields the endpoint accepts are carried over (`channels`, `updatedAt` are not).
pub fn merge_notify(
    current: &str,
    slack: Option<&str>,
    bot_token_ref: Option<&str>,
    app_token_ref: Option<&str>,
    app_url: Option<&str>,
    kinds: &[String],
) -> Result<serde_json::Value> {
    let cur: serde_json::Value = serde_json::from_str(current).unwrap_or_default();
    let mut s = cur["slack"].as_object().cloned().unwrap_or_default();
    // A reader without the operator scope sees no refs; setting then needs both named.
    s.entry("enabled").or_insert(serde_json::json!(false));
    if let Some(v) = slack {
        s.insert("enabled".into(), serde_json::json!(v == "on"));
    }
    if let Some(v) = bot_token_ref {
        s.insert("botTokenRef".into(), serde_json::json!(v));
    }
    if let Some(v) = app_token_ref {
        s.insert("appTokenRef".into(), serde_json::json!(v));
    }
    let mut defaults = cur["kindDefaults"].as_object().cloned().unwrap_or_default();
    for k in kinds {
        let (kind, mode) = k.split_once('=').ok_or_else(|| {
            anyhow::anyhow!("Wrong: --kind {k}.\nFix: --kind KIND=immediate or --kind KIND=off.")
        })?;
        defaults.insert(kind.trim().into(), serde_json::json!(mode.trim()));
    }
    let mut out = serde_json::json!({ "slack": s, "kindDefaults": defaults });
    if let Some(u) = app_url
        .map(String::from)
        .or_else(|| cur["appUrl"].as_str().map(String::from))
    {
        out["appUrl"] = serde_json::json!(u);
    }
    Ok(out)
}

/// A readable summary of a `GET /v1/node/notify` body.
pub fn notify_lines(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let on = v["slack"]["enabled"].as_bool().unwrap_or(false);
    let mut lines = vec![format!("slack: {}", if on { "on" } else { "off" })];
    for (label, key) in [("bot token", "botTokenRef"), ("app token", "appTokenRef")] {
        if let Some(r) = v["slack"][key].as_str() {
            lines.push(format!("  {label}: vault secret `{r}`"));
        }
    }
    if let Some(u) = v["appUrl"].as_str() {
        lines.push(format!("app url: {u}"));
    }
    if let Some(d) = v["kindDefaults"].as_object().filter(|d| !d.is_empty()) {
        let list: Vec<String> = d
            .iter()
            .map(|(k, m)| format!("{k}={}", m.as_str().unwrap_or("?")))
            .collect();
        lines.push(format!("org kind defaults: {}", list.join(", ")));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::{
        base_line, isolation_line, keys_document, merge_notify, models_lines, notify_lines,
    };

    #[test]
    fn notify_set_lays_changes_over_the_current_settings() {
        let cur = r#"{"slack":{"enabled":true,"botTokenRef":"b","appTokenRef":"a"},"kindDefaults":{"budget.hit":"off"},"appUrl":"https://x.test","channels":["slack"],"updatedAt":5}"#;
        let out = merge_notify(
            cur,
            None,
            Some("b2"),
            None,
            None,
            &["run.finished=immediate".to_string()],
        )
        .unwrap();
        assert_eq!(out["slack"]["botTokenRef"], "b2");
        assert_eq!(out["slack"]["appTokenRef"], "a");
        assert_eq!(out["slack"]["enabled"], true);
        assert_eq!(out["kindDefaults"]["budget.hit"], "off");
        assert_eq!(out["kindDefaults"]["run.finished"], "immediate");
        assert_eq!(out["appUrl"], "https://x.test");
        assert!(out.get("channels").is_none() && out.get("updatedAt").is_none());
        let off = merge_notify(cur, Some("off"), None, None, Some("https://y.test"), &[]).unwrap();
        assert_eq!(off["slack"]["enabled"], false);
        assert_eq!(off["appUrl"], "https://y.test");
        assert!(merge_notify(cur, None, None, None, None, &["nomode".into()]).is_err());
        let from_nothing = merge_notify("{}", Some("on"), Some("b"), Some("a"), None, &[]).unwrap();
        assert_eq!(from_nothing["slack"]["appTokenRef"], "a");
    }

    #[test]
    fn notify_show_names_refs_not_values() {
        let lines = notify_lines(
            r#"{"slack":{"enabled":true,"botTokenRef":"notify/slack/bot-token"},"appUrl":"https://x.test","kindDefaults":{"budget.hit":"off"}}"#,
        );
        assert_eq!(lines[0], "slack: on");
        assert!(lines[1].contains("notify/slack/bot-token"));
        assert!(lines
            .iter()
            .any(|l| l == "org kind defaults: budget.hit=off"));
        assert!(notify_lines("nope").is_empty());
    }

    #[test]
    fn models_show_the_default_and_the_allowed_list() {
        let b = r#"{"models":["a/one","b/two"],"defaultModel":"b/two","effectiveDefaultModel":"b/two"}"#;
        assert_eq!(
            models_lines(b),
            vec![
                "default model: b/two (set by the operator)",
                "allowed models: a/one, b/two"
            ]
        );
        let gw = r#"{"models":[],"effectiveDefaultModel":"x","defaultGateway":"openrouter"}"#;
        assert_eq!(
            models_lines(gw)[2],
            "default gateway: openrouter (for a provider/model id the catalog does not know)"
        );
        let none_set = r#"{"models":["a/one"],"effectiveDefaultModel":"a/one"}"#;
        assert_eq!(models_lines(none_set)[0], "default model: a/one (none set)");
        let open = r#"{"models":[],"effectiveDefaultModel":"claude-x"}"#;
        assert_eq!(
            models_lines(open)[1],
            "allowed models: any (the operator has not set a list)"
        );
        assert!(models_lines("not json").is_empty());
        assert!(models_lines(r#"{"models":[]}"#).is_empty());
    }

    const NODE_BODY: &str = r#"{"status":{"guestMode":"vm","listenAddr":"x","signingKeys":[
        {"keyId":"ed25519:fe812c12f3ab4ce6","publicKey":"ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c","state":"current","seed":"SECRET"},
        {"keyId":"ed25519:aaaaaaaaaaaaaaaa","publicKey":"00","state":"retired"}]}}"#;

    #[test]
    fn keys_document_prints_public_fields_only_in_the_shape_verify_reads() {
        let out = keys_document(NODE_BODY).unwrap();
        assert!(!out.contains("SECRET") && !out.contains("listenAddr"));
        let keys = vam_security::receipt::trusted_keys_from_json(out.as_bytes()).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].key_id, "ed25519:fe812c12f3ab4ce6");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["signingKeys"][1]["state"], "retired");
    }

    #[test]
    fn an_older_node_without_keys_is_an_error() {
        assert!(keys_document(r#"{"status":{"guestMode":"vm"}}"#).is_err());
        assert!(keys_document(r#"{"status":{"signingKeys":[]}}"#).is_err());
        assert!(keys_document("not json").is_err());
    }

    #[test]
    fn reads_the_summary() {
        let b = r#"{"status":{"guestMode":"vm","isolation":{"vmm":"firecracker","summary":"firecracker microVM, jailer off"}}}"#;
        assert_eq!(
            isolation_line(b).as_deref(),
            Some("firecracker microVM, jailer off")
        );
    }

    #[test]
    fn older_node_prints_nothing() {
        assert_eq!(isolation_line(r#"{"status":{"guestMode":"vm"}}"#), None);
        assert_eq!(isolation_line("not json"), None);
    }

    #[test]
    fn base_line_from_a_recorded_body() {
        let b = r#"{"status":{"guestMode":"vm","base":{"name":"vam/base:2","version":"2.0.0",
            "digest":"sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            "verified":true,"runtimes":[{"name":"git","version":"2.47.3"},
            {"name":"python","version":"3.13.5"}]}}}"#;
        assert_eq!(
            base_line(b).as_deref(),
            Some("base: vam/base:2 (sha256:0123456789ab, verified) git 2.47.3, python 3.13.5")
        );
        let u = r#"{"status":{"base":{"name":"vam/base:2","digest":"sha256:abcdef0123456789",
            "runtimes":[]}}}"#;
        assert_eq!(
            base_line(u).as_deref(),
            Some("base: vam/base:2 (sha256:abcdef012345, unverified) no runtimes")
        );
        assert_eq!(base_line(r#"{"status":{"guestMode":"vm"}}"#), None);
        assert_eq!(base_line("not json"), None);
    }
}
