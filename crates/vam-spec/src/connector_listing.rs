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
//! | `api_key` provider | exactly one field; the member enters the key at the node, which keeps it pinned to the connector's hosts and attaches it as a bearer or in the one header the binding names: REST `bindings` (`value: api_key`, `inject: bearer`, or `inject: header` with a `headerName` that is a plain header name, never `Authorization`, `Host`, `Cookie`, `Proxy-*` or a framing header) or an `mcp` server, never both |
//! | `entra`, `google`, `slack` | refused: `entra` needs REST bindings (tokens attached to requests the guest makes), the others need a client secret |
//! | `mcp` with an `https` URL on a full DNS name, 1 to 64 vetted tools, no `bindings` | a tool list the admin can read; no token reaches the guest |
//! | names that pass for Squrell or a built-in connector | an Unverified connector must not look verified |
//! | `icon` a plain name | the app must not load an address a publisher chose |
//!
//! Nothing here reads a database or the network. The node adds what depends on it: the URN under
//! the marketplace's own `publisherDomain`, and clashes with other installed connectors.

use serde_json::Value;

use crate::connector::{
    is_placeholder_client_id, parse_connector, url_host, BindingValue, ClientKind, ClientSecret,
    Connector, ConnectorError, Inject, OAuthProfile, OrgConsent, Provider, BUILTIN_CONNECTOR_FILES,
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

/// True when the egress gate cannot tell `a` and `b` apart: it finds a placeholder in a request by
/// substring (`Envelope::screen`), so a request carrying the longer one carries both and is
/// refused. Equal placeholders overlap too. Two bindings whose placeholders overlap cannot both be
/// admitted.
pub fn placeholders_overlap(a: &str, b: &str) -> bool {
    a.contains(b) || b.contains(a)
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

/// The host a connector's MCP URL names, for the capability summary: [`check_mcp_server_url`]'s host,
/// or for a URL whose first label is `{subdomain}` that host with the template kept
/// (`{subdomain}.bugwatch.example.org`). Empty when the URL is not one a marketplace connector may name.
pub fn mcp_summary_host(url: &str) -> String {
    match url.strip_prefix("https://{subdomain}") {
        Some(rest) => check_mcp_server_url(&format!("https://{SAMPLE_LABEL}{rest}"))
            .map(|host| format!("{{subdomain}}{}", &host[SAMPLE_LABEL.len()..]))
            .unwrap_or_default(),
        None => check_mcp_server_url(url).unwrap_or_default(),
    }
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

/// The label a template is filled with to check a host's DNS form: the longest company label the
/// node accepts (63 characters), so a suffix that would make a filled host longer than a DNS name
/// can be is refused here and no host the node fills in is ever too long.
const SAMPLE_LABEL: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";

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
    for (i, b) in bs.iter().enumerate() {
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
        // A key may also go in the one header the binding names (`headerName`, checked by the file
        // validator); a token is a bearer, and nothing is ever put in a query string.
        let allowed = match b.inject {
            Inject::Bearer => true,
            Inject::Header => b.value == BindingValue::ApiKey,
            Inject::Query => false,
        };
        if !allowed {
            return bad(match b.value {
                BindingValue::ApiKey => format!(
                    "binding `{}`: only `inject: bearer` or `inject: header` is available to a marketplace connector",
                    b.name
                ),
                BindingValue::AccessToken => format!(
                    "binding `{}`: only `inject: bearer` is available to a marketplace connector",
                    b.name
                ),
            });
        }
        let want = format!("vamsec_{id}_{}", b.name);
        if b.placeholder != want {
            return bad(format!(
                "binding `{}` placeholder `{}` must be `{want}` (`vamsec_<connector id>_<binding name>`): a connector cannot use another's placeholder",
                b.name, b.placeholder
            ));
        }
        // `vamsec_<id>_<name>` can equal a built-in placeholder (id `m365`, name `graph`), or hold
        // one (`vamsec_m365_graph_x`), and two bindings the gate cannot tell apart are refused by
        // the swap for both (T4): see [`placeholders_overlap`].
        if let Some(x) = crate::connector::builtin_connectors()
            .iter()
            .flat_map(|c| c.bindings.iter().flatten())
            .find(|x| placeholders_overlap(&x.placeholder, &b.placeholder))
        {
            return bad(format!(
                "binding `{}` placeholder `{}` overlaps the Squrell connector's `{}`: a connector cannot take another's placeholder",
                b.name, b.placeholder, x.placeholder
            ));
        }
        // Two of its own bindings (`a` and `a_b`): the request for one would carry both.
        if let Some(x) = bs[..i]
            .iter()
            .find(|x| placeholders_overlap(&x.placeholder, &b.placeholder))
        {
            return bad(format!(
                "bindings `{}` and `{}`: the placeholder `{}` overlaps `{}`, and the gate could not tell them apart",
                x.name, b.name, x.placeholder, b.placeholder
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
        // The provider's own rules (one field, `value: api_key`, bearer) ran in `validate_connector`.
        Provider::ApiKey(_) => {}
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
    if matches!(&c.provider, Provider::ApiKey(_)) && c.bindings.is_some() {
        // REST bindings carrying the member's key (an api_key connector with `mcp` is checked below).
        return check_marketplace_bindings(c, id);
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
            Some("oauth2" | "api_key") | None => {}
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
    fn profiles_without_a_flow_are_refused() {
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
    fn a_binding_cannot_take_a_built_in_placeholder_through_a_matching_id_and_name() {
        // `vamsec_<id>_<name>` is exactly the built-in `vamsec_m365_graph` for id `m365`.
        let mut v = zendesk();
        v["urn"] = json!("urn:vam:connector:connectors.example/pub/m365");
        v["bindings"][0]["name"] = json!("graph");
        v["bindings"][0]["placeholder"] = json!("vamsec_m365_graph");
        let e = err(v);
        assert!(e.contains("Squrell connector's"), "{e}");
    }

    #[test]
    fn a_placeholder_that_overlaps_another_under_substring_matching_is_refused() {
        assert!(placeholders_overlap("vamsec_a_b", "vamsec_a_b_c"));
        assert!(placeholders_overlap("vamsec_a_b_c", "vamsec_a_b"));
        assert!(placeholders_overlap("vamsec_a_b", "vamsec_a_b"));
        assert!(!placeholders_overlap("vamsec_a_b", "vamsec_a_c"));
        // A built-in's placeholder inside this one (id `m365`, name `graph_x`).
        let mut v = zendesk();
        v["urn"] = json!("urn:vam:connector:connectors.example/pub/m365");
        v["bindings"][0]["name"] = json!("graph_x");
        v["bindings"][0]["placeholder"] = json!("vamsec_m365_graph_x");
        let e = err(v);
        assert!(e.contains("Squrell connector's"), "{e}");
        // Two of its own bindings, `api` and `api_v2`.
        let mut v = zendesk();
        let mut two = v["bindings"][0].clone();
        two["name"] = json!("api_v2");
        two["placeholder"] = json!("vamsec_zendesk_api_v2");
        v["bindings"].as_array_mut().unwrap().push(two);
        assert_eq!(v["bindings"][0]["placeholder"], json!("vamsec_zendesk_api"));
        let e = err(v);
        assert!(e.contains("overlaps"), "{e}");
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
    fn a_templated_mcp_host_is_kept_in_the_summary() {
        let mut v = good();
        v["tenant"] = json!({ "name": "subdomain", "suffix": "bugwatch.example.org",
                              "label": "Bugwatch address" });
        v["mcp"]["url"] = json!("https://{subdomain}.bugwatch.example.org/mcp");
        let c = parse_marketplace_connector(&v.to_string()).unwrap();
        let cs = crate::package::summarize_connector(&c).connector.unwrap();
        assert_eq!(cs.mcp_host, "{subdomain}.bugwatch.example.org");
        assert_eq!(cs.hosts, vec!["{subdomain}.bugwatch.example.org"]);
        // An exact host is unchanged.
        let c = parse_marketplace_connector(&good().to_string()).unwrap();
        let cs = crate::package::summarize_connector(&c).connector.unwrap();
        assert_eq!(cs.mcp_host, "mcp.bugwatch.example.org");
        assert_eq!(cs.hosts, vec!["mcp.bugwatch.example.org"]);
    }

    #[test]
    fn a_suffix_that_makes_a_filled_host_longer_than_a_dns_name_is_refused() {
        let label = "a".repeat(63);
        let long = format!("{label}.{label}.{label}.com"); // 195 characters: 63 + 1 + 195 = 259
        let ok = format!("{label}.{label}.com"); // 63 + 1 + 129 = 193
        let with = |suffix: &str| {
            let text = zendesk().to_string().replace("zendesk.com", suffix);
            serde_json::from_str::<Value>(&text).unwrap()
        };
        assert!(parse_marketplace_connector(&with(&ok).to_string()).is_ok());
        let e = err(with(&long));
        assert!(e.contains("DNS name"), "{e}");
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

    // ---- api_key (CN-7.2) -------------------------------------------------------------------

    fn api_key_rest() -> Value {
        json!({
            "urn": "urn:vam:connector:market.example.org/acme/notesdesk",
            "version": "1.0.0",
            "displayName": "Notesdesk",
            "publisher": "Acme Tools",
            "description": "Notes in the member's Notesdesk.",
            "icon": "notesdesk",
            "provider": { "kind": "api_key", "fields": [{ "name": "key", "label": "Notesdesk API key" }] },
            "connection": { "holder": "user", "multiAccount": false, "label": "none" },
            "bindings": [{ "name": "api", "placeholder": "vamsec_notesdesk_api", "value": "api_key",
                           "inject": "bearer", "hosts": ["api.notesdesk.example.org"] }]
        })
    }

    fn api_key_mcp() -> Value {
        let mut v = good();
        v["urn"] = json!("urn:vam:connector:market.example.org/acme/keyed-bugwatch");
        v["provider"] = json!({ "kind": "api_key", "fields": [{ "name": "key", "label": "Bugwatch API key" }] });
        v
    }

    #[test]
    fn rest_and_mcp_api_key_connectors_pass_and_are_summarized_with_the_field_label() {
        for (v, host) in [
            (api_key_rest(), "api.notesdesk.example.org"),
            (api_key_mcp(), "mcp.bugwatch.example.org"),
        ] {
            let c = parse_marketplace_connector(&v.to_string()).unwrap();
            let cs = crate::package::summarize_connector(&c).connector.unwrap();
            assert_eq!(cs.provider, "api_key");
            assert_eq!(cs.fields.len(), 1);
            assert!(cs.fields[0].label.ends_with("API key"));
            assert_eq!(cs.hosts, vec![host]);
            let json = serde_json::to_value(&cs).unwrap();
            assert_eq!(json["provider"], "api_key");
            assert_eq!(json["fields"][0]["label"], cs.fields[0].label.as_str());
            // The name of the field is internal: the summary shows the label only.
            assert!(json["fields"][0].get("name").is_none());
        }
        // A company address works for a REST key connector as it does for an OAuth one.
        let mut v = api_key_rest();
        v["tenant"] = json!({ "name": "subdomain", "suffix": "notesdesk.example.org", "label": "Notesdesk address" });
        v["bindings"][0]["hosts"] = json!(["{subdomain}.notesdesk.example.org"]);
        assert!(parse_marketplace_connector(&v.to_string()).is_ok());
    }

    #[test]
    fn an_api_key_connector_takes_exactly_one_field_and_names_it() {
        for fields in [
            json!([]),
            json!([{ "name": "account", "label": "Account" }, { "name": "key", "label": "Key" }]),
        ] {
            let mut v = api_key_rest();
            v["provider"]["fields"] = fields;
            let e = err(v);
            assert!(
                e.contains("one field") && e.contains("provider.fields"),
                "{e}"
            );
        }
        for (name, label) in [
            ("Key", "Key"),
            ("1key", "Key"),
            ("", "Key"),
            ("key-1", "Key"),
            (&"k".repeat(33), "Key"),
            ("key", ""),
            ("key", "   "),
            ("key", &"x".repeat(65)),
            ("key", "two\nlines"),
        ] {
            let mut v = api_key_rest();
            v["provider"]["fields"] = json!([{ "name": name, "label": label }]);
            let e = err(v);
            assert!(e.contains("provider.fields[0]"), "{name:?} {label:?}: {e}");
        }
        let mut v = api_key_rest();
        v["provider"]["fields"] = json!([{ "name": "k", "label": "x".repeat(64) }]);
        assert!(parse_marketplace_connector(&v.to_string()).is_ok());
    }

    #[test]
    fn a_binding_value_must_match_the_provider() {
        let mut v = api_key_rest();
        v["bindings"][0]["value"] = json!("access_token");
        let e = err(v);
        assert!(e.contains("value: api_key") && e.contains("`api`"), "{e}");
        let mut v = good();
        v.as_object_mut().unwrap().remove("mcp");
        v["provider"] = json!({ "kind": "oauth2", "profile": "oauth", "client": "org",
            "clientSecret": "none", "orgConsent": "none", "scopes": [],
            "endpoints": { "authorize": "https://auth.example.org/a", "token": "https://auth.example.org/t" } });
        v["bindings"] = json!([{ "name": "api", "placeholder": "vamsec_bugwatch_api",
            "value": "api_key", "inject": "bearer", "hosts": ["api.bugwatch.example.org"] }]);
        let e = err(v);
        assert!(
            e.contains("only for an api_key provider") && e.contains("`api`"),
            "{e}"
        );
    }

    #[test]
    fn an_api_key_binding_is_a_bearer_or_a_named_header_never_a_query() {
        let mut v = api_key_rest();
        v["bindings"][0]["inject"] = json!("query");
        let e = err(v);
        assert!(
            e.contains("`inject`") && e.contains("bearer") && e.contains("`api`"),
            "query: {e}"
        );
        // A header binding needs its name, which the file validator checks.
        let mut v = api_key_rest();
        v["bindings"][0]["inject"] = json!("header");
        assert!(err(v).contains("needs a `headerName`"));
        let mut v = api_key_rest();
        v["bindings"][0]["inject"] = json!("header");
        v["bindings"][0]["headerName"] = json!("X-Api-Key");
        v["bindings"][0]["prefix"] = json!("Token ");
        let c = parse_marketplace_connector(&v.to_string()).unwrap();
        assert_eq!(
            c.bindings.unwrap()[0].header_name.as_deref(),
            Some("X-Api-Key")
        );
        // Each name a key is never put in is refused with the import's words, in any letter case.
        for name in [
            "Host",
            "authorization",
            "COOKIE",
            "Proxy-Authorization",
            "proxy-x",
            "Connection",
            "content-length",
            "Transfer-Encoding",
            "Keep-Alive",
            "te",
            "Trailer",
            "Upgrade",
        ] {
            let mut v = api_key_rest();
            v["bindings"][0]["inject"] = json!("header");
            v["bindings"][0]["headerName"] = json!(name);
            let e = err(v);
            assert!(
                e.contains("`headerName`") && e.contains("`api`"),
                "{name}: {e}"
            );
        }
        let mut v = api_key_rest();
        v["bindings"][0]["inject"] = json!("header");
        v["bindings"][0]["headerName"] = json!("X Api Key");
        assert!(err(v).contains("RFC 9110"));
        let mut v = api_key_rest();
        v["bindings"][0]["placeholder"] = json!("vamsec_other_api");
        assert!(err(v).contains("must be `vamsec_notesdesk_api`"));
    }

    #[test]
    fn an_api_key_connector_has_bindings_or_an_mcp_server_never_both() {
        let mut v = api_key_rest();
        v["mcp"] = api_key_mcp()["mcp"].clone();
        assert!(err(v).contains("exactly one of bindings and mcp"));
        let mut v = api_key_rest();
        v.as_object_mut().unwrap().remove("bindings");
        assert!(err(v).contains("exactly one of bindings and mcp"));
        // The MCP rules of a discover connector hold for a key connector's server.
        let mut v = api_key_mcp();
        v["mcp"]["url"] = json!("http://mcp.bugwatch.example.org/mcp");
        assert!(err(v).contains("https"));
        let mut v = api_key_mcp();
        v["mcp"]["tools"] = json!([]);
        assert!(err(v).contains("1 to 64 vetted tools"));
        // Host rules of a REST connector hold too.
        let mut v = api_key_rest();
        v["bindings"][0]["hosts"] = json!(["localhost"]);
        assert!(parse_marketplace_connector(&v.to_string()).is_err());
        // A key connector has no OAuth knobs: the file refuses them as unknown.
        let mut v = api_key_rest();
        v["provider"]["clientSecret"] = json!("none");
        assert!(parse_marketplace_connector(&v.to_string()).is_err());
        let mut v = api_key_rest();
        v["provider"]["clientSecret"] = json!("hunter2");
        assert!(err(v).contains("client secret"));
    }

    #[test]
    fn an_api_key_mcp_file_that_asks_for_a_header_or_adds_bindings_is_refused_at_import() {
        // The MCP connect always sends `Authorization: Bearer <key>`; a file that asks for a
        // header, or names bindings next to its `mcp` server, never reaches the node. Import reads
        // a listing's connector with `parse_marketplace_connector` (file rules, then the limits).
        for extra in [
            json!({ "inject": "header" }),
            json!({ "headerName": "X-Api-Key" }),
        ] {
            let mut v = api_key_mcp();
            for (k, val) in extra.as_object().unwrap() {
                v["mcp"][k] = val.clone();
            }
            assert!(err(v).contains("unknown field"), "{extra}");
        }
        for inject in [
            json!({ "inject": "bearer" }),
            json!({ "inject": "header", "headerName": "X-Api-Key" }),
        ] {
            let mut v = api_key_mcp();
            let mut b = api_key_rest()["bindings"][0].clone();
            for (k, val) in inject.as_object().unwrap() {
                b[k] = val.clone();
            }
            v["bindings"] = json!([b]);
            assert!(
                err(v).contains("exactly one of bindings and mcp"),
                "{inject}"
            );
        }
    }

    #[test]
    fn existing_connector_summaries_do_not_gain_a_fields_key() {
        // The summary of an installed release is digested: an OAuth connector's JSON is exactly
        // what it was before `api_key` was accepted.
        let golden_good = r#"{"kind":"connector","urn":"urn:vam:connector:market.example.org/acme/bugwatch","displayName":"Bugwatch","description":"Issues in Bugwatch.","connector":{"publisher":"Acme Tools","provider":"oauth2","profile":"discover","clientSecret":"none","orgConsent":"none","scopes":["read","write"],"mcpUrl":"https://mcp.bugwatch.example.org/mcp","mcpHost":"mcp.bugwatch.example.org","hosts":["mcp.bugwatch.example.org"],"tools":[{"remote":"list_issues","sideEffect":"read_only"},{"remote":"save_issue","sideEffect":"mutable_external","default":"ask"}]}}"#;
        let golden_zendesk = r#"{"kind":"connector","urn":"urn:vam:connector:connectors.community.example/community/zendesk","displayName":"Zendesk Support","description":"Tickets in the company's own Zendesk, through its REST API and a client the org's admin registered.","connector":{"publisher":"Community Tools Ltd","provider":"oauth2","profile":"oauth","client":"org","clientText":"admin-supplied client","clientSecret":"none","orgConsent":"none","scopes":["read","write"],"hosts":["{subdomain}.zendesk.com"],"tools":[],"bindings":[{"name":"api","hosts":["{subdomain}.zendesk.com"]}],"endpoints":{"authorize":"https://{subdomain}.zendesk.com/oauth/authorizations/new","token":"https://{subdomain}.zendesk.com/oauth/tokens"},"tenant":{"label":"Zendesk address","suffix":"zendesk.com","text":"company address: Zendesk address (…zendesk.com)"}}}"#;
        for (v, golden) in [(good(), golden_good), (zendesk(), golden_zendesk)] {
            let c = parse_marketplace_connector(&v.to_string()).unwrap();
            let got = serde_json::to_string(&crate::package::summarize_connector(&c)).unwrap();
            assert_eq!(got, golden);
        }
        for c in crate::connector::builtin_connectors() {
            let got = serde_json::to_value(crate::package::summarize_connector(&c)).unwrap();
            assert!(got["connector"].get("fields").is_none(), "{}", c.urn);
            // A built-in's bindings are the name and hosts they always were.
            for b in got["connector"]["bindings"]
                .as_array()
                .into_iter()
                .flatten()
            {
                let mut keys: Vec<&str> =
                    b.as_object().unwrap().keys().map(String::as_str).collect();
                keys.sort_unstable();
                assert_eq!(keys, ["hosts", "name"], "{}: {b}", c.urn);
            }
        }
        // A bearer API-key binding serializes as before the header form was summarized.
        let c = parse_marketplace_connector(&api_key_rest().to_string()).unwrap();
        let got = serde_json::to_value(crate::package::summarize_connector(&c)).unwrap();
        assert_eq!(
            got["connector"]["bindings"],
            json!([{ "name": "api", "hosts": ["api.notesdesk.example.org"] }])
        );
    }

    /// The summary digest in `marketplace/import.rs` is the SHA-256 of the summary JSON without
    /// wording, so the JSON's difference is the digest's.
    fn summary_json(v: &Value) -> String {
        let c = parse_marketplace_connector(&v.to_string()).unwrap();
        let mut j = serde_json::to_value(crate::package::summarize_connector(&c)).unwrap();
        fn strip(v: &mut Value) {
            match v {
                Value::Object(m) => {
                    m.remove("displayName");
                    m.remove("description");
                    m.values_mut().for_each(strip);
                }
                Value::Array(a) => a.iter_mut().for_each(strip),
                _ => {}
            }
        }
        strip(&mut j);
        j.to_string()
    }

    #[test]
    fn where_the_key_goes_is_part_of_the_summary_so_moving_it_resets_enable() {
        let bearer = api_key_rest();
        let named = |name: &str, prefix: Option<&str>| {
            let mut v = api_key_rest();
            v["bindings"][0]["inject"] = json!("header");
            v["bindings"][0]["headerName"] = json!(name);
            if let Some(p) = prefix {
                v["bindings"][0]["prefix"] = json!(p);
            }
            v
        };
        let base = summary_json(&bearer);
        let x_key = summary_json(&named("X-Api-Key", None));
        // Bearer to a named header: a different summary.
        assert_ne!(base, x_key);
        assert!(
            x_key.contains(r#""inject":"header""#) && x_key.contains(r#""headerName":"X-Api-Key""#)
        );
        // The header name and the prefix are each part of it.
        let other_name = summary_json(&named("X-Token", None));
        let prefixed = summary_json(&named("X-Api-Key", Some("Token ")));
        let other_prefix = summary_json(&named("X-Api-Key", Some("Key ")));
        let all = [&base, &x_key, &other_name, &prefixed, &other_prefix];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a, b);
            }
        }
        // Wording is not capability: the same placement digests the same.
        let mut reworded = named("X-Api-Key", None);
        reworded["description"] = json!("Another sentence.");
        assert_eq!(x_key, summary_json(&reworded));
    }
}
