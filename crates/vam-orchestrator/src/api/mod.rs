use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub mod allowed_models;
pub mod auth;
pub mod catalog_auth;
pub mod catalog_owner;
pub mod catalog_release;
mod composition;
pub mod connectors;
pub mod contacts;
pub mod decide;
pub mod error;
mod inbox;
pub mod login;
pub mod marketplaces;
pub mod mcp_connections;
pub mod me;
pub mod node_notify;
pub mod notifications;
pub mod org_secrets_admin;
mod outcomes;
mod paging;
mod permissions;
mod permissions_migrate;
pub mod publish_grant;
mod question;
pub mod queue;
pub mod rate_limit;
pub(crate) mod reauth;
mod release_suggestion;
pub(crate) mod resume_decisions;
pub mod resume_run;
pub mod run_start;
pub mod skill_packages;
pub mod spaces;
mod stop;
pub mod telemetry_stream;
pub mod threads;
pub mod users_admin;
pub mod users_keys_admin;
pub mod workspace_files;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{middleware, Router};
use prost_types::Timestamp;
use tower_http::trace::TraceLayer;

use crate::api::auth::{require_idempotency_key, AuthContext, ControlPlanePdp};
use crate::api::catalog_auth::require_catalog_read;
use crate::api::error::{json_response, json_response_accepted, parse_json_body, ApiError};
use crate::catalog_suggest::SuggestKind;
use crate::error::OrchestratorError;
use crate::hsp::{HspClient, HspEndpoint};
use crate::hsp_registry::HspRegistry;
use crate::hsp_store::HspBrokerToolSnapshot;
use crate::keys::{mint_api_key_secret, ApiKeyRecord, BOOTSTRAP_SCOPES, BROKERS_ADMIN_SCOPE};
use crate::manager::{SessionManager, SnapshotKind};
use crate::secrets::{
    authorize_org_secret_write, resolve_secret_scope, vault_owner_user_id,
    HostSecretMetadata as StoreSecretMetadata, SecretKind, SecretScope, SecretsVault,
};
use crate::session::{DesiredState, ObservedState, SessionRecord};
use crate::workflow::{PlaybookStepRecord, PlaybookStepState, SqliteWorkflowStore};
use vam_security::ScopeRule;
use vam_spec::v1::{
    ApiKeyRecord as ProtoApiKeyRecord, ApprovalRequest, ApprovalStatus, CreateApiKeyRequest,
    CreateApiKeyResponse, CreateRunRequest, CreateRunResponse, CreateSessionRequest,
    CreateSessionResponse, CreateSnapshotRequest, CreateSnapshotResponse, DecideApprovalRequest,
    DecideApprovalResponse, DeleteHostSecretResponse, ExecutionLease, GetAgentResponse,
    GetApprovalResponse, GetHostedBrokerResponse, GetNodeStatusResponse, GetPlaybookResponse,
    GetPolicyResponse, GetRunResponse, GetSessionResponse, GetSkillResponse, HealthResponse,
    HostExecSpec, HostSecretMetadata, HostedBrokerRecord, ListAgentsResponse, ListApiKeysResponse,
    ListApprovalsResponse, ListHostSecretsResponse, ListHostedBrokersResponse,
    ListPoliciesResponse, ListRunEventsResponse, ListRunsResponse, ListSessionsResponse,
    NodeStatus, PauseSessionResponse, PutHostSecretRequest, PutHostSecretResponse,
    PutHostedBrokerRequest, PutHostedBrokerResponse, RegisterRequest, Remember,
    ResolveSecretRequirementsRequest, ResolveSecretRequirementsResponse, ResourceId,
    RestoreSessionRequest, RestoreSessionResponse, ResumeSessionResponse, RevokeApiKeyResponse,
    RunEvent, RunStatus, SessionStatus, StepExecutionStatus, TaskOutcomeState,
    TerminateSessionResponse, ToolSpec,
};

const OPENAPI_SPEC: &str = include_str!("../../../../docs/api/openapi.yaml");

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<SqliteWorkflowStore>,
    pub vault: Arc<SecretsVault>,
    pub manager: SessionManager,
    pub pdp: Arc<ControlPlanePdp>,
    pub listen_addr: String,
    pub draining: Arc<AtomicBool>,
    pub ready: Arc<AtomicBool>,
    /// True when this node loaded scripts from `VAMD_E2E_MOCK_SCRIPTS`. Only such a node
    /// accepts `mock-model` (design/e2e-journeys.md D3).
    pub e2e_mock_scripts: bool,
}

/// Routes only a process-guest build has (test seams). A default `vamd` has none: the function
/// is empty there and the release gate looks for the route's path in the binary.
#[cfg(feature = "process-guest")]
fn process_only_routes() -> Router<AppState> {
    marketplaces::process_routes()
}

#[cfg(not(feature = "process-guest"))]
fn process_only_routes() -> Router<AppState> {
    Router::new()
}

pub fn router(state: AppState) -> Router {
    let store = state.store.clone();
    let limits = rate_limit::RateLimiter::installed(store.clone());
    Router::new()
        .route("/openapi.yaml", get(openapi_spec_handler))
        .route("/docs", get(docs_handler))
        .route("/v1/health", get(health))
        .route("/v1/login", post(login::login))
        .route("/v1/login/refresh", post(login::refresh_login))
        .route("/v1/logout", post(login::logout))
        .route("/v1/me", get(me::get_me))
        .route("/v1/keys", post(create_key).get(list_keys))
        .route("/v1/keys/:id", delete(revoke_key))
        .route("/v1/users", users_admin::users_collection_route())
        .route(
            "/v1/users/:id/deactivate",
            users_admin::deactivate_user_route(),
        )
        .route(
            "/v1/users/:id/password",
            users_admin::reset_user_password_route(),
        )
        .route(
            "/v1/users/:id/publish-grant",
            publish_grant::publish_grant_route(),
        )
        .route(
            "/v1/users/:id/keys",
            users_keys_admin::user_keys_collection_route(),
        )
        .route(
            "/v1/users/:id/keys/:key_id",
            users_keys_admin::revoke_user_key_route(),
        )
        .route(
            "/v1/catalog/reassign-owner",
            catalog_owner::catalog_owner_route(),
        )
        .route(
            "/v1/org/secrets",
            org_secrets_admin::org_secrets_collection_route(),
        )
        .route(
            "/v1/org/secrets/*id",
            org_secrets_admin::delete_org_secret_route(),
        )
        .route("/v1/node", get(get_node))
        .route(
            "/v1/node/allowed-models",
            allowed_models::allowed_models_route(),
        )
        .route("/v1/agents", put(put_agent).get(list_agents))
        .route(
            "/v1/permission-presets",
            get(permissions::list_permission_presets),
        )
        .route(
            "/v1/agents/*rest",
            get(get_agent_wildcard)
                .put(permissions::put_draft_permissions)
                .post(post_agent_wildcard)
                .delete(delete_agent_wildcard),
        )
        .route("/v1/policies", put(put_policy).get(list_policies))
        .route(
            "/v1/policies/*rest",
            get(get_policy_wildcard).merge(catalog_release::policy_release_route()),
        )
        .route("/v1/playbooks", put(put_playbook).get(list_playbooks))
        .route("/v1/playbook-tests", post(playbook_test))
        .route("/v1/agent-checks", post(composition::agent_check))
        .route(
            "/v1/playbooks/*rest",
            get(get_playbook_wildcard)
                .merge(catalog_release::playbook_release_route())
                .delete(delete_playbook_wildcard),
        )
        .merge(skill_packages::skill_package_routes())
        .route("/v1/skills", put(put_skill))
        .route(
            "/v1/skills/*rest",
            get(get_skill_wildcard).merge(catalog_release::skill_release_route()),
        )
        .route("/v1/sessions", post(create_session).get(list_sessions))
        .route("/v1/workspaces/:id/files", workspace_files::list_route())
        .merge(workspace_files::path_routes())
        .merge(spaces::routes())
        .merge(notifications::routes())
        .merge(contacts::routes())
        .merge(node_notify::routes())
        .merge(connectors::routes())
        .merge(marketplaces::routes())
        .merge(process_only_routes())
        .merge(threads::routes())
        .route("/v1/sessions/:id", get(get_session))
        .route("/v1/sessions/:id/egress-ca", get(get_session_egress_ca))
        .route("/v1/sessions/:id/pause", post(pause_session))
        .route("/v1/sessions/:id/resume", post(resume_session))
        .route("/v1/sessions/:id/terminate", post(terminate_session))
        .route("/v1/sessions/:id/snapshots", post(create_snapshot))
        .route("/v1/sessions/restore", post(restore_session))
        .route("/v1/runs", post(create_run).get(list_runs))
        // Catch-all so run URNs with slashes (`urn:vam:run:registry/ns/name`) match.
        // Axum 0.6 wildcards must be the last path segment, so /events and /cancel
        // are parsed from the captured rest.
        .route(
            "/v1/runs/*rest",
            get(run_wildcard_get).post(run_wildcard_post),
        )
        .route("/v1/approvals", get(list_approvals))
        .route("/v1/approvals/:id", get(get_approval))
        .route("/v1/approvals/:id/decide", post(decide_approval))
        .route("/v1/secrets", put(put_secret).get(list_secrets))
        .route("/v1/secrets/resolve", post(resolve_secret_requirements))
        .route("/v1/secrets/*id", get(get_secret).delete(delete_secret))
        .route("/v1/mcp/connections", mcp_connections::collection_route())
        .route("/v1/mcp/connections/:id", mcp_connections::item_route())
        .route(
            "/v1/mcp/connections/:id/refresh",
            mcp_connections::refresh_route(),
        )
        .route(
            "/v1/mcp/connections/:id/oauth/tokens",
            mcp_connections::oauth_tokens_route(),
        )
        .route(
            "/v1/hosted/brokers",
            put(put_hosted_broker).get(list_hosted_brokers),
        )
        .route(
            "/v1/hosted/brokers/*id",
            get(get_hosted_broker).delete(delete_hosted_broker),
        )
        .layer(middleware::from_fn_with_state(
            limits.clone(),
            rate_limit::key_layer,
        ))
        .layer(middleware::from_fn(move |req, next| {
            let store = store.clone();
            async move { crate::api::auth::bearer_auth_middleware(store, req, next).await }
        }))
        .layer(middleware::from_fn_with_state(limits, rate_limit::ip_layer))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}

pub async fn serve(
    state: AppState,
    listener: tokio::net::TcpListener,
) -> Result<(), OrchestratorError> {
    let app = router(state);
    let std_listener = listener.into_std().map_err(OrchestratorError::Io)?;
    axum::Server::from_tcp(std_listener)
        .map_err(|e| OrchestratorError::Core(format!("HTTP server error: {}", e)))?
        .serve(app.into_make_service_with_connect_info::<std::net::SocketAddr>())
        .await
        .map_err(|e| OrchestratorError::Core(format!("HTTP server error: {}", e)))
}

/// Serves the API as `transport` says: TLS by default, plain HTTP only for `--insecure-dev`.
pub async fn serve_with(
    state: AppState,
    listener: tokio::net::TcpListener,
    transport: crate::tls::Transport,
) -> Result<(), OrchestratorError> {
    // Queued messages start when the task they wait behind has ended (AR-3.8); the first
    // pass also starts what was waiting behind a run that ended while vamd was down.
    queue::spawn_queue_starter(state.clone());
    // Tasks a crash or a restart interrupted are picked up again (E10 Phase 4).
    resume_run::spawn_resume_pass(state.clone());
    // One trigger gateway socket per approved broker that declares run_request_permission.
    crate::gateway::sync_all(&state).await;
    match transport {
        crate::tls::Transport::Plain => serve(state, listener).await,
        crate::tls::Transport::Tls(tls) => {
            crate::tls::serve_app(router(state), listener, tls).await
        }
    }
}

async fn openapi_spec_handler() -> Response {
    (
        StatusCode::OK,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/yaml"),
        )],
        OPENAPI_SPEC,
    )
        .into_response()
}

async fn docs_handler() -> axum::response::Html<&'static str> {
    axum::response::Html(
        r#"<!doctype html>
<html>
  <head>
    <title>VAMD Control Plane API Documentation</title>
    <meta charset="utf-8" />
    <meta name="viewport" content="width=device-width, initial-scale=1" />
    <style>
      body {
        margin: 0;
      }
    </style>
  </head>
  <body>
    <script
      id="api-reference"
      data-url="/openapi.yaml"
      data-configuration='{"theme":"purple","layout":"modern"}'></script>
    <script
      src="https://cdn.jsdelivr.net/npm/@scalar/api-reference@1.74.0/dist/browser/standalone.js"
      integrity="sha384-VXnKAz61Xwc3tbw1P+KKrUP7YbcsSl1Ik4is5UnogJyw9XEYZ5Ay/T1i0zPYtSZH"
      crossorigin="anonymous"></script>
  </body>
</html>"#,
    )
}

pub(crate) fn ensure_ready(state: &AppState, mutating: bool) -> Result<(), ApiError> {
    if !state.ready.load(Ordering::SeqCst) {
        return Err(OrchestratorError::StorageUnavailable.into());
    }
    if mutating && state.draining.load(Ordering::SeqCst) {
        return Err(OrchestratorError::NodeDraining.into());
    }
    Ok(())
}

pub(crate) fn json_http_response(status: StatusCode, body: String) -> Response {
    (
        status,
        [(
            axum::http::header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        )],
        body,
    )
        .into_response()
}

/// Attach skill-manifest publish warnings. Empty `policy` leaves `json` unchanged.
pub(crate) fn with_skill_policy_warnings(
    json: String,
    spec: Option<&vam_spec::v1::SkillManifest>,
) -> String {
    let Some(spec) = spec else {
        return json;
    };
    let warnings = vam_spec::validation::skill_manifest_warnings(spec);
    if warnings.is_empty() {
        return json;
    }
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&json) else {
        return json;
    };
    value["warnings"] = serde_json::Value::Array(
        warnings
            .into_iter()
            .map(serde_json::Value::String)
            .collect(),
    );
    value.to_string()
}

async fn health(State(state): State<AppState>) -> Result<Response, ApiError> {
    let status = if state.ready.load(Ordering::SeqCst) && !state.draining.load(Ordering::SeqCst) {
        health_response::ServingStatus::Serving
    } else {
        health_response::ServingStatus::NotServing
    };
    let msg = HealthResponse {
        status: status as i32,
    };
    let (code, body) = json_response("vam.v1.HealthResponse", &msg)?;
    Ok(json_http_response(code, body))
}

mod health_response {
    pub enum ServingStatus {
        Serving = 1,
        NotServing = 2,
    }
}

async fn create_key(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "CreateApiKey", "keys:admin")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: CreateApiKeyRequest = parse_json_body("vam.v1.CreateApiKeyRequest", &body)?;
    let key_id = format!("key-{}", uuid_like());
    let secret = mint_api_key_secret();
    let owner_ref = if req.user_id.is_empty() {
        auth.owner_user_id().to_string()
    } else {
        req.user_id.clone()
    };
    let owner = state.store.resolve_user(&owner_ref)?;
    crate::users::reject_deactivated_user(&owner)?;
    let scopes = crate::keys::sanitize_scopes_for_role(owner.role, &req.scopes);
    if scopes.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "API key must have at least one scope; tenants never receive keys:admin or brokers:admin"
                .to_string(),
        )
        .into());
    }
    let principal = if req.principal.is_empty() {
        format!("User::\"{}\"", req.name)
    } else {
        req.principal
    };
    let record = state.store.create_api_key_owned(
        &key_id,
        &req.name,
        &secret,
        &principal,
        &scopes,
        &owner.user_id,
    )?;
    let resp = CreateApiKeyResponse {
        key: Some(to_proto_key(&record)),
        secret,
    };
    let (code, json) = json_response("vam.v1.CreateApiKeyResponse", &resp)?;
    Ok(json_http_response(code, json))
}

#[derive(serde::Deserialize, Default)]
struct ListKeysQuery {
    holder: Option<String>,
}

