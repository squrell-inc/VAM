//! `GET /v1/notifications`, `/summary`, `/{id}`, `POST /read`, `/{id}/read`, `/{id}/acted`
//! (`design/notifications.md` section 3.2). Every route answers only the caller's rows; a
//! foreign id is "not found". The API never decides an approval.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::api::auth::AuthContext;
use crate::api::error::{json_response, ApiError};
use crate::api::{ensure_ready, json_http_response, paging, AppState};
use crate::error::OrchestratorError;
use crate::notifications::{Kind, ListFilter, Row};
use crate::workflow::SqliteWorkflowStore;

const READ_SCOPE: &str = "notifications:read";
const WRITE_SCOPE: &str = "notifications:write";
/// Largest `pageSize` (the design caps the list at 100).
const MAX_SIZE: usize = 100;
const DEFAULT_SIZE: usize = 50;
const AGENT_TEXT_CHARS: usize = 500;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/notifications", get(list))
        .route("/v1/notifications/summary", get(summary))
        .route("/v1/notifications/read", post(read_many))
        .route("/v1/notifications/:id", get(get_one))
        .route("/v1/notifications/:id/read", post(read_one))
        .route("/v1/notifications/:id/acted", post(acted))
        .route("/v1/me/notification-prefs", get(get_prefs).put(put_prefs))
}

fn user(auth: &AuthContext) -> String {
    crate::owner::resolved_owner(auth.owner_user_id()).to_string()
}

fn ok(body: Value) -> Response {
    json_http_response(StatusCode::OK, body.to_string())
}

fn bad_token() -> OrchestratorError {
    OrchestratorError::InvalidContract("pageToken is not a token this list returned".into())
}

async fn list(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(page): Query<paging::PageQuery>,
    Query(extra): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state
        .pdp
        .authorize(&auth, "ReadNotifications", READ_SCOPE)?;
    let asked = page
        .page_size
        .as_deref()
        .is_some_and(|s| !s.trim().is_empty());
    let size = if asked {
        page.size()?.min(MAX_SIZE)
    } else {
        DEFAULT_SIZE
    };
    let after = match page.cursor("notifications")? {
        None => None,
        Some(p) if p.len() == 3 => Some((
            p[0].parse::<i64>().map_err(|_| bad_token())?,
            p[1].parse::<i64>().map_err(|_| bad_token())?,
            p[2].clone(),
        )),
        Some(_) => return Err(bad_token().into()),
    };
    let filter = ListFilter {
        open_only: extra.get("state").map(String::as_str) == Some("open"),
        unread_only: matches!(extra.get("unread").map(String::as_str), Some("true" | "1")),
        kind: extra.get("kind").filter(|k| !k.is_empty()).cloned(),
    };
    let mut rows = state
        .store
        .list_notifications(&user(&auth), &filter, after, size + 1)?;
    let more = rows.len() > size;
    rows.truncate(size);
    let next = match (more, rows.last()) {
        (true, Some(last)) => {
            let (t, ts, id) = SqliteWorkflowStore::notification_keyset(last);
            paging::token("notifications", &[t.to_string(), ts.to_string(), id])
        }
        _ => String::new(),
    };
    Ok(ok(json!({
        "records": rows.iter().map(Row::to_json).collect::<Vec<_>>(),
        "pagination": { "nextPageToken": next },
    })))
}

async fn summary(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state
        .pdp
        .authorize(&auth, "ReadNotifications", READ_SCOPE)?;
    let (counts, etag) = state.store.notification_summary(&user(&auth))?;
    let etag_value = HeaderValue::from_str(&etag)
        .map_err(|_| OrchestratorError::Core("etag is not a header value".into()))?;
    let matches = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag));
    if matches {
        let mut resp = StatusCode::NOT_MODIFIED.into_response();
        resp.headers_mut().insert(header::ETAG, etag_value);
        return Ok(resp);
    }
    let mut resp = ok(json!({
        "action": counts.action,
        "warn": counts.warn,
        "unread": counts.unread,
        "etag": etag,
    }));
    resp.headers_mut().insert(header::ETAG, etag_value);
    Ok(resp)
}

