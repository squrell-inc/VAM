//! Drive9 HEAD workspace file list, download, and upload (`/v1/workspaces/:id/files`).

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::header::{CONTENT_DISPOSITION, CONTENT_TYPE};
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use base64::Engine;
use serde::{Deserialize, Serialize};
use vam_spec::v1::VamErrorCode;
use vam_storage::{
    list_workspace_files_from_head, put_workspace_file_to_head, read_workspace_file_from_head,
    Drive9Error, Drive9Layout, DEFAULT_MAX_LIST_FILES, MAX_DOWNLOAD_BYTES,
};

use crate::api::auth::AuthContext;
use crate::api::error::ApiError;
use crate::api::{drive9_root, ensure_ready, json_http_response, percent_decode_path, AppState};
use crate::error::OrchestratorError;

const SCOPE: &str = "sessions:write";

/// HTTP body cap for JSON base64 uploads (~32 MiB decoded plus encoding overhead).
const MAX_UPLOAD_HTTP_BODY_BYTES: usize = ((MAX_DOWNLOAD_BYTES as u128 * 4 / 3) + 8192) as usize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PutJsonBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, alias = "media_type")]
    media_type: Option<String>,
    base64: String,
}

fn decode_upload_body(content_type: Option<&str>, body: &Bytes) -> Result<Vec<u8>, ApiError> {
    let is_json = content_type
        .and_then(|v| v.split(';').next())
        .map(str::trim)
        .is_some_and(|v| v.eq_ignore_ascii_case("application/json"));
    if !is_json {
        return Ok(body.to_vec());
    }

    let payload: PutJsonBody = serde_json::from_slice(body)
        .map_err(|e| OrchestratorError::InvalidContract(format!("upload JSON body: {e}")))?;
    let _ = (payload.name, payload.media_type);

    base64::engine::general_purpose::STANDARD
        .decode(payload.base64.trim())
        .map_err(|e| OrchestratorError::InvalidContract(format!("invalid base64: {e}")).into())
}

pub fn list_route() -> axum::routing::MethodRouter<AppState> {
    get(list_workspace_files)
}

