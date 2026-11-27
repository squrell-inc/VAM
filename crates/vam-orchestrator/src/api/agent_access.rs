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

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{json, Map, Value};

use crate::agent_access::{
    is_agent_or_org_admin, AccessMode, AccessRow, AgentAccess, AgentAccessPolicy, Decision,
    LiveState, RequestState, Visibility,
};
use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::catalog_auth::require_catalog_read;
use crate::api::error::ApiError;
use crate::api::{ensure_ready, json_http_response, percent_decode_path, permissions, AppState};
use crate::catalog::catalog_not_found;
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

const POLICY_SUFFIX: &str = "/access-policy";
const ACCESS_SUFFIX: &str = "/access";
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

fn suffix_urn(rest: &str, suffix: &str) -> Option<String> {
    rest.trim_start_matches('/')
        .strip_suffix(suffix)
        .filter(|urn| !urn.is_empty())
        .map(percent_decode_path)
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
    match policy_urn(&rest) {
        Some(urn) => put_access_policy(&state, &auth, &urn, &headers, &body),
        None => {
            permissions::put_draft_permissions(State(state), auth, Path(rest), headers, body).await
        }
    }
}

/// What the caller is to the agent, after the agent was read.
struct Standing {
    access: AgentAccess,
    admin: bool,
}

/// Read the agent as the route's own missing-URN answer and classify the caller. A missing URN
/// and a Hidden one both answer `agents '<urn>' not found in catalog`.
fn stand(state: &AppState, auth: &AuthContext, urn: &str) -> Result<Standing, OrchestratorError> {
    let store = state.store.as_ref();
    let caller = auth.owner_user_id();
    let org_admin = auth.has_users_admin();
    let record = store.get_agent_record_admitted(urn, caller, org_admin)?;
    let own_draft = record.channel == vam_spec::v1::SkillChannel::Draft as i32
        && crate::owner::resolved_owner(&record.owner_user_id)
            == crate::owner::resolved_owner(caller);
    let access = crate::agent_access::agent_access(store, urn, caller, org_admin, own_draft)?;
    if access == AgentAccess::Hidden {
        return Err(catalog_not_found("agents", urn));
    }
    let admin = is_agent_or_org_admin(store, urn, caller, org_admin)?;
    Ok(Standing { access, admin })
}

fn put_access_policy(
    state: &AppState,
    auth: &AuthContext,
    urn: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<Response, ApiError> {
    ensure_ready(state, true)?;
    require_catalog_read(auth)?;
    let _idempotency = require_idempotency_key(headers).map_err(ApiError::from)?;
    let standing = stand(state, auth, urn)?;
    if !standing.admin {
        return Err(OrchestratorError::PermissionDenied(format!(
            "only an agent admin or org admin may change access to '{urn}'"
        ))
        .into());
    }
    let (visibility, mode) = parse_policy_body(body)?;
    let policy =
        state
            .store
            .put_agent_access_policy(urn, visibility, mode, auth.owner_user_id())?;
    let out = json!({ "policy": policy_json(&policy) });
    Ok(json_http_response(StatusCode::OK, out.to_string()))
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

/// `POST /v1/agents/{urn}/access {message?}`: Add (`open`) or Request (`approval`).
pub(crate) fn post_access(
    state: &AppState,
    auth: &AuthContext,
    urn: &str,
    headers: &HeaderMap,
    body: &str,
) -> Result<Response, ApiError> {
    ensure_ready(state, true)?;
    require_catalog_read(auth)?;
    let _idempotency = require_idempotency_key(headers).map_err(ApiError::from)?;
    let standing = stand(state, auth, urn)?;
    match standing.access {
        AgentAccess::Hidden => return Err(catalog_not_found("agents", urn).into()),
        AgentAccess::Usable => {
            return Err(
                OrchestratorError::Conflict(format!("agent '{urn}' is already usable")).into(),
            )
        }
        AgentAccess::Visible {
            state: Some(LiveState::Pending),
        } => {
            return Err(OrchestratorError::Conflict(format!(
                "agent '{urn}' already has a pending access request"
            ))
            .into())
        }
        AgentAccess::Visible { .. } => {}
    }
    let message = parse_message(body)?;
    // A `(restricted, everyone)` row cannot be written; the rule reads one as approval.
    let policy = state.store.agent_access_policy(urn)?;
    let (initial, status) = match policy.map(|p| p.mode) {
        Some(AccessMode::Open) => (RequestState::Approved, StatusCode::OK),
        _ => (RequestState::Pending, StatusCode::ACCEPTED),
    };
    let row = state.store.insert_access_request(
        urn,
        auth.owner_user_id(),
        message.as_deref(),
        initial,
    )?;
    let out = json!({ "request": request_json(state.store.as_ref(), &row) });
    Ok(json_http_response(status, out.to_string()))
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
/// the request's agent by the design §2.3 rule (`users:admin` on the key, or an `agent_admins`
/// row), never `pdp.authorize` or a scope. Anyone else, and an unknown id, gets the same 404, so
/// request ids reveal nothing about hidden agents.
pub(crate) async fn post_decide(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(request_id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    require_catalog_read(&auth)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let not_found =
        || OrchestratorError::NotFound(format!("access request '{request_id}' not found"));
    let store = state.store.as_ref();
    let row = store
        .get_access_request(&request_id)?
        .ok_or_else(not_found)?;
    let caller = auth.owner_user_id();
    if !is_agent_or_org_admin(store, &row.urn, caller, auth.has_users_admin())? {
        return Err(not_found().into());
    }
    let (decision, reason) = parse_decision(&body)?;
    let decided = store.decide_access_request(&request_id, decision, caller, reason.as_deref())?;
    let out = json!({ "request": request_json(store, &decided) });
    Ok(json_http_response(StatusCode::OK, out.to_string()))
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
    ensure_ready(state, true)?;
    require_catalog_read(auth)?;
    let _idempotency = require_idempotency_key(headers).map_err(ApiError::from)?;
    let standing = stand(state, auth, urn)?;
    if !standing.admin {
        return Err(OrchestratorError::PermissionDenied(format!(
            "only an agent admin or org admin may change access to '{urn}'"
        ))
        .into());
    }
    let reason = parse_reason(&parse_object(body)?, true)?;
    let store = state.store.as_ref();
    let approved = store
        .live_access_row(urn, user_id)?
        .filter(|row| row.state == RequestState::Approved);
    if approved.is_none() {
        return Err(OrchestratorError::NotFound(format!(
            "user '{user_id}' has no approved access to agent '{urn}'"
        ))
        .into());
    }
    let row = store.revoke_access(urn, user_id, auth.owner_user_id(), reason.as_deref())?;
    let out = json!({ "request": request_json(store, &row) });
    Ok(json_http_response(StatusCode::OK, out.to_string()))
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

/// The design §6 `AccessRequest`: camelCase, RFC3339 times, unset optionals omitted.
pub(crate) fn request_json(store: &SqliteWorkflowStore, row: &AccessRow) -> Value {
    let mut out = Map::new();
    out.insert("requestId".into(), json!(row.request_id));
    out.insert("urn".into(), json!(row.urn));
    if let Some(name) = store
        .get_agent_admitted(&row.urn, &row.user_id, true)
        .ok()
        .map(|spec| spec.display_name)
        .filter(|name| !name.is_empty())
    {
        out.insert("agentDisplayName".into(), json!(name));
    }
    out.insert("userId".into(), json!(row.user_id));
    if let Ok(user) = store.get_user(&row.user_id) {
        out.insert("username".into(), json!(user.username));
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
