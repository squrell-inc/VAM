//! Connector files shipped in a marketplace listing (CN-4.11, `design/connectors.md` D25 and D26).
//!
//! A listing folder may hold a connector file (`connector.json`, the format of
//! `spec/connectors/*.json`). The node imports it like the other artifacts, but a connector is
//! the one artifact that makes the node reach a third party's server on a member's behalf, so a
//! file from a marketplace is held to more than [`crate::connector::validate_connector`]:
//!
//! | Rule | Why |
//! |---|---|
//! | the id (last URN segment) is a short slug and not Squrell's (`microsoft-365`, `linear`, any `spec/connectors/` file) | a fixed id must mean the same connector on every node |
//! | provider `oauth2`, profile `discover`, `clientSecret: none`, `orgConsent: none` | the only sign-in a node can run for a marketplace connector without a secret or a REST binding |
//! | `api_key` provider | refused: a node has no connection flow that stores an API key for a connector |
//! | `entra`, `google`, `slack` | refused: `entra` needs REST bindings (tokens attached to requests the guest makes), the others need a client secret |
//! | `mcp` with an `https` URL on a full DNS name, 1 to 64 vetted tools, no `bindings` | a tool list the admin can read; no token reaches the guest |
//! | names that pass for Squrell or a built-in connector | an Unverified connector must not look verified |
//! | `icon` a plain name | the app must not load an address a publisher chose |
//!
//! Nothing here reads a database or the network. The node adds what depends on it: the URN under
//! the marketplace's own `publisherDomain`, and clashes with other installed connectors.

use serde_json::Value;

use crate::connector::{
    is_placeholder_client_id, parse_connector, ClientSecret, Connector, ConnectorError,
    OAuthProfile, OrgConsent, Provider, BUILTIN_CONNECTOR_FILES,
};

/// Ids only Squrell's built-in connectors may have (design decision D26). Every id of a file in
/// `spec/connectors/` is reserved too ([`is_reserved_connector_id`]), so adding a built-in file
/// reserves its id without editing this list.
pub const RESERVED_CONNECTOR_IDS: [&str; 2] = ["microsoft-365", "linear"];

/// Longest connector id: the id is also the id of a member's MCP connection.
pub const MAX_CONNECTOR_ID_LEN: usize = 64;

/// Sign-in profiles a connector file may name, as the words in the file.
const KNOWN_PROFILES: [&str; 4] = ["entra", "discover", "google", "slack"];

fn bad<T>(m: impl Into<String>) -> Result<T, ConnectorError> {
    Err(ConnectorError::Invalid(m.into()))
}

/// The id of the connector a URN names: its last segment.
pub fn connector_id_of_urn(urn: &str) -> &str {
    urn.rsplit('/').next().unwrap_or_default()
}

fn builtin_values() -> impl Iterator<Item = Value> {
    BUILTIN_CONNECTOR_FILES
        .iter()
        .filter_map(|(_, raw)| serde_json::from_str::<Value>(raw).ok())
}

/// True when `id` is one of Squrell's: named in [`RESERVED_CONNECTOR_IDS`] or the id of a built-in
/// connector file.
pub fn is_reserved_connector_id(id: &str) -> bool {
    RESERVED_CONNECTOR_IDS.contains(&id)
        || builtin_values().any(|v| {
            v.get("urn")
                .and_then(Value::as_str)
                .is_some_and(|urn| connector_id_of_urn(urn) == id)
        })
}