async fn list_keys(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(q): Query<ListKeysQuery>,
) -> Result<Response, ApiError> {
    let kind = match q.holder.as_deref() {
        None | Some("") => crate::keys::KeyKind::Http,
        Some("broker") => crate::keys::KeyKind::Grants,
        Some("all") => crate::keys::KeyKind::All,
        Some(other) => {
            return Err(OrchestratorError::InvalidContract(format!(
                "holder must be broker or all, not '{other}'"
            ))
            .into())
        }
    };
    // Callers list their own keys. `keys:admin` still sees every `http` row so operators can
    // manage bootstrap / operator keys; a grant is listed to its owner only, for every role.
    let keys = state
        .store
        .list_keys_of_kind(auth.owner_user_id(), auth.has_keys_admin(), kind)?;
    let resp = ListApiKeysResponse {
        keys: keys
            .iter()
            .map(|k| to_proto_key_with_labels(&state.store, k))
            .collect(),
        pagination: None,
    };
    let (code, json) = json_response("vam.v1.ListApiKeysResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn revoke_key(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let record = state
        .store
        .revoke_api_key_as(&id, auth.owner_user_id(), auth.has_keys_admin())?;
    // A fire the grant left in a queue ends with it.
    if record.holder().starts_with("broker:") {
        if let Err(err) = state.store.discard_queued_fires(
            Some(&record.key_id),
            None,
            crate::gateway::GRANT_REVOKED_MESSAGE,
        ) {
            tracing::warn!(
                "queued fire of grant {} not discarded: {err}",
                record.key_id
            );
        }
    }
    let resp = RevokeApiKeyResponse {
        key: Some(to_proto_key_with_labels(&state.store, &record)),
    };
    let (code, json) = json_response("vam.v1.RevokeApiKeyResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// Capacity / draining snapshot for the local node.
///
/// Requires `node:read`. Role defaults include that scope for member,
/// org_admin, publisher, and operator, so password sessions and
/// default-scope API keys return 200. The VAM app may probe this after
/// `GET /v1/health`. The body is node capacity, not a list of other
/// users' runs.
async fn get_node(State(state): State<AppState>, auth: AuthContext) -> Result<Response, ApiError> {
    state.pdp.authorize(&auth, "GetNodeStatus", "node:read")?;
    let sessions = state
        .store
        .load_all_sessions()
        .map_err(|e: OrchestratorError| ApiError::from(e))?;
    let active = sessions
        .iter()
        .filter(|s| {
            !matches!(
                s.observed_state,
                ObservedState::Terminated | ObservedState::Failed
            )
        })
        .count() as i32;
    let status = NodeStatus {
        node_id: "local".to_string(),
        listen_addr: state.listen_addr.clone(),
        draining: state.draining.load(Ordering::SeqCst),
        ready: state.ready.load(Ordering::SeqCst),
        active_sessions: active,
        max_sessions: 64,
        total_vcpus_allocated: active * 2,
        max_vcpus: 128,
        total_memory_mib_allocated: (active as i64) * 512,
        max_memory_mib: 65536,
        updated_at: Some(now_ts()),
        guest_mode: crate::guest_mode().to_string(),
        isolation: Some(crate::isolation::describe(&crate::isolation::current())),
        // Public keys only; the secrets never leave the node's signing directory.
        signing_keys: crate::signing::node_keys()
            .map(|k| k.node_signing_keys())
            .unwrap_or_default(),
        base: crate::node_base::report(),
    };
    let resp = GetNodeStatusResponse {
        status: Some(status),
    };
    let (code, json) = json_response("vam.v1.GetNodeStatusResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn put_agent(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: vam_spec::v1::PutAgentRequest = parse_json_body("vam.v1.PutAgentRequest", &body)?;
    let spec = req
        .spec
        .ok_or_else(|| OrchestratorError::InvalidContract("spec is required".into()))?;
    let saved = state
        .store
        .publish_agent_draft_with_owner(&spec, auth.owner_user_id())?;
    let resp = vam_spec::v1::PutAgentResponse { spec: Some(saved) };
    let (code, json) = json_response("vam.v1.PutAgentResponse", &resp)?;
    Ok(json_http_response(code, json))
}

#[derive(Debug, Default, serde::Deserialize)]
struct MigrateQuery {
    #[serde(default)]
    force: Option<String>,
}

async fn post_agent_wildcard(
    state: State<AppState>,
    auth: AuthContext,
    path: Path<String>,
    Query(query): Query<MigrateQuery>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    if permissions_migrate::is_migrate_path(&path) {
        let force = permissions_migrate::force_requested(query.force.as_deref());
        return permissions_migrate::migrate_legacy_draft(state, auth, path, headers, force, body)
            .await;
    }
    catalog_release::release_agent_wildcard(state, auth, path, headers, body).await
}

async fn get_agent_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    Query(query): Query<permissions::PermissionsQuery>,
) -> Result<Response, ApiError> {
    if let Some(kind) = permissions::AgentPermissionsGet::match_rest(&rest) {
        return permissions::get_agent_permissions(state, auth, kind, &rest, query).await;
    }
    require_catalog_read(&auth)?;
    let rest = rest.trim_start_matches('/');
    if let Some(urn_raw) = rest.strip_suffix("/versions") {
        return catalog_release::versions_response(&state, &auth, "agents", urn_raw);
    }
    if let Some((urn_raw, diff)) = outcomes::outcomes_urn(rest) {
        return outcomes::outcomes_response(
            &state,
            &auth,
            urn_raw,
            query.version.as_deref(),
            query.k.as_deref(),
            diff,
        );
    }
    if let Some(urn) = release_suggestion::suggestion_urn(rest) {
        return release_suggestion::release_suggestion(&state, &auth, SuggestKind::Agent, &urn);
    }
    if rest.ends_with("/release") {
        return Err(OrchestratorError::NotFound(format!("GET /v1/agents/{rest}")).into());
    }
    let urn = percent_decode_path(rest);
    let mut record =
        state
            .store
            .get_agent_record_visible(&urn, auth.owner_user_id(), auth.has_users_admin())?;
    state
        .store
        .attach_agent_origins(std::slice::from_mut(&mut record))?;
    let resp = GetAgentResponse {
        spec: record.spec,
        owner_user_id: record.owner_user_id,
        channel: record.channel,
        catalog_tier: record.catalog_tier,
        tier: record.tier,
        read_only: record.read_only,
        origin: record.origin,
    };
    let (code, json) = json_response("vam.v1.GetAgentResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// `DELETE /v1/{agents|playbooks|skills/packages}/{urn}`: the catalog keeps every release, so
/// nothing is deleted. The route exists so the answer is explicit: an installed row is refused as
/// read-only (409, M2) and any other row as not deletable (405).
pub(crate) async fn refuse_catalog_delete(
    state: &AppState,
    auth: &AuthContext,
    rest: &str,
    noun: &str,
) -> Result<Response, ApiError> {
    state.pdp.authorize(auth, "PutCatalog", "catalog:write")?;
    let urn = percent_decode_path(rest.trim_start_matches('/'));
    state.store.require_not_installed(&urn, noun)?;
    Err(ApiError {
        status: StatusCode::METHOD_NOT_ALLOWED,
        code: vam_spec::v1::VamErrorCode::Unimplemented,
        message: format!("{noun} '{urn}' cannot be deleted: the catalog keeps every release"),
        retryable: false,
    })
}

async fn delete_agent_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
) -> Result<Response, ApiError> {
    refuse_catalog_delete(&state, &auth, &rest, "agent").await
}

async fn delete_playbook_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
) -> Result<Response, ApiError> {
    refuse_catalog_delete(&state, &auth, &rest, "playbook").await
}

async fn get_policy_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let rest = rest.trim_start_matches('/');
    if let Some(urn_raw) = rest.strip_suffix("/versions") {
        return catalog_release::versions_response(&state, &auth, "policies", urn_raw);
    }
    if let Some(urn) = release_suggestion::suggestion_urn(rest) {
        return release_suggestion::release_suggestion(&state, &auth, SuggestKind::Policy, &urn);
    }
    if rest.ends_with("/release") {
        return Err(OrchestratorError::NotFound(format!("GET /v1/policies/{rest}")).into());
    }
    let urn = percent_decode_path(rest);
    let record = state.store.get_policy_visible(&urn, auth.owner_user_id())?;
    let resp = GetPolicyResponse {
        record: Some(record),
    };
    let (code, json) = json_response("vam.v1.GetPolicyResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn get_playbook_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let rest = rest.trim_start_matches('/');
    if let Some(urn_raw) = rest.strip_suffix("/versions") {
        return catalog_release::versions_response(&state, &auth, "playbooks", urn_raw);
    }
    if let Some(urn) = release_suggestion::suggestion_urn(rest) {
        return release_suggestion::release_suggestion(&state, &auth, SuggestKind::Playbook, &urn);
    }
    if rest.ends_with("/release") {
        return Err(OrchestratorError::NotFound(format!("GET /v1/playbooks/{rest}")).into());
    }
    let urn = percent_decode_path(rest);
    let spec = state
        .store
        .get_playbook_visible(&urn, auth.owner_user_id())?;
    let resp = GetPlaybookResponse { spec: Some(spec) };
    let (code, json) = json_response("vam.v1.GetPlaybookResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn get_skill_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let rest = rest.trim_start_matches('/');
    if let Some(urn_raw) = rest.strip_suffix("/versions") {
        return catalog_release::versions_response(&state, &auth, "skills", urn_raw);
    }
    if let Some(urn) = release_suggestion::suggestion_urn(rest) {
        return release_suggestion::release_suggestion(&state, &auth, SuggestKind::Skill, &urn);
    }
    if rest.ends_with("/release") {
        return Err(OrchestratorError::NotFound(format!("GET /v1/skills/{rest}")).into());
    }
    let urn = percent_decode_path(rest);
    let spec = state.store.get_skill_visible(&urn, auth.owner_user_id())?;
    let resp = GetSkillResponse { spec: Some(spec) };
    let (code, json) = json_response("vam.v1.GetSkillResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn list_agents(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let mut records = state
        .store
        .list_agent_records_visible(auth.owner_user_id(), auth.has_users_admin())?;
    state.store.attach_agent_origins(&mut records)?;
    let specs = records.iter().filter_map(|row| row.spec.clone()).collect();
    let resp = ListAgentsResponse {
        specs,
        pagination: None,
        records,
    };
    let (code, json) = json_response("vam.v1.ListAgentsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn put_policy(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: vam_spec::v1::PutPolicyRequest = parse_json_body("vam.v1.PutPolicyRequest", &body)?;
    if req.urn.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("urn is required".into()).into());
    }
    let saved =
        state
            .store
            .publish_policy_draft_with_owner(&req.urn, &req.cedar, auth.owner_user_id())?;
    let resp = vam_spec::v1::PutPolicyResponse {
        record: Some(saved),
    };
    let (code, json) = json_response("vam.v1.PutPolicyResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn list_policies(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let records = state.store.list_policies_visible(auth.owner_user_id())?;
    let resp = ListPoliciesResponse {
        records,
        pagination: None,
    };
    let (code, json) = json_response("vam.v1.ListPoliciesResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn list_playbooks(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let mut records = state
        .store
        .list_playbook_records_visible(auth.owner_user_id())?;
    state.store.attach_playbook_origins(&mut records)?;
    let specs = records
        .iter()
        .filter_map(|record| record.spec.clone())
        .collect();
    let resp = vam_spec::v1::ListPlaybooksResponse {
        specs,
        pagination: None,
        records,
    };
    let (code, json) = json_response("vam.v1.ListPlaybooksResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// Body of `POST /v1/playbook-tests`. `spec` is a `PlaybookSpec` in proto JSON.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaybookTestRequest {
    spec: serde_json::Value,
    scenario: serde_json::Value,
    #[serde(default = "default_playbook_test_repeat")]
    repeat: u32,
}

fn default_playbook_test_repeat() -> u32 {
    1
}

/// Run a scripted playbook scenario on an in-memory store. Writes no catalog row and leaves no
/// run or session behind.
async fn playbook_test(
    State(state): State<AppState>,
    auth: AuthContext,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    require_catalog_read(&auth)?;
    let req: PlaybookTestRequest = serde_json::from_str(&body).map_err(|e| {
        OrchestratorError::InvalidContract(format!("invalid playbook test body: {e}"))
    })?;
    let spec: vam_spec::v1::PlaybookSpec =
        vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &req.spec.to_string())
            .map_err(OrchestratorError::InvalidContract)?;
    let scenario: crate::playbook_test::Scenario = serde_json::from_value(req.scenario)
        .map_err(|e| OrchestratorError::InvalidContract(format!("invalid scenario: {e}")))?;
    let report = crate::playbook_test::run_playbook_test(&spec, &scenario, req.repeat)?;
    let json = serde_json::to_string(&report)
        .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
    Ok(json_http_response(StatusCode::OK, json))
}

async fn put_playbook(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: vam_spec::v1::PutPlaybookRequest =
        parse_json_body("vam.v1.PutPlaybookRequest", &body)?;
    let spec = req
        .spec
        .ok_or_else(|| OrchestratorError::InvalidContract("spec is required".into()))?;
    let saved = state
        .store
        .publish_playbook_draft_with_owner(&spec, auth.owner_user_id())?;
    let resp = vam_spec::v1::PutPlaybookResponse { spec: Some(saved) };
    let (code, json) = json_response("vam.v1.PutPlaybookResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn put_skill(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: vam_spec::v1::PutSkillRequest = parse_json_body("vam.v1.PutSkillRequest", &body)?;
    let spec = req
        .spec
        .ok_or_else(|| OrchestratorError::InvalidContract("spec is required".into()))?;
    crate::node_base::check_requires_runtime(&spec)?;
    let saved = state
        .store
        .publish_skill_draft_with_owner(&spec, auth.owner_user_id())?;
    let resp = vam_spec::v1::PutSkillResponse { spec: Some(saved) };
    let (code, json) = json_response("vam.v1.PutSkillResponse", &resp)?;
    let json = with_skill_policy_warnings(json, resp.spec.as_ref());
    Ok(json_http_response(code, json))
}

async fn create_session(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "CreateSession", "sessions:write")?;
    let idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: CreateSessionRequest = parse_json_body("vam.v1.CreateSessionRequest", &body)?;
    // Phase 0 user isolation: session_id and vm_id are server-minted only.
    // Client-supplied values are an existence oracle / DoS vector — reject with 400.
    let client_session_id = req
        .session_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .filter(|u| !u.is_empty());
    if client_session_id.is_some() || !req.vm_id.is_empty() {
        return Err(ApiError::from(OrchestratorError::InvalidContract(
            "session_id and vm_id are server-generated; omit both from CreateSessionRequest"
                .to_string(),
        )));
    }
    let session_id = format!("sess-{}", uuid_like());
    let vm_id = format!("vm-{}", session_id);
    // An `sp_…` id the owner holds, or a label (the owner's default Space for it).
    let workspace_id = if crate::spaces::is_space_id(req.workspace_id.trim()) {
        crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &req.workspace_id)?
    } else {
        state
            .store
            .default_space(
                auth.owner_user_id(),
                None,
                crate::spaces::DefaultKind::Released,
                &crate::spaces::clean_label(&req.workspace_id, crate::spaces::DEFAULT_SPACE_LABEL)?,
            )?
            .space_id
    };
    let storage_profile_id = if req.storage_profile_id.is_empty() {
        "default".to_string()
    } else {
        req.storage_profile_id
    };
    let idem = if req.idempotency_key.is_empty() {
        idempotency
    } else {
        req.idempotency_key
    };

    if let Some(existing) = state
        .store
        .find_session_by_idempotency_key(auth.owner_user_id(), &idem)?
    {
        let resp = CreateSessionResponse {
            session: Some(to_session_status(&state.store, &existing)),
        };
        let (code, json) = json_response("vam.v1.CreateSessionResponse", &resp)?;
        return Ok(json_http_response(code, json));
    }

    state
        .manager
        .create_session(
            session_id.clone(),
            Some(idem),
            vm_id,
            workspace_id,
            storage_profile_id,
            None,
        )
        .await
        .map_err(ApiError::from)?;
    // Manager create path stamps `local` by default; overwrite with the calling
    // key's mapped user. Idempotent replay returned earlier, so this is a new row.
    state
        .store
        .stamp_session_owner(&session_id, auth.owner_user_id())?;

    let record = state.store.load_session(&session_id)?;
    let resp = CreateSessionResponse {
        session: Some(to_session_status(&state.store, &record)),
    };
    let (code, json) = json_response("vam.v1.CreateSessionResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn get_session(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state
        .pdp
        .authorize(&auth, "CreateSession", "sessions:write")?;
    let record = state
        .store
        .load_session_for_owner(&id, auth.owner_user_id())?;
    let resp = GetSessionResponse {
        session: Some(to_session_status(&state.store, &record)),
    };
    let (code, json) = json_response("vam.v1.GetSessionResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// Public per-session CA certificate. Owner or operator only. The private key
/// stays in process memory and is not part of this response.
async fn get_session_egress_ca(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state.pdp.authorize(&auth, "GetSession", "sessions:write")?;
    let operator = auth.role() == crate::users::UserRole::Operator
        || auth.owner_user_id() == crate::users::OPERATOR_USER_ID;
    let record = if operator {
        state.store.load_session(&id)?
    } else {
        state
            .store
            .load_session_for_owner(&id, auth.owner_user_id())?
    };
    let pem = state
        .store
        .session_ca_pem(&record.session_id)
        .ok_or_else(|| OrchestratorError::NotFound(format!("session '{id}' has no egress CA")))?;
    if pem.contains("PRIVATE KEY") {
        return Err(ApiError::from(OrchestratorError::Core(
            "refusing to return an egress CA that contains a private key".into(),
        )));
    }
    let body = serde_json::json!({
        "sessionId": record.session_id,
        "pem": pem,
    })
    .to_string();
    Ok(json_http_response(StatusCode::OK, body))
}

async fn list_sessions(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(page): Query<paging::PageQuery>,
) -> Result<Response, ApiError> {
    state
        .pdp
        .authorize(&auth, "CreateSession", "sessions:write")?;
    let size = page.size()?;
    let after = paging::pair_cursor(page.cursor("sessions")?)?;
    let mut sessions =
        state
            .store
            .load_sessions_page_for_owner(auth.owner_user_id(), size + 1, after)?;
    let more = sessions.len() > size;
    sessions.truncate(size);
    let next = match (more, sessions.last()) {
        (true, Some(s)) => paging::token(
            "sessions",
            &[s.created_at.to_string(), s.session_id.clone()],
        ),
        _ => String::new(),
    };
    let resp = ListSessionsResponse {
        sessions: sessions
            .iter()
            .map(|s| to_session_status(&state.store, s))
            .collect(),
        pagination: Some(vam_spec::v1::PaginationResponse {
            next_page_token: next,
        }),
    };
    let (code, json) = json_response("vam.v1.ListSessionsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn pause_session(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PauseSession", "sessions:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let _owned = state
        .store
        .load_session_for_owner(&id, auth.owner_user_id())?;
    state
        .manager
        .pause_session(id.clone())
        .await
        .map_err(ApiError::from)?;
    let record = state.store.load_session(&id)?;
    let resp = PauseSessionResponse {
        session: Some(to_session_status(&state.store, &record)),
    };
    let (code, json) = json_response("vam.v1.PauseSessionResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn resume_session(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "ResumeSession", "sessions:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let _owned = state
        .store
        .load_session_for_owner(&id, auth.owner_user_id())?;
    state
        .manager
        .resume_session(id.clone())
        .await
        .map_err(ApiError::from)?;
    let record = state.store.load_session(&id)?;
    let resp = ResumeSessionResponse {
        session: Some(to_session_status(&state.store, &record)),
    };
    let (code, json) = json_response("vam.v1.ResumeSessionResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// The session's run is being worked on (not finished, not parked for an
/// approval), so a stop interrupts it.
fn run_is_interruptible(store: &SqliteWorkflowStore, session_id: &str) -> bool {
    let Ok(Some(run_id)) = store.find_run_id_for_session(session_id) else {
        return false;
    };
    match store.get_run_state(&run_id) {
        Ok(state) => !matches!(
            state.to_ascii_uppercase().as_str(),
            "SUCCEEDED" | "FAILED" | "CANCELLED" | "TIMED_OUT" | "REJECTED" | "PAUSED"
        ),
        Err(_) => false,
    }
}

async fn terminate_session(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "TerminateSession", "sessions:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let _owned = state
        .store
        .load_session_for_owner(&id, auth.owner_user_id())?;
    // A stop of a task that is running: take the VM down and wait for the
    // run's finalizer to write the end of the task and settle the workspace
    // (files roll back to the last complete turn commit). Any other terminate
    // (a parked approval, a session with no dispatch) ends as it always did.
    if state.store.session_ends.is_finalizing(&id) {
        // The run has ended and its finalizer is settling the workspace: there
        // is nothing to interrupt, only that to wait for.
        if !state
            .store
            .session_ends
            .wait_settled(&id, crate::session_end::STOP_SETTLE_TIMEOUT)
            .await
        {
            return Err(ApiError {
                status: StatusCode::GATEWAY_TIMEOUT,
                code: vam_spec::v1::VamErrorCode::Unavailable,
                message: "stop_pending: the run has ended; its workspace is still being settled"
                    .to_string(),
                retryable: true,
            });
        }
    } else if run_is_interruptible(&state.store, &id) && state.store.session_ends.begin_stop(&id) {
        let stop_started = std::time::Instant::now();
        state
            .manager
            .stop_session_vm(id.clone())
            .await
            .map_err(ApiError::from)?;
        let settled = state
            .store
            .session_ends
            .wait_settled(&id, crate::session_end::STOP_SETTLE_TIMEOUT)
            .await;
        if settled {
            let timing = state
                .store
                .session_ends
                .take_stop_timing(&id)
                .unwrap_or_default();
            eprintln!(
                "[orchestrator] stop settled: session={} wait_ms={} prepare_end_ms={} resolve_ms={} scan_ms={}",
                id,
                stop_started.elapsed().as_millis(),
                timing.prepare_end_ms,
                timing.resolve_ms,
                timing.scan_ms
            );
        }
        if !settled {
            return Err(ApiError {
                status: StatusCode::GATEWAY_TIMEOUT,
                code: vam_spec::v1::VamErrorCode::Unavailable,
                message: "stop_pending: the session is stopped; its workspace is still being \
                          settled"
                    .to_string(),
                retryable: true,
            });
        }
    } else {
        state
            .manager
            .terminate_session(id.clone())
            .await
            .map_err(ApiError::from)?;
    }
    let record = state.store.load_session(&id)?;
    let resp = TerminateSessionResponse {
        session: Some(to_session_status(&state.store, &record)),
    };
    let (code, json) = json_response("vam.v1.TerminateSessionResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn create_snapshot(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "CreateSnapshot", "sessions:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let _owned = state
        .store
        .load_session_for_owner(&id, auth.owner_user_id())?;
    let req: CreateSnapshotRequest = parse_json_body("vam.v1.CreateSnapshotRequest", &body)?;
    let snapshot_id = req
        .snapshot_id
        .as_ref()
        .map(|r| r.urn.clone())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("snap-{}", uuid_like()));
    let kind = match req.kind {
        2 => SnapshotKind::Warm,
        _ => SnapshotKind::Cold,
    };
    let meta = state
        .manager
        .create_snapshot(id, snapshot_id.clone(), kind)
        .await
        .map_err(ApiError::from)?;
    let resp = CreateSnapshotResponse {
        snapshot_id: Some(ResourceId {
            urn: meta.snapshot_id,
        }),
        session_id: Some(ResourceId {
            urn: meta.session_id,
        }),
        drive9_manifest_hash: meta.drive9_manifest_hash,
        created_at: Some(Timestamp {
            seconds: meta.created_at,
            nanos: 0,
        }),
    };
    let (code, json) = json_response("vam.v1.CreateSnapshotResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn restore_session(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "RestoreSession", "sessions:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: RestoreSessionRequest = parse_json_body("vam.v1.RestoreSessionRequest", &body)?;
    let snapshot_id = req
        .snapshot_id
        .as_ref()
        .map(|r| r.urn.clone())
        .ok_or_else(|| OrchestratorError::InvalidContract("snapshot_id required".into()))?;
    let new_session_id = req
        .new_session_id
        .as_ref()
        .map(|r| r.urn.clone())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("sess-restored-{}", uuid_like()));
    state
        .manager
        .restore_session(snapshot_id, new_session_id.clone())
        .await
        .map_err(ApiError::from)?;
    let record = state.store.load_session(&new_session_id)?;
    let resp = RestoreSessionResponse {
        session: Some(to_session_status(&state.store, &record)),
    };
    let (code, json) = json_response("vam.v1.RestoreSessionResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn create_run(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "CreateRun", "runs:write")?;
    let idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: CreateRunRequest = parse_json_body("vam.v1.CreateRunRequest", &body)?;
    let caller = run_start::RunCaller::from_auth(&auth, request_surface(&headers));
    let opts = run_start::StartOpts {
        skip_if_busy: false,
        idempotency_key: idempotency,
        traceparent: extract_http_traceparent(&headers),
    };
    // The one start path: the queue starter calls the same function.
    match run_start::start_run(&state, &caller, req, opts).await {
        Ok(outcome) => outcome.to_http(&state),
        Err(err) => Err(err.into_api()),
    }
}

enum RunPathKind {
    Status,
    Events,
    Evidence,
    Trajectory,
    Receipt,
    Cancel,
    Messages,
    Stop,
}

fn parse_run_path(rest: &str) -> (String, RunPathKind) {
    let rest = rest.trim_start_matches('/');
    let (raw_id, kind) = if let Some(id) = rest.strip_suffix("/events") {
        (id, RunPathKind::Events)
    } else if let Some(id) = rest.strip_suffix("/evidence") {
        (id, RunPathKind::Evidence)
    } else if let Some(id) = rest.strip_suffix("/trajectory") {
        (id, RunPathKind::Trajectory)
    } else if let Some(id) = rest.strip_suffix("/receipt") {
        (id, RunPathKind::Receipt)
    } else if let Some(id) = rest.strip_suffix("/cancel") {
        (id, RunPathKind::Cancel)
    } else if let Some(id) = rest.strip_suffix("/messages") {
        (id, RunPathKind::Messages)
    } else if let Some(id) = rest.strip_suffix("/stop") {
        (id, RunPathKind::Stop)
    } else {
        (rest, RunPathKind::Status)
    };
    (percent_decode_path(raw_id), kind)
}

/// Extract validated inbound `traceparent`. Invalid/missing → `None` (new root).
/// Never used as auth (T7). Reusable for other `/v1` writes.
pub fn extract_http_traceparent(headers: &HeaderMap) -> Option<vam_telemetry::Traceparent> {
    let raw = headers.get("traceparent").and_then(|v| v.to_str().ok());
    vam_telemetry::inbound_traceparent(raw)
}

/// Optional inbound `baggage` after allowlist + guest `vam.user.id` strip (T7).
pub fn extract_http_baggage(headers: &HeaderMap) -> Option<Vec<(String, String)>> {
    let raw = headers.get("baggage").and_then(|v| v.to_str().ok());
    vam_telemetry::inbound_baggage(raw)
}

async fn run_wildcard_get(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    Query(q): Query<EventsQuery>,
) -> Result<Response, ApiError> {
    let (id, kind) = parse_run_path(&rest);
    match kind {
        RunPathKind::Status => get_run_by_id(&state, &auth, &id).await,
        RunPathKind::Events => {
            list_run_events_by_id(&state, &auth, &id, q.after.unwrap_or(0)).await
        }
        RunPathKind::Evidence => get_run_evidence_by_id(&state, &auth, &id).await,
        RunPathKind::Trajectory => get_run_trajectory_by_id(&state, &auth, &id).await,
        RunPathKind::Receipt => get_run_receipt_by_id(&state, &auth, &id).await,
        RunPathKind::Cancel | RunPathKind::Messages | RunPathKind::Stop => {
            Err(OrchestratorError::NotFound(format!("GET /v1/runs/{rest}")).into())
        }
    }
}

async fn run_wildcard_post(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let (id, kind) = parse_run_path(&rest);
    match kind {
        RunPathKind::Cancel => cancel_run_by_id(&state, &auth, &id, &headers).await,
        RunPathKind::Messages => inbox::send_run_message(&state, &auth, &id, &headers, &body).await,
        RunPathKind::Stop => stop::stop_run(&state, &auth, &id, &headers).await,
        _ => Err(OrchestratorError::NotFound(format!("POST /v1/runs/{rest}")).into()),
    }
}

async fn get_run_by_id(
    state: &AppState,
    auth: &AuthContext,
    id: &str,
) -> Result<Response, ApiError> {
    state.pdp.authorize(auth, "CreateRun", "runs:write")?;
    state.store.require_run_owner(id, auth.owner_user_id())?;
    let status = build_run_status(state, id)?;
    let resp = GetRunResponse {
        status: Some(status),
    };
    let (code, json) = json_response("vam.v1.GetRunResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn list_runs(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(page): Query<paging::PageQuery>,
) -> Result<Response, ApiError> {
    state.pdp.authorize(&auth, "CreateRun", "runs:write")?;
    let size = page.size()?;
    let before = paging::int_cursor(page.cursor("runs")?)?;
    let mut runs = state
        .store
        .list_runs_page_for_owner(auth.owner_user_id(), size + 1, before)?;
    let more = runs.len() > size;
    runs.truncate(size);
    let next = match (more, runs.last()) {
        (true, Some((_, _, rowid))) => paging::token("runs", &[rowid.to_string()]),
        _ => String::new(),
    };
    let statuses: Result<Vec<_>, ApiError> = runs
        .into_iter()
        .map(|(run_id, _, _)| build_run_status(&state, &run_id))
        .collect();
    let resp = ListRunsResponse {
        statuses: statuses?,
        pagination: Some(vam_spec::v1::PaginationResponse {
            next_page_token: next,
        }),
    };
    let (code, json) = json_response("vam.v1.ListRunsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn cancel_run_by_id(
    state: &AppState,
    auth: &AuthContext,
    id: &str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(state, true)?;
    state.pdp.authorize(auth, "CancelRun", "runs:write")?;
    let _idempotency = require_idempotency_key(headers).map_err(ApiError::from)?;
    state.store.require_run_owner(id, auth.owner_user_id())?;
    state.store.update_run_state(id, "CANCELLED")?;
    // A run waiting to be resumed is taken out of the resume cycle.
    let _ = state.store.end_resume_pending(id);
    inbox::discard_on_cancel(state, id);
    state.store.release_run_reservation(id)?;
    state.store.clear_keep_warm(id)?;
    if let Some(session_id) = state.store.get_run_session_id(id)? {
        let _ = state.manager.terminate_session(session_id).await;
    }
    // Cancel runs no finalizer; its terminal work is done.
    crate::receipt::issue_receipt_logged(&state.store, id, None);
    let status = build_run_status(state, id)?;
    let resp = vam_spec::v1::CancelRunResponse {
        status: Some(status),
    };
    let (code, json) = json_response("vam.v1.CancelRunResponse", &resp)?;
    Ok(json_http_response(code, json))
}

#[derive(serde::Deserialize)]
struct EventsQuery {
    after: Option<i64>,
}

async fn list_run_events_by_id(
    state: &AppState,
    auth: &AuthContext,
    id: &str,
    after: i64,
) -> Result<Response, ApiError> {
    state.pdp.authorize(auth, "CreateRun", "runs:write")?;
    state.store.require_run_owner(id, auth.owner_user_id())?;
    let events = state.store.read_outbox_for_run(id, after)?;
    let resp = ListRunEventsResponse {
        events: events
            .into_iter()
            .map(|e| RunEvent {
                sequence: e.sequence,
                event_type: e.event_type,
                payload: e.payload,
                emitted_at: Some(now_ts()),
            })
            .collect(),
    };
    let (code, json) = json_response("vam.v1.ListRunEventsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn get_run_evidence_by_id(
    state: &AppState,
    auth: &AuthContext,
    id: &str,
) -> Result<Response, ApiError> {
    state.pdp.authorize(auth, "CreateRun", "runs:write")?;
    state.store.require_run_owner(id, auth.owner_user_id())?;
    let pack = crate::evidence::assemble_evidence_pack(&state.store, id)?;
    let json = serde_json::to_string(&pack)
        .map_err(|e| OrchestratorError::Core(format!("failed to serialize evidence pack: {e}")))?;
    Ok(json_http_response(StatusCode::OK, json))
}

/// The run's stored receipt. Never issues one: a receipt is signed once, after
/// the run's terminal work (the finalizer, cancel and the sweeper issue it), so
/// answering early here would store an incomplete one.
async fn get_run_receipt_by_id(
    state: &AppState,
    auth: &AuthContext,
    id: &str,
) -> Result<Response, ApiError> {
    state.pdp.authorize(auth, "CreateRun", "runs:write")?;
    state.store.require_run_owner(id, auth.owner_user_id())?;
    if let Some(stored) = state.store.run_receipt(id)? {
        return Ok(json_http_response(StatusCode::OK, stored.document));
    }
    let run_state = state.store.get_run_state(id)?;
    if !crate::receipt::is_terminal(&run_state) {
        return Err(OrchestratorError::Conflict(format!(
            "run '{id}' is {run_state}; its receipt is issued when it ends"
        ))
        .into());
    }
    if crate::signing::node_keys().is_none() {
        return Err(OrchestratorError::Core(
            "node signing keys not installed; receipts cannot be issued".into(),
        )
        .into());
    }
    Err(OrchestratorError::Conflict(format!("receipt not yet issued for run '{id}'")).into())
}

async fn get_run_trajectory_by_id(
    state: &AppState,
    auth: &AuthContext,
    id: &str,
) -> Result<Response, ApiError> {
    state.pdp.authorize(auth, "CreateRun", "runs:write")?;
    state.store.require_run_owner(id, auth.owner_user_id())?;
    let layout = vam_storage::Drive9Layout::new(drive9_root());
    let chunk_store = vam_storage::ChunkStore::new(layout);
    let replay = crate::trajectory::load_trajectory_replay(&state.store, &chunk_store, id)?;
    let json = serde_json::to_string(&replay).map_err(|e| {
        OrchestratorError::Core(format!("failed to serialize trajectory replay: {e}"))
    })?;
    Ok(json_http_response(StatusCode::OK, json))
}

async fn list_approvals(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(page): Query<paging::PageQuery>,
) -> Result<Response, ApiError> {
    state
        .pdp
        .authorize(&auth, "DecideApproval", "approvals:write")?;
    let size = page.size()?;
    let after = paging::pair_cursor(page.cursor("approvals")?)?;
    let mut pending = state.store.list_published_approvals_page_for_owner(
        auth.owner_user_id(),
        size + 1,
        after,
    )?;
    let more = pending.len() > size;
    pending.truncate(size);
    let next = match (more, pending.last()) {
        (true, Some(a)) => paging::token(
            "approvals",
            &[a.created_at.to_string(), a.request_id.clone()],
        ),
        _ => String::new(),
    };
    let approvals: Vec<ApprovalRequest> = pending.iter().map(stored_to_approval_request).collect();
    let resp = ListApprovalsResponse {
        approvals,
        pagination: Some(vam_spec::v1::PaginationResponse {
            next_page_token: next,
        }),
    };
    let (code, json) = json_response("vam.v1.ListApprovalsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn get_approval(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state
        .pdp
        .authorize(&auth, "DecideApproval", "approvals:write")?;
    let stored = state
        .store
        .get_approval_for_owner(&id, auth.owner_user_id())?;
    if !stored.published {
        return Err(OrchestratorError::ApprovalNotFound(id).into());
    }
    let resp = GetApprovalResponse {
        approval: Some(stored_to_approval_request(&stored)),
        status: Some(stored_to_approval_status(&stored)),
    };
    let (code, json) = json_response("vam.v1.GetApprovalResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn decide_approval(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "DecideApproval", "approvals:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: DecideApprovalRequest = parse_json_body("vam.v1.DecideApprovalRequest", &body)?;
    let remember = Remember::try_from(req.remember).map_err(|_| {
        OrchestratorError::InvalidContract("remember must be ALWAYS_ALLOW or ALWAYS_DENY".into())
    })?;
    // "Always allow" is an approval and "Always deny" a rejection; the decision
    // field, when set, must agree.
    let remembered_rule = match remember {
        Remember::Unspecified => None,
        Remember::AlwaysAllow => Some((ScopeRule::Allow, 1, "APPROVED")),
        Remember::AlwaysDeny => Some((ScopeRule::Deny, 2, "REJECTED")),
    };
    let decision = match (req.decision, remembered_rule) {
        (0, Some((_, _, forced))) => forced,
        (given, Some((_, wanted, forced))) if given == wanted => forced,
        (_, Some((_, _, forced))) => {
            return Err(OrchestratorError::InvalidContract(format!(
                "remember {remember:?} records the decision {forced}; send decision {forced} or leave it unset"
            ))
            .into());
        }
        (1, None) => "APPROVED",
        (2, None) => "REJECTED",
        (3, None) => "AMENDED",
        (4, None) => "SUBMITTED",
        _ => {
            return Err(OrchestratorError::InvalidContract(
                "decision must be APPROVED, REJECTED, AMENDED, or SUBMITTED".into(),
            )
            .into());
        }
    };
    let request_id = ResourceId {
        urn: match req.request_id.map(|r| r.urn).filter(|u| !u.is_empty()) {
            Some(urn) => urn,
            None => id.clone(),
        },
    };
    let amended_json = req
        .amended_args
        .as_ref()
        .and_then(|s| vam_spec::encoding::proto_to_json("google.protobuf.Struct", s).ok());
    let response_json = req
        .response
        .as_ref()
        .and_then(|s| vam_spec::encoding::proto_to_json("google.protobuf.Struct", s).ok());
    let expected_revision = if req.expected_decision_revision > 0 {
        Some(req.expected_decision_revision)
    } else {
        None
    };
    // Everything after parsing is `decide::decide_as`, the one function the notification gateway
    // also calls. The surface is the client's hint (never `slack`); a standing rule is allowed here.
    let decided = decide::decide_as(
        &state,
        decide::DeciderCaller {
            user_id: auth.owner_user_id().to_string(),
            principal: auth.key.record.principal.clone(),
            surface: request_surface(&headers).to_string(),
            allow_remember: true,
        },
        request_id,
        decide::DecideInput {
            decision,
            remember: remembered_rule.map(|(rule, _, _)| rule),
            rejection_reason: req.rejection_reason.clone(),
            amended_args_json: amended_json,
            response_json,
            expected_revision,
        },
    )
    .await
    .map_err(ApiError::from)?;
    let resp = DecideApprovalResponse {
        status: Some(stored_to_approval_status(&decided.stored)),
    };
    let (code, json) = json_response("vam.v1.DecideApprovalResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// The stored `{jig, ir, data}` as the wire card. A stored card that does not parse is dropped
/// (the approval is still served, with no card).
fn approval_card_from_json(raw: &str) -> Option<vam_spec::v1::ApprovalCard> {
    let value = serde_json::from_str::<serde_json::Value>(raw).ok()?;
    Some(vam_spec::v1::ApprovalCard {
        jig: value.get("jig")?.as_str()?.to_string(),
        ir: struct_from_json_value(value.get("ir")?),
        data: struct_from_json_value(value.get("data")?),
    })
}

pub(crate) fn stored_to_approval_request(stored: &crate::error::StoredApproval) -> ApprovalRequest {
    let requested_args = serde_json::from_str::<serde_json::Value>(&stored.requested_args_json)
        .ok()
        .and_then(|v| struct_from_json_value(&v));
    let amendable_schema = serde_json::from_str::<serde_json::Value>(&stored.amendable_schema_json)
        .ok()
        .and_then(|v| struct_from_json_value(&v));
    ApprovalRequest {
        claims: Some(vam_spec::v1::ApprovalRequestClaims {
            request_id: Some(ResourceId {
                urn: stored.request_id.clone(),
            }),
            run_id: Some(ResourceId {
                urn: stored.run_id.clone(),
            }),
            step_id: stored.step_id.clone(),
            target_tool_name: stored.tool_name.clone(),
            requested_args,
            required_roles: serde_json::from_str::<Vec<String>>(&stored.required_roles_json)
                .unwrap_or_default(),
            created_at: Some(Timestamp {
                seconds: stored.created_at,
                nanos: 0,
            }),
            expires_at: stored.expires_at.map(|s| Timestamp {
                seconds: s,
                nanos: 0,
            }),
            attempt_id: Some(ResourceId {
                urn: stored.attempt_id.clone(),
            }),
            requested_args_digest: Some(vam_spec::v1::Digest {
                algorithm: 1,
                hex_hash: stored.args_digest.clone(),
            }),
            continuation_generation: stored.continuation_generation,
            approval_message: stored.approval_message.clone(),
            amendable_schema,
            kind: match stored.kind.as_str() {
                "gate" => vam_spec::v1::ApprovalKind::Gate as i32,
                "question" => vam_spec::v1::ApprovalKind::Question as i32,
                _ => vam_spec::v1::ApprovalKind::ToolCall as i32,
            },
            target: stored.target.clone(),
            always_offered: stored.always_offered,
            locked_reason: stored.locked_reason.clone(),
            card: stored
                .card_json
                .as_deref()
                .and_then(approval_card_from_json),
            ..Default::default()
        }),
        authorization: None,
    }
}

pub(crate) fn stored_to_approval_status(stored: &crate::error::StoredApproval) -> ApprovalStatus {
    let state = match stored.decision.as_deref() {
        Some("APPROVED") => 2,
        Some("AMENDED") => 3,
        Some("REJECTED") => 4,
        Some("EXPIRED") => 5,
        Some("SUBMITTED") => 7,
        Some("CANCELLED") => 6,
        _ => 1,
    };
    ApprovalStatus {
        request_id: Some(ResourceId {
            urn: stored.request_id.clone(),
        }),
        state,
        decision_revision: stored.decision_revision,
        ..Default::default()
    }
}

fn struct_from_json_value(value: &serde_json::Value) -> Option<prost_types::Struct> {
    let json = value.to_string();
    vam_spec::encoding::json_to_proto("google.protobuf.Struct", &json).ok()
}

async fn put_hosted_broker(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutHostedBroker", BROKERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: PutHostedBrokerRequest = parse_json_body("vam.v1.PutHostedBrokerRequest", &body)?;
    let manifest = req
        .manifest
        .ok_or_else(|| OrchestratorError::InvalidContract("manifest is required".into()))?;
    if req.unix_path.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("unix_path is required".into()).into());
    }
    let broker_id = manifest.broker_id.clone();
    if broker_id.trim().is_empty() {
        return Err(
            OrchestratorError::InvalidContract("manifest.broker_id is required".into()).into(),
        );
    }

    // Operator approval first, then fail-closed dial/register (endpoint only on success).
    state.store.upsert_hsp_broker_approval(&manifest)?;
    let unlisted = crate::hsp_registry::unlisted_secret_bindings(&manifest);
    if !unlisted.is_empty() {
        eprintln!(
            "[orchestrator] WARNING broker '{broker_id}' has secret bindings no tool lists in \
             required_secrets ({}); its tools receive no secrets",
            unlisted.join(", ")
        );
    }
    let unix_path = std::path::PathBuf::from(req.unix_path.trim());
    let mut client = HspClient::connect_with_timeout(
        HspEndpoint::Unix(unix_path.clone()),
        std::time::Duration::from_secs(5),
    )
    .await?;

    let register = RegisterRequest {
        broker_id: broker_id.clone(),
        broker_version: String::new(),
        manifest: Some(manifest.clone()),
    };
    let registry = HspRegistry::new(&state.store);
    let response = registry.register_admitted(&mut client, register).await?;
    if let Some(error) = response.error {
        return Err(
            OrchestratorError::Hsp(format!("hsp register denied: {}", error.message)).into(),
        );
    }

    // The gateway socket opens only for an admitted broker; a re-PUT may narrow or move it.
    state
        .store
        .set_hsp_broker_gateway(&broker_id, None, req.peer_uid)?;
    crate::gateway::sync_broker(&state, &broker_id).await?;
    let record = hosted_broker_record(&state.store, &broker_id)?;
    let resp = PutHostedBrokerResponse {
        broker: Some(record),
    };
    let (code, json) = json_response("vam.v1.PutHostedBrokerResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// `DELETE /v1/hosted/brokers/{id}`: disables the approval and closes the broker's gateway
/// socket. Idempotent for an approval that is already disabled.
async fn delete_hosted_broker(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutHostedBroker", BROKERS_ADMIN_SCOPE)?;
    let broker_id = percent_decode_path(&id);
    state
        .store
        .get_hsp_broker_approval(&broker_id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("hosted broker '{broker_id}'")))?;
    state.store.disable_hsp_broker_approval(&broker_id)?;
    crate::gateway::close_broker(&state, &broker_id)?;
    let mut record = hosted_broker_record(&state.store, &broker_id)?;
    record.enabled = false;
    let resp = GetHostedBrokerResponse {
        broker: Some(record),
    };
    let (code, json) = json_response("vam.v1.GetHostedBrokerResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn list_hosted_brokers(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let ids = state.store.list_enabled_hsp_broker_ids()?;
    let mut brokers = Vec::with_capacity(ids.len());
    for id in ids {
        brokers.push(hosted_broker_record(&state.store, &id)?);
    }
    let resp = ListHostedBrokersResponse {
        brokers,
        pagination: None,
    };
    let (code, json) = json_response("vam.v1.ListHostedBrokersResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn get_hosted_broker(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let broker_id = percent_decode_path(&id);
    let approval = state
        .store
        .get_hsp_broker_approval(&broker_id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("hosted broker '{broker_id}'")))?;
    let mut record = hosted_broker_record(&state.store, &broker_id)?;
    record.enabled = approval.enabled;
    record.manifest = Some(approval.manifest);
    let resp = GetHostedBrokerResponse {
        broker: Some(record),
    };
    let (code, json) = json_response("vam.v1.GetHostedBrokerResponse", &resp)?;
    Ok(json_http_response(code, json))
}

fn hosted_broker_record(
    store: &SqliteWorkflowStore,
    broker_id: &str,
) -> Result<HostedBrokerRecord, OrchestratorError> {
    let approval = store
        .get_hsp_broker_approval(broker_id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("hosted broker '{broker_id}'")))?;
    let unix_path = match store.get_hsp_broker_endpoint(broker_id)? {
        Some(HspEndpoint::Unix(path)) => path.to_string_lossy().into_owned(),
        Some(HspEndpoint::Tcp(addr)) => addr.to_string(),
        None => String::new(),
    };
    let tools = store
        .list_hsp_broker_tools(broker_id)?
        .into_iter()
        .map(tool_spec_from_snapshot)
        .collect();
    Ok(HostedBrokerRecord {
        manifest: Some(approval.manifest),
        unix_path,
        enabled: approval.enabled,
        tools,
        gateway_socket: store
            .get_hsp_broker_gateway(broker_id)?
            .0
            .unwrap_or_default(),
    })
}

fn tool_spec_from_snapshot(snapshot: HspBrokerToolSnapshot) -> ToolSpec {
    ToolSpec {
        name: snapshot.tool_name,
        description: snapshot.description,
        input_schema_json: snapshot.input_schema_json,
        execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
            HostExecSpec {
                adapter_id: snapshot.adapter_id,
                config: Default::default(),
            },
        )),
        required_secrets: vec![],
        side_effect: snapshot.side_effect,
        output_schema_json: String::new(),
        timeout: None,
        ..Default::default()
    }
}

async fn put_secret(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutHostSecret", "secrets:write")?;
    let idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: PutHostSecretRequest = parse_json_body("vam.v1.PutHostSecretRequest", &body)?;
    if req.binding_id.is_empty() {
        return Err(OrchestratorError::InvalidContract("binding_id is required".into()).into());
    }
    let kind = SecretKind::from_proto(req.kind)?;
    let scope = resolve_secret_scope(SecretScope::from_proto(req.scope)?, auth.role());
    authorize_org_secret_write(scope, auth.role())?;
    let idem = if req.idempotency_key.is_empty() {
        Some(idempotency.as_str())
    } else {
        Some(req.idempotency_key.as_str())
    };
    let meta = state.vault.put_secret_at(
        &req.binding_id,
        &req.value,
        kind,
        &req.allowed_destinations,
        idem,
        scope,
        vault_owner_user_id(scope, auth.owner_user_id()),
    )?;
    let resp = PutHostSecretResponse {
        secret: Some(to_proto_secret(&meta)),
    };
    let (code, json) = json_response("vam.v1.PutHostSecretResponse", &resp)?;
    Ok(json_http_response(code, json))
}

#[derive(serde::Deserialize, Default)]
struct SecretQuery {
    scope: Option<String>,
}

fn requested_secret_scope(query: &SecretQuery) -> Result<Option<SecretScope>, OrchestratorError> {
    match query
        .scope
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        None => Ok(None),
        Some(raw) => SecretScope::parse(raw).map(Some),
    }
}

async fn list_secrets(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(query): Query<SecretQuery>,
) -> Result<Response, ApiError> {
    state
        .pdp
        .authorize(&auth, "ListHostSecrets", "secrets:write")?;
    let filter = requested_secret_scope(&query)?;
    let mut secrets = state.vault.list_visible_metadata(auth.owner_user_id())?;
    if let Some(scope) = filter {
        secrets.retain(|meta| meta.scope == scope);
    }
    let resp = ListHostSecretsResponse {
        secrets: secrets.iter().map(to_proto_secret).collect(),
    };
    let (code, json) = json_response("vam.v1.ListHostSecretsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn get_secret(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    Query(query): Query<SecretQuery>,
) -> Result<Response, ApiError> {
    state
        .pdp
        .authorize(&auth, "GetHostSecret", "secrets:write")?;
    let binding_id = percent_decode_path(&id);
    let scope = resolve_secret_scope(requested_secret_scope(&query)?, auth.role());
    let meta = state.vault.get_metadata_at(
        scope,
        vault_owner_user_id(scope, auth.owner_user_id()),
        &binding_id,
    )?;
    let resp = HostSecretMetadata {
        ..to_proto_secret(&meta)
    };
    let (code, json) = json_response("vam.v1.HostSecretMetadata", &resp)?;
    Ok(json_http_response(code, json))
}

async fn delete_secret(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    Query(query): Query<SecretQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "DeleteHostSecret", "secrets:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let binding_id = percent_decode_path(&id);
    let scope = resolve_secret_scope(requested_secret_scope(&query)?, auth.role());
    authorize_org_secret_write(scope, auth.role())?;
    let meta = state.vault.delete_secret_at(
        scope,
        vault_owner_user_id(scope, auth.owner_user_id()),
        &binding_id,
    )?;
    let resp = DeleteHostSecretResponse {
        secret: Some(to_proto_secret(&meta)),
    };
    let (code, json) = json_response("vam.v1.DeleteHostSecretResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// Discovery for the VAM app: resolve declared `SecretRequirement`s against the
/// caller's vault and report configured vs missing.
///
/// Only checks org rows plus the caller's own user-scope row. Another user's
/// user-scope rows are never revealed; org existence is shared on purpose.
/// Always returns 200; required-but-missing entries are also listed in
/// `missing` for fail-fast surfacing at run create / skill load.
async fn resolve_secret_requirements(
    State(state): State<AppState>,
    auth: AuthContext,
    body: String,
) -> Result<Response, ApiError> {
    state
        .pdp
        .authorize(&auth, "ListHostSecrets", "secrets:write")?;
    let req: ResolveSecretRequirementsRequest =
        parse_json_body("vam.v1.ResolveSecretRequirementsRequest", &body)?;
    let statuses = crate::secret_requirements::discover_requirements(
        &state.vault,
        auth.owner_user_id(),
        &req.requirements,
    )?;
    let missing: Vec<vam_spec::v1::SecretRequirementStatus> = statuses
        .iter()
        .filter(|s| s.required && !s.configured)
        .cloned()
        .collect();
    let resp = ResolveSecretRequirementsResponse { statuses, missing };
    let (code, json) = json_response("vam.v1.ResolveSecretRequirementsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// Run-create admission: collect `required_secrets` across the agent's
/// `skill_refs` and fail fast with binding + scope when required secrets are
/// missing from the caller's vault. Missing skills are skipped here; skill
/// resolution reports them separately.
fn admit_run_secret_requirements(
    state: &AppState,
    owner_user_id: &str,
    agent: &vam_spec::v1::AgentSpec,
) -> Result<(), ApiError> {
    let mut requirements = Vec::new();
    for skill_ref in &agent.skill_refs {
        let urn = skill_ref
            .resource_id
            .as_ref()
            .map(|r| r.urn.as_str())
            .unwrap_or_default();
        if urn.is_empty() {
            continue;
        }
        match state.store.get_skill(urn) {
            Ok(skill) => {
                for tool in &skill.tools {
                    requirements.extend(tool.required_secrets.iter().cloned());
                }
            }
            Err(OrchestratorError::NotFound(_)) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    crate::secret_requirements::admit_requirements(&state.vault, owner_user_id, &requirements)
        .map_err(ApiError::from)?;
    Ok(())
}

pub(crate) fn to_proto_secret(meta: &StoreSecretMetadata) -> HostSecretMetadata {
    HostSecretMetadata {
        binding_id: meta.binding_id.clone(),
        kind: meta.kind.to_proto(),
        fingerprint: meta.fingerprint.clone(),
        allowed_destinations: meta.allowed_destinations.clone(),
        created_at: Some(Timestamp {
            seconds: meta.created_at,
            nanos: 0,
        }),
        updated_at: Some(Timestamp {
            seconds: meta.updated_at,
            nanos: 0,
        }),
        last_used_at: meta.last_used_at.map(|s| Timestamp {
            seconds: s,
            nanos: 0,
        }),
        scope: meta.scope.to_proto(),
        owner_user_id: meta.owner_user_id.clone(),
    }
}

pub(crate) fn to_proto_key(record: &ApiKeyRecord) -> ProtoApiKeyRecord {
    ProtoApiKeyRecord {
        key_id: Some(ResourceId {
            urn: record.key_id.clone(),
        }),
        name: record.name.clone(),
        principal: record.principal.clone(),
        scopes: record.scopes.clone(),
        created_at: Some(Timestamp {
            seconds: record.created_at,
            nanos: 0,
        }),
        last_used_at: record.last_used_at.map(|s| Timestamp {
            seconds: s,
            nanos: 0,
        }),
        revoked_at: record.revoked_at.map(|s| Timestamp {
            seconds: s,
            nanos: 0,
        }),
        user_id: record.owner_user_id.clone(),
        holder: record.holder().to_string(),
        target_kind: record.target_kind.clone().unwrap_or_default(),
        target_id: record.target_id.clone().unwrap_or_default(),
        issued_via: record.issued_via.clone().unwrap_or_default(),
        tool_name: record.tool_name.clone().unwrap_or_default(),
        expires_at: record.expires_at.map(|s| Timestamp {
            seconds: s,
            nanos: 0,
        }),
        ..Default::default()
    }
}

/// [`to_proto_key`] plus what a person needs to recognise a grant: the label of the Space (and
/// thread) it points at, read for the grant's owner, and a short preview of its bound message.
pub(crate) fn to_proto_key_with_labels(
    store: &crate::workflow::SqliteWorkflowStore,
    record: &ApiKeyRecord,
) -> ProtoApiKeyRecord {
    let mut proto = to_proto_key(record);
    if record.is_http() {
        return proto;
    }
    let constraints: serde_json::Value = record
        .constraints_json
        .as_deref()
        .and_then(|j| serde_json::from_str(j).ok())
        .unwrap_or(serde_json::Value::Null);
    if record.target_kind.as_deref() == Some("space") {
        if let Some(space_id) = record.target_id.as_deref() {
            let mut label = store
                .get_space(&record.owner_user_id, space_id)
                .map(|s| s.label)
                .unwrap_or_default();
            if let Some(thread_id) = constraints.get("thread_id").and_then(|v| v.as_str()) {
                if let Ok(t) = store.get_thread(&record.owner_user_id, space_id, thread_id) {
                    label = format!("{label} / {}", t.label);
                }
            }
            proto.target_label = label;
        }
    }
    if let Some(message) = constraints.get("message").and_then(|v| v.as_str()) {
        proto.message_preview = message_preview(message);
    }
    proto
}

/// The first line of `message`, at most 80 characters.
fn message_preview(message: &str) -> String {
    let line = message.lines().next().unwrap_or_default().trim();
    let mut out: String = line.chars().take(80).collect();
    if line.chars().count() > 80 || message.trim().lines().count() > 1 {
        out.push('\u{2026}');
    }
    out
}

fn to_session_status(store: &SqliteWorkflowStore, record: &SessionRecord) -> SessionStatus {
    let agent_urn = store
        .find_agent_urn_for_session(&record.session_id)
        .ok()
        .flatten()
        .unwrap_or_default();
    SessionStatus {
        session_id: Some(ResourceId {
            urn: record.session_id.clone(),
        }),
        idempotency_key: record.idempotency_key.clone().unwrap_or_default(),
        observed_state: map_observed(record.observed_state),
        desired_state: map_desired(record.desired_state),
        vm_id: record.vm_id.clone(),
        workspace_id: record.workspace_id.clone(),
        storage_profile_id: record.storage_profile_id.clone(),
        guest_cid: record.guest_cid.unwrap_or(0) as i32,
        created_at: Some(Timestamp {
            seconds: record.created_at,
            nanos: 0,
        }),
        updated_at: Some(Timestamp {
            seconds: record.updated_at,
            nanos: 0,
        }),
        agent_urn,
    }
}

fn map_observed(state: ObservedState) -> i32 {
    match state {
        ObservedState::Creating => 1,
        ObservedState::Booting => 2,
        ObservedState::Running => 3,
        ObservedState::Paused => 4,
        ObservedState::Terminating => 5,
        ObservedState::Terminated => 6,
        ObservedState::Failed => 7,
    }
}

fn map_desired(state: DesiredState) -> i32 {
    match state {
        DesiredState::Created => 1,
        DesiredState::Running => 3,
        DesiredState::Paused => 4,
        DesiredState::Terminated => 6,
    }
}

/// Where the client says a request came from (`X-Vam-Surface`): `cli`, `app`,
/// else `api`. A declaration by the client, not a proof.
fn request_surface(headers: &HeaderMap) -> &'static str {
    crate::run_facts::surface_from_header(
        headers
            .get(crate::run_facts::SURFACE_HEADER)
            .and_then(|v| v.to_str().ok()),
    )
}

fn ts_from_unix(seconds: i64) -> Timestamp {
    Timestamp { seconds, nanos: 0 }
}

fn build_run_status(state: &AppState, run_id: &str) -> Result<RunStatus, ApiError> {
    let run_state = state.store.get_run_state(run_id).map_err(ApiError::from)?;
    let state_upper = run_state.to_ascii_uppercase();
    let state_enum = match state_upper.as_str() {
        "RUNNING" | "RUN_STATE_RUNNING" => run_status::RunState::Running,
        "SUCCEEDED" | "RUN_STATE_SUCCEEDED" => run_status::RunState::Succeeded,
        "FAILED" | "RUN_STATE_FAILED" => run_status::RunState::Failed,
        "CANCELLED" | "RUN_STATE_CANCELLED" => run_status::RunState::Cancelled,
        "PAUSED" | "RUN_STATE_PAUSED" => run_status::RunState::Paused,
        "TIMED_OUT" | "RUN_STATE_TIMED_OUT" => run_status::RunState::TimedOut,
        "EXPIRED" | "RUN_STATE_EXPIRED" => run_status::RunState::Expired,
        "REJECTED" | "RUN_STATE_REJECTED" => run_status::RunState::Rejected,
        _ => run_status::RunState::Pending,
    };
    let final_outputs = state
        .store
        .get_run_result_json(run_id)
        .ok()
        .flatten()
        .and_then(|json| json_object_to_struct(&json));

    let (spec_json, session_id, agent_urn) = state
        .store
        .get_run_spec_context(run_id)
        .map_err(ApiError::from)?;
    let mut thread_ref = crate::run_context::DEFAULT_THREAD_REF.to_string();
    let mut workspace_id = String::new();
    let mut space_id = String::new();
    let mut thread_id = String::new();
    let mut prompt = String::new();
    if let Some(json) = spec_json {
        if let Ok(spec) =
            vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &json)
        {
            thread_ref = crate::run_context::extract_thread_ref(&spec)
                .unwrap_or_else(|_| crate::run_context::DEFAULT_THREAD_REF.to_string());
            if let Ok(ws) = crate::run_context::parse_workspace_ref_uri(&spec.workspace_ref_uri) {
                workspace_id = ws;
            }
            prompt = extract_prompt_from_inputs(spec.inputs.as_ref());
            space_id = spec.space_id.clone();
            thread_id = spec.thread_id.clone();
        }
    }
    // `threadRef` is the thread's label; the ids are the identity. A run without ids
    // (older rows) keeps the stored `threadRef`.
    if !space_id.is_empty() && !thread_id.is_empty() {
        if let Ok(owner) = state.store.run_owner_user_id(run_id) {
            if let Ok(row) = state.store.get_thread(&owner, &space_id, &thread_id) {
                thread_ref = row.label;
            }
        }
    }
    let step_statuses = state
        .store
        .list_playbook_steps(run_id)
        .map_err(ApiError::from)?
        .iter()
        .map(map_playbook_step_record)
        .collect();

    // The workspace on both sides of the run, once the run has ended.
    let final_outputs = match (
        state_enum as i32,
        state.store.run_workspace_heads(run_id).ok().flatten(),
    ) {
        (ended, Some(heads))
            if ended == run_status::RunState::Succeeded as i32
                || ended == run_status::RunState::Failed as i32
                || ended == run_status::RunState::Cancelled as i32 =>
        {
            let mut outputs = final_outputs.unwrap_or_default();
            outputs.fields.insert(
                "workspace".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StructValue(workspace_struct(
                        &heads,
                    ))),
                },
            );
            Some(outputs)
        }
        (_, _) => final_outputs,
    };

    // Real start and end; unset for a run that has none recorded.
    let run_times = state
        .store
        .run_facts(run_id)
        .map(|f| (f.created_at, f.ended_at))
        .unwrap_or((None, None));

    let source = run_source_of(state, run_id);

    Ok(RunStatus {
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        source: Some(source),
        state: state_enum as i32,
        started_at: run_times.0.map(ts_from_unix),
        finished_at: run_times.1.map(ts_from_unix),
        final_outputs,
        agent_urn: agent_urn.unwrap_or_default(),
        session_id: session_id.unwrap_or_default(),
        thread_ref,
        workspace_id,
        space_id,
        thread_id,
        prompt,
        step_statuses,
        waiting_for: question::waiting_for(&state.store, run_id, state_enum as i32),
        // Times the task was picked up again after a guest crash or a restart (both kinds).
        resumes: state.store.run_resumes(run_id) as i32,
        ..Default::default()
    })
}

/// What started the run: a person, or the trigger recorded with the request.
fn run_source_of(state: &AppState, run_id: &str) -> vam_spec::v1::RunSource {
    let trigger = state.store.run_facts(run_id).ok().and_then(|f| f.trigger);
    match trigger {
        Some(t) => vam_spec::v1::RunSource {
            kind: "trigger".into(),
            broker_id: t.broker_id,
            grant_id: t.grant_id,
            trigger_ref: t.trigger_ref,
            fire_at: Some(ts_from_unix(t.fire_at)),
            fire_kind: t.fire_kind,
        },
        None => vam_spec::v1::RunSource {
            kind: "person".into(),
            ..Default::default()
        },
    }
}

fn map_playbook_step_record(record: &PlaybookStepRecord) -> StepExecutionStatus {
    use vam_spec::v1::step_execution_status::StepState;

    let state = match record.state {
        PlaybookStepState::Pending => StepState::Queued,
        PlaybookStepState::Available => StepState::Available,
        PlaybookStepState::Open => StepState::Paused,
        PlaybookStepState::Completed => StepState::Success,
        PlaybookStepState::Skipped => StepState::Skipped,
    };
    StepExecutionStatus {
        step_id: record.step_id.clone(),
        state: state as i32,
        started_at: record.started_at.map(unix_seconds_to_timestamp),
        finished_at: record.finished_at.map(unix_seconds_to_timestamp),
        output: record
            .output_json
            .as_deref()
            .and_then(json_object_to_struct),
        transition_revision: record.transition_revision,
        outcome: record.outcome.clone(),
        summary: record.agent_summary.clone().unwrap_or_default(),
        evidence_refs: record.evidence_refs.clone(),
        ..Default::default()
    }
}

fn unix_seconds_to_timestamp(seconds: i64) -> Timestamp {
    Timestamp { seconds, nanos: 0 }
}

fn extract_prompt_from_inputs(inputs: Option<&prost_types::Struct>) -> String {
    let Some(inputs) = inputs else {
        return String::new();
    };
    string_from_struct_field(inputs, "prompt").unwrap_or_else(|| {
        inputs
            .fields
            .get("fields")
            .and_then(|v| {
                if let prost_types::value::Kind::StructValue(nested) = v.kind.as_ref()? {
                    string_from_struct_field(nested, "prompt")
                } else {
                    None
                }
            })
            .unwrap_or_default()
    })
}

fn string_from_struct_field(s: &prost_types::Struct, key: &str) -> Option<String> {
    s.fields.get(key).and_then(string_from_value)
}

fn string_from_value(v: &prost_types::Value) -> Option<String> {
    match v.kind.as_ref()? {
        prost_types::value::Kind::StringValue(s) => Some(s.clone()),
        prost_types::value::Kind::StructValue(nested) => {
            nested.fields.get("stringValue").and_then(string_from_value)
        }
        _ => None,
    }
}

/// `finalOutputs.workspace`: the HEAD before and after the run and the hashes
/// of its files. `headAfter` is absent while the run's end commit is pending
/// or failed.
fn workspace_struct(heads: &crate::session_end::RunWorkspaceHeads) -> prost_types::Struct {
    let text = |s: &str| prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(s.to_string())),
    };
    let mut out = prost_types::Struct::default();
    out.fields
        .insert("headBefore".to_string(), text(&heads.head_before));
    out.fields.insert(
        "filesHashBefore".to_string(),
        text(&heads.files_hash_before),
    );
    if let Some(after) = &heads.head_after {
        out.fields.insert("headAfter".to_string(), text(after));
    }
    if let Some(after) = &heads.files_hash_after {
        out.fields.insert("filesHashAfter".to_string(), text(after));
    }
    out
}

fn json_object_to_struct(json: &str) -> Option<prost_types::Struct> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let obj = value.as_object()?;
    let mut out = prost_types::Struct::default();
    for (key, val) in obj {
        let kind = match val {
            serde_json::Value::Null => Some(prost_types::value::Kind::NullValue(0)),
            serde_json::Value::Bool(b) => Some(prost_types::value::Kind::BoolValue(*b)),
            serde_json::Value::Number(n) => n.as_f64().map(prost_types::value::Kind::NumberValue),
            serde_json::Value::String(s) => Some(prost_types::value::Kind::StringValue(s.clone())),
            other => Some(prost_types::value::Kind::StringValue(other.to_string())),
        };
        out.fields.insert(key.clone(), prost_types::Value { kind });
    }
    Some(out)
}

mod run_status {
    #[allow(dead_code)]
    #[derive(Clone, Copy)]
    pub enum RunState {
        Unspecified = 0,
        Pending = 1,
        Running = 2,
        Paused = 3,
        Succeeded = 4,
        Failed = 5,
        Cancelled = 6,
        TimedOut = 7,
        Expired = 8,
        Rejected = 9,
    }
}

pub(crate) fn drive9_root() -> std::path::PathBuf {
    std::env::var("VAM_DRIVE9_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/vam/drive9"))
}

fn materialized_dir_for_session(session_id: &str) -> std::path::PathBuf {
    let layout = vam_storage::Drive9Layout::new(drive9_root());
    layout.session_dir(session_id).join("materialized")
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
async fn run_lifecycle(
    manager: SessionManager,
    store: Arc<SqliteWorkflowStore>,
    run_id: String,
    session_id: String,
    workspace_id: String,
    thread_ref: String,
    spec_idem: String,
    spec_json: String,
    agent_urn: String,
    agent_digest: String,
    user_prompt: String,
    mut run_inputs: Option<prost_types::Struct>,
    model: String,
    owner: String,
    owner_user_id: String,
    timer: Option<crate::timing::SharedPhaseTimer>,
    inbound_parent: Option<vam_telemetry::Traceparent>,
) {
    let run_span = tracing::info_span!(
        "run",
        run_id = %run_id,
        session_id = %session_id,
        workspace_id = %workspace_id,
        thread_ref = %thread_ref,
        agent_urn = %agent_urn,
        model = %model,
        owner = %owner,
        "vam.user.id" = %owner_user_id,
    );
    vam_telemetry::apply_inbound_parent(&run_span, inbound_parent.as_ref());
    let _run_guard = run_span.enter();

    let vm_id = format!("vm-{}", session_id);
    let boot_span = tracing::info_span!("session_boot", vm_id = %vm_id, session_id = %session_id);
    let boot_result = {
        let _boot_guard = boot_span.enter();
        manager
            .create_session(
                session_id.clone(),
                Some(format!("run-session-{}", spec_idem)),
                vm_id,
                workspace_id.clone(),
                "default".to_string(),
                timer.clone(),
            )
            .await
    };

    if let Err(e) = boot_result {
        tracing::error!("[orchestrator] run session boot failed: {}", e);
        vam_telemetry::VamMetricsRegistry::global()
            .record_counter(
                vam_telemetry::METRIC_ADMISSION_REQUESTS_TOTAL,
                1,
                &[("admission_status", "denied")],
            )
            .ok();
        let _ = manager.terminate_session(session_id.clone()).await;
        let _ = store.update_run_state(&run_id, "FAILED");
        return;
    }

    vam_telemetry::VamMetricsRegistry::global()
        .record_counter(
            vam_telemetry::METRIC_ADMISSION_REQUESTS_TOTAL,
            1,
            &[("admission_status", "allowed")],
        )
        .ok();

    // From here a stop of this session is settled by this function's finalizer.
    let _tracking = crate::session_end::RunTracking::new(store.clone(), &session_id, &run_id);
    let heads_before = record_run_heads_before(&store, &run_id, &session_id, &workspace_id);
    record_run_isolation_fact(&store, &run_id, &session_id).await;

    let _ = store.persist_run_spec(
        &run_id,
        &spec_json,
        Some(&session_id),
        &agent_urn,
        Some(&spec_idem),
        &owner,
    );

    let materialized = materialized_dir_for_session(&session_id);
    let prep_span = tracing::info_span!("prepare_thread_context", workspace_id = %workspace_id, thread_ref = %thread_ref);
    let thread_start = {
        let _prep_guard = prep_span.enter();
        match prepare_thread_context(
            &materialized,
            &workspace_id,
            &thread_ref,
            &run_id,
            &agent_urn,
            &agent_digest,
            &spec_json,
            &user_prompt,
        ) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("[orchestrator] thread context setup failed: {}", e);
                let result_json =
                    serde_json::json!({ "error": format!("thread context setup failed: {e}") })
                        .to_string();
                store.remove_thread_recorder(&session_id);
                let _ = store.complete_run_with_result(&run_id, "FAILED", &result_json);
                let _ = manager.terminate_session(session_id).await;
                return;
            }
        }
    };
    let replayed_prompt = apply_thread_start(
        store.as_ref(),
        &vam_storage::Drive9Layout::new(drive9_root()),
        &run_id,
        &session_id,
        &user_prompt,
        thread_start,
        &mut run_inputs,
    );

    // The thread log opens the task before the guest can send a request. The
    // prompt is the one the user typed, not the replayed history.
    if let Some(recorder) = store.thread_recorder(&session_id) {
        if let Some((head, files_hash)) = &heads_before {
            recorder.set_heads_before(head, files_hash);
        }
        recorder
            .record_task_start(&user_prompt, &model, &agent_digest, false)
            .await;
    }

    let dispatch_span =
        tracing::info_span!("dispatch_prompt", run_id = %run_id, session_id = %session_id);
    let dispatch_result = {
        let _disp_guard = dispatch_span.enter();
        dispatch_run_prompt(
            store.clone(),
            run_id.clone(),
            session_id.clone(),
            replayed_prompt.clone(),
            run_inputs,
            timer,
        )
        .await
    };

    match dispatch_result {
        Ok(outcome) => {
            if outcome.paused {
                tracing::info!(
                    "[orchestrator] run {} paused awaiting HITL approval (session kept alive)",
                    run_id
                );
                return;
            }
            if outcome.interrupted {
                resume_run::interrupt_or_end(
                    &manager,
                    &store,
                    &run_id,
                    &session_id,
                    crate::resume_store::REASON_GUEST_CRASHED,
                )
                .await;
                return;
            }
            finalize_run_session(
                &manager,
                store.as_ref(),
                &run_id,
                &session_id,
                &workspace_id,
                &thread_ref,
                &user_prompt,
                &model,
                &outcome,
            )
            .await;
        }
        Err(e) => {
            tracing::error!("[orchestrator] run dispatch failed: {}", e);
            let spend_micro = store
                .get_session_budget(&session_id)
                .map(|b| b.get_accumulated_cost());
            finalize_run_session(
                &manager,
                store.as_ref(),
                &run_id,
                &session_id,
                &workspace_id,
                &thread_ref,
                &user_prompt,
                &model,
                &DispatchOutcome {
                    answer: String::new(),
                    succeeded: false,
                    paused: false,
                    interrupted: false,
                    spend_micro,
                },
            )
            .await;
        }
    }
}

/// Post-dispatch teardown shared by normal runs and terminal HITL resumes.
///
/// Callers must **skip** this when `DispatchOutcome.paused` is true (nested HITL):
/// the session stays alive awaiting another approval. On success, appends the turn
/// to the thread timeline; always terminates the session and records a cold snapshot.
#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
async fn finalize_run_session(
    manager: &SessionManager,
    store: &SqliteWorkflowStore,
    run_id: &str,
    session_id: &str,
    workspace_id: &str,
    thread_ref: &str,
    user_prompt: &str,
    model: &str,
    outcome: &DispatchOutcome,
) {
    let finalize_span =
        tracing::info_span!("finalize_run_session", run_id = %run_id, session_id = %session_id);
    let _fin_guard = finalize_span.enter();

    let raw_outbox_events = store.read_outbox_for_run(run_id, 0).unwrap_or_default();
    // The workspace the task started from, as recorded when it started: the
    // HEAD before. (The HEAD after is only known once the end commit is made.)
    let heads_before = store.run_workspace_heads(run_id).ok().flatten();
    let head_before = heads_before
        .as_ref()
        .map(|h| h.head_before.clone())
        .unwrap_or_else(|| read_workspace_head(workspace_id).unwrap_or_default());

    // Persist canonical CAS trajectory replay blob (Phase 6.5)
    let layout = vam_storage::Drive9Layout::new(drive9_root());
    let chunk_store = vam_storage::ChunkStore::new(layout.clone());
    let trajectory_replay_digest = match crate::trajectory::persist_trajectory_replay_blob(
        &raw_outbox_events,
        &chunk_store,
    ) {
        Ok(digest) => {
            tracing::info!(trajectory_replay_digest = %digest, "[orchestrator] persisted trajectory replay blob to CAS");
            digest
        }
        Err(e) => {
            tracing::warn!(
                "[orchestrator] failed to persist trajectory replay blob: {}",
                e
            );
            String::new()
        }
    };

    // The thread log gets its `task_end` before the timeline turn (which records
    // the run's log range), the attestation (which carries the head) and the
    // session end (whose commit keeps the log). A recorder failure is counted
    // inside the recorder and never changes the outcome.
    let recorder = store.thread_recorder(session_id);
    // A stop request ends the task as `stopped`; a run that had already
    // succeeded when the stop came ends as the success it is.
    if outcome.succeeded {
        store.session_ends.demote_stop(session_id);
    }
    let stopped = !outcome.succeeded && store.session_ends.stop_requested(session_id);
    // From here this function owns the end of the session: a stop now waits
    // for it. The VM goes down first so nothing writes while the end is fixed;
    // then the files of the end commit (and their hash) are fixed, so the
    // conversation can record the hash before the files are composed into HEAD.
    store.session_ends.begin_finalize(session_id);
    if let Err(e) = manager.stop_session_vm(session_id.to_string()).await {
        tracing::error!("[orchestrator] post-run VM teardown failed: {}", e);
        return;
    }
    let disposition = if stopped {
        crate::session_end::Disposition::Stopped
    } else {
        crate::session_end::Disposition::Live
    };
    let end_summary = match manager
        .prepare_session_end(session_id.to_string(), disposition)
        .await
    {
        Ok(summary) => Some(summary),
        Err(e) => {
            tracing::warn!(
                "[orchestrator] the files of the end commit could not be fixed first ({}); \
                 the end is committed the old way",
                e
            );
            None
        }
    };
    if let (Some(recorder), Some(summary)) = (recorder.as_ref(), end_summary.as_ref()) {
        recorder.set_end_commit(&summary.files_hash, summary.base, &summary.dropped);
    }
    let log_head = match recorder.as_ref() {
        Some(recorder) if stopped => recorder.record_stop().await,
        Some(recorder) => {
            recorder
                .record_task_end(
                    if outcome.succeeded {
                        "succeeded"
                    } else {
                        "failed"
                    },
                    &outcome.answer,
                    "",
                )
                .await
        }
        None => None,
    };
    let log_range = match (recorder.as_ref().and_then(|r| r.run_first_seq()), &log_head) {
        (Some(from), Some(head)) => Some(vam_storage::TurnLogRange {
            seq_from: from,
            seq_to: head.seq,
            head_digest: head.digest.clone(),
        }),
        _ => None,
    };

    // The timeline turn on every end that reached here (a stop is Wave 3).
    {
        let timeline_span = tracing::info_span!("append_timeline", thread_ref = %thread_ref);
        let _tl_guard = timeline_span.enter();
        let materialized = materialized_dir_for_session(session_id);
        let trajectory = Some(crate::trajectory::summarize_trajectory(&raw_outbox_events));
        let answer = if outcome.succeeded || !outcome.answer.is_empty() {
            outcome.answer.as_str()
        } else if stopped {
            "(the task was stopped)"
        } else {
            "(the task did not finish)"
        };
        let turn_workspace = end_summary
            .as_ref()
            .map(|summary| vam_storage::TurnWorkspace {
                head_before: head_before.clone(),
                files_hash: summary.files_hash.clone(),
            });
        if let Err(e) = vam_storage::append_turn_full(
            &materialized,
            thread_ref,
            run_id,
            user_prompt,
            answer,
            if model.is_empty() { "unknown" } else { model },
            &head_before,
            outcome.spend_micro,
            trajectory.as_ref(),
            log_range.as_ref(),
            turn_workspace.as_ref(),
        ) {
            tracing::error!("[orchestrator] timeline append failed: {}", e);
        }
    }

    // Settle the workspace: HEAD := the planned files + the host `.vam/` (which
    // now holds the end of the task). Only then is the run's end reported.
    let session_record = store.load_session(session_id).ok();
    let term_span = tracing::info_span!("terminate_session", session_id = %session_id);
    let term_res = {
        let _t_guard = term_span.enter();
        manager.terminate_session(session_id.to_string()).await
    };
    // The session is over: its recorder goes with it (sealed since `task_end`).
    store.remove_thread_recorder(session_id);

    // The assembled contexts granted to this run are no longer needed.
    {
        let layout = vam_storage::Drive9Layout::new(drive9_root());
        for digest in store.revoke_run_context_grants(run_id) {
            crate::thread_context::remove_context(&layout, session_id, &digest);
        }
    }

    if let Err(e) = term_res {
        tracing::error!("[orchestrator] post-run session terminate failed: {}", e);
        return;
    }

    // The workspace commit failed and its tree was kept: the run ends FAILED
    // with the storage reason (the work is safe under drive9/kept). Otherwise
    // the result the dispatch staged is published as it was.
    let kept = store.kept_for_session(session_id).ok().flatten();
    let kept_result = kept.as_ref().map(|kept| {
        serde_json::json!({
            "error": "the workspace could not be committed; the work was kept",
            "reason": crate::turn_commit::RUN_ERROR_STORAGE_COMMIT_FAILED,
            "kept_id": kept.kept_id,
        })
        .to_string()
    });
    if let Err(e) =
        store.publish_run_result(run_id, kept_result.as_deref().map(|json| ("FAILED", json)))
    {
        tracing::error!("[orchestrator] run result not published: {}", e);
    }
    if let Err(e) = dispatch_on_complete_for_terminal(store, run_id, session_id).await {
        tracing::warn!("[orchestrator] on_complete hooks failed: {}", e);
    }
    let heads_after = store.run_workspace_heads(run_id).ok().flatten();
    let head_after = heads_after
        .as_ref()
        .and_then(|h| h.head_after.clone())
        .unwrap_or_default();
    let workspace_receipt = match (&heads_after, kept.is_none()) {
        (Some(heads), true) if !head_after.is_empty() => Some(vam_telemetry::WorkspaceReceipt {
            head_before: heads.head_before.clone(),
            head_after: head_after.clone(),
            files_hash: heads.files_hash_after.clone().unwrap_or_default(),
        }),
        _ => None,
    };

    // Emit Signed Run-Outcome Attestation (Phase 6.2)
    let evaluations = store.get_evaluations_for_run(run_id).unwrap_or_default();
    let eval_summary =
        evaluations
            .into_iter()
            .next()
            .map(
                |(assertion_id, evaluator_id, passed)| vam_telemetry::EvaluationSummary {
                    assertion_id,
                    evaluator_id,
                    passed,
                },
            );

    let budget = store.get_session_budget(session_id);
    let cost_summary = budget.as_ref().map(|b| vam_telemetry::CostSummary {
        currency: b
            .max_financial_budget
            .as_ref()
            .map(|c| c.currency.clone())
            .unwrap_or_else(|| "USD".to_string()),
        spend_micro: b.get_accumulated_cost(),
        prompt_tokens: b.get_accumulated_prompt_tokens(),
        completion_tokens: b.get_accumulated_completion_tokens(),
        total_tokens: b.get_accumulated_tokens(),
        pricing_catalog_version: Some("pricing-v1".to_string()),
    });

    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let attestation = vam_telemetry::RunOutcomeAttestation {
        run_id: run_id.to_string(),
        session_id: session_id.to_string(),
        goal: user_prompt.to_string(),
        terminal_state: if outcome.succeeded {
            "SUCCEEDED".to_string()
        } else {
            "FAILED".to_string()
        },
        terminator: if outcome.succeeded {
            "agent".to_string()
        } else {
            "system".to_string()
        },
        trajectory_digest: trajectory_replay_digest.clone(),
        artifacts: Vec::new(),
        paths: if head_after.is_empty() {
            Vec::new()
        } else {
            vec![format!(
                "drive9://workspace/{}/manifests/{}",
                workspace_id, head_after
            )]
        },
        skills: Vec::new(),
        tools: Vec::new(),
        eval: eval_summary,
        cost: cost_summary,
        timestamp_unix_ms: now_ms,
        thread_log_head_seq: log_head.as_ref().map(|h| h.seq),
        thread_log_head_digest: log_head.as_ref().map(|h| h.digest.clone()),
        workspace: workspace_receipt,
    };

    // Signed by the node key, so the envelope's key id is the one `GET /v1/node` lists.
    let signer = crate::signing::node_keys().map(|k| k.current().approval_signer());
    if signer.is_none() {
        tracing::error!(
            run_id,
            "node signing keys not installed; run attestation not issued"
        );
    }
    if let Some(Ok(envelope)) = signer.as_ref().map(|s| attestation.sign(s)) {
        if let Ok(json_payload) =
            vam_spec::encoding::proto_to_json("vam.v1.SignedEnvelope", &envelope)
        {
            let _ = store.append_outbox_event(run_id, "vam.v1.run.attested", &json_payload);
            let _ = store.record_audit_event(
                session_id,
                "vam_audit_attestation",
                json_payload.as_bytes(),
            );
            tracing::info!(
                "[orchestrator] run outcome attestation emitted for run {}",
                run_id
            );
        }
        if let Some(egress) = crate::egress::standards_egress() {
            if let Ok(attestation_json) = serde_json::to_vec(&attestation) {
                if let Err(e) = egress.write_dsse(&attestation_json, &envelope) {
                    tracing::warn!(error = %e, "failed to write DSSE attestation.jsonl");
                }
            }
            if let Some(ref cost) = attestation.cost {
                let (start, end) = crate::egress::focus_period_now();
                if let Err(e) = egress.write_focus(run_id, cost, &start, &end) {
                    tracing::warn!(error = %e, "failed to write focus.jsonl");
                }
            }
        }
    }

    // Emit OpenLineage Run Event (Phase 6.3)
    let input_manifest = vam_telemetry::build_drive9_dataset(
        &format!("workspace-{}-initial", workspace_id),
        &head_before,
        None,
        None,
    );
    let output_manifest = vam_telemetry::build_drive9_dataset(
        &format!("workspace-{}-final", workspace_id),
        &head_after,
        None,
        None,
    );
    let lineage_event = vam_telemetry::OpenLineageRunEvent::new(
        if outcome.succeeded {
            "COMPLETE"
        } else {
            "FAIL"
        },
        prost_types::Timestamp {
            seconds: now_ms / 1000,
            nanos: ((now_ms % 1000) * 1_000_000) as i32,
        }
        .to_string(),
        run_id,
        "vam.orchestrator",
        format!("run:{}", run_id),
        vec![input_manifest],
        vec![output_manifest],
    );
    if let Ok(lineage_json) = serde_json::to_string(&lineage_event) {
        let _ = store.append_outbox_event(run_id, "vam.v1.lineage.emitted", &lineage_json);
        if let Some(egress) = crate::egress::standards_egress() {
            egress.emit_lineage(&lineage_json);
        }
        tracing::info!("[orchestrator] lineage event emitted for run {}", run_id);
    }

    // Record Cold snapshot from committed HEAD (no live-disk extract).
    if kept.is_none() && !head_after.is_empty() {
        let snap_span = tracing::info_span!("record_snapshot", workspace_id = %workspace_id);
        let _snap_guard = snap_span.enter();
        let snap_id = format!("snap-{}", uuid_like());
        let guest_cid = session_record.as_ref().and_then(|s| s.guest_cid);
        let tap_name = session_record.as_ref().and_then(|s| s.tap_device.clone());
        match crate::manager::record_workspace_snapshot(
            store,
            session_id,
            workspace_id,
            &snap_id,
            &head_after,
            guest_cid,
            tap_name,
        ) {
            Ok(_) => {
                tracing::info!(
                    "[orchestrator] snapshot {} recorded for workspace '{}' (manifest {})",
                    snap_id,
                    workspace_id,
                    head_after
                );
            }
            Err(e) => {
                tracing::error!("[orchestrator] post-run snapshot failed: {}", e);
            }
        }
    }

    // The last step: the run's heads, the trajectory and the attestation are all
    // in place, so the one receipt is complete. (GET never issues.)
    crate::receipt::issue_receipt_logged(
        store,
        run_id,
        Some(trajectory_replay_digest.as_str()).filter(|d| !d.is_empty()),
    );
}

/// How a task on a thread starts.
enum ThreadStart {
    /// No history.
    Fresh,
    /// Timeline turns but no log: the flattened replay, once (owner decision 3).
    Legacy { prompt: String, runs: Vec<String> },
    /// The host-assembled context, fetched by the guest by digest.
    Log(Box<crate::thread_context::BuiltContext>),
}

#[allow(clippy::too_many_arguments)]
fn prepare_thread_context(
    materialized: &std::path::Path,
    workspace_id: &str,
    thread_ref: &str,
    run_id: &str,
    agent_urn: &str,
    agent_digest: &str,
    spec_json: &str,
    user_prompt: &str,
) -> Result<ThreadStart, OrchestratorError> {
    let agent_version = vam_spec::encoding::json_to_proto("vam.v1.RunSpec", spec_json)
        .ok()
        .and_then(|parsed: vam_spec::v1::RunSpec| {
            parsed
                .agent_ref
                .and_then(|r| r.version)
                .map(|version| vam_storage::WorkspaceAgentVersion::from_spec(&version))
        });
    vam_storage::ensure_workspace_json(
        materialized,
        workspace_id,
        Some(vam_storage::WorkspaceAgentBind {
            agent_urn,
            agent_digest,
            agent_version: agent_version.as_ref(),
        }),
    )
    .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    vam_storage::ensure_thread_json(materialized, thread_ref, agent_urn, agent_digest)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    let sources: Vec<Box<dyn crate::thread_context::ContextSource>> = vec![Box::new(
        crate::thread_context::ThreadLogSource::new(materialized, thread_ref),
    )];
    let built = crate::thread_context::build_context(run_id, &sources)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    if !built.context.messages.is_empty() {
        return Ok(ThreadStart::Log(Box::new(built)));
    }
    let timeline = vam_storage::read_timeline(materialized, thread_ref)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    if timeline.is_empty() {
        return Ok(ThreadStart::Fresh);
    }
    let start = timeline
        .len()
        .saturating_sub(vam_storage::DEFAULT_REPLAY_TURNS);
    let mut runs: Vec<String> = Vec::new();
    for turn in &timeline[start..] {
        if !runs.contains(&turn.run_id) {
            runs.push(turn.run_id.clone());
        }
    }
    Ok(ThreadStart::Legacy {
        prompt: vam_storage::replay_prompt(
            &timeline,
            user_prompt,
            vam_storage::DEFAULT_REPLAY_TURNS,
        ),
        runs,
    })
}

/// Input keys only the host may set; a caller's copy is removed.
const RESERVED_CONTEXT_INPUTS: [&str; 2] = [
    vam_spec::thread_context::THREAD_HEAD_DIGEST_INPUT,
    vam_spec::thread_context::THREAD_HEAD_SEQ_INPUT,
];

/// Writes the assembled context beside the session and grants its digest to
/// the run.
fn stage_context(
    store: &SqliteWorkflowStore,
    layout: &vam_storage::Drive9Layout,
    run_id: &str,
    session_id: &str,
    built: &crate::thread_context::BuiltContext,
) -> Result<(), String> {
    crate::thread_context::write_context(layout, session_id, built)
        .map_err(|e| e.to_string())
        .and_then(|_| {
            store
                .grant_run_context(run_id, &built.digest)
                .map_err(|e| e.to_string())
        })
}

fn set_context_inputs(
    inputs: &mut Option<prost_types::Struct>,
    built: &crate::thread_context::BuiltContext,
) {
    let fields = &mut inputs.get_or_insert_with(Default::default).fields;
    let string = |s: String| prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(s)),
    };
    fields.insert(
        vam_spec::thread_context::THREAD_HEAD_DIGEST_INPUT.to_string(),
        string(built.digest.clone()),
    );
    fields.insert(
        vam_spec::thread_context::THREAD_HEAD_SEQ_INPUT.to_string(),
        string(built.context.head_seq.to_string()),
    );
}

/// An approval or denial resumes from the thread log when the run was parked
/// after the switch: the log's latest `suspend` of this run is newer than any
/// legacy `run_checkpoints` row. A run parked before keeps its legacy digest.
fn attach_log_resume_context(
    store: &SqliteWorkflowStore,
    layout: &vam_storage::Drive9Layout,
    session_id: &str,
    run_id: &str,
    thread_ref: &str,
    inputs: &mut prost_types::Struct,
) {
    let flag = |key: &str| {
        matches!(
            inputs.fields.get(key).and_then(|v| v.kind.as_ref()),
            Some(prost_types::value::Kind::BoolValue(true))
        )
    };
    if !(flag("hitl_denied") || inputs.fields.contains_key("approved_tool")) {
        return;
    }
    let legacy_generation = match inputs
        .fields
        .get("hitl_continuation_generation")
        .and_then(|v| v.kind.as_ref())
    {
        Some(prost_types::value::Kind::StringValue(g)) => g.parse::<i64>().ok(),
        _ => None,
    };
    let materialized = layout.session_dir(session_id).join("materialized");
    let suspend = match crate::thread_context::latest_suspend(&materialized, thread_ref, run_id) {
        Ok(Some(suspend)) => suspend,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("resume context for {run_id} unavailable: {e}");
            return;
        }
    };
    if legacy_generation.is_some_and(|g| g >= suspend.lifecycle_generation) {
        return;
    }
    let sources: Vec<Box<dyn crate::thread_context::ContextSource>> = vec![Box::new(
        crate::thread_context::ThreadLogSource::new(&materialized, thread_ref),
    )];
    let built = match crate::thread_context::build_context_with(run_id, &sources, Some(suspend)) {
        Ok(built) => built,
        Err(e) => {
            tracing::error!("resume context for {run_id} could not be built: {e}");
            return;
        }
    };
    if let Err(e) = stage_context(store, layout, run_id, session_id, &built) {
        tracing::error!("resume context for {run_id} could not be staged: {e}");
        return;
    }
    inputs.fields.remove("hitl_continuation_digest");
    inputs.fields.remove("hitl_continuation_generation");
    let mut holder = Some(std::mem::take(inputs));
    set_context_inputs(&mut holder, &built);
    *inputs = holder.unwrap_or_default();
}

/// Turns a [`ThreadStart`] into the prompt the guest gets and the context
/// inputs: writes the assembled context beside the session, grants its digest
/// to this run, and inherits the context URLs of the runs it contains.
fn apply_thread_start(
    store: &SqliteWorkflowStore,
    layout: &vam_storage::Drive9Layout,
    run_id: &str,
    session_id: &str,
    user_prompt: &str,
    start: ThreadStart,
    inputs: &mut Option<prost_types::Struct>,
) -> String {
    if let Some(inputs) = inputs.as_mut() {
        for key in RESERVED_CONTEXT_INPUTS {
            inputs.fields.remove(key);
        }
    }
    match start {
        ThreadStart::Fresh => user_prompt.to_string(),
        ThreadStart::Legacy { prompt, runs } => {
            if let Err(e) = store.inherit_context_urls(run_id, &runs) {
                tracing::warn!("context URL inheritance failed for {run_id}: {e}");
            }
            prompt
        }
        ThreadStart::Log(built) => {
            let written = stage_context(store, layout, run_id, session_id, &built);
            if let Err(e) = written {
                // The guest cannot fetch it. The run proceeds with the plain
                // prompt and the failure is visible on the run's events.
                tracing::error!("thread context for {run_id} could not be staged: {e}");
                let _ = store.append_outbox_event(
                    run_id,
                    "vam.v1.thread.context_unavailable",
                    &serde_json::json!({ "error": e }).to_string(),
                );
                return user_prompt.to_string();
            }
            for warning in &built.warnings {
                let _ = store.append_outbox_event(
                    run_id,
                    "vam.v1.thread.log_warning",
                    &serde_json::json!({ "warning": warning }).to_string(),
                );
            }
            if let Err(e) = store.inherit_context_urls(run_id, &built.runs) {
                tracing::warn!("context URL inheritance failed for {run_id}: {e}");
            }
            set_context_inputs(inputs, &built);
            user_prompt.to_string()
        }
    }
}

pub(crate) fn find_active_session_on_workspace(
    store: &SqliteWorkflowStore,
    workspace_id: &str,
) -> Result<Option<SessionRecord>, OrchestratorError> {
    let sessions = store.load_all_sessions()?;
    Ok(sessions.into_iter().find(|session| {
        session.workspace_id == workspace_id
            && !matches!(
                session.observed_state,
                ObservedState::Terminated | ObservedState::Failed
            )
    }))
}

fn read_bound_workspace_meta(
    workspace_id: &str,
) -> Result<Option<vam_storage::WorkspaceMeta>, OrchestratorError> {
    let layout = vam_storage::Drive9Layout::new(drive9_root());
    let chunk_store = vam_storage::ChunkStore::new(layout.clone());
    let manifest_manager = vam_storage::ManifestManager::new(layout, chunk_store.clone());
    vam_storage::read_workspace_meta_from_head(&manifest_manager, &chunk_store, workspace_id)
        .map_err(|e| OrchestratorError::Core(e.to_string()))
}

/// Records the workspace HEAD a run starts from (empty when the workspace has
/// no commit yet) with the hash of its files, once per run. Returns both.
fn record_run_heads_before(
    store: &SqliteWorkflowStore,
    run_id: &str,
    session_id: &str,
    workspace_id: &str,
) -> Option<(String, String)> {
    let layout = vam_storage::Drive9Layout::new(drive9_root());
    let chunk_store = vam_storage::ChunkStore::new(layout.clone());
    let manifests = vam_storage::ManifestManager::new(layout, chunk_store);
    let head = manifests
        .read_head(workspace_id)
        .ok()
        .flatten()
        .unwrap_or_default();
    let files_hash = if head.is_empty() {
        vam_storage::MerkleManifest::empty_files_hash()
    } else {
        manifests
            .get_manifest(workspace_id, &head)
            .map(|m| m.files_hash())
            .ok()?
    };
    store
        .record_run_head_before(run_id, session_id, &head, &files_hash)
        .ok()?;
    Some((head, files_hash))
}

/// Records the isolation this run's session started under, with the image
/// digests when it runs in a microVM. Evidence only: a failure is logged.
async fn record_run_isolation_fact(store: &SqliteWorkflowStore, run_id: &str, session_id: &str) {
    let inputs = crate::isolation::current();
    let images = if inputs.guest_mode == "vm" {
        match tokio::task::spawn_blocking(crate::golden::image_digests).await {
            Ok(Ok(pair)) => Some(pair),
            Ok(Err(err)) => {
                tracing::warn!("image digests unavailable for {run_id}: {err}");
                None
            }
            Err(err) => {
                tracing::warn!("image digest task failed for {run_id}: {err}");
                None
            }
        }
    } else {
        None
    };
    let generation = store
        .load_session(session_id)
        .map(|s| s.lifecycle_generation)
        .unwrap_or(0);
    if let Err(err) = store.record_run_isolation(
        run_id,
        session_id,
        &crate::isolation::describe(&inputs),
        inputs.guest_mode,
        images,
        generation,
    ) {
        tracing::warn!("isolation write failed for {run_id}: {err}");
    }
}

fn read_workspace_head(workspace_id: &str) -> Result<String, OrchestratorError> {
    let layout = vam_storage::Drive9Layout::new(drive9_root());
    let chunk_store = vam_storage::ChunkStore::new(layout.clone());
    let manifest_manager = vam_storage::ManifestManager::new(layout, chunk_store);
    manifest_manager
        .read_head(workspace_id)
        .map_err(|e| OrchestratorError::Core(e.to_string()))?
        .ok_or_else(|| OrchestratorError::Core("workspace HEAD missing after run".into()))
}

struct DispatchOutcome {
    answer: String,
    succeeded: bool,
    paused: bool,
    /// The guest died and the task waits to be picked up again (E10 Phase 4). Nothing ends.
    interrupted: bool,
    spend_micro: Option<i64>,
}

fn build_run_lease(run_id: &str, generation: i64) -> ExecutionLease {
    use std::sync::atomic::{AtomicU64, Ordering};
    static LEASE_SEQ: AtomicU64 = AtomicU64::new(1);
    // The counter restarts at 1 with the process, but a resumed run keeps its
    // earlier attempts in the store: make the ids unique across restarts.
    let seq = format!(
        "{}-{}",
        uuid_like(),
        LEASE_SEQ.fetch_add(1, Ordering::SeqCst)
    );
    ExecutionLease {
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        step_id: "live-step".to_string(),
        attempt_id: Some(ResourceId {
            urn: format!("urn:vam:attempt:local.vam.dev/acme/attempt-{seq}"),
        }),
        lease_id: Some(ResourceId {
            urn: format!("urn:vam:lease:local.vam.dev/acme/lease-{seq}"),
        }),
        lifecycle_generation: generation,
        fencing_token: vec![1],
        expires_at: Some(Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: format!("run-lease-{seq}"),
    }
}

fn approved_tool_args_json(stored: &crate::error::StoredApproval) -> &str {
    stored
        .amended_args_json
        .as_deref()
        .unwrap_or(stored.requested_args_json.as_str())
}

fn run_spec_inputs_from_store(
    store: &SqliteWorkflowStore,
    run_id: &str,
) -> Option<prost_types::Struct> {
    let (spec_json, _, _) = store.get_run_spec_context(run_id).ok()?;
    let spec_json = spec_json?;
    let val: serde_json::Value = serde_json::from_str(&spec_json).ok()?;
    val.get("inputs").and_then(struct_from_json_value)
}

/// Default guest ExecuteTask wait when RunSpec.max_duration is unset or non-positive.
///
/// New runs store 1800s at create time. This default remains for rows persisted
/// before that resolution existed.
const DEFAULT_GUEST_TASK_TIMEOUT_SECS: u64 = 1800;

fn unix_now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs() as i64)
        .unwrap_or(0)
}

/// True when a positive `max_duration` has been consumed by active time.
fn active_budget_exhausted(store: &SqliteWorkflowStore, run_id: &str) -> bool {
    matches!(
        store.remaining_active_budget_secs(run_id, unix_now_secs()),
        Ok(Some(0))
    )
}

/// Host wait for one active guest-task segment.
///
/// When `RunSpec.max_duration` is positive, the wait is the budget still left
/// after active segments. Parked HITL time is not in that total. Unset or
/// non-positive budgets keep the 30 minute default.
fn guest_task_timeout_secs(store: &SqliteWorkflowStore, run_id: &str) -> u64 {
    match store.remaining_active_budget_secs(run_id, unix_now_secs()) {
        Ok(Some(remaining)) => remaining,
        _ => DEFAULT_GUEST_TASK_TIMEOUT_SECS,
    }
}

/// Re-inject catalog AgentSpec fields that were merged at dispatch time but not persisted.
fn remerge_catalog_agent_into_inputs(
    store: &SqliteWorkflowStore,
    run_id: &str,
    inputs: prost_types::Struct,
) -> prost_types::Struct {
    let Some(agent_urn) = store
        .get_run_spec_context(run_id)
        .ok()
        .and_then(|(_, _, urn)| urn)
        .filter(|urn| !urn.is_empty())
    else {
        return inputs;
    };
    let agent = store
        .run_owner_user_id(run_id)
        .ok()
        .and_then(|owner| store.get_agent_visible(&agent_urn, &owner, false).ok())
        .or_else(|| store.get_agent(&agent_urn).ok());
    let Some(agent) = agent else {
        return inputs;
    };
    crate::run_context::merge_agent_into_inputs(Some(inputs), &agent).unwrap_or_default()
}

fn build_hitl_rejection_resume_inputs(
    store: &SqliteWorkflowStore,
    run_id: &str,
    tool_name: &str,
    args_json: &str,
    rejection_reason: &str,
) -> Result<prost_types::Struct, OrchestratorError> {
    let args_value: serde_json::Value = serde_json::from_str(args_json).map_err(|e| {
        OrchestratorError::InvalidContract(format!("requested tool args invalid: {e}"))
    })?;
    let tool_args = struct_from_json_value(&args_value).ok_or_else(|| {
        OrchestratorError::InvalidContract("requested tool args must be a JSON object".into())
    })?;

    let mut inputs = run_spec_inputs_from_store(store, run_id).unwrap_or_default();
    inputs = remerge_catalog_agent_into_inputs(store, run_id, inputs);
    inputs.fields.insert(
        "hitl_denied".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(true)),
        },
    );
    inputs.fields.insert(
        "denied_tool".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(tool_name.to_string())),
        },
    );
    inputs.fields.insert(
        "denied_tool_args".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(tool_args)),
        },
    );
    inputs.fields.insert(
        "denial_reason".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                rejection_reason.to_string(),
            )),
        },
    );
    inputs.fields.insert(
        "hitl_denied_note".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "The user denied your tool request. Briefly explain that you could not \
                 proceed with that action and suggest alternatives if appropriate. \
                 Do not invoke the same tool again."
                    .to_string(),
            )),
        },
    );
    Ok(inputs)
}

