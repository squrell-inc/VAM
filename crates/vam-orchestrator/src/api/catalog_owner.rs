//! Org-admin reassignment of catalog artifact ownership.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::error::OrchestratorError;
use crate::keys::USERS_ADMIN_SCOPE;
use crate::owner::CatalogOwnerReassignment;
use vam_spec::v1::{ReassignCatalogOwnerRequest, ReassignCatalogOwnerResponse};

pub fn catalog_owner_route() -> axum::routing::MethodRouter<AppState> {
    axum::routing::post(reassign_catalog_owner)
}

fn reassignment_response(result: CatalogOwnerReassignment) -> ReassignCatalogOwnerResponse {
    ReassignCatalogOwnerResponse {
        kind: result.kind,
        urn: result.urn,
        previous_owner_user_id: result.previous_owner_user_id,
        owner_user_id: result.owner_user_id,
        rows_updated: result.rows_updated,
    }
}

async fn reassign_catalog_owner(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "ReassignCatalogOwner", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: ReassignCatalogOwnerRequest =
        parse_json_body("vam.v1.ReassignCatalogOwnerRequest", &body)?;
    if req.kind.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("kind is required".into()).into());
    }
    if req.urn.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("urn is required".into()).into());
    }
    if req.owner_user_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("owner_user_id is required".into()).into());
    }
    let result = state
        .store
        .reassign_catalog_owner(&req.kind, &req.urn, &req.owner_user_id)?;
    let resp = reassignment_response(result);
    let (code, json) = json_response("vam.v1.ReassignCatalogOwnerResponse", &resp)?;
    Ok(json_http_response(code, json))
}
