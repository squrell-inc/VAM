//! Host-side HSP lifecycle hook dispatcher (OnRequest / OnComplete / OnResponse / OnPaused).

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use vam_spec::v1::{
    ContextBlock, HookDisposition, HookPhase, Label, OnCompleteRequest, OnPausedRequest,
    OnRequestRequest, OnResponseRequest, OutputBlock, ResourceId, RunSpec, RunStatus,
};

use crate::error::OrchestratorError;
use crate::hsp::{HspClient, HspEndpoint};
use crate::hsp_registry::HspRegistry;
use crate::workflow::SqliteWorkflowStore;

pub const EVENT_HOOK_COMPLETED: &str = "vam.v1.hsp.hook.completed";
pub const EVENT_HOOK_DEGRADED: &str = "vam.v1.hsp.hook.degraded";
pub const EVENT_HOOK_FAILED: &str = "vam.v1.hsp.hook.failed";

/// Per-phase per-broker RPC budgets (overridable in tests).
#[derive(Debug, Clone)]
pub struct HookTimeouts {
    pub on_request: Duration,
    pub on_complete: Duration,
    pub on_response: Duration,
    pub on_paused: Duration,
}

impl Default for HookTimeouts {
    fn default() -> Self {
        Self {
            on_request: Duration::from_secs(30),
            on_complete: Duration::from_secs(60),
            on_response: Duration::from_secs(15),
            on_paused: Duration::from_secs(15),
        }
    }
}

/// Aggregated OnRequest dispatch outcome.
#[derive(Debug, Clone, Default)]
pub struct OnRequestDispatchResult {
    pub injected_context: Vec<ContextBlock>,
    pub aborted: bool,
    pub abort_reason: Option<String>,
}

/// Aggregated OnComplete dispatch outcome.
#[derive(Debug, Clone, Default)]
pub struct OnCompleteDispatchResult {
    pub retention_labels: Vec<Label>,
    pub aborted: bool,
    pub abort_reason: Option<String>,
}

/// Aggregated OnResponse dispatch outcome.
#[derive(Debug, Clone, Default)]
pub struct OnResponseDispatchResult {
    pub response_blocks: Vec<OutputBlock>,
    pub aborted: bool,
    pub abort_reason: Option<String>,
}

/// Aggregated OnPaused dispatch outcome. Notification only: callers keep the
/// run paused and the approval listed even when `aborted` is set.
#[derive(Debug, Clone, Default)]
pub struct OnPausedDispatchResult {
    pub aborted: bool,
    pub abort_reason: Option<String>,
}

#[derive(Clone)]
#[allow(clippy::enum_variant_names)] // variants mirror the On* hook RPC names
enum HookRequest {
    OnRequest(OnRequestRequest),
    OnComplete(OnCompleteRequest),
    OnResponse(OnResponseRequest),
    OnPaused(OnPausedRequest),
}

fn phase_name(phase: HookPhase) -> &'static str {
    match phase {
        HookPhase::OnRequest => "on_request",
        HookPhase::OnComplete => "on_complete",
        HookPhase::OnResponse => "on_response",
        HookPhase::OnPaused => "on_paused",
        HookPhase::Unspecified => "unspecified",
    }
}

#[derive(Clone)]
struct Participant {
    broker_id: String,
    endpoint: HspEndpoint,
    required: bool,
    depends_on: Vec<String>,
    known_brokers: HashSet<String>,
}

/// Dispatches HSP lifecycle hooks across approved brokers ordered by `depends_on`.
pub struct HspHookDispatcher<'a> {
    store: &'a SqliteWorkflowStore,
    timeouts: HookTimeouts,
}

impl<'a> HspHookDispatcher<'a> {
    pub fn new(store: &'a SqliteWorkflowStore) -> Self {
        Self {
            store,
            timeouts: HookTimeouts::default(),
        }
    }