fn insert_hitl_checkpoint_ref(
    inputs: &mut prost_types::Struct,
    checkpoint: crate::run_checkpoint::ResumeCheckpoint,
) {
    inputs.fields.insert(
        "hitl_continuation_digest".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(checkpoint.digest)),
        },
    );
    inputs.fields.insert(
        "hitl_continuation_generation".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                checkpoint.lifecycle_generation.to_string(),
            )),
        },
    );
}

fn build_gate_resume_inputs(
    store: &SqliteWorkflowStore,
    run_id: &str,
    step_id: &str,
    outcome: &str,
    response_json: &str,
) -> Result<prost_types::Struct, OrchestratorError> {
    let response_value: serde_json::Value = serde_json::from_str(response_json)
        .unwrap_or_else(|_| serde_json::json!({ "outcome": outcome }));
    let response = struct_from_json_value(&response_value).unwrap_or_default();
    let mut inputs = run_spec_inputs_from_store(store, run_id).unwrap_or_default();
    inputs = remerge_catalog_agent_into_inputs(store, run_id, inputs);
    inputs.fields.insert(
        "hitl_resume".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(true)),
        },
    );
    inputs.fields.insert(
        "gate_step_id".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(step_id.to_string())),
        },
    );
    inputs.fields.insert(
        "gate_outcome".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(outcome.to_string())),
        },
    );
    inputs.fields.insert(
        "gate_response".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(response)),
        },
    );
    let status = crate::playbook_flight::status_block(store, run_id);
    if let Some(status) = status.as_ref() {
        inputs.fields.insert(
            crate::run_context::PLAYBOOK_STATUS_FIELD.to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(status.clone())),
            },
        );
    }
    inputs.fields.insert(
        "hitl_resume_note".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(format!(
                "Gate '{step_id}' was decided with outcome '{outcome}'. Continue the available steps. Do not call step_complete on that gate."
            ))),
        },
    );
    if let Some(checkpoint) = crate::run_checkpoint::resume_checkpoint(store, run_id)? {
        insert_hitl_checkpoint_ref(&mut inputs, checkpoint);
    }
    Ok(inputs)
}

