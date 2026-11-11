//! Operator allowed-models HTTP: GET/PUT `/v1/node/allowed-models`.

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::Response;
use axum::routing::get;

use crate::allowed_models::require_scripted_mode_for_model;
use crate::api::auth::{require_idempotency_key, AuthContext};
use crate::api::error::{json_response, parse_json_body, ApiError};
use crate::api::{ensure_ready, json_http_response, AppState};
use crate::keys::BROKERS_ADMIN_SCOPE;
use vam_spec::v1::{
    GetOperatorAllowedModelsResponse, PutOperatorAllowedModelsRequest,
    PutOperatorAllowedModelsResponse,
};

pub fn allowed_models_route() -> axum::routing::MethodRouter<AppState> {
    get(get_allowed_models).put(put_allowed_models)
}

async fn get_allowed_models(
    State(state): State<AppState>,
    auth: AuthContext,
) -> Result<Response, ApiError> {
    state
        .pdp
        .authorize(&auth, "GetOperatorAllowedModels", "node:read")?;
    let (models, default) = state.store.node_model_settings()?;
    let catalog = vam_inference::ModelCatalog::new();
    let effective = crate::allowed_models::effective_default_model(
        &models,
        default.as_deref(),
        catalog.default_model(),
    );
    let resp = GetOperatorAllowedModelsResponse {
        models,
        default_model: default,
        effective_default_model: effective,
        default_gateway: state.store.get_operator_default_gateway()?,
    };
    let (code, json) = json_response("vam.v1.GetOperatorAllowedModelsResponse", &resp)?;
    Ok(json_http_response(code, json))
}

async fn put_allowed_models(
    State(state): State<AppState>,
    auth: AuthContext,
    headers: HeaderMap,
    body: String,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state
        .pdp
        .authorize(&auth, "PutOperatorAllowedModels", BROKERS_ADMIN_SCOPE)?;
    let _idempotency = require_idempotency_key(&headers).map_err(ApiError::from)?;
    let req: PutOperatorAllowedModelsRequest =
        parse_json_body("vam.v1.PutOperatorAllowedModelsRequest", &body)?;
    for model in &req.models {
        require_scripted_mode_for_model(model, state.e2e_mock_scripts)?;
    }
    if let Some(default) = req.default_model.as_deref() {
        require_scripted_mode_for_model(default, state.e2e_mock_scripts)?;
    }
    // Validated before anything is written, so a bad gateway leaves the models as they were.
    if let Some(gateway) = req.default_gateway.as_deref() {
        crate::allowed_models::validate_gateway(gateway)?;
    }
    let (models, default_model) = state
        .store
        .replace_operator_models(&req.models, req.default_model.as_deref())?;
    let default_gateway = match req.default_gateway.as_deref() {
        Some(g) => state.store.set_operator_default_gateway(g)?,
        None => state.store.get_operator_default_gateway()?,
    };
    let resp = PutOperatorAllowedModelsResponse {
        models,
        default_model,
        default_gateway,
    };
    let (code, json) = json_response("vam.v1.PutOperatorAllowedModelsResponse", &resp)?;
    Ok(json_http_response(code, json))
}
