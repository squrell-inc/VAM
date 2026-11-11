//! Bounded guest-side task execution.
//!
//! The runtime validates a host lease and delegates the work to a local task
//! handler. It does not select workflow nodes, retry attempts, or commit
//! authoritative workflow state.

// The rootfs guest is always a static musl build; a process guest runs
// agent code with no sandbox and must never end up in an image.
#[cfg(all(feature = "process-guest", target_env = "musl"))]
compile_error!(
    "the process-guest feature is test-only and must not be built for musl (the rootfs target)"
);

pub mod agent;
pub mod compaction;
pub mod context_window;
pub mod drive9;
pub mod egress;
pub mod exec;
pub mod freeze;
pub mod hitl_checkpoint;
pub mod inbox;
pub mod inference;
pub mod instruction;
pub mod landlock;
pub mod loop_detect;
pub mod outbox;
pub mod pep;
pub mod primitives;
pub mod resample;
pub mod skill_index;
pub mod skills;
pub mod telemetry;
pub mod tool_examples;

pub use agent::{run_react_agent_loop, run_react_agent_loop_with_trace};
pub use drive9::{Drive9Client, Drive9GuestError};
#[cfg(not(feature = "process-guest"))]
pub use egress::start_guest_proxy;
pub use egress::{
    advertise_proxy_url, proxy_url, start_guest_proxy_with, BoundEgress, EgressClient,
    EgressEndpoint,
};
pub use exec::{
    cancel_active_lease, cancel_all_active_leases, execute_tool_subprocess,
    execute_tool_subprocess_with_token, execute_tool_subprocess_with_trace, register_active_lease,
    unregister_active_lease,
};
pub use inference::InferenceClient;
pub use landlock::{apply_landlock_sandbox, LandlockPolicy};
pub use pep::request_tool_authorization;
pub use telemetry::{
    flush_batch_to_stream, frame_from_log, global_telemetry, start_host_flush_loop,
    GuestTelemetryBuffer,
};

use std::collections::HashSet;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};
use thiserror::Error;
use vam_spec::v1::{ExecuteTaskRequest, ExecutionLease, TaskOutcome};
use vam_spec::validation::Validate;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("task request does not contain an execution lease")]
    MissingLease,
    #[error("invalid execution lease: {0}")]
    InvalidLease(String),
    #[error("lease belongs to lifecycle generation {actual}, expected {expected}")]
    StaleGeneration { expected: i64, actual: i64 },
    #[error("lease has expired")]
    LeaseExpired,
    #[error("lease is already active or has already completed")]
    DuplicateLease,
    #[error("task request has no executable action")]
    MissingAction,
    #[error("task handler returned an outcome for a different lease")]
    OutcomeLeaseMismatch,
    #[error("runtime lease registry is unavailable")]
    LeaseRegistryUnavailable,
    #[error("serialization error: {0}")]
    Serialization(String),
    #[error("task handler failed: {0}")]
    Handler(String),
}

/// A local task implementation. Implementations must run subprocesses
/// unprivileged and honor the host-provided timeout.
pub trait TaskHandler: Send + Sync {
    fn execute(&self, request: &ExecuteTaskRequest) -> Result<TaskOutcome, RuntimeError>;
}

pub struct LeasedTaskExecutor {
    lifecycle_generation: AtomicI64,
    completed_or_active: Mutex<HashSet<String>>,
}

impl LeasedTaskExecutor {
    pub fn new(lifecycle_generation: i64) -> Self {
        Self {
            lifecycle_generation: AtomicI64::new(lifecycle_generation),
            completed_or_active: Mutex::new(HashSet::new()),
        }
    }

    /// Raises minimum accepted lease generation and clears replay state (snapshot restore fence).
    pub fn fence_min_generation(&self, min_generation: i64) {
        loop {
            let current = self.lifecycle_generation.load(Ordering::SeqCst);
            if min_generation <= current {
                break;
            }
            if self
                .lifecycle_generation
                .compare_exchange(current, min_generation, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                break;
            }
        }
        if let Ok(mut leases) = self.completed_or_active.lock() {
            leases.clear();
        }
    }

