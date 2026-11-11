//! Org-admin grant/revoke of the catalog `publish:grant` right.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::{json_response, ApiError};
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::error::OrchestratorError;
use crate::keys::USERS_ADMIN_SCOPE;
use crate::users::UserRecord;
use vam_spec::v1::{GrantPublishRightResponse, RevokePublishRightResponse};

pub fn publish_grant_route() -> axum::routing::MethodRouter<AppState> {
    axum::routing::post(grant_publish_right).delete(revoke_publish_right)
}

fn grant_response(user: UserRecord) -> GrantPublishRightResponse {
    GrantPublishRightResponse {
        user_id: user.user_id,
        role: user.role.as_str().to_string(),
        granted: user.role.has_publish_grant(),
    }
}

fn revoke_response(user: UserRecord) -> RevokePublishRightResponse {
    RevokePublishRightResponse {
        user_id: user.user_id,
        role: user.role.as_str().to_string(),
        granted: user.role.has_publish_grant(),
    }
}

async fn grant_publish_right(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "GrantPublish", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    if user_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("user_id is required".into()).into());
    }
    let user = state.store.grant_publish_right(&user_id)?;
    let resp = grant_response(user);
    let (code, json) = json_response("vam.v1.GrantPublishRightResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn revoke_publish_right(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "RevokePublish", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    if user_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("user_id is required".into()).into());
    }
    let user = state.store.revoke_publish_right(&user_id)?;
    let resp = revoke_response(user);
    let (code, json) = json_response("vam.v1.RevokePublishRightResponse", &resp)?;
    Ok(json_http_response(code, json))
}