fn build_hitl_resume_inputs(
    store: &SqliteWorkflowStore,
    run_id: &str,
    tool_name: &str,
    args_json: &str,
) -> Result<prost_types::Struct, OrchestratorError> {
    let args_value: serde_json::Value = serde_json::from_str(args_json).map_err(|e| {
        OrchestratorError::InvalidContract(format!("approved tool args invalid: {e}"))
    })?;
    let tool_args = struct_from_json_value(&args_value).ok_or_else(|| {
        OrchestratorError::InvalidContract("approved tool args must be a JSON object".into())
    })?;

    let mut inputs = run_spec_inputs_from_store(store, run_id).unwrap_or_default();
    inputs = remerge_catalog_agent_into_inputs(store, run_id, inputs);
    inputs.fields.insert(
        "hitl_resume".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(true)),
        },
    );
    inputs.fields.insert(
        "approved_tool".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(tool_name.to_string())),
        },
    );
    inputs.fields.insert(
        "approved_tool_args".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(tool_args)),
        },
    );
    inputs.fields.insert(
        "hitl_resume_note".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                "Human approval was granted and the approved tool has already executed. \
                 Continue the original task from this point. Use other tools as needed to \
                 finish remaining steps. Do not re-invoke the same approved tool unless the \
                 remaining work requires it."
                    .to_string(),
            )),
        },
    );
    if let Some(checkpoint) = crate::run_checkpoint::resume_checkpoint(store, run_id)? {
        insert_hitl_checkpoint_ref(&mut inputs, checkpoint);
    }
    Ok(inputs)
}

