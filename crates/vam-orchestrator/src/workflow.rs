use crate::error::{
    ApprovalRecord, AttemptKey, AttemptState, OrchestratorError, OutboxEvent, StoredApproval,
};
use crate::events::{hitl_decided_json, hitl_expired_json, hitl_paused_json};
use crate::hitl::{
    default_amendable_schema, render_approval_message, render_approval_message_with_description,
    validate_amended_args, validate_gate_response, validate_requested_args_size, HitlApprover,
    APPROVAL_KIND_GATE, APPROVAL_KIND_QUESTION,
};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use vam_security::{ReplayStore, SecurityError};
use vam_spec::v1::{
    ArtifactRef, EvaluationResult, ExecutionLease, OutcomeTerminal, PlaybookGuard, PlaybookSpec,
    PlaybookStep, ResourceId, StepKind, StepMode, TaskOutcome, TaskOutcomeState,
};
use vam_spec::validation::Validate;

const APPROVAL_SELECT: &str = "
            SELECT request_id, run_id, step_id, attempt_id, session_id, tool_name,
                    approval_message, requested_args_json, amendable_schema_json, args_digest,
                    required_roles_json, checkpoint_digest, continuation_generation,
                    created_at, expires_at, decision, decision_revision, reviewer_principal,
                    amended_args_json, amended_args_digest, rejection_reason, published,
                    approver_user_id, kind, target, always_offered, locked_reason, card_json";

/// Iteration, spend, and token counters for one run.
///
/// `iteration` is the last value handed to the PEP (the first tool call is 1).
/// Spend and token fields are cumulative across every session of the run.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RunMetering {
    pub iteration: i64,
    pub spend_micro: i64,
    pub token_count: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

/// Default keep-warm window for a human wait: 5 minutes.
///
/// A gate uses its own `keep_warm` when that field is set. Tool-level HITL
/// and a gate that omits `keep_warm` use this operator default.
pub const DEFAULT_KEEP_WARM_SECS: i64 = 300;

/// Gate timeout when the step does not set `timeout`. Publish rejects values
/// above 30 days.
pub const DEFAULT_GATE_TIMEOUT_SECS: i64 = 72 * 60 * 60;

/// A paused run whose keep-warm window has elapsed and still has no decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParkCandidate {
    pub run_id: String,
    pub session_id: String,
    pub workspace_id: String,
    /// Approval `expires_at`, or [`i64::MAX`] when the wait has no deadline.
    /// The workspace reservation uses the same bound.
    pub reservation_expires_at: i64,
}

/// Workspace held for a parked run so another `CreateRun` cannot take it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceReservation {
    pub workspace_id: String,
    pub run_id: String,
    pub created_at: i64,
    pub expires_at: i64,
}

/// An approval row moved to `EXPIRED` by [`SqliteWorkflowStore::expire_stale_approvals`].
///
/// `session_id` and `workspace_id` are what the sweeper needs to terminate the
/// parked VM and release its workspace write lock. Either may be `None` when
/// the approval has no session row (for example, a legacy row).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExpiredApproval {
    pub request_id: String,
    pub run_id: String,
    pub session_id: Option<String>,
    pub workspace_id: Option<String>,
}

/// An approval the user's stop rejected (or, for a question, cancelled).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoppedApproval {
    pub request_id: String,
    pub step_id: String,
    pub session_id: Option<String>,
    pub kind: String,
    pub decision: String,
}

/// What the user's stop did to a run's pending approvals.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StopRejection {
    /// Rejected or cancelled by the stop.
    pub stopped: Vec<StoppedApproval>,
    /// Decided by somebody else between the read and the write: that decision stands
    /// and its resume is on its way.
    pub lost: usize,
}

/// A run whose active time reached `RunSpec.max_duration`.
///
/// Parked gaps are not part of that total. The sweeper uses `session_id` and
/// `workspace_id` to terminate a still-warm VM and release its write lock.
/// The store has already dropped any workspace reservation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActiveDurationExpiry {
    pub run_id: String,
    pub session_id: Option<String>,
    pub workspace_id: Option<String>,
}

/// Pinned playbook identity stored with a run's step state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookRunPin {
    pub urn: String,
    pub version: String,
    pub digest: String,
}

/// Host-owned state of one playbook step.
///
/// `pending` waits on preconditions. `available` may be completed by
/// `step_complete`. `open` is a gate waiting on a human. `completed` has a
/// final outcome. `skipped` can no longer run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybookStepState {
    Pending,
    Available,
    Open,
    Completed,
    Skipped,
}

impl PlaybookStepState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Available => "available",
            Self::Open => "open",
            Self::Completed => "completed",
            Self::Skipped => "skipped",
        }
    }

    fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "pending" => Ok(Self::Pending),
            "available" => Ok(Self::Available),
            "open" => Ok(Self::Open),
            "completed" => Ok(Self::Completed),
            "skipped" => Ok(Self::Skipped),
            other => Err(OrchestratorError::CorruptState(format!(
                "unknown playbook step state '{other}'"
            ))),
        }
    }
}

/// One persisted playbook step, in spec order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookStepRecord {
    pub step_id: String,
    pub state: PlaybookStepState,
    pub outcome: String,
    pub output_json: Option<String>,
    pub agent_summary: Option<String>,
    pub evidence_refs: Vec<String>,
    pub started_at: Option<i64>,
    pub finished_at: Option<i64>,
    pub transition_revision: i64,
}

/// Outcome the host records for an available step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookStepCompletion {
    pub outcome: String,
    pub output_json: Option<String>,
    pub agent_summary: Option<String>,
}

/// Header row for a run's playbook step state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybookRunRecord {
    pub run_id: String,
    pub urn: String,
    pub version: String,
    pub digest: String,
    pub state: String,
}

/// Host finish check for a playbook run when the guest task ends.
///
/// Required (non-optional) steps that are still `pending`, `available`, or
/// `open` are missing. `skipped` steps are not reachable and do not block
/// completion. An open gate is missing even when the step is optional, so a
/// run cannot succeed while a human decision is outstanding. Run output that
/// does not match `PlaybookSpec.output_schema` is a missing item as well.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlaybookFinishCheck {
    Complete,
    Incomplete {
        missing_steps: Vec<String>,
        open_gates: Vec<String>,
        schema_error: Option<String>,
    },
}

/// True when a gate is open and no task step is available: the only thing the
/// run waits on is a human decision. The host parks such a run when the guest
/// task ends ([`SqliteWorkflowStore::park_for_open_gates`]), and the guest has
/// nothing left to do, so it should end its turn instead of asking the model again.
fn only_open_gates_remain(steps: &[PlaybookStepRecord]) -> bool {
    !steps
        .iter()
        .any(|step| step.state == PlaybookStepState::Available)
        && steps
            .iter()
            .any(|step| step.state == PlaybookStepState::Open)
}

/// Error string stored on a run that ends incomplete. Keeps the PB-0.6
/// "missing required steps" phrase and adds open-gate and schema details.
pub fn playbook_finish_error(
    missing_steps: &[String],
    open_gates: &[String],
    schema_error: Option<&str>,
) -> String {
    let mut parts = Vec::new();
    if !missing_steps.is_empty() {
        parts.push(format!(
            "missing required steps: {}",
            missing_steps.join(", ")
        ));
    }
    if !open_gates.is_empty() {
        parts.push(format!("open gates: {}", open_gates.join(", ")));
    }
    if let Some(schema) = schema_error.filter(|message| !message.is_empty()) {
        parts.push(format!("output schema: {schema}"));
    }
    if parts.is_empty() {
        return "playbook incomplete".to_string();
    }
    format!("playbook incomplete: {}", parts.join("; "))
}

/// Trailing instruction on an incomplete playbook finish.
///
/// The guest fallback nudge uses this same sentence. Keep the wording identical.
pub const PLAYBOOK_FINISH_STEP_GUIDANCE: &str = "For each missing step: call run_step if it is bound, do the work and call step_complete if it is agentic, and wait for the human decision if it is a gate. Never invent a step's outcome.";

/// How the guest completes one playbook step. Shared by the rendered procedure
/// and the finish-incomplete observation.
pub(crate) fn playbook_step_completion_line(step: &PlaybookStep) -> String {
    if StepKind::try_from(step.kind) == Ok(StepKind::Gate) {
        return gate_completion_line();
    }
    match StepMode::try_from(step.mode).unwrap_or(StepMode::Unspecified) {
        StepMode::Bound => bound_completion_line(&step.id),
        StepMode::Assisted => assisted_completion_line(step),
        StepMode::Agentic | StepMode::Unspecified => agentic_completion_line(&step.id),
    }
}

fn gate_completion_line() -> String {
    "Wait for the human decision on the gate approval. Do not call run_step or step_complete for it."
        .to_string()
}

fn bound_completion_line(step_id: &str) -> String {
    format!(
        "Complete it by calling run_step with step_id \"{step_id}\". The host runs the bound tool."
    )
}

fn agentic_completion_line(step_id: &str) -> String {
    format!(
        "Do this step yourself with the tools you have, then call step_complete with step_id \"{step_id}\" and an outcome describing what you did. Never invent results. If you cannot do it with your tools, say so in the outcome."
    )
}

/// Assisted steps name the tool and leave the arguments to the model.
/// `run_step` rejects them; `step_complete` records the outcome after the
/// guest calls that tool.
fn assisted_completion_line(step: &PlaybookStep) -> String {
    let tool = step
        .binding
        .as_ref()
        .map(|binding| binding.tool_name.trim())
        .filter(|name| !name.is_empty());
    match tool {
        Some(name) => format!(
            "Call \"{name}\" yourself, filling its arguments from context, then call step_complete with step_id \"{}\" and an outcome describing what you did. Do not call run_step for it.",
            step.id
        ),
        None => format!(
            "Call the bound tool yourself, filling its arguments from context, then call step_complete with step_id \"{}\" and an outcome describing what you did. Do not call run_step for it.",
            step.id
        ),
    }
}

/// Gates a human has already decided, as `(step id, outcome label)` in spec
/// order. A later step that waits on a gate (`after: ["decide:pay"]`) depends on
/// this, so the guest must be told the decision, not left to infer it.
pub fn decided_gate_outcomes(
    steps: &[PlaybookStep],
    records: &[PlaybookStepRecord],
) -> Vec<(String, String)> {
    records
        .iter()
        .filter(|record| record.state == PlaybookStepState::Completed)
        .filter(|record| {
            steps.iter().any(|step| {
                step.id == record.step_id && StepKind::try_from(step.kind) == Ok(StepKind::Gate)
            })
        })
        .map(|record| (record.step_id.clone(), record.outcome.clone()))
        .collect()
}

/// Observation returned to the guest when a finish attempt is incomplete.
///
/// `steps` is the pinned playbook spec. Each missing step is listed with the
/// same completion line the procedure renders for that step. `decided_gates`
/// comes from [`decided_gate_outcomes`].
pub fn playbook_finish_observation(
    missing_steps: &[String],
    open_gates: &[String],
    decided_gates: &[(String, String)],
    schema_error: Option<&str>,
    steps: &[PlaybookStep],
) -> String {
    let mut lines = vec!["Playbook finish is incomplete. Do not end the task yet.".to_string()];
    if !missing_steps.is_empty() {
        lines.push(format!(
            "Missing required steps: {}.",
            missing_steps.join(", ")
        ));
    }
    if !open_gates.is_empty() {
        lines.push(format!("Open gates: {}.", open_gates.join(", ")));
    }
    if !decided_gates.is_empty() {
        let decided = decided_gates
            .iter()
            .map(|(gate, outcome)| format!("{gate} chose {outcome}"))
            .collect::<Vec<_>>()
            .join(", ");
        lines.push(format!("Decided gates: {decided}."));
    }
    if let Some(schema) = schema_error.filter(|message| !message.is_empty()) {
        lines.push(format!("Output schema: {schema}."));
    }
    for step_id in missing_steps {
        let action = steps
            .iter()
            .find(|step| step.id == *step_id)
            .map(playbook_step_completion_line)
            .unwrap_or_else(|| {
                if open_gates.iter().any(|gate| gate == step_id) {
                    gate_completion_line()
                } else {
                    agentic_completion_line(step_id)
                }
            });
        lines.push(format!("{step_id}: {action}"));
    }
    lines.push(PLAYBOOK_FINISH_STEP_GUIDANCE.to_string());
    lines.push(vam_spec::playbook::PLAYBOOK_GATE_RULE.to_string());
    lines.join("\n")
}

const PLAYBOOK_RUN_ACTIVE: &str = "active";
const PLAYBOOK_RUN_SUCCEEDED: &str = "succeeded";
const PLAYBOOK_RUN_FAILED: &str = "failed";

/// SQLite-backed authoritative state. Every method that changes a workflow
/// also appends an outbox record in the same transaction.
pub struct SqliteWorkflowStore {
    pub(crate) connection: Mutex<Connection>,
    pub(crate) event_bus:
        std::sync::RwLock<Option<std::sync::Arc<vam_telemetry::BroadcastEventBus>>>,
    pub(crate) audit_ledger:
        std::sync::RwLock<Option<std::sync::Arc<vam_telemetry::PartitionedAuditLedger>>>,
    pub(crate) session_budgets: std::sync::RwLock<
        std::collections::HashMap<String, std::sync::Arc<vam_inference::SessionBudget>>,
    >,
    /// The host's thread-log recorder of each live session.
    pub(crate) thread_recorders: std::sync::RwLock<
        std::collections::HashMap<String, std::sync::Arc<crate::thread_recorder::ThreadRecorder>>,
    >,
    /// Stop requests and workspace settlement of the sessions with a run
    /// being dispatched (AR-2.6). In memory: a restart has no dispatcher.
    pub session_ends: crate::session_end::SessionEnds,
    /// Runs whose terminal state waits for the workspace to settle: the staged
    /// `(state, result_json)` once the run has finished (Wave 3b).
    pub(crate) deferred_runs: Mutex<std::collections::HashMap<String, Option<(String, String)>>>,
    /// Host calls of each run that are in flight against this store's effect journal
    /// (`effect_path`). Kept with the journal it guards, so "nothing is sending this `sent` row"
    /// is only ever decided from calls on the same journal.
    pub(crate) effect_inflight: Mutex<std::collections::HashMap<String, usize>>,
    /// Woken when a run reaches a terminal state: a queued message may be able to
    /// start (AR-3.8). A permit is kept, so a wake with no waiter is not lost.
    pub queue_wake: tokio::sync::Notify,
    /// Wakes the resume pass (`resume.rs`) when a run is interrupted.
    pub resume_wake: tokio::sync::Notify,
    /// Queued runs the starter is starting right now (AR-3.8): only these may adopt their
    /// PENDING row, so a client cannot start another user's queued message.
    pub(crate) queue_starting: Mutex<HashSet<String>>,
    /// Trigger gateway sockets and request windows (E5).
    pub gateways: crate::gateway::Hub,
    /// In-memory per-session CA. The private key never enters SQLite.
    pub(crate) session_cas: std::sync::RwLock<
        std::collections::HashMap<String, std::sync::Arc<vam_network::tls_ca::SessionCa>>,
    >,
}

/// Marks the `intended` journal row that waits on `approval_id` as failed (`denied` or
/// `stopped`). Runs inside the decision's own transaction.
pub(crate) fn fail_journal_row_for_approval(
    transaction: &rusqlite::Transaction<'_>,
    approval_id: &str,
    detail: &str,
) -> Result<(), OrchestratorError> {
    transaction.execute(
        "UPDATE effect_journal SET state = 'failed', detail = ?1, updated_at = ?2
         WHERE approval_id = ?3 AND state = 'intended'",
        params![detail, unix_timestamp() * 1000, approval_id],
    )?;
    Ok(())
}

impl SqliteWorkflowStore {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, OrchestratorError> {
        let connection = Connection::open(path)?;
        Self::initialize(connection)
    }

    pub fn in_memory() -> Result<Self, OrchestratorError> {
        Self::initialize(Connection::open_in_memory()?)
    }

