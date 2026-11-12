//! `/v1/marketplaces`: the Git marketplaces this node trusts (CN-4.4)
//! and the installs from them (CN-4.5).
//!
//! Every route needs `users:admin`; a member gets 403 on all of them (including the list). The
//! routes that only read (list, refresh, listings) install nothing (M1); installing is the explicit
//! `POST …/listings/{listing}/install` (and `…/update` for a newer tag) of an admin. Refresh
//! answers 200 with `lastError` set on that marketplace when its index or network is broken, and
//! 409 when it refuses to follow a tag that moved or a publisher domain that changed.
//! Install and update answer 200 `{ installed: { ref, commit, changed, artifacts } }`; 400 for a
//! refused listing (a connector in an Unverified marketplace, a URN under a reserved domain, a
//! package that fails its check) and 409 for a moved or missing tag or a clash with existing
//! catalog content.

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::ApiError;
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::error::OrchestratorError;
use crate::keys::USERS_ADMIN_SCOPE;
use crate::marketplace::{self, fetch::valid_id};
use vam_spec::v1::VamErrorCode;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/marketplaces",
            get(list_marketplaces).post(add_marketplace),
        )
        .route(
            "/v1/marketplaces/:id",
            axum::routing::put(put_marketplace).delete(delete_marketplace),
        )
        .route("/v1/marketplaces/:id/refresh", post(refresh_marketplace))
        .route("/v1/marketplaces/:id/listings", get(list_listings))
        .route(
            "/v1/marketplaces/:id/listings/:listing/install",
            post(install_listing),
        )
        .route(
            "/v1/marketplaces/:id/listings/:listing/update",
            post(update_listing),
        )
}

/// The test-only route a process-guest node adds so the runner can return the node's marketplace
/// state to its boot state between scenarios (`POST` as an admin). It exists only in a build with
/// the `process-guest` feature and answers only while process mode is active; the release gate
/// proves the route is absent from a default `vamd`.
#[cfg(feature = "process-guest")]
pub fn process_routes() -> Router<AppState> {
    Router::new().route(PROCESS_RESET_ROUTE, post(reset_installs))
}

#[cfg(feature = "process-guest")]
pub const PROCESS_RESET_ROUTE: &str = "/v1/process/marketplace-reset";

#[cfg(feature = "process-guest")]
async fn reset_installs(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "ResetMarketplaceInstalls", USERS_ADMIN_SCOPE)?;
    if !crate::process_mode::is_active() {
        return Err(OrchestratorError::NotFound(PROCESS_RESET_ROUTE.to_string()).into());
    }
    let removed = state.store.reset_marketplace_installs()?;
    Ok(json_http_response(
        StatusCode::OK,
        json!({ "removedRows": removed }).to_string(),
    ))
}

/// Optional body of an install or update: the tag the client believes it is installing (the CLI's
/// `--ref`). The node installs the tag its last read of the index named; a different one is a 409.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    #[serde(rename = "ref")]
    git_ref: Option<String>,
}

async fn install_listing(
    state: State<AppState>,
    auth: AuthContext,
    path: Path<(String, String)>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    install(
        state,
        auth,
        path,
        headers,
        body,
        "InstallMarketplaceListing",
        marketplace::import::Mode::Install,
    )
    .await
}

async fn update_listing(
    state: State<AppState>,
    auth: AuthContext,
    path: Path<(String, String)>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    install(
        state,
        auth,
        path,
        headers,
        body,
        "UpdateMarketplaceListing",
        marketplace::import::Mode::Update,
    )
    .await
}

