use axum::extract::{DefaultBodyLimit, Multipart, Path, Query, State};
use axum::http::HeaderMap;
use axum::response::Response;

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::catalog_auth::{require_catalog_read, require_skill_package_release_authorization};
use crate::api::catalog_release;
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::release_suggestion;
use crate::api::{ensure_ready, json_http_response, percent_decode_path, AppState};
use crate::catalog_publish::ReleaseMeta;
use crate::catalog_suggest::SuggestKind;
use crate::error::OrchestratorError;
use crate::skill_package_admit::{admit_and_publish_skill_package_owned, MAX_SKILL_PACKAGE_BYTES};
use vam_spec::v1::{
    GetSkillPackageResponse, ListSkillPackagesResponse, PublishSkillPackageResponse,
    ReleaseSkillPackageRequest, ReleaseSkillPackageResponse, SkillChannel, Version,
};

pub fn skill_package_routes() -> axum::Router<AppState> {
    axum::Router::new()
        .route(
            "/v1/skills/packages",
            axum::routing::post(publish_skill_package).get(list_skill_packages),
        )
        .route(
            "/v1/skills/packages/*rest",
            axum::routing::get(get_skill_package_wildcard)
                .post(skill_package_wildcard_post)
                .delete(delete_skill_package_wildcard),
        )
        .layer(DefaultBodyLimit::max(MAX_SKILL_PACKAGE_BYTES))
}

async fn publish_skill_package(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;

    let mut archive_bytes: Option<Vec<u8>> = None;
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|e| OrchestratorError::InvalidContract(format!("multipart read failed: {e}")))?
    {
        if field.name() == Some("channel") {
            if let Ok(channel) = field.text().await {
                if !channel.is_empty() && channel != "draft" {
                    return Err(OrchestratorError::InvalidContract(
                        "publish channel must be draft".to_string(),
                    )
                    .into());
                }
            }
            continue;
        }
        if archive_bytes.is_some() {
            continue;
        }
        let bytes = field.bytes().await.map_err(|e| {
            OrchestratorError::InvalidContract(format!("multipart field read failed: {e}"))
        })?;
        if !bytes.is_empty() {
            archive_bytes = Some(bytes.to_vec());
        }
    }

    let archive_bytes = archive_bytes.ok_or_else(|| {
        OrchestratorError::InvalidContract(
            "skill package tar.gz file field is required".to_string(),
        )
    })?;

    let package = admit_and_publish_skill_package_owned(
        state.store.as_ref(),
        &archive_bytes,
        auth.owner_user_id(),
    )?;
    let resp = PublishSkillPackageResponse {
        package: Some(package),
    };
    let (code, json) = json_response("vam.v1.PublishSkillPackageResponse", &resp)?;
    let json = crate::api::with_skill_policy_warnings(
        json,
        resp.package
            .as_ref()
            .and_then(|package| package.spec.as_ref()),
    );
    Ok(json_http_response(code, json))
}

async fn list_skill_packages(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let mut packages = state
        .store
        .list_skill_packages_visible(auth.owner_user_id(), auth.has_users_admin())?;
    state.store.attach_skill_package_origins(&mut packages)?;
    let resp = ListSkillPackagesResponse {
        packages,
        pagination: None,
    };
    let (code, json) = json_response("vam.v1.ListSkillPackagesResponse", &resp)?;
    Ok(json_http_response(code, json))
}

#[derive(serde::Deserialize)]
struct SkillPackageQuery {
    channel: Option<String>,
    version: Option<String>,
}