    pub(crate) fn initialize(connection: Connection) -> Result<Self, OrchestratorError> {
        connection.execute_batch(
            "
            PRAGMA foreign_keys = ON;
            PRAGMA journal_mode = WAL;
            PRAGMA synchronous = NORMAL;

            CREATE TABLE IF NOT EXISTS runs (
                run_id TEXT PRIMARY KEY NOT NULL,
                state TEXT NOT NULL,
                revision INTEGER NOT NULL,
                spec_json TEXT,
                session_id TEXT,
                agent_urn TEXT,
                idempotency_key TEXT,
                owner_principal TEXT,
                result_json TEXT
            );

            CREATE TABLE IF NOT EXISTS attempts (
                run_id TEXT NOT NULL,
                step_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL,
                lease_id TEXT NOT NULL,
                lifecycle_generation INTEGER NOT NULL,
                fencing_token BLOB NOT NULL,
                expires_at INTEGER NOT NULL,
                state TEXT NOT NULL,
                continuation_generation INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (run_id, step_id, attempt_id),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );

            CREATE TABLE IF NOT EXISTS approvals (
                request_id TEXT PRIMARY KEY NOT NULL,
                run_id TEXT NOT NULL,
                step_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL,
                checkpoint_digest TEXT NOT NULL,
                continuation_generation INTEGER NOT NULL,
                decision TEXT,
                consumed_token_id TEXT UNIQUE
            );

            CREATE TABLE IF NOT EXISTS workflow_outbox (
                sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                run_id TEXT NOT NULL,
                event_type TEXT NOT NULL,
                payload TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS workflow_evaluations (
                run_id TEXT NOT NULL,
                assertion_id TEXT NOT NULL,
                evaluator_id TEXT NOT NULL,
                passed INTEGER NOT NULL,
                PRIMARY KEY (run_id, assertion_id, evaluator_id),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );

            CREATE TABLE IF NOT EXISTS run_playbook (
                run_id TEXT PRIMARY KEY NOT NULL,
                playbook_urn TEXT NOT NULL,
                playbook_version TEXT NOT NULL,
                playbook_digest TEXT NOT NULL,
                spec_json TEXT NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('active', 'succeeded', 'failed')),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );

            CREATE TABLE IF NOT EXISTS run_playbook_steps (
                run_id TEXT NOT NULL,
                step_id TEXT NOT NULL,
                position INTEGER NOT NULL,
                state TEXT NOT NULL CHECK(state IN ('pending', 'available', 'open', 'completed', 'skipped')),
                outcome TEXT NOT NULL DEFAULT '',
                output_json TEXT,
                agent_summary TEXT,
                evidence_refs_json TEXT,
                started_at INTEGER,
                finished_at INTEGER,
                transition_revision INTEGER NOT NULL,
                PRIMARY KEY (run_id, step_id),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );
            CREATE INDEX IF NOT EXISTS idx_run_playbook_steps_run
                ON run_playbook_steps(run_id, position);

            CREATE TABLE IF NOT EXISTS run_metering (
                run_id TEXT PRIMARY KEY NOT NULL,
                iteration INTEGER NOT NULL,
                spend_micro INTEGER NOT NULL,
                token_count INTEGER NOT NULL,
                prompt_tokens INTEGER NOT NULL,
                completion_tokens INTEGER NOT NULL
            );

            CREATE TABLE IF NOT EXISTS run_checkpoints (
                run_id TEXT NOT NULL,
                lifecycle_generation INTEGER NOT NULL,
                digest TEXT NOT NULL,
                blob_ref TEXT NOT NULL,
                checkpoint_bytes BLOB NOT NULL,
                created_at INTEGER NOT NULL,
                PRIMARY KEY (run_id, lifecycle_generation),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );

            CREATE TABLE IF NOT EXISTS turn_commits (
                run_id TEXT NOT NULL,
                round INTEGER NOT NULL,
                log_seq INTEGER NOT NULL,
                log_digest TEXT NOT NULL,
                manifest_hash TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL CHECK(state IN ('pending', 'complete', 'failed')),
                created_at INTEGER NOT NULL,
                PRIMARY KEY (run_id, round),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );

            CREATE TABLE IF NOT EXISTS run_checkpoint_grants (
                run_id TEXT NOT NULL,
                digest TEXT NOT NULL,
                PRIMARY KEY (run_id, digest),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );

            CREATE TABLE IF NOT EXISTS run_reservations (
                workspace_id TEXT PRIMARY KEY NOT NULL,
                run_id TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                expires_at INTEGER NOT NULL,
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
            );
            CREATE INDEX IF NOT EXISTS idx_run_reservations_run
                ON run_reservations(run_id);

            -- A session tree whose commit failed, moved to drive9/kept (AR-2.8).
            CREATE TABLE IF NOT EXISTS workspace_kept (
                session_id TEXT PRIMARY KEY NOT NULL,
                workspace_id TEXT NOT NULL,
                run_id TEXT NOT NULL DEFAULT '',
                kept_id TEXT NOT NULL,
                error TEXT NOT NULL,
                attempts INTEGER NOT NULL DEFAULT 1,
                state TEXT NOT NULL DEFAULT 'kept',
                created_at INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_workspace_kept_workspace
                ON workspace_kept(workspace_id);

            -- The workspace on both sides of a run: the HEAD it started from and
            -- the HEAD its end commit wrote (Wave 3b; a receipt pin for E12).
            CREATE TABLE IF NOT EXISTS run_workspace_heads (
                run_id TEXT PRIMARY KEY NOT NULL,
                session_id TEXT NOT NULL DEFAULT '',
                head_before TEXT NOT NULL,
                files_hash_before TEXT NOT NULL,
                head_after TEXT,
                files_hash_after TEXT,
                updated_at INTEGER NOT NULL
            );
            ",
        )?;
        migrate_playbook_step_open_state(&connection)?;
        retire_wave_one_conversation_store(&connection)?;
        migrate_turn_commits_boundary(&connection)?;

        // Run session schema migrations as well!
        crate::session::initialize_session_schema(&connection)?;

        let store = Self {
            connection: Mutex::new(connection),
            event_bus: std::sync::RwLock::new(None),
            audit_ledger: std::sync::RwLock::new(None),
            session_budgets: std::sync::RwLock::new(std::collections::HashMap::new()),
            thread_recorders: std::sync::RwLock::new(std::collections::HashMap::new()),
            session_ends: crate::session_end::SessionEnds::default(),
            deferred_runs: Mutex::new(std::collections::HashMap::new()),
            effect_inflight: Mutex::new(std::collections::HashMap::new()),
            queue_wake: tokio::sync::Notify::new(),
            resume_wake: tokio::sync::Notify::new(),
            queue_starting: Mutex::new(HashSet::new()),
            gateways: crate::gateway::Hub::default(),
            session_cas: std::sync::RwLock::new(std::collections::HashMap::new()),
        };
        store.initialize_users_schema()?;
        store.initialize_api_keys_schema()?;
        store.initialize_session_tokens_schema()?;
        store.initialize_catalog_schema()?;
        store.initialize_runs_extended_schema()?;
        store.initialize_host_secrets_schema()?;
        store.initialize_hsp_broker_approvals_schema()?;
        store.initialize_mcp_connections_schema()?;
        store.initialize_operator_allowed_models_schema()?;
        store.initialize_hitl_schema()?;
        store.initialize_session_targets_schema()?;
        store.initialize_owner_stamps()?;
        store.initialize_versioned_catalog_artifacts()?;
        store.initialize_per_user_idempotency()?;
        store.initialize_vault_key()?;
        store.initialize_connection_secret_kinds()?;
        store.initialize_skill_package_blob_namespace()?;
        store.initialize_catalog_tier()?;
        store.initialize_web_sources_schema()?;
        {
            let connection = store
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            crate::run_facts::initialize_schema(&connection)?;
            crate::receipt::initialize_schema(&connection)?;
            crate::outcomes::initialize_schema(&connection)?;
            crate::inbox_store::initialize_schema(&connection)?;
            crate::effect_store::initialize_schema(&connection)?;
            crate::resume_store::initialize_schema(&connection)?;
            crate::spaces::initialize_schema(&connection)?;
            crate::notifications::initialize_schema(&connection)?;
            crate::contacts::initialize_schema(&connection)?;
            crate::node_notify::initialize_schema(&connection)?;
            crate::connectors::initialize_schema(&connection)?;
            crate::marketplace::store::initialize_schema(&connection)?;
            crate::connection_egress::initialize_schema(&connection)?;
            crate::notification_prefs::initialize_schema(&connection)?;
            crate::notify_delivery::initialize_schema(&connection)?;
        }
        Ok(store)
    }

    pub fn create_run(&self, run_id: &ResourceId) -> Result<(), OrchestratorError> {
        run_id
            .validate()
            .map_err(|error| OrchestratorError::InvalidContract(error.to_string()))?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let inserted = transaction.execute(
            "INSERT OR IGNORE INTO runs (run_id, state, revision, owner_user_id, created_at)
             VALUES (?1, 'RUNNING', 0, ?2, ?3)",
            params![run_id.urn, crate::users::LOCAL_USER_ID, unix_timestamp()],
        )?;
        if inserted == 1 {
            append_outbox(&transaction, &run_id.urn, "vam.v1.run.created", "created")?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn issue_lease(&self, lease: &ExecutionLease) -> Result<(), OrchestratorError> {
        lease
            .validate()
            .map_err(|error| OrchestratorError::InvalidContract(error.to_string()))?;
        let run_id = required_urn(&lease.run_id, "run_id")?;
        let attempt_id = required_urn(&lease.attempt_id, "attempt_id")?;
        let lease_id = required_urn(&lease.lease_id, "lease_id")?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let run_exists = transaction
            .query_row(
                "SELECT 1 FROM runs WHERE run_id = ?1",
                [&run_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if !run_exists {
            return Err(OrchestratorError::AttemptNotFound {
                step_id: lease.step_id.clone(),
                attempt_id,
            });
        }
        transaction.execute(
            "INSERT INTO attempts (
                run_id, step_id, attempt_id, lease_id, lifecycle_generation,
                fencing_token, expires_at, state, continuation_generation
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'RUNNING', 0)",
            params![
                run_id,
                lease.step_id,
                attempt_id,
                lease_id,
                lease.lifecycle_generation,
                lease.fencing_token,
                lease.expires_at.as_ref().map(|timestamp| timestamp.seconds),
            ],
        )?;
        increment_revision(&transaction, &run_id)?;
        append_outbox(
            &transaction,
            &run_id,
            "vam.v1.step.lease-issued",
            &lease.step_id,
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn record_outcome(&self, outcome: &TaskOutcome) -> Result<AttemptState, OrchestratorError> {
        outcome
            .validate()
            .map_err(|error| OrchestratorError::InvalidContract(error.to_string()))?;
        let lease = outcome.lease.as_ref().ok_or_else(|| {
            OrchestratorError::InvalidContract("TaskOutcome.lease is required".to_string())
        })?;
        let key = attempt_key(lease)?;
        let lease_id = required_urn(&lease.lease_id, "lease_id")?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let stored = transaction
            .query_row(
                "SELECT lease_id, lifecycle_generation, fencing_token, expires_at, state
                 FROM attempts WHERE run_id = ?1 AND step_id = ?2 AND attempt_id = ?3",
                params![key.run_id, key.step_id, key.attempt_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)?,
                        row.get::<_, String>(4)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| OrchestratorError::AttemptNotFound {
                step_id: key.step_id.clone(),
                attempt_id: key.attempt_id.clone(),
            })?;
        if stored.0 != lease_id
            || stored.1 != lease.lifecycle_generation
            || stored.2 != lease.fencing_token
        {
            return Err(OrchestratorError::StaleLease);
        }
        if stored.3 < unix_timestamp() {
            return Err(OrchestratorError::LeaseExpired);
        }
        let current = AttemptState::parse(&stored.4)?;
        if current != AttemptState::Running && current != AttemptState::Suspending {
            return Err(OrchestratorError::InvalidState {
                expected: AttemptState::Running.as_str(),
                actual: stored.4,
            });
        }

        let state = outcome_state(outcome.state)?;
        transaction.execute(
            "UPDATE attempts SET state = ?1 WHERE run_id = ?2 AND step_id = ?3 AND attempt_id = ?4",
            params![state.as_str(), key.run_id, key.step_id, key.attempt_id],
        )?;
        increment_revision(&transaction, &key.run_id)?;
        append_outbox(
            &transaction,
            &key.run_id,
            "vam.v1.step.outcome-recorded",
            state.as_str(),
        )?;
        transaction.commit()?;
        Ok(state)
    }

    #[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
    pub fn register_hitl_suspend(
        &self,
        key: &AttemptKey,
        session_id: &str,
        request_id: &ResourceId,
        tool_name: &str,
        requested_args_json: &str,
        args_digest: &str,
        expires_at: Option<i64>,
    ) -> Result<(), OrchestratorError> {
        self.register_hitl_suspend_card(
            key,
            session_id,
            request_id,
            tool_name,
            requested_args_json,
            args_digest,
            expires_at,
            None,
        )
    }

    /// Like [`Self::register_hitl_suspend`], for a tool whose skill ships an ask card (CN-3.5).
    /// With a card the approval stores it and its amendable schema is the card's submit fields
    /// typed from the tool's input schema; without one this is `register_hitl_suspend`.
    #[allow(clippy::too_many_arguments)]
    pub fn register_hitl_suspend_card(
        &self,
        key: &AttemptKey,
        session_id: &str,
        request_id: &ResourceId,
        tool_name: &str,
        requested_args_json: &str,
        args_digest: &str,
        expires_at: Option<i64>,
        card: Option<&crate::hitl::AskApprovalCard>,
    ) -> Result<(), OrchestratorError> {
        self.register_hitl_suspend_inner(
            key,
            session_id,
            request_id,
            tool_name,
            requested_args_json,
            args_digest,
            expires_at,
            None,
            card,
        )
    }

    /// Like [`Self::register_hitl_suspend`], with a caller-built amendable schema
    /// and message (the `start_playbook` ask card). "Always" is never offered for
    /// it: the host decides ask versus auto from the playbook, not from a rule.
    #[allow(clippy::too_many_arguments)]
    pub fn register_hitl_suspend_custom(
        &self,
        key: &AttemptKey,
        session_id: &str,
        request_id: &ResourceId,
        tool_name: &str,
        requested_args_json: &str,
        args_digest: &str,
        expires_at: Option<i64>,
        custom: Option<&crate::hitl::StartPlaybookApproval>,
    ) -> Result<(), OrchestratorError> {
        self.register_hitl_suspend_inner(
            key,
            session_id,
            request_id,
            tool_name,
            requested_args_json,
            args_digest,
            expires_at,
            custom,
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn register_hitl_suspend_inner(
        &self,
        key: &AttemptKey,
        session_id: &str,
        request_id: &ResourceId,
        tool_name: &str,
        requested_args_json: &str,
        args_digest: &str,
        expires_at: Option<i64>,
        custom: Option<&crate::hitl::StartPlaybookApproval>,
        card: Option<&crate::hitl::AskApprovalCard>,
    ) -> Result<(), OrchestratorError> {
        validate_requested_args_size(requested_args_json)?;
        request_id
            .validate()
            .map_err(|error| OrchestratorError::InvalidContract(error.to_string()))?;

        let args_value: serde_json::Value =
            serde_json::from_str(requested_args_json).map_err(|e| {
                OrchestratorError::InvalidContract(format!("requested_args_json invalid: {e}"))
            })?;
        let amendable = match (custom, card) {
            (Some(custom), _) => custom.amendable_schema.clone(),
            (None, Some(card)) => card.amendable_schema(),
            (None, None) => default_amendable_schema(&args_value),
        };
        let card_json = card.map(|card| card.stored_json().to_string());
        let amendable_json = serde_json::to_string(&amendable).map_err(|e| {
            OrchestratorError::InvalidContract(format!("amendable_schema encode failed: {e}"))
        })?;
        let model_description =
            crate::mcp_registry::mcp_model_description(self, session_id, tool_name);
        let approval_message = match custom {
            Some(custom) => custom.message.clone(),
            None => {
                render_approval_message_with_description(tool_name, model_description.as_deref())
            }
        };
        let created_at = unix_timestamp();

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        // The target is read from the session's own row now and stored with the
        // approval. It is never recomputed from the live connection or agent.
        let mut stamp = session_target_stamp(&transaction, session_id, tool_name)?;
        if custom.is_some() {
            stamp.always_offered = false;
            stamp.locked_reason = if let Some(reason) = custom.and_then(|c| c.locked_reason.clone())
            {
                reason
            } else if tool_name == crate::playbook_activation::START_PLAYBOOK_TOOL {
                "a playbook start is confirmed each time".to_string()
            } else if tool_name == vam_spec::outbox::ASK_USER_TOOL {
                "a question is asked each time".to_string()
            } else {
                "a playbook step with values from tool output is confirmed each time".to_string()
            };
        }
        let state: String = transaction.query_row(
            "SELECT state FROM attempts WHERE run_id = ?1 AND step_id = ?2 AND attempt_id = ?3",
            params![key.run_id, key.step_id, key.attempt_id],
            |row| row.get(0),
        )?;
        if AttemptState::parse(&state)? != AttemptState::Running {
            return Err(OrchestratorError::InvalidState {
                expected: AttemptState::Running.as_str(),
                actual: state,
            });
        }
        transaction.execute(
            "UPDATE attempts SET state = ?1 WHERE run_id = ?2 AND step_id = ?3 AND attempt_id = ?4",
            params![
                AttemptState::Suspending.as_str(),
                key.run_id,
                key.step_id,
                key.attempt_id
            ],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO hitl_pending (
                request_id, run_id, step_id, attempt_id, session_id,
                tool_name, requested_args_json, args_digest, approval_message,
                amendable_schema_json, expires_at, created_at,
                target, always_offered, locked_reason, card_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            params![
                request_id.urn,
                key.run_id,
                key.step_id,
                key.attempt_id,
                session_id,
                tool_name,
                requested_args_json,
                args_digest,
                approval_message,
                amendable_json,
                expires_at,
                created_at,
                stamp.target,
                i64::from(stamp.always_offered),
                stamp.locked_reason,
                card_json,
            ],
        )?;
        increment_revision(&transaction, &key.run_id)?;
        transaction.commit()?;
        Ok(())
    }

    pub fn create_approval(
        &self,
        key: &AttemptKey,
        request_id: &ResourceId,
        checkpoint: &ArtifactRef,
        continuation_generation: i64,
    ) -> Result<i64, OrchestratorError> {
        request_id
            .validate()
            .map_err(|error| OrchestratorError::InvalidContract(error.to_string()))?;
        checkpoint
            .validate()
            .map_err(|error| OrchestratorError::InvalidContract(error.to_string()))?;
        let checkpoint_digest = checkpoint
            .digest
            .as_ref()
            .map(|digest| digest.hex_hash.as_str())
            .ok_or_else(|| {
                OrchestratorError::InvalidContract("checkpoint digest is required".to_string())
            })?;

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let state: String = transaction.query_row(
            "SELECT state FROM attempts WHERE run_id = ?1 AND step_id = ?2 AND attempt_id = ?3",
            params![key.run_id, key.step_id, key.attempt_id],
            |row| row.get(0),
        )?;
        if AttemptState::parse(&state)? != AttemptState::Suspended {
            return Err(OrchestratorError::InvalidState {
                expected: AttemptState::Suspended.as_str(),
                actual: state,
            });
        }

        let pending = transaction
            .query_row(
                "SELECT session_id, tool_name, requested_args_json, args_digest,
                        approval_message, amendable_schema_json, expires_at, created_at,
                        target, always_offered, locked_reason, card_json
                 FROM hitl_pending WHERE request_id = ?1",
                [&request_id.urn],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, String>(5)?,
                        row.get::<_, Option<i64>>(6)?,
                        row.get::<_, i64>(7)?,
                        row.get::<_, String>(8)?,
                        row.get::<_, i64>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, Option<String>>(11)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| OrchestratorError::ApprovalNotFound(request_id.urn.clone()))?;

        transaction.execute(
            "INSERT INTO approvals (
                request_id, run_id, step_id, attempt_id, checkpoint_digest,
                continuation_generation, approval_message, tool_name, requested_args_json,
                amendable_schema_json, args_digest, required_roles_json, created_at,
                expires_at, decision_revision, published, session_id, owner_user_id,
                approver_user_id, target, always_offered, locked_reason, kind, card_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, '[]', ?12, ?13, 0, 1, ?14,
                (SELECT COALESCE(owner_user_id, 'local') FROM runs WHERE run_id = ?2),
                (SELECT COALESCE(owner_user_id, 'local') FROM runs WHERE run_id = ?2),
                ?15, ?16, ?17,
                CASE WHEN ?8 = 'ask_user' THEN 'question'
                     WHEN substr(?17, 1, 7) = 'reauth:' THEN 'reauth'
                     ELSE 'tool_call' END, ?18)",
            params![
                request_id.urn,
                key.run_id,
                key.step_id,
                key.attempt_id,
                checkpoint_digest,
                continuation_generation,
                pending.4,
                pending.1,
                pending.2,
                pending.5,
                pending.3,
                pending.7,
                pending.6,
                pending.0,
                pending.8,
                pending.9,
                pending.10,
                pending.11,
            ],
        )?;
        transaction.execute(
            "DELETE FROM hitl_pending WHERE request_id = ?1",
            [&request_id.urn],
        )?;
        increment_revision(&transaction, &key.run_id)?;
        let sequence: i64 = transaction.query_row(
            "SELECT IFNULL(MAX(sequence), 0) + 1 FROM workflow_outbox WHERE run_id = ?1",
            [&key.run_id],
            |row| row.get(0),
        )?;
        let event_json = hitl_paused_json(
            &key.run_id,
            sequence,
            &request_id.urn,
            &pending.1,
            &render_approval_message(&pending.1),
            &key.attempt_id,
            checkpoint,
            &pending.3,
            continuation_generation,
        )
        .map_err(OrchestratorError::InvalidContract)?;
        append_outbox(&transaction, &key.run_id, "vam.v1.hitl.paused", &event_json)?;
        // The notification is written on this transaction: the approval and its record exist
        // together or not at all.
        crate::notifications::record_for_approval(&transaction, &request_id.urn)?;
        transaction.commit()?;
        Ok(sequence)
    }

    pub fn list_published_approvals(&self) -> Result<Vec<StoredApproval>, OrchestratorError> {
        self.list_published_approvals_filtered(None)
    }

    /// Pending published approvals owned by `owner_user_id`.
    pub fn list_published_approvals_for_owner(
        &self,
        owner_user_id: &str,
    ) -> Result<Vec<StoredApproval>, OrchestratorError> {
        self.list_published_approvals_filtered(Some(crate::owner::resolved_owner(owner_user_id)))
    }

    fn list_published_approvals_filtered(
        &self,
        owner_user_id: Option<&str>,
    ) -> Result<Vec<StoredApproval>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let sql = if owner_user_id.is_some() {
            format!(
                "{APPROVAL_SELECT}
                 FROM approvals
                 WHERE decision IS NULL AND published = 1 AND owner_user_id = ?1
                 ORDER BY created_at ASC"
            )
        } else {
            format!(
                "{APPROVAL_SELECT}
                 FROM approvals
                 WHERE decision IS NULL AND published = 1
                 ORDER BY created_at ASC"
            )
        };
        let mut stmt = connection.prepare(&sql)?;
        match owner_user_id {
            Some(owner) => {
                let rows = stmt.query_map([owner], map_stored_approval_row)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(OrchestratorError::Database)
            }
            None => {
                let rows = stmt.query_map([], map_stored_approval_row)?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(OrchestratorError::Database)
            }
        }
    }

    /// One page of `owner_user_id`'s pending published approvals, oldest first (`created_at`, then
    /// `request_id`): up to `limit` rows after the cursor `(created_at, request_id)` of the
    /// previous page's last row. A decided approval simply drops out of later pages.
    pub fn list_published_approvals_page_for_owner(
        &self,
        owner_user_id: &str,
        limit: usize,
        after: Option<(i64, String)>,
    ) -> Result<Vec<StoredApproval>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let (created, id, first) = match after {
            Some((c, i)) => (c, i, false),
            None => (i64::MIN, String::new(), true),
        };
        let sql = format!(
            "{APPROVAL_SELECT}
             FROM approvals
             WHERE decision IS NULL AND published = 1 AND owner_user_id = ?1
               AND (?4 OR created_at > ?2 OR (created_at = ?2 AND request_id > ?3))
             ORDER BY created_at ASC, request_id ASC LIMIT ?5"
        );
        let mut stmt = connection.prepare(&sql)?;
        let rows = stmt.query_map(
            rusqlite::params![
                crate::owner::resolved_owner(owner_user_id),
                created,
                id,
                first,
                limit as i64
            ],
            map_stored_approval_row,
        )?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    pub fn get_approval(&self, request_id: &str) -> Result<StoredApproval, OrchestratorError> {
        self.get_approval_filtered(request_id, None)
    }

    /// Get a published-or-not approval owned by `owner_user_id`. Missing or
    /// foreign → ApprovalNotFound (same message; no existence leak).
    pub fn get_approval_for_owner(
        &self,
        request_id: &str,
        owner_user_id: &str,
    ) -> Result<StoredApproval, OrchestratorError> {
        self.get_approval_filtered(
            request_id,
            Some(crate::owner::resolved_owner(owner_user_id)),
        )
    }

    fn get_approval_filtered(
        &self,
        request_id: &str,
        owner_user_id: Option<&str>,
    ) -> Result<StoredApproval, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let missing = || OrchestratorError::ApprovalNotFound(request_id.to_string());
        let result = if let Some(owner) = owner_user_id {
            connection.query_row(
                &format!(
                    "{APPROVAL_SELECT} FROM approvals WHERE request_id = ?1 AND owner_user_id = ?2"
                ),
                params![request_id, owner],
                map_stored_approval_row,
            )
        } else {
            connection.query_row(
                &format!("{APPROVAL_SELECT} FROM approvals WHERE request_id = ?1"),
                [request_id],
                map_stored_approval_row,
            )
        };
        result.map_err(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                missing()
            } else {
                OrchestratorError::Database(e)
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    pub fn decide_approval(
        &self,
        request_id: &ResourceId,
        token_id: &ResourceId,
        decision: &str,
        reviewer_principal: &str,
        caller_user_id: &str,
        rejection_reason: Option<&str>,
        amended_args_json: Option<&str>,
        expected_revision: Option<i64>,
        response_json: Option<&str>,
    ) -> Result<ApprovalRecord, OrchestratorError> {
        self.decide_approval_with_surface(
            request_id,
            token_id,
            decision,
            reviewer_principal,
            caller_user_id,
            rejection_reason,
            amended_args_json,
            expected_revision,
            response_json,
            crate::run_facts::SURFACE_API,
        )
    }

    /// [`Self::decide_approval`], also recording where the client says the
    /// decision came from. The surface is the client's declaration; the
    /// deciding user is the authenticated caller.
    #[allow(clippy::too_many_arguments)]
    pub fn decide_approval_with_surface(
        &self,
        request_id: &ResourceId,
        token_id: &ResourceId,
        decision: &str,
        reviewer_principal: &str,
        caller_user_id: &str,
        rejection_reason: Option<&str>,
        amended_args_json: Option<&str>,
        expected_revision: Option<i64>,
        response_json: Option<&str>,
        surface: &str,
    ) -> Result<ApprovalRecord, OrchestratorError> {
        if !matches!(
            decision,
            "APPROVED" | "AMENDED" | "REJECTED" | "SUBMITTED" | crate::hitl::DECISION_RECONNECTED
        ) {
            return Err(OrchestratorError::InvalidContract(
                "approval decisions must be APPROVED, AMENDED, REJECTED, or SUBMITTED".to_string(),
            ));
        }
        let stored = self.get_approval_for_owner(&request_id.urn, caller_user_id)?;
        if !stored.published {
            return Err(OrchestratorError::ApprovalNotFound(request_id.urn.clone()));
        }
        if !HitlApprover::parse(&stored.approver_user_id).allows_decider(caller_user_id) {
            return Err(OrchestratorError::PermissionDenied(
                "caller is not the selected HITL approver".into(),
            ));
        }
        let is_gate = stored.kind == APPROVAL_KIND_GATE;
        let mut row_payload: Option<String> = None;
        let mut declined_reason: Option<String> = None;
        let gate_completion = if is_gate {
            if !matches!(decision, "SUBMITTED" | "REJECTED") {
                return Err(OrchestratorError::InvalidContract(
                    "gate decisions must be SUBMITTED or REJECTED. SUBMITTED carries the response; REJECTED records the rejected outcome.".into(),
                ));
            }
            let spec = self.get_playbook_spec(&stored.run_id)?;
            let spec_step = spec
                .steps
                .iter()
                .find(|step| step.id == stored.step_id)
                .ok_or_else(|| {
                    OrchestratorError::CorruptState(format!(
                        "stored playbook is missing gate step '{}'",
                        stored.step_id
                    ))
                })?;
            let (outcome, output_json) = if decision == "SUBMITTED" {
                let response_json = response_json.ok_or_else(|| {
                    OrchestratorError::InvalidContract(
                        "response is required for a SUBMITTED gate decision".into(),
                    )
                })?;
                let response =
                    validate_gate_response(response_json, &stored.amendable_schema_json)?;
                let outcome = gate_outcome_label(spec_step, &response)?;
                let output_json = response.to_string();
                row_payload = Some(output_json.clone());
                (outcome, Some(output_json))
            } else if allowed_outcomes(&spec, spec_step)
                .iter()
                .any(|(label, _)| label == "rejected")
            {
                let output_json = serde_json::json!({ "outcome": "rejected" }).to_string();
                row_payload = Some(output_json.clone());
                ("rejected".to_string(), Some(output_json))
            } else {
                return Err(OrchestratorError::InvalidContract(format!(
                    "gate '{}' has no rejected outcome. SUBMITTED with response.option selects a declared option.",
                    spec_step.id
                )));
            };
            Some(PlaybookStepCompletion {
                outcome,
                output_json,
                agent_summary: None,
            })
        } else if stored.kind == APPROVAL_KIND_QUESTION {
            // A question is answered (SUBMITTED, checked against the jig) or
            // declined (REJECTED). Nothing else, and never a grant.
            match decision {
                "SUBMITTED" => {
                    let response_json = response_json.ok_or_else(|| {
                        OrchestratorError::InvalidContract(
                            "response is required to answer a question".into(),
                        )
                    })?;
                    crate::hitl::validate_question_answer(
                        &stored.requested_args_json,
                        response_json,
                    )?;
                    row_payload = Some(response_json.to_string());
                }
                "REJECTED" => {
                    declined_reason = Some(vam_spec::outbox::DECLINED_ANSWER.to_string());
                }
                _ => {
                    return Err(OrchestratorError::InvalidContract(
                        "a question is answered with SUBMITTED and a response, or declined with REJECTED"
                            .into(),
                    ));
                }
            }
            None
        } else if stored.kind == crate::hitl::APPROVAL_KIND_REAUTH {
            // A reconnect resolves the park (never a grant: the call is decided again, like any
            // other, once it is re-run). A person may only decline.
            match decision {
                crate::hitl::DECISION_RECONNECTED => {}
                "REJECTED" => {
                    if rejection_reason.filter(|r| !r.is_empty()).is_none() {
                        return Err(OrchestratorError::InvalidContract(
                            "rejection_reason is required for REJECTED decisions".into(),
                        ));
                    }
                }
                _ => {
                    return Err(OrchestratorError::InvalidContract(
                        "this call waits for its connection to be signed in again; \
                         reconnect it, or decline with REJECTED"
                            .into(),
                    ));
                }
            }
            None
        } else {
            if decision == "SUBMITTED" || decision == crate::hitl::DECISION_RECONNECTED {
                return Err(OrchestratorError::InvalidContract(
                    "SUBMITTED is only valid for a gate approval or a question".into(),
                ));
            }
            if decision == "REJECTED"
                && rejection_reason
                    .filter(|reason| !reason.is_empty())
                    .is_none()
            {
                return Err(OrchestratorError::InvalidContract(
                    "rejection_reason is required for REJECTED decisions".into(),
                ));
            }
            if decision == "AMENDED" {
                let amended = amended_args_json.ok_or_else(|| {
                    OrchestratorError::InvalidContract(
                        "amended_args is required for AMENDED decisions".into(),
                    )
                })?;
                let schema: serde_json::Value = serde_json::from_str(&stored.amendable_schema_json)
                    .map_err(|e| {
                        OrchestratorError::InvalidContract(format!("amendable_schema corrupt: {e}"))
                    })?;
                let amended_value: serde_json::Value =
                    serde_json::from_str(amended).map_err(|e| {
                        OrchestratorError::InvalidContract(format!(
                            "amended_args invalid JSON: {e}"
                        ))
                    })?;
                if stored.card_json.is_some() {
                    // A card approval: only the card's submit fields change, each checked
                    // against the tool's input schema; the run gets requested args with those
                    // fields replaced, and the grant is for that merged digest.
                    let merged = crate::hitl::merge_card_amend(
                        &stored.requested_args_json,
                        &amended_value,
                        &schema,
                    )?;
                    row_payload = Some(merged.to_string());
                } else {
                    validate_amended_args(&amended_value, &schema)?;
                }
                if stored.card_json.is_some() {
                    // merged above
                } else if stored.tool_name == crate::playbook_activation::START_PLAYBOOK_TOOL {
                    crate::hitl::validate_start_playbook_amend(
                        &stored.requested_args_json,
                        &amended_value,
                        &schema,
                    )?;
                }
                if row_payload.is_none() {
                    row_payload = Some(amended.to_string());
                }
            }
            None
        };

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let record = transaction
            .query_row(
                "SELECT run_id, step_id, attempt_id, checkpoint_digest, continuation_generation,
                        decision, decision_revision, session_id, tool_name, args_digest,
                        amendable_schema_json, published
                 FROM approvals WHERE request_id = ?1",
                [&request_id.urn],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)?,
                        row.get::<_, Option<String>>(7)?.unwrap_or_default(),
                        row.get::<_, String>(8)?,
                        row.get::<_, String>(9)?,
                        row.get::<_, String>(10)?,
                        row.get::<_, i64>(11)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| OrchestratorError::ApprovalNotFound(request_id.urn.clone()))?;

        // A finished run's receipt is signed and immutable; a decision must not
        // reopen the run. Checked in this transaction so no decision lands after.
        let run_state: Option<String> = transaction
            .query_row(
                "SELECT state FROM runs WHERE run_id = ?1",
                [&record.0],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(run_state) = run_state.filter(|s| crate::receipt::is_terminal(s)) {
            return Err(OrchestratorError::Conflict(format!(
                "run '{}' is {run_state}; a decision on a finished run is refused",
                record.0
            )));
        }

        if record.11 != 1 {
            return Err(OrchestratorError::ApprovalNotFound(request_id.urn.clone()));
        }
        if record.5.is_some() {
            return Err(OrchestratorError::ApprovalAlreadyDecided(
                request_id.urn.clone(),
            ));
        }
        if let Some(expected) = expected_revision {
            if expected != record.6 {
                return Err(OrchestratorError::StaleDecisionRevision {
                    expected,
                    actual: record.6,
                });
            }
        }

        let amended_digest = if let Some(payload) = row_payload.as_deref() {
            let value: serde_json::Value = serde_json::from_str(payload).map_err(|e| {
                OrchestratorError::InvalidContract(format!("approval payload invalid: {e}"))
            })?;
            Some(crate::events::args_digest_from_json(&value))
        } else if decision == "APPROVED" {
            Some(record.9.clone())
        } else {
            None
        };

        let new_revision = record.6 + 1;
        let next_continuation = record.4 + 1;

        transaction.execute(
            "UPDATE approvals SET decision = ?1, consumed_token_id = ?2,
             reviewer_principal = ?3, decision_revision = ?4,
             amended_args_json = ?5, amended_args_digest = ?6, rejection_reason = ?7,
             decided_at = ?9, decided_surface = ?10, decided_by_user_id = ?11
             WHERE request_id = ?8 AND decision IS NULL",
            params![
                decision,
                token_id.urn,
                reviewer_principal,
                new_revision,
                row_payload.as_deref().or(amended_args_json),
                amended_digest,
                declined_reason
                    .as_deref()
                    .filter(|_| rejection_reason.is_none_or(str::is_empty))
                    .or(rejection_reason),
                request_id.urn,
                unix_timestamp(),
                surface,
                caller_user_id,
            ],
        )?;

        crate::notifications::resolve(
            &transaction,
            &request_id.urn,
            "acted",
            Some(crate::notifications::Acted {
                surface,
                by_user_id: caller_user_id,
            }),
        )?;

        if let Some(completion) = gate_completion.as_ref() {
            record_open_gate_outcome(&transaction, &record.0, &record.1, completion)?;
        }

        // A denial settles the call's journal row in the same transaction: the call never ran.
        if !is_gate && decision == "REJECTED" {
            fail_journal_row_for_approval(&transaction, &request_id.urn, "denied")?;
        }

        if !is_gate
            && matches!(
                decision,
                "APPROVED" | "AMENDED" | crate::hitl::DECISION_RECONNECTED
            )
        {
            // A reconnect is not a permission: it resumes the call but never mints a grant.
            if decision != crate::hitl::DECISION_RECONNECTED {
                let digest = amended_digest.as_deref().unwrap_or(&record.9);
                transaction.execute(
                    "INSERT OR IGNORE INTO hitl_grants (session_id, tool_name, args_digest)
                     VALUES (?1, ?2, ?3)",
                    params![record.7, record.8, digest],
                )?;
            }
            transaction.execute(
                "UPDATE attempts SET continuation_generation = ?1
                 WHERE run_id = ?2 AND step_id = ?3 AND attempt_id = ?4",
                params![next_continuation, record.0, record.1, record.2],
            )?;
        }

        increment_revision(&transaction, &record.0)?;
        let sequence: i64 = transaction.query_row(
            "SELECT IFNULL(MAX(sequence), 0) + 1 FROM workflow_outbox WHERE run_id = ?1",
            [&record.0],
            |row| row.get(0),
        )?;
        let args_digest_for_event = amended_digest.as_deref().unwrap_or(&record.9);
        let event_json = hitl_decided_json(
            &record.0,
            sequence,
            &request_id.urn,
            decision,
            reviewer_principal,
            args_digest_for_event,
            new_revision,
        )
        .map_err(OrchestratorError::InvalidContract)?;
        append_outbox(&transaction, &record.0, "vam.v1.hitl.decided", &event_json)?;
        transaction.commit()?;

        Ok(ApprovalRecord {
            request_id: request_id.urn.clone(),
            run_id: record.0,
            step_id: record.1,
            attempt_id: record.2,
            session_id: record.7,
            checkpoint_digest: record.3,
            continuation_generation: next_continuation,
            tool_name: record.8,
            args_digest: args_digest_for_event.to_string(),
            decision_revision: new_revision,
        })
    }

    /// Copy approval grants onto a resumed session so the decision still applies
    /// after the parked VM is replaced.
    pub fn copy_session_hitl_grants(
        &self,
        from_session: &str,
        to_session: &str,
    ) -> Result<(), OrchestratorError> {
        if from_session == to_session || from_session.is_empty() || to_session.is_empty() {
            return Ok(());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT OR IGNORE INTO hitl_grants (session_id, tool_name, args_digest)
             SELECT ?1, tool_name, args_digest FROM hitl_grants WHERE session_id = ?2",
            params![to_session, from_session],
        )?;
        Ok(())
    }

    pub fn has_hitl_grant(&self, session_id: &str, tool_name: &str, args_digest: &str) -> bool {
        let connection = match self.connection.lock() {
            Ok(c) => c,
            Err(_) => return false,
        };
        connection
            .query_row(
                "SELECT 1 FROM hitl_grants WHERE session_id = ?1 AND tool_name = ?2 AND args_digest = ?3",
                params![session_id, tool_name, args_digest],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Move every published, undecided approval past its `expires_at` to `EXPIRED`.
    ///
    /// DB-only: this does not touch sessions or workspace locks. The caller
    /// (see `manager::sweep_expired_approvals`) uses the returned session and
    /// workspace ids to terminate the parked VM and release its write lock.
    pub fn expire_stale_approvals(&self) -> Result<Vec<ExpiredApproval>, OrchestratorError> {
        let now = unix_timestamp();
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let mut stmt = transaction.prepare(
            "SELECT a.request_id, a.run_id, a.step_id, a.attempt_id, a.session_id, s.workspace_id,
                    COALESCE(a.kind, 'tool_call')
             FROM approvals a
             LEFT JOIN sessions s ON s.session_id = a.session_id
             WHERE a.decision IS NULL AND a.published = 1 AND a.expires_at IS NOT NULL AND a.expires_at <= ?1",
        )?;
        // (record returned to the caller, step_id, attempt_id, kind)
        let expired: Vec<(ExpiredApproval, String, String, String)> = stmt
            .query_map([now], |row| {
                Ok((
                    ExpiredApproval {
                        request_id: row.get(0)?,
                        run_id: row.get(1)?,
                        session_id: row.get(4)?,
                        workspace_id: row.get(5)?,
                    },
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(6)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut expired_out = Vec::new();
        for (expired_row, step_id, attempt_id, kind) in &expired {
            if kind == APPROVAL_KIND_GATE
                && gate_on_timeout_label(&transaction, &expired_row.run_id, step_id)?.is_some()
            {
                // `apply_gate_timeouts` records the outcome and resumes.
                continue;
            }
            let request_id = &expired_row.request_id;
            let run_id = &expired_row.run_id;
            transaction.execute(
                "UPDATE approvals SET decision = 'EXPIRED', decision_revision = decision_revision + 1,
                 decided_at = ?2, decided_surface = 'system'
                 WHERE request_id = ?1 AND decision IS NULL",
                params![request_id, now],
            )?;
            crate::notifications::resolve(&transaction, request_id, "expired", None)?;
            increment_revision(&transaction, run_id)?;
            let sequence: i64 = transaction.query_row(
                "SELECT IFNULL(MAX(sequence), 0) + 1 FROM workflow_outbox WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )?;
            let event_json = hitl_expired_json(run_id, sequence, request_id, step_id, attempt_id)
                .map_err(OrchestratorError::InvalidContract)?;
            append_outbox(&transaction, run_id, "vam.v1.hitl.expired", &event_json)?;
            transaction.execute(
                "UPDATE attempts SET state = ?1 WHERE run_id = ?2 AND step_id = ?3 AND attempt_id = ?4",
                params![
                    AttemptState::Failed.as_str(),
                    run_id,
                    step_id,
                    attempt_id
                ],
            )?;
            let run_state = if kind == APPROVAL_KIND_GATE {
                "EXPIRED"
            } else {
                "FAILED"
            };
            transaction.execute(
                "UPDATE runs SET state = ?1, revision = revision + 1 WHERE run_id = ?2",
                params![run_state, run_id],
            )?;
            if kind == APPROVAL_KIND_GATE {
                transaction.execute(
                    "UPDATE run_playbook SET state = 'failed' WHERE run_id = ?1 AND state = 'active'",
                    [run_id],
                )?;
            }
            // A parked run's workspace reservation ends with the wait.
            transaction.execute("DELETE FROM run_reservations WHERE run_id = ?1", [run_id])?;
            transaction.execute(
                "UPDATE runs SET warm_until = NULL WHERE run_id = ?1",
                [run_id],
            )?;
            expired_out.push(expired_row.clone());
        }
        transaction.commit()?;
        Ok(expired_out)
    }

    /// The user's stop of a parked run: every undecided approval of the run is rejected
    /// with the reason "stopped by user" (a question is cancelled), `hitl.decided` is
    /// emitted, and nothing resumes. The write is a compare-and-set on `decision IS NULL`,
    /// the same one a human decision makes, so a decision and a stop cannot both win.
    pub fn reject_pending_for_stop(
        &self,
        run_id: &str,
        decider_user_id: &str,
        reviewer_principal: &str,
        surface: &str,
    ) -> Result<StopRejection, OrchestratorError> {
        let now = unix_timestamp();
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        #[allow(clippy::type_complexity)] // one-off SQL row tuple; local to this query
        let pending: Vec<(String, String, String, Option<String>, String, String, i64)> = {
            let mut stmt = transaction.prepare(
                "SELECT request_id, step_id, attempt_id, session_id, COALESCE(kind, 'tool_call'),
                        args_digest, decision_revision
                 FROM approvals
                 WHERE run_id = ?1 AND decision IS NULL AND published = 1
                 ORDER BY request_id",
            )?;
            let rows = stmt
                .query_map([run_id], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut out = StopRejection::default();
        for (request_id, step_id, attempt_id, session_id, kind, args_digest, revision) in pending {
            let decision = if kind == crate::hitl::APPROVAL_KIND_QUESTION {
                "CANCELLED"
            } else {
                "REJECTED"
            };
            let new_revision = revision + 1;
            let changed = transaction.execute(
                "UPDATE approvals SET decision = ?2, decision_revision = ?3, reviewer_principal = ?4,
                     rejection_reason = ?5, decided_at = ?6, decided_surface = ?7,
                     decided_by_user_id = ?8
                 WHERE request_id = ?1 AND decision IS NULL",
                params![
                    request_id,
                    decision,
                    new_revision,
                    reviewer_principal,
                    crate::session_end::STOPPED_BY_USER,
                    now,
                    surface,
                    decider_user_id
                ],
            )?;
            if changed == 0 {
                out.lost += 1;
                continue;
            }
            crate::notifications::resolve(&transaction, &request_id, "stopped", None)?;
            if decision == "REJECTED" {
                fail_journal_row_for_approval(&transaction, &request_id, "stopped")?;
            }
            increment_revision(&transaction, run_id)?;
            let sequence: i64 = transaction.query_row(
                "SELECT IFNULL(MAX(sequence), 0) + 1 FROM workflow_outbox WHERE run_id = ?1",
                [run_id],
                |row| row.get(0),
            )?;
            let event_json = hitl_decided_json(
                run_id,
                sequence,
                &request_id,
                decision,
                reviewer_principal,
                &args_digest,
                new_revision,
            )
            .map_err(OrchestratorError::InvalidContract)?;
            append_outbox(&transaction, run_id, "vam.v1.hitl.decided", &event_json)?;
            transaction.execute(
                "UPDATE attempts SET state = ?1 WHERE run_id = ?2 AND step_id = ?3 AND attempt_id = ?4",
                params![AttemptState::Failed.as_str(), run_id, step_id, attempt_id],
            )?;
            if kind == APPROVAL_KIND_GATE {
                transaction.execute(
                    "UPDATE run_playbook SET state = 'failed' WHERE run_id = ?1 AND state = 'active'",
                    [run_id],
                )?;
            }
            out.stopped.push(StoppedApproval {
                request_id,
                step_id,
                session_id,
                kind,
                decision: decision.to_string(),
            });
        }
        transaction.commit()?;
        Ok(out)
    }

    /// The kind of the run's oldest published approval that nobody has decided
    /// (`tool_call`, `gate` or `question`); `None` when nothing is pending.
    pub fn pending_approval_kind(&self, run_id: &str) -> Option<String> {
        let connection = self.connection.lock().ok()?;
        connection
            .query_row(
                "SELECT COALESCE(kind, 'tool_call') FROM approvals
                 WHERE run_id = ?1 AND decision IS NULL AND published = 1
                 ORDER BY created_at, request_id LIMIT 1",
                [run_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    /// The undecided reauth parks waiting on one connection: `(request id, run owner)`. The
    /// connection is named by its `reauth:<scope>:<owner>:<id>` key, so a reconnect of another
    /// owner's connection with the same id resolves nothing.
    pub fn pending_reauth_approvals(
        &self,
        connection_key: &str,
    ) -> Result<Vec<(String, String)>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT request_id, owner_user_id FROM approvals
             WHERE kind = 'reauth' AND decision IS NULL AND published = 1 AND locked_reason = ?1
             ORDER BY created_at, request_id",
        )?;
        let rows = stmt
            .query_map([connection_key], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// A stop of this run already rejected (or cancelled) one of its approvals.
    pub fn has_stop_rejection(&self, run_id: &str) -> bool {
        let Ok(connection) = self.connection.lock() else {
            return false;
        };
        connection
            .query_row(
                "SELECT 1 FROM approvals WHERE run_id = ?1 AND decision IN ('REJECTED','CANCELLED')
                   AND rejection_reason = ?2 LIMIT 1",
                params![run_id, crate::session_end::STOPPED_BY_USER],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Record `on_timeout` for due gate approvals and return the ones that
    /// should resume. Gates with no `on_timeout` are left for
    /// [`Self::expire_stale_approvals`].
    pub fn apply_gate_timeouts(&self) -> Result<Vec<ApprovalRecord>, OrchestratorError> {
        let now = unix_timestamp();
        let due = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            let mut stmt = connection.prepare(
                "SELECT request_id FROM approvals
                 WHERE decision IS NULL AND published = 1 AND kind = 'gate'
                   AND expires_at IS NOT NULL AND expires_at <= ?1
                 ORDER BY request_id ASC",
            )?;
            let rows = stmt.query_map([now], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(OrchestratorError::Database)?
        };
        let mut resumes = Vec::new();
        for request_id in due {
            if let Some(record) = self.apply_one_gate_timeout(&request_id)? {
                resumes.push(record);
            }
        }
        Ok(resumes)
    }

    fn apply_one_gate_timeout(
        &self,
        request_id: &str,
    ) -> Result<Option<ApprovalRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let row = transaction
            .query_row(
                "SELECT run_id, step_id, attempt_id, checkpoint_digest, continuation_generation,
                        session_id, decision_revision
                 FROM approvals
                 WHERE request_id = ?1 AND decision IS NULL AND published = 1 AND kind = 'gate'",
                [request_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, i64>(4)?,
                        row.get::<_, Option<String>>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            run_id,
            step_id,
            attempt_id,
            checkpoint_digest,
            continuation,
            session_id,
            revision,
        )) = row
        else {
            return Ok(None);
        };
        let Some(outcome) = gate_on_timeout_label(&transaction, &run_id, &step_id)? else {
            return Ok(None);
        };
        let output_json = serde_json::json!({ "on_timeout": outcome }).to_string();
        record_open_gate_outcome(
            &transaction,
            &run_id,
            &step_id,
            &PlaybookStepCompletion {
                outcome: outcome.clone(),
                output_json: Some(output_json.clone()),
                agent_summary: None,
            },
        )?;
        let new_revision = revision + 1;
        let next_continuation = continuation + 1;
        let digest = crate::events::args_digest_from_json(&serde_json::json!({
            "on_timeout": outcome
        }));
        transaction.execute(
            "UPDATE approvals
             SET decision = 'SUBMITTED', decision_revision = ?1,
                 amended_args_json = ?2, amended_args_digest = ?3,
                 reviewer_principal = 'system', consumed_token_id = ?4,
                 decided_at = ?6, decided_surface = 'system'
             WHERE request_id = ?5 AND decision IS NULL",
            params![
                new_revision,
                output_json,
                digest,
                format!("urn:vam:token:gate-timeout:{request_id}"),
                request_id,
                unix_timestamp(),
            ],
        )?;
        crate::notifications::resolve(&transaction, request_id, "timed_out", None)?;
        increment_revision(&transaction, &run_id)?;
        let playbook_state: Option<String> = transaction
            .query_row(
                "SELECT state FROM run_playbook WHERE run_id = ?1",
                [&run_id],
                |row| row.get(0),
            )
            .optional()?;
        let resume = match playbook_state.as_deref() {
            Some(PLAYBOOK_RUN_SUCCEEDED) => {
                transaction.execute(
                    "UPDATE runs SET state = 'SUCCEEDED', revision = revision + 1, warm_until = NULL
                     WHERE run_id = ?1",
                    [&run_id],
                )?;
                transaction.execute("DELETE FROM run_reservations WHERE run_id = ?1", [&run_id])?;
                None
            }
            Some(PLAYBOOK_RUN_FAILED) => {
                transaction.execute(
                    "UPDATE runs SET state = 'FAILED', revision = revision + 1, warm_until = NULL
                     WHERE run_id = ?1",
                    [&run_id],
                )?;
                transaction.execute("DELETE FROM run_reservations WHERE run_id = ?1", [&run_id])?;
                None
            }
            _ => {
                let run_state: String = transaction.query_row(
                    "SELECT state FROM runs WHERE run_id = ?1",
                    [&run_id],
                    |row| row.get(0),
                )?;
                let session_id = session_id.unwrap_or_default();
                if run_state == "PAUSED" && !session_id.is_empty() {
                    Some(ApprovalRecord {
                        request_id: request_id.to_string(),
                        run_id: run_id.clone(),
                        step_id: step_id.clone(),
                        attempt_id,
                        session_id,
                        checkpoint_digest,
                        continuation_generation: next_continuation,
                        tool_name: String::new(),
                        args_digest: digest,
                        decision_revision: new_revision,
                    })
                } else {
                    None
                }
            }
        };
        transaction.commit()?;
        Ok(resume)
    }

    /// Park a run whose only remaining work is an open gate.
    ///
    /// Returns the keep-warm deadline. `None` when a task step is still
    /// available or no gate is open. The existing keep-warm sweeper shuts the
    /// VM down and reserves the workspace.
    pub fn park_for_open_gates(&self, run_id: &str) -> Result<Option<i64>, OrchestratorError> {
        if self.get_playbook_run(run_id)?.is_none() {
            return Ok(None);
        }
        let steps = self.list_playbook_steps(run_id)?;
        if !only_open_gates_remain(&steps) {
            return Ok(None);
        }
        let open: Vec<&PlaybookStepRecord> = steps
            .iter()
            .filter(|step| step.state == PlaybookStepState::Open)
            .collect();
        let spec = self.get_playbook_spec(run_id)?;
        let mut keep_warm = None;
        for step in &open {
            if let Some(spec_step) = spec
                .steps
                .iter()
                .find(|candidate| candidate.id == step.step_id)
            {
                let secs = gate_keep_warm_secs(spec_step);
                keep_warm = Some(
                    keep_warm
                        .map(|current: i64| current.min(secs))
                        .unwrap_or(secs),
                );
            }
        }
        let keep_warm = keep_warm.unwrap_or(DEFAULT_KEEP_WARM_SECS);
        let (_, session_id, _) = self.get_run_spec_context(run_id)?;
        if let Some(session_id) = session_id.filter(|session_id| !session_id.is_empty()) {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            connection.execute(
                "UPDATE approvals SET session_id = ?1
                 WHERE run_id = ?2 AND kind = 'gate' AND decision IS NULL
                   AND (session_id IS NULL OR session_id = '')",
                params![session_id, run_id],
            )?;
        }
        self.update_run_state(run_id, "PAUSED")?;
        let deadline = self.arm_keep_warm(run_id, keep_warm)?;
        Ok(Some(deadline))
    }

    /// Arm the keep-warm timer for a run that just entered `PAUSED`.
    ///
    /// Returns the unix deadline (`now + keep_warm_secs`).
    pub fn arm_keep_warm(
        &self,
        run_id: &str,
        keep_warm_secs: i64,
    ) -> Result<i64, OrchestratorError> {
        let until = unix_timestamp().saturating_add(keep_warm_secs.max(0));
        self.schedule_keep_warm(run_id, until)?;
        Ok(until)
    }

    /// Set the keep-warm deadline explicitly. Tests use a past timestamp.
    pub fn schedule_keep_warm(
        &self,
        run_id: &str,
        warm_until: i64,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE runs SET warm_until = ?1 WHERE run_id = ?2",
            params![warm_until, run_id],
        )?;
        if changed == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "run '{run_id}' not found"
            )));
        }
        Ok(())
    }

    /// Drop the keep-warm deadline after the run has been parked or left `PAUSED`.
    pub fn clear_keep_warm(&self, run_id: &str) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE runs SET warm_until = NULL WHERE run_id = ?1",
            [run_id],
        )?;
        Ok(())
    }

    pub fn keep_warm_deadline(&self, run_id: &str) -> Result<Option<i64>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        match connection.query_row(
            "SELECT warm_until FROM runs WHERE run_id = ?1",
            [run_id],
            |row| row.get::<_, Option<i64>>(0),
        ) {
            Ok(deadline) => Ok(deadline),
            Err(rusqlite::Error::QueryReturnedNoRows) => Err(OrchestratorError::NotFound(format!(
                "run '{run_id}' not found"
            ))),
            Err(err) => Err(OrchestratorError::Database(err)),
        }
    }

    /// Paused runs whose keep-warm deadline has passed and whose approval is
    /// still undecided. One row per run.
    pub fn runs_due_to_park(&self, now: i64) -> Result<Vec<ParkCandidate>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT r.run_id, a.session_id, s.workspace_id, a.expires_at
             FROM runs r
             INNER JOIN approvals a ON a.run_id = r.run_id
             INNER JOIN sessions s ON s.session_id = a.session_id
             WHERE r.state = 'PAUSED'
               AND r.warm_until IS NOT NULL
               AND r.warm_until <= ?1
               AND a.decision IS NULL
               AND a.published = 1
               AND a.session_id IS NOT NULL
               AND s.workspace_id != ''",
        )?;
        let rows = stmt.query_map([now], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<i64>>(3)?,
            ))
        })?;
        let mut by_run: std::collections::HashMap<String, ParkCandidate> =
            std::collections::HashMap::new();
        for row in rows {
            let (run_id, session_id, workspace_id, expires_at) = row?;
            let reservation_expires_at = expires_at.unwrap_or(i64::MAX);
            match by_run.get_mut(&run_id) {
                Some(existing) if reservation_expires_at < existing.reservation_expires_at => {
                    existing.reservation_expires_at = reservation_expires_at;
                    existing.session_id = session_id;
                    existing.workspace_id = workspace_id;
                }
                Some(_) => {}
                None => {
                    by_run.insert(
                        run_id.clone(),
                        ParkCandidate {
                            run_id,
                            session_id,
                            workspace_id,
                            reservation_expires_at,
                        },
                    );
                }
            }
        }
        let mut due: Vec<_> = by_run.into_values().collect();
        due.sort_by(|a, b| a.run_id.cmp(&b.run_id));
        Ok(due)
    }

    /// Hold `workspace_id` for `run_id` until `expires_at`.
    ///
    /// A second reserve of the same run refreshes the expiry and keeps the
    /// original `created_at`. A different run already holding the workspace
    /// is a conflict.
    pub fn reserve_workspace(
        &self,
        workspace_id: &str,
        run_id: &str,
        expires_at: i64,
    ) -> Result<(), OrchestratorError> {
        if workspace_id.is_empty() || run_id.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "workspace reservation requires a workspace and a run".into(),
            ));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let existing: Option<(String, i64)> = transaction
            .query_row(
                "SELECT run_id, created_at FROM run_reservations WHERE workspace_id = ?1",
                [workspace_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((holder, _)) if holder != run_id => {
                return Err(OrchestratorError::Conflict(format!(
                    "workspace '{workspace_id}' is already reserved by run '{holder}'"
                )));
            }
            Some(_) => {
                transaction.execute(
                    "UPDATE run_reservations SET expires_at = ?1 WHERE workspace_id = ?2 AND run_id = ?3",
                    params![expires_at, workspace_id, run_id],
                )?;
            }
            None => {
                transaction.execute(
                    "INSERT INTO run_reservations (workspace_id, run_id, created_at, expires_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![workspace_id, run_id, unix_timestamp(), expires_at],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn workspace_reservation(
        &self,
        workspace_id: &str,
    ) -> Result<Option<WorkspaceReservation>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT workspace_id, run_id, created_at, expires_at
                 FROM run_reservations WHERE workspace_id = ?1",
                [workspace_id],
                |row| {
                    Ok(WorkspaceReservation {
                        workspace_id: row.get(0)?,
                        run_id: row.get(1)?,
                        created_at: row.get(2)?,
                        expires_at: row.get(3)?,
                    })
                },
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    /// Every workspace reservation, ordered by workspace id.
    ///
    /// Startup reconciliation compares this before and after its pass. A parked
    /// run has no VM to adopt, so that pass must not drop or replace these rows.
    pub(crate) fn list_workspace_reservations(
        &self,
    ) -> Result<Vec<WorkspaceReservation>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT workspace_id, run_id, created_at, expires_at
             FROM run_reservations
             ORDER BY workspace_id ASC",
        )?;
        let rows = stmt.query_map([], |row| {
            Ok(WorkspaceReservation {
                workspace_id: row.get(0)?,
                run_id: row.get(1)?,
                created_at: row.get(2)?,
                expires_at: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    /// Reservation that still blocks `CreateRun`. An elapsed `expires_at` does not.
    pub fn active_workspace_reservation(
        &self,
        workspace_id: &str,
        now: i64,
    ) -> Result<Option<WorkspaceReservation>, OrchestratorError> {
        Ok(self
            .workspace_reservation(workspace_id)?
            .filter(|reservation| reservation.expires_at > now))
    }

    pub fn release_run_reservation(&self, run_id: &str) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute("DELETE FROM run_reservations WHERE run_id = ?1", [run_id])?;
        Ok(())
    }

    /// Open an active segment at `now` when one is not already open.
    ///
    /// A second call while the segment is open leaves the original start in
    /// place. Parked time is the gap after [`Self::end_active_segment`].
    pub fn begin_active_segment(&self, run_id: &str, now: i64) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let changed = connection.execute(
            "UPDATE runs SET active_since = ?1 WHERE run_id = ?2 AND active_since IS NULL",
            params![now, run_id],
        )?;
        if changed == 0 {
            let exists: Option<i64> = connection
                .query_row("SELECT 1 FROM runs WHERE run_id = ?1", [run_id], |row| {
                    row.get(0)
                })
                .optional()?;
            if exists.is_none() {
                return Err(OrchestratorError::NotFound(format!(
                    "run '{run_id}' not found"
                )));
            }
        }
        Ok(())
    }

    /// Close the open segment at `now` and return the accumulated active seconds.
    ///
    /// A closed segment stays closed: a later call does not add the gap, so
    /// parked time is not counted.
    pub fn end_active_segment(&self, run_id: &str, now: i64) -> Result<i64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let (elapsed, since): (i64, Option<i64>) = transaction
            .query_row(
                "SELECT IFNULL(active_elapsed_secs, 0), active_since FROM runs WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|err| map_missing_run(run_id, err))?;
        let open = since.map(|start| (now - start).max(0)).unwrap_or(0);
        let total = elapsed.saturating_add(open);
        transaction.execute(
            "UPDATE runs SET active_elapsed_secs = ?1, active_since = NULL WHERE run_id = ?2",
            params![total, run_id],
        )?;
        transaction.commit()?;
        Ok(total)
    }

    /// Closed active seconds plus the open segment up to `now`.
    pub fn active_elapsed_secs(&self, run_id: &str, now: i64) -> Result<i64, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let (elapsed, since): (i64, Option<i64>) = connection
            .query_row(
                "SELECT IFNULL(active_elapsed_secs, 0), active_since FROM runs WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|err| map_missing_run(run_id, err))?;
        let open = since.map(|start| (now - start).max(0)).unwrap_or(0);
        Ok(elapsed.saturating_add(open))
    }

    /// Seconds of `max_duration` still available at `now`.
    ///
    /// `None` when the run has no positive `max_duration` (callers keep the
    /// historical per-task default). `Some(0)` means active time is exhausted.
    /// Parked gaps are absent from the elapsed total.
    pub fn remaining_active_budget_secs(
        &self,
        run_id: &str,
        now: i64,
    ) -> Result<Option<u64>, OrchestratorError> {
        let (spec_json, _, _) = self.get_run_spec_context(run_id)?;
        let Some(budget) = positive_max_duration_secs(spec_json.as_deref()) else {
            return Ok(None);
        };
        let elapsed = self.active_elapsed_secs(run_id, now)?;
        Ok(Some(budget.saturating_sub(elapsed) as u64))
    }

    /// Fail `RUNNING` runs whose active time has reached `max_duration`.
    ///
    /// `PAUSED` runs are left alone: the clock is stopped for keep-warm and
    /// parked waits. Each returned row has already lost its reservation and
    /// keep-warm deadline. A second pass returns nothing.
    pub fn expire_over_active_budget(
        &self,
        now: i64,
    ) -> Result<Vec<ActiveDurationExpiry>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let mut stmt = transaction.prepare(
            "SELECT r.run_id, r.session_id, r.spec_json, IFNULL(r.active_elapsed_secs, 0),
                    r.active_since, s.workspace_id
             FROM runs r
             LEFT JOIN sessions s ON s.session_id = r.session_id
             WHERE r.state = 'RUNNING'",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);

        let mut expired = Vec::new();
        for (run_id, session_id, spec_json, elapsed, since, workspace_id) in rows {
            let Some(budget) = positive_max_duration_secs(spec_json.as_deref()) else {
                continue;
            };
            let open = since.map(|start| (now - start).max(0)).unwrap_or(0);
            let active = elapsed.saturating_add(open);
            if active < budget {
                continue;
            }
            let result_json =
                serde_json::json!({ "error": crate::notifications::DURATION_STOP_ERROR })
                    .to_string();
            transaction.execute(
                "UPDATE runs
                 SET state = 'FAILED', revision = revision + 1, result_json = ?1,
                     active_elapsed_secs = ?2, active_since = NULL, warm_until = NULL
                 WHERE run_id = ?3 AND state = 'RUNNING'",
                params![result_json, active, run_id],
            )?;
            transaction.execute(
                "UPDATE attempts SET state = ?1
                 WHERE run_id = ?2 AND state IN ('RUNNING', 'SUSPENDING')",
                params![AttemptState::Failed.as_str(), run_id],
            )?;
            transaction.execute("DELETE FROM run_reservations WHERE run_id = ?1", [&run_id])?;
            append_outbox(&transaction, &run_id, "vam.v1.run.state", "FAILED")?;
            append_outbox(&transaction, &run_id, "vam.v1.run.completed", &result_json)?;
            crate::notifications::record_budget_hit(
                &transaction,
                &run_id,
                crate::notifications::BudgetPoint::Duration,
            )?;
            let workspace_id = workspace_id.filter(|id| !id.is_empty());
            expired.push(ActiveDurationExpiry {
                run_id,
                session_id,
                workspace_id,
            });
        }
        transaction.commit()?;
        Ok(expired)
    }

    pub fn find_pending_request_for_attempt(
        &self,
        key: &AttemptKey,
    ) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT request_id FROM hitl_pending
                 WHERE run_id = ?1 AND step_id = ?2 AND attempt_id = ?3
                 ORDER BY created_at DESC LIMIT 1",
                params![key.run_id, key.step_id, key.attempt_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    pub fn record_evaluation(
        &self,
        run_id: &ResourceId,
        result: &EvaluationResult,
    ) -> Result<(), OrchestratorError> {
        let assertion_id = result
            .assertion_id
            .as_ref()
            .map(|id| id.urn.as_str())
            .filter(|urn| !urn.is_empty());
        let evaluator_id = result
            .evaluator_ref
            .as_ref()
            .and_then(|reference| reference.resource_id.as_ref())
            .map(|id| id.urn.as_str())
            .filter(|urn| !urn.is_empty());
        if !result.authoritative || assertion_id.is_none() || evaluator_id.is_none() {
            return Err(OrchestratorError::UntrustedEvaluation);
        }

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT OR REPLACE INTO workflow_evaluations (
                run_id, assertion_id, evaluator_id, passed
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id.urn,
                assertion_id,
                evaluator_id,
                if result.passed { 1_i64 } else { 0_i64 },
            ],
        )?;
        increment_revision(&transaction, &run_id.urn)?;

        let eval_payload = serde_json::json!({
            "run_id": &run_id.urn,
            "assertion_id": assertion_id.unwrap_or_default(),
            "evaluator_id": evaluator_id.unwrap_or_default(),
            "passed": result.passed,
        })
        .to_string();

        append_outbox(
            &transaction,
            &run_id.urn,
            "vam.v1.evaluation.recorded",
            &eval_payload,
        )?;
        transaction.commit()?;

        // Broadcast to event bus and append to partitioned audit ledger
        if let Some(bus) = self.event_bus() {
            let cloud_event = crate::events::cloud_event_from_raw(
                &run_id.urn,
                0,
                "vam.v1.evaluation.recorded",
                &eval_payload,
            );
            bus.publish(cloud_event);
        }
        if let Some(ledger) = self.audit_ledger() {
            let _ = ledger.record_event(
                "vam_audit_evaluation",
                "vam.v1.evaluation.recorded",
                eval_payload.as_bytes(),
            );
        }

        Ok(())
    }

    pub fn get_evaluations_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<(String, String, bool)>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "SELECT assertion_id, evaluator_id, passed FROM workflow_evaluations WHERE run_id = ?1",
        )?;
        let rows = stmt.query_map([run_id], |row| {
            let passed: i64 = row.get(2)?;
            Ok((row.get(0)?, row.get(1)?, passed != 0))
        })?;
        let mut results = Vec::new();
        for r in rows {
            results.push(r?);
        }
        Ok(results)
    }

    pub fn register_session_budget(
        &self,
        session_id: &str,
        budget: std::sync::Arc<vam_inference::SessionBudget>,
    ) {
        if let Ok(Some(run_id)) = self.find_run_id_for_session(session_id) {
            if let Ok(Some(metering)) = self.get_run_metering(&run_id) {
                budget.absorb_baseline(
                    metering.spend_micro,
                    metering.token_count,
                    metering.prompt_tokens,
                    metering.completion_tokens,
                );
            }
        }
        if let Ok(mut lock) = self.session_budgets.write() {
            lock.insert(session_id.to_string(), budget);
        }
    }

    /// Remember the session CA. Only the public PEM is readable; the key stays here.
    pub fn register_session_ca(
        &self,
        session_id: &str,
        ca: std::sync::Arc<vam_network::tls_ca::SessionCa>,
    ) {
        let mut lock = self
            .session_cas
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lock.insert(session_id.to_string(), ca);
    }

    /// Public CA certificate PEM, or `None` when this session has no CA in memory.
    pub fn session_ca_pem(&self, session_id: &str) -> Option<String> {
        let lock = self
            .session_cas
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lock.get(session_id).map(|ca| ca.ca_cert_pem())
    }

    /// Drop the in-memory CA when the session ends.
    pub fn clear_session_ca(&self, session_id: &str) {
        let mut lock = self
            .session_cas
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        lock.remove(session_id);
    }

    /// Persisted counters for `run_id`, or `None` when the run has not been metered.
    pub fn get_run_metering(&self, run_id: &str) -> Result<Option<RunMetering>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT iteration, spend_micro, token_count, prompt_tokens, completion_tokens
                 FROM run_metering WHERE run_id = ?1",
                [run_id],
                map_run_metering,
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    /// Point a run at a new session so a resumed VM loads that run's counters.
    pub fn set_run_session_id(
        &self,
        run_id: &str,
        session_id: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let updated = connection.execute(
            "UPDATE runs SET session_id = ?2 WHERE run_id = ?1",
            params![run_id, session_id],
        )?;
        if updated == 0 {
            return Err(OrchestratorError::InvalidContract(format!(
                "run not found: {run_id}"
            )));
        }
        Ok(())
    }

    /// Fold persisted run totals into `budget`, then record this call's iteration
    /// and the budget's spend and token counters.
    ///
    /// The returned row is what the PEP enforces. A new run starts at iteration 1.
    /// A later session continues from the stored iteration.
    pub fn apply_run_metering(
        &self,
        run_id: &str,
        budget: &vam_inference::SessionBudget,
    ) -> Result<RunMetering, OrchestratorError> {
        if run_id.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "run_id is empty".to_string(),
            ));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let existing = transaction
            .query_row(
                "SELECT iteration, spend_micro, token_count, prompt_tokens, completion_tokens
                 FROM run_metering WHERE run_id = ?1",
                [run_id],
                map_run_metering,
            )
            .optional()?;
        if let Some(metering) = existing.as_ref() {
            budget.absorb_baseline(
                metering.spend_micro,
                metering.token_count,
                metering.prompt_tokens,
                metering.completion_tokens,
            );
        } else {
            budget.absorb_baseline(0, 0, 0, 0);
        }
        transaction.execute(
            "INSERT INTO run_metering (
                run_id, iteration, spend_micro, token_count, prompt_tokens, completion_tokens
             ) VALUES (?1, 1, ?2, ?3, ?4, ?5)
             ON CONFLICT(run_id) DO UPDATE SET
                iteration = run_metering.iteration + 1,
                spend_micro = MAX(run_metering.spend_micro, excluded.spend_micro),
                token_count = MAX(run_metering.token_count, excluded.token_count),
                prompt_tokens = MAX(run_metering.prompt_tokens, excluded.prompt_tokens),
                completion_tokens = MAX(run_metering.completion_tokens, excluded.completion_tokens)",
            params![
                run_id,
                budget.get_accumulated_cost(),
                budget.get_accumulated_tokens(),
                budget.get_accumulated_prompt_tokens(),
                budget.get_accumulated_completion_tokens(),
            ],
        )?;
        let metering = transaction.query_row(
            "SELECT iteration, spend_micro, token_count, prompt_tokens, completion_tokens
             FROM run_metering WHERE run_id = ?1",
            [run_id],
            map_run_metering,
        )?;
        transaction.commit()?;
        Ok(metering)
    }

    /// Remember the session's thread-log recorder (replaces an earlier one).
    pub fn register_thread_recorder(
        &self,
        session_id: &str,
        recorder: std::sync::Arc<crate::thread_recorder::ThreadRecorder>,
    ) {
        if let Ok(mut lock) = self.thread_recorders.write() {
            lock.insert(session_id.to_string(), recorder);
        }
    }

    pub fn thread_recorder(
        &self,
        session_id: &str,
    ) -> Option<std::sync::Arc<crate::thread_recorder::ThreadRecorder>> {
        self.thread_recorders
            .read()
            .ok()
            .and_then(|lock| lock.get(session_id).cloned())
    }

    /// Drop the recorder when its session ends, and the cards kept for it.
    pub fn remove_thread_recorder(&self, session_id: &str) {
        crate::skill_cards::forget_session(session_id);
        if let Ok(mut lock) = self.thread_recorders.write() {
            lock.remove(session_id);
        }
    }

    pub fn get_session_budget(
        &self,
        session_id: &str,
    ) -> Option<std::sync::Arc<vam_inference::SessionBudget>> {
        self.session_budgets
            .read()
            .ok()
            .and_then(|lock| lock.get(session_id).cloned())
    }

    pub fn set_event_bus(&self, bus: std::sync::Arc<vam_telemetry::BroadcastEventBus>) {
        if let Ok(mut lock) = self.event_bus.write() {
            *lock = Some(bus);
        }
    }

    pub fn event_bus(&self) -> Option<std::sync::Arc<vam_telemetry::BroadcastEventBus>> {
        self.event_bus.read().ok().and_then(|lock| lock.clone())
    }

    pub fn set_audit_ledger(&self, ledger: std::sync::Arc<vam_telemetry::PartitionedAuditLedger>) {
        if let Ok(mut lock) = self.audit_ledger.write() {
            *lock = Some(ledger);
        }
    }

    pub fn audit_ledger(&self) -> Option<std::sync::Arc<vam_telemetry::PartitionedAuditLedger>> {
        self.audit_ledger.read().ok().and_then(|lock| lock.clone())
    }

    /// Records an audit log in the partitioned audit ledger (fail-closed, Decision T25).
    /// Ledger keys stay `tenant:{id}:{session}` (default tenant `local`).
    /// `user_id` is a record attribute from the session/run owner, never a key.
    pub fn record_audit_event(
        &self,
        session_id: &str,
        event_type: &str,
        payload: &[u8],
    ) -> Result<(), OrchestratorError> {
        let tenant = vam_telemetry::current_tenant_id();
        self.record_tenant_audit_event(tenant.as_str(), session_id, event_type, payload)
    }

    /// Best-effort host owner for audit attribution. Missing session/run → `None`.
    fn owner_user_id_for_audit_session(&self, session_id: &str) -> Option<String> {
        if let Ok(owner) = self.session_owner_user_id(session_id) {
            return Some(owner);
        }
        let run_id = self.find_run_id_for_session(session_id).ok().flatten()?;
        self.run_owner_user_id(&run_id).ok()
    }

    /// Records an audit log in a tenant-isolated partition (Phase 7.3 Multi-tenancy).
    pub fn record_tenant_audit_event(
        &self,
        tenant_id: &str,
        partition_name: &str,
        event_type: &str,
        payload: &[u8],
    ) -> Result<(), OrchestratorError> {
        if let Some(ledger) = self.audit_ledger() {
            let partition_key =
                vam_telemetry::tenant::TenantContext::partition_key(tenant_id, partition_name);
            let user_id = self.owner_user_id_for_audit_session(partition_name);
            let record = ledger
                .record_event_for_user(&partition_key, event_type, payload, user_id.as_deref())
                .map_err(|e| {
                    OrchestratorError::Core(format!(
                        "Tenant audit ledger append failed (fail-closed): {}",
                        e
                    ))
                })?;
            crate::evidence::persist_audit_record_to_spool(&record);
        }
        Ok(())
    }

    /// Append a single outbox row outside a workflow transaction (trajectory events).
    pub fn append_outbox_event(
        &self,
        run_id: &str,
        event_type: &str,
        payload: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO workflow_outbox (run_id, event_type, payload) VALUES (?1, ?2, ?3)",
            params![run_id, event_type, payload],
        )?;
        let seq = connection.last_insert_rowid();

        if let Some(bus) = self.event_bus() {
            let cloud_event = crate::events::cloud_event_from_raw(run_id, seq, event_type, payload);
            bus.publish(cloud_event);
        }

        Ok(())
    }

    pub fn read_outbox(&self, after_sequence: i64) -> Result<Vec<OutboxEvent>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(
            "SELECT sequence, run_id, event_type, payload
             FROM workflow_outbox
             WHERE sequence > ?1
             ORDER BY sequence ASC",
        )?;
        let rows = statement.query_map([after_sequence], |row| {
            Ok(OutboxEvent {
                sequence: row.get(0)?,
                run_id: row.get(1)?,
                event_type: row.get(2)?,
                payload: row.get(3)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::Database)
    }

    /// Persist step rows for a playbook run. Steps with no preconditions start
    /// `available`; the rest start `pending` until a completion makes them
    /// runnable or impossible.
    pub fn init_playbook_run(
        &self,
        run_id: &str,
        pin: &PlaybookRunPin,
        spec: &PlaybookSpec,
    ) -> Result<Vec<PlaybookStepRecord>, OrchestratorError> {
        require_nonempty(&pin.urn, "playbook urn")?;
        require_nonempty(&pin.version, "playbook version")?;
        require_nonempty(&pin.digest, "playbook digest")?;
        let mut spec = spec.clone();
        vam_spec::playbook::migrate_playbook_spec(&mut spec);
        let spec_json = vam_spec::encoding::proto_to_json("vam.v1.PlaybookSpec", &spec)
            .map_err(OrchestratorError::InvalidContract)?;
        let now = unix_timestamp();
        let mut steps = graph_from_spec(&spec)?;
        settle_pending(&mut steps, now, false);
        promote_available_gates(&mut steps, &spec);
        for step in &mut steps {
            step.transition_revision = 1;
            step.dirty = false;
        }

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let run_exists: bool = transaction
            .query_row("SELECT 1 FROM runs WHERE run_id = ?1", [run_id], |_| {
                Ok(true)
            })
            .optional()?
            .unwrap_or(false);
        if !run_exists {
            return Err(OrchestratorError::NotFound(format!(
                "run '{run_id}' not found"
            )));
        }
        let already: bool = transaction
            .query_row(
                "SELECT 1 FROM run_playbook WHERE run_id = ?1",
                [run_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if already {
            return Err(OrchestratorError::Conflict(format!(
                "playbook step state already exists for run '{run_id}'"
            )));
        }
        transaction.execute(
            "INSERT INTO run_playbook (
                run_id, playbook_urn, playbook_version, playbook_digest, spec_json, state
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                pin.urn,
                pin.version,
                pin.digest,
                spec_json,
                PLAYBOOK_RUN_ACTIVE,
            ],
        )?;
        insert_playbook_steps(&transaction, run_id, &steps)?;
        open_ready_gates(&transaction, run_id, &spec, &steps, now)?;
        for step in &steps {
            append_outbox(
                &transaction,
                run_id,
                "vam.v1.playbook.step",
                &step_event_payload(step),
            )?;
        }
        increment_revision(&transaction, run_id)?;
        transaction.commit()?;
        Ok(steps.iter().map(StepMem::to_record).collect())
    }

    /// Record an outcome for an `available` step and propagate skips.
    pub fn complete_playbook_step(
        &self,
        run_id: &str,
        step_id: &str,
        completion: &PlaybookStepCompletion,
    ) -> Result<Vec<PlaybookStepRecord>, OrchestratorError> {
        if completion.outcome.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "step outcome is required".to_string(),
            ));
        }
        let output_json = validate_output_json(completion.output_json.as_deref())?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let (spec_json, playbook_state) = load_playbook_header(&transaction, run_id)?;
        if playbook_state != PLAYBOOK_RUN_ACTIVE {
            return Err(OrchestratorError::InvalidState {
                expected: PLAYBOOK_RUN_ACTIVE,
                actual: playbook_state,
            });
        }
        let spec: PlaybookSpec =
            vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &spec_json)
                .map_err(OrchestratorError::CorruptState)?;
        let mut steps = load_playbook_steps(&transaction, run_id)?;
        attach_preconditions(&mut steps, &spec)?;
        let index = steps
            .iter()
            .position(|step| step.step_id == step_id)
            .ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "playbook step '{step_id}' not found on run '{run_id}'"
                ))
            })?;
        let spec_step = spec
            .steps
            .iter()
            .find(|step| step.id == step_id)
            .ok_or_else(|| {
                OrchestratorError::CorruptState(format!(
                    "stored playbook is missing step '{step_id}'"
                ))
            })?;
        if is_gate_step(spec_step) {
            return Err(OrchestratorError::InvalidContract(format!(
                "step '{step_id}' is a gate. step_complete cannot record its outcome. The host opens it for a human decision; submit that decision on the gate approval."
            )));
        }
        if steps[index].state != PlaybookStepState::Available {
            return Err(OrchestratorError::InvalidState {
                expected: "available",
                actual: steps[index].state.as_str().to_string(),
            });
        }
        vam_spec::validate_playbook_step_output(
            spec_step.output_schema.as_ref(),
            output_json.as_deref(),
        )
        .map_err(|err| OrchestratorError::InvalidContract(err.message))?;
        let terminal = outcome_terminality(&spec, spec_step, &completion.outcome)?;
        let evidence_refs =
            evidence_refs_for_completion(&transaction, run_id, spec_step, &completion.outcome)?;
        let now = unix_timestamp();
        mark_completed(
            &mut steps[index],
            completion.outcome.clone(),
            output_json,
            completion.agent_summary.clone(),
            evidence_refs,
            now,
        );
        let next_run_state = match terminal {
            OutcomeTerminality::Succeeded => {
                skip_unfinished(&mut steps, now);
                Some(PLAYBOOK_RUN_SUCCEEDED)
            }
            OutcomeTerminality::Failed => {
                skip_unfinished(&mut steps, now);
                Some(PLAYBOOK_RUN_FAILED)
            }
            OutcomeTerminality::Open => {
                settle_pending(&mut steps, now, true);
                promote_available_gates(&mut steps, &spec);
                None
            }
        };
        for step in steps.iter().filter(|step| step.dirty) {
            update_playbook_step(&transaction, run_id, step)?;
            append_outbox(
                &transaction,
                run_id,
                "vam.v1.playbook.step",
                &step_event_payload(step),
            )?;
        }
        if let Some(state) = next_run_state {
            transaction.execute(
                "UPDATE run_playbook SET state = ?1 WHERE run_id = ?2",
                params![state, run_id],
            )?;
            append_outbox(&transaction, run_id, "vam.v1.playbook.state", state)?;
        }
        open_ready_gates(&transaction, run_id, &spec, &steps, now)?;
        increment_revision(&transaction, run_id)?;
        transaction.commit()?;
        Ok(steps.iter().map(StepMem::to_record).collect())
    }

