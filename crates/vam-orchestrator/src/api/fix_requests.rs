//! `POST /v1/fix-requests {decisionRef}` (docs: orchestrator/denials): a member asks the person
//! who can apply the fix a refusal named. The host resolves the stored denial row, checks that
//! the requester is the run's caller, and re-derives change, target and who from that row; a
//! client-sent `fix` (or any other field) is never read. Recipients are recomputed now.
//!
//! Every way a reference can fail to be the requester's to use (no such row, another caller's
//! row, a row with no askable fix, a row older than 30 days, an over-long reference) answers
//! the same 404 with the same text, after the same single lookup.

use axum::extract::rejection::JsonRejection;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use vam_spec::v1::VamErrorCode;

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::ApiError;
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::error::OrchestratorError;
use crate::fix_requests::{self, FixRequest, FixRequestError};

const SCOPE: &str = "runs:write";
/// The one text of every refused reference.
const NOT_FOUND_TEXT: &str = "denial not found";
/// No real id is this long; a longer reference is refused without a lookup.
const MAX_REF_BYTES: usize = 256;

pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/fix-requests", post(create))
}

fn not_found() -> ApiError {
    OrchestratorError::NotFound(NOT_FOUND_TEXT.into()).into()
}

fn invalid(message: &str) -> ApiError {
    OrchestratorError::InvalidContract(message.into()).into()
}

fn refused(status: StatusCode, code: VamErrorCode, message: &str) -> ApiError {
    ApiError {
        status,
        code,
        message: message.into(),
        retryable: false,
        denials: Vec::new(),
    }
}

fn body_of(req: &FixRequest, existing: bool) -> Value {
    json!({
        "id": req.id,
        "change": req.change,
        "target": req.target,
        "who": req.who,
        "createdAt": req.created_at,
        "asked": fix_requests::asked_text(&req.who),
        "existing": existing,
    })
}

async fn create(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: Result<Json<Value>, JsonRejection>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "CreateFixRequest", SCOPE)?;
    // Like the other mutating routes; the dedupe below is what makes a retry safe.
    let _idempotency = require_idempotency_key(&headers)?;
    // Parsed after the scope check, so a missing scope never depends on the body.
    let Json(body) = body.map_err(|_| invalid("the body must be a JSON object"))?;
    let reference = body
        .get("decisionRef")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("decisionRef is required"))?;

    let requester = crate::owner::resolved_owner(auth.owner_user_id()).to_string();
    let now = crate::catalog::unix_timestamp();

    // One lookup for every reference (none for one no id can be); then only in-memory checks
    // until the single refusal below, so no branch of the 404 does more work than another.
    let stored = if reference.len() > MAX_REF_BYTES {
        None
    } else {
        state.store.get_denial(reference)?
    };
    let entitled = stored
        .as_ref()
        .filter(|den| den.caller_user_id == requester)
        .and_then(|den| fix_requests::eligible(den, now).map(|el| (den, el)));
    let Some((den, el)) = entitled else {
        tracing::debug!(requester = %requester, "fix request refused: not the caller's, unknown or not askable");
        return Err(not_found());
    };

    // Recipients now: the current owner (or the active admins), never the requester, never a
    // deactivated user. A deleted agent has no owner to ask; a storage error is an error.
    let candidates = match crate::denials::fix_recipients(&state.store, el.who, &den.agent_urn) {
        Ok(ids) => ids,
        Err(OrchestratorError::NotFound(_)) => Vec::new(),
        Err(e) => return Err(e.into()),
    };
    let mut recipients: Vec<String> = Vec::new();
    for id in candidates {
        if id == requester || recipients.contains(&id) {
            continue;
        }
        match state.store.get_user(&id) {
            Ok(user) if !user.is_deactivated() => recipients.push(id),
            Ok(_) | Err(OrchestratorError::NotFound(_)) => {}
            Err(e) => return Err(e.into()),
        }
    }
    let requester_name = state
        .store
        .get_user(&requester)
        .map(|u| u.username)
        .unwrap_or_else(|_| requester.clone());
    let plan = fix_requests::plan(den, &el, &requester, &requester_name, &recipients, now);

    match state.store.file_fix_request(&plan) {
        Ok(filed) => {
            let status = if filed.existing {
                StatusCode::OK
            } else {
                StatusCode::CREATED
            };
            Ok(json_http_response(
                status,
                body_of(&filed.request, filed.existing).to_string(),
            ))
        }
        Err(FixRequestError::NoRecipients) => Err(refused(
            StatusCode::CONFLICT,
            VamErrorCode::FailedPrecondition,
            "nobody can be asked to apply this fix right now",
        )),
        Err(FixRequestError::RateLimited) => Err(refused(
            StatusCode::TOO_MANY_REQUESTS,
            VamErrorCode::ResourceExhausted,
            "too many host requests for this agent in the last 24 hours; try again later",
        )),
        Err(FixRequestError::Store(e)) => Err(e.into()),
    }
}
