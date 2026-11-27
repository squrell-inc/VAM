//! Agent access routes (design/agent-access.md §4, Phase 1 wave 2):
//!
//! - `PUT  /v1/agents/{urn}/access-policy {visibility, mode}`: set who may see and use an agent.
//! - `POST /v1/agents/{urn}/access {message?}`: Add (`open`, granted at once, 200) or Request
//!   (`approval`, pending, 202).
//! - `POST /v1/access-requests/{id}/decide {decision, reason?}` (wave 3): approve or deny.
//! - `POST /v1/agents/{urn}/access/{userId}/revoke {reason}` (wave 3): end an approved access.
//!
//! The agent routes are parsed from the `/v1/agents/{*rest}` wildcard by suffix. The admin check is the
//! design §2.3 rule (`users:admin` on the key, or an `agent_admins` row), never `pdp.authorize`,
//! `catalog:write`, `publish:grant` or `keys:admin`. A Hidden agent answers 404 with the text of
//! a missing URN, built by the same constructor, so the two are equal byte for byte.

use std::collections::HashMap;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::agent_access::{
    access_view_on, decide_on, get_access_request_on, is_agent_or_org_admin_on, list_admins_on,
    live_access_row_on, put_policy_on, replace_admins_on, replay_fingerprint, request_access_on,
    revoke_on, standing_on, with_replay, AccessAttempt, AccessMode, AccessScope, AccessView,
    AdminEntry, AgentAccessPolicy, BrowseAgent, Decision, RequestState, Standing, Visibility,
};
use crate::api::auth::{require_replay_key, AuthContext};
use crate::api::catalog_auth::require_catalog_read;
use crate::api::error::ApiError;
use crate::api::paging::{self, PageQuery};
use crate::api::{ensure_ready, json_http_response, percent_decode_path, permissions, AppState};
use crate::catalog::catalog_not_found;
use crate::error::OrchestratorError;
use crate::owner::resolved_owner;

const POLICY_SUFFIX: &str = "/access-policy";
const ACCESS_SUFFIX: &str = "/access";
const ADMINS_SUFFIX: &str = "/admins";
const REVOKE_SUFFIX: &str = "/revoke";
const ACCESS_INFIX: &str = "/access/";
/// Longest `message` on an access request, in characters.
const MESSAGE_MAX_CHARS: usize = 1000;
/// Longest `reason` on a decision or a revoke, in characters.
const REASON_MAX_CHARS: usize = 1000;

/// The URN in `rest` when it names `…/access-policy`.
pub(crate) fn policy_urn(rest: &str) -> Option<String> {
    suffix_urn(rest, POLICY_SUFFIX)
}

/// The URN in `rest` when it names `…/access` (and not a longer `…/access/…` path).
pub(crate) fn access_urn(rest: &str) -> Option<String> {
    suffix_urn(rest, ACCESS_SUFFIX)
}

/// `(urn, userId)` when `rest` names `…/access/{userId}/revoke`. A user id holding a `/` cannot
/// be told from part of the URN, so it is refused with a 400 rather than mis-parsed.
pub(crate) fn revoke_target(rest: &str) -> Option<Result<(String, String), OrchestratorError>> {
    let head = rest.trim_start_matches('/').strip_suffix(REVOKE_SUFFIX)?;
    let (urn, user) = head.rsplit_once(ACCESS_INFIX)?;
    if urn.is_empty() || user.is_empty() {
        return None;
    }
    if user.contains('/') {
        return Some(Err(OrchestratorError::InvalidContract(
            "user id must not contain '/'".to_string(),
        )));
    }
    Some(Ok((percent_decode_path(urn), percent_decode_path(user))))
}

/// The head of a suffix route must parse as an agent URN; a path that only ends in `/admins`
/// or `/access` (an agent whose last segment is called that) falls through to its other route.
fn suffix_urn(rest: &str, suffix: &str) -> Option<String> {
    rest.trim_start_matches('/')
        .strip_suffix(suffix)
        .map(percent_decode_path)
        .filter(|urn| urn.starts_with(AGENT_URN_PREFIX))
}

const AGENT_URN_PREFIX: &str = "urn:vam:agent:";

/// The URN in `rest` when it names `…/admins`.
pub(crate) fn admins_urn(rest: &str) -> Option<String> {
    suffix_urn(rest, ADMINS_SUFFIX)
}

