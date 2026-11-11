//! HTTP login, refresh, and logout. Username + password only — no email, no OIDC.

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::Response;
use serde::{Deserialize, Serialize};

use crate::api::auth::AuthContext;
use crate::api::error::ApiError;
use crate::api::{json_http_response, AppState};
use crate::error::OrchestratorError;
use crate::keys::AuthSource;
use crate::session_tokens::IssuedSession;

#[derive(Debug, Deserialize)]
struct LoginRequest {
    username: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct RefreshRequest {
    #[serde(alias = "refreshToken")]
    refresh_token: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct LoginResponse {
    user_id: String,
    username: String,
    token: String,
    expires_at: i64,
    refresh_token: String,
    refresh_expires_at: i64,
}

#[derive(Debug, Serialize)]
struct LogoutResponse {
    revoked: bool,
}

impl From<IssuedSession> for LoginResponse {
    fn from(issued: IssuedSession) -> Self {
        Self {
            user_id: issued.user_id,
            username: issued.username,
            token: issued.token,
            expires_at: issued.expires_at,
            refresh_token: issued.refresh_token,
            refresh_expires_at: issued.refresh_expires_at,
        }
    }
}

pub async fn login(State(state): State<AppState>, body: String) -> Result<Response, ApiError> {
    crate::api::ensure_ready(&state, true)?;
    let req: LoginRequest = serde_json::from_str(&body)
        .map_err(|e| OrchestratorError::InvalidContract(format!("invalid login request: {e}")))?;
    if req.username.trim().is_empty() || req.password.is_empty() {
        return Err(OrchestratorError::Unauthenticated.into());
    }
    let issued = state
        .store
        .login_with_password(&req.username, &req.password)?;
    json_login_response(issued)
}

pub async fn refresh_login(
    State(state): State<AppState>,
    body: String,
) -> Result<Response, ApiError> {
    crate::api::ensure_ready(&state, true)?;
    let req: RefreshRequest = serde_json::from_str(&body)
        .map_err(|e| OrchestratorError::InvalidContract(format!("invalid refresh request: {e}")))?;
    if req.refresh_token.is_empty() {
        return Err(OrchestratorError::Unauthenticated.into());
    }
    let issued = state.store.refresh_session(&req.refresh_token)?;
    json_login_response(issued)
}

pub async fn logout(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    crate::api::ensure_ready(&state, true)?;
    let revoked = if auth.key.source == AuthSource::Session {
        state
            .store
            .revoke_session_by_token_id(&auth.key.record.key_id)?
            > 0
    } else {
        false
    };
    let body = serde_json::to_string(&LogoutResponse { revoked })
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    Ok(json_http_response(StatusCode::OK, body))
}

fn json_login_response(issued: IssuedSession) -> Result<Response, ApiError> {
    let body = serde_json::to_string(&LoginResponse::from(issued))
        .map_err(|e| OrchestratorError::Core(e.to_string()))?;
    Ok(json_http_response(StatusCode::OK, body))
}