fn original_prompt_from_run(
    store: &SqliteWorkflowStore,
    run_id: &str,
) -> Result<String, OrchestratorError> {
    let (spec_json, _, _) = store.get_run_spec_context(run_id)?;
    let Some(spec_json) = spec_json else {
        return Ok(crate::run_context::extract_prompt_from_inputs(
            &vam_spec::v1::RunSpec {
                inputs: None,
                ..Default::default()
            },
        ));
    };
    let spec =
        vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &spec_json)
            .map_err(OrchestratorError::Core)?;
    Ok(crate::run_context::extract_prompt_from_inputs(&spec))
}

fn thread_ref_and_model_from_run(
    store: &SqliteWorkflowStore,
    run_id: &str,
) -> Result<(String, String), OrchestratorError> {
    let (spec_json, _, _) = store.get_run_spec_context(run_id)?;
    let Some(spec_json) = spec_json else {
        return Ok((
            crate::run_context::DEFAULT_THREAD_REF.to_string(),
            String::new(),
        ));
    };
    let spec =
        vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &spec_json)
            .map_err(OrchestratorError::Core)?;
    let thread_ref = crate::run_context::extract_thread_ref(&spec)
        .unwrap_or_else(|_| crate::run_context::DEFAULT_THREAD_REF.to_string());
    let model = crate::run_context::extract_model(&spec);
    Ok((thread_ref, model))
}

async fn resume_run_after_approval(
    manager: SessionManager,
    store: Arc<SqliteWorkflowStore>,
    record: crate::error::ApprovalRecord,
) -> Result<(), OrchestratorError> {
    let stored = store.get_approval(&record.request_id)?;
    let resume_inputs = resume_inputs_for(store.as_ref(), &stored, &record)?;
    resume_run_after_hitl_decision(manager, store, record, resume_inputs).await
}

async fn resume_run_after_rejection(
    manager: SessionManager,
    store: Arc<SqliteWorkflowStore>,
    record: crate::error::ApprovalRecord,
) -> Result<(), OrchestratorError> {
    let stored = store.get_approval(&record.request_id)?;
    let resume_inputs = resume_inputs_for(store.as_ref(), &stored, &record)?;
    resume_run_after_hitl_decision(manager, store, record, resume_inputs).await
}

/// The inputs the resumed task starts from for one decided approval. The decision's kind picks
/// the shape: an approved tool call, a denied one, a gate, or an answered question.
pub(crate) fn resume_inputs_for(
    store: &SqliteWorkflowStore,
    stored: &crate::error::StoredApproval,
    record: &crate::error::ApprovalRecord,
) -> Result<prost_types::Struct, OrchestratorError> {
    match hitl_resume_kind(stored.decision.as_deref(), &stored.kind) {
        HitlResumeKind::Approved => build_hitl_resume_inputs(
            store,
            &record.run_id,
            &record.tool_name,
            approved_tool_args_json(stored),
        ),
        HitlResumeKind::Rejected => {
            let rejection_reason = stored
                .rejection_reason
                .as_deref()
                .filter(|s| !s.is_empty())
                .unwrap_or("Denied by user");
            build_hitl_rejection_resume_inputs(
                store,
                &record.run_id,
                &record.tool_name,
                &stored.requested_args_json,
                rejection_reason,
            )
        }
        HitlResumeKind::Gate => {
            let outcome = store
                .list_playbook_steps(&record.run_id)
                .ok()
                .and_then(|steps| {
                    steps
                        .into_iter()
                        .find(|step| step.step_id == record.step_id)
                        .map(|step| step.outcome)
                })
                .filter(|outcome| !outcome.is_empty())
                .unwrap_or_else(|| "submitted".to_string());
            let response_json = stored
                .amended_args_json
                .clone()
                .unwrap_or_else(|| "{}".to_string());
            build_gate_resume_inputs(
                store,
                &record.run_id,
                &record.step_id,
                &outcome,
                &response_json,
            )
        }
        HitlResumeKind::Answered => question::answer_resume_inputs(store, stored, record),
        HitlResumeKind::None => Err(OrchestratorError::InvalidContract(
            "approval has no decision to resume".into(),
        )),
    }
}

/// Resume a decided HITL wait. A still-warm VM is resumed in place; a parked
/// VM is replaced by a new session on the reserved workspace.
///
/// One resume per parked run: the caller that moves the run `PAUSED -> RUNNING` owns it. A
/// second caller, for the same or another decision, finds the run already resumed and returns
/// `Ok(())` with nothing started; its decision is recorded and reaches the task through the
/// resume that owns the run (`resume_decisions`).
pub async fn resume_after_hitl_decision(
    manager: SessionManager,
    store: Arc<SqliteWorkflowStore>,
    record: crate::error::ApprovalRecord,
) -> Result<(), OrchestratorError> {
    if !store.claim_parked_run(&record.run_id)? {
        eprintln!(
            "[orchestrator] resume for decision {} on run {} skipped: the run is not parked (a resume of it is in flight)",
            record.request_id, record.run_id
        );
        return Ok(());
    }
    resume_claimed_decision(manager, store, record).await
}

/// The resume of a decision whose caller already claimed the run (`claim_parked_run`).
async fn resume_claimed_decision(
    manager: SessionManager,
    store: Arc<SqliteWorkflowStore>,
    record: crate::error::ApprovalRecord,
) -> Result<(), OrchestratorError> {
    let stored = store.get_approval(&record.request_id)?;
    match hitl_resume_kind(stored.decision.as_deref(), &stored.kind) {
        HitlResumeKind::Approved => resume_run_after_approval(manager, store, record).await,
        HitlResumeKind::Rejected => resume_run_after_rejection(manager, store, record).await,
        HitlResumeKind::Gate => resume_run_after_gate(manager, store, record).await,
        HitlResumeKind::Answered => question::resume_run_after_answer(manager, store, record).await,
        HitlResumeKind::None => Err(OrchestratorError::InvalidContract(
            "approval has no decision to resume".into(),
        )),
    }
}

/// Which resume note a decided approval should send back to the guest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HitlResumeKind {
    Approved,
    Rejected,
    Gate,
    /// A question was answered (AR-3.10).
    Answered,
    None,
}

fn hitl_resume_kind(decision: Option<&str>, approval_kind: &str) -> HitlResumeKind {
    if approval_kind == crate::hitl::APPROVAL_KIND_GATE
        && matches!(decision, Some("SUBMITTED" | "REJECTED"))
    {
        return HitlResumeKind::Gate;
    }
    if approval_kind == crate::hitl::APPROVAL_KIND_QUESTION && decision == Some("SUBMITTED") {
        return HitlResumeKind::Answered;
    }
    match decision {
        Some("APPROVED" | "AMENDED") => HitlResumeKind::Approved,
        // A reconnect re-runs the parked call through its normal decision, with no grant.
        Some(crate::hitl::DECISION_RECONNECTED) => HitlResumeKind::Approved,
        Some("REJECTED") => HitlResumeKind::Rejected,
        _ => HitlResumeKind::None,
    }
}

async fn resume_run_after_gate(
    manager: SessionManager,
    store: Arc<SqliteWorkflowStore>,
    record: crate::error::ApprovalRecord,
) -> Result<(), OrchestratorError> {
    let stored = store.get_approval(&record.request_id)?;
    let resume_inputs = resume_inputs_for(store.as_ref(), &stored, &record)?;
    resume_run_after_hitl_decision(manager, store, record, resume_inputs).await
}

/// Apply due gate `on_timeout` outcomes and resume parked runs through the
/// same warm-or-fresh path as a human decision.
pub async fn sweep_gate_timeouts(
    manager: &SessionManager,
    store: &Arc<SqliteWorkflowStore>,
) -> Result<usize, OrchestratorError> {
    let resumes = store.apply_gate_timeouts()?;
    let count = resumes.len();
    for record in resumes {
        if let Err(err) =
            resume_after_hitl_decision(manager.clone(), store.clone(), record.clone()).await
        {
            tracing::error!(
                "[orchestrator] gate timeout resume failed for run '{}': {err}",
                record.run_id
            );
        }
    }
    Ok(count)
}

/// Lease generation for a new session: higher than the parked session and at
/// least the continuation generation recorded on the decision.
fn resume_lease_generation(prior_generation: i64, continuation_generation: i64) -> i64 {
    prior_generation
        .saturating_add(1)
        .max(continuation_generation)
        .max(1)
}

fn ensure_cold_resume_workspace(
    store: &SqliteWorkflowStore,
    workspace_id: &str,
    run_id: &str,
) -> Result<(), OrchestratorError> {
    if let Some(reservation) = store.workspace_reservation(workspace_id)? {
        if reservation.run_id != run_id {
            let cancel_command = crate::error::cancel_run_command(&reservation.run_id);
            return Err(OrchestratorError::WorkspaceReserved {
                workspace_id: workspace_id.to_string(),
                run_id: reservation.run_id,
                cancel_command,
            });
        }
    }
    Ok(())
}

/// Why a cold-resume boot did not happen.
#[derive(Debug)]
enum ResumeBootError {
    /// Another resume of this run already holds the resume key and owns the
    /// run. Nothing was changed: the caller must leave the run alone.
    Superseded(OrchestratorError),
    /// The resume failed; the run fails with it.
    Failed(OrchestratorError),
}

impl From<OrchestratorError> for ResumeBootError {
    fn from(err: OrchestratorError) -> Self {
        Self::Failed(err)
    }
}

async fn boot_resume_session(
    manager: &SessionManager,
    store: &SqliteWorkflowStore,
    prior: &SessionRecord,
    run_id: &str,
    session_id: &str,
    generation: i64,
) -> Result<(), ResumeBootError> {
    let vm_id = format!("vm-{session_id}");
    let idempotency_key = format!("resume-{}-{generation}", prior.session_id);

    // Tie the session to the run BEFORE its brokers start, the same order a
    // fresh run uses (`create_run`): the brokers read the run's agent, spec,
    // budget and owner by session id. A run that records no agent cannot be
    // resumed: refuse before anything boots.
    let (_, _, agent_urn) = store.get_run_spec_context(run_id)?;
    if agent_urn.as_deref().is_none_or(str::is_empty) {
        return Err(ResumeBootError::Failed(OrchestratorError::CorruptState(
            format!(
                "run '{run_id}' records no agent; refusing to resume it (a resumed run never runs without its agent's policy)"
            ),
        )));
    }
    // The run's owner is the owner of its sessions; the parked session's owner
    // is only a fallback.
    let owner = store
        .run_owner_user_id(run_id)
        .or_else(|_| store.session_owner_user_id(&prior.session_id))
        .unwrap_or_else(|_| crate::users::LOCAL_USER_ID.to_string());
    // Claim the resume key first. A different session already holding it means
    // another resume of this run is in flight: return before the run is
    // touched, so that resume keeps the run (and its lookups by session id).
    if let Some(existing) = store.create_session_with_owner(
        session_id,
        Some(&idempotency_key),
        &vm_id,
        &prior.workspace_id,
        &prior.storage_profile_id,
        &owner,
    )? {
        if existing.session_id != session_id {
            return Err(ResumeBootError::Superseded(OrchestratorError::Conflict(
                format!(
                    "resume of run '{run_id}' already started session '{}'",
                    existing.session_id
                ),
            )));
        }
    }
    // Still before the manager boots the session and starts its brokers.
    store.set_run_session_id(run_id, session_id)?;

    // The manager adopts the row inserted above.
    manager
        .create_session(
            session_id.to_string(),
            Some(idempotency_key),
            vm_id,
            prior.workspace_id.clone(),
            prior.storage_profile_id.clone(),
            None,
        )
        .await?;
    store.assign_lifecycle_generation(session_id, generation)?;
    store.copy_session_hitl_grants(&prior.session_id, session_id)?;
    Ok(())
}

/// Mark the run failed and drop the park so a boot or dispatch error cannot
/// leave the workspace reserved with the run stuck `PAUSED`.
async fn fail_parked_resume(
    manager: &SessionManager,
    store: &SqliteWorkflowStore,
    run_id: &str,
    session_id: Option<&str>,
    message: &str,
) {
    let _ = store.release_run_reservation(run_id);
    let _ = store.clear_keep_warm(run_id);
    let result_json = serde_json::json!({ "error": message }).to_string();
    let _ = store.complete_run_with_result(run_id, "FAILED", &result_json);
    if let Some(session_id) = session_id {
        let _ = manager.terminate_session(session_id.to_string()).await;
    }
}

async fn resume_run_after_hitl_decision(
    manager: SessionManager,
    store: Arc<SqliteWorkflowStore>,
    record: crate::error::ApprovalRecord,
    resume_inputs: prost_types::Struct,
) -> Result<(), OrchestratorError> {
    let prompt = original_prompt_from_run(store.as_ref(), &record.run_id)?;
    let (thread_ref, model) = thread_ref_and_model_from_run(store.as_ref(), &record.run_id)?;
    if store.get_run_state(&record.run_id).ok().as_deref() == Some("PAUSED") {
        let _ = store.update_run_state(&record.run_id, "RUNNING");
    }

    let prior = store.load_session(&record.session_id)?;
    if active_budget_exhausted(store.as_ref(), &record.run_id) {
        let warm = crate::manager::session_is_warm(&prior).then_some(prior.session_id.as_str());
        fail_parked_resume(
            &manager,
            store.as_ref(),
            &record.run_id,
            warm,
            "run exceeded max_duration of active time",
        )
        .await;
        return Err(OrchestratorError::Core(
            "run exceeded max_duration of active time".into(),
        ));
    }
    let workspace_id = prior.workspace_id.clone();
    let (session_id, generation) = if crate::manager::session_is_warm(&prior) {
        // The same lease generation a cold resume gets: above the one the parked task ran
        // under, so every resume of a session runs under a lease of its own.
        let generation =
            resume_lease_generation(prior.lifecycle_generation, record.continuation_generation);
        store.assign_lifecycle_generation(&prior.session_id, generation)?;
        (prior.session_id.clone(), generation)
    } else {
        if let Err(e) = ensure_cold_resume_workspace(store.as_ref(), &workspace_id, &record.run_id)
        {
            fail_parked_resume(
                &manager,
                store.as_ref(),
                &record.run_id,
                None,
                &e.to_string(),
            )
            .await;
            return Err(e);
        }
        let generation =
            resume_lease_generation(prior.lifecycle_generation, record.continuation_generation);
        let session_id = format!("sess-{}", uuid_like());
        match boot_resume_session(
            &manager,
            store.as_ref(),
            &prior,
            &record.run_id,
            &session_id,
            generation,
        )
        .await
        {
            Ok(()) => {
                let _ = store.release_run_reservation(&record.run_id);
                let _ = store.clear_keep_warm(&record.run_id);
                (session_id, generation)
            }
            // Another resume owns the run: leave it (and its session) alone. This decision
            // starts nothing, so there is nothing to fail.
            Err(ResumeBootError::Superseded(e)) => {
                eprintln!(
                    "[orchestrator] resume for decision {} on run {} skipped: {e}",
                    record.request_id, record.run_id
                );
                return Ok(());
            }
            Err(ResumeBootError::Failed(e)) => {
                fail_parked_resume(
                    &manager,
                    store.as_ref(),
                    &record.run_id,
                    Some(&session_id),
                    &e.to_string(),
                )
                .await;
                return Err(e);
            }
        }
    };

    // The run is claimed and its session is up. Read every decision recorded for it so far, so
    // the one resumed task is told all of them; whatever lands later is marked late when the
    // task ends or parks, and the next resume reports it.
    let _late_decisions = resume_decisions::LateDecisions::new(store.clone(), &record.run_id);
    let mut resume_inputs =
        resume_decisions::compose_resume_inputs(&store, &record, resume_inputs).await;

    // From here a stop of this session is settled by this function's finalizer.
    let _tracking =
        crate::session_end::RunTracking::new(store.clone(), &session_id, &record.run_id);

    let lease = build_run_lease(&record.run_id, generation);
    store.issue_lease(&lease)?;
    let session = store.load_session(&session_id)?;
    let vsock_path = vam_core::jail::vsock_uds_path(&session.vm_id);
    let timeout_secs = guest_task_timeout_secs(store.as_ref(), &record.run_id);
    let _ = store.begin_active_segment(&record.run_id, unix_now_secs());
    attach_log_resume_context(
        store.as_ref(),
        &vam_storage::Drive9Layout::new(drive9_root()),
        &session_id,
        &record.run_id,
        &thread_ref,
        &mut resume_inputs,
    );
    if let Some(recorder) = store.thread_recorder(&session_id) {
        recorder.record_task_start(&prompt, &model, "", true).await;
    }
    drive_resumed_guest(
        &manager,
        &store,
        &record.run_id,
        &session_id,
        &workspace_id,
        &thread_ref,
        &prompt,
        &model,
        &lease,
        &vsock_path,
        resume_inputs,
        timeout_secs,
    )
    .await
}

