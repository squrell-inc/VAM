//! Security floor: MCP connection credentials never reach guest egress.
//!
//! The guest is untrusted. An MCP bearer that the egress proxy would inject
//! for a guessed `vamsec_` placeholder lets guest code call any MCP tool
//! directly, bypassing the per-tool Cedar decision, HITL approval and the
//! pinned tool list (`runtime/mcp.md`: "the guest never sees the token").
//!
//! These tests drive the real vault rows written by `mcp_registry` through the
//! same two steps the per-session egress broker takes: build the session's
//! bindings, then let the egress gate's injection step see a guest request.

use std::collections::HashMap;
use std::sync::Arc;

use secrecy::ExposeSecret;
use vam_network::{phase2_inject_secrets, SecretInjectionPlan};
use vam_orchestrator::mcp_registry::{
    mcp_auth_binding_id, mcp_header_binding_id, mcp_oauth_access_token_binding_id,
    mcp_oauth_client_secret_binding_id, mcp_oauth_refresh_token_binding_id,
    store_mcp_header_credential, store_mcp_oauth_tokens, McpAuthMode, McpOauthMetadata,
    McpOauthRegistrationMethod, McpOauthStatus, McpOauthTokenWrite,
};
use vam_orchestrator::secrets::{
    broker_allowed_domains, SecretKind, SecretScope, SecretsVault, VaultSecretProvider,
};
use vam_orchestrator::users::UserRole;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_spec::v1::EgressRequest;

const MCP_HOST: &str = "mcp.example.com";
const TOKEN_HOST: &str = "auth.example.com";
const ACCESS: &str = "mcp-access-token-value";
const REFRESH: &str = "mcp-refresh-token-value";
const CLIENT_SECRET: &str = "mcp-client-secret-value";
const OWNER: &str = "alice";

fn oauth_meta() -> McpOauthMetadata {
    McpOauthMetadata {
        resource: format!("https://{MCP_HOST}/mcp"),
        authorization_server: format!("https://{TOKEN_HOST}"),
        token_endpoint: format!("https://{TOKEN_HOST}/oauth/token"),
        revocation_endpoint: None,
        client_id: "client-123".into(),
        registration_method: McpOauthRegistrationMethod::Dcr,
        scopes: vec!["mcp:tools".into()],
        expires_at: Some(1_700_000_000),
        status: McpOauthStatus::Connected,
        last_refresh_at: None,
        last_refresh_error: None,
    }
}

fn oauth_fixture(scope: SecretScope) -> (Arc<SqliteWorkflowStore>, Arc<SecretsVault>) {
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    let vault = SecretsVault::for_tests(Arc::clone(&store));
    let owner = if scope == SecretScope::User {
        OWNER
    } else {
        ""
    };
    let conn = store
        .upsert_mcp_connection(
            scope,
            owner,
            "acme",
            &format!("https://{MCP_HOST}/mcp"),
            "",
            &[],
            &[],
            McpAuthMode::Oauth,
            Some(&oauth_meta()),
        )
        .expect("connection");
    store_mcp_oauth_tokens(
        &vault,
        &conn,
        &McpOauthTokenWrite {
            access_token: ACCESS.into(),
            refresh_token: REFRESH.into(),
            client_secret: Some(CLIENT_SECRET.into()),
        },
        None,
        UserRole::OrgAdmin,
    )
    .expect("tokens");
    (store, vault)
}

/// The guest sends `placeholder` as the Authorization header to `host` and the
/// egress gate runs its injection step. Returns the header the upstream would
/// receive.
fn upstream_authorization(
    vault: &Arc<SecretsVault>,
    owner: &str,
    placeholder: &str,
    host: &str,
) -> String {
    let bindings = vault
        .session_egress_bindings_for_caller(owner)
        .expect("bindings");
    let provider = VaultSecretProvider::new(Arc::clone(vault)).with_caller(owner.to_string());
    let mut request = EgressRequest {
        method: "POST".into(),
        url: format!("https://{host}/mcp"),
        headers: HashMap::from([("Authorization".to_string(), placeholder.to_string())]),
        body: Vec::new(),
    };
    // A refusal is as good as no substitution: the upstream never sees a secret.
    let _ = phase2_inject_secrets(
        &mut request,
        &bindings,
        &provider,
        host,
        &SecretInjectionPlan::default(),
    );
    request
        .headers
        .get("Authorization")
        .cloned()
        .unwrap_or_default()
}

