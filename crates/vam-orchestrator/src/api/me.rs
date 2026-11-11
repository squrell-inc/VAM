//! `GET /v1/me` — identity read for any authenticated caller.

use axum::extract::State;
use axum::response::Response;

use crate::api::auth::AuthContext;
use crate::api::error::{json_response, ApiError};
use crate::api::{json_http_response, AppState};
use crate::error::OrchestratorError;
use vam_spec::v1::GetMeResponse;

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