    pub fn with_timeouts(mut self, timeouts: HookTimeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    pub async fn dispatch_on_request(
        &self,
        run_id: &str,
        session_id: &str,
        run_spec: Option<RunSpec>,
    ) -> Result<OnRequestDispatchResult, OrchestratorError> {
        let caller = crate::caller_context::stamp_caller_context(
            self.store,
            session_id,
            run_id,
            None,
            run_spec.as_ref(),
        )?;
        let request = HookRequest::OnRequest(OnRequestRequest {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            run_spec,
            attachments: vec![],
            knowledge_refs: vec![],
            trace_context: Some(vam_telemetry::create_trace_context(Some(run_id), None)),
            caller_context: Some(caller.clone()),
        });
        let outcome = self
            .dispatch_phase(HookPhase::OnRequest, run_id, request, Some(caller))
            .await?;
        let mut result = OnRequestDispatchResult {
            aborted: outcome.aborted,
            abort_reason: outcome.abort_reason,
            ..Default::default()
        };
        for item in outcome.collected {
            if let CollectedPayload::InjectedContext(blocks) = item {
                result.injected_context.extend(blocks);
            }
        }
        Ok(result)
    }

    pub async fn dispatch_on_complete(
        &self,
        run_id: &str,
        session_id: &str,
        status: RunStatus,
    ) -> Result<OnCompleteDispatchResult, OrchestratorError> {
        let caller = crate::caller_context::stamp_caller_context(
            self.store, session_id, run_id, None, None,
        )?;
        let request = HookRequest::OnComplete(OnCompleteRequest {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            status: Some(status),
            output_artifacts: vec![],
            trace_context: Some(vam_telemetry::create_trace_context(Some(run_id), None)),
            caller_context: Some(caller.clone()),
        });
        let outcome = self
            .dispatch_phase(HookPhase::OnComplete, run_id, request, Some(caller))
            .await?;
        let mut result = OnCompleteDispatchResult {
            aborted: outcome.aborted,
            abort_reason: outcome.abort_reason,
            ..Default::default()
        };
        for item in outcome.collected {
            if let CollectedPayload::RetentionLabels(labels) = item {
                result.retention_labels.extend(labels);
            }
        }
        Ok(result)
    }

    pub async fn dispatch_on_response(
        &self,
        run_id: &str,
        session_id: &str,
        status: RunStatus,
        agent_output: Vec<OutputBlock>,
    ) -> Result<OnResponseDispatchResult, OrchestratorError> {
        let caller = crate::caller_context::stamp_caller_context(
            self.store, session_id, run_id, None, None,
        )?;
        let request = HookRequest::OnResponse(OnResponseRequest {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            status: Some(status),
            agent_output,
            trace_context: Some(vam_telemetry::create_trace_context(Some(run_id), None)),
            caller_context: Some(caller.clone()),
        });
        let outcome = self
            .dispatch_phase(HookPhase::OnResponse, run_id, request, Some(caller))
            .await?;
        let mut result = OnResponseDispatchResult {
            aborted: outcome.aborted,
            abort_reason: outcome.abort_reason,
            ..Default::default()
        };
        for item in outcome.collected {
            if let CollectedPayload::ResponseBlocks(blocks) = item {
                result.response_blocks.extend(blocks);
            }
        }
        Ok(result)
    }

    pub async fn dispatch_on_paused(
        &self,
        run_id: &str,
        session_id: &str,
        approval_request_id: &str,
        tool_name: &str,
        reason: &str,
    ) -> Result<OnPausedDispatchResult, OrchestratorError> {
        let caller = crate::caller_context::stamp_caller_context(
            self.store, session_id, run_id, None, None,
        )?;
        let request = HookRequest::OnPaused(OnPausedRequest {
            session_id: session_id.to_string(),
            run_id: run_id.to_string(),
            approval_request_id: if approval_request_id.is_empty() {
                None
            } else {
                Some(ResourceId {
                    urn: approval_request_id.to_string(),
                })
            },
            tool_name: tool_name.to_string(),
            reason: reason.to_string(),
            trace_context: Some(vam_telemetry::create_trace_context(Some(run_id), None)),
            caller_context: Some(caller.clone()),
        });
        let outcome = self
            .dispatch_phase(HookPhase::OnPaused, run_id, request, Some(caller))
            .await?;
        Ok(OnPausedDispatchResult {
            aborted: outcome.aborted,
            abort_reason: outcome.abort_reason,
        })
    }

    async fn dispatch_phase(
        &self,
        phase: HookPhase,
        run_id: &str,
        request: HookRequest,
        caller: Option<vam_spec::v1::CallerContext>,
    ) -> Result<PhaseOutcome, OrchestratorError> {
        let participants = self.collect_participants(phase)?;
        if participants.is_empty() {
            return Ok(PhaseOutcome::default());
        }

        let waves = match build_waves(&participants) {
            Ok(waves) => waves,
            Err(errors) => {
                let mut drop_ids = HashSet::new();
                for err in &errors {
                    let phase_str = phase_name(phase);
                    let _ = vam_telemetry::VamMetricsRegistry::global().record_counter(
                        vam_telemetry::METRIC_HSP_HOOK_FAILURES_TOTAL,
                        1,
                        &[("hook_type", phase_str), ("broker_id", &err.broker_id)],
                    );
                    emit_hook_event(self.store, run_id, &err.broker_id, phase, err.disposition)?;
                    if err.aborts {
                        return Ok(PhaseOutcome {
                            aborted: true,
                            abort_reason: Some(err.reason.clone()),
                            collected: vec![],
                        });
                    }
                    drop_ids.insert(err.broker_id.clone());
                }
                let remaining: Vec<Participant> = participants
                    .into_iter()
                    .filter(|p| !drop_ids.contains(&p.broker_id))
                    .collect();
                if remaining.is_empty() {
                    return Ok(PhaseOutcome::default());
                }
                build_waves(&remaining).map_err(|more| {
                    OrchestratorError::Hsp(format!(
                        "hook depends_on unresolved after degrade: {}",
                        more.into_iter()
                            .map(|e| e.reason)
                            .collect::<Vec<_>>()
                            .join("; ")
                    ))
                })?
            }
        };

        let budget = self.budget_for(phase);
        let mut outcome = PhaseOutcome::default();

        let phase_str = phase_name(phase);
        let metrics = vam_telemetry::VamMetricsRegistry::global();
        let registry = HspRegistry::new(self.store);
        for wave in waves {
            let wave_start = std::time::Instant::now();
            // Parallel within a wave; await all before the next wave.
            let mut join_set = tokio::task::JoinSet::new();
            for participant in wave {
                let admit_result =
                    registry.admit_hook(&participant.broker_id, phase, caller.as_ref());
                let store_endpoint = participant.endpoint.clone();
                let broker_id = participant.broker_id.clone();
                let required = participant.required;
                let req = request.clone();
                join_set.spawn(async move {
                    run_broker_hook(
                        admit_result,
                        store_endpoint,
                        broker_id,
                        required,
                        phase,
                        budget,
                        req,
                    )
                    .await
                });
            }

            while let Some(joined) = join_set.join_next().await {
                let broker_outcome = joined
                    .map_err(|err| OrchestratorError::Hsp(format!("hook task join: {err}")))?;
                emit_hook_event(
                    self.store,
                    run_id,
                    &broker_outcome.broker_id,
                    phase,
                    broker_outcome.disposition,
                )?;
                if broker_outcome.disposition != AuditDisposition::Ok || broker_outcome.aborts {
                    let _ = metrics.record_counter(
                        vam_telemetry::METRIC_HSP_HOOK_FAILURES_TOTAL,
                        1,
                        &[
                            ("hook_type", phase_str),
                            ("broker_id", &broker_outcome.broker_id),
                        ],
                    );
                }
                if let Some(payload) = broker_outcome.collected {
                    outcome.collected.push(payload);
                }
                if broker_outcome.aborts {
                    outcome.aborted = true;
                    outcome.abort_reason = Some(broker_outcome.reason.unwrap_or_else(|| {
                        format!(
                            "required hook failed for broker {}",
                            broker_outcome.broker_id
                        )
                    }));
                    // Cancel remaining tasks in this wave.
                    join_set.abort_all();
                    while join_set.join_next().await.is_some() {}
                    let wave_duration_ms = wave_start.elapsed().as_secs_f64() * 1000.0;
                    let _ = metrics.record_histogram(
                        vam_telemetry::METRIC_HSP_HOOK_WAVE_DURATION_MS,
                        wave_duration_ms,
                        &[("hook_type", phase_str)],
                    );
                    return Ok(outcome);
                }
            }

            let wave_duration_ms = wave_start.elapsed().as_secs_f64() * 1000.0;
            let _ = metrics.record_histogram(
                vam_telemetry::METRIC_HSP_HOOK_WAVE_DURATION_MS,
                wave_duration_ms,
                &[("hook_type", phase_str)],
            );
        }

        Ok(outcome)
    }

    fn collect_participants(
        &self,
        phase: HookPhase,
    ) -> Result<Vec<Participant>, OrchestratorError> {
        let phase_i32 = phase as i32;
        let enabled = self.store.list_enabled_hsp_broker_ids()?;
        let known: HashSet<String> = enabled.iter().cloned().collect();
        let mut participants = Vec::new();

        for broker_id in enabled {
            let Some(approval) = self.store.get_hsp_broker_approval(&broker_id)? else {
                continue;
            };
            if !approval.enabled {
                continue;
            }
            let Some(endpoint) = self.store.get_hsp_broker_endpoint(&broker_id)? else {
                continue;
            };
            let Some(hook) = approval
                .manifest
                .hooks
                .iter()
                .find(|h| h.phase == phase_i32)
            else {
                continue;
            };
            participants.push(Participant {
                broker_id,
                endpoint,
                required: hook.required,
                depends_on: approval.manifest.depends_on.clone(),
                known_brokers: known.clone(),
            });
        }
        Ok(participants)
    }

    fn budget_for(&self, phase: HookPhase) -> Duration {
        match phase {
            HookPhase::OnRequest => self.timeouts.on_request,
            HookPhase::OnComplete => self.timeouts.on_complete,
            HookPhase::OnResponse => self.timeouts.on_response,
            HookPhase::OnPaused => self.timeouts.on_paused,
            HookPhase::Unspecified => self.timeouts.on_request,
        }
    }
}

/// Dispatch `on_paused` after the run is `PAUSED` and `vam.v1.hitl.paused`
/// has been written. The latest pending approval for the run supplies the
/// tool name and request id. Hook failure is returned to the caller; it does
/// not remove the approval.
pub async fn notify_run_paused(
    store: &SqliteWorkflowStore,
    run_id: &str,
    session_id: &str,
) -> Result<OnPausedDispatchResult, OrchestratorError> {
    let pending = store.list_published_approvals()?;
    let approval = pending.iter().rev().find(|row| row.run_id == run_id);
    let request_id = approval.map(|row| row.request_id.as_str()).unwrap_or("");
    let tool_name = approval.map(|row| row.tool_name.as_str()).unwrap_or("");
    let reason = approval
        .map(|row| row.approval_message.as_str())
        .filter(|message| !message.is_empty())
        .unwrap_or("run paused for human approval");
    HspHookDispatcher::new(store)
        .dispatch_on_paused(run_id, session_id, request_id, tool_name, reason)
        .await
}

#[derive(Debug)]
struct DagParticipantError {
    broker_id: String,
    disposition: AuditDisposition,
    aborts: bool,
    reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AuditDisposition {
    Ok,
    Degraded,
    Failed,
}

impl AuditDisposition {
    fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::Degraded => "degraded",
            Self::Failed => "failed",
        }
    }

