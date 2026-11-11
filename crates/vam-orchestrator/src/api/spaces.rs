//! `GET /v1/spaces`, `GET /v1/spaces/:id`: the caller's Spaces. A Space is read with its owner
//! (a foreign id is "not found"); `name` is the label, `spaceId` the identity.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::{get, patch};
use axum::Router;
use prost_types::Timestamp;
use vam_spec::v1::{ListSpacesResponse, Space};

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::{json_response, ApiError};
use crate::api::{ensure_ready, json_http_response, paging, AppState};
use crate::error::OrchestratorError;
use crate::spaces::SpaceRow;

const SCOPE: &str = "sessions:write";

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/spaces", get(list_spaces).post(create_space))
        .route("/v1/spaces/:id", get(get_space).patch(rename_space))
        .route("/v1/spaces/:id/threads/:thread", patch(rename_thread))
}

fn ts(seconds: i64) -> Timestamp {
    Timestamp { seconds, nanos: 0 }
}

fn space_message(row: &SpaceRow) -> Space {
    Space {
        space_id: row.space_id.clone(),
        name: row.label.clone(),
        workspace_id: row.space_id.clone(),
        agent_urn: row.agent_urn.clone().unwrap_or_default(),
        created_at: Some(ts(row.created_at)),
        last_active_at: Some(ts(row.last_active_at)),
        created_by_grant_id: row.created_by_grant_id.clone().unwrap_or_default(),
    }
}

async fn list_spaces(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(page): Query<paging::PageQuery>,
    Query(extra): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "ListSpaces", SCOPE)?;
    let size = page.size()?;
    let after = paging::pair_cursor(page.cursor("spaces")?)?;
    let agent = extra
        .get("agentUrn")
        .map(String::as_str)
        .filter(|u| !u.is_empty());
    let mut rows = state
        .store
        .list_spaces(auth.owner_user_id(), agent, after, size + 1)?;
    let more = rows.len() > size;
    rows.truncate(size);
    let next_page_token = match (more, rows.last()) {
        (true, Some(last)) => paging::token(
            "spaces",
            &[last.created_at.to_string(), last.space_id.clone()],
        ),
        _ => String::new(),
    };
    let resp = ListSpacesResponse {
        spaces: rows.iter().map(space_message).collect(),
        next_page_token,
    };
    let (code, json) = json_response("vam.v1.ListSpacesResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn get_space(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "GetSpace", SCOPE)?;
    let space_id = crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &id)?;
    let row = state.store.get_space(auth.owner_user_id(), &space_id)?;
    let threads = state
        .store
        .list_thread_rows(auth.owner_user_id(), &space_id)?;
    let (code, json) = json_response("vam.v1.Space", &space_message(&row))?;
    let mut value: serde_json::Value = serde_json::from_str(&json).unwrap_or_default();
    if let Some(object) = value.as_object_mut() {
        object.insert(
            "threads".into(),
            threads
                .iter()
                .map(|t| {
                    serde_json::json!({
                        "threadId": t.thread_id,
                        "spaceId": t.space_id,
                        "name": t.label,
                        "createdAt": t.created_at,
                    })
                })
                .collect(),
        );
    }
    Ok(json_http_response(
        code,
        serde_json::to_string(&value).unwrap_or(json),
    ))
}

/// The `label` of a JSON body; a missing or blank label is refused (a rename never empties a name).
fn body_label(body: &str, fallback: Option<&str>) -> Result<String, ApiError> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| OrchestratorError::InvalidContract(format!("invalid JSON body: {e}")))?;
    let raw = value.get("label").and_then(|v| v.as_str()).unwrap_or("");
    if raw.trim().is_empty() && fallback.is_none() {
        return Err(OrchestratorError::InvalidContract("label is required".into()).into());
    }
    Ok(crate::spaces::clean_label(raw, fallback.unwrap_or(""))?)
}

/// `POST /v1/spaces {agentUrn, label}`: a new Space for an agent the caller may run. The
/// `Idempotency-Key` header makes a retry return the same Space.
async fn create_space(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "CreateSpace", SCOPE)?;
    let key = require_idempotency_key(&headers)?;
    let value: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| OrchestratorError::InvalidContract(format!("invalid JSON body: {e}")))?;
    let agent = value
        .get("agentUrn")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .ok_or_else(|| OrchestratorError::InvalidContract("agentUrn is required".into()))?;
    let label = body_label(&body, Some(crate::spaces::UNTITLED))?;
    // The same catalog check a run makes: released agents, and the caller's own drafts.
    state
        .store
        .get_agent_visible(agent, auth.owner_user_id(), auth.has_users_admin())?;
    let row = state
        .store
        .create_space_once(auth.owner_user_id(), &key, agent, &label)?;
    let (code, json) = json_response("vam.v1.Space", &space_message(&row))?;
    Ok(json_http_response(code, json))
}

/// `PATCH /v1/spaces/{id} {label}`: renames in place. Id and directory never change.
async fn rename_space(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "RenameSpace", SCOPE)?;
    let space_id = crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &id)?;
    let label = body_label(&body, None)?;
    let row = state
        .store
        .rename_space(auth.owner_user_id(), &space_id, &label)?;
    let (code, json) = json_response("vam.v1.Space", &space_message(&row))?;
    Ok(json_http_response(code, json))
}

/// `PATCH /v1/spaces/{id}/threads/{threadId} {label}`: renames a thread in place.
async fn rename_thread(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((id, thread)): Path<(String, String)>,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "RenameThread", SCOPE)?;
    let space_id = crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &id)?;
    let label = body_label(&body, None)?;
    let row = state
        .store
        .rename_thread(auth.owner_user_id(), &space_id, thread.trim(), &label)?;
    let value = serde_json::json!({
        "threadId": row.thread_id,
        "spaceId": row.space_id,
        "name": row.label,
        "createdAt": row.created_at,
    });
    Ok(json_http_response(
        axum::http::StatusCode::OK,
        value.to_string(),
    ))
}