async fn get_skill_package_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    Query(query): Query<SkillPackageQuery>,
) -> Result<Response, ApiError> {
    require_catalog_read(&auth)?;
    let rest = rest.trim_start_matches('/');
    if let Some(urn_raw) = rest.strip_suffix("/versions") {
        return catalog_release::versions_response(&state, &auth, "skill_packages", urn_raw);
    }
    if let Some(urn) = release_suggestion::suggestion_urn(rest) {
        return release_suggestion::release_suggestion(
            &state,
            &auth,
            SuggestKind::SkillPackage,
            &urn,
        );
    }
    let urn = percent_decode_path(rest);
    if urn.ends_with("/release") {
        return Err(OrchestratorError::NotFound(format!("GET /v1/skills/packages/{urn}")).into());
    }
    let requested = parse_channel_query(query.channel.as_deref())?;
    let version = parse_version_query(query.version.as_deref())?;
    // A version names a release. The draft has no version, so a versioned GET
    // never answers with the draft, even though `channel` defaults to draft.
    let channel = if version.is_some() {
        SkillChannel::Released
    } else {
        requested
    };
    let mut package = state.store.get_skill_package_for_query_visible(
        &urn,
        version.as_ref(),
        channel,
        auth.owner_user_id(),
        auth.has_users_admin(),
    )?;
    state
        .store
        .attach_skill_package_origins(std::slice::from_mut(&mut package))?;
    let resp = GetSkillPackageResponse {
        package: Some(package),
    };
    let (code, json) = json_response("vam.v1.GetSkillPackageResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn delete_skill_package_wildcard(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
) -> Result<Response, ApiError> {
    crate::api::refuse_catalog_delete(&state, &auth, &rest, "skill package").await
}

async fn skill_package_wildcard_post(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(rest): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutCatalog", "catalog:write")?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;

    let rest = rest.trim_start_matches('/');
    let Some(urn_raw) = rest.strip_suffix("/release") else {
        return Err(OrchestratorError::NotFound(format!("POST /v1/skills/packages/{rest}")).into());
    };
    let urn = percent_decode_path(urn_raw);
    require_skill_package_release_authorization(state.store.as_ref(), &auth, &urn)?;
    let req: ReleaseSkillPackageRequest =
        parse_json_body("vam.v1.ReleaseSkillPackageRequest", &body)?;
    let version = req.version.ok_or_else(|| {
        OrchestratorError::InvalidContract("ReleaseSkillPackageRequest.version is required".into())
    })?;
    if !version.prerelease.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "release version must not include a prerelease suffix".into(),
        )
        .into());
    }

    crate::node_base::check_draft_package(&state.store, &urn, auth.owner_user_id())?;
    let meta = ReleaseMeta {
        note: &req.note,
        released_by: Some(auth.owner_user_id()),
    };
    let package = state
        .store
        .release_skill_package_with_meta(&urn, &version, &meta)?;
    let resp = ReleaseSkillPackageResponse {
        package: Some(package),
    };
    let (code, json) = json_response("vam.v1.ReleaseSkillPackageResponse", &resp)?;
    Ok(json_http_response(code, json))
}

fn parse_channel_query(raw: Option<&str>) -> Result<SkillChannel, ApiError> {
    match raw.unwrap_or("draft").trim().to_ascii_lowercase().as_str() {
        "draft" => Ok(SkillChannel::Draft),
        "released" => Ok(SkillChannel::Released),
        other => Err(OrchestratorError::InvalidContract(format!(
            "channel must be 'draft' or 'released', got '{other}'"
        ))
        .into()),
    }
}

fn parse_version_query(raw: Option<&str>) -> Result<Option<Version>, ApiError> {
    let Some(raw) = raw else {
        return Ok(None);
    };
    let raw = raw.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = raw.split('.').collect();
    if parts.len() != 3 {
        return Err(OrchestratorError::InvalidContract(format!(
            "version must be X.Y.Z, got '{raw}'"
        ))
        .into());
    }
    let major = parts[0].parse::<i32>().map_err(|_| {
        OrchestratorError::InvalidContract(format!("invalid version major in '{raw}'"))
    })?;
    let minor = parts[1].parse::<i32>().map_err(|_| {
        OrchestratorError::InvalidContract(format!("invalid version minor in '{raw}'"))
    })?;
    let patch = parts[2].parse::<i32>().map_err(|_| {
        OrchestratorError::InvalidContract(format!("invalid version patch in '{raw}'"))
    })?;
    Ok(Some(Version {
        major,
        minor,
        patch,
        prerelease: String::new(),
        build: String::new(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_channel_defaults_to_draft() {
        assert_eq!(parse_channel_query(None).unwrap(), SkillChannel::Draft);
    }

    #[test]
    fn parse_version_triplet() {
        let version = parse_version_query(Some("1.2.3")).unwrap().unwrap();
        assert_eq!(version.major, 1);
        assert_eq!(version.minor, 2);
        assert_eq!(version.patch, 3);
    }
}