    fn event_type(self) -> &'static str {
        match self {
            Self::Ok => EVENT_HOOK_COMPLETED,
            Self::Degraded => EVENT_HOOK_DEGRADED,
            Self::Failed => EVENT_HOOK_FAILED,
        }
    }
}

fn build_waves(
    participants: &[Participant],
) -> Result<Vec<Vec<Participant>>, Vec<DagParticipantError>> {
    let ids: HashSet<&str> = participants.iter().map(|p| p.broker_id.as_str()).collect();
    let mut errors = Vec::new();

    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    let mut indegree: HashMap<String, usize> = HashMap::new();
    for p in participants {
        indegree.entry(p.broker_id.clone()).or_insert(0);
        for dep in &p.depends_on {
            if !p.known_brokers.contains(dep) {
                errors.push(DagParticipantError {
                    broker_id: p.broker_id.clone(),
                    disposition: if p.required {
                        AuditDisposition::Failed
                    } else {
                        AuditDisposition::Degraded
                    },
                    aborts: p.required,
                    reason: format!("unknown depends_on broker '{dep}'"),
                });
                continue;
            }
            if !ids.contains(dep.as_str()) {
                continue;
            }
            edges
                .entry(dep.clone())
                .or_default()
                .push(p.broker_id.clone());
            *indegree.entry(p.broker_id.clone()).or_insert(0) += 1;
        }
    }

    if !errors.is_empty() {
        return Err(errors);
    }

    let by_id: HashMap<&str, &Participant> = participants
        .iter()
        .map(|p| (p.broker_id.as_str(), p))
        .collect();

    let mut ready: Vec<String> = indegree
        .iter()
        .filter(|(_, deg)| **deg == 0)
        .map(|(id, _)| id.clone())
        .collect();
    ready.sort();

    let mut waves = Vec::new();
    let mut visited = 0usize;
    let mut queue: VecDeque<String> = ready.into();

    while !queue.is_empty() {
        let mut wave_ids: Vec<String> = queue.drain(..).collect();
        wave_ids.sort();
        let mut next_ready = Vec::new();
        let mut wave = Vec::with_capacity(wave_ids.len());
        for id in &wave_ids {
            visited += 1;
            if let Some(p) = by_id.get(id.as_str()) {
                wave.push((*p).clone());
            }
            if let Some(dependents) = edges.get(id) {
                for dep in dependents {
                    if let Some(deg) = indegree.get_mut(dep) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 {
                            next_ready.push(dep.clone());
                        }
                    }
                }
            }
        }
        waves.push(wave);
        next_ready.sort();
        queue.extend(next_ready);
    }

    if visited != participants.len() {
        let mut cycle_errors = Vec::new();
        for p in participants {
            if indegree.get(&p.broker_id).copied().unwrap_or(0) > 0 {
                cycle_errors.push(DagParticipantError {
                    broker_id: p.broker_id.clone(),
                    disposition: if p.required {
                        AuditDisposition::Failed
                    } else {
                        AuditDisposition::Degraded
                    },
                    aborts: p.required,
                    reason: "depends_on cycle detected".into(),
                });
            }
        }
        return Err(cycle_errors);
    }

    Ok(waves)
}