async fn install(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((id, listing)): Path<(String, String)>,
    headers: HeaderMap,
    body: String,
    action: &'static str,
    mode: marketplace::import::Mode,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, action, USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    known(&id)?;
    let req: InstallRequest = if body.trim().is_empty() {
        InstallRequest::default()
    } else {
        parse(&body, "{ ref? }")?
    };
    let cfg = marketplace::config()?;
    let _guard = marketplace::lock_marketplace(&id).await;
    let (store, mid, lid, by) = (
        state.store.clone(),
        id.clone(),
        listing.clone(),
        auth.owner_user_id().to_string(),
    );
    let done = tokio::task::spawn_blocking(move || {
        marketplace::import::install_blocking(
            &store,
            cfg,
            &mid,
            &lid,
            mode,
            req.git_ref.as_deref(),
            &by,
        )
    })
    .await
    .map_err(blocking_failed)??;
    Ok(json_http_response(
        StatusCode::OK,
        json!({
            "marketplace": id,
            "listing": done.listing_id,
            "installed": {
                "ref": done.git_ref,
                "commit": done.commit,
                "changed": done.changed,
                "capabilitiesChanged": done.capabilities_changed,
                "artifacts": done.artifacts.iter().map(|a| json!({
                    "kind": a.kind, "urn": a.urn, "version": a.version, "digest": a.digest,
                })).collect::<Vec<_>>(),
            },
        })
        .to_string(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct AddRequest {
    url: String,
    publisher_domain: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PutRequest {
    enabled: bool,
}

fn parse<T: serde::de::DeserializeOwned>(body: &str, shape: &str) -> Result<T, OrchestratorError> {
    // A fixed message: a serde error can quote the value it rejected.
    serde_json::from_str(body)
        .map_err(|_| OrchestratorError::InvalidContract(format!("body must be {shape}")))
}

fn known(id: &str) -> Result<(), OrchestratorError> {
    if valid_id(id) {
        Ok(())
    } else {
        Err(OrchestratorError::NotFound(format!("marketplace `{id}`")))
    }
}

fn blocking_failed(e: tokio::task::JoinError) -> ApiError {
    tracing::error!("marketplace task failed: {e}");
    OrchestratorError::StorageUnavailable.into()
}

async fn list_marketplaces(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state
        .pdp
        .authorize(&auth, "ListMarketplaces", USERS_ADMIN_SCOPE)?;
    let rows = state.store.marketplaces()?;
    let git = marketplace::config()
        .map(|c| c.fetcher.git_bin.is_some())
        .unwrap_or(false);
    Ok(json_http_response(
        StatusCode::OK,
        json!({
            "marketplaces": rows.iter().map(marketplace::marketplace_view).collect::<Vec<_>>(),
            "gitAvailable": git,
        })
        .to_string(),
    ))
}

async fn add_marketplace(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "AddMarketplace", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: AddRequest = parse(&body, "{ url, publisherDomain }")?;
    let cfg = marketplace::config()?;
    let (store, by) = (state.store.clone(), auth.owner_user_id().to_string());
    let row = tokio::task::spawn_blocking(move || {
        marketplace::add_blocking(
            &store,
            cfg,
            &req.url,
            req.publisher_domain.as_deref().unwrap_or(""),
            &by,
        )
    })
    .await
    .map_err(blocking_failed)??;
    Ok(json_http_response(
        StatusCode::CREATED,
        json!({ "marketplace": marketplace::marketplace_view(&row) }).to_string(),
    ))
}

async fn put_marketplace(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutMarketplace", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    known(&id)?;
    let req: PutRequest = parse(&body, "{ enabled }")?;
    let _guard = marketplace::lock_marketplace(&id).await;
    if !state.store.set_marketplace_enabled(&id, req.enabled)? {
        return Err(OrchestratorError::NotFound(format!("marketplace `{id}`")).into());
    }
    let row = state
        .store
        .marketplace(&id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("marketplace `{id}`")))?;
    Ok(json_http_response(
        StatusCode::OK,
        json!({ "marketplace": marketplace::marketplace_view(&row) }).to_string(),
    ))
}

async fn delete_marketplace(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "DeleteMarketplace", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    known(&id)?;
    let cfg = marketplace::config()?;
    let _guard = marketplace::lock_marketplace(&id).await;
    let (store, mid) = (state.store.clone(), id.clone());
    tokio::task::spawn_blocking(move || marketplace::delete_blocking(&store, cfg, &mid))
        .await
        .map_err(blocking_failed)??;
    Ok(json_http_response(
        StatusCode::OK,
        json!({ "id": id, "removed": true }).to_string(),
    ))
}

async fn refresh_marketplace(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "RefreshMarketplace", USERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    known(&id)?;
    let cfg = marketplace::config()?;
    let _guard = marketplace::lock_marketplace(&id).await;
    let (store, mid) = (state.store.clone(), id.clone());
    let done =
        tokio::task::spawn_blocking(move || marketplace::refresh_blocking(&store, cfg, &mid))
            .await
            .map_err(blocking_failed)??;
    if let Some(why) = done.refused {
        return Err(ApiError {
            status: StatusCode::CONFLICT,
            code: VamErrorCode::FailedPrecondition,
            message: why,
            retryable: false,
        });
    }
    Ok(json_http_response(
        StatusCode::OK,
        json!({ "marketplace": marketplace::marketplace_view(&done.row) }).to_string(),
    ))
}

async fn list_listings(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state
        .pdp
        .authorize(&auth, "ListMarketplaceListings", USERS_ADMIN_SCOPE)?;
    known(&id)?;
    let row = state
        .store
        .marketplace(&id)?
        .ok_or_else(|| OrchestratorError::NotFound(format!("marketplace `{id}`")))?;
    let installs = state.store.marketplace_installs(&id)?;
    let listings: Value = marketplace::listings_view_with(&row, &installs);
    Ok(json_http_response(
        StatusCode::OK,
        json!({ "listings": listings }).to_string(),
    ))
}

#[cfg(test)]
#[path = "marketplaces_tests.rs"]
mod tests;