/// Runs a resumed session's guest to the end of the task (an approval resume or a crash
/// resume): the same ending for both. The caller holds the session's `RunTracking`. A guest that
/// dies with no stop recorded is a crash: the run waits to be picked up again instead of failing.
#[allow(clippy::too_many_arguments)]
async fn drive_resumed_guest(
    manager: &SessionManager,
    store: &Arc<SqliteWorkflowStore>,
    run_id: &str,
    session_id: &str,
    workspace_id: &str,
    thread_ref: &str,
    prompt: &str,
    model: &str,
    lease: &ExecutionLease,
    vsock_path: &std::path::Path,
    inputs: prost_types::Struct,
    timeout_secs: u64,
) -> Result<(), OrchestratorError> {
    let task_outcome = match crate::driver::run_task_on_guest_classified(
        vsock_path,
        lease,
        prompt,
        Some(inputs),
        timeout_secs,
        None,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(e) => {
            let _ = store.end_active_segment(run_id, unix_now_secs());
            if crate::resume::classify(store, run_id, session_id, &e)
                == crate::resume::Verdict::Crash
            {
                resume_run::interrupt_or_end(
                    manager,
                    store,
                    run_id,
                    session_id,
                    crate::resume_store::REASON_GUEST_CRASHED,
                )
                .await;
                return Ok(());
            }
            let result_json = if store.session_ends.stop_requested(session_id) {
                serde_json::json!({ "error": "the task was stopped", "reason": "stopped" })
            } else {
                serde_json::json!({ "error": e.message() })
            }
            .to_string();
            store.complete_run_with_result(run_id, "FAILED", &result_json)?;
            let _ = store.release_run_reservation(run_id);
            let _ = store.clear_keep_warm(run_id);
            let _ = dispatch_on_complete_for_terminal(store.as_ref(), run_id, session_id).await;
            // Guest failure after a resume: tear down so the workspace is not pinned.
            finalize_run_session(
                manager,
                store.as_ref(),
                run_id,
                session_id,
                workspace_id,
                thread_ref,
                prompt,
                model,
                &DispatchOutcome {
                    answer: String::new(),
                    succeeded: false,
                    paused: false,
                    interrupted: false,
                    spend_micro: None,
                },
            )
            .await;
            return Err(OrchestratorError::Core(e.to_string()));
        }
    };

    let outcome = apply_guest_task_outcome(store.as_ref(), run_id, lease, task_outcome)?;
    // Nested HITL: keep the session alive (do not finalize / terminate).
    if outcome.paused {
        if let Err(err) =
            crate::hsp_hooks::notify_run_paused(store.as_ref(), run_id, session_id).await
        {
            eprintln!(
                "[orchestrator] on_paused notification failed for run {}: {err}",
                run_id
            );
        }
        eprintln!(
            "[orchestrator] run {} paused again awaiting nested HITL approval (session kept alive)",
            run_id
        );
        return Ok(());
    }
    dispatch_on_complete_for_terminal(store.as_ref(), run_id, session_id).await?;
    finalize_run_session(
        manager,
        store.as_ref(),
        run_id,
        session_id,
        workspace_id,
        thread_ref,
        prompt,
        model,
        &outcome,
    )
    .await;
    Ok(())
}

