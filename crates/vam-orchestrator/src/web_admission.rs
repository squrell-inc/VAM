//! Open-web admission for HSP tools (AR-1.4) and the per-agent web switch
//! (AR-1.7).
//!
//! A tool exported with `destination_policy = PUBLIC_WEB` may name any public
//! host in its `url` argument. `vamd` admits such a call only when, in order:
//!
//! 1. the URL is well formed (`invalid_url`);
//! 2. it is in the task's context set, normalized the same way the set is
//!    filled (`not_in_context`);
//! 3. the pinned agent's `internet` rules allow the host, the same decision the
//!    guest egress proxy makes (`blocked`);
//! 4. the port is 80 or 443 and every address the host resolves to passes the
//!    strict SSRF classifier. A DNS failure fails closed (`blocked`).
//!
//! The broker pins DNS again on every hop. That stays the real guard, since
//! `vamd` and the broker resolve separately.
//!
//! Every refusal is a [`Refusal`]: a code the model can act on and one line of
//! advice. Refusals never echo model-supplied text except the host name.

use std::sync::Arc;
use std::time::Duration;

use prost_types::value::Kind;
use prost_types::{Struct, Value};
use url::Url;
use vam_network::{EgressAuthorization, EgressAuthorizer};
use vam_security::ssrf_guard::{
    is_restricted_ip_strict, HostResolver, PinError, PinnedUrl, SystemResolver,
};
use vam_spec::v1::AgentSpec;

use crate::workflow::SqliteWorkflowStore;

/// How long DNS may take before the call is refused.
const DNS_TIMEOUT: Duration = Duration::from_secs(5);

/// The argument that names a PUBLIC_WEB tool's destination.
pub const DESTINATION_ARG: &str = "url";

/// A typed refusal: `<tool>: <code>: <advice>` reaches the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: &'static str,
    pub advice: String,
}

impl Refusal {
    fn new(code: &'static str, advice: impl Into<String>) -> Self {
        Self {
            code,
            advice: advice.into(),
        }
    }

    /// The message the model sees, after `tool_error: `.
    pub fn message(&self, tool: &str) -> String {
        format!("{tool}: {}: {}", self.code, self.advice)
    }
}

/// What admission needs besides the store: who is calling, and how to resolve
/// DNS. Tests inject a resolver; production uses the operating system's.
#[derive(Clone)]
pub struct AdmissionEnv {
    pub session_id: String,
    pub agent_urn: String,
    pub resolver: Arc<dyn HostResolver + Send + Sync>,
}

impl AdmissionEnv {
    pub fn new(session_id: impl Into<String>, agent_urn: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            agent_urn: agent_urn.into(),
            resolver: Arc::new(SystemResolver),
        }
    }

    /// No session: any web call is refused (fail closed).
    pub fn none() -> Self {
        Self::new("", "")
    }

    pub fn with_resolver(mut self, resolver: Arc<dyn HostResolver + Send + Sync>) -> Self {
        self.resolver = resolver;
        self
    }
}

impl std::fmt::Debug for AdmissionEnv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdmissionEnv")
            .field("session_id", &self.session_id)
            .field("agent_urn", &self.agent_urn)
            .finish()
    }
}

// ------------------------------------------------------------ web switch

/// `permissions.web.enabled`. An agent without `permissions` (legacy) is off.
pub fn agent_web_enabled(agent: &AgentSpec) -> bool {
    agent
        .permissions
        .as_ref()
        .and_then(|permissions| permissions.web.as_ref())
        .is_some_and(|web| web.enabled)
}

/// Whether the session's pinned agent has web on. Any failure to load the
/// agent is off.
pub fn session_web_enabled(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
) -> bool {
    if session_id.is_empty() {
        return false;
    }
    let principal = match principal_urn.filter(|urn| !urn.is_empty()) {
        Some(urn) => Some(urn.to_string()),
        None => store.find_agent_urn_for_session(session_id).ok().flatten(),
    };
    match crate::policy_legacy::load_session_agent(store, session_id, principal.as_deref()) {
        Ok(session) => session.agent.as_ref().is_some_and(agent_web_enabled),
        Err(_) => false,
    }
}

// ------------------------------------------------------- `agent check`

