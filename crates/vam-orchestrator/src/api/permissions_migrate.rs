//! `POST /v1/agents/*urn/permissions/migrate` — convert a legacy draft to `permissions`.
//!
//! `catalog:write` and `Idempotency-Key`, same as `PUT /v1/agents`. `?force=true`
//! (or `1`) drops guest tool names that map to no target; without it those names
//! are a 409 and the draft is not written.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::ApiError;
use crate::api::{ensure_ready, json_http_response, percent_decode_path, AppState};
use crate::error::OrchestratorError;
use vam_spec::v1::VamErrorCode;

pub(crate) fn is_migrate_path(rest: &str) -> bool {
    rest.trim_start_matches('/')
        .ends_with("/permissions/migrate")
}

pub(crate) fn force_requested(force: Option<&str>) -> bool {
    matches!(force, Some("true") | Some("1"))
}

pub(crate) async fn migrate_legacy_draft(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    force: bool,
    _body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let urn = agent_urn_from_migrate_path(&rest)?;
    let report = crate::permissions_migrate::migrate_legacy_draft(
        &state.store,
        &state.vault,
        &urn,
        auth.owner_user_id(),
        force,
    )?;
    let body = serde_json::to_string(&report)
        .map_err(|err| OrchestratorError::Core(format!("migration report: {err}")))?;
    Ok(json_http_response(StatusCode::OK, body))
}

fn agent_urn_from_migrate_path(rest: &str) -> Result<String, ApiError> {
    let rest = rest.trim_start_matches('/');
    let raw = rest
        .strip_suffix("/permissions/migrate")
        .ok_or_else(|| ApiError {
            status: StatusCode::NOT_FOUND,
            code: VamErrorCode::NotFound,
            message: format!("no draft of agent '{rest}' for this user"),
            retryable: false,
        })?;
    let urn = percent_decode_path(raw);
    if urn.is_empty() {
        return Err(OrchestratorError::InvalidContract("agent URN is required".into()).into());
    }
    Ok(urn)
}
