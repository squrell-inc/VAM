//! `GET /v1/me` — identity read for any authenticated caller — and
//! `POST /v1/me/password` — self-service password change for a password session.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::Extension;

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::rate_limit::{self, PasswordAttemptLimiter};
use crate::api::users_admin::user_account;
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::error::OrchestratorError;
use crate::keys::AuthSource;
use crate::password::validate_password;
use crate::users::ChangeOwnPassword;
use vam_spec::v1::{ChangeMyPasswordRequest, ChangeMyPasswordResponse, GetMeResponse};

/// Returns `userId`, `username`, `role`, the bearer's live scopes, and the
/// node's `registryDomain`, `marketplaceDomain` and the caller's `defaultNamespace`.
///
/// Any authenticated password session or API key may call this — it is not
/// `users:admin`. Deactivated users and revoked keys are rejected in bearer
/// auth as 401 (see [`crate::api::auth::bearer_auth_middleware`]).
pub async fn get_me(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    let user_id = auth.owner_user_id().to_string();
    let username = match state.store.get_user(&user_id) {
        Ok(user) => {
            if user.is_deactivated() {
                return Err(OrchestratorError::Unauthenticated.into());
            }
            user.username
        }
        Err(OrchestratorError::NotFound(_)) => user_id.clone(),
        Err(e) => return Err(e.into()),
    };
    let domains = crate::registry::registry_domains();
    let resp = GetMeResponse {
        user_id,
        default_namespace: vam_spec::default_namespace(&username),
        registry_domain: domains.registry_domain().to_string(),
        marketplace_domain: domains.marketplace_domain().to_string(),
        username,
        role: auth.role().as_str().to_string(),
        scopes: auth.key.record.scopes.clone(),
    };
    let (code, json) = json_response("vam.v1.GetMeResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// Payload of `vam.v1.user.password_changed`. Field order is the wire order. It holds no
/// password, hash or session token, and that is the whole point of this type.
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct PasswordChangedAudit<'a> {
    user_id: &'a str,
    username: &'a str,
    role: &'a str,
    actor: &'static str,
    credential: &'static str,
    password_updated_at: i64,
}

/// Changes the caller's own password.
///
/// Only a password session (`vam login`) may call it: any API key, the bootstrap key and run
/// grants included, is refused. The subject is always the caller's own `user_id`, so it works
/// for every role. A wrong `currentPassword` is 403 (never 401: clients read 401 as "session
/// gone") and spends one of the user's wrong-attempt tokens, reserved before every verify. On success
/// every session of the user is revoked, the caller's included; API keys are kept.
/// Neither password is ever logged or echoed.
pub async fn change_my_password(
    State(state): State<AppState>,
    Extension(attempts): Extension<Arc<PasswordAttemptLimiter>>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    if auth.key.source != AuthSource::Session {
        return Err(OrchestratorError::PermissionDenied(
            "changing a password needs a password session (vam login); API keys cannot change a password"
                .to_string(),
        )
        .into());
    }
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: ChangeMyPasswordRequest = parse_json_body("vam.v1.ChangeMyPasswordRequest", &body)?;
    if req.current_password.is_empty() {
        return Err(
            OrchestratorError::InvalidContract("currentPassword is required".to_string()).into(),
        );
    }
    validate_password(&req.new_password).map_err(OrchestratorError::from)?;

    let user_id = auth.owner_user_id().to_string();
    // Reserve the attempt before verifying: check and take are one atomic step, so concurrent
    // guesses cannot all pass a check while argon2 runs. Nothing below awaits, so the token
    // is always settled: a wrong password keeps it spent, every other outcome refunds it.
    if let Err(refusal) = attempts.try_take(&user_id, Instant::now()) {
        return Ok(rate_limit::refuse(&refusal));
    }
    let outcome =
        state
            .store
            .change_own_password(&user_id, &req.current_password, &req.new_password);
    match outcome {
        Ok(ChangeOwnPassword::WrongCurrent) => Err(OrchestratorError::PermissionDenied(
            "current password is incorrect".to_string(),
        )
        .into()),
        Err(e) => {
            attempts.refund(&user_id, Instant::now());
            Err(e.into())
        }
        Ok(ChangeOwnPassword::Changed(user)) => {
            attempts.refund(&user_id, Instant::now());
            tracing::info!(
                user_id = user_id.as_str(),
                "user changed their own password"
            );
            let password_updated_at = user.password_updated_at.unwrap_or_default();
            // The password is already changed and every session revoked, so a failed audit
            // write cannot undo it: log loudly and still answer 200.
            let audit = PasswordChangedAudit {
                user_id: &user.user_id,
                username: &user.username,
                role: user.role.as_str(),
                actor: "self",
                credential: "session",
                password_updated_at,
            };
            if let Err(e) = state.store.record_identity_audit_event(
                &user_id,
                "vam.v1.user.password_changed",
                serde_json::to_string(&audit).unwrap_or_default().as_bytes(),
            ) {
                tracing::error!(
                    user_id = user_id.as_str(),
                    error = %e,
                    "password changed but the audit record could not be written"
                );
            }
            let resp = ChangeMyPasswordResponse {
                password_updated_at,
                user: Some(user_account(user)),
                relogin_required: true,
            };
            let (code, json) = json_response("vam.v1.ChangeMyPasswordResponse", &resp)?;
            Ok(json_http_response(code, json))
        }
    }
}