/// `PUT /v1/agents/{*rest}`: the access-policy route, else the draft-permissions route (which
/// answers 405 for any other path).
pub(crate) async fn put_agent_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    if let Some(urn) = policy_urn(&rest) {
        return put_access_policy(&state, &auth, &urn, &headers, &body);
    }
    if let Some(urn) = admins_urn(&rest) {
        return put_admins(&state, &auth, &urn, &headers, &body);
    }
    permissions::put_draft_permissions(State(state), auth, Path(rest), headers, body).await
}

const POLICY_DENIED: &str = "only an agent admin or org admin may change access to";

/// The one precheck of the admin writes, run inside the replay lock: a missing or Hidden agent
/// answers the missing-URN text, a caller who is not an admin by the design §2.3 rule (`users:admin`
/// on the key, or an `agent_admins` row; never `pdp.authorize` or a scope) answers 403.
fn admin_precheck(
    c: &Connection,
    auth: &AuthContext,
    urn: &str,
) -> Result<Standing, OrchestratorError> {
    let standing = standing_on(c, urn, auth.owner_user_id(), auth.has_users_admin())?;
    if !standing.admin {
        return Err(OrchestratorError::PermissionDenied(format!(
            "{POLICY_DENIED} '{urn}'"
        )));
    }
    Ok(standing)
}

/// The answer of a replayed or fresh write: JSON, or an empty 204.
fn replay_response(status: u16, body: String) -> Response {
    let code = StatusCode::from_u16(status).unwrap_or(StatusCode::OK);
    if code == StatusCode::NO_CONTENT {
        return Response::builder()
            .status(code)
            .body(axum::body::Body::empty())
            .unwrap_or_default();
    }
    json_http_response(code, body)
}

/// Every access write: the gates and the key in front, the body parsed and fingerprinted (a 400
/// is never stored), then one `with_replay`: precheck, lookup, state checks, write, render.
struct Write<'a> {
    state: &'a AppState,
    auth: &'a AuthContext,
    key: String,
    fingerprint: String,
}

impl<'a> Write<'a> {
    fn run<P>(
        &self,
        precheck: impl FnOnce(&Connection) -> Result<P, OrchestratorError>,
        run: impl FnOnce(&Connection, P) -> Result<(u16, String), OrchestratorError>,
    ) -> Result<Response, ApiError> {
        let owner = resolved_owner(self.auth.owner_user_id());
        let (status, body) = with_replay(
            self.state.store.as_ref(),
            owner,
            &self.key,
            &self.fingerprint,
            precheck,
            run,
        )?;
        Ok(replay_response(status, body))
    }
}

/// Gates shared by the six writes, in order: node ready, `catalog:read`, the replay key.
fn gate(state: &AppState, auth: &AuthContext, headers: &HeaderMap) -> Result<String, ApiError> {
    ensure_ready(state, true)?;
    require_catalog_read(auth)?;
    Ok(require_replay_key(headers)?)
}