/// Advisory findings for `agent check`: web on with no web broker, and
/// enabled brokers whose secret bindings no tool export lists.
pub fn check_warnings(store: &SqliteWorkflowStore, agent: &AgentSpec) -> Vec<String> {
    let mut warnings = Vec::new();
    if agent_web_enabled(agent) && !store.web_broker_available() {
        warnings.push(
            "web access is on, but no web broker is registered on this node: web_search and \
             web_fetch will not be offered. Register a broker whose manifest carries the label \
             vam.capability=web."
                .to_string(),
        );
    }
    for broker_id in store.list_enabled_hsp_broker_ids().unwrap_or_default() {
        let Ok(Some(approval)) = store.get_hsp_broker_approval(&broker_id) else {
            continue;
        };
        let unlisted = crate::hsp_registry::unlisted_secret_bindings(&approval.manifest);
        if !unlisted.is_empty() {
            warnings.push(format!(
                "broker `{broker_id}` has secret bindings that no tool lists in required_secrets \
                 ({}): its tools receive no secrets. List each secret on the tools that need it.",
                unlisted.join(", ")
            ));
        }
    }
    warnings
}

// ------------------------------------------------------------ query rule

/// True when `text` holds an http(s) URL that could carry data: a path, a
/// query string, a fragment or credentials. A bare domain or an origin-only
/// URL is fine (owner answer 7).
pub fn text_carries_url_data(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    for prefix in ["https://", "http://"] {
        for (start, _) in lower.match_indices(prefix) {
            let tail = &text[start..];
            let end = tail
                .find(|c: char| {
                    c.is_whitespace() || c.is_control() || matches!(c, '"' | '\'' | '<' | '>' | '`')
                })
                .unwrap_or(tail.len());
            let candidate = tail[..end].trim_end_matches(['.', ',', ';', ')', ']']);
            if let Ok(url) = Url::parse(candidate) {
                if !url.username().is_empty()
                    || url.password().is_some()
                    || url.path() != "/"
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return true;
                }
            }
        }
    }
    false
}

fn any_string_leaf(args: &Struct, skip_top: Option<&str>, pred: &dyn Fn(&str) -> bool) -> bool {
    fn value(v: &Value, pred: &dyn Fn(&str) -> bool) -> bool {
        match v.kind.as_ref() {
            Some(Kind::StringValue(text)) => pred(text),
            Some(Kind::StructValue(nested)) => nested.fields.values().any(|v| value(v, pred)),
            Some(Kind::ListValue(list)) => list.values.iter().any(|v| value(v, pred)),
            _ => false,
        }
    }
    args.fields
        .iter()
        .filter(|(key, _)| Some(key.as_str()) != skip_top)
        .any(|(_, v)| value(v, pred))
}

/// The query rule: no string argument may hold a URL that carries a path or
/// query. `skip` names the destination argument of a PUBLIC_WEB tool, which
/// has its own checks.
pub fn check_query_args(args: Option<&Struct>, skip: Option<&str>) -> Result<(), Refusal> {
    let Some(args) = args else {
        return Ok(());
    };
    if any_string_leaf(args, skip, &text_carries_url_data) {
        return Err(Refusal::new(
            "invalid_query",
            "an argument contains a URL with a path or query string. To read a page, call \
             web_fetch with its URL. Bare domains such as example.com are fine in a query.",
        ));
    }
    Ok(())
}

// ------------------------------------------------------- public-web gate

/// Parse and shape-check the destination. `invalid_url` for anything that is
/// not a plain http(s) URL the context set could hold.
pub fn parse_destination(args: Option<&Struct>) -> Result<(String, Url), Refusal> {
    let invalid = || {
        Refusal::new(
            "invalid_url",
            "pass `url` as one http or https URL without a username or password, at most \
             2048 characters.",
        )
    };
    let raw = args
        .and_then(|a| a.fields.get(DESTINATION_ARG))
        .and_then(|v| match v.kind.as_ref() {
            Some(Kind::StringValue(text)) => Some(text.as_str()),
            _ => None,
        })
        .ok_or_else(invalid)?;
    let normalized = crate::web_sources::normalize_url(raw).ok_or_else(invalid)?;
    let url = Url::parse(&normalized).map_err(|_| invalid())?;
    Ok((raw.to_string(), url))
}

/// The agent's internet rules, as the guest egress proxy applies them.
pub fn check_agent_rule(authorizer: &dyn EgressAuthorizer, host: &str) -> Result<(), Refusal> {
    match authorizer.authorize(host) {
        EgressAuthorization::Allow { .. } => Ok(()),
        EgressAuthorization::Deny { rule } => Err(Refusal::new(
            "blocked",
            format!(
                "{rule} does not allow {host}. Use a different source, or ask the user to \
                 change the agent's internet permissions."
            ),
        )),
    }
}

