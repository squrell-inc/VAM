use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::{json, Value};
use vam_spec::v1::VamErrorCode;

use crate::error::OrchestratorError;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: VamErrorCode,
    pub message: String,
    pub retryable: bool,
}

impl ApiError {
    #[allow(clippy::wrong_self_convention)] // consumes self by design; renaming would touch every caller
    fn as_parts(self) -> (StatusCode, Json<Value>) {
        (
            self.status,
            Json(json!({
                "code": format!("{:?}", self.code),
                "message": self.message,
                "retryable": self.retryable,
            })),
        )
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        self.as_parts().into_response()
    }
}

impl From<OrchestratorError> for ApiError {
    fn from(err: OrchestratorError) -> Self {
        match err {
            OrchestratorError::InvalidContract(msg) => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: VamErrorCode::InvalidArgument,
                message: msg,
                retryable: false,
            },
            OrchestratorError::NotFound(msg) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: VamErrorCode::NotFound,
                message: msg,
                retryable: false,
            },
            OrchestratorError::Conflict(msg) => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::AlreadyExists,
                message: msg,
                retryable: false,
            },
            // 409, not 422: the request is well formed and fixable by releasing the
            // named dependencies first, the same shape as InvalidState.
            err @ OrchestratorError::DependenciesNotReleased(_) => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::FailedPrecondition,
                message: err.to_string(),
                retryable: false,
            },
            OrchestratorError::Unauthenticated => ApiError {
                status: StatusCode::UNAUTHORIZED,
                code: VamErrorCode::Unauthenticated,
                message: "missing or invalid bearer token".to_string(),
                retryable: false,
            },
            OrchestratorError::PermissionDenied(msg) => ApiError {
                status: StatusCode::FORBIDDEN,
                code: VamErrorCode::PermissionDenied,
                message: msg,
                retryable: false,
            },
            OrchestratorError::NodeDraining => ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: VamErrorCode::FailedPrecondition,
                message: "node is draining; mutating requests are not accepted".to_string(),
                retryable: true,
            },
            OrchestratorError::MissingIdempotencyKey => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: VamErrorCode::InvalidArgument,
                message: "Idempotency-Key header is required for mutating requests".to_string(),
                retryable: false,
            },
            OrchestratorError::StorageUnavailable => ApiError {
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: VamErrorCode::Unavailable,
                message: "workflow storage is unavailable".to_string(),
                retryable: true,
            },
            OrchestratorError::ApprovalAlreadyDecided(id) => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::AlreadyExists,
                message: format!("approval '{}' already decided", id),
                retryable: false,
            },
            OrchestratorError::ApprovalNotFound(id) => ApiError {
                status: StatusCode::NOT_FOUND,
                code: VamErrorCode::NotFound,
                message: format!("approval '{}' not found", id),
                retryable: false,
            },
            OrchestratorError::StaleDecisionRevision { expected, actual } => ApiError {
                status: StatusCode::PRECONDITION_FAILED,
                code: VamErrorCode::Aborted,
                message: format!(
                    "decision revision mismatch: expected {}, got {}",
                    expected, actual
                ),
                retryable: true,
            },
            OrchestratorError::InvalidState { expected, actual } => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::FailedPrecondition,
                message: format!("invalid state: expected {}, got {}", expected, actual),
                retryable: false,
            },
            OrchestratorError::AttemptNotFound {
                step_id,
                attempt_id,
            } => ApiError {
                status: StatusCode::NOT_FOUND,
                code: VamErrorCode::NotFound,
                message: format!("{} '{}' not found", step_id, attempt_id),
                retryable: false,
            },
            OrchestratorError::LeaseExpired => ApiError {
                status: StatusCode::GATEWAY_TIMEOUT,
                code: VamErrorCode::DeadlineExceeded,
                message: "lease has expired".to_string(),
                retryable: false,
            },
            OrchestratorError::StaleLease => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::Aborted,
                message: "stale lease".to_string(),
                retryable: true,
            },
            OrchestratorError::AgentRunning {
                workspace_id,
                session_id,
                run_id,
            } => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::FailedPrecondition,
                message: format!(
                    "agent is running on workspace '{}' (session {}, run {})",
                    workspace_id, session_id, run_id
                ),
                retryable: true,
            },
            OrchestratorError::WorkspaceReserved {
                workspace_id,
                run_id,
                cancel_command,
            } => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::FailedPrecondition,
                message: format!(
                    "workspace '{workspace_id}' is reserved by parked run '{run_id}'; cancel it with: {cancel_command}"
                ),
                retryable: true,
            },
            OrchestratorError::InboxFull { run_id, pending } => ApiError {
                status: StatusCode::TOO_MANY_REQUESTS,
                code: VamErrorCode::ResourceExhausted,
                message: format!(
                    "inbox_full: run '{run_id}' already has {pending} undelivered messages"
                ),
                retryable: false,
            },
            OrchestratorError::AgentWorkspaceMismatch {
                workspace_id,
                bound,
                requested,
            } => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::FailedPrecondition,
                message: format!(
                    "agent '{}' does not match workspace-bound agent '{}' on workspace '{}'",
                    requested, bound, workspace_id
                ),
                retryable: false,
            },
            OrchestratorError::AgentWorkspaceVersionMismatch {
                workspace_id,
                bound,
                requested,
            } => ApiError {
                status: StatusCode::CONFLICT,
                code: VamErrorCode::FailedPrecondition,
                message: format!(
                    "agent version '{}' does not match workspace-bound version '{}' on workspace '{}'",
                    requested, bound, workspace_id
                ),
                retryable: false,
            },
            OrchestratorError::WorkspaceNotBound { workspace_id } => ApiError {
                status: StatusCode::BAD_REQUEST,
                code: VamErrorCode::InvalidArgument,
                message: format!(
                    "workspace '{}' is not bound to an agent; provide agentRef in run.json",
                    workspace_id
                ),
                retryable: false,
            },
            OrchestratorError::Security(e) => ApiError {
                status: StatusCode::FORBIDDEN,
                code: VamErrorCode::PermissionDenied,
                message: e.to_string(),
                retryable: false,
            },
            OrchestratorError::Hsp(msg) => ApiError {
                status: StatusCode::FORBIDDEN,
                code: VamErrorCode::PermissionDenied,
                message: msg,
                retryable: false,
            },
            OrchestratorError::Mcp(msg) => ApiError {
                status: StatusCode::BAD_GATEWAY,
                code: VamErrorCode::Unavailable,
                message: msg,
                retryable: true,
            },
            OrchestratorError::MissingSecretBinding { binding_id, scope } => ApiError {
                status: StatusCode::PRECONDITION_FAILED,
                code: VamErrorCode::FailedPrecondition,
                message: format!("missing secret binding '{binding_id}' (scope {scope})"),
                retryable: false,
            },
            OrchestratorError::MissingRequiredSecrets(missing) => ApiError {
                status: StatusCode::PRECONDITION_FAILED,
                code: VamErrorCode::FailedPrecondition,
                message: format!("admission failed: missing required secrets: {missing}"),
                retryable: false,
            },
            other => ApiError {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                code: VamErrorCode::Internal,
                message: other.to_string(),
                retryable: false,
            },
        }
    }
}