fn put_access_policy(
    state: &AppState,
    auth: &AuthContext,
    urn: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<Response, ApiError> {
    let key = gate(state, auth, headers)?;
    let (visibility, mode) = parse_policy_body(body)?;
    let fingerprint = replay_fingerprint(&[
        "PUT",
        "/v1/agents/{urn}/access-policy",
        urn,
        visibility.as_str(),
        mode.as_str(),
    ]);
    Write {
        state,
        auth,
        key,
        fingerprint,
    }
    .run(
        |c| admin_precheck(c, auth, urn),
        |c, _| {
            let policy = put_policy_on(c, urn, visibility, mode, auth.owner_user_id())?;
            Ok((200, json!({ "policy": policy_json(&policy) }).to_string()))
        },
    )
}

fn parse_policy_body(body: &str) -> Result<(Visibility, AccessMode), OrchestratorError> {
    let value = parse_object(body)?;
    let text = |field: &str| -> Result<&str, OrchestratorError> {
        value
            .get(field)
            .and_then(Value::as_str)
            .ok_or_else(|| OrchestratorError::InvalidContract(format!("'{field}' is required")))
    };
    let visibility = match text("visibility")? {
        "org" => Visibility::Org,
        "restricted" => Visibility::Restricted,
        other => {
            return Err(OrchestratorError::InvalidContract(format!(
                "visibility must be 'org' or 'restricted', got '{other}'"
            )))
        }
    };
    let mode = match text("mode")? {
        "everyone" => AccessMode::Everyone,
        "open" => AccessMode::Open,
        "approval" => AccessMode::Approval,
        other => {
            return Err(OrchestratorError::InvalidContract(format!(
                "mode must be 'everyone', 'open' or 'approval', got '{other}'"
            )))
        }
    };
    if visibility == Visibility::Restricted && mode == AccessMode::Everyone {
        return Err(OrchestratorError::InvalidContract(
            "a restricted agent cannot be open to everyone: use mode 'open' or 'approval'"
                .to_string(),
        ));
    }
    Ok((visibility, mode))
}

/// The body as a JSON object; an empty body is `{}`.
fn parse_object(body: &str) -> Result<Map<String, Value>, OrchestratorError> {
    if body.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(body) {
        Ok(Value::Object(map)) => Ok(map),
        Ok(_) => Err(OrchestratorError::InvalidContract(
            "request body must be a JSON object".to_string(),
        )),
        Err(e) => Err(OrchestratorError::InvalidContract(format!(
            "invalid JSON body: {e}"
        ))),
    }
}

/// `POST /v1/agents/{urn}/access {message?}`: Add (`open`) or Request (`approval`). The agent is
/// read as the route's own missing-URN answer first (a Hidden one is 404); the rule, the policy
/// read and the insert then run under the same lock, so a concurrent switch to restricted
/// cannot slip between them.
pub(crate) fn post_access(
    state: &AppState,
    auth: &AuthContext,
    urn: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<Response, ApiError> {
    let key = gate(state, auth, headers)?;
    let message = parse_message(body)?;
    let fingerprint = replay_fingerprint(&[
        "POST",
        "/v1/agents/{urn}/access",
        urn,
        message.as_deref().unwrap_or(""),
    ]);
    let (caller, org_admin) = (auth.owner_user_id(), auth.has_users_admin());
    Write {
        state,
        auth,
        key,
        fingerprint,
    }
    .run(
        |c| standing_on(c, urn, caller, org_admin),
        |c, standing| {
            let row = match request_access_on(
                c,
                urn,
                caller,
                org_admin,
                standing.own_draft,
                message.as_deref(),
            )? {
                AccessAttempt::Hidden => return Err(catalog_not_found("agents", urn)),
                AccessAttempt::Usable => {
                    return Err(OrchestratorError::Conflict(format!(
                        "agent '{urn}' is already usable"
                    )))
                }
                AccessAttempt::AlreadyPending => {
                    return Err(OrchestratorError::Conflict(format!(
                        "agent '{urn}' already has a pending access request"
                    )))
                }
                AccessAttempt::Inserted(row) => row,
            };
            let status = if row.state == RequestState::Approved {
                200
            } else {
                202
            };
            let out = json!({ "request": request_json(&access_view_on(c, row, true)) });
            Ok((status, out.to_string()))
        },
    )
}

/// `DELETE /v1/agents/{urn}/access`: withdraw the caller's pending request or leave an approved
/// agent. Keyed on the caller's own row, so there is no rule precheck: a live row ends with 204
/// even after the agent turned restricted; with no row, a missing or Hidden agent answers the
/// missing-URN text and a Visible one `you have no access request`. 204, no body. Leaving is
/// checked at the next admission (A7).
pub(crate) fn delete_access(
    state: &AppState,
    auth: &AuthContext,
    urn: &str,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let key = gate(state, auth, headers)?;
    let fingerprint = replay_fingerprint(&["DELETE", "/v1/agents/{urn}/access", urn]);
    let (caller, org_admin) = (auth.owner_user_id(), auth.has_users_admin());
    Write {
        state,
        auth,
        key,
        fingerprint,
    }
    .run(
        |_| Ok(()),
        |c, ()| {
            crate::agent_access::withdraw_on(c, urn, caller, org_admin)?;
            Ok((204, String::new()))
        },
    )
}

/// The caller's `reason`: required when `required`, trimmed, never longer than the cap.
fn parse_reason(
    map: &Map<String, Value>,
    required: bool,
) -> Result<Option<String>, OrchestratorError> {
    let reason = match map.get("reason") {
        None | Some(Value::Null) => None,
        Some(Value::String(text)) => Some(text.trim().to_string()).filter(|t| !t.is_empty()),
        Some(_) => {
            return Err(OrchestratorError::InvalidContract(
                "reason must be a string".to_string(),
            ))
        }
    };
    if required && reason.is_none() {
        return Err(OrchestratorError::InvalidContract(
            "reason is required".to_string(),
        ));
    }
    if reason
        .as_deref()
        .is_some_and(|r| r.chars().count() > REASON_MAX_CHARS)
    {
        return Err(OrchestratorError::InvalidContract(format!(
            "reason is longer than {REASON_MAX_CHARS} characters"
        )));
    }
    Ok(reason)
}

fn parse_decision(body: &str) -> Result<(Decision, Option<String>), OrchestratorError> {
    let map = parse_object(body)?;
    let decision = match map.get("decision").and_then(Value::as_str) {
        Some("approve") => Decision::Approve,
        Some("deny") => Decision::Deny,
        Some(other) => {
            return Err(OrchestratorError::InvalidContract(format!(
                "decision must be 'approve' or 'deny', got '{other}'"
            )))
        }
        None => {
            return Err(OrchestratorError::InvalidContract(
                "'decision' is required".to_string(),
            ))
        }
    };
    let reason = parse_reason(&map, decision == Decision::Deny)?;
    Ok((decision, reason))
}

/// `POST /v1/access-requests/{id}/decide {decision, reason?}`. The caller must be an admin of
/// the request's agent by the design §2.3 rule, never `pdp.authorize` or a scope. Anyone else,
/// and an unknown id, gets the same 404, so request ids reveal nothing about hidden agents.
pub(crate) async fn post_decide(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(request_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let key = gate(&state, &auth, &headers)?;
    let (decision, reason) = parse_decision(&body)?;
    let fingerprint = replay_fingerprint(&[
        "POST",
        "/v1/access-requests/{id}/decide",
        &request_id,
        match decision {
            Decision::Approve => "approve",
            Decision::Deny => "deny",
        },
        reason.as_deref().unwrap_or(""),
    ]);
    let (caller, org_admin) = (auth.owner_user_id(), auth.has_users_admin());
    let not_found =
        || OrchestratorError::NotFound(format!("access request '{request_id}' not found"));
    Write {
        state: &state,
        auth: &auth,
        key,
        fingerprint,
    }
    .run(
        |c| {
            let row = get_access_request_on(c, &request_id)?.ok_or_else(not_found)?;
            if !is_agent_or_org_admin_on(c, &row.urn, caller, org_admin)? {
                return Err(not_found());
            }
            Ok(())
        },
        |c, ()| {
            let decided = decide_on(c, &request_id, decision, caller, reason.as_deref())?;
            let out = json!({ "request": request_json(&access_view_on(c, decided, true)) });
            Ok((200, out.to_string()))
        },
    )
}

/// `POST /v1/agents/{urn}/access/{userId}/revoke {reason}`: end the user's approved access. It
/// takes effect at the next admission; a session admitted before keeps running (A7).
pub(crate) fn post_revoke(
    state: &AppState,
    auth: &AuthContext,
    urn: &str,
    user_id: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<Response, ApiError> {
    let key = gate(state, auth, headers)?;
    let reason = parse_reason(&parse_object(body)?, true)?;
    let fingerprint = replay_fingerprint(&[
        "POST",
        "/v1/agents/{urn}/access/{userId}/revoke",
        urn,
        user_id,
        reason.as_deref().unwrap_or(""),
    ]);
    Write {
        state,
        auth,
        key,
        fingerprint,
    }
    .run(
        |c| admin_precheck(c, auth, urn).map(|_| ()),
        |c, ()| {
            let approved = live_access_row_on(c, urn, user_id)?
                .filter(|row| row.state == RequestState::Approved);
            if approved.is_none() {
                return Err(OrchestratorError::NotFound(format!(
                    "user '{user_id}' has no approved access to agent '{urn}'"
                )));
            }
            let row = revoke_on(c, urn, user_id, auth.owner_user_id(), reason.as_deref())?;
            let out = json!({ "request": request_json(&access_view_on(c, row, true)) });
            Ok((200, out.to_string()))
        },
    )
}

/// The most ids one `PUT …/admins` may name.
const ADMINS_MAX: usize = 50;

/// `{userIds: [...]}`: 0..=50 distinct, non-empty strings, in the order the caller gave them.
fn parse_admin_ids(body: &str) -> Result<Vec<String>, OrchestratorError> {
    let bad = |msg: &str| OrchestratorError::InvalidContract(msg.to_string());
    let map = parse_object(body)?;
    let Some(Value::Array(items)) = map.get("userIds") else {
        return Err(bad("'userIds' must be an array of user ids"));
    };
    if items.len() > ADMINS_MAX {
        return Err(bad(&format!("userIds holds more than {ADMINS_MAX} ids")));
    }
    let mut ids: Vec<String> = Vec::with_capacity(items.len());
    for item in items {
        let Value::String(id) = item else {
            return Err(bad("userIds must hold strings"));
        };
        if id.is_empty() {
            return Err(bad("userIds must not hold an empty id"));
        }
        if ids.contains(id) {
            return Err(bad(&format!("userIds names '{id}' twice")));
        }
        ids.push(id.clone());
    }
    Ok(ids)
}

fn admins_json(admins: &[AdminEntry]) -> Value {
    let rows: Vec<Value> = admins
        .iter()
        .map(|admin| {
            let mut out = Map::new();
            out.insert("userId".into(), json!(admin.user_id));
            if let Some(name) = &admin.username {
                out.insert("username".into(), json!(name));
            }
            out.insert("addedBy".into(), json!(admin.added_by));
            out.insert("addedAt".into(), json!(rfc3339(admin.added_at)));
            if admin.deactivated {
                out.insert("deactivated".into(), json!(true));
            }
            Value::Object(out)
        })
        .collect();
    json!({ "admins": rows })
}

/// `GET /v1/agents/{urn}/admins`: an admin of the agent or an org admin; Hidden is the
/// missing-URN text, anyone else 403.
pub(crate) fn get_admins(
    state: &AppState,
    auth: &AuthContext,
    urn: &str,
) -> Result<Response, ApiError> {
    ensure_ready(state, false)?;
    let out = state.store.with_access_connection(|c| {
        let standing = standing_on(c, urn, auth.owner_user_id(), auth.has_users_admin())?;
        if !standing.admin {
            return Err(OrchestratorError::PermissionDenied(format!(
                "only an agent admin or org admin may see the admins of '{urn}'"
            )));
        }
        Ok(admins_json(&list_admins_on(c, urn)?))
    })?;
    Ok(json_http_response(StatusCode::OK, out.to_string()))
}

/// `PUT /v1/agents/{urn}/admins {userIds}`: replace the list; answers the `GET` shape. Order of
/// the checks: key, body shape (400), then inside the lock the Hidden and admin rules (404, 403),
/// the replay lookup, and last that every NEW id is an existing, active user (400).
fn put_admins(
    state: &AppState,
    auth: &AuthContext,
    urn: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<Response, ApiError> {
    let key = gate(state, auth, headers)?;
    let ids = parse_admin_ids(body)?;
    let mut parts: Vec<&str> = vec!["PUT", "/v1/agents/{urn}/admins", urn];
    parts.extend(ids.iter().map(String::as_str));
    let fingerprint = replay_fingerprint(&parts);
    Write {
        state,
        auth,
        key,
        fingerprint,
    }
    .run(
        |c| admin_precheck(c, auth, urn).map(|_| ()),
        |c, ()| {
            replace_admins_on(c, urn, &ids, auth.owner_user_id())?;
            Ok((200, admins_json(&list_admins_on(c, urn)?).to_string()))
        },
    )
}

// ---- lists -------------------------------------------------------------------------------

fn ok_json(body: Value) -> Response {
    json_http_response(StatusCode::OK, body.to_string())
}

/// `GET /v1/catalog/agents?pageSize&pageToken`: the agents the caller may ask for or use (design
/// §6). Hidden agents are filtered in SQL on every page; the cursor is the last URN.
pub(crate) async fn get_catalog_agents(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(page): Query<PageQuery>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    require_catalog_read(&auth)?;
    let size = page.size()?;
    let after = paging::str_cursor(page.cursor("catalog-agents")?)?;
    let mut rows = state.store.browse_agents(
        auth.owner_user_id(),
        auth.has_users_admin(),
        after.as_deref(),
        size + 1,
    )?;
    let more = rows.len() > size;
    rows.truncate(size);
    let next = match (more, rows.last()) {
        (true, Some(last)) => paging::token("catalog-agents", std::slice::from_ref(&last.urn)),
        _ => String::new(),
    };
    Ok(ok_json(json!({
        "agents": rows.iter().map(browse_json).collect::<Vec<_>>(),
        "pagination": { "nextPageToken": next },
    })))
}

fn browse_json(agent: &BrowseAgent) -> Value {
    let mut access = Map::new();
    access.insert("state".into(), json!(agent.state));
    if let Some(id) = &agent.request_id {
        access.insert("requestId".into(), json!(id));
    }
    if let Some(at) = agent.decided_at {
        access.insert("decidedAt".into(), json!(rfc3339(at)));
    }
    if let Some(reason) = &agent.reason {
        access.insert("reason".into(), json!(reason));
    }
    let mut out = Map::new();
    out.insert("urn".into(), json!(agent.urn));
    if !agent.display_name.is_empty() {
        out.insert("displayName".into(), json!(agent.display_name));
    }
    if !agent.description.is_empty() {
        out.insert("description".into(), json!(agent.description));
    }
    out.insert("catalogTier".into(), json!(agent.catalog_tier));
    out.insert("latestVersion".into(), json!(agent.latest_version));
    out.insert("visibility".into(), json!(agent.visibility.as_str()));
    out.insert("mode".into(), json!(agent.mode.as_str()));
    out.insert("access".into(), Value::Object(access));
    out.insert("canAdmin".into(), json!(agent.can_admin));
    Value::Object(out)
}

/// One page of access rows as `{requests, pagination}`; `kind` binds the token to its list.
fn access_page(
    state: &AppState,
    auth: &AuthContext,
    page: &PageQuery,
    kind: &str,
    scope: AccessScope,
    filter: Option<RequestState>,
) -> Result<Response, ApiError> {
    let size = page.size()?;
    let after = paging::pair_cursor(page.cursor(kind)?)?;
    let mut rows = state.store.list_access(
        auth.owner_user_id(),
        auth.has_users_admin(),
        scope,
        filter,
        after.as_ref(),
        size + 1,
    )?;
    let more = rows.len() > size;
    rows.truncate(size);
    let next = match (more, rows.last()) {
        (true, Some(last)) => paging::token(
            kind,
            &[
                last.row.requested_at.to_string(),
                last.row.request_id.clone(),
            ],
        ),
        _ => String::new(),
    };
    Ok(ok_json(json!({
        "requests": rows.iter().map(request_json).collect::<Vec<_>>(),
        "pagination": { "nextPageToken": next },
    })))
}

/// `GET /v1/me/access?pageSize&pageToken`: the caller's own rows, all states, newest first.
pub(crate) async fn get_my_access(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(page): Query<PageQuery>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    require_catalog_read(&auth)?;
    access_page(&state, &auth, &page, "my-access", AccessScope::Mine, None)
}

/// `GET /v1/access-requests?state&adminOf&pageSize&pageToken`: requests for the agents the caller
/// administers (every row for an org admin), `state` defaulting to `pending`. A caller who
/// administers nothing gets an empty list with 200.
pub(crate) async fn get_access_requests(
    State(state): State<AppState>,
    auth: AuthContext,
    Query(page): Query<PageQuery>,
    Query(extra): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    require_catalog_read(&auth)?;
    let filter = match extra.get("state").map(String::as_str) {
        None | Some("pending") => Some(RequestState::Pending),
        Some("approved") => Some(RequestState::Approved),
        Some("denied") => Some(RequestState::Denied),
        Some("revoked") => Some(RequestState::Revoked),
        Some("withdrawn") => Some(RequestState::Withdrawn),
        Some("all") => None,
        Some(other) => {
            return Err(OrchestratorError::InvalidContract(format!(
                "state must be pending, approved, denied, revoked, withdrawn or all, got '{other}'"
            ))
            .into())
        }
    };
    match extra.get("adminOf").map(String::as_str) {
        None | Some("me") => {}
        Some(other) => {
            return Err(OrchestratorError::InvalidContract(format!(
                "adminOf must be 'me', got '{other}'"
            ))
            .into())
        }
    }
    let scope = AccessScope::Administered {
        org_admin: auth.has_users_admin(),
    };
    access_page(&state, &auth, &page, "access-requests", scope, filter)
}

fn parse_message(body: &str) -> Result<Option<String>, OrchestratorError> {
    let value = parse_object(body)?;
    match value.get("message") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            if text.chars().count() > MESSAGE_MAX_CHARS {
                return Err(OrchestratorError::InvalidContract(format!(
                    "message is longer than {MESSAGE_MAX_CHARS} characters"
                )));
            }
            Ok((!text.is_empty()).then(|| text.clone()))
        }
        Some(_) => Err(OrchestratorError::InvalidContract(
            "message must be a string".to_string(),
        )),
    }
}

fn rfc3339(secs: i64) -> String {
    chrono::DateTime::from_timestamp(secs, 0)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_default()
}

pub(crate) fn policy_json(policy: &AgentAccessPolicy) -> Value {
    json!({
        "urn": policy.urn,
        "visibility": policy.visibility.as_str(),
        "mode": policy.mode.as_str(),
        "updatedBy": policy.updated_by,
        "updatedAt": rfc3339(policy.updated_at),
    })
}

/// The design §6 `AccessRequest`: camelCase, RFC3339 times, unset optionals omitted. Everything
/// from other tables was read under the writer's lock into the view.
pub(crate) fn request_json(view: &AccessView) -> Value {
    let row = &view.row;
    let mut out = Map::new();
    out.insert("requestId".into(), json!(row.request_id));
    out.insert("urn".into(), json!(row.urn));
    if let Some(name) = &view.agent_display_name {
        out.insert("agentDisplayName".into(), json!(name));
    }
    out.insert("userId".into(), json!(row.user_id));
    if let Some(username) = &view.username {
        out.insert("username".into(), json!(username));
    }
    out.insert("state".into(), json!(row.state.as_str()));
    if let Some(message) = row.message.as_deref().filter(|m| !m.is_empty()) {
        out.insert("message".into(), json!(message));
    }
    out.insert("requestedAt".into(), json!(rfc3339(row.requested_at)));
    if let Some(by) = row.decided_by.as_deref() {
        out.insert("decidedBy".into(), json!(by));
    }
    if let Some(at) = row.decided_at {
        out.insert("decidedAt".into(), json!(rfc3339(at)));
    }
    if let Some(reason) = row.reason.as_deref().filter(|r| !r.is_empty()) {
        out.insert("reason".into(), json!(reason));
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_parsing_is_exact() {
        let urn = "urn:vam:agent:local.vam.dev/acme/gated";
        assert_eq!(
            policy_urn(&format!("{urn}/access-policy")).as_deref(),
            Some(urn)
        );
        assert_eq!(access_urn(&format!("{urn}/access")).as_deref(), Some(urn));
        assert_eq!(access_urn(&format!("{urn}/access-policy")), None);
        assert_eq!(access_urn(&format!("{urn}/access/u1/revoke")), None);
        assert_eq!(policy_urn(&format!("{urn}/permissions")), None);
        assert_eq!(access_urn("/access"), None);
    }

    #[test]
    fn policy_body_rules() {
        let ok = |body: &str| parse_policy_body(body);
        assert_eq!(
            ok(r#"{"visibility":"org","mode":"open"}"#).unwrap(),
            (Visibility::Org, AccessMode::Open)
        );
        assert_eq!(
            ok(r#"{"visibility":"restricted","mode":"approval"}"#).unwrap(),
            (Visibility::Restricted, AccessMode::Approval)
        );
        assert_eq!(
            ok(r#"{"visibility":"org","mode":"everyone"}"#).unwrap(),
            (Visibility::Org, AccessMode::Everyone)
        );
        for bad in [
            r#"{"visibility":"restricted","mode":"everyone"}"#,
            r#"{"visibility":"public","mode":"open"}"#,
            r#"{"visibility":"org","mode":"sometimes"}"#,
            r#"{"visibility":"org"}"#,
            r#"{"mode":"open"}"#,
            "",
            "[]",
            "not json",
        ] {
            assert!(
                matches!(ok(bad), Err(OrchestratorError::InvalidContract(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn message_rules() {
        assert_eq!(parse_message("").unwrap(), None);
        assert_eq!(parse_message("{}").unwrap(), None);
        assert_eq!(
            parse_message(r#"{"message":"please"}"#).unwrap().as_deref(),
            Some("please")
        );
        let long = format!(r#"{{"message":"{}"}}"#, "x".repeat(MESSAGE_MAX_CHARS));
        assert!(parse_message(&long).unwrap().is_some());
        let too_long = format!(r#"{{"message":"{}"}}"#, "x".repeat(MESSAGE_MAX_CHARS + 1));
        assert!(matches!(
            parse_message(&too_long),
            Err(OrchestratorError::InvalidContract(_))
        ));
        assert!(parse_message(r#"{"message":3}"#).is_err());
    }
}
