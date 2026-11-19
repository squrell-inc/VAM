//! Connector files shipped in a marketplace listing (CN-4.11).
//!
//! A listing folder may hold a connector file (`connector.json`, the format of
//! `spec/connectors/*.json`). The node imports it like the other artifacts, but a connector is
//! the one artifact that makes the node reach a third party's server on a member's behalf, so a
//! file from a marketplace is held to more than [`crate::connector::validate_connector`]:
//!
//! | Rule | Why |
//! |---|---|
//! | the id (last URN segment) is a short slug and not Squrell's (`microsoft-365`, `linear`, any `spec/connectors/` file) | a fixed id must mean the same connector on every node |
//! | provider `oauth2`, profile `discover`, `clientSecret: none` or (with `client: org`) `org_vault`, `orgConsent: none` | the only sign-ins a node can run for a marketplace connector: the publisher's public client, or the org's own client whose id and secret the node's admin enters at Enable (the file carries neither) |
//! | `squrell_relay` | refused: Squrell's relay is for Squrell's own connectors |
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
    is_placeholder_client_id, parse_connector, url_host, ClientKind, ClientSecret, Connector,
    ConnectorError, Inject, OAuthProfile, OrgConsent, Provider, BUILTIN_CONNECTOR_FILES,
};

/// Ids only Squrell's built-in connectors may have (design decision D26). Every id of a file in
/// `spec/connectors/` is reserved too ([`is_reserved_connector_id`]), so adding a built-in file
/// reserves its id without editing this list.
pub const RESERVED_CONNECTOR_IDS: [&str; 2] = ["microsoft-365", "linear"];

/// Longest connector id: the id is also the id of a member's MCP connection.
pub const MAX_CONNECTOR_ID_LEN: usize = 64;

/// Sign-in profiles a connector file may name, as the words in the file.
const KNOWN_PROFILES: [&str; 5] = ["entra", "discover", "google", "slack", "oauth"];

const SECRET_REFUSED: &str = "the connector asks for a client secret: a marketplace connector must use `clientSecret: none` (or `org_vault` with `client: org`)";

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
    check_public_dns_host("mcp.url", host)?;
    Ok(host.to_string())
}

/// A full lowercase public DNS name: at least two labels of `[a-z0-9-]` (1-63, no edge hyphen),
/// last label not all digits, at most 253 characters, no private-network suffix. `what` names the
/// field in the error.
pub fn check_public_dns_host(what: &str, host: &str) -> Result<(), String> {
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
        return Err(format!(
            "{what} must name a server by a full lowercase DNS name (no IP address, no bare name)"
        ));
    }
    let dotted = format!(".{host}");
    if INTERNAL_SUFFIXES.iter().any(|s| dotted.ends_with(s)) {
        return Err(format!("{what} host `{host}` names a private network"));
    }
    Ok(())
}

/// The label a template is filled with to check a host's DNS form.
const SAMPLE_LABEL: &str = "x";

/// `host` with a leading `{subdomain}` filled by [`SAMPLE_LABEL`]; any other host unchanged.
fn sample_fill(host: &str) -> String {
    match host.strip_prefix("{subdomain}") {
        Some(rest) => format!("{SAMPLE_LABEL}{rest}"),
        None => host.to_string(),
    }
}

/// `url` with a leading `{subdomain}` host label filled by [`SAMPLE_LABEL`].
fn sample_fill_url(url: &str) -> String {
    match url.strip_prefix("https://{subdomain}") {
        Some(rest) => format!("https://{SAMPLE_LABEL}{rest}"),
        None => url.to_string(),
    }
}