fn placeholder(binding_id: &str) -> String {
    format!("vamsec_{}", binding_id.replace(['/', '-'], "_"))
}

#[test]
fn oauth_connection_rows_are_not_session_egress_bindings() {
    for scope in [SecretScope::Org, SecretScope::User] {
        let (_store, vault) = oauth_fixture(scope);
        for bindings in [
            vault.session_egress_bindings().expect("org bindings"),
            vault
                .session_egress_bindings_for_caller(OWNER)
                .expect("caller bindings"),
        ] {
            let ids: Vec<&str> = bindings.iter().map(|b| b.binding_id.as_str()).collect();
            assert!(
                ids.is_empty(),
                "{scope:?}: MCP credentials must not become guest egress bindings, got {ids:?}"
            );
            let domains = broker_allowed_domains(&bindings);
            assert!(
                !domains.iter().any(|d| d == MCP_HOST || d == TOKEN_HOST),
                "{scope:?}: MCP hosts must not enter the guest allowlist, got {domains:?}"
            );
        }
    }
}

#[test]
fn guest_placeholder_for_an_mcp_token_is_not_substituted() {
    let (_store, vault) = oauth_fixture(SecretScope::User);
    for (binding_id, host, secret) in [
        (mcp_oauth_access_token_binding_id("acme"), MCP_HOST, ACCESS),
        (
            mcp_oauth_refresh_token_binding_id("acme"),
            TOKEN_HOST,
            REFRESH,
        ),
        (
            mcp_oauth_client_secret_binding_id("acme"),
            TOKEN_HOST,
            CLIENT_SECRET,
        ),
    ] {
        let sent = placeholder(&binding_id);
        let received = upstream_authorization(&vault, OWNER, &sent, host);
        assert!(
            !received.contains(secret),
            "{binding_id}: the real credential reached {host}"
        );
        assert_eq!(
            received, sent,
            "{binding_id}: the placeholder is left as sent"
        );
    }
}

#[test]
fn static_header_credentials_are_not_guest_injectable() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    let vault = SecretsVault::for_tests(Arc::clone(&store));
    store_mcp_header_credential(
        &vault,
        SecretScope::User,
        OWNER,
        "acme",
        "Authorization",
        "Bearer static-auth-value",
        MCP_HOST,
        None,
        UserRole::Member,
    )
    .expect("auth header");
    store_mcp_header_credential(
        &vault,
        SecretScope::User,
        OWNER,
        "acme",
        "X-Api-Key",
        "static-key-value",
        MCP_HOST,
        None,
        UserRole::Member,
    )
    .expect("api key header");

    let bindings = vault
        .session_egress_bindings_for_caller(OWNER)
        .expect("bindings");
    assert!(
        bindings.is_empty(),
        "static MCP headers must not be guest egress bindings"
    );
    for binding_id in [
        mcp_auth_binding_id("acme"),
        mcp_header_binding_id("acme", "X-Api-Key"),
    ] {
        let received = upstream_authorization(&vault, OWNER, &placeholder(&binding_id), MCP_HOST);
        assert!(
            !received.contains("static-"),
            "{binding_id}: a static MCP credential reached the host"
        );
    }
}

#[test]
fn ordinary_egress_secrets_still_inject() {
    // Control: the fix must not break the guest's own egress credentials.
    let (_store, vault) = oauth_fixture(SecretScope::User);
    vault
        .put_secret_with_owner(
            "hrms/api-token",
            "hrms-secret",
            SecretKind::Egress,
            &["hrms.example.com".to_string()],
            None,
            OWNER,
        )
        .expect("hrms");
    let received = upstream_authorization(
        &vault,
        OWNER,
        &placeholder("hrms/api-token"),
        "hrms.example.com",
    );
    assert!(received.contains("hrms-secret"), "got {received:?}");
    let bindings = vault
        .session_egress_bindings_for_caller(OWNER)
        .expect("bindings");
    assert_eq!(
        broker_allowed_domains(&bindings),
        vec!["hrms.example.com".to_string()]
    );
}