/// The record, with the approval a decision kind names and the agent's own words, labelled.
async fn get_one(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state
        .pdp
        .authorize(&auth, "ReadNotifications", READ_SCOPE)?;
    let who = user(&auth);
    let row = state.store.get_notification(&who, &id)?;
    let mut out = row.to_json();
    if row.is_decision() {
        if let Some(approval_id) = row.approval_id.as_deref() {
            if let Ok(stored) = state.store.get_approval_for_owner(approval_id, &who) {
                if stored.published {
                    let label = if stored.kind == "gate" {
                        "from the playbook".to_string()
                    } else if row.agent_label.is_empty() {
                        "written by the agent, not verified by the host".to_string()
                    } else {
                        format!("written by {}, not verified by the host", row.agent_label)
                    };
                    let text: String = stored
                        .approval_message
                        .chars()
                        .take(AGENT_TEXT_CHARS)
                        .collect();
                    out["agentText"] = json!({ "label": label, "text": text });
                    let resp = vam_spec::v1::GetApprovalResponse {
                        approval: Some(crate::api::stored_to_approval_request(&stored)),
                        status: Some(crate::api::stored_to_approval_status(&stored)),
                    };
                    let (_, json) = json_response("vam.v1.GetApprovalResponse", &resp)?;
                    if let Ok(v) = serde_json::from_str::<Value>(&json) {
                        out["approval"] = v;
                    }
                }
            }
        }
    }
    Ok(ok(out))
}

async fn read_one(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "UpdateNotifications", WRITE_SCOPE)?;
    let row = state.store.mark_notification_read(&user(&auth), &id)?;
    Ok(ok(row.to_json()))
}

async fn read_many(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "UpdateNotifications", WRITE_SCOPE)?;
    let ids: Vec<String> = body
        .get("ids")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    let before = body.get("before").and_then(Value::as_i64);
    if ids.is_empty() && before.is_none() {
        return Err(OrchestratorError::InvalidContract(
            "send `ids` or `before` (unix seconds)".into(),
        )
        .into());
    }
    let changed = state
        .store
        .mark_notifications_read(&user(&auth), &ids, before)?;
    Ok(ok(json!({ "updated": changed })))
}

/// The caller's preferences, with the mode each kind has now (their setting, the node's default,
/// or the built-in one). `warn` means immediate only when the record is a warning.
fn prefs_view(
    state: &AppState,
    prefs: &crate::notification_prefs::Prefs,
) -> Result<Value, OrchestratorError> {
    let (node, _) = state.store.node_notify_settings()?;
    let effective: serde_json::Map<String, Value> = Kind::ALL
        .into_iter()
        .map(|k| {
            (
                k.as_str().to_string(),
                json!(prefs.effective_mode(&node, k)),
            )
        })
        .collect();
    let mut out = serde_json::to_value(prefs).unwrap_or_else(|_| json!({}));
    out["effective"] = Value::Object(effective);
    Ok(out)
}

async fn get_prefs(State(state): State<AppState>, auth: AuthContext) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state
        .pdp
        .authorize(&auth, "ReadNotifications", READ_SCOPE)?;
    let prefs = state.store.notification_prefs(&user(&auth))?;
    Ok(ok(prefs_view(&state, &prefs)?))
}

/// Replaces the caller's whole document: `{kinds, quietHours: {start, end, tz}, always}`.
async fn put_prefs(
    State(state): State<AppState>,
    auth: AuthContext,
    Json(body): Json<Value>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "UpdateNotifications", WRITE_SCOPE)?;
    let asked: crate::notification_prefs::Prefs = serde_json::from_value(body)
        .map_err(|e| OrchestratorError::InvalidContract(format!("notification prefs: {e}")))?;
    let prefs = asked.validate()?;
    state.store.put_notification_prefs(&user(&auth), &prefs)?;
    Ok(ok(prefs_view(&state, &prefs)?))
}

async fn acted(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "UpdateNotifications", WRITE_SCOPE)?;
    let surface = crate::run_facts::surface_from_header(
        headers
            .get(crate::run_facts::SURFACE_HEADER)
            .and_then(|v| v.to_str().ok()),
    );
    let row = state
        .store
        .mark_notification_acted(&user(&auth), &id, surface)?;
    Ok(ok(row.to_json()))
}