pub fn json_response<T: prost::Message>(
    proto_name: &str,
    msg: &T,
) -> Result<(StatusCode, String), ApiError> {
    let body =
        vam_spec::encoding::proto_to_json(proto_name, msg).map_err(OrchestratorError::Core)?;
    Ok((StatusCode::OK, body))
}

pub fn json_response_accepted<T: prost::Message>(
    proto_name: &str,
    msg: &T,
) -> Result<(StatusCode, String), ApiError> {
    let body =
        vam_spec::encoding::proto_to_json(proto_name, msg).map_err(OrchestratorError::Core)?;
    Ok((StatusCode::ACCEPTED, body))
}

pub fn parse_json_body<T: prost::Message + Default>(
    proto_name: &str,
    body: &str,
) -> Result<T, ApiError> {
    vam_spec::encoding::json_to_proto(proto_name, body)
        .map_err(|e| OrchestratorError::InvalidContract(e).into())
}

#[cfg(test)]
mod inbox_error_tests {
    use super::*;

    #[test]
    fn inbox_full_is_429_resource_exhausted() {
        let e = ApiError::from(OrchestratorError::InboxFull {
            run_id: "urn:vam:run:local.vam.dev/acme/r1".into(),
            pending: 20,
        });
        assert_eq!(e.status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(e.code, VamErrorCode::ResourceExhausted);
        assert!(e.message.starts_with("inbox_full:"));
        assert!(!e.retryable);
    }
}