pub fn path_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/workspaces/:id/files/*path",
            get(download_workspace_file).put(put_workspace_file),
        )
        .layer(DefaultBodyLimit::max(MAX_UPLOAD_HTTP_BODY_BYTES))
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ListResponse<'a> {
    workspace_id: &'a str,
    head: &'a str,
    files: Vec<FileView>,
    truncated: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PutResponse<'a> {
    workspace_id: &'a str,
    path: &'a str,
    head: &'a str,
    size_bytes: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FileView {
    path: String,
    size_bytes: u64,
    mode: u32,
}

fn map_drive9_error(err: Drive9Error) -> ApiError {
    match err {
        Drive9Error::NotFound(msg) => OrchestratorError::NotFound(msg).into(),
        Drive9Error::FileTooLarge { size, max } => ApiError {
            status: StatusCode::PAYLOAD_TOO_LARGE,
            code: VamErrorCode::InvalidArgument,
            message: format!("file size {size} bytes exceeds maximum {max} bytes"),
            retryable: false,
        },
        Drive9Error::SandboxViolation { .. }
        | Drive9Error::InvalidPathCharacters(_)
        | Drive9Error::Layout(_) => OrchestratorError::InvalidContract(err.to_string()).into(),
        other => OrchestratorError::Core(other.to_string()).into(),
    }
}

/// Echo a caller-supplied full workspace id. A bare name stays the guest suffix
/// so existing clients that send `assistant` still see `assistant`.
fn display_workspace_id(guest_workspace_id: &str, resolved: &str) -> String {
    let guest = guest_workspace_id.trim().trim_end_matches('/');
    let guest = guest.rsplit('/').next().unwrap_or(guest);
    if guest == resolved {
        return resolved.to_string();
    }
    vam_storage::guest_id_suffix(guest_workspace_id)
        .unwrap_or_else(|_| guest_workspace_id.to_string())
}

fn attachment_basename(manifest_path: &str) -> String {
    let name = manifest_path.rsplit('/').next().unwrap_or("download");
    name.chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\' && *c != '/')
        .collect()
}

async fn list_workspace_files(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(guest_workspace_id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "ListWorkspaceFiles", SCOPE)?;

    let workspace_id =
        crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &guest_workspace_id)?;
    let layout = Drive9Layout::new(drive9_root());
    let listing = list_workspace_files_from_head(&layout, &workspace_id, DEFAULT_MAX_LIST_FILES)
        .map_err(map_drive9_error)?;

    let files = listing
        .files
        .into_iter()
        .map(|f| FileView {
            path: f.path,
            size_bytes: f.size_bytes,
            mode: f.mode,
        })
        .collect();

    let workspace_id_display = display_workspace_id(&guest_workspace_id, &workspace_id);

    let body = serde_json::to_string(&ListResponse {
        workspace_id: &workspace_id_display,
        head: &listing.head,
        files,
        truncated: listing.truncated,
    })
    .map_err(|e| OrchestratorError::Core(format!("list workspace files json: {e}")))?;

    Ok(json_http_response(StatusCode::OK, body))
}

async fn download_workspace_file(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((guest_workspace_id, rest)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "GetWorkspaceFile", SCOPE)?;

    let workspace_id =
        crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &guest_workspace_id)?;
    let rel_path = percent_decode_path(rest.trim_start_matches('/'));
    let normalized =
        vam_storage::normalize_workspace_file_path(&rel_path).map_err(map_drive9_error)?;
    let layout = Drive9Layout::new(drive9_root());
    let bytes =
        read_workspace_file_from_head(&layout, &workspace_id, &rel_path, MAX_DOWNLOAD_BYTES)
            .map_err(map_drive9_error)?;

    let filename = attachment_basename(&normalized);
    let disposition = format!("attachment; filename=\"{filename}\"");
    let disposition_header = HeaderValue::from_str(&disposition)
        .map_err(|e| OrchestratorError::Core(format!("content-disposition header: {e}")))?;

    Ok((
        StatusCode::OK,
        [
            (
                CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (CONTENT_DISPOSITION, disposition_header),
        ],
        bytes,
    )
        .into_response())
}

async fn put_workspace_file(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((guest_workspace_id, rest)): Path<(String, String)>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    ensure_ready(&state, true)?;
    state.pdp.authorize(&auth, "PutWorkspaceFile", SCOPE)?;

    let workspace_id =
        crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &guest_workspace_id)?;
    if let Some(active) = super::find_active_session_on_workspace(&state.store, &workspace_id)? {
        let run_id = state
            .store
            .find_run_id_for_session(&active.session_id)?
            .unwrap_or_else(|| "unknown".to_string());
        return Err(OrchestratorError::AgentRunning {
            workspace_id,
            session_id: active.session_id,
            run_id,
        }
        .into());
    }

    let content_type = headers.get(CONTENT_TYPE).and_then(|v| v.to_str().ok());
    let bytes = decode_upload_body(content_type, &body)?;

    let rel_path = percent_decode_path(rest.trim_start_matches('/'));
    let layout = Drive9Layout::new(drive9_root());
    let put = put_workspace_file_to_head(&layout, &workspace_id, &rel_path, &bytes)
        .map_err(map_drive9_error)?;

    let workspace_id_display = display_workspace_id(&guest_workspace_id, &workspace_id);
    let json = serde_json::to_string(&PutResponse {
        workspace_id: &workspace_id_display,
        path: &put.path,
        head: &put.head,
        size_bytes: put.size_bytes,
    })
    .map_err(|e| OrchestratorError::Core(format!("put workspace file json: {e}")))?;

    Ok(json_http_response(StatusCode::OK, json))
}
