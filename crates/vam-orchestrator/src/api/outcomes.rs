//! `GET /v1/agents/{urn}/outcomes`: what an agent version's stored receipts add up to.
//!
//! Auth is the `versions` route's: any catalog reader sees released versions; the draft is
//! shown only to its owner or a caller with `users:admin`, and is a 404 for anyone else.

use axum::response::Response;
use serde_json::{json, Map, Value};
use vam_spec::v1::SkillChannel;

use crate::api::auth::AuthContext;
use crate::api::catalog_auth::require_catalog_read;
use crate::api::error::ApiError;
use crate::api::{json_http_response, percent_decode_path, AppState};
use crate::error::OrchestratorError;
use crate::outcomes::{aggregate, base_version, changes, list_versions, select, Group, DEFAULT_K};

/// The urn part of `<urn>/outcomes` or `<urn>/outcomes/diff` (second value `true`).
pub(crate) fn outcomes_urn(rest: &str) -> Option<(&str, bool)> {
    let rest = rest.trim_start_matches('/');
    match rest.strip_suffix("/outcomes/diff") {
        Some(urn) => Some((urn, true)),
        None => rest.strip_suffix("/outcomes").map(|urn| (urn, false)),
    }
}

/// `version` is `X.Y.Z` or `draft`; `k` is the pass^k size.
pub(crate) fn outcomes_response(
    state: &AppState,
    auth: &AuthContext,
    urn_raw: &str,
    version: Option<&str>,
    k: Option<&str>,
    diff: bool,
) -> Result<Response, ApiError> {
    require_catalog_read(auth)?;
    let urn = percent_decode_path(urn_raw);
    let k = parse_k(k)?;
    // The caller's visible versions: releases for everyone, the draft for its owner or admin.
    let rows = state.store.list_catalog_versions(
        "agents",
        &urn,
        auth.owner_user_id(),
        auth.has_users_admin(),
    )?;
    // Same answer as `versions` for an agent the caller cannot see (the store already
    // 404s on no rows; kept explicit so the listing never leans on that alone).
    if rows.is_empty() {
        return Err(OrchestratorError::NotFound(format!(
            "agents '{urn}' has no versions visible to this caller"
        ))
        .into());
    }
    let draft_digest = rows
        .iter()
        .find(|r| r.channel == SkillChannel::Draft)
        .map(|r| r.digest.clone());
    let facts = state.store.receipt_facts_for_agent(&urn)?;
    let catalog = vam_inference::ModelCatalog::new();

    if diff {
        let candidate = version.ok_or_else(|| {
            OrchestratorError::InvalidContract("diff needs ?version=X.Y.Z or draft".into())
        })?;
        let releases: Vec<String> = rows
            .iter()
            .filter(|r| r.channel == SkillChannel::Released)
            .map(label)
            .collect();
        let pick = |v: &str| -> Result<Value, OrchestratorError> {
            let runs = if v == "draft" {
                let digest = draft_digest.as_deref().ok_or_else(|| {
                    OrchestratorError::NotFound(format!(
                        "agents '{urn}' has no draft visible to you"
                    ))
                })?;
                select(&facts, Group::Draft(digest))
            } else {
                select(&facts, Group::Release(v))
            };
            Ok(with_version(aggregate(&runs, k, &catalog), v))
        };
        if candidate != "draft" {
            validate_version(candidate)?;
            if !releases.iter().any(|r| r == candidate) {
                return Err(OrchestratorError::NotFound(format!(
                    "agents '{urn}' has no released version {candidate}"
                ))
                .into());
            }
        }
        let cand = pick(candidate)?;
        let mut body = Map::new();
        if let Some(b) = base_version(candidate, &releases) {
            let base = pick(&b)?;
            body.insert("changes".into(), changes(&base, &cand));
            body.insert("base".into(), base);
        } else {
            body.insert("changes".into(), json!({}));
        }
        body.insert("candidate".into(), cand);
        return Ok(json_http_response(
            axum::http::StatusCode::OK,
            Value::Object(body).to_string(),
        ));
    }

    let body: Value = match version {
        None => {
            // Only releases in the caller's visible catalog rows; receipts of a version
            // the catalog no longer shows them must not be counted.
            let visible: Vec<String> = rows
                .iter()
                .filter(|r| r.channel == SkillChannel::Released)
                .map(label)
                .collect();
            let facts: Vec<_> = facts
                .into_iter()
                .filter(|f| f.is_draft() || visible.contains(&f.version))
                .collect();
            json!({ "versions": list_versions(&facts, draft_digest.as_deref()) })
        }
        Some("draft") => {
            let digest = draft_digest.ok_or_else(|| {
                OrchestratorError::NotFound(format!("agents '{urn}' has no draft visible to you"))
            })?;
            let runs = select(&facts, Group::Draft(&digest));
            with_version(aggregate(&runs, k, &catalog), "draft")
        }
        Some(v) => {
            validate_version(v)?;
            let released = rows
                .iter()
                .any(|r| r.channel == SkillChannel::Released && label(r) == v);
            if !released {
                return Err(OrchestratorError::NotFound(format!(
                    "agents '{urn}' has no released version {v}"
                ))
                .into());
            }
            let runs = select(&facts, Group::Release(v));
            with_version(aggregate(&runs, k, &catalog), v)
        }
    };
    Ok(json_http_response(
        axum::http::StatusCode::OK,
        body.to_string(),
    ))
}

fn label(row: &crate::catalog::CatalogVersionRow) -> String {
    format!(
        "{}.{}.{}",
        row.version.major, row.version.minor, row.version.patch
    )
}

fn with_version(mut aggregate: Value, version: &str) -> Value {
    if let Some(o) = aggregate.as_object_mut() {
        o.insert("version".into(), json!(version));
    }
    aggregate
}

fn parse_k(k: Option<&str>) -> Result<u32, OrchestratorError> {
    match k {
        None => Ok(DEFAULT_K),
        Some(s) => match s.parse::<u32>() {
            Ok(n) if (1..=1000).contains(&n) => Ok(n),
            _ => Err(OrchestratorError::InvalidContract(
                "k must be an integer from 1 to 1000".into(),
            )),
        },
    }
}

fn validate_version(v: &str) -> Result<(), OrchestratorError> {
    let ok = v.split('.').count() == 3 && v.split('.').all(|p| p.parse::<u64>().is_ok());
    if ok {
        Ok(())
    } else {
        Err(OrchestratorError::InvalidContract(
            "version must be X.Y.Z or draft".into(),
        ))
    }
}