    /// The run inputs that playbook templates read (`{{inputs.…}}`).
    pub fn run_template_inputs(&self, run_id: &str) -> Option<prost_types::Struct> {
        let connection = self.connection.lock().ok()?;
        load_run_inputs(&connection, run_id)
    }

    pub fn list_playbook_steps(
        &self,
        run_id: &str,
    ) -> Result<Vec<PlaybookStepRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let steps = load_playbook_steps(&connection, run_id)?;
        Ok(steps.iter().map(StepMem::to_record).collect())
    }

    pub fn get_playbook_run(
        &self,
        run_id: &str,
    ) -> Result<Option<PlaybookRunRecord>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT run_id, playbook_urn, playbook_version, playbook_digest, state
                 FROM run_playbook WHERE run_id = ?1",
                [run_id],
                |row| {
                    Ok(PlaybookRunRecord {
                        run_id: row.get(0)?,
                        urn: row.get(1)?,
                        version: row.get(2)?,
                        digest: row.get(3)?,
                        state: row.get(4)?,
                    })
                },
            )
            .optional()
            .map_err(OrchestratorError::Database)
    }

    /// Load the playbook spec pinned on a run's step-state header.
    pub fn get_playbook_spec(&self, run_id: &str) -> Result<PlaybookSpec, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let (spec_json, _) = load_playbook_header(&connection, run_id)?;
        vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &spec_json)
            .map_err(OrchestratorError::CorruptState)
    }

    /// Whether the run waits only on open gates (see [`only_open_gates_remain`]).
    /// `false` for a run with no playbook. Does not mutate state.
    pub fn playbook_waits_only_on_gates(&self, run_id: &str) -> Result<bool, OrchestratorError> {
        if self.get_playbook_run(run_id)?.is_none() {
            return Ok(false);
        }
        Ok(only_open_gates_remain(&self.list_playbook_steps(run_id)?))
    }

    /// Compute whether a playbook run may finish.
    ///
    /// Missing items are required steps that are not completed or skipped, any
    /// gate still `open`, and run output that does not match
    /// `PlaybookSpec.output_schema`. `run_output_json` is the JSON object the
    /// guest proposes as the run output (`None` is an empty object when the
    /// schema is constrained).
    ///
    /// Returns `Ok(None)` when the run has no playbook. Does not mutate state.
    pub fn check_playbook_finish(
        &self,
        run_id: &str,
        run_output_json: Option<&str>,
    ) -> Result<Option<PlaybookFinishCheck>, OrchestratorError> {
        if self.get_playbook_run(run_id)?.is_none() {
            return Ok(None);
        }
        let spec = self.get_playbook_spec(run_id)?;
        let steps = self.list_playbook_steps(run_id)?;
        let optional_ids: HashSet<&str> = spec
            .steps
            .iter()
            .filter(|step| step.optional)
            .map(|step| step.id.as_str())
            .collect();
        let mut missing_steps = Vec::new();
        let mut open_gates = Vec::new();
        for step in &steps {
            match step.state {
                PlaybookStepState::Completed | PlaybookStepState::Skipped => {}
                PlaybookStepState::Open => {
                    open_gates.push(step.step_id.clone());
                    missing_steps.push(step.step_id.clone());
                }
                PlaybookStepState::Pending | PlaybookStepState::Available => {
                    if !optional_ids.contains(step.step_id.as_str()) {
                        missing_steps.push(step.step_id.clone());
                    }
                }
            }
        }
        let schema_error = match vam_spec::validate_playbook_run_output(
            spec.output_schema.as_ref(),
            run_output_json,
        ) {
            Ok(()) => None,
            Err(err) => Some(err.message),
        };
        if missing_steps.is_empty() && open_gates.is_empty() && schema_error.is_none() {
            Ok(Some(PlaybookFinishCheck::Complete))
        } else {
            Ok(Some(PlaybookFinishCheck::Incomplete {
                missing_steps,
                open_gates,
                schema_error,
            }))
        }
    }

    /// Persist the playbook header terminal state after a finish check.
    pub fn set_playbook_run_state(
        &self,
        run_id: &str,
        state: &str,
    ) -> Result<(), OrchestratorError> {
        if state != PLAYBOOK_RUN_SUCCEEDED && state != PLAYBOOK_RUN_FAILED {
            return Err(OrchestratorError::InvalidContract(format!(
                "playbook run state must be '{PLAYBOOK_RUN_SUCCEEDED}' or '{PLAYBOOK_RUN_FAILED}', got '{state}'"
            )));
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let updated = transaction.execute(
            "UPDATE run_playbook SET state = ?1 WHERE run_id = ?2",
            params![state, run_id],
        )?;
        if updated == 0 {
            return Err(OrchestratorError::NotFound(format!(
                "playbook run '{run_id}' not found"
            )));
        }
        append_outbox(&transaction, run_id, "vam.v1.playbook.state", state)?;
        increment_revision(&transaction, run_id)?;
        transaction.commit()?;
        Ok(())
    }
}