fn answer_from_task_outcome(task_outcome: &vam_spec::v1::TaskOutcome) -> String {
    task_outcome
        .output
        .as_ref()
        .and_then(|s| s.fields.get("answer"))
        .and_then(|v| v.kind.as_ref())
        .and_then(|k| match k {
            prost_types::value::Kind::StringValue(s) => Some(s.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

/// `claimed` from a run under `TERMINATION_POLICY_EXPLICIT_CLAIM`: true when the
/// model called `task_complete`, false when the run ended without it. Absent for
/// other runs.
fn claimed_from_task_outcome(task_outcome: &vam_spec::v1::TaskOutcome) -> Option<bool> {
    task_outcome
        .output
        .as_ref()
        .and_then(|s| s.fields.get("claimed"))
        .and_then(|v| v.kind.as_ref())
        .and_then(|k| match k {
            prost_types::value::Kind::BoolValue(flag) => Some(*flag),
            _ => None,
        })
}

/// The stored result of a run that ended with an answer. `claimed` becomes
/// `RunStatus.final_outputs.claimed`.
pub fn answer_result_json(
    answer: &str,
    claimed: Option<bool>,
    citations: Option<&crate::web_sources::Citations>,
) -> String {
    let mut result = serde_json::json!({ "answer": answer });
    if let Some(flag) = claimed {
        result["claimed"] = serde_json::json!(flag);
    }
    if let Some(citations) = citations {
        crate::web_sources::apply_citations(&mut result, citations);
    }
    result.to_string()
}

/// The finish check for a web run (AR-1.8). Only for an agent with web on, and
/// only when the answer carries `[S<n>]` markers; otherwise no key is added.
/// Flag only: the answer is not changed and there is no re-prompt.
pub fn web_citations_for_answer(
    store: &SqliteWorkflowStore,
    run_id: &str,
    answer: &str,
) -> Option<crate::web_sources::Citations> {
    if !answer.contains("[S") {
        return None;
    }
    let session_id = store.get_run_session_id(run_id).ok().flatten()?;
    if !crate::web_admission::session_web_enabled(store, &session_id, None) {
        return None;
    }
    let run_sources = store.list_run_sources(run_id).unwrap_or_default();
    let citations = crate::web_sources::resolve_citations(answer, &run_sources)?;
    if !citations.unresolved.is_empty() {
        crate::trajectory::append_trajectory_event(
            store,
            run_id,
            crate::events::TYPE_WEB_CITATIONS_UNRESOLVED,
            &crate::events::web_citations_unresolved_payload(
                &citations.unresolved,
                citations.sources.len(),
                run_sources.len(),
            ),
        );
    }
    Some(citations)
}

fn apply_guest_task_outcome(
    store: &SqliteWorkflowStore,
    run_id: &str,
    lease: &ExecutionLease,
    task_outcome: vam_spec::v1::TaskOutcome,
) -> Result<DispatchOutcome, OrchestratorError> {
    let _ = store.end_active_segment(run_id, unix_now_secs());
    let spend_micro = store
        .get_run_session_id(run_id)
        .ok()
        .flatten()
        .and_then(|sess_id| store.get_session_budget(&sess_id))
        .map(|budget| budget.get_accumulated_cost());

    let state =
        TaskOutcomeState::try_from(task_outcome.state).unwrap_or(TaskOutcomeState::Unspecified);

    if state == TaskOutcomeState::Suspended {
        store.record_outcome(&task_outcome)?;
        let key = crate::error::AttemptKey {
            run_id: run_id.to_string(),
            step_id: lease.step_id.clone(),
            attempt_id: lease
                .attempt_id
                .as_ref()
                .map(|a| a.urn.clone())
                .unwrap_or_default(),
        };
        let request_id = store
            .find_pending_request_for_attempt(&key)?
            .ok_or_else(|| {
                OrchestratorError::InvalidContract(
                    "SUSPENDED outcome without pending HITL request".into(),
                )
            })?;
        let checkpoint = task_outcome.continuation.clone().ok_or_else(|| {
            OrchestratorError::InvalidContract(
                "SUSPENDED outcome missing continuation artifact".into(),
            )
        })?;
        // The run is parked before its approval can be decided: the decision that resumes it
        // moves the run PAUSED -> RUNNING (`claim_parked_run`), so a decision must never find the
        // run still RUNNING.
        store.update_run_state(run_id, "PAUSED")?;
        store.create_approval(&key, &ResourceId { urn: request_id }, &checkpoint, 1)?;
        // The guest uploads the checkpoint before it returns SUSPENDED. The
        // keep-warm window starts at this pause. Gate waits that enter PAUSED
        // with no available task step use the same timer.
        store.arm_keep_warm(run_id, crate::workflow::DEFAULT_KEEP_WARM_SECS)?;
        return Ok(DispatchOutcome {
            answer: String::new(),
            succeeded: false,
            paused: true,
            interrupted: false,
            spend_micro,
        });
    }

    let _ = store.record_outcome(&task_outcome);

    if state == TaskOutcomeState::Failed {
        let error_msg = task_outcome
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .filter(|m| !m.is_empty())
            .unwrap_or_else(|| "task failed".to_string());
        // The guest names a storage failure by this prefix (a turn commit or a
        // suspend the host could not record); the run's `reason` says so.
        let result_json =
            if error_msg.starts_with(crate::turn_commit::RUN_ERROR_STORAGE_COMMIT_FAILED) {
                serde_json::json!({
                    "error": error_msg,
                    "reason": crate::turn_commit::RUN_ERROR_STORAGE_COMMIT_FAILED,
                })
            } else {
                serde_json::json!({ "error": error_msg })
            }
            .to_string();
        store.complete_run_with_result(run_id, "FAILED", &result_json)?;
        return Ok(DispatchOutcome {
            answer: String::new(),
            succeeded: false,
            paused: false,
            interrupted: false,
            spend_micro,
        });
    }

    let answer = answer_from_task_outcome(&task_outcome);
    eprintln!("[orchestrator] run {} agent answer:\n{}", run_id, answer);
    let claimed = claimed_from_task_outcome(&task_outcome);
    let run_output = vam_spec::playbook_run_output_json(&answer);
    let citations = web_citations_for_answer(store, run_id, &answer);

    match store.check_playbook_finish(run_id, Some(&run_output))? {
        None => {
            let result_json = answer_result_json(&answer, claimed, citations.as_ref());
            store.complete_run_with_result(run_id, "SUCCEEDED", &result_json)?;
            Ok(DispatchOutcome {
                answer,
                succeeded: true,
                paused: false,
                interrupted: false,
                spend_micro,
            })
        }
        Some(crate::workflow::PlaybookFinishCheck::Complete) => {
            store.set_playbook_run_state(run_id, "succeeded")?;
            let result_json = answer_result_json(&answer, claimed, citations.as_ref());
            store.complete_run_with_result(run_id, "SUCCEEDED", &result_json)?;
            Ok(DispatchOutcome {
                answer,
                succeeded: true,
                paused: false,
                interrupted: false,
                spend_micro,
            })
        }
        Some(crate::workflow::PlaybookFinishCheck::Incomplete {
            missing_steps,
            open_gates,
            schema_error,
        }) => {
            // An open gate with no remaining task step still parks (PB-2.1).
            // The run does not succeed. When a task is still available, or the
            // only gap is the output schema, the run fails with the gap listed.
            if store.park_for_open_gates(run_id)?.is_some() {
                return Ok(DispatchOutcome {
                    answer,
                    succeeded: false,
                    paused: true,
                    interrupted: false,
                    spend_micro,
                });
            }
            store.set_playbook_run_state(run_id, "failed")?;
            let error = crate::workflow::playbook_finish_error(
                &missing_steps,
                &open_gates,
                schema_error.as_deref(),
            );
            eprintln!("[orchestrator] run {} {}", run_id, error);
            let mut result = serde_json::json!({
                "error": error,
                "missing_steps": missing_steps,
                "open_gates": open_gates,
                "schema_error": schema_error,
                "answer": answer,
            });
            if let Some(flag) = claimed {
                result["claimed"] = serde_json::json!(flag);
            }
            if let Some(citations) = citations.as_ref() {
                crate::web_sources::apply_citations(&mut result, citations);
            }
            let result_json = result.to_string();
            store.complete_run_with_result(run_id, "FAILED", &result_json)?;
            Ok(DispatchOutcome {
                answer,
                succeeded: false,
                paused: false,
                interrupted: false,
                spend_micro,
            })
        }
    }
}

async fn dispatch_run_prompt(
    store: Arc<SqliteWorkflowStore>,
    run_id: String,
    session_id: String,
    prompt: String,
    inputs: Option<prost_types::Struct>,
    timer: Option<crate::timing::SharedPhaseTimer>,
) -> Result<DispatchOutcome, OrchestratorError> {
    let record = store.load_session(&session_id)?;
    let vsock_path = vam_core::jail::vsock_uds_path(&record.vm_id);
    let lease = build_run_lease(&run_id, record.lifecycle_generation.max(1));
    store.issue_lease(&lease)?;
    crate::timing::mark_shared(&timer, "lease_issued");
    crate::timing::finish_shared(&timer);
    if active_budget_exhausted(store.as_ref(), &run_id) {
        let message = "run exceeded max_duration of active time";
        let result_json = serde_json::json!({ "error": message }).to_string();
        let _ = store.release_run_reservation(&run_id);
        let _ = store.clear_keep_warm(&run_id);
        let _ = store.complete_run_with_result(&run_id, "FAILED", &result_json);
        return Err(OrchestratorError::Core(message.into()));
    }
    let timeout_secs = guest_task_timeout_secs(store.as_ref(), &run_id);
    let _ = store.begin_active_segment(&run_id, unix_now_secs());
    let outcome = crate::driver::run_task_on_guest_classified(
        &vsock_path,
        &lease,
        &prompt,
        inputs,
        timeout_secs,
        None,
    )
    .await;
    match outcome {
        Ok(task_outcome) => {
            let dispatch = apply_guest_task_outcome(store.as_ref(), &run_id, &lease, task_outcome)?;
            if dispatch.paused {
                if let Err(err) =
                    crate::hsp_hooks::notify_run_paused(store.as_ref(), &run_id, &session_id).await
                {
                    eprintln!(
                        "[orchestrator] on_paused notification failed for run {run_id}: {err}"
                    );
                }
            }
            dispatch_on_complete_for_terminal(store.as_ref(), &run_id, &session_id).await?;
            Ok(dispatch)
        }
        Err(e) => {
            eprintln!("[orchestrator] guest task failed for run {}: {}", run_id, e);
            let _ = store.end_active_segment(&run_id, unix_now_secs());
            // A guest that died with no stop recorded is not the end of the task: the caller
            // takes the session down and the run waits to be picked up again.
            if crate::resume::classify(&store, &run_id, &session_id, &e)
                == crate::resume::Verdict::Crash
            {
                return Ok(DispatchOutcome {
                    answer: String::new(),
                    succeeded: false,
                    paused: false,
                    interrupted: true,
                    spend_micro: None,
                });
            }
            let result_json = if store.session_ends.stop_requested(&session_id) {
                serde_json::json!({ "error": "the task was stopped", "reason": "stopped" })
            } else {
                serde_json::json!({ "error": e.message() })
            }
            .to_string();
            store.complete_run_with_result(&run_id, "FAILED", &result_json)?;
            let _ = store.release_run_reservation(&run_id);
            let _ = dispatch_on_complete_for_terminal(store.as_ref(), &run_id, &session_id).await;
            Err(OrchestratorError::Core(e.to_string()))
        }
    }
}

/// Best-effort OnComplete after a run reaches a terminal state (SUCCEEDED/FAILED).
/// Required FAIL marks a succeeding run FAILED.
async fn dispatch_on_complete_for_terminal(
    store: &SqliteWorkflowStore,
    run_id: &str,
    session_id: &str,
) -> Result<(), OrchestratorError> {
    let run_state = store.get_run_state(run_id)?;
    let upper = run_state.to_ascii_uppercase();
    if upper != "SUCCEEDED"
        && upper != "FAILED"
        && upper != "CANCELLED"
        && upper != "TIMED_OUT"
        && upper != "REJECTED"
    {
        return Ok(());
    }
    let state_enum = match upper.as_str() {
        "SUCCEEDED" => run_status::RunState::Succeeded,
        "FAILED" => run_status::RunState::Failed,
        "CANCELLED" => run_status::RunState::Cancelled,
        "TIMED_OUT" => run_status::RunState::TimedOut,
        "REJECTED" => run_status::RunState::Rejected,
        _ => run_status::RunState::Failed,
    };
    let status = RunStatus {
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        state: state_enum as i32,
        session_id: session_id.to_string(),
        ..Default::default()
    };
    let result = crate::hsp_hooks::HspHookDispatcher::new(store)
        .dispatch_on_complete(run_id, session_id, status)
        .await?;
    if result.aborted && upper == "SUCCEEDED" {
        let _ = store.update_run_state(run_id, "FAILED");
    }
    Ok(())
}

/// A cold resume ties the new session to the run before its brokers start.
#[cfg(test)]
mod cold_resume_binding_tests {
    use super::*;
    use crate::manager::ManagerCommand;
    use crate::session::ObservedState;

    const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/guarded";
    const PRIOR: &str = "sess-prior";

    /// What the store says about the new session at the moment the manager is
    /// asked to boot it (that is when its brokers resolve their identity).
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct SeenAtBoot {
        run: Option<String>,
        agent: Option<String>,
        owner: String,
    }

    struct Parked {
        store: Arc<SqliteWorkflowStore>,
        run_id: String,
        record: crate::error::ApprovalRecord,
    }

    fn seed_cold_parked_run(label: &str, agent_urn: &str) -> Parked {
        let run_id = format!("urn:vam:run:local.vam.dev/acme/run-{label}");
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        store.initialize_catalog_schema().expect("catalog schema");
        store.create_user("alice", "alice").expect("alice");
        store
            .persist_run_spec_with_owner(
                &run_id,
                r#"{"inputs":{"prompt":"finish after approval"}}"#,
                Some(PRIOR),
                agent_urn,
                None,
                "User::\"alice\"",
                "alice",
            )
            .expect("run");
        // The parked session's own row carries the implicit local owner, so a
        // resumed session that copied it would be wrong.
        store
            .create_session(PRIOR, None, "vm-prior", "ws-cold", "default")
            .expect("prior session");
        store
            .update_observed_state(PRIOR, ObservedState::Terminated)
            .expect("terminated");
        store.update_run_state(&run_id, "PAUSED").expect("paused");
        let record = crate::error::ApprovalRecord {
            request_id: format!("urn:vam:hitl:local.vam.dev/acme/approval-{label}"),
            run_id: run_id.clone(),
            step_id: "live-step".to_string(),
            attempt_id: "attempt-1".to_string(),
            session_id: PRIOR.to_string(),
            checkpoint_digest: String::new(),
            continuation_generation: 2,
            tool_name: "write_file".to_string(),
            args_digest: "digest".to_string(),
            decision_revision: 1,
        };
        Parked {
            store,
            run_id,
            record,
        }
    }

    /// A manager whose boot is refused after recording what the store knew about
    /// the session. Returns the number of boots asked for and the first sighting.
    fn refusing_manager(
        store: Arc<SqliteWorkflowStore>,
    ) -> (
        SessionManager,
        Arc<std::sync::Mutex<Vec<SeenAtBoot>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let (manager, mut rx) = SessionManager::new(
            store.clone(),
            Arc::new(vam_core::CidAllocator::default()),
            Arc::new(|_, _| Box::new(vam_core::MockVmm::new())),
        );
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_loop = seen.clone();
        let handle = tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                match command {
                    ManagerCommand::CreateSession {
                        session_id, reply, ..
                    } => {
                        seen_loop.lock().unwrap().push(SeenAtBoot {
                            run: store.find_run_id_for_session(&session_id).unwrap(),
                            agent: store.find_agent_urn_for_session(&session_id).unwrap(),
                            owner: store.session_owner_user_id(&session_id).unwrap_or_default(),
                        });
                        let _ = reply.send(Err(OrchestratorError::Core(
                            "boot refused by test".to_string(),
                        )));
                    }
                    ManagerCommand::TerminateSession { reply, .. } => {
                        let _ = reply.send(Ok(()));
                    }
                    _ => {}
                }
            }
        });
        (manager, seen, handle)
    }

    async fn resume_with(parked: &Parked, inputs: prost_types::Struct) -> OrchestratorError {
        let (manager, seen, handle) = refusing_manager(parked.store.clone());
        let err = resume_run_after_hitl_decision(
            manager.clone(),
            parked.store.clone(),
            parked.record.clone(),
            inputs,
        )
        .await
        .expect_err("the boot is refused");

        let seen = seen.lock().unwrap().clone();
        assert_eq!(
            seen,
            vec![SeenAtBoot {
                run: Some(parked.run_id.clone()),
                agent: Some(AGENT.to_string()),
                owner: "alice".to_string(),
            }],
            "the session is tied to the run, its agent and its owner before it boots"
        );
        // Boot failed: the run fails with the error, stays bound to the failed
        // new session, and the park is released.
        assert_eq!(
            parked.store.get_run_state(&parked.run_id).unwrap(),
            "FAILED"
        );
        let (_, bound_session, _) = parked.store.get_run_spec_context(&parked.run_id).unwrap();
        assert_ne!(bound_session.as_deref(), Some(PRIOR));
        assert!(bound_session.is_some_and(|s| s.starts_with("sess-")));
        assert!(parked
            .store
            .workspace_reservation("ws-cold")
            .unwrap()
            .is_none());
        drop(manager);
        handle.await.unwrap();
        err
    }

    #[tokio::test]
    async fn cold_resume_after_approval_binds_the_run_before_boot() {
        let parked = seed_cold_parked_run("cold-approve", AGENT);
        let err = resume_with(&parked, prost_types::Struct::default()).await;
        assert!(err.to_string().contains("boot refused by test"), "{err}");
    }

    #[tokio::test]
    async fn cold_resume_after_rejection_binds_the_run_before_boot() {
        let parked = seed_cold_parked_run("cold-reject", AGENT);
        let mut inputs = prost_types::Struct::default();
        inputs.fields.insert(
            "hitl_denied".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        let err = resume_with(&parked, inputs).await;
        assert!(err.to_string().contains("boot refused by test"), "{err}");
    }

    #[tokio::test]
    async fn cold_gate_resume_binds_the_run_before_boot() {
        // The gate entry builds its own inputs and shares the resume function.
        let parked = seed_cold_parked_run("cold-gate", AGENT);
        let inputs = build_gate_resume_inputs(
            parked.store.as_ref(),
            &parked.run_id,
            "approve",
            "submitted",
            "{}",
        )
        .expect("gate inputs");
        let err = resume_with(&parked, inputs).await;
        assert!(err.to_string().contains("boot refused by test"), "{err}");
    }

    #[tokio::test]
    async fn a_second_resume_with_the_key_held_starts_nothing_and_leaves_the_run_alone() {
        let parked = seed_cold_parked_run("cold-twice", AGENT);
        // The first resume is in flight: its session holds the resume key
        // (`resume-<prior>-<generation>`, generation 2) and the run is bound.
        parked
            .store
            .create_session_with_owner(
                "sess-first",
                Some("resume-sess-prior-2"),
                "vm-first",
                "ws-cold",
                "default",
                "alice",
            )
            .expect("first resume's session");
        parked
            .store
            .set_run_session_id(&parked.run_id, "sess-first")
            .expect("bind");
        parked
            .store
            .update_run_state(&parked.run_id, "RUNNING")
            .expect("running");

        let (manager, seen, handle) = refusing_manager(parked.store.clone());
        // The second resume is a no-op that succeeds: the first resume owns the run, and a
        // failure here would make the decision's caller fail the run.
        resume_run_after_hitl_decision(
            manager.clone(),
            parked.store.clone(),
            parked.record.clone(),
            prost_types::Struct::default(),
        )
        .await
        .expect("the second resume starts nothing");

        assert!(seen.lock().unwrap().is_empty(), "nothing may boot");
        // The run is not failed and stays bound to the first resume's session.
        assert_eq!(
            parked.store.get_run_state(&parked.run_id).unwrap(),
            "RUNNING"
        );
        let (_, bound, _) = parked.store.get_run_spec_context(&parked.run_id).unwrap();
        assert_eq!(bound.as_deref(), Some("sess-first"));
        assert_eq!(
            parked
                .store
                .find_run_id_for_session("sess-first")
                .unwrap()
                .as_deref(),
            Some(parked.run_id.as_str())
        );
        // No second session row was created, and the first is untouched.
        let ids: Vec<String> = parked
            .store
            .load_all_sessions()
            .unwrap()
            .into_iter()
            .map(|s| s.session_id)
            .collect();
        assert_eq!(ids.len(), 2, "{ids:?}");
        assert_eq!(
            parked
                .store
                .load_session("sess-first")
                .unwrap()
                .observed_state,
            ObservedState::Creating
        );
        drop(manager);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn a_decision_for_a_run_that_is_not_parked_resumes_nothing() {
        let parked = seed_cold_parked_run("not-parked", AGENT);
        // A resume is already in flight: the run is RUNNING, not parked.
        parked
            .store
            .update_run_state(&parked.run_id, "RUNNING")
            .expect("running");
        let (manager, seen, handle) = refusing_manager(parked.store.clone());
        resume_after_hitl_decision(manager.clone(), parked.store.clone(), parked.record.clone())
            .await
            .expect("a second claim is a no-op");
        assert!(seen.lock().unwrap().is_empty(), "nothing may boot");
        assert_eq!(
            parked.store.get_run_state(&parked.run_id).unwrap(),
            "RUNNING"
        );
        drop(manager);
        handle.await.unwrap();
    }

    #[tokio::test]
    async fn cold_resume_of_a_run_with_no_agent_fails_before_any_boot() {
        let parked = seed_cold_parked_run("cold-noagent", "");
        let (manager, seen, handle) = refusing_manager(parked.store.clone());
        let err = resume_run_after_hitl_decision(
            manager.clone(),
            parked.store.clone(),
            parked.record.clone(),
            prost_types::Struct::default(),
        )
        .await
        .expect_err("a run without an agent is not resumed");
        assert!(err.to_string().contains("records no agent"), "{err}");
        assert!(seen.lock().unwrap().is_empty(), "nothing may boot for it");
        assert_eq!(
            parked.store.get_run_state(&parked.run_id).unwrap(),
            "FAILED"
        );
        drop(manager);
        handle.await.unwrap();
    }

    #[test]
    fn lease_ids_do_not_repeat_across_a_restart() {
        // A restart resets the in-process counter, so a resumed run's new
        // lease must not reuse an id the run already holds.
        let run_id = "urn:vam:run:local.vam.dev/acme/run-lease-ids";
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .persist_run_spec(run_id, "{}", Some("sess-l"), AGENT, None, "User::\"t\"")
            .expect("run");
        let mut earlier = build_run_lease(run_id, 1);
        earlier.attempt_id = Some(ResourceId {
            urn: "urn:vam:attempt:local.vam.dev/acme/attempt-1".to_string(),
        });
        earlier.lease_id = Some(ResourceId {
            urn: "urn:vam:lease:local.vam.dev/acme/lease-1".to_string(),
        });
        store.issue_lease(&earlier).expect("pre-restart lease");
        let first = build_run_lease(run_id, 2);
        let second = build_run_lease(run_id, 2);
        assert_ne!(first.attempt_id, second.attempt_id);
        assert_ne!(first.lease_id, second.lease_id);
        store.issue_lease(&first).expect("first resume lease");
        store.issue_lease(&second).expect("second resume lease");
    }
}

#[cfg(test)]
mod hitl_resume_tests {
    use super::*;
    use vam_spec::v1::agent_spec::InstructionSource;
    use vam_spec::v1::{AgentSpec, ArtifactRef, Digest, ModelPreference, TaskOutcome};

    #[test]
    fn gate_decisions_resume_as_gates_and_tool_decisions_stay_on_the_tool_path() {
        assert_eq!(
            hitl_resume_kind(Some("SUBMITTED"), crate::hitl::APPROVAL_KIND_GATE),
            HitlResumeKind::Gate
        );
        assert_eq!(
            hitl_resume_kind(Some("REJECTED"), crate::hitl::APPROVAL_KIND_GATE),
            HitlResumeKind::Gate
        );
        assert_eq!(
            hitl_resume_kind(Some("APPROVED"), crate::hitl::APPROVAL_KIND_TOOL_CALL),
            HitlResumeKind::Approved
        );
        assert_eq!(
            hitl_resume_kind(Some("REJECTED"), crate::hitl::APPROVAL_KIND_TOOL_CALL),
            HitlResumeKind::Rejected
        );
        assert_eq!(
            hitl_resume_kind(Some("SUBMITTED"), crate::hitl::APPROVAL_KIND_TOOL_CALL),
            HitlResumeKind::None
        );
    }

    #[test]
    fn only_a_run_being_worked_on_is_interrupted_by_a_stop() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        assert!(
            !run_is_interruptible(&store, "no-such-session"),
            "no run, nothing to interrupt"
        );
        store
            .persist_run_spec(
                "urn:vam:run:local.vam.dev/acme/run-stop",
                r#"{"inputs":{"prompt":"go"}}"#,
                Some("sess-stop"),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("run");
        for (state, interruptible) in [
            ("RUNNING", true),
            ("PAUSED", false),
            ("SUCCEEDED", false),
            ("FAILED", false),
            ("CANCELLED", false),
        ] {
            store
                .update_run_state("urn:vam:run:local.vam.dev/acme/run-stop", state)
                .expect("state");
            assert_eq!(
                run_is_interruptible(&store, "sess-stop"),
                interruptible,
                "{state}"
            );
        }
    }

    #[test]
    fn cold_resume_generation_is_higher_than_the_parked_session() {
        assert_eq!(resume_lease_generation(1, 2), 2);
        assert_eq!(resume_lease_generation(4, 2), 5);
        assert_eq!(resume_lease_generation(0, 0), 1);
    }

    fn hitl_checkpoint_artifact(suffix: &str) -> ArtifactRef {
        ArtifactRef {
            artifact_id: Some(ResourceId {
                urn: format!("urn:vam:artifact:local.vam.dev/acme/cp-{suffix}"),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            media_type: "application/octet-stream".to_string(),
            size_bytes: 0,
            uri: "memory://checkpoint".to_string(),
        }
    }

    fn catalog_agent(urn: &str) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Journey Agent".to_string(),
            description: "test".to_string(),
            role: "Staff engineer".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions(
                "Finish every remaining step after approval.".to_string(),
            )),
            preferred_models: vec![ModelPreference {
                provider: "openrouter".to_string(),
                model_name: "anthropic/claude-sonnet-4".to_string(),
                temperature: 0.1,
                top_p: 1.0,
                max_completion_tokens: 4096,
            }],
            allow_any_playbook: true,
            ..Default::default()
        }
    }

    #[test]
    fn failed_task_outcome_is_not_stored_as_succeeded() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-hitl-fail";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");

        let lease = build_run_lease(run_id, 2);
        let task_outcome = TaskOutcome {
            state: TaskOutcomeState::Failed as i32,
            error: Some(vam_spec::v1::ErrorStatus {
                code: 13,
                message: "PEP authorization failed".to_string(),
                details: vec![],
                retryable: false,
            }),
            ..Default::default()
        };

        let outcome =
            apply_guest_task_outcome(&store, run_id, &lease, task_outcome).expect("apply");
        assert!(!outcome.succeeded);
        assert!(!outcome.paused);
        assert_eq!(store.get_run_state(run_id).expect("state"), "FAILED");
        let result = store
            .get_run_result_json(run_id)
            .expect("result")
            .expect("result json");
        assert!(result.contains("PEP authorization failed"));
        assert!(!result.contains("\"answer\""));
    }

    fn playbook_pin() -> crate::workflow::PlaybookRunPin {
        crate::workflow::PlaybookRunPin {
            urn: "urn:vam:playbook:local.vam.dev/acme/finish".to_string(),
            version: "1.0.0".to_string(),
            digest: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
        }
    }

    fn playbook_task(id: &str, after: &[&str]) -> vam_spec::v1::PlaybookStep {
        vam_spec::v1::PlaybookStep {
            id: id.to_string(),
            kind: vam_spec::v1::StepKind::Task as i32,
            instruction: format!("do {id}"),
            after: after.iter().map(|token| (*token).to_string()).collect(),
            ..Default::default()
        }
    }

    fn succeeded_task_outcome(answer: &str) -> TaskOutcome {
        TaskOutcome {
            state: TaskOutcomeState::Succeeded as i32,
            output: Some(prost_types::Struct {
                fields: [(
                    "answer".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(answer.to_string())),
                    },
                )]
                .into_iter()
                .collect(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn claimed_flag_reaches_the_stored_result_that_run_status_reports() {
        for (name, claimed, expected) in [
            ("false", Some(false), Some(false)),
            ("true", Some(true), Some(true)),
            ("absent", None, None),
        ] {
            let run_id = format!("urn:vam:run:local.vam.dev/acme/run-claim-{name}");
            let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
            store.initialize_catalog_schema().expect("catalog schema");
            store
                .create_run(&ResourceId {
                    urn: run_id.clone(),
                })
                .expect("create run");
            let lease = build_run_lease(&run_id, 1);
            let mut outcome = succeeded_task_outcome("text only");
            if let Some(flag) = claimed {
                outcome.output.as_mut().expect("output").fields.insert(
                    "claimed".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::BoolValue(flag)),
                    },
                );
            }
            apply_guest_task_outcome(&store, &run_id, &lease, outcome).expect("apply");
            let stored = store
                .get_run_result_json(&run_id)
                .expect("result")
                .expect("stored result");
            // `RunStatus.final_outputs` is built from this JSON (`build_run_status`).
            let final_outputs = json_object_to_struct(&stored).expect("struct");
            let got = match final_outputs
                .fields
                .get("claimed")
                .and_then(|v| v.kind.as_ref())
            {
                Some(prost_types::value::Kind::BoolValue(flag)) => Some(*flag),
                _ => None,
            };
            assert_eq!(got, expected, "{stored}");
        }
    }

    #[test]
    fn successful_guest_task_without_playbook_still_succeeds() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-finish-plain";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");
        let lease = build_run_lease(run_id, 1);
        let outcome =
            apply_guest_task_outcome(&store, run_id, &lease, succeeded_task_outcome("all good"))
                .expect("apply");
        assert!(outcome.succeeded);
        assert_eq!(store.get_run_state(run_id).expect("state"), "SUCCEEDED");
    }

    #[test]
    fn successful_guest_task_fails_when_required_playbook_steps_missing() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-finish-missing";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");
        store
            .init_playbook_run(
                run_id,
                &playbook_pin(),
                &vam_spec::v1::PlaybookSpec {
                    steps: vec![
                        playbook_task("first", &[]),
                        playbook_task("second", &["first"]),
                    ],
                    ..Default::default()
                },
            )
            .expect("init");

        let lease = build_run_lease(run_id, 1);
        let outcome =
            apply_guest_task_outcome(&store, run_id, &lease, succeeded_task_outcome("done early"))
                .expect("apply");
        assert!(!outcome.succeeded);
        assert_eq!(store.get_run_state(run_id).expect("state"), "FAILED");
        let result = store
            .get_run_result_json(run_id)
            .expect("result")
            .expect("result json");
        assert!(result.contains("missing required steps"));
        assert!(result.contains("\"first\""));
        assert!(result.contains("\"second\""));
        assert!(result.contains("done early"));
        let header = store
            .get_playbook_run(run_id)
            .expect("header")
            .expect("row");
        assert_eq!(header.state, "failed");
    }

    #[test]
    fn successful_guest_task_succeeds_when_playbook_steps_complete() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-finish-ok";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");
        store
            .init_playbook_run(
                run_id,
                &playbook_pin(),
                &vam_spec::v1::PlaybookSpec {
                    steps: vec![playbook_task("only", &[])],
                    ..Default::default()
                },
            )
            .expect("init");
        store
            .complete_playbook_step(
                run_id,
                "only",
                &crate::workflow::PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{"ok":true}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect("complete");

        let lease = build_run_lease(run_id, 1);
        let outcome =
            apply_guest_task_outcome(&store, run_id, &lease, succeeded_task_outcome("finished"))
                .expect("apply");
        assert!(outcome.succeeded);
        assert_eq!(store.get_run_state(run_id).expect("state"), "SUCCEEDED");
        let header = store
            .get_playbook_run(run_id)
            .expect("header")
            .expect("row");
        assert_eq!(header.state, "succeeded");
    }

    fn run_output_schema() -> prost_types::Struct {
        vam_spec::encoding::json_to_proto(
            "google.protobuf.Struct",
            r#"{"type":"object","additionalProperties":false,"required":["report_file_path","transitioned_count"],"properties":{"report_file_path":{"type":"string"},"transitioned_count":{"type":"integer"}}}"#,
        )
        .expect("schema")
    }

    #[test]
    fn run_output_that_fails_output_schema_is_failed_not_succeeded() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-finish-schema";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");
        store
            .init_playbook_run(
                run_id,
                &playbook_pin(),
                &vam_spec::v1::PlaybookSpec {
                    steps: vec![playbook_task("only", &[])],
                    output_schema: Some(run_output_schema()),
                    ..Default::default()
                },
            )
            .expect("init");
        store
            .complete_playbook_step(
                run_id,
                "only",
                &crate::workflow::PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{"ok":true}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect("complete");

        let lease = build_run_lease(run_id, 1);
        let outcome =
            apply_guest_task_outcome(&store, run_id, &lease, succeeded_task_outcome("finished"))
                .expect("apply");
        assert!(!outcome.succeeded);
        assert_eq!(store.get_run_state(run_id).expect("state"), "FAILED");
        let result = store
            .get_run_result_json(run_id)
            .expect("result")
            .expect("result json");
        assert!(result.contains("output schema") || result.contains("schema_error"));
        assert!(result.contains("report_file_path") || result.contains("answer"));
        assert!(result.contains("\"missing_steps\""));
        assert!(result.contains("finished"));
        let header = store
            .get_playbook_run(run_id)
            .expect("header")
            .expect("row");
        assert_eq!(header.state, "failed");
    }

    #[test]
    fn complete_playbook_with_matching_run_output_succeeds() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-finish-schema-ok";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");
        store
            .init_playbook_run(
                run_id,
                &playbook_pin(),
                &vam_spec::v1::PlaybookSpec {
                    steps: vec![playbook_task("only", &[])],
                    output_schema: Some(run_output_schema()),
                    ..Default::default()
                },
            )
            .expect("init");
        store
            .complete_playbook_step(
                run_id,
                "only",
                &crate::workflow::PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{"ok":true}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect("complete");

        let lease = build_run_lease(run_id, 1);
        let outcome = apply_guest_task_outcome(
            &store,
            run_id,
            &lease,
            succeeded_task_outcome(
                r#"{"report_file_path":"notes/report.md","transitioned_count":2}"#,
            ),
        )
        .expect("apply");
        assert!(outcome.succeeded);
        assert_eq!(store.get_run_state(run_id).expect("state"), "SUCCEEDED");
        let header = store
            .get_playbook_run(run_id)
            .expect("header")
            .expect("row");
        assert_eq!(header.state, "succeeded");
    }

    #[test]
    fn open_gate_prevents_success_and_is_listed_when_a_task_is_still_available() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-finish-gate";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");
        let mut gate = playbook_task("approve", &[]);
        gate.kind = vam_spec::v1::StepKind::Gate as i32;
        gate.instruction.clear();
        gate.options = vec![
            vam_spec::v1::StepOutcome {
                label: "approved".to_string(),
                ..Default::default()
            },
            vam_spec::v1::StepOutcome {
                label: "rejected".to_string(),
                terminal: vam_spec::v1::OutcomeTerminal::Succeeded as i32,
                ..Default::default()
            },
        ];
        store
            .init_playbook_run(
                run_id,
                &playbook_pin(),
                &vam_spec::v1::PlaybookSpec {
                    steps: vec![playbook_task("work", &[]), gate],
                    ..Default::default()
                },
            )
            .expect("init");

        let lease = build_run_lease(run_id, 1);
        let outcome =
            apply_guest_task_outcome(&store, run_id, &lease, succeeded_task_outcome("done early"))
                .expect("apply");
        assert!(!outcome.succeeded);
        assert!(!outcome.paused);
        assert_eq!(store.get_run_state(run_id).expect("state"), "FAILED");
        let result = store
            .get_run_result_json(run_id)
            .expect("result")
            .expect("result json");
        assert!(result.contains("\"work\""));
        assert!(result.contains("\"approve\""));
        assert!(result.contains("open_gates"));
        assert!(result.contains("missing required steps") || result.contains("missing_steps"));
    }

    #[test]
    fn guest_task_timeout_secs_defaults_when_unset() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .persist_run_spec(
                "urn:vam:run:local.vam.dev/acme/run-timeout-unset",
                r#"{"inputs":{"prompt":"hello"}}"#,
                None,
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");

        assert_eq!(DEFAULT_GUEST_TASK_TIMEOUT_SECS, 1_800);
        assert_eq!(
            guest_task_timeout_secs(&store, "urn:vam:run:local.vam.dev/acme/run-timeout-unset"),
            1_800
        );
        assert_eq!(
            guest_task_timeout_secs(&store, "urn:vam:run:missing"),
            DEFAULT_GUEST_TASK_TIMEOUT_SECS
        );
    }

    #[test]
    fn guest_task_timeout_secs_honors_max_duration() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .persist_run_spec(
                "urn:vam:run:local.vam.dev/acme/run-timeout-1800",
                r#"{"inputs":{"prompt":"hello"},"maxDuration":"1800s"}"#,
                None,
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");

        assert_eq!(
            guest_task_timeout_secs(&store, "urn:vam:run:local.vam.dev/acme/run-timeout-1800"),
            1800
        );
    }

    #[test]
    fn guest_task_timeout_secs_subtracts_only_active_time() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        let run_id = "urn:vam:run:local.vam.dev/acme/run-timeout-active";
        store
            .persist_run_spec(
                run_id,
                r#"{"inputs":{"prompt":"hello"},"maxDuration":"1800s"}"#,
                None,
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");
        store.begin_active_segment(run_id, 1_000).expect("start");
        store.end_active_segment(run_id, 1_100).expect("pause");
        // A day of parked time must not shrink the next segment's wait.
        assert_eq!(
            store
                .active_elapsed_secs(run_id, 1_100 + 86_400)
                .expect("elapsed"),
            100
        );
        assert_eq!(guest_task_timeout_secs(&store, run_id), 1_700);
    }

    #[test]
    fn guest_task_timeout_secs_ignores_zero_or_negative_max_duration() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        for (run_id, spec_json) in [
            (
                "urn:vam:run:local.vam.dev/acme/run-timeout-zero",
                r#"{"inputs":{"prompt":"hello"},"maxDuration":"0s"}"#,
            ),
            (
                "urn:vam:run:local.vam.dev/acme/run-timeout-neg",
                r#"{"maxDuration":{"seconds":-5,"nanos":0}}"#,
            ),
        ] {
            store
                .persist_run_spec(
                    run_id,
                    spec_json,
                    None,
                    "urn:vam:agent:test",
                    None,
                    "User::\"tester\"",
                )
                .expect("persist spec");
            assert_eq!(
                guest_task_timeout_secs(&store, run_id),
                DEFAULT_GUEST_TASK_TIMEOUT_SECS,
                "run_id={run_id}"
            );
        }
    }

    #[test]
    fn build_hitl_resume_inputs_adds_resume_note() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .persist_run_spec(
                "urn:vam:run:local.vam.dev/acme/run-hitl-note",
                r#"{"inputs":{"prompt":"transition CGA-1298"}}"#,
                None,
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");

        let inputs = build_hitl_resume_inputs(
            &store,
            "urn:vam:run:local.vam.dev/acme/run-hitl-note",
            "jira_transition_issue",
            r#"{"issue_key":"CGA-1298"}"#,
        )
        .expect("resume inputs");

        assert_eq!(
            inputs
                .fields
                .get("hitl_resume")
                .and_then(|v| match &v.kind {
                    Some(prost_types::value::Kind::BoolValue(b)) => Some(*b),
                    _ => None,
                }),
            Some(true)
        );
        assert!(inputs.fields.contains_key("hitl_resume_note"));
        assert_eq!(
            string_from_struct_field(&inputs, "prompt").as_deref(),
            Some("transition CGA-1298")
        );
        let note = string_from_struct_field(&inputs, "hitl_resume_note").expect("note");
        assert!(note.contains("Continue the original task"));
        assert!(!note.contains("Summarize the result"));
        assert!(
            string_from_struct_field(&inputs, "hitl_continuation").is_none(),
            "resume without an uploaded checkpoint must not invent one"
        );
    }

    #[test]
    fn build_hitl_resume_inputs_references_host_checkpoint() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        let run_id = "urn:vam:run:local.vam.dev/acme/run-hitl-checkpoint";
        store
            .persist_run_spec(
                run_id,
                r#"{"inputs":{"prompt":"transition CGA-1298"}}"#,
                Some("sess-hitl-checkpoint"),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");

        let generation = 1i64;
        let body = format!(
            r#"{{"schema_version":1,"run_id":"{run_id}","lifecycle_generation":{generation},"marker":"from-host"}}"#
        );
        let digest = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(body.as_bytes())
        );
        store
            .put_run_checkpoint(run_id, generation, &digest, body.as_bytes())
            .expect("store checkpoint");

        let inputs = build_hitl_resume_inputs(
            &store,
            run_id,
            "jira_transition_issue",
            r#"{"issue_key":"CGA-1298"}"#,
        )
        .expect("resume inputs");
        assert!(
            string_from_struct_field(&inputs, "hitl_continuation").is_none(),
            "resume carries the checkpoint by digest"
        );
        assert_eq!(
            string_from_struct_field(&inputs, "hitl_continuation_digest").as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(
            string_from_struct_field(&inputs, "hitl_continuation_generation").as_deref(),
            Some("1")
        );
        assert!(!body.contains("from-workspace"));
    }

    #[test]
    fn build_gate_resume_inputs_references_host_checkpoint() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        let run_id = "urn:vam:run:local.vam.dev/acme/run-gate-checkpoint";
        store
            .persist_run_spec(
                run_id,
                r#"{"inputs":{"prompt":"approve the gate"}}"#,
                Some("sess-gate-checkpoint"),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");
        let body = format!(
            r#"{{"schema_version":2,"run_id":"{run_id}","lifecycle_generation":2,"marker":"gate"}}"#
        );
        let digest = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(body.as_bytes())
        );
        store
            .put_run_checkpoint(run_id, 2, &digest, body.as_bytes())
            .expect("store checkpoint");

        let inputs = build_gate_resume_inputs(
            &store,
            run_id,
            "approve-spend",
            "approved",
            r#"{"note":"ok"}"#,
        )
        .expect("gate resume inputs");
        assert!(string_from_struct_field(&inputs, "hitl_continuation").is_none());
        assert_eq!(
            string_from_struct_field(&inputs, "hitl_continuation_digest").as_deref(),
            Some(digest.as_str())
        );
        assert_eq!(
            string_from_struct_field(&inputs, "hitl_continuation_generation").as_deref(),
            Some("2")
        );
    }

    #[test]
    fn build_hitl_resume_inputs_preserves_max_iterations() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .persist_run_spec(
                "urn:vam:run:local.vam.dev/acme/run-hitl-iters",
                r#"{"inputs":{"prompt":"finish the job","max_iterations":15}}"#,
                None,
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");

        let inputs = build_hitl_resume_inputs(
            &store,
            "urn:vam:run:local.vam.dev/acme/run-hitl-iters",
            "jira_transition_issue",
            r#"{"issue_key":"CGA-1300"}"#,
        )
        .expect("resume inputs");

        assert_eq!(
            inputs
                .fields
                .get("max_iterations")
                .and_then(|v| match &v.kind {
                    Some(prost_types::value::Kind::NumberValue(n)) => Some(*n as u64),
                    _ => None,
                }),
            Some(15)
        );
        assert_eq!(
            string_from_struct_field(&inputs, "prompt").as_deref(),
            Some("finish the job")
        );
    }

    #[test]
    fn resume_inputs_carry_stored_iteration_and_step_budgets() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        let run_id = "urn:vam:run:local.vam.dev/acme/run-hitl-budgets";
        let spec = vam_spec::v1::RunSpec {
            inputs: Some(prost_types::Struct {
                fields: [
                    ("prompt".to_string(), string_input("finish the job")),
                    ("max_iterations".to_string(), string_input("50")),
                    ("step_timeout_secs".to_string(), string_input("300")),
                ]
                .into_iter()
                .collect(),
            }),
            max_duration: Some(prost_types::Duration {
                seconds: 1_800,
                nanos: 0,
            }),
            ..Default::default()
        };
        let spec_json =
            vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("encode spec");
        store
            .persist_run_spec(
                run_id,
                &spec_json,
                None,
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");

        let approval = build_hitl_resume_inputs(
            &store,
            run_id,
            "jira_transition_issue",
            r#"{"issue_key":"CGA-1301"}"#,
        )
        .expect("approval resume");
        let gate = build_gate_resume_inputs(&store, run_id, "approve-spend", "approved", "{}")
            .expect("gate resume");
        for inputs in [&approval, &gate] {
            assert_eq!(
                string_from_struct_field(inputs, "max_iterations").as_deref(),
                Some("50")
            );
            assert_eq!(
                string_from_struct_field(inputs, "step_timeout_secs").as_deref(),
                Some("300")
            );
        }
    }

    fn string_input(value: &str) -> prost_types::Value {
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(value.to_string())),
        }
    }

    #[test]
    fn build_hitl_resume_inputs_merges_catalog_agent_context() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        let agent_urn = "urn:vam:agent:local.vam.dev/acme/journey-agent";
        store
            .put_agent(&catalog_agent(agent_urn))
            .expect("put agent");
        store
            .persist_run_spec(
                "urn:vam:run:local.vam.dev/acme/run-hitl-agent",
                r#"{"inputs":{"prompt":"ship the remaining steps"}}"#,
                None,
                agent_urn,
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");

        let inputs = build_hitl_resume_inputs(
            &store,
            "urn:vam:run:local.vam.dev/acme/run-hitl-agent",
            "slack_post_message",
            r#"{"channel":"ops"}"#,
        )
        .expect("resume inputs");

        assert_eq!(
            string_from_struct_field(&inputs, "system_instructions").as_deref(),
            Some("Finish every remaining step after approval.")
        );
        assert_eq!(
            string_from_struct_field(&inputs, "agent_role").as_deref(),
            Some("Staff engineer")
        );
        assert_eq!(
            string_from_struct_field(&inputs, "model").as_deref(),
            Some("anthropic/claude-sonnet-4")
        );
        assert_eq!(
            string_from_struct_field(&inputs, "agent_display_name").as_deref(),
            Some("Journey Agent")
        );
    }

    #[test]
    fn build_hitl_rejection_resume_inputs_adds_denied_note() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .persist_run_spec(
                "urn:vam:run:local.vam.dev/acme/run-hitl-deny",
                r#"{"inputs":{"prompt":"move issue to in progress"}}"#,
                None,
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("persist spec");

        let inputs = build_hitl_rejection_resume_inputs(
            &store,
            "urn:vam:run:local.vam.dev/acme/run-hitl-deny",
            "jira_transition_issue",
            r#"{"issue_key":"CGA-1299"}"#,
            "Denied by user",
        )
        .expect("rejection resume inputs");

        assert_eq!(
            inputs
                .fields
                .get("hitl_denied")
                .and_then(|v| match &v.kind {
                    Some(prost_types::value::Kind::BoolValue(b)) => Some(*b),
                    _ => None,
                }),
            Some(true)
        );
        assert!(inputs.fields.contains_key("hitl_denied_note"));
        assert_eq!(
            string_from_struct_field(&inputs, "denial_reason").as_deref(),
            Some("Denied by user")
        );
    }

    /// Nested HITL: a paused DispatchOutcome must not imply teardown — session stays active.
    #[test]
    fn nested_hitl_pause_keeps_session_active() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-hitl-nested";
        let session_id = "sess-hitl-nested";
        let workspace_id = "ws-hitl-nested";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_session(session_id, None, "vm-hitl-nested", workspace_id, "default")
            .expect("create session");
        store
            .update_observed_state(session_id, ObservedState::Booting)
            .expect("booting");
        store
            .update_observed_state(session_id, ObservedState::Running)
            .expect("running");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");

        let lease = build_run_lease(run_id, 2);
        store.issue_lease(&lease).expect("lease");
        let key = crate::error::AttemptKey {
            run_id: run_id.to_string(),
            step_id: lease.step_id.clone(),
            attempt_id: lease
                .attempt_id
                .as_ref()
                .map(|a| a.urn.clone())
                .unwrap_or_default(),
        };
        let request_id = ResourceId {
            urn: "urn:vam:hitl:local.vam.dev/acme/nested-1".to_string(),
        };
        store
            .register_hitl_suspend(
                &key,
                session_id,
                &request_id,
                "jira_transition_issue",
                r#"{"issue_key":"CGA-1"}"#,
                "digest-nested",
                None,
            )
            .expect("register pending HITL");

        let checkpoint = ArtifactRef {
            artifact_id: Some(ResourceId {
                urn: "urn:vam:artifact:local.vam.dev/acme/cp-nested".to_string(),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            media_type: "application/octet-stream".to_string(),
            size_bytes: 0,
            uri: "memory://checkpoint".to_string(),
        };
        let task_outcome = TaskOutcome {
            lease: Some(lease.clone()),
            state: TaskOutcomeState::Suspended as i32,
            continuation: Some(checkpoint),
            ..Default::default()
        };

        let checkpoint_body = format!(
            r#"{{"schema_version":1,"run_id":"{run_id}","lifecycle_generation":2,"marker":"nested"}}"#
        );
        let checkpoint_bytes = checkpoint_body.into_bytes();
        let checkpoint_digest = format!(
            "{:x}",
            <sha2::Sha256 as sha2::Digest>::digest(&checkpoint_bytes)
        );
        store
            .put_run_checkpoint(run_id, 2, &checkpoint_digest, &checkpoint_bytes)
            .expect("checkpoint upload before pause");

        let outcome =
            apply_guest_task_outcome(&store, run_id, &lease, task_outcome).expect("apply");
        assert!(
            outcome.paused,
            "nested HITL must yield paused=true (finalize_run_session must be skipped)"
        );
        assert!(!outcome.succeeded);
        assert_eq!(store.get_run_state(run_id).expect("state"), "PAUSED");
        let warm_until = store
            .keep_warm_deadline(run_id)
            .expect("deadline")
            .expect("keep-warm armed when the run pauses");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        assert!(
            (now + crate::workflow::DEFAULT_KEEP_WARM_SECS - 5
                ..=now + crate::workflow::DEFAULT_KEEP_WARM_SECS + 5)
                .contains(&warm_until),
            "keep-warm default is 5 min, got deadline {warm_until} at {now}"
        );

        let session = store.load_session(session_id).expect("session");
        assert_eq!(session.observed_state, ObservedState::Running);
        let active = find_active_session_on_workspace(&store, workspace_id)
            .expect("active lookup")
            .expect("session must remain active while paused");
        assert_eq!(active.session_id, session_id);
    }

    #[test]
    fn two_sequential_hitl_approvals_keep_session_alive_until_final_success() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-hitl-seq";
        let session_id = "sess-hitl-seq";
        let workspace_id = "ws-hitl-seq";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_session(session_id, None, "vm-hitl-seq", workspace_id, "default")
            .expect("create session");
        store
            .update_observed_state(session_id, ObservedState::Booting)
            .expect("booting");
        store
            .update_observed_state(session_id, ObservedState::Running)
            .expect("running");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("create run");

        let first_lease = build_run_lease(run_id, 1);
        store.issue_lease(&first_lease).expect("lease 1");
        let first_key = crate::error::AttemptKey {
            run_id: run_id.to_string(),
            step_id: first_lease.step_id.clone(),
            attempt_id: first_lease
                .attempt_id
                .as_ref()
                .map(|a| a.urn.clone())
                .unwrap_or_default(),
        };
        store
            .register_hitl_suspend(
                &first_key,
                session_id,
                &ResourceId {
                    urn: "urn:vam:hitl:local.vam.dev/acme/seq-1".to_string(),
                },
                "slack_post_message",
                r#"{"channel":"ops"}"#,
                "digest-seq-1",
                None,
            )
            .expect("register first HITL");
        let first = apply_guest_task_outcome(
            &store,
            run_id,
            &first_lease,
            TaskOutcome {
                lease: Some(first_lease.clone()),
                state: TaskOutcomeState::Suspended as i32,
                continuation: Some(hitl_checkpoint_artifact("seq-1")),
                ..Default::default()
            },
        )
        .expect("first suspend");
        assert!(first.paused);
        assert!(!first.succeeded);
        assert_eq!(store.get_run_state(run_id).expect("state"), "PAUSED");
        assert_eq!(
            store
                .load_session(session_id)
                .expect("session")
                .observed_state,
            ObservedState::Running
        );

        let second_lease = build_run_lease(run_id, 2);
        store.issue_lease(&second_lease).expect("lease 2");
        let second_key = crate::error::AttemptKey {
            run_id: run_id.to_string(),
            step_id: second_lease.step_id.clone(),
            attempt_id: second_lease
                .attempt_id
                .as_ref()
                .map(|a| a.urn.clone())
                .unwrap_or_default(),
        };
        store
            .register_hitl_suspend(
                &second_key,
                session_id,
                &ResourceId {
                    urn: "urn:vam:hitl:local.vam.dev/acme/seq-2".to_string(),
                },
                "jira_transition_issue",
                r#"{"issue_key":"CGA-1"}"#,
                "digest-seq-2",
                None,
            )
            .expect("register second HITL");
        let second = apply_guest_task_outcome(
            &store,
            run_id,
            &second_lease,
            TaskOutcome {
                lease: Some(second_lease.clone()),
                state: TaskOutcomeState::Suspended as i32,
                continuation: Some(hitl_checkpoint_artifact("seq-2")),
                ..Default::default()
            },
        )
        .expect("second suspend");
        assert!(
            second.paused,
            "a second gated tool must pause again without tearing down"
        );
        assert!(!second.succeeded);
        assert_eq!(store.get_run_state(run_id).expect("state"), "PAUSED");
        let active = find_active_session_on_workspace(&store, workspace_id)
            .expect("active lookup")
            .expect("session must remain active across sequential HITL pauses");
        assert_eq!(active.session_id, session_id);

        let final_lease = build_run_lease(run_id, 3);
        let mut output = prost_types::Struct::default();
        output.fields.insert(
            "answer".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "all remaining steps done".to_string(),
                )),
            },
        );
        let terminal = apply_guest_task_outcome(
            &store,
            run_id,
            &final_lease,
            TaskOutcome {
                lease: Some(final_lease.clone()),
                state: TaskOutcomeState::Succeeded as i32,
                output: Some(output),
                ..Default::default()
            },
        )
        .expect("final success");
        assert!(terminal.succeeded);
        assert!(
            !terminal.paused,
            "only the terminal outcome finalizes the run"
        );
        assert_eq!(store.get_run_state(run_id).expect("state"), "SUCCEEDED");
        assert_eq!(
            store
                .load_session(session_id)
                .expect("session")
                .observed_state,
            ObservedState::Running,
            "session teardown is the caller's job after the terminal dispatch"
        );
    }

    /// After terminal HITL finalization marks the session Terminated/Failed, the
    /// workspace is free — find_active returns None (follow-up create_run is not 409).
    #[test]
    fn terminal_hitl_finalize_frees_workspace() {
        let session_id = "sess-hitl-done";
        let workspace_id = "ws-hitl-done";
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
            .create_session(session_id, None, "vm-hitl-done", workspace_id, "default")
            .expect("create session");
        store
            .update_observed_state(session_id, ObservedState::Booting)
            .expect("booting");
        store
            .update_observed_state(session_id, ObservedState::Running)
            .expect("running");

        assert!(
            find_active_session_on_workspace(&store, workspace_id)
                .expect("lookup")
                .is_some(),
            "HITL-kept-alive session occupies workspace before finalize"
        );

        // finalize_run_session → terminate_session marks Terminated; simulate that mark.
        store
            .update_observed_state(session_id, ObservedState::Terminated)
            .expect("terminated after HITL finalize");

        assert!(
            find_active_session_on_workspace(&store, workspace_id)
                .expect("lookup")
                .is_none(),
            "after terminal HITL finalize, workspace must be free for follow-up create_run"
        );
        let session = store.load_session(session_id).expect("session");
        assert_eq!(session.observed_state, ObservedState::Terminated);
    }

    #[test]
    fn prefixed_workspaces_do_not_share_the_active_session_lock() {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store.create_user("alice", "alice").expect("alice user");
        store.create_user("bob", "bob").expect("bob user");
        let alice_ws = crate::run_context::resolve_session_workspace_id("alice", "assistant")
            .expect("alice prefix");
        let bob_ws = crate::run_context::resolve_session_workspace_id("bob", "assistant")
            .expect("bob prefix");
        assert_ne!(alice_ws, bob_ws);

        store
            .create_session_with_owner(
                "sess-alice-ws",
                None,
                "vm-alice-ws",
                &alice_ws,
                "default",
                "alice",
            )
            .expect("alice session");
        store
            .update_observed_state("sess-alice-ws", ObservedState::Booting)
            .expect("alice booting");
        store
            .update_observed_state("sess-alice-ws", ObservedState::Running)
            .expect("alice running");
        store
            .create_session_with_owner("sess-bob-ws", None, "vm-bob-ws", &bob_ws, "default", "bob")
            .expect("bob session");
        store
            .update_observed_state("sess-bob-ws", ObservedState::Booting)
            .expect("bob booting");
        store
            .update_observed_state("sess-bob-ws", ObservedState::Running)
            .expect("bob running");

        let alice_active = find_active_session_on_workspace(&store, &alice_ws)
            .expect("alice lookup")
            .expect("alice occupies her volume");
        assert_eq!(alice_active.session_id, "sess-alice-ws");
        let bob_active = find_active_session_on_workspace(&store, &bob_ws)
            .expect("bob lookup")
            .expect("bob occupies his volume");
        assert_eq!(bob_active.session_id, "sess-bob-ws");
        assert!(
            find_active_session_on_workspace(&store, "assistant")
                .expect("unprefixed lookup")
                .is_none(),
            "unprefixed assistant must not match a host-prefixed volume"
        );
    }
}