// ---- HSP brokers ---------------------------------------------------------------------------

fn manifest_binding(
    logical: &str,
    vault_key: &str,
    required: bool,
) -> vam_spec::v1::ManifestSecretBinding {
    vam_spec::v1::ManifestSecretBinding {
        logical_name: logical.into(),
        vault_key: vault_key.into(),
        required,
    }
}

#[test]
fn broker_manifest_naming_an_mcp_credential_gets_nothing() {
    use vam_orchestrator::resolve_secrets_for_caller;
    let (_store, vault) = oauth_fixture(SecretScope::User);
    for binding_id in [
        mcp_oauth_access_token_binding_id("acme"),
        mcp_oauth_refresh_token_binding_id("acme"),
        mcp_oauth_client_secret_binding_id("acme"),
    ] {
        for required in [true, false] {
            // Even a manifest whose approved domains cover the pinned host.
            let err = resolve_secrets_for_caller(
                Some(vault.as_ref()),
                OWNER,
                &[manifest_binding("tok", &binding_id, required)],
                &[MCP_HOST.to_string(), TOKEN_HOST.to_string()],
            )
            .expect_err("an MCP credential must not reach a broker");
            assert!(err.contains("not permitted"), "{binding_id}: {err}");
            assert!(
                !err.contains(ACCESS) && !err.contains(REFRESH) && !err.contains(CLIENT_SECRET),
                "the refusal must not carry a credential: {err}"
            );
        }
    }
}

#[test]
fn org_scope_mcp_credential_is_refused_for_a_user_caller_too() {
    use vam_orchestrator::resolve_secrets_for_caller;
    let (_store, vault) = oauth_fixture(SecretScope::Org);
    let err = resolve_secrets_for_caller(
        Some(vault.as_ref()),
        "bob",
        &[manifest_binding(
            "tok",
            &mcp_oauth_access_token_binding_id("acme"),
            true,
        )],
        &[MCP_HOST.to_string()],
    )
    .expect_err("org-scope MCP credential must not reach a broker");
    assert!(err.contains("not permitted"), "{err}");
}

#[test]
fn broker_gets_a_secret_only_inside_its_approved_egress_domains() {
    use vam_orchestrator::resolve_secrets_for_caller;
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    let vault = SecretsVault::for_tests(Arc::clone(&store));
    vault
        .put_secret(
            "crm/api-token",
            "crm-secret",
            SecretKind::Egress,
            &["api.crm.example.com".to_string()],
            None,
        )
        .expect("crm");
    vault
        .put_secret(
            "jira/auth-header",
            "jira-secret",
            SecretKind::Egress,
            &["*.atlassian.net".to_string()],
            None,
        )
        .expect("jira");
    vault
        .put_secret("web/key", "web-secret", SecretKind::Inference, &[], None)
        .expect("unpinned");
    let bindings = |key: &str| vec![manifest_binding("tok", key, true)];
    let resolve = |key: &str, domains: &[&str]| {
        let domains: Vec<String> = domains.iter().map(|d| d.to_string()).collect();
        resolve_secrets_for_caller(Some(vault.as_ref()), OWNER, &bindings(key), &domains)
    };

    // Inside: the exact host, a parent domain, and a wildcard under a parent domain.
    assert!(resolve("crm/api-token", &["api.crm.example.com"]).is_ok());
    assert!(resolve("crm/api-token", &["crm.example.com"]).is_ok());
    assert!(resolve("crm/api-token", &["example.com"]).is_ok());
    assert!(resolve("jira/auth-header", &["atlassian.net"]).is_ok());
    // A secret with no pinned host cannot be sent anywhere, so it still resolves.
    assert!(resolve("web/key", &[]).is_ok());

    // Outside: another host, a lookalike suffix, a narrower approved domain than the pin,
    // no approved domains at all, and a wildcard pin the approved domain does not cover.
    assert!(resolve("crm/api-token", &["other.example.com"]).is_err());
    assert!(resolve("crm/api-token", &["evilexample.com"]).is_err());
    assert!(resolve("crm/api-token", &["v2.api.crm.example.com"]).is_err());
    assert!(resolve("crm/api-token", &[]).is_err());
    assert!(resolve("jira/auth-header", &["jira.atlassian.net"]).is_err());
    assert!(resolve("jira/auth-header", &["*"]).is_err());
}