#[derive(Default)]
struct PhaseOutcome {
    aborted: bool,
    abort_reason: Option<String>,
    collected: Vec<CollectedPayload>,
}

enum CollectedPayload {
    InjectedContext(Vec<ContextBlock>),
    RetentionLabels(Vec<Label>),
    ResponseBlocks(Vec<OutputBlock>),
    Paused,
}

struct BrokerHookOutcome {
    broker_id: String,
    disposition: AuditDisposition,
    aborts: bool,
    reason: Option<String>,
    collected: Option<CollectedPayload>,
}

async fn run_broker_hook(
    admit_result: Result<(), OrchestratorError>,
    endpoint: HspEndpoint,
    broker_id: String,
    required: bool,
    _phase: HookPhase,
    budget: Duration,
    request: HookRequest,
) -> BrokerHookOutcome {
    if let Err(err) = admit_result {
        return BrokerHookOutcome {
            broker_id,
            disposition: AuditDisposition::Failed,
            aborts: true,
            reason: Some(err.to_string()),
            collected: None,
        };
    }

    let mut client = HspClient::new(endpoint).with_timeout(budget);
    let rpc_result = match request {
        HookRequest::OnRequest(req) => client.on_request(req).await.map(|resp| RpcMapped {
            error: resp.error.is_some(),
            disposition: resp.disposition,
            collected: CollectedPayload::InjectedContext(resp.injected_context),
        }),
        HookRequest::OnComplete(req) => client.on_complete(req).await.map(|resp| RpcMapped {
            error: resp.error.is_some(),
            disposition: resp.disposition,
            collected: CollectedPayload::RetentionLabels(resp.retention_labels),
        }),
        HookRequest::OnResponse(req) => client.on_response(req).await.map(|resp| RpcMapped {
            error: resp.error.is_some(),
            disposition: resp.disposition,
            collected: CollectedPayload::ResponseBlocks(resp.response_blocks),
        }),
        HookRequest::OnPaused(req) => client.on_paused(req).await.map(|resp| RpcMapped {
            error: resp.error.is_some(),
            disposition: resp.disposition,
            collected: CollectedPayload::Paused,
        }),
    };

    match rpc_result {
        Ok(mapped) => map_rpc_mapped(&broker_id, required, mapped),
        Err(err) => map_transport_error(&broker_id, required, err),
    }
}

