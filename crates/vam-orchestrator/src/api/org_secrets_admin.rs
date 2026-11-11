//! Org-admin write, list, and delete of org-scope host secrets.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{delete, get};

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::{
    ensure_ready, json_http_response, percent_decode_path, to_proto_secret, AppState,
};
use crate::error::OrchestratorError;
use crate::keys::USERS_ADMIN_SCOPE;
use crate::secrets::{authorize_org_secret_write, SecretKind, SecretScope, ORG_OWNER_USER_ID};
use vam_spec::v1::{
    DeleteHostSecretResponse, ListHostSecretsResponse, PutHostSecretRequest, PutHostSecretResponse,
};

pub fn org_secrets_collection_route() -> axum::routing::MethodRouter<AppState> {
    get(list_org_secrets).put(put_org_secret)
}

pub fn delete_org_secret_route() -> axum::routing::MethodRouter<AppState> {
    delete(delete_org_secret)
}

async fn put_org_secret(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutOrgHostSecret", USERS_ADMIN_SCOPE)?;
    authorize_org_secret_write(SecretScope::Org, auth.role())?;
    let idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: PutHostSecretRequest = parse_json_body("vam.v1.PutHostSecretRequest", &body)?;
    if req.binding_id.is_empty() {
        return Err(OrchestratorError::InvalidContract("binding_id is required".into()).into());
    }
    let kind = SecretKind::from_proto(req.kind)?;
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
        SecretScope::Org,
        ORG_OWNER_USER_ID,
    )?;
    let resp = PutHostSecretResponse {
        secret: Some(to_proto_secret(&meta)),
    };
    let (code, json) = json_response("vam.v1.PutHostSecretResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn list_org_secrets(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state
        .pdp
        .authorize(&auth, "ListOrgHostSecrets", USERS_ADMIN_SCOPE)?;
    let secrets = state.vault.list_org_metadata()?;
    let resp = ListHostSecretsResponse {
        secrets: secrets.iter().map(to_proto_secret).collect(),
    };
    let (code, json) = json_response("vam.v1.ListHostSecretsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn delete_org_secret(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "DeleteOrgHostSecret", USERS_ADMIN_SCOPE)?;
    authorize_org_secret_write(SecretScope::Org, auth.role())?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let binding_id = percent_decode_path(&id);
    if binding_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("binding_id is required".into()).into());
    }
    let meta = state
        .vault
        .delete_secret_at(SecretScope::Org, ORG_OWNER_USER_ID, &binding_id)?;
    let resp = DeleteHostSecretResponse {
        secret: Some(to_proto_secret(&meta)),
    };
    let (code, json) = json_response("vam.v1.DeleteHostSecretResponse", &resp)?;
    Ok(json_http_response(code, json))
}
