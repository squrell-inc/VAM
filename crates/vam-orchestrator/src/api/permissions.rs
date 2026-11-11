//! Permission HTTP routes.
//!
//! Paths (later issues use these verbatim):
//! - `GET /v1/agents/*urn/permissions` — the owner's draft permissions (AX-5.6a).
//! - `PUT /v1/agents/*urn/permissions` — replace that draft (AX-5.6a).
//!   `catalog:write` and `Idempotency-Key`, same as `PUT /v1/agents`.
//! - `GET /v1/agents/*urn/permissions/effective` — effective permissions of the draft,
//!   or of `?version=` when that query is set (AX-5.8a).
//! - `GET /v1/agents/*urn/permissions/explain?tool=` — one guest tool name or target.
//!   `GET /v1/agents/*urn/permissions/explain?host=` — one host (AX-5.8a).
//! - `GET /v1/permission-presets` — copied starting rule sets (AX-5.8a).
//!
//! Reads use the same catalog-read scope as `GET /v1/agents`. A foreign draft is
//! 404, including for `users:admin` on PUT (owner only). GET follows other draft
//! reads: the owner, or `users:admin`, sees the draft.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::catalog_auth::require_catalog_read;
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::{ensure_ready, AppState};
use crate::error::OrchestratorError;
use vam_spec::v1::VamErrorCode;

/// Query string for effective (`version`) and explain (`tool` or `host`).
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub(crate) struct PermissionsQuery {
    pub version: Option<String>,
    /// pass^k size for `/outcomes`.
    pub k: Option<String>,
    pub tool: Option<String>,
    pub host: Option<String>,
}

/// Which `GET` under `/v1/agents/*urn/permissions` this request is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AgentPermissionsGet {
    Draft,
    Effective,
    Explain,
}

impl AgentPermissionsGet {
    /// `rest` is the axum `*rest` capture (`urn:vam:agent:.../name/permissions/...`).
    pub(crate) fn match_rest(rest: &str) -> Option<Self> {
        let rest = rest.trim_start_matches('/');
        if rest.ends_with("/permissions/effective") {
            Some(Self::Effective)
        } else if rest.ends_with("/permissions/explain") {
            Some(Self::Explain)
        } else if rest.ends_with("/permissions") {
            Some(Self::Draft)
        } else {
            None
        }
    }
}

pub(crate) fn is_draft_permissions_path(rest: &str) -> bool {
    AgentPermissionsGet::match_rest(rest) == Some(AgentPermissionsGet::Draft)
}

pub(crate) async fn get_agent_permissions(
    state: AppState,
    auth: AuthContext,
    kind: AgentPermissionsGet,
    rest: &str,
    query: PermissionsQuery,
) -> Result<Response, ApiError> {
    match kind {
        AgentPermissionsGet::Draft => get_draft_permissions(state, auth, rest).await,
        AgentPermissionsGet::Effective => effective_permissions(state, auth, rest, query).await,
        AgentPermissionsGet::Explain => explain_permissions(state, auth, rest, query).await,
    }
}

async fn effective_permissions(
    state: AppState,
    auth: AuthContext,
    rest: &str,
    query: PermissionsQuery,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let urn = agent_urn_from_permissions_rest(rest)?;
    let view = crate::permissions_view::effective_permissions(
        &state.store,
        auth.owner_user_id(),
        auth.has_users_admin(),
        &urn,
        query.version.as_deref(),
    )?;
    json_body(&view)
}

async fn explain_permissions(
    state: AppState,
    auth: AuthContext,
    rest: &str,
    query: PermissionsQuery,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let urn = agent_urn_from_permissions_rest(rest)?;
    let view = crate::permissions_view::explain_permissions(
        &state.store,
        auth.owner_user_id(),
        auth.has_users_admin(),
        &urn,
        query.version.as_deref(),
        query.tool.as_deref(),
        query.host.as_deref(),
    )?;
    json_body(&view)
}

fn json_body(body: &impl serde::Serialize) -> Result<Response, ApiError> {
    let json = serde_json::to_string(body)
        .map_err(|err| OrchestratorError::CorruptState(err.to_string()))?;
    Ok(super::json_http_response(StatusCode::OK, json))
}

async fn get_draft_permissions(
    state: AppState,
    auth: AuthContext,
    rest: &str,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let urn = agent_urn_from_permissions_path(rest)?;
    let spec = crate::catalog_publish::load_agent_draft(
        &state.store,
        &urn,
        auth.owner_user_id(),
        auth.has_users_admin(),
    )?;
    let permissions = spec.permissions.unwrap_or_default();
    let (code, json) = json_response("vam.v1.AgentPermissions", &permissions)?;
    Ok(super::json_http_response(code, json))
}

pub(crate) async fn put_draft_permissions(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    if !is_draft_permissions_path(&rest) {
        return Ok(StatusCode::METHOD_NOT_ALLOWED.into_response());
    }
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let urn = agent_urn_from_permissions_path(&rest)?;
    // A non-owner gets the same 404 as a missing draft, before the body is read.
    crate::catalog_publish::load_agent_draft(&state.store, &urn, auth.owner_user_id(), false)?;
    let incoming: vam_spec::v1::AgentPermissions =
        parse_json_body("vam.v1.AgentPermissions", &body)?;
    let saved = crate::catalog_publish::update_draft_permissions(
        &state.store,
        &urn,
        auth.owner_user_id(),
        move |permissions| {
            *permissions = incoming;
        },
    )?;
    let permissions = saved.permissions.unwrap_or_default();
    let (code, json) = json_response("vam.v1.AgentPermissions", &permissions)?;
    Ok(super::json_http_response(code, json))
}

