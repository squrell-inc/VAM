//! `GET/PUT /v1/node/notify`: the operator's notification settings (docs: hosted/notify).
//! The operator sets them (`brokers:admin`); anyone with `node:read` can read which
//! adapters are on and the org's kind defaults, but only the operator sees the vault ref names.
//! Values never pass through here: a ref is a vault secret name, checked for existence only.

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Value};

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::ApiError;
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::error::OrchestratorError;
use crate::keys::BROKERS_ADMIN_SCOPE;
use crate::node_notify::{NodeNotifySettings, CHANNELS};
use crate::secrets::SecretScope;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/node/notify", get(get_settings).put(put_settings))
}

fn view(settings: &NodeNotifySettings, updated_at: Option<i64>) -> Response {
    let mut body = serde_json::to_value(settings).unwrap_or_else(|_| json!({}));
    body["channels"] = json!(CHANNELS);
    if let Some(at) = updated_at {
        body["updatedAt"] = json!(at);
    }
    json_http_response(StatusCode::OK, body.to_string())
}

async fn get_settings(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "GetNodeNotify", "node:read")?;
    let (settings, at) = state.store.node_notify_settings()?;
    let operator = state
        .pdp
        .authorize(&auth, "GetNodeNotify", BROKERS_ADMIN_SCOPE)
        .is_ok();
    Ok(if operator {
        view(&settings, at)
    } else {
        view(&settings.redacted(), at)
    })
}

async fn put_settings(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutNodeNotify", BROKERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let raw: Value = serde_json::from_str(&body)
        .map_err(|e| OrchestratorError::InvalidContract(format!("body is not JSON: {e}")))?;
    let asked: NodeNotifySettings = serde_json::from_value(raw)
        .map_err(|e| OrchestratorError::InvalidContract(format!("notify settings: {e}")))?;
    // Validated before anything is written; a refused body leaves the settings as they were.
    let settings = asked.validate(|name| {
        state
            .store
            .host_secret_exists(SecretScope::Org, crate::secrets::ORG_OWNER_USER_ID, name)
    })?;
    let at = state.store.put_node_notify_settings(&settings)?;
    Ok(view(&settings, Some(at)))
}
