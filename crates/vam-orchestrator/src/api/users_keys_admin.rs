//! Org-admin list, mint, and revoke API keys for a given control-plane user.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{delete, get};

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::{ensure_ready, json_http_response, to_proto_key, uuid_like, AppState};
use crate::error::OrchestratorError;
use crate::keys::{mint_api_key_secret, USERS_ADMIN_SCOPE};
use crate::users::{reject_deactivated_user, reject_operator_user_admin_target};
use vam_spec::v1::{
    AdminMintUserApiKeyRequest, CreateApiKeyResponse, ListApiKeysResponse, RevokeApiKeyResponse,
};

pub fn user_keys_collection_route() -> axum::routing::MethodRouter<AppState> {
    get(list_user_keys).post(mint_user_key)
}

pub fn revoke_user_key_route() -> axum::routing::MethodRouter<AppState> {
    delete(revoke_user_key)
}

fn resolve_target_user(
    state: &AppState,
    user_id: &str,
) -> Result<crate::users::UserRecord, OrchestratorError> {
    if user_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "user_id is required".into(),
        ));
    }
    // Names match ignoring letter case, so the operator guard runs on the resolved row: the raw
    // `OPERATOR` reaches the operator only after resolution.
    let user = state.store.resolve_user(user_id)?;
    reject_operator_user_admin_target(&user.user_id)?;
    reject_deactivated_user(&user)?;
    Ok(user)
}

async fn list_user_keys(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(user_id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state
        .pdp
        .authorize(&auth, "ListUserApiKeys", USERS_ADMIN_SCOPE)?;
    if user_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("user_id is required".into()).into());
    }
    let user = state.store.resolve_user(&user_id)?;
    reject_operator_user_admin_target(&user.user_id)?;
    let keys = state.store.list_api_keys_for_owner(&user.user_id)?;
    let resp = ListApiKeysResponse {
        keys: keys.iter().map(to_proto_key).collect(),
        pagination: None,
    };
    let (code, json) = json_response("vam.v1.ListApiKeysResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn mint_user_key(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "AdminMintUserApiKey", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let user = resolve_target_user(&state, &user_id)?;
    let req: AdminMintUserApiKeyRequest =
        parse_json_body("vam.v1.AdminMintUserApiKeyRequest", &body)?;
    if req.name.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("name is required".into()).into());
    }
    let key_id = format!("key-{}", uuid_like());
    let secret = mint_api_key_secret();
    let scopes = crate::keys::sanitize_scopes_for_role(user.role, &req.scopes);
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
        &user.user_id,
    )?;
    let resp = CreateApiKeyResponse {
        key: Some(to_proto_key(&record)),
        secret,
    };
    let (code, json) = json_response("vam.v1.CreateApiKeyResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn revoke_user_key(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((user_id, key_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "AdminRevokeUserApiKey", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let user = resolve_target_user(&state, &user_id)?;
    if key_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("key_id is required".into()).into());
    }
    // An admin reaches a user's `http` keys only; a grant is revoked by its owner.
    if !state
        .store
        .get_api_key_for_owner(&key_id, &user.user_id)?
        .is_http()
    {
        return Err(OrchestratorError::NotFound(format!("API key '{key_id}' not found")).into());
    }
    let record = state
        .store
        .revoke_api_key_for_owner(&key_id, &user.user_id)?;
    let resp = RevokeApiKeyResponse {
        key: Some(to_proto_key(&record)),
    };
    let (code, json) = json_response("vam.v1.RevokeApiKeyResponse", &resp)?;
    Ok(json_http_response(code, json))
}