impl ReplayStore for SqliteWorkflowStore {
    fn consume(&self, token_id: &str) -> Result<(), SecurityError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| SecurityError::ReplayStoreUnavailable)?;
        let changed = connection
            .execute(
                "INSERT OR IGNORE INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, decision, consumed_token_id
                 ) VALUES (?1, '', '', '', '', 0, 'REPLAY_ONLY', ?1)",
                [token_id],
            )
            .map_err(|_| SecurityError::ReplayStoreUnavailable)?;
        if changed == 0 {
            return Err(SecurityError::TokenAlreadyConsumed(token_id.to_string()));
        }
        Ok(())
    }
}

struct Precondition {
    step_id: String,
    outcome: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OutcomeTerminality {
    Open,
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ClauseEval {
    Satisfied,
    Impossible,
    Waiting,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Reach {
    Pending,
    Available,
    Skipped,
}

struct StepMem {
    step_id: String,
    position: i64,
    state: PlaybookStepState,
    outcome: String,
    output_json: Option<String>,
    agent_summary: Option<String>,
    evidence_refs: Vec<String>,
    started_at: Option<i64>,
    finished_at: Option<i64>,
    transition_revision: i64,
    after: Vec<Precondition>,
    after_any: Vec<Precondition>,
    dirty: bool,
}

impl StepMem {
    fn to_record(&self) -> PlaybookStepRecord {
        PlaybookStepRecord {
            step_id: self.step_id.clone(),
            state: self.state,
            outcome: self.outcome.clone(),
            output_json: self.output_json.clone(),
            agent_summary: self.agent_summary.clone(),
            evidence_refs: self.evidence_refs.clone(),
            started_at: self.started_at,
            finished_at: self.finished_at,
            transition_revision: self.transition_revision,
        }
    }
}

fn require_nonempty(value: &str, field: &str) -> Result<(), OrchestratorError> {
    if value.is_empty() {
        Err(OrchestratorError::InvalidContract(format!(
            "{field} is required"
        )))
    } else {
        Ok(())
    }
}

fn validate_output_json(output: Option<&str>) -> Result<Option<String>, OrchestratorError> {
    let Some(output) = output else {
        return Ok(None);
    };
    let value: serde_json::Value = serde_json::from_str(output).map_err(|_| {
        OrchestratorError::InvalidContract("step output must be a JSON object".to_string())
    })?;
    if !value.is_object() {
        return Err(OrchestratorError::InvalidContract(
            "step output must be a JSON object".to_string(),
        ));
    }
    Ok(Some(output.to_string()))
}

fn step_event_payload(step: &StepMem) -> String {
    serde_json::json!({
        "step_id": step.step_id,
        "state": step.state.as_str(),
        "outcome": step.outcome,
        "revision": step.transition_revision,
    })
    .to_string()
}

fn graph_from_spec(spec: &PlaybookSpec) -> Result<Vec<StepMem>, OrchestratorError> {
    let mut seen = HashSet::new();
    let mut steps = Vec::with_capacity(spec.steps.len());
    for (position, step) in spec.steps.iter().enumerate() {
        if step.id.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "playbook step id is required".to_string(),
            ));
        }
        if !seen.insert(step.id.clone()) {
            return Err(OrchestratorError::InvalidContract(format!(
                "duplicate playbook step id '{}'",
                step.id
            )));
        }
        let (after, after_any) = parse_step_preconditions(step)?;
        steps.push(StepMem {
            step_id: step.id.clone(),
            position: position as i64,
            state: PlaybookStepState::Pending,
            outcome: String::new(),
            output_json: None,
            agent_summary: None,
            evidence_refs: Vec::new(),
            started_at: None,
            finished_at: None,
            transition_revision: 0,
            after,
            after_any,
            dirty: false,
        });
    }
    Ok(steps)
}

fn parse_step_preconditions(
    step: &PlaybookStep,
) -> Result<(Vec<Precondition>, Vec<Precondition>), OrchestratorError> {
    let after = step
        .after
        .iter()
        .chain(step.depends_on.iter())
        .map(|token| parse_precondition(token))
        .collect::<Result<Vec<_>, _>>()?;
    let after_any = step
        .after_any
        .iter()
        .map(|token| parse_precondition(token))
        .collect::<Result<Vec<_>, _>>()?;
    Ok((after, after_any))
}

fn parse_precondition(token: &str) -> Result<Precondition, OrchestratorError> {
    if token.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "precondition must be a step id or id:outcome".to_string(),
        ));
    }
    if let Some((step_id, outcome)) = token.split_once(':') {
        if step_id.is_empty() || outcome.is_empty() || outcome.contains(':') {
            return Err(OrchestratorError::InvalidContract(format!(
                "precondition '{token}' must be a step id or id:outcome"
            )));
        }
        return Ok(Precondition {
            step_id: step_id.to_string(),
            outcome: Some(outcome.to_string()),
        });
    }
    Ok(Precondition {
        step_id: token.to_string(),
        outcome: None,
    })
}

fn settle_pending(steps: &mut [StepMem], now: i64, bump_revision: bool) {
    loop {
        let updates: Vec<(usize, Reach)> = steps
            .iter()
            .enumerate()
            .filter(|(_, step)| step.state == PlaybookStepState::Pending)
            .filter_map(|(index, _)| match reach(index, steps) {
                Reach::Pending => None,
                next => Some((index, next)),
            })
            .collect();
        if updates.is_empty() {
            break;
        }
        for (index, next) in updates {
            apply_reach(&mut steps[index], next, now, bump_revision);
        }
    }
}

fn apply_reach(step: &mut StepMem, next: Reach, now: i64, bump_revision: bool) {
    match next {
        Reach::Available => {
            step.state = PlaybookStepState::Available;
            if step.started_at.is_none() {
                step.started_at = Some(now);
            }
        }
        Reach::Skipped => {
            step.state = PlaybookStepState::Skipped;
            if step.finished_at.is_none() {
                step.finished_at = Some(now);
            }
        }
        Reach::Pending => return,
    }
    if bump_revision {
        step.transition_revision += 1;
    }
    step.dirty = true;
}

fn skip_unfinished(steps: &mut [StepMem], now: i64) {
    for step in steps.iter_mut() {
        if matches!(
            step.state,
            PlaybookStepState::Pending | PlaybookStepState::Available | PlaybookStepState::Open
        ) {
            apply_reach(step, Reach::Skipped, now, true);
        }
    }
}

fn mark_completed(
    step: &mut StepMem,
    outcome: String,
    output_json: Option<String>,
    agent_summary: Option<String>,
    evidence_refs: Vec<String>,
    now: i64,
) {
    step.state = PlaybookStepState::Completed;
    step.outcome = outcome;
    step.output_json = output_json;
    step.agent_summary = agent_summary;
    step.evidence_refs = evidence_refs;
    if step.started_at.is_none() {
        step.started_at = Some(now);
    }
    step.finished_at = Some(now);
    step.transition_revision += 1;
    step.dirty = true;
}

fn reach(index: usize, steps: &[StepMem]) -> Reach {
    let after = eval_and(&steps[index].after, steps);
    let any = eval_any(&steps[index].after_any, steps);
    match (after, any) {
        (ClauseEval::Satisfied, ClauseEval::Satisfied) => Reach::Available,
        (ClauseEval::Impossible, _) | (_, ClauseEval::Impossible) => Reach::Skipped,
        _ => Reach::Pending,
    }
}

fn eval_and(clauses: &[Precondition], steps: &[StepMem]) -> ClauseEval {
    let mut waiting = false;
    for clause in clauses {
        match clause_eval(clause, steps) {
            ClauseEval::Satisfied => {}
            ClauseEval::Waiting => waiting = true,
            ClauseEval::Impossible => return ClauseEval::Impossible,
        }
    }
    if waiting {
        ClauseEval::Waiting
    } else {
        ClauseEval::Satisfied
    }
}

fn eval_any(clauses: &[Precondition], steps: &[StepMem]) -> ClauseEval {
    if clauses.is_empty() {
        return ClauseEval::Satisfied;
    }
    let mut waiting = false;
    for clause in clauses {
        match clause_eval(clause, steps) {
            ClauseEval::Satisfied => return ClauseEval::Satisfied,
            ClauseEval::Waiting => waiting = true,
            ClauseEval::Impossible => {}
        }
    }
    if waiting {
        ClauseEval::Waiting
    } else {
        ClauseEval::Impossible
    }
}

fn clause_eval(clause: &Precondition, steps: &[StepMem]) -> ClauseEval {
    let Some(predecessor) = steps.iter().find(|step| step.step_id == clause.step_id) else {
        return ClauseEval::Impossible;
    };
    match predecessor.state {
        PlaybookStepState::Completed => match clause.outcome.as_deref() {
            None => ClauseEval::Satisfied,
            Some(expected) if expected == predecessor.outcome => ClauseEval::Satisfied,
            Some(_) => ClauseEval::Impossible,
        },
        PlaybookStepState::Skipped => ClauseEval::Impossible,
        PlaybookStepState::Pending | PlaybookStepState::Available | PlaybookStepState::Open => {
            ClauseEval::Waiting
        }
    }
}

fn outcome_terminality(
    spec: &PlaybookSpec,
    step: &PlaybookStep,
    label: &str,
) -> Result<OutcomeTerminality, OrchestratorError> {
    let choices = allowed_outcomes(spec, step);
    let declared = choices
        .iter()
        .map(|(choice, _)| format!("'{choice}'"))
        .collect::<Vec<_>>()
        .join(", ");
    choices
        .into_iter()
        .find(|(choice, _)| choice == label)
        .map(|(_, terminal)| terminal)
        .ok_or_else(|| {
            // The model often puts a sentence in `outcome`. Say what the label may be, and where
            // the sentence goes, so the next call succeeds.
            let shown: String = label.chars().take(80).collect();
            let ellipsis = if label.chars().count() > 80 { "..." } else { "" };
            OrchestratorError::InvalidContract(format!(
                "outcome '{shown}{ellipsis}' is not declared on step '{}'. Declared outcomes: {declared}. Use one of them as `outcome` and put any description in `agent_summary`.",
                step.id
            ))
        })
}

fn allowed_outcomes(spec: &PlaybookSpec, step: &PlaybookStep) -> Vec<(String, OutcomeTerminality)> {
    let kind = match StepKind::try_from(step.kind) {
        Ok(StepKind::Gate) => StepKind::Gate,
        _ => StepKind::Task,
    };
    let author_failed = step
        .outcomes
        .iter()
        .chain(step.options.iter())
        .any(|outcome| outcome.label == "failed");
    let depended = failed_is_depended_on(&step.id, &spec.steps, &spec.guards);
    let mut choices = Vec::new();
    if step.outcomes.is_empty() && step.options.is_empty() {
        if kind == StepKind::Task {
            push_outcome(
                &mut choices,
                "done",
                terminality_of(
                    "done",
                    OutcomeTerminal::Unspecified as i32,
                    author_failed,
                    depended,
                ),
            );
        }
        if kind == StepKind::Gate && step.response_schema.is_some() {
            for label in ["submitted", "rejected"] {
                push_outcome(
                    &mut choices,
                    label,
                    terminality_of(
                        label,
                        OutcomeTerminal::Unspecified as i32,
                        author_failed,
                        depended,
                    ),
                );
            }
        }
    } else {
        for outcome in step.outcomes.iter().chain(step.options.iter()) {
            push_outcome(
                &mut choices,
                &outcome.label,
                terminality_of(&outcome.label, outcome.terminal, author_failed, depended),
            );
        }
    }
    if kind == StepKind::Task && !choices.iter().any(|(label, _)| label == "failed") {
        push_outcome(&mut choices, "failed", OutcomeTerminality::Failed);
    }
    choices
}

fn push_outcome(
    choices: &mut Vec<(String, OutcomeTerminality)>,
    label: &str,
    terminal: OutcomeTerminality,
) {
    if label.is_empty() || choices.iter().any(|(existing, _)| existing == label) {
        return;
    }
    choices.push((label.to_string(), terminal));
}

fn terminality_of(
    label: &str,
    terminal: i32,
    author_declared_failed: bool,
    failed_depended_on: bool,
) -> OutcomeTerminality {
    match OutcomeTerminal::try_from(terminal) {
        Ok(OutcomeTerminal::Succeeded) => OutcomeTerminality::Succeeded,
        Ok(OutcomeTerminal::Failed) => OutcomeTerminality::Failed,
        _ if label == "failed" && !(author_declared_failed && failed_depended_on) => {
            OutcomeTerminality::Failed
        }
        _ => OutcomeTerminality::Open,
    }
}

fn failed_is_depended_on(step_id: &str, steps: &[PlaybookStep], guards: &[PlaybookGuard]) -> bool {
    let mentions = |token: &str| match parse_precondition(token) {
        Ok(clause) => {
            clause.step_id == step_id
                && (clause.outcome.is_none() || clause.outcome.as_deref() == Some("failed"))
        }
        Err(_) => false,
    };
    let steps_depend = steps.iter().any(|step| {
        step.id != step_id
            && step
                .after
                .iter()
                .chain(step.depends_on.iter())
                .chain(step.after_any.iter())
                .any(|token| mentions(token))
    });
    let guards_depend = guards
        .iter()
        .any(|guard| guard.allowed_when.iter().any(|token| mentions(token)));
    steps_depend || guards_depend
}

