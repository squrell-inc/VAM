//! `/v1/me/contacts`: link, list and unlink the caller's own channel contacts
//! (`design/notifications.md` section 9.5). A foreign id is "not found". The link code is returned
//! once by the POST that starts the link and is never listed; it is stored hashed. Verification is
//! the broker's `LinkChannelAccount` (`SqliteWorkflowStore::verify_contact_with_code`).

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{delete, get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::api::auth::AuthContext;
use crate::api::error::ApiError;
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::contacts::CODE_TTL_SECS;
use crate::error::OrchestratorError;

const READ_SCOPE: &str = "notifications:read";
const WRITE_SCOPE: &str = "notifications:write";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/me/contacts", get(list).post(start))
        .route("/v1/me/contacts/:id", delete(unlink))
        .route("/v1/me/contacts/:id/unlink", post(unlink))
}

fn user(auth: &AuthContext) -> String {
    crate::owner::resolved_owner(auth.owner_user_id()).to_string()
}

fn ok(body: Value) -> Response {
    json_http_response(StatusCode::OK, body.to_string())
}

async fn list(State(state): State<AppState>, auth: AuthContext) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "ReadContacts", READ_SCOPE)?;
    let contacts = state.store.list_contacts(&user(&auth))?;
    Ok(ok(json!({
        "contacts": contacts.iter().map(|c| c.to_json()).collect::<Vec<_>>(),
    })))
}

async fn start(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "LinkContact", WRITE_SCOPE)?;
    let channel = body.get("channel").and_then(Value::as_str).ok_or_else(|| {
        OrchestratorError::InvalidContract("send `channel` (for example slack)".into())
    })?;
    // The account the user will link from, if they say so; only that account can verify.
    let address = body.get("address").and_then(Value::as_str);
    let (contact, code) = state
        .store
        .start_contact_link(&user(&auth), channel, address)?;
    Ok(ok(json!({
        "contact": contact.to_json(),
        "code": code,
        "expiresInSecs": CODE_TTL_SECS,
        "note": "Single use. Send it from your account to the VAM app within ten minutes. It is shown once.",
    })))
}

async fn unlink(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "UnlinkContact", WRITE_SCOPE)?;
    let contact = state.store.unlink_contact(&user(&auth), &id)?;
    Ok(ok(json!({ "contact": contact.to_json() })))
}