struct RpcMapped {
    error: bool,
    disposition: i32,
    collected: CollectedPayload,
}

fn map_rpc_mapped(broker_id: &str, _required: bool, mapped: RpcMapped) -> BrokerHookOutcome {
    if mapped.error {
        return BrokerHookOutcome {
            broker_id: broker_id.to_string(),
            disposition: AuditDisposition::Failed,
            aborts: true,
            reason: Some(format!("hook error from broker {broker_id}")),
            collected: None,
        };
    }

    let disposition =
        HookDisposition::try_from(mapped.disposition).unwrap_or(HookDisposition::Unspecified);
    match disposition {
        HookDisposition::Ok | HookDisposition::Unspecified => BrokerHookOutcome {
            broker_id: broker_id.to_string(),
            disposition: AuditDisposition::Ok,
            aborts: false,
            reason: None,
            collected: Some(mapped.collected),
        },
        HookDisposition::Degraded => BrokerHookOutcome {
            broker_id: broker_id.to_string(),
            disposition: AuditDisposition::Degraded,
            aborts: false,
            reason: None,
            collected: Some(mapped.collected),
        },
        HookDisposition::Fail => BrokerHookOutcome {
            broker_id: broker_id.to_string(),
            disposition: AuditDisposition::Failed,
            aborts: true,
            reason: Some(format!("hook FAIL disposition from broker {broker_id}")),
            collected: None,
        },
    }
}