fn attach_preconditions(
    steps: &mut [StepMem],
    spec: &PlaybookSpec,
) -> Result<(), OrchestratorError> {
    if steps.len() != spec.steps.len() {
        return Err(OrchestratorError::CorruptState(
            "playbook step rows do not match the stored spec".to_string(),
        ));
    }
    for spec_step in &spec.steps {
        let row = steps
            .iter_mut()
            .find(|step| step.step_id == spec_step.id)
            .ok_or_else(|| {
                OrchestratorError::CorruptState(format!(
                    "stored playbook is missing step '{}'",
                    spec_step.id
                ))
            })?;
        let (after, after_any) = parse_step_preconditions(spec_step)?;
        row.after = after;
        row.after_any = after_any;
    }
    Ok(())
}

fn load_playbook_header(
    connection: &Connection,
    run_id: &str,
) -> Result<(String, String), OrchestratorError> {
    connection
        .query_row(
            "SELECT spec_json, state FROM run_playbook WHERE run_id = ?1",
            [run_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            OrchestratorError::NotFound(format!("playbook step state for run '{run_id}' not found"))
        })
}

fn parse_evidence_refs_json(raw: Option<&str>) -> Result<Vec<String>, OrchestratorError> {
    match raw {
        None => Ok(Vec::new()),
        Some("") => Ok(Vec::new()),
        Some(s) => serde_json::from_str(s).map_err(|err| {
            OrchestratorError::CorruptState(format!("invalid evidence_refs_json: {err}"))
        }),
    }
}

fn encode_evidence_refs(refs: &[String]) -> Result<Option<String>, OrchestratorError> {
    if refs.is_empty() {
        return Ok(None);
    }
    serde_json::to_string(refs)
        .map(Some)
        .map_err(|err| OrchestratorError::InvalidContract(format!("evidence refs: {err}")))
}

/// Non-failed outcomes of an evidence-required step need a matching call
/// recorded after the step became available. The implicit task outcome
/// `failed` does not.
fn evidence_refs_for_completion(
    connection: &Connection,
    run_id: &str,
    step: &PlaybookStep,
    outcome: &str,
) -> Result<Vec<String>, OrchestratorError> {
    if outcome == "failed" {
        return Ok(Vec::new());
    }
    let Some(evidence) = step.evidence.as_ref() else {
        return Ok(Vec::new());
    };
    if evidence.tool.is_empty() {
        return Ok(Vec::new());
    }
    let available_at = step_available_sequence(connection, run_id, &step.id)?.ok_or_else(|| {
        OrchestratorError::InvalidContract(format!(
            "step '{}' has no availability record, so its evidence cannot be checked",
            step.id
        ))
    })?;
    let calls = crate::trajectory::read_completed_tool_calls(connection, run_id)?;
    let refs: Vec<String> = calls
        .iter()
        .filter(|call| {
            call.sequence > available_at
                && call.tool_name == evidence.tool
                && call.result == evidence.result
        })
        .map(crate::trajectory::evidence_ref)
        .collect();
    if refs.is_empty() {
        return Err(OrchestratorError::InvalidContract(format!(
            "step '{}' requires a {} call to '{}' observed while the step was available",
            step.id, evidence.result, evidence.tool
        )));
    }
    Ok(refs)
}

/// Outbox sequence of the event that first recorded this step as available.
/// Tool calls at or before that sequence happened before the step was available.
fn step_available_sequence(
    connection: &Connection,
    run_id: &str,
    step_id: &str,
) -> Result<Option<i64>, OrchestratorError> {
    let mut statement = connection.prepare(
        "SELECT sequence, payload
         FROM workflow_outbox
         WHERE run_id = ?1 AND event_type = 'vam.v1.playbook.step'
         ORDER BY sequence ASC",
    )?;
    let rows = statement.query_map([run_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (sequence, payload) = row.map_err(OrchestratorError::Database)?;
        let Ok(value) = serde_json::from_str::<serde_json::Value>(&payload) else {
            continue;
        };
        let matches_step = value.get("step_id").and_then(|item| item.as_str()) == Some(step_id);
        let available = value.get("state").and_then(|item| item.as_str()) == Some("available");
        if matches_step && available {
            return Ok(Some(sequence));
        }
    }
    Ok(None)
}

fn load_playbook_steps(
    connection: &Connection,
    run_id: &str,
) -> Result<Vec<StepMem>, OrchestratorError> {
    let mut statement = connection.prepare(
        "SELECT step_id, position, state, outcome, output_json, agent_summary,
                evidence_refs_json, started_at, finished_at, transition_revision
         FROM run_playbook_steps
         WHERE run_id = ?1
         ORDER BY position ASC",
    )?;
    let rows = statement.query_map([run_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Option<String>>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<i64>>(7)?,
            row.get::<_, Option<i64>>(8)?,
            row.get::<_, i64>(9)?,
        ))
    })?;
    let raw = rows
        .collect::<Result<Vec<_>, _>>()
        .map_err(OrchestratorError::Database)?;
    raw.into_iter()
        .map(
            |(
                step_id,
                position,
                state,
                outcome,
                output_json,
                agent_summary,
                evidence_refs_json,
                started_at,
                finished_at,
                transition_revision,
            )| {
                Ok(StepMem {
                    step_id,
                    position,
                    state: PlaybookStepState::parse(&state)?,
                    outcome,
                    output_json,
                    agent_summary,
                    evidence_refs: parse_evidence_refs_json(evidence_refs_json.as_deref())?,
                    started_at,
                    finished_at,
                    transition_revision,
                    after: Vec::new(),
                    after_any: Vec::new(),
                    dirty: false,
                })
            },
        )
        .collect()
}

fn insert_playbook_steps(
    transaction: &Transaction<'_>,
    run_id: &str,
    steps: &[StepMem],
) -> Result<(), OrchestratorError> {
    for step in steps {
        transaction.execute(
            "INSERT INTO run_playbook_steps (
                run_id, step_id, position, state, outcome, output_json, agent_summary,
                evidence_refs_json, started_at, finished_at, transition_revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, ?8, ?9, ?10)",
            params![
                run_id,
                step.step_id,
                step.position,
                step.state.as_str(),
                step.outcome,
                step.output_json,
                step.agent_summary,
                step.started_at,
                step.finished_at,
                step.transition_revision,
            ],
        )?;
    }
    Ok(())
}

fn update_playbook_step(
    transaction: &Transaction<'_>,
    run_id: &str,
    step: &StepMem,
) -> Result<(), OrchestratorError> {
    let evidence_refs_json = encode_evidence_refs(&step.evidence_refs)?;
    let changed = transaction.execute(
        "UPDATE run_playbook_steps
         SET state = ?1, outcome = ?2, output_json = ?3, agent_summary = ?4,
             evidence_refs_json = ?5, started_at = ?6, finished_at = ?7,
             transition_revision = ?8
         WHERE run_id = ?9 AND step_id = ?10",
        params![
            step.state.as_str(),
            step.outcome,
            step.output_json,
            step.agent_summary,
            evidence_refs_json,
            step.started_at,
            step.finished_at,
            step.transition_revision,
            run_id,
            step.step_id,
        ],
    )?;
    if changed == 0 {
        return Err(OrchestratorError::NotFound(format!(
            "playbook step '{}' not found on run '{run_id}'",
            step.step_id
        )));
    }
    if step.state == PlaybookStepState::Skipped {
        transaction.execute(
            "UPDATE approvals
             SET decision = 'CANCELLED', decision_revision = decision_revision + 1
             WHERE run_id = ?1 AND step_id = ?2 AND decision IS NULL AND kind = 'gate'",
            params![run_id, step.step_id],
        )?;
        crate::notifications::resolve(
            transaction,
            &format!("{run_id}/gate/{}", step.step_id),
            "superseded",
            None,
        )?;
    }
    Ok(())
}

fn is_gate_step(step: &PlaybookStep) -> bool {
    StepKind::try_from(step.kind) == Ok(StepKind::Gate)
}

fn promote_available_gates(steps: &mut [StepMem], spec: &PlaybookSpec) {
    for step in steps.iter_mut() {
        if step.state != PlaybookStepState::Available {
            continue;
        }
        let Some(spec_step) = spec
            .steps
            .iter()
            .find(|candidate| candidate.id == step.step_id)
        else {
            continue;
        };
        if is_gate_step(spec_step) {
            step.state = PlaybookStepState::Open;
            step.dirty = true;
        }
    }
}

fn gate_timeout_secs(step: &PlaybookStep) -> i64 {
    step.timeout
        .as_ref()
        .map(duration_secs)
        .unwrap_or(DEFAULT_GATE_TIMEOUT_SECS)
}

fn gate_keep_warm_secs(step: &PlaybookStep) -> i64 {
    step.keep_warm
        .as_ref()
        .map(duration_secs)
        .unwrap_or(DEFAULT_KEEP_WARM_SECS)
}

fn duration_secs(duration: &prost_types::Duration) -> i64 {
    duration.seconds.max(0)
}

fn split_choices_from(raw: &str) -> Option<(String, String)> {
    let rest = raw.trim().strip_prefix("steps.")?;
    let output_at = rest.find(".output")?;
    let id = rest[..output_at].to_string();
    if id.is_empty() {
        return None;
    }
    let path = rest[output_at + ".output".len()..].to_string();
    if path.is_empty() || path.starts_with('.') || path.starts_with('[') {
        return Some((id, path));
    }
    None
}

fn choices_for_gate(step: &PlaybookStep, steps: &[StepMem]) -> Vec<serde_json::Value> {
    let Some((id, path)) = split_choices_from(&step.choices_from) else {
        return Vec::new();
    };
    let Some(output) = steps
        .iter()
        .find(|candidate| candidate.step_id == id)
        .and_then(|candidate| candidate.output_json.as_deref())
    else {
        return Vec::new();
    };
    crate::run_context::choice_values_from_output(output, &path)
}

fn gate_response_schema(step: &PlaybookStep, choices: &[serde_json::Value]) -> serde_json::Value {
    if let Some(schema) = step.response_schema.as_ref() {
        let mut json = crate::run_context::struct_json(schema);
        if !choices.is_empty() {
            fill_schema_enums(&mut json, choices);
        }
        return json;
    }
    let options: Vec<String> = step
        .options
        .iter()
        .map(|option| option.label.clone())
        .filter(|label| !label.is_empty())
        .collect();
    let mut properties = serde_json::Map::new();
    if !options.is_empty() {
        properties.insert(
            "option".to_string(),
            serde_json::json!({ "type": "string", "enum": options }),
        );
    }
    if !choices.is_empty() {
        properties.insert(
            "selected".to_string(),
            serde_json::json!({ "type": "array", "items": { "enum": choices } }),
        );
    }
    let mut schema = serde_json::json!({
        "type": "object",
        "additionalProperties": true,
        "properties": properties,
    });
    if !options.is_empty() {
        schema["required"] = serde_json::json!(["option"]);
    }
    schema
}

fn schema_contains_enum(node: &serde_json::Value) -> bool {
    match node {
        serde_json::Value::Object(map) => {
            map.contains_key("enum") || map.values().any(schema_contains_enum)
        }
        serde_json::Value::Array(items) => items.iter().any(schema_contains_enum),
        _ => false,
    }
}

fn replace_existing_enums(node: &mut serde_json::Value, choices: &[serde_json::Value]) {
    match node {
        serde_json::Value::Object(map) => {
            if map.contains_key("enum") {
                map.insert(
                    "enum".to_string(),
                    serde_json::Value::Array(choices.to_vec()),
                );
            }
            let keys: Vec<String> = map
                .keys()
                .filter(|key| key.as_str() != "enum")
                .cloned()
                .collect();
            for key in keys {
                if let Some(child) = map.get_mut(&key) {
                    replace_existing_enums(child, choices);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                replace_existing_enums(item, choices);
            }
        }
        _ => {}
    }
}

fn inject_enums(node: &mut serde_json::Value, choices: &[serde_json::Value]) {
    let Some(map) = node.as_object_mut() else {
        return;
    };
    if map.get("type").and_then(serde_json::Value::as_str) == Some("string")
        && !map.contains_key("enum")
    {
        map.insert(
            "enum".to_string(),
            serde_json::Value::Array(choices.to_vec()),
        );
    }
    let property_keys: Vec<String> = map
        .get("properties")
        .and_then(serde_json::Value::as_object)
        .map(|props| props.keys().cloned().collect())
        .unwrap_or_default();
    for key in property_keys {
        if let Some(child) = map
            .get_mut("properties")
            .and_then(serde_json::Value::as_object_mut)
            .and_then(|props| props.get_mut(&key))
        {
            inject_enums(child, choices);
        }
    }
    if map.contains_key("items") {
        if let Some(items) = map.get_mut("items") {
            inject_enums(items, choices);
        }
    }
}

fn fill_schema_enums(node: &mut serde_json::Value, choices: &[serde_json::Value]) {
    if choices.is_empty() {
        return;
    }
    if schema_contains_enum(node) {
        replace_existing_enums(node, choices);
    } else {
        inject_enums(node, choices);
    }
}

fn gate_outcome_label(
    step: &PlaybookStep,
    response: &serde_json::Value,
) -> Result<String, OrchestratorError> {
    if step.options.is_empty() {
        return Ok("submitted".to_string());
    }
    let option = response
        .get("option")
        .and_then(serde_json::Value::as_str)
        .filter(|label| !label.is_empty())
        .ok_or_else(|| {
            OrchestratorError::InvalidContract(
                "response.option must name a declared gate outcome".into(),
            )
        })?;
    if !step.options.iter().any(|outcome| outcome.label == option) {
        return Err(OrchestratorError::InvalidContract(format!(
            "response.option '{option}' is not a declared outcome of gate '{}'",
            step.id
        )));
    }
    Ok(option.to_string())
}

fn load_run_inputs(connection: &Connection, run_id: &str) -> Option<prost_types::Struct> {
    let spec_json: Option<String> = connection
        .query_row(
            "SELECT spec_json FROM runs WHERE run_id = ?1",
            [run_id],
            |row| row.get(0),
        )
        .ok()?;
    let spec_json = spec_json?;
    let spec =
        vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &spec_json)
            .ok()?;
    spec.inputs
}

fn rendered_gate_question(
    connection: &Connection,
    run_id: &str,
    step: &PlaybookStep,
    steps: &[StepMem],
) -> String {
    let inputs = load_run_inputs(connection, run_id);
    let outputs: Vec<(&str, &str)> = steps
        .iter()
        .filter_map(|candidate| {
            candidate
                .output_json
                .as_deref()
                .map(|json| (candidate.step_id.as_str(), json))
        })
        .collect();
    let rendered =
        crate::run_context::render_gate_question(&step.question, inputs.as_ref(), &outputs);
    if rendered.trim().is_empty() {
        format!("Gate {}", step.id)
    } else {
        rendered
    }
}

fn open_ready_gates(
    transaction: &Transaction<'_>,
    run_id: &str,
    spec: &PlaybookSpec,
    steps: &[StepMem],
    now: i64,
) -> Result<(), OrchestratorError> {
    for step in steps {
        if step.state != PlaybookStepState::Open {
            continue;
        }
        let exists: bool = transaction
            .query_row(
                "SELECT 1 FROM approvals WHERE run_id = ?1 AND step_id = ?2 AND kind = 'gate'",
                params![run_id, step.step_id],
                |_| Ok(true),
            )
            .optional()?
            .unwrap_or(false);
        if exists {
            continue;
        }
        let spec_step = spec
            .steps
            .iter()
            .find(|candidate| candidate.id == step.step_id)
            .ok_or_else(|| {
                OrchestratorError::CorruptState(format!(
                    "stored playbook is missing step '{}'",
                    step.step_id
                ))
            })?;
        insert_gate_approval(transaction, run_id, spec_step, steps, now)?;
    }
    Ok(())
}

fn insert_gate_approval(
    transaction: &Transaction<'_>,
    run_id: &str,
    step: &PlaybookStep,
    steps: &[StepMem],
    now: i64,
) -> Result<(), OrchestratorError> {
    let choices = choices_for_gate(step, steps);
    let mut schema = gate_response_schema(step, &choices);
    // G5: defaults templated from earlier outputs are resolved as the gate opens.
    let run_inputs = load_run_inputs(transaction, run_id);
    let outputs: std::collections::BTreeMap<String, serde_json::Value> = steps
        .iter()
        .filter_map(|candidate| {
            let json = candidate.output_json.as_deref()?;
            let value = serde_json::from_str::<serde_json::Value>(json).ok()?;
            Some((candidate.step_id.clone(), value))
        })
        .collect();
    crate::run_context::render_schema_templates(
        &mut schema,
        &crate::run_context::template_inputs_json(run_inputs.as_ref()),
        &outputs,
    );
    let question = rendered_gate_question(transaction, run_id, step, steps);
    let options: Vec<&str> = step
        .options
        .iter()
        .map(|option| option.label.as_str())
        .filter(|label| !label.is_empty())
        .collect();
    let preview = serde_json::json!({
        "question": question,
        "options": options,
        "response_schema": schema,
    });
    let schema_json = schema.to_string();
    let digest = crate::events::args_digest_from_json(&schema);
    let request_id = format!("{run_id}/gate/{}", step.id);
    let expires_at = now.saturating_add(gate_timeout_secs(step));
    transaction.execute(
        "INSERT INTO approvals (
            request_id, run_id, step_id, attempt_id, checkpoint_digest,
            continuation_generation, approval_message, tool_name, requested_args_json,
            amendable_schema_json, args_digest, required_roles_json, created_at,
            expires_at, decision_revision, published, session_id, owner_user_id,
            approver_user_id, kind
         ) VALUES (
            ?1, ?2, ?3, ?4, '', 0, ?5, '', ?6, ?7, ?8, '[]', ?9, ?10, 0, 1,
            (SELECT session_id FROM runs WHERE run_id = ?2),
            (SELECT COALESCE(owner_user_id, 'local') FROM runs WHERE run_id = ?2),
            (SELECT COALESCE(owner_user_id, 'local') FROM runs WHERE run_id = ?2),
            'gate'
         )",
        params![
            request_id,
            run_id,
            step.id,
            format!("gate:{}", step.id),
            question,
            preview.to_string(),
            schema_json,
            digest,
            now,
            expires_at,
        ],
    )?;
    crate::notifications::record_for_approval(transaction, &request_id)?;
    Ok(())
}

fn gate_on_timeout_label(
    connection: &Connection,
    run_id: &str,
    step_id: &str,
) -> Result<Option<String>, OrchestratorError> {
    let Ok((spec_json, _)) = load_playbook_header(connection, run_id) else {
        return Ok(None);
    };
    let spec: PlaybookSpec = vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &spec_json)
        .map_err(OrchestratorError::CorruptState)?;
    let Some(step) = spec.steps.iter().find(|step| step.id == step_id) else {
        return Ok(None);
    };
    if step.on_timeout.is_empty() {
        Ok(None)
    } else {
        Ok(Some(step.on_timeout.clone()))
    }
}

fn record_open_gate_outcome(
    transaction: &Transaction<'_>,
    run_id: &str,
    step_id: &str,
    completion: &PlaybookStepCompletion,
) -> Result<(), OrchestratorError> {
    let (spec_json, playbook_state) = load_playbook_header(transaction, run_id)?;
    if playbook_state != PLAYBOOK_RUN_ACTIVE {
        return Err(OrchestratorError::InvalidState {
            expected: PLAYBOOK_RUN_ACTIVE,
            actual: playbook_state,
        });
    }
    let spec: PlaybookSpec = vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &spec_json)
        .map_err(OrchestratorError::CorruptState)?;
    let mut steps = load_playbook_steps(transaction, run_id)?;
    attach_preconditions(&mut steps, &spec)?;
    let index = steps
        .iter()
        .position(|step| step.step_id == step_id)
        .ok_or_else(|| {
            OrchestratorError::NotFound(format!(
                "playbook step '{step_id}' not found on run '{run_id}'"
            ))
        })?;
    if steps[index].state != PlaybookStepState::Open {
        return Err(OrchestratorError::InvalidState {
            expected: "open",
            actual: steps[index].state.as_str().to_string(),
        });
    }
    let spec_step = spec
        .steps
        .iter()
        .find(|step| step.id == step_id)
        .ok_or_else(|| {
            OrchestratorError::CorruptState(format!("stored playbook is missing step '{step_id}'"))
        })?;
    let output_json = validate_output_json(completion.output_json.as_deref())?;
    vam_spec::validate_playbook_step_output(
        spec_step.output_schema.as_ref(),
        output_json.as_deref(),
    )
    .map_err(|err| OrchestratorError::InvalidContract(err.message))?;
    let terminal = outcome_terminality(&spec, spec_step, &completion.outcome)?;
    let now = unix_timestamp();
    mark_completed(
        &mut steps[index],
        completion.outcome.clone(),
        output_json,
        completion.agent_summary.clone(),
        Vec::new(),
        now,
    );
    let next_run_state = match terminal {
        OutcomeTerminality::Succeeded => {
            skip_unfinished(&mut steps, now);
            Some(PLAYBOOK_RUN_SUCCEEDED)
        }
        OutcomeTerminality::Failed => {
            skip_unfinished(&mut steps, now);
            Some(PLAYBOOK_RUN_FAILED)
        }
        OutcomeTerminality::Open => {
            settle_pending(&mut steps, now, true);
            promote_available_gates(&mut steps, &spec);
            None
        }
    };
    for step in steps.iter().filter(|step| step.dirty) {
        update_playbook_step(transaction, run_id, step)?;
        append_outbox(
            transaction,
            run_id,
            "vam.v1.playbook.step",
            &step_event_payload(step),
        )?;
    }
    if let Some(state) = next_run_state {
        transaction.execute(
            "UPDATE run_playbook SET state = ?1 WHERE run_id = ?2",
            params![state, run_id],
        )?;
        append_outbox(transaction, run_id, "vam.v1.playbook.state", state)?;
    }
    open_ready_gates(transaction, run_id, &spec, &steps, now)?;
    Ok(())
}

/// A thread's conversation lives in the workspace thread log (decision 23). The
/// SQLite conversation store of the unreleased turn-checkpoint chain is dropped
/// (dev databases only: nothing wrote it), and `turn_commits` is recreated with
/// the log anchor (`log_seq`, `log_digest`) in place of a checkpoint digest.
/// `run_checkpoints` (HITL) and `run_checkpoint_grants` are untouched.
fn retire_wave_one_conversation_store(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "DROP TABLE IF EXISTS thread_checkpoints;
         DROP TABLE IF EXISTS checkpoint_blobs;",
    )?;
    let old_shape: bool = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('turn_commits') WHERE name = 'checkpoint_digest'",
        [],
        |row| Ok(row.get::<_, i64>(0)? > 0),
    )?;
    if old_shape {
        connection.execute_batch(
            "DROP TABLE turn_commits;
             CREATE TABLE turn_commits (
                run_id TEXT NOT NULL,
                round INTEGER NOT NULL,
                log_seq INTEGER NOT NULL,
                log_digest TEXT NOT NULL,
                manifest_hash TEXT NOT NULL DEFAULT '',
                state TEXT NOT NULL CHECK(state IN ('pending', 'complete', 'failed')),
                created_at INTEGER NOT NULL,
                PRIMARY KEY (run_id, round),
                FOREIGN KEY (run_id) REFERENCES runs(run_id)
             );",
        )?;
    }
    Ok(())
}

/// A turn commit completes when the host has seen the round's tool results
/// (the boundary in the log) and has a manifest. These columns carry the
/// session that took it, the digest the guest announced for the results, and
/// the boundary.
fn migrate_turn_commits_boundary(connection: &Connection) -> Result<(), OrchestratorError> {
    let columns: [(&str, &str); 5] = [
        ("session_id", "TEXT NOT NULL DEFAULT ''"),
        ("tail_digest", "TEXT NOT NULL DEFAULT ''"),
        ("boundary_seq", "INTEGER"),
        ("boundary_digest", "TEXT NOT NULL DEFAULT ''"),
        ("error", "TEXT NOT NULL DEFAULT ''"),
    ];
    for (name, definition) in columns {
        let present: bool = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('turn_commits') WHERE name = ?1",
            [name],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )?;
        if !present {
            connection.execute_batch(&format!(
                "ALTER TABLE turn_commits ADD COLUMN {name} {definition};"
            ))?;
        }
    }
    Ok(())
}

fn migrate_playbook_step_open_state(connection: &Connection) -> Result<(), OrchestratorError> {
    let sql: Option<String> = connection
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'run_playbook_steps'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let Some(sql) = sql else {
        return Ok(());
    };
    if sql.contains("'open'") {
        return Ok(());
    }
    connection.execute_batch(
        "
        CREATE TABLE run_playbook_steps_v2 (
            run_id TEXT NOT NULL,
            step_id TEXT NOT NULL,
            position INTEGER NOT NULL,
            state TEXT NOT NULL CHECK(state IN ('pending', 'available', 'open', 'completed', 'skipped')),
            outcome TEXT NOT NULL DEFAULT '',
            output_json TEXT,
            agent_summary TEXT,
            evidence_refs_json TEXT,
            started_at INTEGER,
            finished_at INTEGER,
            transition_revision INTEGER NOT NULL,
            PRIMARY KEY (run_id, step_id),
            FOREIGN KEY (run_id) REFERENCES runs(run_id)
        );
        INSERT INTO run_playbook_steps_v2 (
            run_id, step_id, position, state, outcome, output_json, agent_summary,
            evidence_refs_json, started_at, finished_at, transition_revision
        )
        SELECT run_id, step_id, position, state, outcome, output_json, agent_summary,
               evidence_refs_json, started_at, finished_at, transition_revision
        FROM run_playbook_steps;
        DROP TABLE run_playbook_steps;
        ALTER TABLE run_playbook_steps_v2 RENAME TO run_playbook_steps;
        CREATE INDEX IF NOT EXISTS idx_run_playbook_steps_run
            ON run_playbook_steps(run_id, position);
        ",
    )?;
    Ok(())
}

fn required_urn(
    resource_id: &Option<ResourceId>,
    field: &str,
) -> Result<String, OrchestratorError> {
    resource_id
        .as_ref()
        .map(|resource_id| resource_id.urn.clone())
        .filter(|urn| !urn.is_empty())
        .ok_or_else(|| OrchestratorError::InvalidContract(format!("{} is required", field)))
}

fn attempt_key(lease: &ExecutionLease) -> Result<AttemptKey, OrchestratorError> {
    Ok(AttemptKey {
        run_id: required_urn(&lease.run_id, "run_id")?,
        step_id: lease.step_id.clone(),
        attempt_id: required_urn(&lease.attempt_id, "attempt_id")?,
    })
}

fn outcome_state(value: i32) -> Result<AttemptState, OrchestratorError> {
    match TaskOutcomeState::try_from(value).ok() {
        Some(TaskOutcomeState::Succeeded) => Ok(AttemptState::Succeeded),
        Some(TaskOutcomeState::Failed) => Ok(AttemptState::Failed),
        Some(TaskOutcomeState::Suspended) => Ok(AttemptState::Suspended),
        Some(TaskOutcomeState::Cancelled) => Ok(AttemptState::Cancelled),
        _ => Err(OrchestratorError::InvalidContract(
            "TaskOutcome state cannot be committed".to_string(),
        )),
    }
}

pub(crate) fn increment_revision(
    transaction: &Transaction<'_>,
    run_id: &str,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "UPDATE runs SET revision = revision + 1 WHERE run_id = ?1",
        [run_id],
    )?;
    Ok(())
}

pub(crate) fn append_outbox(
    transaction: &Transaction<'_>,
    run_id: &str,
    event_type: &str,
    payload: &str,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "INSERT INTO workflow_outbox (run_id, event_type, payload) VALUES (?1, ?2, ?3)",
        params![run_id, event_type, payload],
    )?;
    Ok(())
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

fn map_missing_run(run_id: &str, err: rusqlite::Error) -> OrchestratorError {
    if err == rusqlite::Error::QueryReturnedNoRows {
        OrchestratorError::NotFound(format!("run '{run_id}' not found"))
    } else {
        OrchestratorError::Database(err)
    }
}

/// Positive `RunSpec.max_duration` seconds, or `None` when unset or non-positive.
fn positive_max_duration_secs(spec_json: Option<&str>) -> Option<i64> {
    let spec_json = spec_json?;
    let spec =
        vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", spec_json)
            .ok()?;
    match spec.max_duration {
        Some(duration) if duration.seconds > 0 => Some(duration.seconds),
        _ => None,
    }
}

fn map_run_metering(row: &rusqlite::Row<'_>) -> rusqlite::Result<RunMetering> {
    Ok(RunMetering {
        iteration: row.get(0)?,
        spend_micro: row.get(1)?,
        token_count: row.get(2)?,
        prompt_tokens: row.get(3)?,
        completion_tokens: row.get(4)?,
    })
}

/// The target stamp for a suspending tool, read in the suspend transaction. A
/// session with no row for the tool is a legacy session.
fn session_target_stamp(
    transaction: &Transaction<'_>,
    session_id: &str,
    tool_name: &str,
) -> Result<crate::session_targets::ApprovalStamp, OrchestratorError> {
    let stamp = transaction
        .query_row(
            "SELECT target, always_offered, locked_reason FROM session_tool_targets
             WHERE session_id = ?1 AND tool_name = ?2",
            params![session_id, tool_name],
            |row| {
                Ok(crate::session_targets::ApprovalStamp {
                    target: row.get(0)?,
                    always_offered: row.get::<_, i64>(1)? != 0,
                    locked_reason: row.get(2)?,
                })
            },
        )
        .optional()?;
    Ok(stamp.unwrap_or_else(crate::session_targets::ApprovalStamp::legacy))
}