/// Port, then DNS and the strict address classifier. Fails closed.
pub async fn check_destination(
    resolver: &Arc<dyn HostResolver + Send + Sync>,
    url: &Url,
) -> Result<(), Refusal> {
    let host = url.host_str().unwrap_or_default().to_string();
    if !matches!(url.port_or_known_default(), Some(80) | Some(443)) {
        return Err(Refusal::new(
            "blocked",
            format!("{host}: only ports 80 and 443 can be fetched."),
        ));
    }
    let resolver = resolver.clone();
    let target = url.to_string();
    let pin = tokio::time::timeout(
        DNS_TIMEOUT,
        tokio::task::spawn_blocking(move || {
            PinnedUrl::validate_and_pin_with_policy(
                &target,
                &[],
                resolver.as_ref(),
                &is_restricted_ip_strict,
            )
        }),
    )
    .await;
    match pin {
        Ok(Ok(Ok(_))) => Ok(()),
        Ok(Ok(Err(PinError::Rejected(_)))) => Err(Refusal::new(
            "blocked",
            format!(
                "{host} is a private, local or otherwise restricted address. Only public web \
                 pages can be fetched."
            ),
        )),
        _ => Err(Refusal::new(
            "blocked",
            format!(
                "{host} could not be resolved safely (DNS failed or timed out). Try another \
                 source."
            ),
        )),
    }
}

/// The full admission for a PUBLIC_WEB call.
pub async fn admit_public_web(
    store: &SqliteWorkflowStore,
    env: &AdmissionEnv,
    run_id: &str,
    args: Option<&Struct>,
) -> Result<(), Refusal> {
    let (raw, url) = parse_destination(args)?;
    if !store.run_context_contains(run_id, &raw) {
        return Err(Refusal::new(
            "not_in_context",
            "this URL has not appeared in the user's messages or in an earlier tool result of \
             this task. Fetch only URLs you were given or that a search or fetch returned; do \
             not build or edit URLs yourself.",
        ));
    }
    let host = url.host_str().unwrap_or_default().to_string();
    let authorizer = crate::brokers::SessionEgressAuthorizer::build(
        store,
        &env.session_id,
        Some(&env.agent_urn)
            .filter(|urn| !urn.is_empty())
            .map(|s| s.as_str()),
        crate::security::SessionPolicyMode::Permissions,
        &[],
    );
    check_agent_rule(&authorizer, &host)?;
    check_destination(&env.resolver, &url).await
}