fn lower_alnum(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Whose name `name` passes for (Squrell, or a built-in connector's name or publisher), if anyone's.
fn imitates(name: &str) -> Option<String> {
    let norm = lower_alnum(name);
    if norm.contains("squrell") {
        return Some("Squrell".into());
    }
    for v in builtin_values() {
        for key in ["displayName", "publisher"] {
            if let Some(n) = v.get(key).and_then(Value::as_str) {
                if lower_alnum(n) == norm {
                    return Some(n.to_string());
                }
            }
        }
    }
    None
}

const INTERNAL_SUFFIXES: [&str; 11] = [
    ".localhost",
    ".local",
    ".localdomain",
    ".internal",
    ".intranet",
    ".corp",
    ".home",
    ".lan",
    ".private",
    ".svc",
    ".home.arpa",
];

/// The host of an MCP server URL a marketplace connector may name: `https`, a full lowercase DNS
/// name (no IP address, no user info), no port other than 443, no query or fragment. The node
/// still checks the addresses the name resolves to when it connects.
pub fn check_mcp_server_url(url: &str) -> Result<String, String> {
    let rest = url
        .strip_prefix("https://")
        .ok_or("mcp.url must be an https url")?;
    if url.len() > 512 {
        return Err("mcp.url is longer than 512 characters".into());
    }
    if url
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == '\\')
    {
        return Err("mcp.url must not hold whitespace, control characters or a backslash".into());
    }
    if rest.contains('?') || rest.contains('#') {
        return Err("mcp.url must not carry a query or a fragment".into());
    }
    let authority = rest.split('/').next().unwrap_or_default();
    if authority.contains('@') {
        return Err("mcp.url must not carry user info".into());
    }
    if authority.starts_with('[') {
        return Err(
            "mcp.url must name a server by a full lowercase DNS name (no IP address, no bare name)"
                .into(),
        );
    }
    let host = match authority.rsplit_once(':') {
        Some((h, "443")) => h,
        Some(_) => return Err("mcp.url must use the default https port".into()),
        None => authority,
    };
    let labels: Vec<&str> = host.split('.').collect();
    let dns = labels.len() >= 2
        && labels.iter().all(|l| {
            !l.is_empty()
                && l.len() <= 63
                && !l.starts_with('-')
                && !l.ends_with('-')
                && l.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
        && labels
            .last()
            .is_some_and(|l| !l.bytes().all(|b| b.is_ascii_digit()));
    if !dns || host.len() > 253 {
        return Err(
            "mcp.url must name a server by a full lowercase DNS name (no IP address, no bare name)"
                .into(),
        );
    }
    let dotted = format!(".{host}");
    if INTERNAL_SUFFIXES.iter().any(|s| dotted.ends_with(s)) {
        return Err(format!("mcp.url host `{host}` names a private network"));
    }
    Ok(host.to_string())
}

fn token_ok(s: &str, max: usize) -> bool {
    !s.is_empty() && s.len() <= max && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

/// The limits on a connector file shipped in a marketplace listing (module docs). The file has
/// already passed [`crate::connector::validate_connector`].
pub fn check_marketplace_connector(c: &Connector) -> Result<(), ConnectorError> {
    let id = connector_id_of_urn(&c.urn);
    let id_ok = !id.is_empty()
        && id.len() <= MAX_CONNECTOR_ID_LEN
        && id.starts_with(|ch: char| ch.is_ascii_lowercase() || ch.is_ascii_digit())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if !id_ok {
        return bad(format!(
            "connector id `{id}` must be 1 to {MAX_CONNECTOR_ID_LEN} characters of a-z, 0-9, '-' or '_'"
        ));
    }
    if is_reserved_connector_id(id) {
        return bad(format!(
            "connector id `{id}` is a Squrell connector id: a marketplace connector cannot take it"
        ));
    }
    if !token_ok(&c.version, 64) {
        return bad("version must be a short token");
    }
    for (n, v) in [
        ("displayName", &c.display_name),
        ("publisher", &c.publisher),
    ] {
        if v.len() > 120 || v.chars().any(char::is_control) {
            return bad(format!("{n} must be at most 120 characters, on one line"));
        }
        if let Some(whose) = imitates(v) {
            return bad(format!(
                "{n} `{v}` passes for {whose}: a marketplace connector must have its own name"
            ));
        }
    }
    if c.description.len() > 2000 {
        return bad("description is longer than 2000 characters");
    }
    let icon_ok = !c.icon.is_empty()
        && c.icon.len() <= 40
        && c.icon
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if !icon_ok {
        return bad("icon must be a plain name of a-z, 0-9 and '-' (not a URL)");
    }
    if c.connection.label.len() > 64 || c.connection.label.chars().any(char::is_control) {
        return bad("connection.label must be at most 64 characters");
    }
    if c.events.is_some() {
        return bad("events are reserved and not accepted from a marketplace");
    }
    match &c.provider {
        Provider::ApiKey(_) => {
            return bad("api_key connectors are not supported yet: a node has no connection flow for an API key")
        }
        Provider::Oauth2(p) => {
            if p.profile != OAuthProfile::Discover {
                let name = match p.profile {
                    OAuthProfile::Entra => "entra",
                    OAuthProfile::Google => "google",
                    OAuthProfile::Slack => "slack",
                    OAuthProfile::Discover => "discover",
                };
                return bad(format!(
                    "OAuth profile `{name}` is not available to a marketplace connector yet: use `discover` with a remote MCP server"
                ));
            }
            if p.client_secret != ClientSecret::None {
                return bad("the connector asks for a client secret: a marketplace connector must use `clientSecret: none`");
            }
            if p.org_consent != OrgConsent::None {
                return bad("orgConsent must be `none` for profile `discover`");
            }
            if let Some(cid) = &p.client_id {
                let ok = !cid.is_empty()
                    && cid.len() <= 128
                    && cid
                        .chars()
                        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
                    && !is_placeholder_client_id(cid);
                if !ok {
                    return bad(
                        "clientId must be 1-128 characters of letters, digits, '.', '_' or '-'",
                    );
                }
            }
            if p.scopes.len() > 32 || p.scopes.iter().any(|s| !token_ok(s, 128)) {
                return bad("scopes must be at most 32 plain tokens of at most 128 characters");
            }
        }
    }
    if c.bindings.is_some() {
        return bad("a marketplace connector cannot declare bindings: it is remote MCP only");
    }
    let Some(mcp) = &c.mcp else {
        return bad("a marketplace connector needs an mcp server");
    };
    check_mcp_server_url(&mcp.url).map_err(ConnectorError::Invalid)?;
    if mcp.tools.is_empty() || mcp.tools.len() > 64 {
        return bad("mcp.tools must list 1 to 64 vetted tools");
    }
    for t in &mcp.tools {
        let ok = !t.remote.is_empty()
            && t.remote.len() <= 64
            && t.remote
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
        if !ok {
            return bad(format!(
                "mcp tool name `{}` must be 1-64 characters of letters, digits, '_', '-' or '.'",
                t.remote
            ));
        }
    }
    Ok(())
}

/// Parses the text of a connector file shipped in a marketplace listing, validates it as any
/// connector, and applies [`check_marketplace_connector`]. What is wrong is said in the file's own
/// words (an unknown profile, a client secret) before any generic parse error.
pub fn parse_marketplace_connector(raw: &str) -> Result<Connector, ConnectorError> {
    let v: Value = serde_json::from_str(raw).map_err(|e| ConnectorError::Parse(e.to_string()))?;
    if let Some(p) = v.get("provider") {
        match p.get("kind").and_then(Value::as_str) {
            Some("api_key") => {
                return bad("api_key connectors are not supported yet: a node has no connection flow for an API key")
            }
            Some("oauth2") | None => {}
            Some(other) => return bad(format!("unknown provider kind `{other}`")),
        }
        if let Some(secret) = p.get("clientSecret") {
            if secret.as_str() != Some("none") {
                return bad("the connector asks for a client secret: a marketplace connector must use `clientSecret: none`");
            }
        }
        if let Some(profile) = p.get("profile") {
            let known = profile
                .as_str()
                .is_some_and(|x| KNOWN_PROFILES.contains(&x));
            if !known {
                return bad(format!(
                    "unknown OAuth profile {profile}: the profiles a node knows are {}",
                    KNOWN_PROFILES.join(", ")
                ));
            }
        }
    }
    let c = parse_connector(raw)?;
    check_marketplace_connector(&c)?;
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn good() -> Value {
        json!({
            "urn": "urn:vam:connector:market.example.org/acme/bugwatch",
            "version": "1.0.0",
            "displayName": "Bugwatch",
            "publisher": "Acme Tools",
            "description": "Issues in Bugwatch.",
            "icon": "bugwatch",
            "provider": { "kind": "oauth2", "profile": "discover", "clientSecret": "none",
                          "scopes": ["read", "write"], "orgConsent": "none" },
            "connection": { "holder": "user", "multiAccount": false, "label": "whoami.email" },
            "mcp": { "url": "https://mcp.bugwatch.example.org/mcp", "tools": [
                { "remote": "list_issues", "sideEffect": "READ_ONLY" },
                { "remote": "save_issue", "sideEffect": "MUTABLE_EXTERNAL", "default": "ask" } ] }
        })
    }

    fn err(v: Value) -> String {
        parse_marketplace_connector(&v.to_string())
            .unwrap_err()
            .to_string()
    }

    #[test]
    fn a_remote_mcp_connector_with_discovery_passes() {
        let c = parse_marketplace_connector(&good().to_string()).unwrap();
        assert_eq!(connector_id_of_urn(&c.urn), "bugwatch");
        // A declared client id is accepted (it is shown to the admin).
        let mut v = good();
        v["provider"]["clientId"] = json!("pub-client-1");
        assert!(parse_marketplace_connector(&v.to_string()).is_ok());
    }

    #[test]
    fn squrell_ids_are_refused() {
        for id in ["linear", "microsoft-365"] {
            let mut v = good();
            v["urn"] = json!(format!("urn:vam:connector:market.example.org/acme/{id}"));
            let e = err(v);
            assert!(e.contains("Squrell connector id") && e.contains(id), "{e}");
        }
        assert!(is_reserved_connector_id("linear"));
        assert!(!is_reserved_connector_id("bugwatch"));
    }

    #[test]
    fn the_id_is_a_slug_that_fits_a_connection_id() {
        for id in ["Bad", "dot.ted", "-lead", &"x".repeat(65)] {
            let mut v = good();
            v["urn"] = json!(format!("urn:vam:connector:market.example.org/acme/{id}"));
            let e = err(v);
            assert!(e.contains("connector id") || e.contains("urn"), "{id}: {e}");
        }
    }

    #[test]
    fn an_unknown_profile_and_a_secret_are_named() {
        let mut v = good();
        v["provider"]["profile"] = json!("generic");
        assert!(err(v).contains("unknown OAuth profile"));
        let mut v = good();
        v["provider"]["clientSecret"] = json!("squrell_relay");
        assert!(err(v).contains("client secret"));
        let mut v = good();
        v["provider"]["clientSecret"] = json!("hunter2");
        assert!(err(v).contains("client secret"));
    }

    #[test]
    fn profiles_and_providers_without_a_flow_are_refused() {
        let mut v = good();
        v["provider"] = json!({ "kind": "api_key", "fields": [{ "name": "key", "label": "Key" }] });
        assert!(err(v).contains("api_key"));
        for profile in ["google", "slack"] {
            let mut v = good();
            v["provider"]["profile"] = json!(profile);
            assert!(err(v).contains("not available"), "{profile}");
        }
        let mut v = good();
        v["provider"]["profile"] = json!("entra");
        v["provider"]["orgConsent"] = json!("admin_required");
        v["provider"]["scopes"] = json!(["offline_access"]);
        v.as_object_mut().unwrap().remove("mcp");
        v["bindings"] = json!([{ "name": "api", "placeholder": "vamsec_x", "value": "access_token",
                                  "inject": "bearer", "hosts": ["api.example.org"] }]);
        assert!(err(v).contains("not available"));
    }

    #[test]
    fn bindings_events_and_a_missing_server_are_refused() {
        let mut v = good();
        v["bindings"] = json!([{ "name": "api", "placeholder": "vamsec_x", "value": "access_token",
                                  "inject": "bearer", "hosts": ["api.example.org"] }]);
        assert!(err(v).contains("exactly one"));
        let mut v = good();
        v["events"] = json!([{}]);
        assert!(err(v).contains("events"));
        let mut v = good();
        v["mcp"]["tools"] = json!([]);
        assert!(err(v).contains("1 to 64"));
    }

    #[test]
    fn the_server_url_is_a_public_https_dns_name() {
        for (url, why) in [
            ("http://mcp.example.org/mcp", "https"),
            ("https://127.0.0.1/mcp", "DNS name"),
            ("https://10.0.0.1/mcp", "DNS name"),
            ("https://[::1]/mcp", "DNS name"),
            ("https://intranet/mcp", "DNS name"),
            ("https://u:p@mcp.example.org/mcp", "user info"),
            ("https://mcp.example.org:8443/mcp", "port"),
            ("https://mcp.example.org/mcp?x=1", "query"),
            ("https://mcp.example.org/mcp#f", "query"),
            ("https://db.corp/mcp", "private network"),
            ("https://svc.cluster.local/mcp", "private network"),
            ("https://MCP.example.org/mcp", "lowercase"),
        ] {
            let mut v = good();
            v["mcp"]["url"] = json!(url);
            let e = err(v);
            assert!(e.contains(why), "{url}: {e}");
        }
        assert_eq!(
            check_mcp_server_url("https://mcp.example.org:443/mcp").unwrap(),
            "mcp.example.org"
        );
    }

    #[test]
    fn names_that_pass_for_squrell_are_refused() {
        for (field, value) in [
            ("publisher", "Squrell"),
            ("publisher", "The Squrell Team"),
            ("displayName", "Microsoft 365"),
            ("displayName", "linear"),
            ("displayName", "  L-i-n-e-a-r "),
        ] {
            let mut v = good();
            v[field] = json!(value);
            assert!(err(v).contains("passes for"), "{field} {value}");
        }
        let mut v = good();
        v["icon"] = json!("https://evil.example/x.png");
        assert!(err(v).contains("icon"));
    }

    #[test]
    fn the_built_in_files_are_not_marketplace_connectors() {
        for (name, raw) in BUILTIN_CONNECTOR_FILES {
            assert!(parse_marketplace_connector(raw).is_err(), "{name}");
        }
    }
}