fn map_stored_approval_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredApproval> {
    Ok(StoredApproval {
        request_id: row.get(0)?,
        run_id: row.get(1)?,
        step_id: row.get(2)?,
        attempt_id: row.get(3)?,
        session_id: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
        tool_name: row.get(5)?,
        approval_message: row.get(6)?,
        requested_args_json: row.get(7)?,
        amendable_schema_json: row.get(8)?,
        args_digest: row.get(9)?,
        required_roles_json: row.get(10)?,
        checkpoint_digest: row.get(11)?,
        continuation_generation: row.get(12)?,
        created_at: row.get(13)?,
        expires_at: row.get(14)?,
        decision: row.get(15)?,
        decision_revision: row.get(16)?,
        reviewer_principal: row.get(17)?,
        amended_args_json: row.get(18)?,
        amended_args_digest: row.get(19)?,
        rejection_reason: row.get(20)?,
        published: row.get::<_, i64>(21)? != 0,
        approver_user_id: {
            let raw: Option<String> = row.get(22)?;
            raw.filter(|s| !s.is_empty())
                .unwrap_or_else(|| crate::users::LOCAL_USER_ID.to_string())
        },
        kind: {
            let raw: Option<String> = row.get(23)?;
            raw.filter(|s| !s.is_empty())
                .unwrap_or_else(|| crate::hitl::APPROVAL_KIND_TOOL_CALL.to_string())
        },
        target: row.get::<_, Option<String>>(24)?.unwrap_or_default(),
        always_offered: row.get::<_, Option<i64>>(25)?.unwrap_or(0) != 0,
        locked_reason: row.get::<_, Option<String>>(26)?.unwrap_or_default(),
        card_json: row
            .get::<_, Option<String>>(27)?
            .filter(|card| !card.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::Timestamp;
    use vam_spec::v1::{
        ArtifactRef, Digest, OutcomeTerminal, PlaybookSpec, PlaybookStep, StepBinding, StepKind,
        StepMode, StepOutcome,
    };

    fn resource(kind: &str, name: &str) -> ResourceId {
        ResourceId {
            urn: format!("urn:vam:{kind}:local.vam.dev/acme/{name}"),
        }
    }

    fn lease_for_run(run_name: &str) -> ExecutionLease {
        ExecutionLease {
            run_id: Some(resource("run", run_name)),
            step_id: "step-1".to_string(),
            attempt_id: Some(resource("attempt", "attempt-1")),
            lease_id: Some(resource("lease", "lease-1")),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: "idempotency-1".to_string(),
        }
    }

    fn lease() -> ExecutionLease {
        lease_for_run("run-1")
    }

    fn artifact(name: &str) -> ArtifactRef {
        ArtifactRef {
            artifact_id: Some(resource("artifact", name)),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            media_type: "application/octet-stream".to_string(),
            size_bytes: 0,
            uri: "memory://checkpoint".to_string(),
        }
    }

    #[test]
    fn stale_guest_outcomes_cannot_commit() {
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        let run_id = resource("run", "run-1");
        store.create_run(&run_id).expect("run should be created");
        let lease = lease();
        store.issue_lease(&lease).expect("lease should be issued");

        let mut stale = lease.clone();
        stale.fencing_token = vec![2];
        let outcome = TaskOutcome {
            lease: Some(stale),
            state: TaskOutcomeState::Succeeded as i32,
            output: None,
            output_artifacts: Vec::new(),
            continuation: None,
            evidence: None,
            observed_usage: None,
            error: None,
        };

        assert!(matches!(
            store.record_outcome(&outcome),
            Err(OrchestratorError::StaleLease)
        ));
    }

    #[test]
    fn approval_requires_suspended_attempt_and_checkpoint() {
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        let run_id = resource("run", "run-1");
        store.create_run(&run_id).expect("run should be created");
        let lease = lease();
        store.issue_lease(&lease).expect("lease should be issued");

        let checkpoint = artifact("checkpoint-1");
        store
            .register_hitl_suspend(
                &attempt_key(&lease).expect("lease should have an attempt key"),
                "sess-test",
                &resource("hitl", "approval-1"),
                "jira_create_issue",
                r#"{"title":"issue"}"#,
                "abc123",
                None,
            )
            .expect("pending hitl should register while RUNNING");
        let suspended = TaskOutcome {
            lease: Some(lease.clone()),
            state: TaskOutcomeState::Suspended as i32,
            output: None,
            output_artifacts: Vec::new(),
            continuation: Some(checkpoint.clone()),
            evidence: None,
            observed_usage: None,
            error: None,
        };
        assert_eq!(
            store
                .record_outcome(&suspended)
                .expect("suspension should persist"),
            AttemptState::Suspended
        );

        store
            .create_approval(
                &attempt_key(&lease).expect("lease should have an attempt key"),
                &resource("hitl", "approval-1"),
                &checkpoint,
                1,
            )
            .expect("checkpoint-bound approval should persist");
        let events = store.read_outbox(0).expect("outbox should be readable");
        assert!(events
            .iter()
            .any(|event| event.event_type == "vam.v1.hitl.paused"));
        let listed = store.list_published_approvals().expect("list");
        assert_eq!(listed.len(), 1);
        assert!(listed[0].requested_args_json.contains("title"));
        assert_eq!(listed[0].approver_user_id, crate::users::LOCAL_USER_ID);
        assert!(
            crate::hitl::HitlApprover::parse(&listed[0].approver_user_id)
                .allows_decider(crate::users::LOCAL_USER_ID)
        );
    }

    /// A run parked on one approval (`approval-1`).
    fn parked_store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        store
            .create_run(&resource("run", "run-1"))
            .expect("run should be created");
        let lease = lease();
        store.issue_lease(&lease).expect("lease should be issued");
        let checkpoint = artifact("checkpoint-1");
        let key = attempt_key(&lease).expect("lease should have an attempt key");
        store
            .register_hitl_suspend(
                &key,
                "sess-test",
                &resource("hitl", "approval-1"),
                "jira_create_issue",
                r#"{"title":"issue"}"#,
                "abc123",
                None,
            )
            .expect("pending hitl should register");
        store
            .record_outcome(&TaskOutcome {
                lease: Some(lease),
                state: TaskOutcomeState::Suspended as i32,
                output: None,
                output_artifacts: Vec::new(),
                continuation: Some(checkpoint.clone()),
                evidence: None,
                observed_usage: None,
                error: None,
            })
            .expect("suspension should persist");
        store
            .create_approval(&key, &resource("hitl", "approval-1"), &checkpoint, 1)
            .expect("approval should persist");
        store
    }

    /// A run parked on `approval-1` for a mail send with the skill's ask card (CN-3.5).
    fn mail_card(args: &serde_json::Value) -> crate::hitl::AskApprovalCard {
        let ir = serde_json::json!({
            "state": {"subject": "", "cc": "", "decision": ""},
            "nodes": {
                "ok": {"type": "Button", "props": {"action": {
                    "event": "submit", "set": {"decision": "approve"},
                    "with": ["subject", "cc"]}}}
            }
        });
        crate::hitl::AskApprovalCard {
            jig: "mail.send".into(),
            skill: "urn:vam:skill:local.vam.dev/acme/mail".into(),
            ir: vam_spec::jig::bind_args(&ir, args),
            data: args.clone(),
            submit: vec!["subject".into(), "cc".into()],
            input_schema: serde_json::json!({"type": "object", "properties": {
                "to": {"type": "string"}, "subject": {"type": "string"},
                "cc": {"type": "string"}, "body": {"type": "string"},
                "decision": {"type": "string"}, "count": {"type": "integer"}}}),
        }
    }

    const MAIL_ARGS: &str =
        r#"{"body":"Hello","decision":"x","subject":"Draft","to":"ana@e2e.example"}"#;

    fn parked_card_store(card: bool) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        store
            .create_run(&resource("run", "run-1"))
            .expect("run should be created");
        let lease = lease();
        store.issue_lease(&lease).expect("lease should be issued");
        let checkpoint = artifact("checkpoint-1");
        let key = attempt_key(&lease).expect("lease should have an attempt key");
        let args: serde_json::Value = serde_json::from_str(MAIL_ARGS).unwrap();
        let card = card.then(|| mail_card(&args));
        store
            .register_hitl_suspend_card(
                &key,
                "sess-test",
                &resource("hitl", "approval-1"),
                "mail_send",
                MAIL_ARGS,
                &crate::events::args_digest_from_json(&args),
                None,
                card.as_ref(),
            )
            .expect("pending hitl should register");
        store
            .record_outcome(&TaskOutcome {
                lease: Some(lease),
                state: TaskOutcomeState::Suspended as i32,
                output: None,
                output_artifacts: Vec::new(),
                continuation: Some(checkpoint.clone()),
                evidence: None,
                observed_usage: None,
                error: None,
            })
            .expect("suspension should persist");
        store
            .create_approval(&key, &resource("hitl", "approval-1"), &checkpoint, 1)
            .expect("approval should persist");
        store
    }

    fn decide_card(
        store: &SqliteWorkflowStore,
        decision: &str,
        amended: Option<&str>,
    ) -> Result<ApprovalRecord, OrchestratorError> {
        store.decide_approval_with_surface(
            &resource("hitl", "approval-1"),
            &resource("token", "t1"),
            decision,
            "reviewer",
            crate::users::LOCAL_USER_ID,
            (decision == "REJECTED").then_some("no"),
            amended,
            None,
            None,
            "app",
        )
    }

    fn grant_count(store: &SqliteWorkflowStore) -> i64 {
        store
            .connection
            .lock()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM hitl_grants", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn card_amend_extra_key_is_400_and_row_stays_pending() {
        let store = parked_card_store(true);
        // `to` is a tool input but not a submit field; `decision` is reserved even though the
        // tool's input schema declares it; `nope` is not an input at all.
        for bad in [
            r#"{"to":"evil@e2e.example"}"#,
            r#"{"decision":"approve"}"#,
            r#"{"subject":"ok","body":"x"}"#,
            r#"{"nope":"x"}"#,
        ] {
            let err = decide_card(&store, "AMENDED", Some(bad)).unwrap_err();
            assert!(
                matches!(err, OrchestratorError::InvalidContract(_)),
                "{bad}: {err:?}"
            );
        }
        let row = store.get_approval(APPROVAL_1).unwrap();
        assert_eq!(row.decision, None, "the row stays pending");
        assert_eq!(row.amended_args_json, None);
        assert_eq!(grant_count(&store), 0, "nothing was granted");
    }

    #[test]
    fn card_amend_value_type_checked_against_input_schema() {
        let store = parked_card_store(true);
        for bad in [r#"{"subject":5}"#, r#"{"cc":["a"]}"#, r#"{"subject":null}"#] {
            assert!(
                decide_card(&store, "AMENDED", Some(bad)).is_err(),
                "{bad} must not fit a string field"
            );
        }
        assert_eq!(store.get_approval(APPROVAL_1).unwrap().decision, None);
        assert_eq!(grant_count(&store), 0);
    }

    #[test]
    fn card_amend_merged_args_digest_matches_grant_and_resume() {
        let store = parked_card_store(true);
        decide_card(&store, "AMENDED", Some(r#"{"subject":"Amended subject"}"#)).unwrap();
        let row = store.get_approval(APPROVAL_1).unwrap();
        let merged = row.amended_args_json.clone().expect("merged args stored");
        let value: serde_json::Value = serde_json::from_str(&merged).unwrap();
        // Only the submit field changed; every other key is the requested one.
        assert_eq!(
            value,
            serde_json::json!({"body":"Hello","decision":"x","subject":"Amended subject","to":"ana@e2e.example"})
        );
        // The grant is for the digest of the args that run, as a resumed call computes it ...
        let digest = vam_spec::canonical_args_digest(
            vam_spec::encoding::json_to_proto::<prost_types::Struct>(
                "google.protobuf.Struct",
                &merged,
            )
            .ok()
            .as_ref(),
        )
        .unwrap();
        assert_eq!(row.amended_args_digest.as_deref(), Some(digest.as_str()));
        assert!(store.has_hitl_grant("sess-test", "mail_send", &digest));
        // ... and not for the requested args.
        let requested: serde_json::Value = serde_json::from_str(MAIL_ARGS).unwrap();
        let requested_digest = crate::events::args_digest_from_json(&requested);
        assert_ne!(requested_digest, digest);
        assert!(!store.has_hitl_grant("sess-test", "mail_send", &requested_digest));
        assert_eq!(grant_count(&store), 1);
    }

    #[test]
    fn card_approved_runs_requested_args_and_rejected_grants_nothing() {
        let store = parked_card_store(true);
        decide_card(&store, "APPROVED", None).unwrap();
        let row = store.get_approval(APPROVAL_1).unwrap();
        assert_eq!(row.amended_args_json, None);
        let requested: serde_json::Value = serde_json::from_str(MAIL_ARGS).unwrap();
        assert!(store.has_hitl_grant(
            "sess-test",
            "mail_send",
            &crate::events::args_digest_from_json(&requested)
        ));
        let store = parked_card_store(true);
        decide_card(&store, "REJECTED", None).unwrap();
        assert_eq!(grant_count(&store), 0);
    }

    #[test]
    fn card_approval_keeps_the_always_stamp_and_a_card_less_one_is_as_before() {
        let with = parked_card_store(true).get_approval(APPROVAL_1).unwrap();
        let without = parked_card_store(false).get_approval(APPROVAL_1).unwrap();
        assert_eq!(
            (&with.target, with.always_offered, &with.locked_reason),
            (
                &without.target,
                without.always_offered,
                &without.locked_reason
            ),
            "Always allow / deny is decided outside the card"
        );
        assert!(with.card_json.is_some());
        assert_eq!(without.card_json, None);
        let schema: serde_json::Value =
            serde_json::from_str(&without.amendable_schema_json).unwrap();
        assert_eq!(
            schema,
            default_amendable_schema(&serde_json::from_str(MAIL_ARGS).unwrap())
        );
        // Without a card an edit replaces the args as it always did.
        decide_card(&parked_card_store(false), "AMENDED", Some(r#"{"to":"x"}"#)).unwrap();
    }

    #[test]
    fn card_amendable_schema_is_the_submit_fields_typed_from_the_input_schema() {
        let args: serde_json::Value = serde_json::from_str(MAIL_ARGS).unwrap();
        let schema = mail_card(&args).amendable_schema();
        assert_eq!(
            schema["properties"],
            serde_json::json!({"subject": {"type": "string"}, "cc": {"type": "string"}})
        );
        assert_eq!(schema["additionalProperties"], false);
    }

    fn notification_row(
        store: &SqliteWorkflowStore,
        approval: &str,
    ) -> (
        String,
        String,
        i64,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        let c = store.connection.lock().unwrap();
        c.query_row(
            "SELECT kind, user_id, count, resolved_reason, acted_surface, acted_by_user_id
             FROM notifications WHERE approval_id = ?1",
            [approval],
            |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get(4)?,
                    r.get(5)?,
                ))
            },
        )
        .expect("a notification for the approval")
    }

    const APPROVAL_1: &str = "urn:vam:hitl:local.vam.dev/acme/approval-1";

    /// The path a parked run takes in `vamd` (`apply_guest_task_outcome`, `api/mod.rs`):
    /// `register_hitl_suspend`, `record_outcome(SUSPENDED)`, `create_approval`.
    #[test]
    fn a_parked_tool_call_has_one_open_notification_for_its_owner() {
        let store = parked_store();
        let (kind, user, count, resolved, _, _) = notification_row(&store, APPROVAL_1);
        assert_eq!(
            (kind.as_str(), user.as_str(), count, resolved),
            ("approval.pending", crate::users::LOCAL_USER_ID, 1, None)
        );
    }

    #[test]
    fn a_decision_closes_the_notification_with_the_deciding_surface() {
        let store = parked_store();
        store
            .decide_approval_with_surface(
                &resource("hitl", "approval-1"),
                &resource("token", "t1"),
                "APPROVED",
                "reviewer",
                crate::users::LOCAL_USER_ID,
                None,
                None,
                None,
                None,
                "cli",
            )
            .unwrap();
        let (_, _, count, reason, surface, by) = notification_row(&store, APPROVAL_1);
        assert_eq!(count, 1);
        assert_eq!(reason.as_deref(), Some("acted"));
        assert_eq!(surface.as_deref(), Some("cli"));
        assert_eq!(by.as_deref(), Some(crate::users::LOCAL_USER_ID));
    }

    #[test]
    fn a_stop_closes_the_notification_as_stopped_without_an_acting_surface() {
        let store = parked_store();
        let run = "urn:vam:run:local.vam.dev/acme/run-1";
        store
            .reject_pending_for_stop(run, crate::users::LOCAL_USER_ID, "User::\"u\"", "app")
            .unwrap();
        let (_, _, _, reason, surface, _) = notification_row(&store, APPROVAL_1);
        assert_eq!(reason.as_deref(), Some("stopped"));
        assert_eq!(surface, None);
    }

    #[test]
    fn an_expired_approval_closes_the_notification_as_expired() {
        let store = parked_store();
        store
            .connection
            .lock()
            .unwrap()
            .execute("UPDATE approvals SET expires_at = 1", [])
            .unwrap();
        assert_eq!(store.expire_stale_approvals().unwrap().len(), 1);
        let (_, _, _, reason, surface, _) = notification_row(&store, APPROVAL_1);
        assert_eq!(reason.as_deref(), Some("expired"));
        assert_eq!(surface, None);
    }

    #[test]
    fn no_approval_without_its_notification_and_none_without_the_approval() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store.create_run(&resource("run", "run-1")).unwrap();
        let lease = lease();
        store.issue_lease(&lease).unwrap();
        let checkpoint = artifact("checkpoint-1");
        let key = attempt_key(&lease).unwrap();
        store
            .register_hitl_suspend(
                &key,
                "sess-test",
                &resource("hitl", "approval-1"),
                "jira_create_issue",
                r#"{"title":"issue"}"#,
                "abc123",
                None,
            )
            .unwrap();
        store
            .record_outcome(&TaskOutcome {
                lease: Some(lease),
                state: TaskOutcomeState::Suspended as i32,
                output: None,
                output_artifacts: Vec::new(),
                continuation: Some(checkpoint.clone()),
                evidence: None,
                observed_usage: None,
                error: None,
            })
            .unwrap();
        // The record cannot be written: the whole creation rolls back.
        store
            .connection
            .lock()
            .unwrap()
            .execute_batch("ALTER TABLE notifications RENAME TO notifications_gone")
            .unwrap();
        assert!(store
            .create_approval(&key, &resource("hitl", "approval-1"), &checkpoint, 1)
            .is_err());
        let c = store.connection.lock().unwrap();
        let approvals: i64 = c
            .query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get(0))
            .unwrap();
        let pending: i64 = c
            .query_row("SELECT COUNT(*) FROM hitl_pending", [], |r| r.get(0))
            .unwrap();
        assert_eq!((approvals, pending), (0, 1));
    }

    #[test]
    fn the_users_stop_rejects_a_pending_approval_once_and_a_late_decision_loses() {
        let store = parked_store();
        let run = "urn:vam:run:local.vam.dev/acme/run-1";
        let first = store
            .reject_pending_for_stop(run, crate::users::LOCAL_USER_ID, "User::\"u\"", "app")
            .unwrap();
        assert_eq!(first.lost, 0);
        assert_eq!(first.stopped.len(), 1);
        assert_eq!(first.stopped[0].decision, "REJECTED");
        let stored = store
            .get_approval("urn:vam:hitl:local.vam.dev/acme/approval-1")
            .unwrap();
        assert_eq!(stored.decision.as_deref(), Some("REJECTED"));
        assert_eq!(stored.rejection_reason.as_deref(), Some("stopped by user"));
        let decided: Vec<_> = store
            .read_outbox_for_run(run, 0)
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == "vam.v1.hitl.decided")
            .collect();
        assert_eq!(decided.len(), 1);
        assert!(
            decided[0].payload.contains("REJECTED"),
            "{}",
            decided[0].payload
        );
        assert!(store.has_stop_rejection(run));
        // Again: nothing left to reject, and no second event.
        let again = store
            .reject_pending_for_stop(run, crate::users::LOCAL_USER_ID, "User::\"u\"", "app")
            .unwrap();
        assert!(again.stopped.is_empty() && again.lost == 0);
        // A human decision after the stop finds the approval decided.
        let late = store.decide_approval(
            &resource("hitl", "approval-1"),
            &resource("token", "t-late"),
            "APPROVED",
            "reviewer",
            crate::users::LOCAL_USER_ID,
            None,
            None,
            None,
            None,
        );
        assert!(
            matches!(late, Err(OrchestratorError::ApprovalAlreadyDecided(_))),
            "{late:?}"
        );
    }

    #[test]
    fn a_decision_that_won_stands_and_the_stop_sees_it_as_lost_or_absent() {
        let store = parked_store();
        let run = "urn:vam:run:local.vam.dev/acme/run-1";
        store
            .decide_approval(
                &resource("hitl", "approval-1"),
                &resource("token", "t-first"),
                "APPROVED",
                "reviewer",
                crate::users::LOCAL_USER_ID,
                None,
                None,
                None,
                None,
            )
            .unwrap();
        let rejection = store
            .reject_pending_for_stop(run, crate::users::LOCAL_USER_ID, "User::\"u\"", "app")
            .unwrap();
        assert!(rejection.stopped.is_empty());
        assert!(
            !store.has_stop_rejection(run),
            "the approval stays APPROVED: the stop follows the resume instead"
        );
        let stored = store
            .get_approval("urn:vam:hitl:local.vam.dev/acme/approval-1")
            .unwrap();
        assert_eq!(stored.decision.as_deref(), Some("APPROVED"));
    }

    #[test]
    fn a_pending_question_is_cancelled_by_the_stop() {
        let store = parked_store();
        let run = "urn:vam:run:local.vam.dev/acme/run-1";
        {
            let connection = store.connection.lock().unwrap();
            connection
                .execute("UPDATE approvals SET kind = 'question'", [])
                .unwrap();
        }
        let rejection = store
            .reject_pending_for_stop(run, crate::users::LOCAL_USER_ID, "User::\"u\"", "app")
            .unwrap();
        assert_eq!(rejection.stopped[0].decision, "CANCELLED");
        let stored = store
            .get_approval("urn:vam:hitl:local.vam.dev/acme/approval-1")
            .unwrap();
        assert_eq!(stored.decision.as_deref(), Some("CANCELLED"));
        assert_eq!(stored.rejection_reason.as_deref(), Some("stopped by user"));
    }

    #[test]
    fn approval_carries_the_target_stamped_at_suspend_time() {
        use crate::session_targets::{SessionToolTarget, SourceKind};
        use vam_security::ScopeRule;

        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        let run_id = resource("run", "run-1");
        store.create_run(&run_id).expect("run should be created");
        let lease = lease();
        store.issue_lease(&lease).expect("lease should be issued");
        let row = |target: &str, always: bool, reason: &str| SessionToolTarget {
            tool_name: "jira_create_issue".to_string(),
            target: target.to_string(),
            source_kind: SourceKind::Mcp,
            side_effect: 3,
            source_rule: ScopeRule::None,
            source_label: String::new(),
            agent_rule: ScopeRule::Ask,
            agent_label: "agent draft: tools default".to_string(),
            org_rule: ScopeRule::None,
            deciding_scope: "agent".to_string(),
            deciding_rule: String::new(),
            always_offered: always,
            locked_reason: reason.to_string(),
        };
        store
            .record_session_tool_targets(
                "sess-test",
                &[row("mcp:mcp.atlassian.com/create_issue", true, "")],
            )
            .expect("rows");

        let checkpoint = artifact("checkpoint-1");
        let key = attempt_key(&lease).expect("lease should have an attempt key");
        store
            .register_hitl_suspend(
                &key,
                "sess-test",
                &resource("hitl", "approval-1"),
                "jira_create_issue",
                r#"{"title":"issue"}"#,
                "abc123",
                None,
            )
            .expect("pending hitl should register while RUNNING");
        // Rewriting the session rows after the suspend (the connection was
        // edited or removed) must not change the approval.
        store
            .record_session_tool_targets(
                "sess-test",
                &[row("mcp:other.example/changed", false, "x")],
            )
            .expect("rows rewritten");
        let suspended = TaskOutcome {
            lease: Some(lease.clone()),
            state: TaskOutcomeState::Suspended as i32,
            output: None,
            output_artifacts: Vec::new(),
            continuation: Some(checkpoint.clone()),
            evidence: None,
            observed_usage: None,
            error: None,
        };
        store.record_outcome(&suspended).expect("suspension");
        store
            .create_approval(&key, &resource("hitl", "approval-1"), &checkpoint, 1)
            .expect("approval");

        let listed = store.list_published_approvals().expect("list");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].target, "mcp:mcp.atlassian.com/create_issue");
        assert!(listed[0].always_offered);
        assert_eq!(listed[0].locked_reason, "");
        let fetched = store
            .get_approval(&listed[0].request_id)
            .expect("get approval");
        assert_eq!(fetched.target, listed[0].target);
    }

    #[test]
    fn approval_without_a_session_row_is_stamped_legacy() {
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        let run_id = resource("run", "run-1");
        store.create_run(&run_id).expect("run should be created");
        let lease = lease();
        store.issue_lease(&lease).expect("lease should be issued");
        let checkpoint = artifact("checkpoint-1");
        let key = attempt_key(&lease).expect("lease should have an attempt key");
        store
            .register_hitl_suspend(
                &key,
                "sess-test",
                &resource("hitl", "approval-1"),
                "jira_create_issue",
                "{}",
                "abc123",
                None,
            )
            .expect("register");
        store
            .record_outcome(&TaskOutcome {
                lease: Some(lease.clone()),
                state: TaskOutcomeState::Suspended as i32,
                output: None,
                output_artifacts: Vec::new(),
                continuation: Some(checkpoint.clone()),
                evidence: None,
                observed_usage: None,
                error: None,
            })
            .expect("suspension");
        store
            .create_approval(&key, &resource("hitl", "approval-1"), &checkpoint, 1)
            .expect("approval");
        let listed = store.list_published_approvals().expect("list");
        assert_eq!(listed[0].target, "");
        assert!(!listed[0].always_offered);
        assert_eq!(listed[0].locked_reason, "legacy policy");
    }

    #[test]
    fn unpublished_approval_hidden_from_list() {
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        let run_id = resource("run", "run-unpub");
        store.create_run(&run_id).expect("run");
        let lease = lease_for_run("run-unpub");
        store.issue_lease(&lease).expect("lease");
        let key = attempt_key(&lease).expect("key");
        let connection = store.connection.lock().expect("lock");
        connection
            .execute(
                "INSERT INTO approvals (
                    request_id, run_id, step_id, attempt_id, checkpoint_digest,
                    continuation_generation, published
                 ) VALUES (?1, ?2, ?3, ?4, 'abc', 0, 0)",
                params![
                    "urn:vam:hitl:local.vam.dev/acme/unpub",
                    key.run_id,
                    key.step_id,
                    key.attempt_id,
                ],
            )
            .expect("insert unpublished");
        drop(connection);
        assert!(store.list_published_approvals().expect("list").is_empty());
    }

    #[test]
    fn expire_stale_approvals_returns_session_and_workspace() {
        let store = SqliteWorkflowStore::in_memory().expect("store should initialize");
        store
            .create_session("sess-exp", None, "vm-exp", "ws-exp", "prof-1")
            .expect("session should be created");
        let run_id = resource("run", "run-exp");
        store.create_run(&run_id).expect("run should be created");
        let lease = lease_for_run("run-exp");
        store.issue_lease(&lease).expect("lease should be issued");
        let key = attempt_key(&lease).expect("key");

        {
            let connection = store.connection.lock().expect("lock");
            // Expired: past expires_at, undecided, published, bound to a session.
            connection
                .execute(
                    "INSERT INTO approvals (
                        request_id, run_id, step_id, attempt_id, checkpoint_digest,
                        continuation_generation, session_id, published, expires_at
                     ) VALUES (?1, ?2, ?3, ?4, 'abc', 0, 'sess-exp', 1, 1)",
                    params![
                        "urn:vam:hitl:local.vam.dev/acme/expired",
                        key.run_id,
                        key.step_id,
                        key.attempt_id,
                    ],
                )
                .expect("insert expired approval");
            // Not expired: future expires_at must be left alone.
            connection
                .execute(
                    "INSERT INTO approvals (
                        request_id, run_id, step_id, attempt_id, checkpoint_digest,
                        continuation_generation, session_id, published, expires_at
                     ) VALUES (?1, ?2, ?3, ?4, 'abc', 0, 'sess-exp', 1, 4102444800)",
                    params![
                        "urn:vam:hitl:local.vam.dev/acme/future",
                        key.run_id,
                        key.step_id,
                        key.attempt_id,
                    ],
                )
                .expect("insert future approval");
        }

        let expired = store
            .expire_stale_approvals()
            .expect("expiry sweep should succeed");
        assert_eq!(
            expired,
            vec![ExpiredApproval {
                request_id: "urn:vam:hitl:local.vam.dev/acme/expired".to_string(),
                run_id: run_id.urn.clone(),
                session_id: Some("sess-exp".to_string()),
                workspace_id: Some("ws-exp".to_string()),
            }]
        );

        {
            let connection = store.connection.lock().expect("lock");
            let decision: Option<String> = connection
                .query_row(
                    "SELECT decision FROM approvals WHERE request_id = ?1",
                    ["urn:vam:hitl:local.vam.dev/acme/expired"],
                    |row| row.get(0),
                )
                .expect("expired row");
            assert_eq!(decision.as_deref(), Some("EXPIRED"));
            let future: Option<String> = connection
                .query_row(
                    "SELECT decision FROM approvals WHERE request_id = ?1",
                    ["urn:vam:hitl:local.vam.dev/acme/future"],
                    |row| row.get(0),
                )
                .expect("future row");
            assert_eq!(future, None);
            let run_state: String = connection
                .query_row(
                    "SELECT state FROM runs WHERE run_id = ?1",
                    [&key.run_id],
                    |row| row.get(0),
                )
                .expect("run row");
            assert_eq!(run_state, "FAILED");
        }

        // Second sweep: the row is already EXPIRED, so nothing is returned.
        assert!(store
            .expire_stale_approvals()
            .expect("second sweep should succeed")
            .is_empty());
    }

    #[test]
    fn active_time_excludes_parked_gaps() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let run_id = "urn:vam:run:local.vam.dev/acme/active-time";
        store
            .persist_run_spec(
                run_id,
                r#"{"maxDuration":"1800s"}"#,
                Some("sess-active"),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("spec");
        store
            .create_session("sess-active", None, "vm-active", "ws-active", "default")
            .expect("session");

        store.begin_active_segment(run_id, 1_000).expect("start");
        assert_eq!(store.active_elapsed_secs(run_id, 1_040).expect("open"), 40);
        assert_eq!(
            store
                .remaining_active_budget_secs(run_id, 1_040)
                .expect("budget"),
            Some(1_760)
        );
        assert_eq!(store.end_active_segment(run_id, 1_100).expect("pause"), 100);

        // 24h parked: the segment is closed, so the gap is not active time.
        let after_park = 1_100 + 86_400;
        assert_eq!(
            store
                .active_elapsed_secs(run_id, after_park)
                .expect("parked"),
            100
        );
        assert_eq!(
            store
                .remaining_active_budget_secs(run_id, after_park)
                .expect("budget after park"),
            Some(1_700)
        );

        store
            .begin_active_segment(run_id, after_park)
            .expect("resume");
        assert_eq!(
            store
                .end_active_segment(run_id, after_park + 50)
                .expect("second pause"),
            150
        );
        // Closing again must not add the following parked gap.
        assert_eq!(
            store
                .end_active_segment(run_id, after_park + 50 + 86_400)
                .expect("still closed"),
            150
        );
        assert_eq!(
            store
                .remaining_active_budget_secs(run_id, after_park + 50 + 86_400)
                .expect("final budget"),
            Some(1_650)
        );
    }

    #[test]
    fn active_budget_expiry_releases_reservation_and_skips_parked_runs() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let running = "urn:vam:run:local.vam.dev/acme/duration-running";
        let parked = "urn:vam:run:local.vam.dev/acme/duration-parked";
        store
            .persist_run_spec(
                running,
                r#"{"maxDuration":"30s"}"#,
                Some("sess-duration"),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("running spec");
        store
            .create_session(
                "sess-duration",
                None,
                "vm-duration",
                "ws-duration",
                "default",
            )
            .expect("session");
        store
            .begin_active_segment(running, 1_000)
            .expect("running segment");
        store
            .reserve_workspace("ws-duration", running, 9_000)
            .expect("reserve");
        store.schedule_keep_warm(running, 9_000).expect("warm");

        store
            .persist_run_spec(
                parked,
                r#"{"maxDuration":"30s"}"#,
                None,
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("parked spec");
        store
            .begin_active_segment(parked, 1_000)
            .expect("parked start");
        store
            .end_active_segment(parked, 1_010)
            .expect("parked pause");
        store.update_run_state(parked, "PAUSED").expect("pause");

        let expired = store.expire_over_active_budget(1_050).expect("expire");
        assert_eq!(
            expired,
            vec![ActiveDurationExpiry {
                run_id: running.to_string(),
                session_id: Some("sess-duration".to_string()),
                workspace_id: Some("ws-duration".to_string()),
            }]
        );
        assert_eq!(store.get_run_state(running).expect("state"), "FAILED");
        assert!(store
            .workspace_reservation("ws-duration")
            .expect("reservation")
            .is_none());
        assert_eq!(
            store.keep_warm_deadline(running).expect("warm"),
            None,
            "duration expiry clears a still-warm deadline"
        );
        assert_eq!(
            store.active_elapsed_secs(running, 1_050).expect("frozen"),
            50
        );
        // The paused run's clock stopped at 10s, so a day later it is not due.
        assert_eq!(store.get_run_state(parked).expect("parked"), "PAUSED");
        assert_eq!(
            store
                .active_elapsed_secs(parked, 1_050 + 86_400)
                .expect("parked elapsed"),
            10
        );
        assert!(store
            .expire_over_active_budget(1_050 + 86_400)
            .expect("second pass")
            .is_empty());
    }

    fn pin() -> PlaybookRunPin {
        PlaybookRunPin {
            urn: "urn:vam:playbook:local.vam.dev/acme/demo".to_string(),
            version: "1.0.0".to_string(),
            digest: "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
        }
    }

    fn spec(steps: Vec<PlaybookStep>) -> PlaybookSpec {
        PlaybookSpec {
            resource_id: Some(resource("playbook", "demo")),
            display_name: "demo".to_string(),
            steps,
            ..Default::default()
        }
    }

    fn task_step(
        id: &str,
        after: &[&str],
        after_any: &[&str],
        outcomes: Vec<StepOutcome>,
    ) -> PlaybookStep {
        PlaybookStep {
            id: id.to_string(),
            kind: StepKind::Task as i32,
            instruction: format!("do {id}"),
            after: after.iter().map(|token| (*token).to_string()).collect(),
            after_any: after_any.iter().map(|token| (*token).to_string()).collect(),
            outcomes,
            ..Default::default()
        }
    }

    fn labeled(label: &str, terminal: OutcomeTerminal) -> StepOutcome {
        StepOutcome {
            label: label.to_string(),
            terminal: terminal as i32,
            ..Default::default()
        }
    }

    fn open(label: &str) -> StepOutcome {
        labeled(label, OutcomeTerminal::Unspecified)
    }

    fn fresh(name: &str) -> (SqliteWorkflowStore, String) {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let run = resource("run", name);
        store.create_run(&run).expect("run");
        (store, run.urn)
    }

    fn done(outcome: &str) -> PlaybookStepCompletion {
        PlaybookStepCompletion {
            outcome: outcome.to_string(),
            output_json: Some(r#"{"ok":true}"#.to_string()),
            agent_summary: Some("finished".to_string()),
        }
    }

    fn with_output(outcome: &str, output_json: &str) -> PlaybookStepCompletion {
        PlaybookStepCompletion {
            outcome: outcome.to_string(),
            output_json: Some(output_json.to_string()),
            agent_summary: None,
        }
    }

    fn gate_step(id: &str, after: &[&str]) -> PlaybookStep {
        let mut step = task_step(id, after, &[], vec![]);
        step.kind = StepKind::Gate as i32;
        step.instruction.clear();
        step
    }

    fn branch_gate(id: &str, after: &[&str]) -> PlaybookStep {
        let mut step = gate_step(id, after);
        step.options = vec![
            open("approved"),
            labeled("rejected", OutcomeTerminal::Succeeded),
        ];
        step
    }

    fn json_struct(json: &str) -> prost_types::Struct {
        vam_spec::encoding::json_to_proto("google.protobuf.Struct", json).expect("struct")
    }

    fn gate_request(run_id: &str, step_id: &str) -> String {
        format!("{run_id}/gate/{step_id}")
    }

    fn decide_gate(
        store: &SqliteWorkflowStore,
        run_id: &str,
        step_id: &str,
        decision: &str,
        response: Option<&str>,
    ) -> Result<ApprovalRecord, OrchestratorError> {
        store.decide_approval(
            &ResourceId {
                urn: gate_request(run_id, step_id),
            },
            &ResourceId {
                urn: format!("urn:vam:token:local.vam.dev/acme/{step_id}-{decision}"),
            },
            decision,
            "User::\"local\"",
            crate::users::LOCAL_USER_ID,
            None,
            None,
            None,
            response,
        )
    }

    fn age_approval(store: &SqliteWorkflowStore, request_id: &str) {
        let connection = store.connection.lock().expect("lock");
        connection
            .execute(
                "UPDATE approvals SET expires_at = 1 WHERE request_id = ?1",
                [request_id],
            )
            .expect("age approval");
    }

    fn evidence(tool: &str, result: &str) -> vam_spec::v1::StepEvidence {
        vam_spec::v1::StepEvidence {
            tool: tool.to_string(),
            result: result.to_string(),
        }
    }

    fn observe_tool(store: &SqliteWorkflowStore, run_id: &str, tool: &str, failed: bool) {
        crate::trajectory::emit_guest_tool_outcome(
            store,
            run_id,
            &serde_json::json!({ "tool_name": tool, "exit_code": if failed { 1 } else { 0 } }),
            failed,
        );
    }

    #[test]
    fn evidence_ignores_calls_from_before_the_step_was_available() {
        let (store, run_id) = fresh("evidence-window");
        observe_tool(&store, &run_id, "lookup", false);
        let mut first = task_step("lookup-step", &[], &[], vec![]);
        first.evidence = Some(evidence("lookup", "success"));
        let mut second = task_step("later", &["lookup-step"], &[], vec![]);
        second.evidence = Some(evidence("lookup", "success"));
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![first, second]))
            .expect("init");

        let too_early = store.complete_playbook_step(&run_id, "lookup-step", &done("done"));
        let message = match too_early {
            Err(OrchestratorError::InvalidContract(message)) => message,
            other => panic!("expected evidence rejection, got {other:?}"),
        };
        assert!(
            message.contains("lookup") && message.contains("available"),
            "{message}"
        );
        assert_eq!(
            store.list_playbook_steps(&run_id).expect("steps")[0].state,
            PlaybookStepState::Available
        );

        observe_tool(&store, &run_id, "lookup", true);
        let failed_call = store.complete_playbook_step(&run_id, "lookup-step", &done("done"));
        assert!(
            matches!(failed_call, Err(OrchestratorError::InvalidContract(_))),
            "a failed call does not satisfy result success: {failed_call:?}"
        );

        observe_tool(&store, &run_id, "lookup", false);
        let completed = store
            .complete_playbook_step(&run_id, "lookup-step", &done("done"))
            .expect("matching call");
        assert_eq!(completed[0].state, PlaybookStepState::Completed);
        assert_eq!(completed[0].outcome, "done");
        assert!(
            completed[0]
                .evidence_refs
                .iter()
                .any(|reference| reference.starts_with("vam.v1.tool.completed:")),
            "{:?}",
            completed[0].evidence_refs
        );
        let stored = store.list_playbook_steps(&run_id).expect("reload");
        assert_eq!(stored[0].evidence_refs, completed[0].evidence_refs);

        let still_early = store.complete_playbook_step(&run_id, "later", &done("done"));
        assert!(
            matches!(still_early, Err(OrchestratorError::InvalidContract(_))),
            "the call that completed the previous step was before `later` was available: {still_early:?}"
        );
        observe_tool(&store, &run_id, "lookup", false);
        let later = store
            .complete_playbook_step(&run_id, "later", &done("done"))
            .expect("call while later was available");
        assert!(!later[1].evidence_refs.is_empty());
    }

    #[test]
    fn implicit_failed_outcome_does_not_require_evidence() {
        let (store, run_id) = fresh("evidence-failed");
        let mut step = task_step("lookup-step", &[], &[], vec![]);
        step.evidence = Some(evidence("lookup", "success"));
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![step]))
            .expect("init");
        let steps = store
            .complete_playbook_step(&run_id, "lookup-step", &done("failed"))
            .expect("failed needs no evidence");
        assert_eq!(steps[0].outcome, "failed");
        assert!(steps[0].evidence_refs.is_empty());
        assert_eq!(
            store
                .get_playbook_run(&run_id)
                .expect("run")
                .expect("row")
                .state,
            PLAYBOOK_RUN_FAILED
        );
    }

    #[test]
    fn host_tool_completion_satisfies_success_evidence() {
        let (store, run_id) = fresh("evidence-host");
        let mut step = task_step("echo-step", &[], &[], vec![]);
        step.evidence = Some(evidence("echo", "success"));
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![step]))
            .expect("init");
        crate::trajectory::emit_host_tool_completed(&store, &run_id, "echo", "echo", "");
        let steps = store
            .complete_playbook_step(&run_id, "echo-step", &done("done"))
            .expect("host completion");
        assert!(steps[0]
            .evidence_refs
            .iter()
            .any(|reference| reference.starts_with("vam.v1.host_tool.completed:")));
    }

    fn snapshot(steps: &[PlaybookStepRecord]) -> Vec<(String, PlaybookStepState, String)> {
        steps
            .iter()
            .map(|step| (step.step_id.clone(), step.state, step.outcome.clone()))
            .collect()
    }

    #[test]
    fn preconditions_hold_before_a_step_is_available() {
        let (store, run_id) = fresh("and-run");
        let mut join = task_step("join", &[], &[], vec![]);
        join.depends_on = vec!["left".to_string(), "right".to_string()];
        let steps = store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("left", &[], &[], vec![]),
                    task_step("right", &[], &[], vec![]),
                    join,
                ]),
            )
            .expect("init");
        assert_eq!(
            snapshot(&steps),
            vec![
                (
                    "left".to_string(),
                    PlaybookStepState::Available,
                    String::new()
                ),
                (
                    "right".to_string(),
                    PlaybookStepState::Available,
                    String::new()
                ),
                (
                    "join".to_string(),
                    PlaybookStepState::Pending,
                    String::new()
                ),
            ]
        );
        let after_left = store
            .complete_playbook_step(&run_id, "left", &done("done"))
            .expect("left");
        assert_eq!(after_left[2].state, PlaybookStepState::Pending);
        let after_right = store
            .complete_playbook_step(&run_id, "right", &done("done"))
            .expect("right");
        assert_eq!(after_right[2].state, PlaybookStepState::Available);
        assert_eq!(after_right[0].outcome, "done");
        assert!(after_right[0]
            .output_json
            .as_deref()
            .unwrap()
            .contains("ok"));
        assert_eq!(after_right[0].agent_summary.as_deref(), Some("finished"));
        assert!(after_right[2].started_at.is_some());
    }

    #[test]
    fn contradicted_preconditions_skip_and_propagate() {
        let (store, run_id) = fresh("skip-run");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("choose", &[], &[], vec![open("ok"), open("stop")]),
                    task_step("next", &["choose:ok"], &[], vec![]),
                    task_step("tail", &["next"], &[], vec![]),
                ]),
            )
            .expect("init");
        let steps = store
            .complete_playbook_step(&run_id, "choose", &done("stop"))
            .expect("stop");
        assert_eq!(
            snapshot(&steps),
            vec![
                (
                    "choose".to_string(),
                    PlaybookStepState::Completed,
                    "stop".to_string()
                ),
                (
                    "next".to_string(),
                    PlaybookStepState::Skipped,
                    String::new()
                ),
                (
                    "tail".to_string(),
                    PlaybookStepState::Skipped,
                    String::new()
                ),
            ]
        );
        assert!(steps[1].finished_at.is_some());
        assert_eq!(
            store
                .get_playbook_run(&run_id)
                .expect("header")
                .expect("row")
                .state,
            "active"
        );
    }

    #[test]
    fn after_any_opens_on_one_branch_and_skips_the_other() {
        let (store, run_id) = fresh("any-run");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("choose", &[], &[], vec![open("left"), open("right")]),
                    task_step("left", &["choose:left"], &[], vec![]),
                    task_step("right", &["choose:right"], &[], vec![]),
                    task_step("merge", &[], &["left", "right"], vec![]),
                ]),
            )
            .expect("init");
        let chosen = store
            .complete_playbook_step(&run_id, "choose", &done("left"))
            .expect("left branch");
        assert_eq!(chosen[1].state, PlaybookStepState::Available);
        assert_eq!(chosen[2].state, PlaybookStepState::Skipped);
        assert_eq!(chosen[3].state, PlaybookStepState::Pending);
        let merged = store
            .complete_playbook_step(&run_id, "left", &done("done"))
            .expect("merge opens");
        assert_eq!(merged[3].state, PlaybookStepState::Available);
    }

    #[test]
    fn terminal_outcome_skips_remaining_steps() {
        let (store, run_id) = fresh("terminal-run");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step(
                        "gate",
                        &[],
                        &[],
                        vec![open("ok"), labeled("rejected", OutcomeTerminal::Succeeded)],
                    ),
                    task_step("then", &["gate:ok"], &[], vec![]),
                    task_step("tail", &["then"], &[], vec![]),
                ]),
            )
            .expect("init");
        let steps = store
            .complete_playbook_step(&run_id, "gate", &done("rejected"))
            .expect("rejected");
        assert_eq!(steps[0].state, PlaybookStepState::Completed);
        assert_eq!(steps[1].state, PlaybookStepState::Skipped);
        assert_eq!(steps[2].state, PlaybookStepState::Skipped);
        assert_eq!(
            store
                .get_playbook_run(&run_id)
                .expect("header")
                .expect("row")
                .state,
            "succeeded"
        );
        let error = store
            .complete_playbook_step(&run_id, "then", &done("done"))
            .expect_err("terminal run rejects further completion");
        assert!(matches!(error, OrchestratorError::InvalidState { .. }));
    }

    #[test]
    fn depended_on_failed_stays_open_and_implicit_failed_is_terminal() {
        let (store, run_id) = fresh("failed-open");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("work", &[], &[], vec![open("ok"), open("failed")]),
                    task_step("recover", &["work:failed"], &[], vec![]),
                    task_step("continue", &["work:ok"], &[], vec![]),
                ]),
            )
            .expect("init");
        let failed = store
            .complete_playbook_step(&run_id, "work", &done("failed"))
            .expect("failed outcome");
        assert_eq!(failed[1].state, PlaybookStepState::Available);
        assert_eq!(failed[2].state, PlaybookStepState::Skipped);
        assert_eq!(
            store
                .get_playbook_run(&run_id)
                .expect("header")
                .expect("row")
                .state,
            "active"
        );

        let (store, run_id) = fresh("failed-terminal");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("work", &[], &[], vec![]),
                    task_step("next", &["work"], &[], vec![]),
                ]),
            )
            .expect("init");
        let steps = store
            .complete_playbook_step(&run_id, "work", &done("failed"))
            .expect("implicit failed");
        assert_eq!(steps[0].outcome, "failed");
        assert_eq!(steps[1].state, PlaybookStepState::Skipped);
        assert_eq!(
            store
                .get_playbook_run(&run_id)
                .expect("header")
                .expect("row")
                .state,
            "failed"
        );
    }

    #[test]
    fn available_gate_opens_and_step_complete_cannot_record_it() {
        let (store, run_id) = fresh("gate-open");
        let steps = store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    branch_gate("approve", &[]),
                    task_step("after", &["approve:approved"], &[], vec![]),
                ]),
            )
            .expect("init");
        assert_eq!(steps[0].state, PlaybookStepState::Open);
        assert_eq!(steps[1].state, PlaybookStepState::Pending);
        let approval = store
            .get_approval(&gate_request(&run_id, "approve"))
            .expect("gate approval");
        assert_eq!(approval.kind, "gate");
        assert!(approval.published);
        assert!(approval.decision.is_none());
        assert_eq!(
            approval.expires_at.expect("expires") - approval.created_at,
            DEFAULT_GATE_TIMEOUT_SECS
        );
        let rejected = store
            .complete_playbook_step(&run_id, "approve", &done("approved"))
            .expect_err("gates are not step_complete");
        let OrchestratorError::InvalidContract(message) = rejected else {
            panic!("expected contract error");
        };
        assert!(
            message.contains("step_complete cannot record its outcome"),
            "{message}"
        );
        assert_eq!(
            store.list_playbook_steps(&run_id).expect("steps")[0].state,
            PlaybookStepState::Open
        );
        let before = unix_timestamp();
        let deadline = store
            .park_for_open_gates(&run_id)
            .expect("park")
            .expect("deadline");
        assert!(
            deadline >= before + DEFAULT_KEEP_WARM_SECS
                && deadline <= before + DEFAULT_KEEP_WARM_SECS + 5,
            "unset keep_warm uses the 5 minute default, got {deadline} at {before}"
        );
        assert_eq!(store.get_run_state(&run_id).expect("run"), "PAUSED");
    }

    #[test]
    fn hitl_gate_decision_records_who_where_and_when() {
        let (store, run_id) = fresh("gate-provenance");
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![branch_gate("approve", &[])]))
            .expect("init");
        let before = unix_timestamp();
        store
            .decide_approval_with_surface(
                &ResourceId {
                    urn: gate_request(&run_id, "approve"),
                },
                &ResourceId {
                    urn: "urn:vam:token:local.vam.dev/acme/prov".into(),
                },
                "SUBMITTED",
                "User::\"local\"",
                crate::users::LOCAL_USER_ID,
                None,
                None,
                None,
                Some(r#"{"option":"approved"}"#),
                "cli",
            )
            .expect("decided");
        let rows = store.approval_provenance(&run_id).expect("provenance");
        let gate = rows.iter().find(|r| r.step_id == "approve").expect("gate");
        assert_eq!(gate.decision.as_deref(), Some("SUBMITTED"));
        assert_eq!(gate.decided_surface.as_deref(), Some("cli"));
        assert_eq!(
            gate.decided_by_user_id.as_deref(),
            Some(crate::users::LOCAL_USER_ID)
        );
        assert!(gate.decided_at.expect("time") >= before);
        assert_eq!(
            gate.response_json.as_deref(),
            Some(r#"{"option":"approved"}"#)
        );
    }

    #[test]
    fn a_decision_on_a_finished_run_is_refused_and_changes_nothing() {
        for terminal in ["SUCCEEDED", "FAILED", "CANCELLED"] {
            let (store, run_id) = fresh(&format!("gate-finished-{}", terminal.to_lowercase()));
            store
                .init_playbook_run(&run_id, &pin(), &spec(vec![branch_gate("approve", &[])]))
                .expect("init");
            store.update_run_state(&run_id, terminal).expect("state");
            let err = decide_gate(
                &store,
                &run_id,
                "approve",
                "SUBMITTED",
                Some(r#"{"option":"approved"}"#),
            )
            .expect_err("refused");
            assert!(matches!(err, OrchestratorError::Conflict(_)), "{err}");
            assert_eq!(store.get_run_state(&run_id).expect("state"), terminal);
            let rows = store.approval_provenance(&run_id).expect("rows");
            assert!(rows[0].decision.is_none(), "approval stays undecided");
        }
    }

    #[test]
    fn hitl_a_decision_without_a_surface_reads_api_and_an_expiry_reads_system() {
        let (store, run_id) = fresh("gate-surface-default");
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![branch_gate("approve", &[])]))
            .expect("init");
        decide_gate(
            &store,
            &run_id,
            "approve",
            "SUBMITTED",
            Some(r#"{"option":"approved"}"#),
        )
        .expect("decided");
        let rows = store.approval_provenance(&run_id).expect("rows");
        assert_eq!(rows[0].decided_surface.as_deref(), Some("api"));

        let (store, run_id) = fresh("gate-expiry");
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![branch_gate("approve", &[])]))
            .expect("init");
        age_approval(&store, &gate_request(&run_id, "approve"));
        store.expire_stale_approvals().expect("sweep");
        let rows = store.approval_provenance(&run_id).expect("rows");
        assert_eq!(rows[0].decision.as_deref(), Some("EXPIRED"));
        assert_eq!(rows[0].decided_surface.as_deref(), Some("system"));
        assert!(rows[0].decided_at.is_some());
    }

    #[test]
    fn submitted_option_completes_the_gate_and_is_the_step_output() {
        let (store, run_id) = fresh("gate-submit");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    branch_gate("approve", &[]),
                    task_step("after", &["approve:approved"], &[], vec![]),
                ]),
            )
            .expect("init");
        let bogus = decide_gate(
            &store,
            &run_id,
            "approve",
            "SUBMITTED",
            Some(r#"{"option":"nope"}"#),
        )
        .expect_err("undeclared option");
        assert!(matches!(bogus, OrchestratorError::InvalidContract(_)));
        assert!(store
            .get_approval(&gate_request(&run_id, "approve"))
            .expect("still open")
            .decision
            .is_none());
        let approved = decide_gate(
            &store,
            &run_id,
            "approve",
            "APPROVED",
            Some(r#"{"option":"approved"}"#),
        )
        .expect_err("tool decision");
        assert!(matches!(approved, OrchestratorError::InvalidContract(_)));
        decide_gate(
            &store,
            &run_id,
            "approve",
            "SUBMITTED",
            Some(r#"{"option":"approved"}"#),
        )
        .expect("submitted");
        let steps = store.list_playbook_steps(&run_id).expect("steps");
        assert_eq!(steps[0].state, PlaybookStepState::Completed);
        assert_eq!(steps[0].outcome, "approved");
        assert_eq!(
            steps[0].output_json.as_deref(),
            Some(r#"{"option":"approved"}"#)
        );
        assert_eq!(steps[1].state, PlaybookStepState::Available);
        let stored = store
            .get_approval(&gate_request(&run_id, "approve"))
            .expect("decided");
        assert_eq!(stored.decision.as_deref(), Some("SUBMITTED"));
        assert_eq!(
            stored.amended_args_json.as_deref(),
            Some(r#"{"option":"approved"}"#)
        );
    }

    #[test]
    fn form_gate_validates_the_response_schema() {
        let (store, run_id) = fresh("gate-form");
        let mut form = gate_step("review", &[]);
        form.response_schema = Some(json_struct(
            r#"{"type":"object","additionalProperties":false,"required":["comment"],"properties":{"comment":{"type":"string"}}}"#,
        ));
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![form]))
            .expect("init");
        let invalid = decide_gate(
            &store,
            &run_id,
            "review",
            "SUBMITTED",
            Some(r#"{"comment":1}"#),
        )
        .expect_err("wrong type");
        let OrchestratorError::InvalidContract(message) = invalid else {
            panic!("expected contract error");
        };
        assert!(message.contains("expected string"), "{message}");
        decide_gate(
            &store,
            &run_id,
            "review",
            "SUBMITTED",
            Some(r#"{"comment":"looks good"}"#),
        )
        .expect("valid form");
        let step = &store.list_playbook_steps(&run_id).expect("steps")[0];
        assert_eq!(step.state, PlaybookStepState::Completed);
        assert_eq!(step.outcome, "submitted");
        assert_eq!(
            step.output_json.as_deref(),
            Some(r#"{"comment":"looks good"}"#)
        );
    }

    #[test]
    fn form_reject_records_the_rejected_outcome() {
        let (store, run_id) = fresh("gate-form-reject");
        let mut form = gate_step("review", &[]);
        form.response_schema = Some(json_struct(
            r#"{"type":"object","properties":{"comment":{"type":"string"}}}"#,
        ));
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![form]))
            .expect("init");
        decide_gate(&store, &run_id, "review", "REJECTED", None).expect("rejected");
        let step = &store.list_playbook_steps(&run_id).expect("steps")[0];
        assert_eq!(step.outcome, "rejected");
        assert_eq!(
            step.output_json.as_deref(),
            Some(r#"{"outcome":"rejected"}"#)
        );
    }

    #[test]
    fn choices_from_fills_the_enum_from_the_preceding_output() {
        let (store, run_id) = fresh("gate-choices");
        let mut gate = branch_gate("pick", &["list"]);
        gate.choices_from = "steps.list.output.issues".to_string();
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![task_step("list", &[], &[], vec![]), gate]),
            )
            .expect("init");
        store
            .complete_playbook_step(
                &run_id,
                "list",
                &with_output("done", r#"{"issues":["LIN-1","LIN-2"]}"#),
            )
            .expect("list");
        let schema: serde_json::Value = serde_json::from_str(
            &store
                .get_approval(&gate_request(&run_id, "pick"))
                .expect("opened")
                .amendable_schema_json,
        )
        .expect("schema json");
        assert_eq!(
            schema["properties"]["selected"]["items"]["enum"],
            serde_json::json!(["LIN-1", "LIN-2"])
        );
        let outside = decide_gate(
            &store,
            &run_id,
            "pick",
            "SUBMITTED",
            Some(r#"{"option":"approved","selected":["OTHER"]}"#),
        )
        .expect_err("not in enum");
        assert!(matches!(outside, OrchestratorError::InvalidContract(_)));
        decide_gate(
            &store,
            &run_id,
            "pick",
            "SUBMITTED",
            Some(r#"{"option":"approved","selected":["LIN-1"]}"#),
        )
        .expect("choice");
        let step = store
            .list_playbook_steps(&run_id)
            .expect("steps")
            .into_iter()
            .find(|step| step.step_id == "pick")
            .expect("pick");
        assert_eq!(step.outcome, "approved");
        assert_eq!(
            step.output_json.as_deref(),
            Some(r#"{"option":"approved","selected":["LIN-1"]}"#)
        );
    }

    #[test]
    fn gate_timeout_uses_the_step_duration_when_set() {
        let (store, run_id) = fresh("gate-timeout");
        let mut gate = branch_gate("approve", &[]);
        gate.timeout = Some(prost_types::Duration {
            seconds: 120,
            nanos: 0,
        });
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![gate]))
            .expect("init");
        let approval = store
            .get_approval(&gate_request(&run_id, "approve"))
            .expect("approval");
        assert_eq!(
            approval.expires_at.expect("expires") - approval.created_at,
            120
        );
    }

    #[test]
    fn on_timeout_resumes_a_parked_gate_with_that_outcome() {
        let (store, run_id) = fresh("gate-on-timeout");
        let mut gate = branch_gate("approve", &[]);
        gate.on_timeout = "approved".to_string();
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    gate,
                    task_step("after", &["approve:approved"], &[], vec![]),
                ]),
            )
            .expect("init");
        store
            .set_run_session_id(&run_id, "sess-gate-timeout")
            .expect("session");
        store
            .reserve_workspace("ws-gate-timeout", &run_id, 9_000_000_000)
            .expect("reserve");
        let request_id = gate_request(&run_id, "approve");
        age_approval(&store, &request_id);
        assert!(
            store
                .expire_stale_approvals()
                .expect("skip on_timeout")
                .is_empty(),
            "on_timeout gates stay for the timeout sweeper"
        );
        assert!(store.park_for_open_gates(&run_id).expect("park").is_some());
        let resumes = store.apply_gate_timeouts().expect("timeout");
        assert_eq!(resumes.len(), 1);
        assert_eq!(resumes[0].request_id, request_id);
        assert_eq!(resumes[0].session_id, "sess-gate-timeout");
        let steps = store.list_playbook_steps(&run_id).expect("steps");
        assert_eq!(steps[0].state, PlaybookStepState::Completed);
        assert_eq!(steps[0].outcome, "approved");
        assert_eq!(
            steps[0].output_json.as_deref(),
            Some(r#"{"on_timeout":"approved"}"#)
        );
        assert_eq!(steps[1].state, PlaybookStepState::Available);
        assert_eq!(
            store.get_run_state(&run_id).expect("run"),
            "PAUSED",
            "the resume caller moves a paused run back to running"
        );
        assert!(store
            .workspace_reservation("ws-gate-timeout")
            .expect("lookup")
            .is_some());
    }

    #[test]
    fn gate_without_on_timeout_expires_the_run_and_releases_the_reservation() {
        let (store, run_id) = fresh("gate-expire");
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![branch_gate("approve", &[])]))
            .expect("init");
        store
            .reserve_workspace("ws-gate-expire", &run_id, 9_000_000_000)
            .expect("reserve");
        age_approval(&store, &gate_request(&run_id, "approve"));
        let expired = store.expire_stale_approvals().expect("expire");
        assert_eq!(expired.len(), 1);
        assert_eq!(store.get_run_state(&run_id).expect("run"), "EXPIRED");
        assert_eq!(
            store
                .get_playbook_run(&run_id)
                .expect("header")
                .expect("row")
                .state,
            "failed"
        );
        assert!(store
            .workspace_reservation("ws-gate-expire")
            .expect("lookup")
            .is_none());
        assert_eq!(
            store
                .get_approval(&gate_request(&run_id, "approve"))
                .expect("row")
                .decision
                .as_deref(),
            Some("EXPIRED")
        );
    }

    #[test]
    fn a_run_waits_only_on_gates_once_the_last_task_step_is_done() {
        let (store, run_id) = fresh("gate-only-wait");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("prep", &[], &[], vec![]),
                    branch_gate("approve", &["prep"]),
                ]),
            )
            .expect("init");
        assert!(
            !store.playbook_waits_only_on_gates(&run_id).expect("check"),
            "a task step is still available and no gate is open"
        );
        store
            .complete_playbook_step(&run_id, "prep", &done("done"))
            .expect("prep");
        assert!(
            store.playbook_waits_only_on_gates(&run_id).expect("check"),
            "the gate is open and nothing else can run"
        );
        assert!(
            !store
                .playbook_waits_only_on_gates("urn:vam:run:local.vam.dev/acme/no-playbook")
                .expect("no playbook"),
            "a run with no playbook never waits on a gate"
        );
    }

    #[test]
    fn open_gate_parks_when_no_task_is_available() {
        let (store, run_id) = fresh("gate-park");
        let mut gate = branch_gate("approve", &["prep"]);
        gate.keep_warm = Some(prost_types::Duration {
            seconds: 90,
            nanos: 0,
        });
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![task_step("prep", &[], &[], vec![]), gate]),
            )
            .expect("init");
        assert!(store.park_for_open_gates(&run_id).expect("busy").is_none());
        store
            .complete_playbook_step(&run_id, "prep", &done("done"))
            .expect("prep");
        store
            .set_run_session_id(&run_id, "sess-gate-park")
            .expect("session");
        let before = unix_timestamp();
        let deadline = store
            .park_for_open_gates(&run_id)
            .expect("park")
            .expect("deadline");
        assert!(
            deadline >= before + 90 && deadline <= before + 95,
            "{deadline}"
        );
        assert_eq!(store.get_run_state(&run_id).expect("run"), "PAUSED");
        assert_eq!(
            store.keep_warm_deadline(&run_id).expect("warm"),
            Some(deadline)
        );
        let approval = store
            .get_approval(&gate_request(&run_id, "approve"))
            .expect("approval");
        assert_eq!(approval.session_id, "sess-gate-park");
        match store.check_playbook_finish(&run_id, None).expect("finish") {
            Some(PlaybookFinishCheck::Incomplete {
                missing_steps,
                open_gates,
                schema_error,
            }) => {
                assert_eq!(missing_steps, vec!["approve".to_string()]);
                assert_eq!(open_gates, vec!["approve".to_string()]);
                assert!(schema_error.is_none());
            }
            other => panic!("open gate stays unfinished, got {other:?}"),
        }
    }

    #[test]
    fn submitted_is_rejected_for_a_tool_approval() {
        let (store, run_id) = fresh("tool-submitted");
        store
            .record_published_approval("appr-tool", &run_id, crate::users::LOCAL_USER_ID, "{}")
            .expect("publish");
        let err = store
            .decide_approval(
                &ResourceId {
                    urn: "appr-tool".to_string(),
                },
                &ResourceId {
                    urn: "urn:vam:token:local.vam.dev/acme/tool-submitted".to_string(),
                },
                "SUBMITTED",
                "User::\"local\"",
                crate::users::LOCAL_USER_ID,
                None,
                None,
                None,
                Some(r#"{}"#),
            )
            .expect_err("tool approvals do not submit");
        let OrchestratorError::InvalidContract(message) = err else {
            panic!("expected contract error");
        };
        assert!(
            message.contains("SUBMITTED is only valid for a gate"),
            "{message}"
        );
    }

    #[test]
    fn illegal_step_transitions_are_rejected() {
        let (store, run_id) = fresh("illegal");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("first", &[], &[], vec![]),
                    task_step("second", &["first"], &[], vec![]),
                ]),
            )
            .expect("init");
        let pending = store
            .complete_playbook_step(&run_id, "second", &done("done"))
            .expect_err("pending");
        assert!(matches!(
            pending,
            OrchestratorError::InvalidState {
                expected: "available",
                ..
            }
        ));
        let unknown = store
            .complete_playbook_step(&run_id, "first", &done("nope"))
            .expect_err("unknown outcome");
        assert!(matches!(unknown, OrchestratorError::InvalidContract(_)));
        store
            .complete_playbook_step(&run_id, "first", &done("done"))
            .expect("complete");
        let again = store
            .complete_playbook_step(&run_id, "first", &done("done"))
            .expect_err("already completed");
        assert!(matches!(again, OrchestratorError::InvalidState { .. }));
        let missing = store
            .complete_playbook_step(&run_id, "missing", &done("done"))
            .expect_err("missing step");
        assert!(matches!(missing, OrchestratorError::NotFound(_)));
        let duplicate = store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![task_step("only", &[], &[], vec![])]),
            )
            .expect_err("second init");
        assert!(matches!(duplicate, OrchestratorError::Conflict(_)));
    }

    #[test]
    fn step_output_schema_mismatch_is_rejected() {
        let (store, run_id) = fresh("schema");
        let mut step = task_step("first", &[], &[], vec![]);
        step.output_schema = Some(prost_types::Struct {
            fields: [
                (
                    "type".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue("object".into())),
                    },
                ),
                (
                    "additionalProperties".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::BoolValue(false)),
                    },
                ),
                (
                    "required".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::ListValue(
                            prost_types::ListValue {
                                values: vec![prost_types::Value {
                                    kind: Some(prost_types::value::Kind::StringValue(
                                        "ticket".into(),
                                    )),
                                }],
                            },
                        )),
                    },
                ),
                (
                    "properties".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StructValue(prost_types::Struct {
                            fields: [(
                                "ticket".to_string(),
                                prost_types::Value {
                                    kind: Some(prost_types::value::Kind::StructValue(
                                        prost_types::Struct {
                                            fields: [(
                                                "type".to_string(),
                                                prost_types::Value {
                                                    kind: Some(
                                                        prost_types::value::Kind::StringValue(
                                                            "string".into(),
                                                        ),
                                                    ),
                                                },
                                            )]
                                            .into_iter()
                                            .collect(),
                                        },
                                    )),
                                },
                            )]
                            .into_iter()
                            .collect(),
                        })),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        });
        store
            .init_playbook_run(&run_id, &pin(), &spec(vec![step]))
            .expect("init");
        let missing = store
            .complete_playbook_step(
                &run_id,
                "first",
                &PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect_err("schema");
        match missing {
            OrchestratorError::InvalidContract(msg) => {
                assert!(
                    msg.contains("output.ticket") || msg.contains("ticket"),
                    "unexpected schema error: {msg}"
                );
            }
            other => panic!("expected InvalidContract, got {other:?}"),
        }
        store
            .complete_playbook_step(
                &run_id,
                "first",
                &PlaybookStepCompletion {
                    outcome: "done".to_string(),
                    output_json: Some(r#"{"ticket":"PROJ-1"}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect("schema ok");
    }

    #[test]
    fn playbook_step_state_survives_reopen() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workflow.db");
        let run_id = {
            let store = SqliteWorkflowStore::open(&path).expect("open");
            let run = resource("run", "durable");
            store.create_run(&run).expect("run");
            store
                .init_playbook_run(
                    &run.urn,
                    &pin(),
                    &spec(vec![
                        task_step("first", &[], &[], vec![]),
                        task_step("second", &["first"], &[], vec![]),
                    ]),
                )
                .expect("init");
            store
                .complete_playbook_step(&run.urn, "first", &done("done"))
                .expect("complete");
            run.urn
        };
        let store = SqliteWorkflowStore::open(&path).expect("reopen");
        let steps = store.list_playbook_steps(&run_id).expect("list");
        assert_eq!(steps[0].state, PlaybookStepState::Completed);
        assert_eq!(steps[0].outcome, "done");
        assert_eq!(steps[1].state, PlaybookStepState::Available);
        let header = store
            .get_playbook_run(&run_id)
            .expect("header")
            .expect("row");
        assert_eq!(header.urn, pin().urn);
        assert_eq!(header.version, pin().version);
        assert_eq!(header.digest, pin().digest);
        assert_eq!(header.state, "active");
        assert!(store
            .list_playbook_steps("urn:vam:run:local.vam.dev/acme/missing")
            .expect("empty")
            .is_empty());
    }

    #[test]
    fn check_playbook_finish_none_without_playbook() {
        let (store, run_id) = fresh("no-playbook-finish");
        assert_eq!(
            store.check_playbook_finish(&run_id, None).expect("check"),
            None
        );
    }

    fn mode_step(id: &str, mode: StepMode, tool: Option<&str>) -> PlaybookStep {
        PlaybookStep {
            id: id.to_string(),
            kind: StepKind::Task as i32,
            mode: mode as i32,
            binding: tool.map(|name| StepBinding {
                tool_name: name.to_string(),
                arguments: None,
            }),
            ..Default::default()
        }
    }

    #[test]
    fn playbook_finish_observation_names_decided_gates() {
        let steps = vec![
            gate_step("decide", &[]),
            mode_step("schedule", StepMode::Agentic, None),
        ];
        let decided = vec![("decide".to_string(), "pay".to_string())];
        let text =
            playbook_finish_observation(&["schedule".to_string()], &[], &decided, None, &steps);
        assert!(text.contains("Decided gates: decide chose pay."));
        assert!(text.contains("schedule: Do this step yourself"));
    }

    #[test]
    fn playbook_finish_observation_lists_each_missing_step_action() {
        let steps = vec![
            mode_step("lookup", StepMode::Bound, Some("echo")),
            mode_step("fill", StepMode::Assisted, Some("search")),
            mode_step("extract", StepMode::Agentic, None),
            gate_step("approve", &[]),
        ];
        let missing = vec![
            "lookup".to_string(),
            "fill".to_string(),
            "extract".to_string(),
            "approve".to_string(),
        ];
        let text = playbook_finish_observation(
            &missing,
            &["approve".to_string()],
            &[],
            Some("missing total"),
            &steps,
        );
        assert!(text.starts_with("Playbook finish is incomplete. Do not end the task yet.\n"));
        assert!(text.contains("Missing required steps: lookup, fill, extract, approve."));
        assert!(text.contains("Open gates: approve."));
        assert!(!text.contains("Decided gates:"));
        assert!(text.contains("Output schema: missing total."));
        assert!(text.contains(
            "lookup: Complete it by calling run_step with step_id \"lookup\". The host runs the bound tool."
        ));
        assert!(text.contains(
            "fill: Call \"search\" yourself, filling its arguments from context, then call step_complete with step_id \"fill\" and an outcome describing what you did. Do not call run_step for it."
        ));
        assert!(text.contains(
            "extract: Do this step yourself with the tools you have, then call step_complete with step_id \"extract\" and an outcome describing what you did. Never invent results. If you cannot do it with your tools, say so in the outcome."
        ));
        assert!(text.contains(
            "approve: Wait for the human decision on the gate approval. Do not call run_step or step_complete for it."
        ));
        assert!(text.contains(PLAYBOOK_FINISH_STEP_GUIDANCE));
        assert!(text.ends_with(vam_spec::playbook::PLAYBOOK_GATE_RULE));
    }

    #[test]
    fn check_playbook_finish_incomplete_lists_required_available_steps() {
        let (store, run_id) = fresh("finish-incomplete");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("first", &[], &[], vec![]),
                    task_step("second", &["first"], &[], vec![]),
                ]),
            )
            .expect("init");
        match store.check_playbook_finish(&run_id, None).expect("check") {
            Some(PlaybookFinishCheck::Incomplete {
                missing_steps,
                open_gates,
                schema_error,
            }) => {
                assert_eq!(
                    missing_steps,
                    vec!["first".to_string(), "second".to_string()]
                );
                assert!(open_gates.is_empty());
                assert!(schema_error.is_none());
            }
            other => panic!("expected incomplete, got {other:?}"),
        }
        store
            .complete_playbook_step(&run_id, "first", &done("done"))
            .expect("first");
        match store.check_playbook_finish(&run_id, None).expect("check") {
            Some(PlaybookFinishCheck::Incomplete {
                missing_steps,
                open_gates,
                ..
            }) => {
                assert_eq!(missing_steps, vec!["second".to_string()]);
                assert!(open_gates.is_empty());
            }
            other => panic!("expected incomplete after first, got {other:?}"),
        }
    }

    #[test]
    fn check_playbook_finish_complete_when_required_steps_done() {
        let (store, run_id) = fresh("finish-complete");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("first", &[], &[], vec![]),
                    task_step("second", &["first"], &[], vec![]),
                ]),
            )
            .expect("init");
        store
            .complete_playbook_step(&run_id, "first", &done("done"))
            .expect("first");
        store
            .complete_playbook_step(&run_id, "second", &done("done"))
            .expect("second");
        assert_eq!(
            store.check_playbook_finish(&run_id, None).expect("check"),
            Some(PlaybookFinishCheck::Complete)
        );
    }

    #[test]
    fn check_playbook_finish_ignores_optional_and_skipped() {
        let (store, run_id) = fresh("finish-optional-skip");
        let mut optional = task_step("extra", &["first"], &[], vec![]);
        optional.optional = true;
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("first", &[], &[], vec![open("left"), open("right")]),
                    task_step("left_path", &["first:left"], &[], vec![]),
                    task_step("right_path", &["first:right"], &[], vec![]),
                    optional,
                ]),
            )
            .expect("init");
        store
            .complete_playbook_step(&run_id, "first", &done("left"))
            .expect("first");
        store
            .complete_playbook_step(&run_id, "left_path", &done("done"))
            .expect("left");
        let steps = store.list_playbook_steps(&run_id).expect("steps");
        assert_eq!(steps[2].state, PlaybookStepState::Skipped); // right_path
        assert_eq!(steps[3].state, PlaybookStepState::Available); // optional unfinished
        assert_eq!(
            store.check_playbook_finish(&run_id, None).expect("check"),
            Some(PlaybookFinishCheck::Complete)
        );
    }

    fn run_output_schema() -> prost_types::Struct {
        json_struct(
            r#"{"type":"object","additionalProperties":false,"required":["report_file_path","transitioned_count"],"properties":{"report_file_path":{"type":"string"},"transitioned_count":{"type":"integer"}}}"#,
        )
    }

    #[test]
    fn check_playbook_finish_rejects_run_output_that_misses_the_schema() {
        let (store, run_id) = fresh("finish-schema");
        let mut spec = spec(vec![task_step("only", &[], &[], vec![])]);
        spec.output_schema = Some(run_output_schema());
        store
            .init_playbook_run(&run_id, &pin(), &spec)
            .expect("init");
        store
            .complete_playbook_step(&run_id, "only", &done("done"))
            .expect("done");
        match store
            .check_playbook_finish(&run_id, Some(r#"{"answer":"finished"}"#))
            .expect("check")
        {
            Some(PlaybookFinishCheck::Incomplete {
                missing_steps,
                open_gates,
                schema_error,
            }) => {
                assert!(missing_steps.is_empty());
                assert!(open_gates.is_empty());
                let schema_error = schema_error.expect("schema error");
                assert!(
                    schema_error.contains("report_file_path") || schema_error.contains("answer"),
                    "{schema_error}"
                );
            }
            other => panic!("schema mismatch is incomplete, got {other:?}"),
        }
        assert_eq!(
            store
                .check_playbook_finish(
                    &run_id,
                    Some(r#"{"report_file_path":"notes/report.md","transitioned_count":2}"#)
                )
                .expect("check"),
            Some(PlaybookFinishCheck::Complete)
        );
    }

    #[test]
    fn check_playbook_finish_lists_an_open_gate_beside_a_missing_step() {
        let (store, run_id) = fresh("finish-open-gate");
        store
            .init_playbook_run(
                &run_id,
                &pin(),
                &spec(vec![
                    task_step("work", &[], &[], vec![]),
                    branch_gate("approve", &[]),
                ]),
            )
            .expect("init");
        match store.check_playbook_finish(&run_id, None).expect("check") {
            Some(PlaybookFinishCheck::Incomplete {
                missing_steps,
                open_gates,
                ..
            }) => {
                assert_eq!(
                    missing_steps,
                    vec!["work".to_string(), "approve".to_string()]
                );
                assert_eq!(open_gates, vec!["approve".to_string()]);
            }
            other => panic!("expected open gate and missing step, got {other:?}"),
        }
    }

    #[test]
    fn run_metering_persists_across_reopen_and_session_change() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("workflow.db");
        let run_id = "urn:vam:run:local.vam.dev/acme/meter";
        let other_run = "urn:vam:run:local.vam.dev/acme/meter-other";

        {
            let store = SqliteWorkflowStore::open(&path).expect("open");
            let budget = vam_inference::SessionBudget::new(None);
            budget
                .record_usage(6, 4, 1_000_000, 1_000_000)
                .expect("usage");
            let first = store.apply_run_metering(run_id, &budget).expect("first");
            assert_eq!(first.iteration, 1);
            assert_eq!(first.spend_micro, 10);
            assert_eq!(first.token_count, 10);
            assert_eq!(first.prompt_tokens, 6);
            assert_eq!(first.completion_tokens, 4);
            let second = store.apply_run_metering(run_id, &budget).expect("second");
            assert_eq!(second.iteration, 2);
            assert_eq!(second.spend_micro, 10);
            assert_eq!(budget.get_accumulated_cost(), 10);

            let fresh = vam_inference::SessionBudget::new(None);
            let other = store
                .apply_run_metering(other_run, &fresh)
                .expect("other run");
            assert_eq!(other.iteration, 1);
            assert_eq!(other.spend_micro, 0);
        }

        let store = SqliteWorkflowStore::open(&path).expect("reopen");
        let metering = store
            .get_run_metering(run_id)
            .expect("read")
            .expect("persisted row");
        assert_eq!(metering.iteration, 2);
        assert_eq!(metering.spend_micro, 10);
        assert_eq!(metering.token_count, 10);
        assert_eq!(metering.prompt_tokens, 6);
        assert_eq!(metering.completion_tokens, 4);
        assert_eq!(
            store
                .get_run_metering(other_run)
                .expect("read")
                .expect("other")
                .iteration,
            1
        );

        store
            .persist_run_spec(
                run_id,
                "{}",
                Some("sess-a"),
                "urn:vam:agent:meter",
                None,
                "User::\"tester\"",
            )
            .expect("bind");
        store
            .set_run_session_id(run_id, "sess-b")
            .expect("rebind session");
        let resumed = std::sync::Arc::new(vam_inference::SessionBudget::new(None));
        store.register_session_budget("sess-b", resumed.clone());
        assert_eq!(resumed.get_accumulated_cost(), 10);
        assert_eq!(resumed.get_accumulated_tokens(), 10);
        assert_eq!(resumed.get_accumulated_prompt_tokens(), 6);
        assert_eq!(resumed.get_accumulated_completion_tokens(), 4);
        store.register_session_budget("sess-b", resumed.clone());
        assert_eq!(resumed.get_accumulated_cost(), 10);

        let stale = std::sync::Arc::new(vam_inference::SessionBudget::new(None));
        store.register_session_budget("sess-a", stale.clone());
        assert_eq!(stale.get_accumulated_cost(), 0);
    }

    fn park_fixture(
        name: &str,
        warm_until: i64,
        expires_at: Option<i64>,
    ) -> (SqliteWorkflowStore, String) {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let run_id = resource("run", name);
        let session_id = format!("sess-{name}");
        let workspace_id = format!("ws-{name}");
        store
            .create_session(
                &session_id,
                None,
                &format!("vm-{name}"),
                &workspace_id,
                "prof",
            )
            .expect("session");
        store.create_run(&run_id).expect("run");
        let mut lease = lease_for_run(name);
        lease.attempt_id = Some(resource("attempt", name));
        store.issue_lease(&lease).expect("lease");
        let key = attempt_key(&lease).expect("key");
        let approval = resource("hitl", name);
        store
            .register_hitl_suspend(
                &key,
                &session_id,
                &approval,
                "jira_transition_issue",
                r#"{"issue_key":"CGA-1"}"#,
                "digest",
                expires_at,
            )
            .expect("pending");
        store
            .record_outcome(&TaskOutcome {
                lease: Some(lease),
                state: TaskOutcomeState::Suspended as i32,
                continuation: Some(artifact(name)),
                ..Default::default()
            })
            .expect("outcome");
        store
            .create_approval(&key, &approval, &artifact(name), 1)
            .expect("approval");
        store
            .update_run_state(&run_id.urn, "PAUSED")
            .expect("pause");
        store
            .schedule_keep_warm(&run_id.urn, warm_until)
            .expect("warm");
        (store, run_id.urn)
    }

    #[test]
    fn keep_warm_due_only_while_undecided_and_past_deadline() {
        let (future, _) = park_fixture("park-future", 4_102_444_800, Some(4_102_444_800));
        assert!(
            future
                .runs_due_to_park(unix_timestamp())
                .expect("due")
                .is_empty(),
            "a future keep-warm deadline must not park"
        );

        let (store, run_id) = park_fixture("park-due", 1, Some(4_102_444_800));
        let due = store.runs_due_to_park(unix_timestamp()).expect("due");
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].run_id, run_id);
        assert_eq!(due[0].workspace_id, "ws-park-due");
        assert_eq!(due[0].reservation_expires_at, 4_102_444_800);

        {
            let connection = store.connection.lock().expect("lock");
            connection
                .execute(
                    "UPDATE approvals SET decision = 'APPROVED' WHERE run_id = ?1",
                    [&run_id],
                )
                .expect("decide");
        }
        assert!(
            store
                .runs_due_to_park(unix_timestamp())
                .expect("due")
                .is_empty(),
            "a decision ends the keep-warm wait"
        );
    }

    #[test]
    fn reservation_blocks_until_expiry_or_cancel() {
        let (store, run_id) = park_fixture("reserve", 1, Some(4_102_444_800));
        let other = resource("run", "reserve-other");
        store.create_run(&other).expect("second run");
        store
            .reserve_workspace("ws-reserve", &run_id, 4_102_444_800)
            .expect("reserve");
        let active = store
            .active_workspace_reservation("ws-reserve", unix_timestamp())
            .expect("lookup")
            .expect("held");
        assert_eq!(active.run_id, run_id);
        assert!(active.created_at > 0);
        let err = store
            .reserve_workspace("ws-reserve", &other.urn, 4_102_444_800)
            .expect_err("a different run cannot take the workspace");
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err}");

        store.release_run_reservation(&run_id).expect("cancel");
        assert!(store
            .workspace_reservation("ws-reserve")
            .expect("lookup")
            .is_none());

        let (store, run_id) = park_fixture("reserve-expire", 1, Some(1));
        store
            .reserve_workspace("ws-reserve-expire", &run_id, 1)
            .expect("reserve");
        assert!(
            store
                .active_workspace_reservation("ws-reserve-expire", unix_timestamp())
                .expect("lookup")
                .is_none(),
            "an elapsed reservation does not block CreateRun"
        );
        let _ = store.expire_stale_approvals().expect("expire");
        assert!(
            store
                .workspace_reservation("ws-reserve-expire")
                .expect("lookup")
                .is_none(),
            "expiry releases the workspace reservation"
        );
    }

    /// A gate and a question decided together are one resume: it starts from the answer (the call
    /// waiting for its result), whichever decision owns it, and is told the gate's outcome. A
    /// second claim of the same park is told nothing more.
    #[tokio::test]
    async fn the_one_resume_is_told_a_gate_and_a_question_decided_together() {
        for gate_owns in [true, false] {
            let name = if gate_owns {
                "together-gate"
            } else {
                "together-question"
            };
            let (store, run_id) = fresh(name);
            let store = std::sync::Arc::new(store);
            store
                .init_playbook_run(
                    &run_id,
                    &pin(),
                    &spec(vec![
                        branch_gate("approve", &[]),
                        task_step("after", &["approve:approved"], &[], vec![]),
                    ]),
                )
                .expect("init");
            let question_id = format!("{run_id}/question");
            {
                let connection = store.connection.lock().expect("connection");
                connection
                    .execute(
                        "INSERT INTO approvals (
                            request_id, run_id, step_id, attempt_id, checkpoint_digest,
                            continuation_generation, published, kind, tool_name, approval_message,
                            requested_args_json, session_id, owner_user_id, approver_user_id,
                            created_at
                         ) VALUES (?1, ?2, 'step-1', 'attempt-1', 'digest', 1, 1, 'question',
                            'ask_user', 'Which title?', ?3, 'sess', 'local', 'local', 100)",
                        params![
                            question_id,
                            run_id,
                            r#"{"jig":"vam.question","data":{"question":"Which title?"}}"#
                        ],
                    )
                    .expect("question row");
            }
            let gate = decide_gate(
                &store,
                &run_id,
                "approve",
                "SUBMITTED",
                Some(r#"{"option":"approved"}"#),
            )
            .expect("gate decided");
            let question = store
                .decide_approval(
                    &ResourceId {
                        urn: question_id.clone(),
                    },
                    &ResourceId {
                        urn: "urn:vam:token:local.vam.dev/acme/question".to_string(),
                    },
                    "SUBMITTED",
                    "User::\"local\"",
                    crate::users::LOCAL_USER_ID,
                    None,
                    None,
                    None,
                    Some(r#"{"answer":"Q4 plan"}"#),
                )
                .expect("question answered");

            let owner = if gate_owns { &gate } else { &question };
            let stored = store.get_approval(&owner.request_id).expect("owner row");
            let own_inputs =
                crate::api::resume_inputs_for(&store, &stored, owner).expect("own inputs");
            let inputs = crate::api::resume_decisions::compose_resume_inputs(
                &store,
                owner,
                own_inputs.clone(),
            )
            .await;

            let text = |key: &str| match inputs.fields.get(key).and_then(|v| v.kind.as_ref()) {
                Some(prost_types::value::Kind::StringValue(s)) => s.clone(),
                _ => String::new(),
            };
            assert_eq!(text("approved_tool"), "ask_user", "{name}");
            let answer = match inputs
                .fields
                .get("hitl_answer")
                .and_then(|v| v.kind.as_ref())
            {
                Some(prost_types::value::Kind::StructValue(s)) => {
                    vam_spec::harness::struct_to_json(s).to_string()
                }
                _ => String::new(),
            };
            assert!(
                answer.contains("Q4 plan"),
                "{name}: the answer is the task's input: {answer}"
            );
            let note = text("hitl_decisions_note");
            assert!(
                note.contains("Gate 'approve' was decided with outcome 'approved'"),
                "{name}: the task is told the gate: {note}"
            );
            assert!(note.contains("[Playbook status:"), "{name}: {note}");
            let events = store.read_outbox_for_run(&run_id, 0).expect("outbox");
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e.event_type == crate::events::TYPE_RESUME_DECISIONS)
                    .count(),
                1,
                "{name}: one event says which decisions the resume carried"
            );

            // A second claim of the same park has nothing left to tell.
            let again = crate::api::resume_decisions::compose_resume_inputs(
                &store,
                owner,
                own_inputs.clone(),
            )
            .await;
            assert_eq!(again, own_inputs, "{name}: nothing is told twice");
        }
    }
}
