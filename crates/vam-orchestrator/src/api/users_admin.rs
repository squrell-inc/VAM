//! Org-admin create, list, deactivate, and password reset of control-plane users.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, post};

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::error::OrchestratorError;
use crate::keys::USERS_ADMIN_SCOPE;
use crate::users::{UserRecord, UserRole};
use vam_spec::v1::{
    CreateUserRequest, CreateUserResponse, DeactivateUserResponse, ListUsersResponse,
    ResetUserPasswordRequest, ResetUserPasswordResponse, UserAccount,
};

pub fn users_collection_route() -> axum::routing::MethodRouter<AppState> {
    get(list_users).post(create_user)
}

pub fn deactivate_user_route() -> axum::routing::MethodRouter<AppState> {
    post(deactivate_user)
}

pub fn reset_user_password_route() -> axum::routing::MethodRouter<AppState> {
    post(reset_user_password)
}

pub(crate) fn user_account(user: UserRecord) -> UserAccount {
    UserAccount {
        user_id: user.user_id,
        username: user.username,
        role: user.role.as_str().to_string(),
        has_password: user.has_password,
        created_at: user.created_at,
        deactivated_at: user.deactivated_at.unwrap_or(0),
        deactivated: user.deactivated_at.is_some(),
    }
}

fn parse_create_role(raw: &str) -> Result<UserRole, OrchestratorError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(UserRole::Member);
    }
    UserRole::parse(trimmed)
}

async fn create_user(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "CreateUser", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: CreateUserRequest = parse_json_body("vam.v1.CreateUserRequest", &body)?;
    if req.username.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("username is required".into()).into());
    }
    if req.password.is_empty() {
        return Err(OrchestratorError::InvalidContract("password is required".into()).into());
    }
    let role = parse_create_role(&req.role)?;
    let user_id = if req.user_id.trim().is_empty() {
        req.username.clone()
    } else {
        req.user_id
    };
    let user = state
        .store
        .create_managed_user(&user_id, &req.username, &req.password, role)?;
    let resp = CreateUserResponse {
        user: Some(user_account(user)),
    };
    let (code, json) = json_response("vam.v1.CreateUserResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn list_users(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "ListUsers", USERS_ADMIN_SCOPE)?;
    let users = state
        .store
        .list_users()?
        .into_iter()
        .map(user_account)
        .collect();
    let resp = ListUsersResponse { users };
    let (code, json) = json_response("vam.v1.ListUsersResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn deactivate_user(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(user_id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "DeactivateUser", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    if user_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("user_id is required".into()).into());
    }
    let user = state.store.deactivate_user(&user_id)?;
    let resp = DeactivateUserResponse {
        user: Some(user_account(user)),
    };
    let (code, json) = json_response("vam.v1.DeactivateUserResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn reset_user_password(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(user_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "ResetUserPassword", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    if user_id.trim().is_empty() {
        return Err(OrchestratorError::InvalidContract("user_id is required".into()).into());
    }
    let req: ResetUserPasswordRequest = parse_json_body("vam.v1.ResetUserPasswordRequest", &body)?;
    if req.password.is_empty() {
        return Err(OrchestratorError::InvalidContract("password is required".into()).into());
    }
    let user = state
        .store
        .admin_reset_user_password(&user_id, &req.password)?;
    let resp = ResetUserPasswordResponse {
        user: Some(user_account(user)),
    };
    let (code, json) = json_response("vam.v1.ResetUserPasswordResponse", &resp)?;
    Ok(json_http_response(code, json))
}
