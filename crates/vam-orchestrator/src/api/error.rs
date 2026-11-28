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
    /// Typed denials that explain the refusal (empty for most errors). The body's `denial` is
    /// the first and `denials` is the list; the message text is the same either way.
    pub denials: Vec<vam_spec::v1::Denial>,
}

impl ApiError {
    #[allow(clippy::wrong_self_convention)] // consumes self by design; renaming would touch every caller
    fn as_parts(self) -> (StatusCode, Json<Value>) {
        let mut body = json!({
            "code": format!("{:?}", self.code),
            "message": self.message,
            "retryable": self.retryable,
        });
        // A denial the encoder cannot write is left out: the refusal itself is unchanged.
        let denials: Vec<Value> = self
            .denials
            .iter()
            .filter_map(crate::denials::denial_json)
            .collect();
        if let (Some(first), Some(object)) = (denials.first().cloned(), body.as_object_mut()) {
            object.insert("denial".to_string(), first);
            object.insert("denials".to_string(), Value::Array(denials));
        }
        (self.status, Json(body))
    }

    /// The same error with typed denials attached.
    pub fn with_denials(mut self, denials: Vec<vam_spec::v1::Denial>) -> Self {
        self.denials = denials;
        self
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
                denials: Vec::new(),
                status: StatusCode::BAD_REQUEST,
                code: VamErrorCode::InvalidArgument,
                message: msg,
                retryable: false,
            },
            OrchestratorError::NotFound(msg) => ApiError {
                denials: Vec::new(),
                status: StatusCode::NOT_FOUND,
                code: VamErrorCode::NotFound,
                message: msg,
                retryable: false,
            },
            OrchestratorError::Conflict(msg) => ApiError {
                denials: Vec::new(),
                status: StatusCode::CONFLICT,
                code: VamErrorCode::AlreadyExists,
                message: msg,
                retryable: false,
            },
            // 409, not 422: the request is well formed and fixable by releasing the
            // named dependencies first, the same shape as InvalidState.
            err @ OrchestratorError::DependenciesNotReleased(_) => ApiError {
                denials: Vec::new(),
                status: StatusCode::CONFLICT,
                code: VamErrorCode::FailedPrecondition,
                message: err.to_string(),
                retryable: false,
            },
            OrchestratorError::Unauthenticated => ApiError {
                denials: Vec::new(),
                status: StatusCode::UNAUTHORIZED,
                code: VamErrorCode::Unauthenticated,
                message: "missing or invalid bearer token".to_string(),
                retryable: false,
            },
            OrchestratorError::PermissionDenied(msg) => ApiError {
                denials: Vec::new(),
                status: StatusCode::FORBIDDEN,
                code: VamErrorCode::PermissionDenied,
                message: msg,
                retryable: false,
            },
            OrchestratorError::NodeDraining => ApiError {
                denials: Vec::new(),
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: VamErrorCode::FailedPrecondition,
                message: "node is draining; mutating requests are not accepted".to_string(),
                retryable: true,
            },
            OrchestratorError::MissingIdempotencyKey => ApiError {
                denials: Vec::new(),
                status: StatusCode::BAD_REQUEST,
                code: VamErrorCode::InvalidArgument,
                message: "Idempotency-Key header is required for mutating requests".to_string(),
                retryable: false,
            },
            OrchestratorError::StorageUnavailable => ApiError {
                denials: Vec::new(),
                status: StatusCode::SERVICE_UNAVAILABLE,
                code: VamErrorCode::Unavailable,
                message: "workflow storage is unavailable".to_string(),
                retryable: true,
            },
            OrchestratorError::ApprovalAlreadyDecided(id) => ApiError {
                denials: Vec::new(),
                status: StatusCode::CONFLICT,
                code: VamErrorCode::AlreadyExists,
                message: format!("approval '{}' already decided", id),
                retryable: false,
            },
            OrchestratorError::ApprovalNotFound(id) => ApiError {
                denials: Vec::new(),
                status: StatusCode::NOT_FOUND,
                code: VamErrorCode::NotFound,
                message: format!("approval '{}' not found", id),
                retryable: false,
            },
            OrchestratorError::StaleDecisionRevision { expected, actual } => ApiError {
                denials: Vec::new(),
                status: StatusCode::PRECONDITION_FAILED,
                code: VamErrorCode::Aborted,
                message: format!(
                    "decision revision mismatch: expected {}, got {}",
                    expected, actual
                ),
                retryable: true,
            },
            OrchestratorError::InvalidState { expected, actual } => ApiError {
                denials: Vec::new(),
                status: StatusCode::CONFLICT,
                code: VamErrorCode::FailedPrecondition,
                message: format!("invalid state: expected {}, got {}", expected, actual),
                retryable: false,
            },
            OrchestratorError::AttemptNotFound {
                step_id,
                attempt_id,
            } => ApiError {
                denials: Vec::new(),
                status: StatusCode::NOT_FOUND,
                code: VamErrorCode::NotFound,
                message: format!("{} '{}' not found", step_id, attempt_id),
                retryable: false,
            },
            OrchestratorError::LeaseExpired => ApiError {
                denials: Vec::new(),
                status: StatusCode::GATEWAY_TIMEOUT,
                code: VamErrorCode::DeadlineExceeded,
                message: "lease has expired".to_string(),
                retryable: false,
            },
            OrchestratorError::StaleLease => ApiError {
                denials: Vec::new(),
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
                denials: Vec::new(),
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
                denials: Vec::new(),
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
                denials: vec![crate::denials::inbox_full_denial(&run_id)],
            },
            OrchestratorError::AgentWorkspaceMismatch {
                workspace_id,
                bound,
                requested,
            } => ApiError {
                denials: Vec::new(),
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
                denials: vec![crate::denials::version_stale_denial(&bound)],
            },
            OrchestratorError::WorkspaceNotBound { workspace_id } => ApiError {
                denials: Vec::new(),
                status: StatusCode::BAD_REQUEST,
                code: VamErrorCode::InvalidArgument,
                message: format!(
                    "workspace '{}' is not bound to an agent; provide agentRef in run.json",
                    workspace_id
                ),
                retryable: false,
            },
            OrchestratorError::Security(e) => ApiError {
                denials: Vec::new(),
                status: StatusCode::FORBIDDEN,
                code: VamErrorCode::PermissionDenied,
                message: e.to_string(),
                retryable: false,
            },
            OrchestratorError::Hsp(msg) => ApiError {
                denials: Vec::new(),
                status: StatusCode::FORBIDDEN,
                code: VamErrorCode::PermissionDenied,
                message: msg,
                retryable: false,
            },
            OrchestratorError::Mcp(msg) => ApiError {
                denials: Vec::new(),
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
                denials: vec![crate::denials::secret_missing_plain(&binding_id, &scope)],
            },
            OrchestratorError::MissingRequiredSecrets(missing) => ApiError {
                status: StatusCode::PRECONDITION_FAILED,
                code: VamErrorCode::FailedPrecondition,
                message: format!("admission failed: missing required secrets: {missing}"),
                retryable: false,
                denials: missing
                    .items
                    .iter()
                    .map(|(binding, scope)| crate::denials::secret_missing_plain(binding, scope))
                    .collect(),
            },
            other => ApiError {
                denials: Vec::new(),
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

#[cfg(test)]
mod denial_body_tests {
    use super::*;

    async fn body_of(error: ApiError) -> (StatusCode, Value) {
        let response = error.into_response();
        let status = response.status();
        let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
            .await
            .expect("body");
        (status, serde_json::from_slice(&bytes).expect("json"))
    }

    #[tokio::test]
    async fn missing_required_secrets_carry_one_denial_per_secret_and_the_same_message() {
        let missing = crate::error::MissingSecrets {
            text: "jira@user, openai@org".to_string(),
            items: vec![
                ("jira".to_string(), "user".to_string()),
                ("openai".to_string(), "org".to_string()),
            ],
        };
        let error = ApiError::from(OrchestratorError::MissingRequiredSecrets(missing));
        assert_eq!(
            error.message,
            "admission failed: missing required secrets: jira@user, openai@org"
        );
        let (status, body) = body_of(error).await;
        assert_eq!(status, StatusCode::PRECONDITION_FAILED);
        assert_eq!(body["denial"]["reason"], "SECRET_MISSING");
        assert_eq!(body["denial"]["subject"], "jira");
        assert_eq!(body["denial"]["fix"]["change"], "SET_SECRET");
        assert_eq!(body["denial"]["fix"]["who"], "SELF");
        let list = body["denials"].as_array().expect("denials");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0], body["denial"]);
        assert_eq!(list[1]["fix"]["who"], "ORG_ADMIN");
        // Without a caller no row exists, so no reference.
        assert!(list[1].get("decisionRef").is_none());
    }