// ---- reserved kind, upgrade ------------------------------------------------------------------

#[test]
fn connection_credential_cannot_be_rewritten_as_egress() {
    let (_store, vault) = oauth_fixture(SecretScope::User);
    let binding_id = mcp_oauth_access_token_binding_id("acme");
    let err = vault
        .put_secret_with_owner(
            &binding_id,
            "attacker-chosen",
            SecretKind::Egress,
            &[MCP_HOST.to_string()],
            None,
            OWNER,
        )
        .expect_err("the kind must not be flipped back to egress");
    assert!(err.to_string().contains("MCP connection"), "{err}");
    let meta = vault
        .get_metadata_at(SecretScope::User, OWNER, &binding_id)
        .expect("row");
    assert_eq!(meta.kind, SecretKind::Connection);
    assert!(vault
        .session_egress_bindings_for_caller(OWNER)
        .expect("bindings")
        .is_empty());
}

#[test]
fn the_secrets_api_cannot_write_the_connection_kind() {
    assert!(SecretKind::from_proto(SecretKind::Connection.to_proto()).is_err());
    assert_eq!(SecretKind::from_proto(1).unwrap(), SecretKind::Inference);
    assert_eq!(SecretKind::from_proto(2).unwrap(), SecretKind::Egress);
}

#[test]
fn refreshed_tokens_stay_connection_kind() {
    use vam_orchestrator::mcp_registry::store_refreshed_mcp_tokens;
    let (store, vault) = oauth_fixture(SecretScope::User);
    let conn = store
        .get_mcp_connection(SecretScope::User, OWNER, "acme")
        .expect("connection");
    store_refreshed_mcp_tokens(&vault, &conn, "new-access", Some("new-refresh")).expect("refresh");
    for binding_id in [
        mcp_oauth_access_token_binding_id("acme"),
        mcp_oauth_refresh_token_binding_id("acme"),
    ] {
        let meta = vault
            .get_metadata_at(SecretScope::User, OWNER, &binding_id)
            .expect("row");
        assert_eq!(meta.kind, SecretKind::Connection, "{binding_id}");
    }
    assert!(vault
        .session_egress_bindings_for_caller(OWNER)
        .expect("bindings")
        .is_empty());
}

