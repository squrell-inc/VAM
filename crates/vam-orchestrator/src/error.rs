use thiserror::Error;
use vam_security::SecurityError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptState {
    Queued,
    Running,
    Suspending,
    Suspended,
    Succeeded,
    Failed,
    Cancelled,
}

impl AttemptState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "QUEUED",
            Self::Running => "RUNNING",
            Self::Suspending => "SUSPENDING",
            Self::Suspended => "SUSPENDED",
            Self::Succeeded => "SUCCEEDED",
            Self::Failed => "FAILED",
            Self::Cancelled => "CANCELLED",
        }
    }

    pub fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "QUEUED" => Ok(Self::Queued),
            "RUNNING" => Ok(Self::Running),
            "SUSPENDING" => Ok(Self::Suspending),
            "SUSPENDED" => Ok(Self::Suspended),
            "SUCCEEDED" => Ok(Self::Succeeded),
            "FAILED" => Ok(Self::Failed),
            "CANCELLED" => Ok(Self::Cancelled),
            _ => Err(OrchestratorError::CorruptState(value.to_string())),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttemptKey {
    pub run_id: String,
    pub step_id: String,
    pub attempt_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalRecord {
    pub request_id: String,
    pub run_id: String,
    pub step_id: String,
    pub attempt_id: String,
    pub session_id: String,
    pub checkpoint_digest: String,
    pub continuation_generation: i64,
    pub tool_name: String,
    pub args_digest: String,
    pub decision_revision: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredApproval {
    pub request_id: String,
    pub run_id: String,
    pub step_id: String,
    pub attempt_id: String,
    pub session_id: String,
    pub tool_name: String,
    pub approval_message: String,
    pub requested_args_json: String,
    pub amendable_schema_json: String,
    pub args_digest: String,
    pub required_roles_json: String,
    pub checkpoint_digest: String,
    pub continuation_generation: i64,
    pub created_at: i64,
    pub expires_at: Option<i64>,
    pub decision: Option<String>,
    pub decision_revision: i64,
    pub reviewer_principal: Option<String>,
    pub amended_args_json: Option<String>,
    pub amended_args_digest: Option<String>,
    pub rejection_reason: Option<String>,
    pub published: bool,
    /// Selected decider. Defaults to the parent run owner (`HitlApprover::Owner`).
    pub approver_user_id: String,
    /// `tool_call` or `gate`. Older rows default to `tool_call`.
    pub kind: String,
    /// Permission target stamped when the tool suspended (`mcp:host/tool`).
    /// Empty for gates and legacy sessions. Never recomputed.
    pub target: String,
    /// "Always allow / deny" can take effect: no source or org rule decides, and
    /// the session is the agent owner's draft run.
    pub always_offered: bool,
    /// Why Always is withheld. Empty when `always_offered` is true.
    pub locked_reason: String,
    /// The skill's ask card (`{jig, skill, ir, data}`) when the tool has one. Authoritative at
    /// decide: an AMENDED decision is checked against `amendable_schema_json`, built from it.
    pub card_json: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct OutboxEvent {
    pub sequence: i64,
    pub run_id: String,
    pub event_type: String,
    pub payload: String,
}

#[derive(Debug, Error)]
pub enum OrchestratorError {
    #[error("workflow storage is unavailable")]
    StorageUnavailable,
    #[error("workflow database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("invalid contract: {0}")]
    InvalidContract(String),
    #[error("attempt '{attempt_id}' was not found for step '{step_id}'")]
    AttemptNotFound { step_id: String, attempt_id: String },
    #[error("attempt is in state '{actual}', expected '{expected}'")]
    InvalidState {
        expected: &'static str,
        actual: String,
    },
    #[error("lease does not match the active attempt")]
    StaleLease,
    #[error("lease has expired")]
    LeaseExpired,
    #[error("approval request '{0}' already has a decision")]
    ApprovalAlreadyDecided(String),
    #[error("approval decision revision mismatch (expected {expected}, got {actual})")]
    StaleDecisionRevision { expected: i64, actual: i64 },
    #[error("approval request '{0}' not found")]
    ApprovalNotFound(String),
    #[error("critical workflow evaluations must be authoritative and pinned")]
    UntrustedEvaluation,
    #[error("workflow state is corrupt: {0}")]
    CorruptState(String),
    #[error(transparent)]
    Security(#[from] SecurityError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("conflict: {0}")]
    Conflict(String),
    /// A release names dependencies that have no released version. One entry per
    /// dependency, each ending in the fix. Maps to HTTP 409.
    #[error("{}", .0.join("\n"))]
    DependenciesNotReleased(Vec<String>),
    #[error("request is unauthenticated")]
    Unauthenticated,
    #[error("permission denied: {0}")]
    PermissionDenied(String),
    #[error("node is draining; new admissions blocked")]
    NodeDraining,
    #[error("missing idempotency key")]
    MissingIdempotencyKey,
    #[error("core VM error: {0}")]
    Core(String),
    #[error("secrets master key unavailable: {0}")]
    SecretsMasterKey(String),
    #[error("secret decrypt failed for binding '{0}'")]
    SecretDecryptFailed(String),
    #[error("missing secret binding '{binding_id}' (scope {scope})")]
    MissingSecretBinding { binding_id: String, scope: String },
    #[error("admission failed: missing required secrets: {0}")]
    MissingRequiredSecrets(String),
    #[error("agent is running on workspace '{workspace_id}' (session {session_id}, run {run_id})")]
    AgentRunning {
        workspace_id: String,
        session_id: String,
        run_id: String,
    },
    /// The workspace is held by a parked run. `cancel_command` is the operator
    /// command that releases it (`vam run cancel <run>`).
    #[error(
        "workspace '{workspace_id}' is reserved by parked run '{run_id}'; cancel it with: {cancel_command}"
    )]
    WorkspaceReserved {
        workspace_id: String,
        run_id: String,
        cancel_command: String,
    },
    /// A run already holds its cap of undelivered inbox messages.
    #[error("inbox_full: run '{run_id}' already has {pending} undelivered messages")]
    InboxFull { run_id: String, pending: usize },
    #[error("agent '{requested}' does not match workspace-bound agent '{bound}' on workspace '{workspace_id}'")]
    AgentWorkspaceMismatch {
        workspace_id: String,
        bound: String,
        requested: String,
    },
    #[error("agent version '{requested}' does not match workspace-bound version '{bound}' on workspace '{workspace_id}'")]
    AgentWorkspaceVersionMismatch {
        workspace_id: String,
        bound: String,
        requested: String,
    },
    #[error("workspace '{workspace_id}' is not bound to an agent; provide agentRef in run.json")]
    WorkspaceNotBound { workspace_id: String },
    #[error("hosted service protocol error: {0}")]
    Hsp(String),
    #[error("remote MCP server error: {0}")]
    Mcp(String),
    #[error("password hashing failed")]
    PasswordHash,
    #[error("password hash is not a valid argon2id PHC string")]
    InvalidPasswordHash,
}

/// Operator command that cancels a run and releases a parked workspace.
pub fn cancel_run_command(run_id: &str) -> String {
    format!("vam run cancel {run_id}")
}