    pub fn execute(
        &self,
        request: &ExecuteTaskRequest,
        handler: &dyn TaskHandler,
    ) -> Result<TaskOutcome, RuntimeError> {
        let lease = request.lease.as_ref().ok_or(RuntimeError::MissingLease)?;
        lease
            .validate()
            .map_err(|error| RuntimeError::InvalidLease(error.to_string()))?;
        let expected = self.lifecycle_generation.load(Ordering::SeqCst);
        if lease.lifecycle_generation < expected {
            return Err(RuntimeError::StaleGeneration {
                expected,
                actual: lease.lifecycle_generation,
            });
        }
        if lease.lifecycle_generation > expected {
            self.lifecycle_generation
                .store(lease.lifecycle_generation, Ordering::SeqCst);
        }
        if is_expired(lease) {
            return Err(RuntimeError::LeaseExpired);
        }
        if request.task_action.is_none() {
            return Err(RuntimeError::MissingAction);
        }

        let lease_key = lease_key(lease)?;
        {
            let mut leases = self
                .completed_or_active
                .lock()
                .map_err(|_| RuntimeError::LeaseRegistryUnavailable)?;
            if !leases.insert(lease_key) {
                return Err(RuntimeError::DuplicateLease);
            }
        }

        let outcome = handler.execute(request)?;
        if outcome.lease.as_ref() != Some(lease) {
            return Err(RuntimeError::OutcomeLeaseMismatch);
        }
        outcome
            .validate()
            .map_err(|error| RuntimeError::Handler(error.to_string()))?;
        Ok(outcome)
    }
}

fn is_expired(lease: &ExecutionLease) -> bool {
    let Some(expires_at) = &lease.expires_at else {
        return true;
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(i64::MAX);
    expires_at.seconds < now
}

fn lease_key(lease: &ExecutionLease) -> Result<String, RuntimeError> {
    let lease_id = lease
        .lease_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .filter(|urn| !urn.is_empty())
        .ok_or_else(|| RuntimeError::InvalidLease("lease_id is required".to_string()))?;
    Ok(format!("{}:{}", lease.lifecycle_generation, lease_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost_types::Timestamp;
    use vam_spec::v1::{
        execute_task_request, ExecuteTaskRequest, ResourceId, TaskOutcomeState, ToolAction,
    };

    struct SuccessfulHandler;

    impl TaskHandler for SuccessfulHandler {
        fn execute(&self, request: &ExecuteTaskRequest) -> Result<TaskOutcome, RuntimeError> {
            Ok(TaskOutcome {
                lease: request.lease.clone(),
                state: TaskOutcomeState::Succeeded as i32,
                output: None,
                output_artifacts: Vec::new(),
                continuation: None,
                evidence: None,
                observed_usage: None,
                error: None,
            })
        }
    }

    fn resource(kind: &str, name: &str) -> ResourceId {
        ResourceId {
            urn: format!("urn:vam:{kind}:local.vam.dev/acme/{name}"),
        }
    }

    fn request() -> ExecuteTaskRequest {
        ExecuteTaskRequest {
            lease: Some(ExecutionLease {
                run_id: Some(resource("run", "run-1")),
                step_id: "step-1".to_string(),
                attempt_id: Some(resource("attempt", "attempt-1")),
                lease_id: Some(resource("lease", "lease-1")),
                lifecycle_generation: 7,
                fencing_token: vec![1],
                expires_at: Some(Timestamp {
                    seconds: 4_102_444_800,
                    nanos: 0,
                }),
                idempotency_key: "idempotency-1".to_string(),
            }),
            task_action: Some(execute_task_request::TaskAction::ToolAction(ToolAction {
                tool_name: "read_file".to_string(),
                arguments: None,
            })),
            inputs: None,
            timeout: None,
            agent_ref: None,
            trace_context: None,
        }
    }

    #[test]
    fn executor_rejects_duplicate_leases() {
        let executor = LeasedTaskExecutor::new(7);
        let request = request();
        let handler = SuccessfulHandler;
        executor
            .execute(&request, &handler)
            .expect("first leased task should execute");
        assert!(matches!(
            executor.execute(&request, &handler),
            Err(RuntimeError::DuplicateLease)
        ));
    }

    #[test]
    fn executor_accepts_forward_lifecycle_generation_bump() {
        let executor = LeasedTaskExecutor::new(1);
        let handler = SuccessfulHandler;
        let mut first = request();
        first.lease.as_mut().unwrap().lifecycle_generation = 1;
        executor
            .execute(&first, &handler)
            .expect("generation 1 should execute");

        let mut resumed = request();
        resumed.lease.as_mut().unwrap().lifecycle_generation = 2;
        resumed
            .lease
            .as_mut()
            .unwrap()
            .lease_id
            .as_mut()
            .unwrap()
            .urn = "urn:vam:lease:local.vam.dev/acme/lease-resume".to_string();
        executor
            .execute(&resumed, &handler)
            .expect("HITL resume with bumped generation should execute");

        let mut stale = request();
        stale.lease.as_mut().unwrap().lifecycle_generation = 1;
        stale.lease.as_mut().unwrap().lease_id.as_mut().unwrap().urn =
            "urn:vam:lease:local.vam.dev/acme/lease-stale".to_string();
        assert!(matches!(
            executor.execute(&stale, &handler),
            Err(RuntimeError::StaleGeneration { .. })
        ));
    }
}