#[test]
fn rows_written_by_an_earlier_build_are_reclassified_on_start() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    let vault = SecretsVault::for_tests(Arc::clone(&store));
    store
        .upsert_mcp_connection(
            SecretScope::User,
            OWNER,
            "acme",
            &format!("https://{MCP_HOST}/mcp"),
            "",
            &[],
            &[],
            McpAuthMode::Oauth,
            Some(&oauth_meta()),
        )
        .expect("connection");
    let old_rows = [
        "acme/oauth/access-token",
        "acme/oauth/refresh-token",
        "acme/oauth/client-secret",
        "acme/auth-header",
        "acme/header/x-api-key",
    ];
    // What the previous build wrote: kind egress, pinned to the MCP host.
    for id in old_rows {
        vault
            .put_secret_with_owner(
                id,
                "legacy",
                SecretKind::Egress,
                &[MCP_HOST.to_string()],
                None,
                OWNER,
            )
            .expect("legacy row");
    }
    // Rows that are not the connection's own stay egress: another connection id that only
    // shares a prefix, the same ids in another partition, and an unrelated guest credential.
    vault
        .put_secret_with_owner(
            "acme-two/oauth/access-token",
            "other",
            SecretKind::Egress,
            &["other.example.com".to_string()],
            None,
            OWNER,
        )
        .expect("prefix lookalike");
    vault
        .put_secret(
            "acme/oauth/access-token",
            "org-lookalike",
            SecretKind::Egress,
            &["org.example.com".to_string()],
            None,
        )
        .expect("org partition");
    vault
        .put_secret_with_owner(
            "hrms/api-token",
            "guest-cred",
            SecretKind::Egress,
            &["hrms.example.com".to_string()],
            None,
            OWNER,
        )
        .expect("guest credential");
    assert_eq!(
        vault
            .session_egress_bindings_for_caller(OWNER)
            .unwrap()
            .len(),
        7, // the user row shadows the org lookalike until it is reclassified
    );

    assert_eq!(store.initialize_connection_secret_kinds().unwrap(), 5);
    assert_eq!(
        store.initialize_connection_secret_kinds().unwrap(),
        0,
        "a second start changes nothing"
    );

    for id in old_rows {
        let meta = vault
            .get_metadata_at(SecretScope::User, OWNER, id)
            .expect("row");
        assert_eq!(meta.kind, SecretKind::Connection, "{id}");
    }
    let mut still_guest: Vec<String> = vault
        .session_egress_bindings_for_caller(OWNER)
        .unwrap()
        .into_iter()
        .map(|b| b.binding_id)
        .collect();
    still_guest.sort();
    assert_eq!(
        still_guest,
        [
            "acme-two/oauth/access-token",
            "acme/oauth/access-token", // the org-partition lookalike
            "hrms/api-token",
        ]
    );
    assert!(
        !broker_allowed_domains(&vault.session_egress_bindings_for_caller(OWNER).unwrap())
            .contains(&MCP_HOST.to_string())
    );
}

#[test]
fn host_components_with_an_operator_named_ref_refuse_a_connection_row() {
    let (_store, vault) = oauth_fixture(SecretScope::Org);
    let err = vault
        .decrypt_secret_not_connection(&mcp_oauth_access_token_binding_id("acme"))
        .expect_err("an MCP credential is not a channel token");
    assert!(err.to_string().contains("MCP connection"), "{err}");
    assert!(!err.to_string().contains(ACCESS));
    vault
        .put_secret("notify/bot", "bot-value", SecretKind::Egress, &[], None)
        .expect("bot");
    assert_eq!(
        vault
            .decrypt_secret_not_connection("notify/bot")
            .expect("ordinary row")
            .expose_secret(),
        "bot-value"
    );
    assert!(matches!(
        vault.decrypt_secret_not_connection("notify/none"),
        Err(vam_orchestrator::error::OrchestratorError::NotFound(_))
    ));
}

#[test]
fn host_tool_calls_never_read_a_connection_row_and_get_the_pins_of_others() {
    use vam_orchestrator::secrets::destination_allows_host;
    // Static header credential of an MCP connection, as the registry writes it.
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    let vault = SecretsVault::for_tests(Arc::clone(&store));
    store_mcp_header_credential(
        &vault,
        SecretScope::Org,
        "",
        "acme",
        "Authorization",
        "Bearer static-auth-value",
        MCP_HOST,
        None,
        UserRole::OrgAdmin,
    )
    .expect("auth header");
    for caller in ["", OWNER] {
        let err = vault
            .decrypt_secret_for_host_call(caller, &mcp_auth_binding_id("acme"))
            .expect_err("a tool must not read an MCP credential");
        assert!(!err.to_string().contains("static-auth-value"), "{err}");
    }
    vault
        .put_secret(
            "jira/auth-header",
            "jira-secret",
            SecretKind::Egress,
            &["*.atlassian.net".to_string()],
            None,
        )
        .expect("jira");
    let (value, pins) = vault
        .decrypt_secret_for_host_call(OWNER, "jira/auth-header")
        .expect("ordinary row");
    assert_eq!(value.expose_secret(), "jira-secret");
    assert!(destination_allows_host(&pins, "acme.atlassian.net"));
    assert!(!destination_allows_host(&pins, "atlassian.net"));
    assert!(!destination_allows_host(&pins, "evil.example"));
    assert!(!destination_allows_host(&[], "acme.atlassian.net"));
    assert!(destination_allows_host(
        &["*".to_string()],
        "anything.example"
    ));
}
