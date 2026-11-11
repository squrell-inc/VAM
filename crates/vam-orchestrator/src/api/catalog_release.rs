use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::response::Response;

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::catalog_auth::{require_catalog_read, require_release_authorization};
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::{ensure_ready, json_http_response, percent_decode_path, AppState};
use crate::catalog::CatalogVersionRow;
use crate::catalog_publish::ReleaseMeta;
use crate::error::OrchestratorError;
use vam_spec::v1::{
    CatalogVersionEntry, ListCatalogVersionsResponse, ReleaseAgentResponse, ReleaseCatalogRequest,
    ReleasePlaybookResponse, ReleasePolicyResponse, ReleaseSkillResponse, SkillChannel, Version,
};

pub fn agent_release_route() -> axum::routing::MethodRouter<AppState> {
    axum::routing::post(release_agent_wildcard)
}

pub fn playbook_release_route() -> axum::routing::MethodRouter<AppState> {
    axum::routing::post(release_playbook_wildcard)
}

pub fn skill_release_route() -> axum::routing::MethodRouter<AppState> {
    axum::routing::post(release_skill_wildcard)
}

pub fn policy_release_route() -> axum::routing::MethodRouter<AppState> {
    axum::routing::post(release_policy_wildcard)
}

pub(crate) async fn release_agent_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let urn = parse_release_urn(&rest)?;
    let (version, note) = parse_release_request(&body)?;
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    require_release_authorization(&state.store, &auth, "agents", &urn, "agent")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let meta = release_meta(&note, &auth);
    let spec = state.store.release_agent_with_meta(&urn, &version, &meta)?;
    let resp = ReleaseAgentResponse { spec: Some(spec) };
    let (code, json) = json_response("vam.v1.ReleaseAgentResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn release_playbook_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let urn = parse_release_urn(&rest)?;
    let (version, note) = parse_release_request(&body)?;
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    require_release_authorization(&state.store, &auth, "playbooks", &urn, "playbook")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let meta = release_meta(&note, &auth);
    let spec = state
        .store
        .release_playbook_with_meta(&urn, &version, &meta)?;
    let resp = ReleasePlaybookResponse { spec: Some(spec) };
    let (code, json) = json_response("vam.v1.ReleasePlaybookResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn release_skill_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let urn = parse_release_urn(&rest)?;
    let (version, note) = parse_release_request(&body)?;
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    require_release_authorization(&state.store, &auth, "skills", &urn, "skill")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    crate::node_base::check_draft_skill(&state.store, &urn, auth.owner_user_id())?;
    let meta = release_meta(&note, &auth);
    let spec = state.store.release_skill_with_meta(&urn, &version, &meta)?;
    let resp = ReleaseSkillResponse { spec: Some(spec) };
    let (code, json) = json_response("vam.v1.ReleaseSkillResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn release_policy_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    let urn = parse_release_urn(&rest)?;
    let (version, note) = parse_release_request(&body)?;
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    require_release_authorization(&state.store, &auth, "policies", &urn, "policy")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let meta = release_meta(&note, &auth);
    let record = state
        .store
        .release_policy_with_meta(&urn, &version, &meta)?;
    let resp = ReleasePolicyResponse {
        record: Some(record),
    };
    let (code, json) = json_response("vam.v1.ReleasePolicyResponse", &resp)?;
    Ok(json_http_response(code, json))
}

/// `GET /v1/<kind>/<urn>/versions`: the artifact's version history. Released
/// rows are visible to every catalog reader. The caller's draft is listed
/// first only for its owner, or for a caller with `users:admin`.
pub(crate) fn versions_response(
    state: &AppState,
    auth: &AuthContext,
    table: &str,
    urn_raw: &str,
) -> Result<Response, ApiError> {
    require_catalog_read(auth)?;
    let urn = percent_decode_path(urn_raw);
    let rows = state.store.list_catalog_versions(
        table,
        &urn,
        auth.owner_user_id(),
        auth.has_users_admin(),
    )?;
    let resp = ListCatalogVersionsResponse {
        versions: rows.into_iter().map(version_entry).collect(),
    };
    let (code, json) = json_response("vam.v1.ListCatalogVersionsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

fn release_meta<'a>(note: &'a str, auth: &'a AuthContext) -> ReleaseMeta<'a> {
    ReleaseMeta {
        note,
        released_by: Some(auth.owner_user_id()),
    }
}

fn version_entry(row: CatalogVersionRow) -> CatalogVersionEntry {
    let released = row.channel == SkillChannel::Released;
    CatalogVersionEntry {
        version: Some(row.version),
        channel: row.channel as i32,
        digest: row.digest,
        released_by: row.released_by,
        released_at: released.then(|| unix_to_timestamp(row.released_at)),
        updated_at: (!released).then(|| unix_to_timestamp(row.updated_at)),
        note: row.note,
    }
}

fn unix_to_timestamp(secs: i64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: secs,
        nanos: 0,
    }
}

fn parse_release_urn(rest: &str) -> Result<String, ApiError> {
    let rest = rest.trim_start_matches('/');
    let Some(urn_raw) = rest.strip_suffix("/release") else {
        return Err(OrchestratorError::NotFound(format!("POST /v1/catalog/{rest}")).into());
    };
    Ok(percent_decode_path(urn_raw))
}

/// The version and the optional note. The note is trimmed and length-checked
/// in the store, so an over-long note is a 400.
fn parse_release_request(body: &str) -> Result<(Version, String), ApiError> {
    let req: ReleaseCatalogRequest = parse_json_body("vam.v1.ReleaseCatalogRequest", body)?;
    let version = req.version.ok_or_else(|| {
        OrchestratorError::InvalidContract("ReleaseCatalogRequest.version is required".into())
    })?;
    if !version.prerelease.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "release version must not include a prerelease suffix".into(),
        )
        .into());
    }
    Ok((version, req.note))
}