/// Binding rules for a marketplace connector (CN-6.1): 1 to 8 bindings, each with 1 to 4 hosts that
/// are exact public DNS names or `{subdomain}.<suffix>` (the file validator ties the suffix to
/// `tenant`), the access token as a bearer, and the placeholder `vamsec_<connector id>_<name>`.
fn check_marketplace_bindings(c: &Connector, id: &str) -> Result<(), ConnectorError> {
    let Some(bs) = &c.bindings else {
        return Ok(());
    };
    if bs.len() > 8 {
        return bad("a marketplace connector may declare at most 8 bindings");
    }
    for b in bs {
        let name_ok = !b.name.is_empty()
            && b.name.len() <= 32
            && b.name
                .bytes()
                .all(|x| x.is_ascii_lowercase() || x.is_ascii_digit() || x == b'_');
        if !name_ok {
            return bad(format!(
                "binding name `{}` must be 1 to 32 characters of a-z, 0-9 or '_'",
                b.name
            ));
        }
        if b.inject != Inject::Bearer {
            return bad(format!(
                "binding `{}`: only `inject: bearer` is available to a marketplace connector",
                b.name
            ));
        }
        let want = format!("vamsec_{id}_{}", b.name);
        if b.placeholder != want {
            return bad(format!(
                "binding `{}` placeholder `{}` must be `{want}` (`vamsec_<connector id>_<binding name>`): a connector cannot use another's placeholder",
                b.name, b.placeholder
            ));
        }
        if b.hosts.len() > 4 {
            return bad(format!("binding `{}` may name at most 4 hosts", b.name));
        }
        for h in &b.hosts {
            check_public_dns_host(&format!("binding `{}` host", b.name), &sample_fill(h))
                .map_err(ConnectorError::Invalid)?;
        }
    }
    Ok(())
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
            if !matches!(p.profile, OAuthProfile::Discover | OAuthProfile::Oauth) {
                let name = match p.profile {
                    OAuthProfile::Entra => "entra",
                    OAuthProfile::Google => "google",
                    OAuthProfile::Slack => "slack",
                    OAuthProfile::Discover => "discover",
                    OAuthProfile::Oauth => "oauth",
                };
                return bad(format!(
                    "OAuth profile `{name}` is not available to a marketplace connector yet: use `discover` with a remote MCP server, or `oauth` with REST bindings"
                ));
            }
            if p.profile == OAuthProfile::Oauth {
                if p.client != ClientKind::Org {
                    return bad("profile `oauth` needs `client: org`: the org's admin supplies the client id at Enable");
                }
                if let Some(e) = &p.endpoints {
                    for (n, u) in [
                        ("endpoints.authorize", Some(&e.authorize)),
                        ("endpoints.token", Some(&e.token)),
                        ("endpoints.revoke", e.revoke.as_ref()),
                    ] {
                        if let Some(u) = u {
                            let h = url_host(n, u).map_err(ConnectorError::Invalid)?;
                            check_public_dns_host(&format!("{n} host"), &sample_fill(h))
                                .map_err(ConnectorError::Invalid)?;
                        }
                    }
                }
            }
            if p.client_secret == ClientSecret::SqurellRelay {
                return bad(SECRET_REFUSED);
            }
            if p.org_consent != OrgConsent::None {
                return bad("orgConsent must be `none` for profile `discover` or `oauth`");
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
    let rest = matches!(&c.provider, Provider::Oauth2(p) if p.profile == OAuthProfile::Oauth);
    if rest {
        if c.mcp.is_some() {
            return bad("profile `oauth` is for REST bindings: a connector with `mcp` uses profile `discover`");
        }
        if c.bindings.is_none() {
            return bad("profile `oauth` needs `bindings`");
        }
        return check_marketplace_bindings(c, id);
    }
    if c.bindings.is_some() {
        return bad("a marketplace connector with `bindings` must use profile `oauth`; `discover` is remote MCP only");
    }
    let Some(mcp) = &c.mcp else {
        return bad("a marketplace connector needs an mcp server");
    };
    check_mcp_server_url(&sample_fill_url(&mcp.url)).map_err(ConnectorError::Invalid)?;
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
            if !matches!(secret.as_str(), Some("none" | "org_vault")) {
                return bad(SECRET_REFUSED);
            }
            if secret.as_str() == Some("org_vault")
                && p.get("client").and_then(Value::as_str) != Some("org")
            {
                return bad(
                    "clientSecret `org_vault` needs `client: org`: the secret is the org's own client's",
                );
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

    fn org_good() -> Value {
        let mut v = good();
        v["provider"] = json!({ "kind": "oauth2", "profile": "discover", "client": "org",
                                "clientSecret": "org_vault", "scopes": [], "orgConsent": "none" });
        v
    }

    #[test]
    fn an_admin_supplied_client_passes_with_or_without_a_secret() {
        let c = parse_marketplace_connector(&org_good().to_string()).unwrap();
        let s = crate::package::summarize_connector(&c);
        let cs = s.connector.unwrap();
        assert_eq!(cs.client.as_deref(), Some("org"));
        assert_eq!(cs.client_secret, "org_vault");
        assert_eq!(
            cs.client_text.as_deref(),
            Some("admin-supplied client (with a secret)")
        );
        let mut v = org_good();
        v["provider"]["clientSecret"] = json!("none");
        let c = parse_marketplace_connector(&v.to_string()).unwrap();
        let cs = crate::package::summarize_connector(&c).connector.unwrap();
        assert_eq!(cs.client_text.as_deref(), Some("admin-supplied client"));
        // A publisher client has neither field (its summary digest is unchanged).
        let c = parse_marketplace_connector(&good().to_string()).unwrap();
        let cs = crate::package::summarize_connector(&c).connector.unwrap();
        assert!(cs.client.is_none() && cs.client_text.is_none());
    }

    #[test]
    fn a_vault_secret_without_an_org_client_names_the_client() {
        let mut v = org_good();
        v["provider"].as_object_mut().unwrap().remove("client");
        assert!(err(v).contains("`client: org`"));
        let mut v = org_good();
        v["provider"]["client"] = json!("publisher");
        assert!(err(v).contains("`client: org`"));
    }

    #[test]
    fn an_org_client_still_refuses_the_relay_a_client_id_and_other_profiles() {
        let mut v = org_good();
        v["provider"]["clientSecret"] = json!("squrell_relay");
        assert!(err(v).contains("client secret"));
        let mut v = org_good();
        v["provider"]["clientId"] = json!("abc");
        assert!(err(v).contains("clientId"));
        for profile in ["google", "slack"] {
            let mut v = org_good();
            v["provider"]["profile"] = json!(profile);
            assert!(err(v).contains("not available"), "{profile}");
        }
        let mut v = org_good();
        v["provider"]["clientSecret"] = json!("hunter2");
        assert!(err(v).contains("client secret"));
    }

    #[test]
    fn the_box_connector_file_passes() {
        let raw = include_str!("../tests/fixtures/box-connector.json");
        let c = parse_marketplace_connector(raw).unwrap();
        assert_eq!(connector_id_of_urn(&c.urn), "box");
        let cs = crate::package::summarize_connector(&c).connector.unwrap();
        assert_eq!(cs.client.as_deref(), Some("org"));
        assert_eq!(cs.mcp_host, "mcp.box.com");
        assert_eq!(cs.tools.len(), 7);
    }

    #[test]
    fn the_built_in_files_are_not_marketplace_connectors() {
        for (name, raw) in BUILTIN_CONNECTOR_FILES {
            assert!(parse_marketplace_connector(raw).is_err(), "{name}");
        }
    }

    fn zendesk() -> Value {
        serde_json::from_str(include_str!(
            "../tests/fixtures/zendesk-shaped-connector.json"
        ))
        .unwrap()
    }

    #[test]
    fn a_company_address_rest_connector_passes_and_is_summarized() {
        let c = parse_marketplace_connector(&zendesk().to_string()).unwrap();
        let cs = crate::package::summarize_connector(&c).connector.unwrap();
        assert_eq!(cs.profile, "oauth");
        assert_eq!(cs.client.as_deref(), Some("org"));
        assert_eq!(cs.client_secret, "none");
        assert_eq!(cs.hosts, vec!["{subdomain}.zendesk.com"]);
        assert_eq!(cs.bindings[0].name, "api");
        assert_eq!(cs.bindings[0].hosts, vec!["{subdomain}.zendesk.com"]);
        assert_eq!(cs.mcp_host, "");
        let t = cs.tenant.unwrap();
        assert_eq!(
            (t.label.as_str(), t.suffix.as_str()),
            ("Zendesk address", "zendesk.com")
        );
        assert_eq!(t.text, "company address: Zendesk address (…zendesk.com)");
        assert!(cs
            .endpoints
            .unwrap()
            .token
            .starts_with("https://{subdomain}."));
        // An MCP connector's summary JSON does not gain the REST fields.
        let b = parse_marketplace_connector(&good().to_string()).unwrap();
        let v = serde_json::to_value(crate::package::summarize_connector(&b).connector.unwrap())
            .unwrap();
        for k in ["bindings", "endpoints", "tenant"] {
            assert!(v.get(k).is_none(), "{k}");
        }
        assert_eq!(v["mcpHost"], "mcp.bugwatch.example.org");
    }

    #[test]
    fn a_host_outside_the_suffix_names_the_suffix() {
        let mut v = zendesk();
        v["bindings"][0]["hosts"] = json!(["{subdomain}.elsewhere.example"]);
        assert!(err(v).contains("suffix"));
        let mut v = zendesk();
        v["provider"]["endpoints"]["token"] = json!("https://{subdomain}.elsewhere.example/t");
        assert!(err(v).contains("suffix"));
    }

    #[test]
    fn the_template_is_only_the_whole_first_label() {
        for host in [
            "api.{subdomain}.zendesk.com",
            "a{subdomain}.zendesk.com",
            "{subdomain}a.zendesk.com",
            "{subdomain}",
            "{subdomain}.{subdomain}.zendesk.com",
            "*.zendesk.com",
            "{subdomain}.com",
        ] {
            let mut v = zendesk();
            v["bindings"][0]["hosts"] = json!([host]);
            assert!(
                parse_marketplace_connector(&v.to_string()).is_err(),
                "{host}"
            );
        }
        let mut v = zendesk();
        v["provider"]["endpoints"]["token"] = json!("https://{subdomain}.zendesk.com/{subdomain}");
        assert!(err(v).contains("only in its host"));
    }

    #[test]
    fn tenant_and_template_go_together() {
        let mut v = zendesk();
        v.as_object_mut().unwrap().remove("tenant");
        assert!(err(v).contains("no `tenant`"));
        let mut v = zendesk();
        v["provider"]["endpoints"] = json!({
            "authorize": "https://login.zendesk.com/a", "token": "https://login.zendesk.com/t" });
        v["bindings"][0]["hosts"] = json!(["api.zendesk.com"]);
        assert!(err(v).contains("no host uses"));
        let mut v = zendesk();
        v["tenant"]["name"] = json!("tenant");
        assert!(err(v).contains("tenant.name"));
        let mut v = zendesk();
        v["tenant"]["suffix"] = json!("com");
        v["bindings"][0]["hosts"] = json!(["{subdomain}.com"]);
        v["provider"]["endpoints"] = json!({
            "authorize": "https://{subdomain}.com/a", "token": "https://{subdomain}.com/t" });
        assert!(err(v).contains("first label"));
        let mut v = zendesk();
        v["tenant"]["suffix"] = json!("zendesk.internal");
        v["bindings"][0]["hosts"] = json!(["{subdomain}.zendesk.internal"]);
        v["provider"]["endpoints"] = json!({
            "authorize": "https://{subdomain}.zendesk.internal/a",
            "token": "https://{subdomain}.zendesk.internal/t" });
        assert!(err(v).contains("private network"));
    }

    #[test]
    fn oauth_needs_endpoints_an_org_client_and_no_secret() {
        let mut v = zendesk();
        v["provider"].as_object_mut().unwrap().remove("endpoints");
        assert!(err(v).contains("endpoints"));
        let mut v = zendesk();
        v["provider"]["clientSecret"] = json!("org_vault");
        assert!(err(v).contains("secret"));
        let mut v = zendesk();
        v["provider"].as_object_mut().unwrap().remove("client");
        assert!(err(v).contains("client"));
        let mut v = zendesk();
        v["provider"]["client"] = json!("org");
        v["provider"]["clientId"] = json!("abc");
        assert!(err(v).contains("clientId"));
        for (k, bad) in [
            ("authorize", "http://{subdomain}.zendesk.com/a"),
            ("token", "https://{subdomain}.zendesk.com:8443/t"),
            ("token", "https://{subdomain}.zendesk.com/t?x=1"),
            ("authorize", "https://user@{subdomain}.zendesk.com/a"),
            ("token", "https://{subdomain}.zendesk.com/t#f"),
        ] {
            let mut v = zendesk();
            v["provider"]["endpoints"][k] = json!(bad);
            assert!(
                parse_marketplace_connector(&v.to_string()).is_err(),
                "{bad}"
            );
        }
        // `endpoints` belong to profile `oauth` only.
        let mut v = good();
        v["provider"]["endpoints"] = json!({ "authorize": "https://a.example.org/a",
                                              "token": "https://a.example.org/t" });
        assert!(err(v).contains("only for profile"));
        // `discover` with bindings is refused.
        let mut v = zendesk();
        v["provider"]["profile"] = json!("discover");
        v["provider"].as_object_mut().unwrap().remove("endpoints");
        assert!(parse_marketplace_connector(&v.to_string()).is_err());
    }

    #[test]
    fn a_binding_placeholder_is_the_connectors_own() {
        for ph in [
            "vamsec_m365_graph",
            "vamsec_zendesk_other",
            "vamsec_zendesk_",
            "zendesk_api",
        ] {
            let mut v = zendesk();
            v["bindings"][0]["placeholder"] = json!(ph);
            assert!(err(v).contains("placeholder"), "{ph}");
        }
        let mut v = zendesk();
        v["bindings"][0]["name"] = json!("Api");
        v["bindings"][0]["placeholder"] = json!("vamsec_zendesk_Api");
        assert!(err(v).contains("binding name"));
    }

    #[test]
    fn bindings_are_bounded_and_bearer_only() {
        let mut v = zendesk();
        v["bindings"][0]["inject"] = json!("header");
        assert!(err(v).contains("bearer"));
        let mut v = zendesk();
        v["bindings"][0]["hosts"] = json!([
            "{subdomain}.zendesk.com",
            "a.zendesk.com",
            "b.zendesk.com",
            "c.zendesk.com",
            "d.zendesk.com"
        ]);
        assert!(err(v).contains("at most 4"));
        let mut v = zendesk();
        let one = v["bindings"][0].clone();
        let many: Vec<Value> = (0..9)
            .map(|i| {
                let mut b = one.clone();
                b["name"] = json!(format!("b{i}"));
                b["placeholder"] = json!(format!("vamsec_zendesk_b{i}"));
                b
            })
            .collect();
        v["bindings"] = json!(many);
        assert!(err(v).contains("at most 8"));
        // Exact hosts next to the template are fine; an IP is not.
        let mut v = zendesk();
        v["bindings"][0]["hosts"] = json!(["{subdomain}.zendesk.com", "api.zendesk.com"]);
        assert!(parse_marketplace_connector(&v.to_string()).is_ok());
        v["bindings"][0]["hosts"] = json!(["10.0.0.1"]);
        assert!(parse_marketplace_connector(&v.to_string()).is_err());
    }

    #[test]
    fn a_discover_connector_may_template_its_mcp_url() {
        let mut v = good();
        v["tenant"] = json!({ "name": "subdomain", "suffix": "bugwatch.example.org",
                              "label": "Bugwatch address" });
        v["mcp"]["url"] = json!("https://{subdomain}.bugwatch.example.org/mcp");
        assert!(parse_marketplace_connector(&v.to_string()).is_ok());
        v["mcp"]["url"] = json!("https://{subdomain}.elsewhere.example.org/mcp");
        assert!(err(v).contains("suffix"));
    }

    #[test]
    fn the_profile_list_names_oauth() {
        let mut v = good();
        v["provider"]["profile"] = json!("generic");
        let e = err(v);
        assert!(
            e.contains("unknown OAuth profile") && e.contains("oauth"),
            "{e}"
        );
    }
}