    #[tokio::test]
    async fn an_error_without_denials_has_no_denial_keys() {
        let (_, body) = body_of(ApiError::from(OrchestratorError::LeaseExpired)).await;
        assert!(body.get("denial").is_none() && body.get("denials").is_none());
        assert_eq!(body["code"], "DeadlineExceeded");
    }

    #[tokio::test]
    async fn a_stale_version_says_to_run_on_the_bound_one() {
        let (status, body) = body_of(ApiError::from(
            OrchestratorError::AgentWorkspaceVersionMismatch {
                workspace_id: "w".into(),
                bound: "1.0.0".into(),
                requested: "2.0.0".into(),
            },
        ))
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["denial"]["reason"], "VERSION_STALE");
        assert_eq!(body["denial"]["subject"], "1.0.0");
        assert_eq!(body["denial"]["fix"]["who"], "SELF");
        assert_eq!(body["denial"]["fix"]["callerCanApply"], true);
        assert!(body["denial"]["fix"].get("change").is_none());
        assert_eq!(
            body["denial"]["text"],
            "Run again on version 1.0.0, or omit the version."
        );
        assert_eq!(
            body["message"],
            "agent version '2.0.0' does not match workspace-bound version '1.0.0' on workspace 'w'"
        );
    }

    #[tokio::test]
    async fn a_full_inbox_is_a_quota_with_no_fix() {
        let (status, body) = body_of(ApiError::from(OrchestratorError::InboxFull {
            run_id: "urn:vam:run:local.vam.dev/acme/r1".into(),
            pending: 20,
        }))
        .await;
        assert_eq!(status, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(body["denial"]["reason"], "QUOTA");
        assert!(body["denial"].get("fix").is_none());
        assert!(body["denial"]["text"]
            .as_str()
            .is_some_and(|t| t.contains("org admin")));
    }
}