/// `web_disabled`: a web broker was called by an agent with web off.
pub fn web_disabled() -> Refusal {
    Refusal::new(
        "web_disabled",
        "web access is off for this agent. It can be turned on in the agent's permissions \
         (`vam agent permissions web <agent> on`).",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;
    use std::net::{IpAddr, SocketAddr};

    struct Fixed(Vec<IpAddr>);
    impl HostResolver for Fixed {
        fn lookup(&self, _host: &str, port: u16) -> io::Result<Vec<SocketAddr>> {
            Ok(self.0.iter().map(|ip| SocketAddr::new(*ip, port)).collect())
        }
    }
    struct Failing;
    impl HostResolver for Failing {
        fn lookup(&self, _host: &str, _port: u16) -> io::Result<Vec<SocketAddr>> {
            Err(io::Error::other("no dns"))
        }
    }
    fn resolver(
        r: impl HostResolver + Send + Sync + 'static,
    ) -> Arc<dyn HostResolver + Send + Sync> {
        Arc::new(r)
    }
    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }
    fn args(v: serde_json::Value) -> Struct {
        crate::playbook_activation::json_to_struct(&v).unwrap_or_default()
    }

    #[test]
    fn urls_with_data_are_found_and_bare_domains_are_not() {
        assert!(text_carries_url_data("see https://evil.example/p?secret=1"));
        assert!(text_carries_url_data("HTTP://evil.example/path"));
        assert!(text_carries_url_data("x https://u:p@evil.example"));
        assert!(text_carries_url_data("https://evil.example#frag"));
        assert!(text_carries_url_data("https://evil.example?q=1"));
        assert!(!text_carries_url_data(
            "rust 1.80 release notes example.com"
        ));
        assert!(!text_carries_url_data("what is at https://example.com"));
        assert!(!text_carries_url_data("https://example.com/ is down"));
        assert!(!text_carries_url_data("no urls here"));
    }

    #[test]
    fn query_rule_walks_nested_strings_and_skips_the_destination() {
        let bad =
            args(serde_json::json!({"query": "a", "include_domains": ["https://x.example/p"]}));
        assert_eq!(
            check_query_args(Some(&bad), None).unwrap_err().code,
            "invalid_query"
        );
        let fetch = args(serde_json::json!({"url": "https://x.example/p?q=1", "focus": "pricing"}));
        assert!(check_query_args(Some(&fetch), Some("url")).is_ok());
        let focus =
            args(serde_json::json!({"url": "https://x.example/", "focus": "https://y.example/a"}));
        assert!(check_query_args(Some(&focus), Some("url")).is_err());
        assert!(check_query_args(None, None).is_ok());
    }

    #[test]
    fn destination_shape_is_checked_before_anything_else() {
        let ok = args(serde_json::json!({"url": "HTTPS://Example.COM:443/a#x"}));
        let (_, url) = parse_destination(Some(&ok)).unwrap();
        assert_eq!(url.as_str(), "https://example.com/a");
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"url": 5}),
            serde_json::json!({"url": "ftp://example.com/a"}),
            serde_json::json!({"url": "https://u:p@example.com/a"}),
            serde_json::json!({"url": "not a url"}),
            serde_json::json!({"url": format!("https://example.com/{}", "a".repeat(2100))}),
        ] {
            let args = args(bad);
            assert_eq!(
                parse_destination(Some(&args)).unwrap_err().code,
                "invalid_url"
            );
        }
        assert_eq!(parse_destination(None).unwrap_err().code, "invalid_url");
    }

    #[tokio::test]
    async fn destination_checks_port_dns_and_addresses_and_fails_closed() {
        let public = resolver(Fixed(vec![ip("93.184.216.34")]));
        let url = |s: &str| Url::parse(s).unwrap();
        assert!(check_destination(&public, &url("https://example.com/a"))
            .await
            .is_ok());
        assert!(check_destination(&public, &url("http://example.com:80/a"))
            .await
            .is_ok());
        let port = check_destination(&public, &url("https://example.com:8080/")).await;
        assert_eq!(port.unwrap_err().code, "blocked");
        for private in [
            "127.0.0.1",
            "10.0.0.5",
            "169.254.169.254",
            "100.100.100.200",
            "192.168.1.1",
        ] {
            let r = resolver(Fixed(vec![ip(private)]));
            let err = check_destination(&r, &url("https://example.com/"))
                .await
                .unwrap_err();
            assert_eq!(err.code, "blocked", "{private}");
        }
        // One private answer among public ones is still refused.
        let mixed = resolver(Fixed(vec![ip("93.184.216.34"), ip("10.0.0.1")]));
        assert!(check_destination(&mixed, &url("https://example.com/"))
            .await
            .is_err());
        // DNS failure and an empty answer fail closed.
        assert!(
            check_destination(&resolver(Failing), &url("https://example.com/"))
                .await
                .is_err()
        );
        assert!(
            check_destination(&resolver(Fixed(vec![])), &url("https://example.com/"))
                .await
                .is_err()
        );
        // IP literals never reach the resolver.
        assert!(
            check_destination(&resolver(Failing), &url("http://127.0.0.1/"))
                .await
                .is_err()
        );
        assert!(check_destination(&resolver(Failing), &url("http://[::1]/"))
            .await
            .is_err());
        assert!(
            check_destination(&resolver(Failing), &url("http://93.184.216.34/"))
                .await
                .is_ok()
        );
    }

    #[test]
    fn agent_rule_denial_names_the_rule_and_the_host() {
        struct Deny;
        impl EgressAuthorizer for Deny {
            fn authorize(&self, _host: &str) -> EgressAuthorization {
                EgressAuthorization::Deny {
                    rule: "agent rule internet:pastebin.com".into(),
                }
            }
        }
        let err = check_agent_rule(&Deny, "pastebin.com").unwrap_err();
        assert_eq!(err.code, "blocked");
        assert!(err.advice.contains("agent rule internet:pastebin.com"));
        assert!(err.advice.contains("pastebin.com"));
        assert!(err.message("web_fetch").starts_with("web_fetch: blocked: "));
    }

    fn web_manifest(list_secret: bool) -> vam_spec::v1::CapabilityManifest {
        let mut labels = std::collections::HashMap::new();
        labels.insert("vam.capability".to_string(), "web".to_string());
        vam_spec::v1::CapabilityManifest {
            broker_id: "vam.web".to_string(),
            trust_tier: 2,
            secret_bindings: vec![vam_spec::v1::ManifestSecretBinding {
                logical_name: "brave-api-key".to_string(),
                vault_key: "web/brave".to_string(),
                required: true,
            }],
            tools: vec![vam_spec::v1::ToolExport {
                tool_name: "web_search".to_string(),
                side_effect: 1,
                required_secrets: if list_secret {
                    vec!["brave-api-key".to_string()]
                } else {
                    vec![]
                },
                destination_policy: 0,
                effect: None,
            }],
            labels,
            ..Default::default()
        }
    }

    fn web_agent(enabled: bool) -> AgentSpec {
        AgentSpec {
            permissions: Some(vam_spec::v1::AgentPermissions {
                web: Some(vam_spec::v1::WebPermissions { enabled }),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    #[test]
    fn agent_check_warns_when_web_is_on_and_no_web_broker_exists() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        assert!(check_warnings(&store, &web_agent(false)).is_empty());
        let warnings = check_warnings(&store, &web_agent(true));
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("no web broker is registered"));

        store
            .upsert_hsp_broker_approval(&web_manifest(true))
            .unwrap();
        store
            .replace_hsp_broker_tools(
                "vam.web",
                &[crate::hsp_store::HspBrokerToolSnapshot {
                    tool_name: "web_search".to_string(),
                    adapter_id: "hosted:vam.web:web_search".to_string(),
                    side_effect: 1,
                    description: String::new(),
                    input_schema_json: "{}".to_string(),
                }],
            )
            .unwrap();
        assert!(check_warnings(&store, &web_agent(true)).is_empty());
        // A legacy agent (no permissions) never warns about web.
        assert!(check_warnings(&store, &AgentSpec::default()).is_empty());
    }

    #[test]
    fn agent_check_warns_about_bindings_no_tool_lists() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&web_manifest(false))
            .unwrap();
        let warnings = check_warnings(&store, &AgentSpec::default());
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("`vam.web`"));
        assert!(warnings[0].contains("brave-api-key"));
        assert!(warnings[0].contains("required_secrets"));
        store
            .upsert_hsp_broker_approval(&web_manifest(true))
            .unwrap();
        assert!(check_warnings(&store, &AgentSpec::default()).is_empty());
    }

    /// `scripts/web-broker/manifest.json` is what `scripts/run-web-broker.sh` registers.
    #[test]
    fn web_broker_manifest_file_decodes_validates_and_is_a_web_broker() {
        use vam_spec::v1::{PutHostedBrokerRequest, ToolDestination};
        let text = include_str!("../../../scripts/web-broker/manifest.json");
        let req: PutHostedBrokerRequest =
            vam_spec::encoding::json_to_proto("vam.v1.PutHostedBrokerRequest", text)
                .expect("the manifest file decodes as a PutHostedBrokerRequest");
        assert!(!req.unix_path.is_empty());
        let manifest = req.manifest.expect("manifest");
        assert_eq!(manifest.broker_id, "vam.web");
        assert_eq!(manifest.trust_tier, 2, "FIRST_PARTY");
        assert_eq!(manifest.egress_domains, ["api.search.brave.com"]);
        assert_eq!(
            manifest.labels.get("vam.capability").map(String::as_str),
            Some("web")
        );
        assert_eq!(manifest.secret_bindings.len(), 1);
        assert_eq!(manifest.secret_bindings[0].logical_name, "brave-api-key");
        assert_eq!(manifest.secret_bindings[0].vault_key, "web/brave-api-key");

        let search = manifest
            .tools
            .iter()
            .find(|t| t.tool_name == "web_search")
            .unwrap();
        assert_eq!(search.required_secrets, ["brave-api-key"]);
        assert_eq!(search.destination_policy, ToolDestination::Allowlist as i32);
        let fetch = manifest
            .tools
            .iter()
            .find(|t| t.tool_name == "web_fetch")
            .unwrap();
        assert!(
            fetch.required_secrets.is_empty(),
            "web_fetch gets no secret"
        );
        assert_eq!(fetch.destination_policy, ToolDestination::PublicWeb as i32);
        assert!(crate::hsp_registry::unlisted_secret_bindings(&manifest).is_empty());

        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&manifest)
            .expect("operator approval accepts it");
        assert!(store.hsp_broker_is_web("vam.web"));
    }
}