fn map_transport_error(
    broker_id: &str,
    required: bool,
    err: OrchestratorError,
) -> BrokerHookOutcome {
    let unreachable = is_unreachable_or_timeout(&err);
    if required {
        BrokerHookOutcome {
            broker_id: broker_id.to_string(),
            disposition: AuditDisposition::Failed,
            aborts: true,
            reason: Some(err.to_string()),
            collected: None,
        }
    } else if unreachable {
        BrokerHookOutcome {
            broker_id: broker_id.to_string(),
            disposition: AuditDisposition::Degraded,
            aborts: false,
            reason: Some(err.to_string()),
            collected: None,
        }
    } else {
        // Optional non-timeout RPC error → DEGRADED and continue.
        BrokerHookOutcome {
            broker_id: broker_id.to_string(),
            disposition: AuditDisposition::Degraded,
            aborts: false,
            reason: Some(err.to_string()),
            collected: None,
        }
    }
}

fn is_unreachable_or_timeout(err: &OrchestratorError) -> bool {
    match err {
        OrchestratorError::Hsp(message) => {
            message.contains("timed out")
                || message.contains("uds connect failed")
                || message.contains("tcp connect failed")
                || message.contains("Unavailable")
                || message.contains("client is not connected")
        }
        _ => false,
    }
}

fn emit_hook_event(
    store: &SqliteWorkflowStore,
    run_id: &str,
    broker_id: &str,
    phase: HookPhase,
    disposition: AuditDisposition,
) -> Result<(), OrchestratorError> {
    let phase_str = phase_name(phase);
    let payload = serde_json::json!({
        "broker_id": broker_id,
        "phase": phase_str,
        "disposition": disposition.as_str(),
    })
    .to_string();
    store.append_outbox_event(run_id, disposition.event_type(), &payload)
}