fn agent_urn_from_permissions_path(rest: &str) -> Result<String, ApiError> {
    agent_urn_from_permissions_rest(rest)
}

fn agent_urn_from_permissions_rest(rest: &str) -> Result<String, ApiError> {
    let rest = rest.trim_start_matches('/');
    let raw = rest
        .strip_suffix("/permissions/effective")
        .or_else(|| rest.strip_suffix("/permissions/explain"))
        .or_else(|| rest.strip_suffix("/permissions"))
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: VamErrorCode::NotFound,
            message: format!("no agent permissions at '{rest}'"),
            retryable: false,
        })?;
    let urn = super::percent_decode_path(raw);
    if urn.is_empty() {
        return Err(OrchestratorError::InvalidContract("agent URN is required".into()).into());
    }
    Ok(urn)
}

pub(crate) async fn list_permission_presets(
    State(_state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    json_body(&crate::permissions_view::permission_presets()?)
}

#[allow(dead_code)] // retained for 501 stubs on routes not yet implemented
pub(crate) fn not_implemented(what: &str) -> Result<Response, ApiError> {
    Err(ApiError {
        status: StatusCode::NOT_IMPLEMENTED,
        code: VamErrorCode::Unimplemented,
        message: format!("{what} is not implemented"),
        retryable: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permissions_get_suffixes() {
        let urn = "urn:vam:agent:local.vam.dev/acme/jira";
        assert_eq!(
            AgentPermissionsGet::match_rest(&format!("{urn}/permissions")),
            Some(AgentPermissionsGet::Draft)
        );
        assert_eq!(
            AgentPermissionsGet::match_rest(&format!("{urn}/permissions/effective")),
            Some(AgentPermissionsGet::Effective)
        );
        assert_eq!(
            AgentPermissionsGet::match_rest(&format!("{urn}/permissions/explain")),
            Some(AgentPermissionsGet::Explain)
        );
        assert_eq!(
            AgentPermissionsGet::match_rest(&format!("{urn}/versions")),
            None
        );
        assert_eq!(
            AgentPermissionsGet::match_rest(&format!("{urn}/permissions/migrate")),
            None
        );
        assert!(is_draft_permissions_path(&format!("{urn}/permissions")));
        assert!(!is_draft_permissions_path(&format!(
            "{urn}/permissions/effective"
        )));
    }

    #[tokio::test]
    async fn draft_permission_routes_404_without_a_draft_and_presets_are_served() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        use axum::body::Body;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt;

        use crate::api::auth::ControlPlanePdp;
        use crate::api::{bootstrap_api_key_if_empty, router, AppState};
        use crate::manager::SessionManager;
        use crate::secrets::SecretsVault;
        use crate::workflow::SqliteWorkflowStore;

        let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
        let secret = bootstrap_api_key_if_empty(&store, Some("test-secret".into()))
            .expect("bootstrap")
            .expect("secret");
        let (manager, _rx) = SessionManager::new(
            store.clone(),
            Arc::new(vam_core::CidAllocator::default()),
            Arc::new(|_vm_id, _cid| -> Box<dyn vam_core::Vmm> {
                unreachable!("permission scaffold test does not boot a VM")
            }),
        );
        let state = AppState {
            store: store.clone(),
            vault: SecretsVault::for_tests(store),
            manager,
            pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
            listen_addr: "127.0.0.1:0".to_string(),
            draining: Arc::new(AtomicBool::new(false)),
            ready: Arc::new(AtomicBool::new(true)),
            e2e_mock_scripts: false,
        };
        let urn = "urn:vam:agent:local.vam.dev/acme/jira";
        let routes = [
            (
                "GET",
                format!("/v1/agents/{urn}/permissions"),
                false,
                StatusCode::NOT_FOUND,
            ),
            (
                "PUT",
                format!("/v1/agents/{urn}/permissions"),
                true,
                StatusCode::NOT_FOUND,
            ),
            (
                "POST",
                format!("/v1/agents/{urn}/permissions/migrate"),
                true,
                // Implemented by AX-5.6b: no draft of this agent exists here.
                StatusCode::NOT_FOUND,
            ),
            (
                "GET",
                format!("/v1/agents/{urn}/permissions/effective?version=1.2.3"),
                false,
                StatusCode::NOT_FOUND,
            ),
            (
                "GET",
                format!("/v1/agents/{urn}/permissions/explain?tool=jira_create_issue"),
                false,
                StatusCode::NOT_FOUND,
            ),
            (
                "GET",
                format!("/v1/agents/{urn}/permissions/explain?host=pastebin.com"),
                false,
                StatusCode::NOT_FOUND,
            ),
            (
                "GET",
                "/v1/permission-presets".to_string(),
                false,
                StatusCode::OK,
            ),
        ];
        for (method, uri, idempotent, expect) in routes {
            let response = router(state.clone())
                .oneshot(authed(method, &uri, &secret, idempotent))
                .await
                .expect("response");
            assert_eq!(response.status(), expect, "{method} {uri}");
        }

        let missing = router(state.clone())
            .oneshot(authed("GET", &format!("/v1/agents/{urn}"), &secret, false))
            .await
            .expect("response");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);

        let open = router(state)
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri("/v1/permission-presets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .expect("response");
        assert_eq!(open.status(), StatusCode::UNAUTHORIZED);
    }

    fn authed(
        method: &str,
        uri: &str,
        secret: &str,
        idempotent: bool,
    ) -> axum::http::Request<axum::body::Body> {
        use axum::body::Body;
        use axum::http::Request;

        let mut builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {secret}"));
        if idempotent {
            builder = builder.header("idempotency-key", "perm-1");
        }
        builder.body(Body::empty()).unwrap()
    }
}