fn now_ts() -> Timestamp {
    Timestamp {
        seconds: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
        nanos: 0,
    }
}

pub(crate) fn percent_decode_path(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[i + 1..i + 3]) {
                if let Ok(value) = u8::from_str_radix(hex, 16) {
                    out.push(value as char);
                    i += 3;
                    continue;
                }
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

pub(crate) fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{:x}", nanos ^ std::process::id() as u128)
}

pub fn bootstrap_api_key_if_empty(
    store: &SqliteWorkflowStore,
    bootstrap_secret: Option<String>,
) -> Result<Option<String>, OrchestratorError> {
    store.initialize_api_keys_schema()?;
    if store.api_key_count()? > 0 {
        return Ok(None);
    }
    let secret = bootstrap_secret.unwrap_or_else(mint_api_key_secret);
    let key_id = "bootstrap".to_string();
    let scopes: Vec<String> = BOOTSTRAP_SCOPES.iter().map(|s| s.to_string()).collect();
    store.create_api_key(
        &key_id,
        "bootstrap",
        &secret,
        "User::\"bootstrap\"",
        &scopes,
    )?;
    Ok(Some(secret))
}

#[cfg(test)]
mod playbook_step_status_tests {
    use super::*;
    use crate::workflow::{PlaybookRunPin, PlaybookStepCompletion};
    use vam_spec::v1::step_execution_status::StepState;
    use vam_spec::v1::{PlaybookSpec, PlaybookStep, StepKind};

    fn pin() -> PlaybookRunPin {
        PlaybookRunPin {
            urn: "urn:vam:playbook:local.vam.dev/acme/demo".to_string(),
            version: "1.0.0".to_string(),
            digest: "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string(),
        }
    }

    fn task(id: &str, after: &[&str]) -> PlaybookStep {
        PlaybookStep {
            id: id.to_string(),
            kind: StepKind::Task as i32,
            instruction: format!("do {id}"),
            after: after.iter().map(|token| (*token).to_string()).collect(),
            ..Default::default()
        }
    }

    fn status_app(store: Arc<SqliteWorkflowStore>) -> AppState {
        let (manager, _rx) = SessionManager::new(
            store.clone(),
            Arc::new(vam_core::CidAllocator::default()),
            Arc::new(|_vm_id, _cid| -> Box<dyn vam_core::Vmm> {
                unreachable!("playbook status test does not boot a VM")
            }),
        );
        AppState {
            store: store.clone(),
            vault: SecretsVault::for_tests(store),
            manager,
            pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
            listen_addr: "127.0.0.1:0".to_string(),
            draining: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            e2e_mock_scripts: false,
        }
    }

    #[test]
    fn run_status_surfaces_step_state_and_outcome() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        let run_id = "urn:vam:run:local.vam.dev/acme/status-run";
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("run");
        store
            .init_playbook_run(
                run_id,
                &pin(),
                &PlaybookSpec {
                    steps: vec![task("first", &[]), task("second", &["first"])],
                    ..Default::default()
                },
            )
            .expect("init");
        store
            .complete_playbook_step(
                run_id,
                "first",
                &PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{"n":1}"#.to_string()),
                    agent_summary: Some("first step done".to_string()),
                },
            )
            .expect("complete");

        let plain = "urn:vam:run:local.vam.dev/acme/plain-run";
        store
            .create_run(&ResourceId {
                urn: plain.to_string(),
            })
            .expect("plain run");

        let app = status_app(store);
        let status = build_run_status(&app, run_id).expect("status");
        assert_eq!(status.step_statuses.len(), 2);
        assert_eq!(status.step_statuses[0].step_id, "first");
        assert_eq!(status.step_statuses[0].state, StepState::Success as i32);
        assert_eq!(status.step_statuses[0].outcome, "done");
        assert_eq!(status.step_statuses[0].summary, "first step done");
        assert!(status.step_statuses[0].evidence_refs.is_empty());
        assert!(status.step_statuses[0].output.is_some());
        assert!(status.step_statuses[0].finished_at.is_some());
        assert_eq!(status.step_statuses[1].step_id, "second");
        assert_eq!(status.step_statuses[1].state, StepState::Available as i32);
        assert!(status.step_statuses[1].outcome.is_empty());
        assert!(status.step_statuses[1].summary.is_empty());

        let pending = map_playbook_step_record(&PlaybookStepRecord {
            step_id: "wait".to_string(),
            state: PlaybookStepState::Pending,
            outcome: String::new(),
            output_json: None,
            agent_summary: None,
            evidence_refs: Vec::new(),
            started_at: None,
            finished_at: None,
            transition_revision: 1,
        });
        assert_eq!(pending.state, StepState::Queued as i32);
        let open_gate = map_playbook_step_record(&PlaybookStepRecord {
            step_id: "approve".to_string(),
            state: PlaybookStepState::Open,
            outcome: String::new(),
            output_json: None,
            agent_summary: None,
            evidence_refs: Vec::new(),
            started_at: None,
            finished_at: None,
            transition_revision: 1,
        });
        assert_eq!(open_gate.state, StepState::Paused as i32);
        assert_ne!(open_gate.state, StepState::Available as i32);
        let skipped = map_playbook_step_record(&PlaybookStepRecord {
            step_id: "skip".to_string(),
            state: PlaybookStepState::Skipped,
            outcome: String::new(),
            output_json: None,
            agent_summary: None,
            evidence_refs: vec!["ev-1".to_string()],
            started_at: None,
            finished_at: Some(10),
            transition_revision: 2,
        });
        assert_eq!(skipped.state, StepState::Skipped as i32);
        assert_eq!(skipped.finished_at.expect("finished").seconds, 10);
        assert_eq!(skipped.evidence_refs, vec!["ev-1".to_string()]);

        let plain_status = build_run_status(&app, plain).expect("plain status");
        assert!(plain_status.step_statuses.is_empty());
    }

    #[test]
    fn run_status_exposes_resolved_workspace_id() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        store.initialize_catalog_schema().expect("schema");
        let run_id = "urn:vam:run:local.vam.dev/acme/ws-status";
        let workspace_id = "local--test-agent-149c38b2--main";
        let spec = vam_spec::v1::RunSpec {
            workspace_ref_uri: format!("volume://{workspace_id}"),
            thread_ref: "local--main".into(),
            ..Default::default()
        };
        let spec_json =
            vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("spec json");
        store
            .persist_run_spec_with_owner(
                run_id,
                &spec_json,
                None,
                "urn:vam:agent:local.vam.dev/acme/test-agent",
                None,
                "User::\"local\"",
                "local",
            )
            .expect("persist");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("run");
        let status = build_run_status(&status_app(store), run_id).expect("status");
        assert_eq!(status.workspace_id, workspace_id);
        assert_eq!(status.thread_ref, "local--main");
    }

    #[test]
    fn run_status_returns_stored_evidence_refs() {
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        let run_id = "urn:vam:run:local.vam.dev/acme/evidence-status";
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("run");
        let mut step = task("lookup-step", &[]);
        step.evidence = Some(vam_spec::v1::StepEvidence {
            tool: "lookup".to_string(),
            result: "success".to_string(),
        });
        store
            .init_playbook_run(
                run_id,
                &pin(),
                &PlaybookSpec {
                    steps: vec![step],
                    ..Default::default()
                },
            )
            .expect("init");
        crate::trajectory::emit_guest_tool_outcome(
            &store,
            run_id,
            &serde_json::json!({ "tool_name": "lookup", "exit_code": 0 }),
            false,
        );
        store
            .complete_playbook_step(
                run_id,
                "lookup-step",
                &PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: None,
                    agent_summary: None,
                },
            )
            .expect("complete");
        let status = build_run_status(&status_app(store), run_id).expect("status");
        assert_eq!(status.step_statuses.len(), 1);
        assert!(
            status.step_statuses[0]
                .evidence_refs
                .iter()
                .any(|reference| reference.starts_with("vam.v1.tool.completed:")),
            "{:?}",
            status.step_statuses[0].evidence_refs
        );
    }
}

#[cfg(test)]
mod thread_start_tests {
    use super::*;
    use vam_storage::thread_log::{
        kind, message_body, LogMessage, NewRecord, ThreadStore, TreeThreadStore, WireMessage,
    };

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-next";

    fn prepare(dir: &std::path::Path) -> ThreadStart {
        prepare_thread_context(
            dir,
            "ws-1",
            "main",
            RUN,
            "urn:vam:agent:t",
            "digest",
            "{}",
            "What does the note say?",
        )
        .expect("prepare")
    }

    fn user_record(run: &str, text: &str) -> NewRecord {
        let m = LogMessage::from_wire(&WireMessage {
            role: "user".into(),
            content: text.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        });
        let mut r = NewRecord::new(kind::MESSAGE, "broker", message_body(&m, None));
        r.run = run.into();
        r
    }

    fn string_field(inputs: &prost_types::Struct, key: &str) -> Option<String> {
        match inputs.fields.get(key)?.kind.as_ref()? {
            prost_types::value::Kind::StringValue(s) => Some(s.clone()),
            _ => None,
        }
    }

    #[test]
    fn a_thread_with_no_history_starts_fresh() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(prepare(dir.path()), ThreadStart::Fresh));
    }

    #[test]
    fn a_timeline_only_thread_replays_once_and_names_its_runs() {
        let dir = tempfile::tempdir().unwrap();
        vam_storage::append_turn(
            dir.path(),
            "main",
            "run-old",
            "p",
            "a",
            "m",
            "mh",
            None,
            None,
        )
        .unwrap();
        match prepare(dir.path()) {
            ThreadStart::Legacy { prompt, runs } => {
                assert!(prompt.contains("Previous conversation"));
                assert!(prompt.ends_with("User: What does the note say?"));
                assert_eq!(runs, ["run-old"]);
            }
            _ => panic!("expected the legacy replay"),
        }
    }

    #[test]
    fn a_thread_with_a_log_starts_from_the_granted_context_and_inherits_urls() {
        let dir = tempfile::tempdir().unwrap();
        TreeThreadStore::new(dir.path())
            .append("main", vec![user_record("run-old", "e2e-step:first Save")])
            .unwrap();
        // A timeline exists too: the log wins and nothing is flattened.
        vam_storage::append_turn(
            dir.path(),
            "main",
            "run-old",
            "p",
            "a",
            "m",
            "mh",
            None,
            None,
        )
        .unwrap();
        let start = prepare(dir.path());
        assert!(matches!(start, ThreadStart::Log(_)));

        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .persist_run_spec(
                RUN,
                "{}",
                Some("sess-next"),
                "urn:vam:agent:t",
                None,
                "User::\"t\"",
            )
            .unwrap();
        store
            .add_context_urls(
                "run-old",
                &["https://seen.example/a"],
                crate::web_sources::ContextOrigin::ToolResult,
            )
            .unwrap();
        let root = tempfile::tempdir().unwrap();
        let layout = vam_storage::Drive9Layout::new(root.path());
        // A caller cannot name its own context.
        let mut inputs = Some(prost_types::Struct {
            fields: [(
                "thread_head_digest".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue("forged".into())),
                },
            )]
            .into_iter()
            .collect(),
        });
        let prompt = apply_thread_start(
            &store,
            &layout,
            RUN,
            "sess-next",
            "What does the note say?",
            start,
            &mut inputs,
        );
        assert_eq!(
            prompt, "What does the note say?",
            "the raw prompt, not a replay"
        );
        let inputs = inputs.unwrap();
        let digest = string_field(&inputs, "thread_head_digest").expect("digest input");
        assert_ne!(digest, "forged");
        assert_eq!(
            string_field(&inputs, "thread_head_seq").as_deref(),
            Some("1")
        );
        assert!(store.run_context_granted(RUN, &digest));
        assert!(!store.run_context_granted("urn:vam:run:other", &digest));
        let path = crate::thread_context::context_path(&layout, "sess-next", &digest).unwrap();
        assert_eq!(sha2_hex(&std::fs::read(path).unwrap()), digest);
        assert!(store.run_context_contains(RUN, "https://seen.example/a"));
    }

    fn sha2_hex(bytes: &[u8]) -> String {
        vam_spec::thread_context::sha256_hex(bytes)
    }

    #[test]
    fn a_log_that_cannot_be_read_fails_the_prepare() {
        let dir = tempfile::tempdir().unwrap();
        let seg = dir.path().join(".vam/threads/main/log");
        std::fs::create_dir_all(&seg).unwrap();
        std::fs::write(
            seg.join("00000001.jsonl"),
            r#"{"v":2,"seq":1}
"#,
        )
        .unwrap();
        let result = prepare_thread_context(
            dir.path(),
            "ws-1",
            "main",
            RUN,
            "urn:vam:agent:t",
            "digest",
            "{}",
            "next",
        );
        assert!(result.is_err());
    }

    fn suspended_session(root: &std::path::Path) -> vam_storage::Drive9Layout {
        let layout = vam_storage::Drive9Layout::new(root);
        let tree = layout.session_dir("sess-next").join("materialized");
        let assistant = LogMessage::from_wire(&WireMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![("c1".into(), "write_file".into(), "{}".into())],
            tool_call_id: String::new(),
        });
        let mut call = NewRecord::new(kind::MESSAGE, "broker", message_body(&assistant, None));
        call.run = RUN.into();
        let mut suspend = NewRecord::new(
            kind::SUSPEND,
            "host",
            serde_json::json!({
                "pending": {"tool_call_id": "c1", "tool_name": "write_file", "args_json": "{}"},
                "unanswered": [],
                "lifecycle_generation": 2,
                "state": {"iterations_used": 1},
            }),
        );
        suspend.run = RUN.into();
        std::fs::create_dir_all(&tree).unwrap();
        TreeThreadStore::new(&tree)
            .append("main", vec![user_record(RUN, "go"), call, suspend])
            .unwrap();
        layout
    }

    fn resume_inputs(legacy_generation: Option<&str>) -> prost_types::Struct {
        let string = |s: &str| prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(s.to_string())),
        };
        let mut inputs = prost_types::Struct::default();
        inputs
            .fields
            .insert("approved_tool".into(), string("write_file"));
        if let Some(g) = legacy_generation {
            inputs
                .fields
                .insert("hitl_continuation_digest".into(), string(&"9".repeat(64)));
            inputs
                .fields
                .insert("hitl_continuation_generation".into(), string(g));
        }
        inputs
    }

    #[test]
    fn a_resume_after_the_switch_gets_the_logged_context_with_the_suspend() {
        let root = tempfile::tempdir().unwrap();
        let layout = suspended_session(root.path());
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .persist_run_spec(
                RUN,
                "{}",
                Some("sess-next"),
                "urn:vam:agent:t",
                None,
                "User::\"t\"",
            )
            .unwrap();
        let mut inputs = resume_inputs(None);
        attach_log_resume_context(&store, &layout, "sess-next", RUN, "main", &mut inputs);
        let digest = string_field(&inputs, "thread_head_digest").expect("digest input");
        assert!(store.run_context_granted(RUN, &digest));
        let path = crate::thread_context::context_path(&layout, "sess-next", &digest).unwrap();
        let ctx = vam_spec::thread_context::ThreadContext::from_verified_bytes(
            &std::fs::read(path).unwrap(),
            &digest,
        )
        .unwrap();
        let suspend = ctx.suspend.expect("the suspend rides in the context");
        assert_eq!(suspend.pending.tool_call_id, "c1");
        assert_eq!(suspend.lifecycle_generation, 2);
        assert_eq!(ctx.messages.len(), 2, "history up to the suspended call");
    }

    #[test]
    fn a_run_parked_before_the_switch_keeps_its_legacy_checkpoint_until_a_newer_suspend() {
        let root = tempfile::tempdir().unwrap();
        let layout = suspended_session(root.path());
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .persist_run_spec(
                RUN,
                "{}",
                Some("sess-next"),
                "urn:vam:agent:t",
                None,
                "User::\"t\"",
            )
            .unwrap();
        // Legacy generation 5 is newer than the log's suspend (2): unchanged.
        let mut legacy = resume_inputs(Some("5"));
        attach_log_resume_context(&store, &layout, "sess-next", RUN, "main", &mut legacy);
        assert!(string_field(&legacy, "thread_head_digest").is_none());
        assert!(string_field(&legacy, "hitl_continuation_digest").is_some());
        // Legacy generation 1 is older: the log wins and the legacy digest is dropped.
        let mut older = resume_inputs(Some("1"));
        attach_log_resume_context(&store, &layout, "sess-next", RUN, "main", &mut older);
        assert!(string_field(&older, "thread_head_digest").is_some());
        assert!(string_field(&older, "hitl_continuation_digest").is_none());
        // A gate decision (no tool) is left alone.
        let mut gate = prost_types::Struct::default();
        attach_log_resume_context(&store, &layout, "sess-next", RUN, "main", &mut gate);
        assert!(gate.fields.is_empty());
    }

    /// The receipt the finalizer issues is the complete one: it carries the HEAD
    /// after the run and the trajectory digest the attestation names.
    #[tokio::test]
    async fn finalize_issues_a_complete_receipt_after_the_attestation() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("VAM_DRIVE9_ROOT", dir.path().join("drive9"));
        let _ = crate::signing::install(crate::signing::NodeKeys::ephemeral());
        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        store.initialize_catalog_schema().expect("catalog");
        let (run_id, session_id) = ("urn:vam:run:local.vam.dev/acme/fin", "sess-fin");
        store
            .create_session(session_id, None, "vm-fin", "ws-fin", "default")
            .unwrap();
        store
            .persist_run_spec(
                run_id,
                "{}",
                Some(session_id),
                "urn:vam:agent:local.vam.dev/acme/a",
                None,
                "User::\"tester\"",
            )
            .unwrap();
        store
            .record_run_head_before(run_id, session_id, "head-0", "fh0")
            .unwrap();
        store
            .record_run_head_after(run_id, "head-1", "fh1")
            .unwrap();
        store
            .append_outbox_event(run_id, "vam.v1.tool.invoked", r#"{"tool_name":"x"}"#)
            .unwrap();
        store.session_ends.track(session_id);
        store
            .complete_run_with_result(run_id, "SUCCEEDED", r#"{"answer":"done"}"#)
            .unwrap();

        let (manager, mut rx) = SessionManager::new(
            store.clone(),
            Arc::new(vam_core::CidAllocator::default()),
            Arc::new(|_, _| Box::new(vam_core::MockVmm::new())),
        );
        let loop_handle = tokio::spawn(async move {
            while let Some(command) = rx.recv().await {
                if let crate::manager::ManagerCommand::TerminateSession { reply, .. } = command {
                    let _ = reply.send(Ok(()));
                }
            }
        });
        // Nothing issues before the finalizer: the run is already terminal here.
        assert!(store.run_receipt(run_id).unwrap().is_none());
        let outcome = DispatchOutcome {
            answer: "done".into(),
            succeeded: true,
            paused: false,
            interrupted: false,
            spend_micro: None,
        };
        finalize_run_session(
            &manager,
            &store,
            run_id,
            session_id,
            "ws-fin",
            "main",
            "goal",
            "mock-model",
            &outcome,
        )
        .await;
        loop_handle.abort();

        let stored = store
            .run_receipt(run_id)
            .unwrap()
            .expect("receipt issued by finalize");
        let doc: serde_json::Value = serde_json::from_str(&stored.document).unwrap();
        let attested = store
            .read_outbox_for_run(run_id, 0)
            .unwrap()
            .into_iter()
            .find(|e| e.event_type == "vam.v1.run.attested")
            .expect("attestation emitted");
        let _ = attested;
        let events = store.read_outbox_for_run(run_id, 0).unwrap();
        let layout = vam_storage::Drive9Layout::new(drive9_root());
        let replay = crate::trajectory::persist_trajectory_replay_blob(
            &events,
            &vam_storage::ChunkStore::new(layout),
        )
        .unwrap();
        assert_eq!(doc["receipt"]["trajectoryDigest"], replay);
        assert_eq!(doc["receipt"]["workspace"]["headBefore"], "head-0");
        assert_eq!(doc["receipt"]["workspace"]["headAfter"], "head-1");
        assert_eq!(doc["modelAppendix"]["finalReply"], "done");
    }
}

#[cfg(test)]
mod approval_card_tests {
    use super::*;

    fn stored(card_json: Option<String>) -> crate::error::StoredApproval {
        crate::error::StoredApproval {
            request_id: "urn:vam:hitl:local.vam.dev/acme/a1".into(),
            run_id: "urn:vam:run:local.vam.dev/acme/r1".into(),
            step_id: "s".into(),
            attempt_id: "a".into(),
            session_id: "sess".into(),
            tool_name: "mail_send".into(),
            approval_message: String::new(),
            requested_args_json: "{}".into(),
            amendable_schema_json: "{}".into(),
            args_digest: "d".into(),
            required_roles_json: "[]".into(),
            checkpoint_digest: "c".into(),
            continuation_generation: 0,
            created_at: 0,
            expires_at: None,
            decision: None,
            decision_revision: 0,
            reviewer_principal: None,
            amended_args_json: None,
            amended_args_digest: None,
            rejection_reason: None,
            published: true,
            approver_user_id: "local".into(),
            kind: "tool_call".into(),
            target: String::new(),
            always_offered: false,
            locked_reason: String::new(),
            card_json,
        }
    }

    #[test]
    fn the_card_is_served_bound_from_the_requested_args_and_cannot_preset_decision() {
        let ir = serde_json::json!({
            "state": {"subject": "", "decision": ""},
            "nodes": {"ok": {"type": "Button", "props": {"action": {
                "event": "submit", "set": {"decision": "approve"}, "with": ["subject"]}}}}
        });
        // The requested args try to preset `decision`.
        let args = serde_json::json!({"subject": "Draft subject", "decision": "approve"});
        let card = crate::hitl::AskApprovalCard {
            jig: "mail.send".into(),
            skill: "urn:vam:skill:local.vam.dev/acme/mail".into(),
            ir: vam_spec::jig::bind_args(&ir, &args),
            data: args,
            submit: vec!["subject".into()],
            input_schema: serde_json::json!({"type": "object"}),
        };
        let served = stored_to_approval_request(&stored(Some(card.stored_json().to_string())));
        let card = served.claims.unwrap().card.expect("card served");
        assert_eq!(card.jig, "mail.send");
        let ir =
            vam_spec::encoding::proto_to_json("google.protobuf.Struct", &card.ir.unwrap()).unwrap();
        let ir: serde_json::Value = serde_json::from_str(&ir).unwrap();
        assert_eq!(ir["state"]["subject"], "Draft subject");
        assert_eq!(ir["state"]["decision"], "");
        let data = vam_spec::encoding::proto_to_json("google.protobuf.Struct", &card.data.unwrap())
            .unwrap();
        assert!(data.contains("Draft subject"));
    }

    #[test]
    fn an_approval_without_a_stored_card_serves_none() {
        assert!(stored_to_approval_request(&stored(None))
            .claims
            .unwrap()
            .card
            .is_none());
    }
}
