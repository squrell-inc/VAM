//! Host-side vsock broker accept loops for a running session.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use crate::daemon::DEFAULT_EGRESS_MAX_BYTES;
use crate::error::OrchestratorError;
use crate::guest_stream::GuestStreamReceiver;
use crate::manager::{resolve_session_identity, AgentIdentity};
use crate::timing::{mark_shared, SharedPhaseTimer};
use crate::workflow::SqliteWorkflowStore;
use prost::Message;
use tokio::io::{AsyncRead, AsyncWrite};
use vam_spec::v1::VsockEnvelope;
use vam_spec::validation::reserved_builtin_tool_names;
use vam_storage::WorkspaceManager;
use vam_telemetry::TelemetryCapture;
use vam_vsock::frame::{Frame, FrameType};

/// `INVALID_ARGUMENT`. A known port-52 method whose payload does not decode.
const PORT52_INVALID_ARGUMENT: i32 = 3;
/// `UNIMPLEMENTED`. The envelope decoded, but the method is not one port 52 serves.
const PORT52_UNIMPLEMENTED: i32 = 12;

/// Handle a guest `StreamLogs` envelope on Port 52 and write the ack frame.
/// Does not touch Cedar / workflow state (locked decision 8).
pub async fn handle_stream_logs_frame<S>(
    stream: &mut S,
    env: &VsockEnvelope,
    receiver: &Arc<Mutex<GuestStreamReceiver>>,
) -> bool
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let env_out = {
        let mut rx = match receiver.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match rx.handle_vsock_envelope(env) {
            Ok(Some(out)) => out,
            Ok(None) | Err(_) => return false,
        }
    };
    let correlation_id = env_out.correlation_id;
    let mut env_bytes = Vec::new();
    let _ = prost::Message::encode(&env_out, &mut env_bytes);
    let frame_out = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id,
        payload: bytes::Bytes::from(env_bytes),
    };
    let _ = frame_out.write_to(stream).await;
    true
}

fn port52_error_status(code: i32, message: impl Into<String>) -> vam_spec::v1::ErrorStatus {
    vam_spec::v1::ErrorStatus {
        code,
        message: message.into(),
        details: Vec::new(),
        retryable: false,
    }
}

fn port52_envelope(
    method: impl Into<String>,
    correlation_id: u32,
    payload: Vec<u8>,
    error: Option<vam_spec::v1::ErrorStatus>,
) -> VsockEnvelope {
    VsockEnvelope {
        method: method.into(),
        correlation_id,
        payload,
        error,
        trace_context: None,
    }
}

fn port52_typed_reply(
    request: &VsockEnvelope,
    response_method: &str,
    message: &impl Message,
) -> VsockEnvelope {
    let mut payload = Vec::new();
    let _ = message.encode(&mut payload);
    port52_envelope(response_method, request.correlation_id, payload, None)
}

fn port52_envelope_error(
    request: &VsockEnvelope,
    response_method: impl Into<String>,
    code: i32,
    message: impl Into<String>,
) -> VsockEnvelope {
    port52_envelope(
        response_method,
        request.correlation_id,
        Vec::new(),
        Some(port52_error_status(code, message)),
    )
}

fn malformed_method_message(method: &str) -> String {
    format!("malformed {method} request")
}

fn malformed_tool_intent_reply(request: &VsockEnvelope) -> VsockEnvelope {
    use vam_spec::v1::tool_intent_response::Disposition;
    use vam_spec::v1::ToolIntentResponse;
    let response = ToolIntentResponse {
        disposition: Disposition::Denied as i32,
        approval_request_id: None,
        error: Some(port52_error_status(
            PORT52_INVALID_ARGUMENT,
            malformed_method_message("RequestToolAuthorization"),
        )),
        capability_token: None,
        resource_envelope: None,
        replayed: None,
    };
    port52_typed_reply(request, "RequestToolAuthorizationResponse", &response)
}

fn malformed_execute_reply(request: &VsockEnvelope) -> VsockEnvelope {
    use vam_spec::v1::ExecuteHostToolResponse;
    let response = ExecuteHostToolResponse {
        text_result: String::new(),
        artifact_relpath: String::new(),
        artifact_bytes: Vec::new(),
        extra_artifacts: Vec::new(),
        error: Some(port52_error_status(
            PORT52_INVALID_ARGUMENT,
            malformed_method_message("ExecuteHostTool"),
        )),
    };
    port52_typed_reply(request, "ExecuteHostToolResponse", &response)
}

fn malformed_upload_reply(request: &VsockEnvelope) -> VsockEnvelope {
    use vam_spec::v1::UploadRunCheckpointResponse;
    let response = UploadRunCheckpointResponse {
        accepted: false,
        digest: String::new(),
        error: Some(port52_error_status(
            PORT52_INVALID_ARGUMENT,
            malformed_method_message("UploadRunCheckpoint"),
        )),
        received: 0,
        complete: false,
    };
    port52_typed_reply(request, "UploadRunCheckpointResponse", &response)
}

fn malformed_fetch_reply(request: &VsockEnvelope) -> VsockEnvelope {
    use vam_spec::v1::FetchRunCheckpointResponse;
    let response = FetchRunCheckpointResponse {
        chunk: Vec::new(),
        total_size: 0,
        last: false,
        error: Some(port52_error_status(
            PORT52_INVALID_ARGUMENT,
            malformed_method_message("FetchRunCheckpoint"),
        )),
    };
    port52_typed_reply(request, "FetchRunCheckpointResponse", &response)
}

fn malformed_commit_turn_reply(request: &VsockEnvelope) -> VsockEnvelope {
    use vam_spec::v1::CommitTurnResponse;
    let response = CommitTurnResponse {
        accepted: false,
        error: Some(port52_error_status(
            PORT52_INVALID_ARGUMENT,
            malformed_method_message("CommitTurn"),
        )),
        state: 0,
        manifest_hash: String::new(),
    };
    port52_typed_reply(request, "CommitTurnResponse", &response)
}

fn malformed_frame_reply(correlation_id: u32) -> VsockEnvelope {
    port52_envelope(
        "Error",
        correlation_id,
        Vec::new(),
        Some(port52_error_status(
            PORT52_INVALID_ARGUMENT,
            "malformed request",
        )),
    )
}

fn unknown_method_reply(request: &VsockEnvelope) -> VsockEnvelope {
    port52_envelope_error(
        request,
        format!("{}Response", request.method),
        PORT52_UNIMPLEMENTED,
        format!("unknown method {}", request.method),
    )
}

/// `Some` when this envelope must be answered with an error instead of dispatched.
///
/// A known method whose payload does not decode gets that method's response,
/// carrying `INVALID_ARGUMENT` and `malformed <method> request`. `ListAdvertisedTools`
/// and `StreamLogs` have no `ErrorStatus` field, so the status rides on the
/// envelope. Any other method gets a generic error envelope. `None` means the
/// payload decoded and the connection loop should run the method.
fn port52_rejection(env: &VsockEnvelope) -> Option<VsockEnvelope> {
    use vam_spec::v1::{
        CommitTurnRequest, ExecuteHostToolRequest, FetchRunCheckpointRequest,
        ListAdvertisedToolsRequest, LogFrame, ToolIntentRequest, UploadRunCheckpointRequest,
    };
    match env.method.as_str() {
        "CommitTurn" => {
            if CommitTurnRequest::decode(&*env.payload).is_err() {
                Some(malformed_commit_turn_reply(env))
            } else {
                None
            }
        }
        "RequestToolAuthorization" => {
            if ToolIntentRequest::decode(&*env.payload).is_err() {
                Some(malformed_tool_intent_reply(env))
            } else {
                None
            }
        }
        "ListAdvertisedTools" => {
            if ListAdvertisedToolsRequest::decode(&*env.payload).is_err() {
                Some(port52_envelope_error(
                    env,
                    "ListAdvertisedToolsResponse",
                    PORT52_INVALID_ARGUMENT,
                    malformed_method_message("ListAdvertisedTools"),
                ))
            } else {
                None
            }
        }
        "ExecuteHostTool" => {
            if ExecuteHostToolRequest::decode(&*env.payload).is_err() {
                Some(malformed_execute_reply(env))
            } else {
                None
            }
        }
        "UploadRunCheckpoint" => {
            if UploadRunCheckpointRequest::decode(&*env.payload).is_err() {
                Some(malformed_upload_reply(env))
            } else {
                None
            }
        }
        "FetchRunCheckpoint" => {
            if FetchRunCheckpointRequest::decode(&*env.payload).is_err() {
                Some(malformed_fetch_reply(env))
            } else {
                None
            }
        }
        "PullInbox" => {
            if vam_spec::v1::PullInboxRequest::decode(&*env.payload).is_err() {
                Some(port52_envelope_error(
                    env,
                    "PullInboxResponse",
                    PORT52_INVALID_ARGUMENT,
                    malformed_method_message("PullInbox"),
                ))
            } else {
                None
            }
        }
        "PostOutbox" => {
            if vam_spec::v1::PostOutboxRequest::decode(&*env.payload).is_err() {
                Some(port52_envelope_error(
                    env,
                    "PostOutboxResponse",
                    PORT52_INVALID_ARGUMENT,
                    malformed_method_message("PostOutbox"),
                ))
            } else {
                None
            }
        }
        "StreamLogs" => {
            if LogFrame::decode(&*env.payload).is_err() {
                Some(port52_envelope_error(
                    env,
                    "StreamLogsResponse",
                    PORT52_INVALID_ARGUMENT,
                    malformed_method_message("StreamLogs"),
                ))
            } else {
                None
            }
        }
        _ => Some(unknown_method_reply(env)),
    }
}

trait Port52Dispatch: Send {
    fn dispatch(
        &mut self,
        env: VsockEnvelope,
    ) -> impl std::future::Future<Output = VsockEnvelope> + Send;
}

/// Read port-52 frames until the peer closes the stream.
///
/// Every decoded request is answered. A frame that is not `ControlGrpc` is
/// skipped, and the connection stays open for the next request.
async fn serve_port52_connection<S, D>(
    mut stream: S,
    stream_logs: Option<Arc<Mutex<GuestStreamReceiver>>>,
    dispatch: &mut D,
) where
    S: AsyncRead + AsyncWrite + Unpin,
    D: Port52Dispatch,
{
    loop {
        let frame = match Frame::read_from(&mut stream).await {
            Ok(frame) => frame,
            Err(_) => break,
        };
        if frame.frame_type != FrameType::ControlGrpc {
            continue;
        }
        let env = match VsockEnvelope::decode(&*frame.payload) {
            Ok(env) => env,
            Err(_) => {
                if write_control_envelope(&mut stream, &malformed_frame_reply(frame.correlation_id))
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
        };
        if let Some(rejection) = port52_rejection(&env) {
            if write_control_envelope(&mut stream, &rejection)
                .await
                .is_err()
            {
                break;
            }
            continue;
        }
        if env.method == "StreamLogs" {
            if let Some(receiver) = stream_logs.as_ref() {
                let _ = handle_stream_logs_frame(&mut stream, &env, receiver).await;
            }
            continue;
        }
        let reply = dispatch.dispatch(env).await;
        if write_control_envelope(&mut stream, &reply).await.is_err() {
            break;
        }
    }
}

/// `RESOURCE_EXHAUSTED`. A reply that does not fit one vsock frame.
const PORT52_RESOURCE_EXHAUSTED: i32 = 8;

/// Write one reply frame. A reply whose encoding exceeds the frame limit is never sent (the
/// codec would refuse it and the caller would drop the connection): the guest gets an error
/// envelope for the same method and correlation id instead, and the stream stays usable.
async fn write_control_envelope<S>(stream: &mut S, envelope: &VsockEnvelope) -> std::io::Result<()>
where
    S: AsyncWrite + Unpin,
{
    let mut payload = Vec::new();
    envelope
        .encode(&mut payload)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    if payload.len() > vam_vsock::frame::MAX_PAYLOAD_SIZE {
        let too_big = port52_envelope(
            envelope.method.clone(),
            envelope.correlation_id,
            Vec::new(),
            Some(port52_error_status(
                PORT52_RESOURCE_EXHAUSTED,
                format!(
                    "result exceeds the frame: {} bytes, at most {} fit",
                    payload.len(),
                    vam_vsock::frame::MAX_PAYLOAD_SIZE
                ),
            )),
        );
        payload = Vec::new();
        too_big
            .encode(&mut payload)
            .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.to_string()))?;
    }
    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: envelope.correlation_id,
        payload: bytes::Bytes::from(payload),
    };
    frame.write_to(stream).await
}

struct Port52Session {
    sec_ctx: Arc<crate::security::SessionSecurityContext>,
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<crate::secrets::SecretsVault>,
    /// The call the PEP last authorized; the egress gate attaches a connection token only
    /// inside its declared shape (CN-2.3).
    call_window: Arc<vam_network::CallWindow>,
    skills_dir: std::path::PathBuf,
    registry: Arc<crate::host_tools::HostToolRegistry>,
    checkpoint_session: String,
    checkpoint_upload: crate::run_checkpoint::CheckpointUpload,
    committer: Arc<dyn crate::turn_commit::TurnCommitter>,
    log_heads: Arc<dyn crate::turn_commit::ThreadLogHeads>,
    layout: vam_storage::Drive9Layout,
}

impl Port52Dispatch for Port52Session {
    async fn dispatch(&mut self, env: VsockEnvelope) -> VsockEnvelope {
        use vam_spec::v1::{
            CommitTurnRequest, ExecuteHostToolRequest, FetchRunCheckpointRequest,
            ListAdvertisedToolsResponse, ToolIntentRequest, UploadRunCheckpointRequest,
        };

        match env.method.as_str() {
            "CommitTurn" => {
                // The turn is over: no tool call is running any more.
                self.call_window.clear();
                let Ok(req) = CommitTurnRequest::decode(&*env.payload) else {
                    return malformed_commit_turn_reply(&env);
                };
                let resp = crate::turn_commit::handle_commit_turn(
                    &self.store,
                    &self.committer,
                    &self.log_heads,
                    &self.checkpoint_session,
                    &req,
                )
                .await;
                port52_typed_reply(&env, "CommitTurnResponse", &resp)
            }
            "RequestToolAuthorization" => {
                // The previous call is over the moment the next one is asked for.
                self.call_window.clear();
                let Ok(req) = ToolIntentRequest::decode(&*env.payload) else {
                    return malformed_tool_intent_reply(&env);
                };
                // A resumed call whose answer was lost is settled first (a verify read, or a
                // re-send of a read), then decided like any other.
                self.sec_ctx
                    .reconcile_unknown(
                        &self.store,
                        Some(self.vault.as_ref()),
                        self.registry.as_ref(),
                        &self.skills_dir,
                        &req,
                    )
                    .await;
                let (tool_name, call_id) = (req.tool_name.clone(), req.call_id.clone());
                let generation = req
                    .lease
                    .as_ref()
                    .map(|l| l.lifecycle_generation as u64)
                    .unwrap_or(1);
                // A skill that needs a connector: refused or parked before the policy decides.
                let gate = crate::connector_park::precheck(
                    &self.store,
                    &self.vault,
                    &self.checkpoint_session,
                    &tool_name,
                )
                .await;
                let resp = self
                    .sec_ctx
                    .handle_tool_intent_request_gated(&self.store, req, gate);
                crate::connection_egress::record_intent(
                    &self.call_window,
                    &self.store,
                    &self.checkpoint_session,
                    &tool_name,
                    &call_id,
                    generation,
                    &resp,
                );
                port52_typed_reply(&env, "RequestToolAuthorizationResponse", &resp)
            }
            "ListAdvertisedTools" => {
                let tool_names = self.sec_ctx.advertised_tool_names(&self.store);
                let resp = ListAdvertisedToolsResponse { tool_names };
                port52_typed_reply(&env, "ListAdvertisedToolsResponse", &resp)
            }
            "ExecuteHostTool" => {
                let Ok(req) = ExecuteHostToolRequest::decode(&*env.payload) else {
                    return malformed_execute_reply(&env);
                };
                let resp = self
                    .sec_ctx
                    .handle_execute_host_tool(
                        &self.store,
                        Some(self.vault.as_ref()),
                        self.registry.as_ref(),
                        &self.skills_dir,
                        req,
                    )
                    .await;
                port52_typed_reply(&env, "ExecuteHostToolResponse", &resp)
            }
            "UploadRunCheckpoint" => {
                let Ok(req) = UploadRunCheckpointRequest::decode(&*env.payload) else {
                    return malformed_upload_reply(&env);
                };
                let resp = self
                    .checkpoint_upload
                    .accept_chunk_with_log(&self.store, &self.checkpoint_session, &req)
                    .await;
                port52_typed_reply(&env, "UploadRunCheckpointResponse", &resp)
            }
            "FetchRunCheckpoint" => {
                let Ok(req) = FetchRunCheckpointRequest::decode(&*env.payload) else {
                    return malformed_fetch_reply(&env);
                };
                let resp = crate::run_checkpoint::fetch_run_checkpoint_chunk_with(
                    &self.store,
                    Some(&self.layout),
                    &self.checkpoint_session,
                    &req,
                );
                port52_typed_reply(&env, "FetchRunCheckpointResponse", &resp)
            }
            "PullInbox" => {
                let Ok(req) = vam_spec::v1::PullInboxRequest::decode(&*env.payload) else {
                    return port52_envelope_error(
                        &env,
                        "PullInboxResponse",
                        PORT52_INVALID_ARGUMENT,
                        malformed_method_message("PullInbox"),
                    );
                };
                let resp =
                    crate::inbox::handle_pull_inbox(&self.store, &self.checkpoint_session, &req);
                port52_typed_reply(&env, "PullInboxResponse", &resp)
            }
            "PostOutbox" => {
                let Ok(req) = vam_spec::v1::PostOutboxRequest::decode(&*env.payload) else {
                    return port52_envelope_error(
                        &env,
                        "PostOutboxResponse",
                        PORT52_INVALID_ARGUMENT,
                        malformed_method_message("PostOutbox"),
                    );
                };
                let resp = crate::outbox::handle_post_outbox(
                    &self.sec_ctx,
                    &self.store,
                    &self.checkpoint_session,
                    &req,
                );
                port52_typed_reply(&env, "PostOutboxResponse", &resp)
            }
            _ => unknown_method_reply(&env),
        }
    }
}

/// The prompt-cache key of a session: a hash of its owner, the run's agent
/// digest and the thread (falling back to the session id when the thread is
/// not known). `None` while the session has no run yet.
fn session_cache_key(
    store: &Arc<SqliteWorkflowStore>,
    session_id: &str,
    owner: &str,
) -> Option<vam_inference::CacheKey> {
    let run_id = store.find_run_id_for_session(session_id).ok().flatten()?;
    let agent_digest = store
        .get_run_spec_context(&run_id)
        .ok()
        .and_then(|(spec_json, _, _)| spec_json)
        .and_then(|json| {
            vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &json).ok()
        })
        .and_then(|spec| spec.agent_ref.and_then(|r| r.digest).map(|d| d.hex_hash))
        .unwrap_or_default();
    let thread = crate::run_checkpoint::thread_scope_for_session(store, session_id, &run_id)
        .map(|(_, thread_ref)| thread_ref)
        .unwrap_or_else(|_| session_id.to_string());
    Some(vam_inference::CacheKey::derive(
        owner,
        &agent_digest,
        &thread,
    ))
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
pub async fn spawn_session_brokers(
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<crate::secrets::SecretsVault>,
    workspace_manager: Arc<WorkspaceManager>,
    session_id: String,
    workspace_id: String,
    timer: Option<SharedPhaseTimer>,
    host_listener: vam_vsock::HostListener,
    host_listener_53: vam_vsock::HostListener,
    host_listener_54: vam_vsock::HostListener,
    host_listener_55: vam_vsock::HostListener,
    telemetry_capture: TelemetryCapture,
    session_ca: Arc<vam_network::tls_ca::SessionCa>,
) -> Result<(), OrchestratorError> {
    // Fails closed for a session that belongs to a run: no brokers without the
    // run's agent. Only a session with no run may be `Unresolved`.
    let identity = resolve_session_identity(&store, &session_id, &workspace_id)?;
    if matches!(identity, AgentIdentity::Unresolved) {
        eprintln!(
            "[orchestrator] WARN session {session_id} workspace={workspace_id} has unresolved agent identity; using builtins-only"
        );
    }
    let agent_urn = identity.principal_urn().to_string();
    let resolved_principal = identity.as_resolved().map(str::to_string);
    let mut resolved =
        crate::security::resolve_session_policy(&store, &session_id, identity.as_resolved())?;
    if matches!(identity, AgentIdentity::Unresolved) {
        resolved.cedar = crate::security::DEFAULT_BUILTIN_FALLBACK_POLICY.to_string();
        resolved.warning = Some(format!(
            "unresolved agent identity for workspace '{workspace_id}'; using builtins-only"
        ));
    }
    crate::security::surface_policy_resolution(&store, &session_id, &agent_urn, &resolved);

    let budget_cost = store
        .find_run_id_for_session(&session_id)
        .ok()
        .flatten()
        .and_then(|run_id| store.get_run_spec_context(&run_id).ok())
        .and_then(|(spec_json, _, _)| spec_json)
        .and_then(|json_str| {
            vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &json_str)
                .ok()
        })
        .and_then(|spec| spec.max_financial_budget)
        .or_else(|| {
            Some(vam_spec::v1::Cost {
                currency: "USD".to_string(),
                micro_amount: 500_000,
            })
        });

    let mut budget = vam_inference::SessionBudget::new(budget_cost);
    let session_owner = store.session_owner_user_id(&session_id).unwrap_or_default();
    if !session_owner.is_empty() {
        budget = budget.with_user_id(session_owner.clone());
    }
    let budget = Arc::new(budget);
    store.register_session_budget(&session_id, budget.clone());

    let mut sec_ctx = crate::security::SessionSecurityContext::new(
        session_id.clone(),
        agent_urn.clone(),
        &resolved.cedar,
        vam_security::PolicyMode::Enforce,
        1,
        // Derived from the node's session master (never in source); fails closed
        // when the node keys are not installed.
        crate::signing::session_signing_key(&session_id)
            .map_err(|e| OrchestratorError::Core(format!("session signing key: {e}")))?,
        None,
        budget.clone(),
    )?
    .with_policy_mode(resolved.mode);
    sec_ctx.configure_pep_telemetry(telemetry_capture, workspace_manager.chunk_store().clone());
    let sec_ctx = Arc::new(sec_ctx);

    let advertised = sec_ctx.advertised_tool_names(&store);
    eprintln!(
        "[orchestrator] session {session_id} spawn principal={agent_urn} policy_urn={} resolution={:?} monitored_tools={:?} advertised_tools={:?}",
        resolved.policy_urn.as_deref().unwrap_or("-"),
        resolved.kind,
        resolved.monitored_tools,
        advertised
    );
    if resolved.skill_ref_count > 0 {
        let only_builtins = !advertised.is_empty()
            && advertised
                .iter()
                .all(|name| reserved_builtin_tool_names().contains(&name.as_str()));
        if only_builtins || advertised.is_empty() {
            eprintln!(
                "[orchestrator] WARN session {session_id} advertises only builtin tools despite {} skill_refs (principal={agent_urn})",
                resolved.skill_ref_count
            );
        }
    }

    let run_id = store
        .find_run_id_for_session(&session_id)
        .ok()
        .flatten()
        .unwrap_or_else(|| session_id.clone());
    let guest_stream = Arc::new(Mutex::new(
        GuestStreamReceiver::new(
            &session_id,
            &run_id,
            Some(workspace_manager.chunk_store().clone()),
            Some(store.clone()),
        )
        .with_capture(telemetry_capture),
    ));

    let call_window = Arc::new(vam_network::CallWindow::new());
    let call_window_52 = call_window.clone();
    let call_window_53 = call_window.clone();
    let call_window_54 = call_window;
    let session_id_clone_52 = session_id.clone();
    let store_clone_52 = store.clone();
    let sec_ctx_clone_52 = sec_ctx.clone();
    let vault_clone_52 = vault.clone();
    let skills_dir_52 = workspace_manager
        .layout()
        .session_dir(&session_id)
        .join("materialized/skills");
    let host_registry = Arc::new(crate::host_tools_fixtures::registry_for_node(
        installed_mock_scripts().is_some(),
    ));
    let guest_stream_52 = guest_stream.clone();
    let layout_52 = workspace_manager.layout().clone();
    let recorder = Arc::new(crate::thread_recorder::ThreadRecorder::new(
        session_id.clone(),
        store.clone(),
        workspace_manager.layout().clone(),
    ));
    store.register_thread_recorder(&session_id, recorder.clone());
    let committer = crate::turn_commit::committer_for(&workspace_manager, &store);
    tokio::spawn(async move {
        eprintln!(
            "[orchestrator] Host PEP listening on Port 52 for session {}...",
            session_id_clone_52
        );
        while let Ok(stream) = host_listener.accept().await {
            let sec_ctx_inner = sec_ctx_clone_52.clone();
            let store_inner = store_clone_52.clone();
            let heads_inner: Arc<dyn crate::turn_commit::ThreadLogHeads> =
                Arc::new(crate::thread_recorder::RecorderHeads {
                    store: store_clone_52.clone(),
                });
            let vault_inner = vault_clone_52.clone();
            let skills_dir_inner = skills_dir_52.clone();
            let registry_inner = host_registry.clone();
            let guest_stream_inner = guest_stream_52.clone();
            let checkpoint_session = session_id_clone_52.clone();
            let layout_inner = layout_52.clone();
            let committer_inner = committer.clone();
            let call_window_inner = call_window_52.clone();
            tokio::spawn(async move {
                let mut session = Port52Session {
                    sec_ctx: sec_ctx_inner,
                    store: store_inner,
                    vault: vault_inner,
                    call_window: call_window_inner,
                    skills_dir: skills_dir_inner,
                    registry: registry_inner,
                    checkpoint_session,
                    checkpoint_upload: crate::run_checkpoint::CheckpointUpload::default(),
                    committer: committer_inner,
                    log_heads: heads_inner,
                    layout: layout_inner,
                };
                serve_port52_connection(stream, Some(guest_stream_inner), &mut session).await;
            });
        }
        call_window_52.clear();
        eprintln!(
            "[orchestrator] Host PEP accept loop terminated for session {}",
            session_id_clone_52
        );
    });

    let session_id_clone_53 = session_id.clone();
    let budget_clone_53 = budget.clone();
    let vault_clone_53 = vault.clone();
    let sec_ctx_clone_53 = sec_ctx.clone();
    let store_clone_53 = store.clone();
    let session_owner_53 = session_owner.clone();
    let chunk_store_inf = workspace_manager.chunk_store().clone();
    let capture_inf = telemetry_capture;
    let recorder_53 = recorder.clone();
    let session_owner_cache = session_owner.clone();
    tokio::spawn(async move {
        use std::sync::Arc;
        use vam_inference::{AdvertisedToolsProvider, InferenceBrokerServer, ModelCatalog};

        use crate::secrets::VaultSecretLookup;

        struct SessionAdvertisedTools {
            sec_ctx: Arc<crate::security::SessionSecurityContext>,
            store: Arc<SqliteWorkflowStore>,
            call_window: Arc<vam_network::CallWindow>,
        }

        impl std::fmt::Debug for SessionAdvertisedTools {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.debug_struct("SessionAdvertisedTools")
                    .finish_non_exhaustive()
            }
        }

        impl AdvertisedToolsProvider for SessionAdvertisedTools {
            fn advertised_tool_names(&self) -> Vec<String> {
                // Asked once at the start of every inference request: the model is thinking
                // again, so the tool call that was running is over (CN-2.3, open question 1).
                self.call_window.clear();
                self.sec_ctx.advertised_tool_names(&self.store)
            }
        }

        let catalog = Arc::new(ModelCatalog::new());
        let secrets = Arc::new(
            VaultSecretLookup::new(vault_clone_53)
                .with_caller(session_owner_53)
                .with_audit(store_clone_53.clone(), session_id_clone_53.clone()),
        );
        let advertised = Arc::new(SessionAdvertisedTools {
            sec_ctx: sec_ctx_clone_53,
            store: store_clone_53.clone(),
            call_window: call_window_53,
        });

        let store_inf = store_clone_53.clone();
        let session_id_inf = session_id_clone_53.clone();
        let catalog_inf = catalog.clone();
        let inf_cb: vam_inference::InferenceCompletionCallback = Arc::new(
            move |model,
                  provider,
                  prompt_tokens,
                  completion_tokens,
                  duration_ms,
                  spend_micro,
                  assistant_text,
                  tool_calls_json,
                  cache,
                  extras| {
                let bound_run = store_inf
                    .find_run_id_for_session(&session_id_inf)
                    .ok()
                    .flatten();
                // Which model served the call, for the run's receipt. Evidence
                // only: the host of the catalog entry's endpoint, never more.
                if let Some(bound) = bound_run.as_deref() {
                    let host = catalog_inf
                        .get(model)
                        .map(|entry| crate::run_facts::endpoint_host(&entry.endpoint))
                        .unwrap_or_default();
                    if let Err(error) = store_inf.record_model_call(
                        bound,
                        model,
                        provider,
                        &host,
                        prompt_tokens,
                        completion_tokens,
                    ) {
                        tracing::warn!(run_id = bound, %error, "could not record the served model");
                    }
                }
                let run_id = bound_run.unwrap_or_else(|| session_id_inf.clone());

                let outcome = crate::pep_capture::capture_redacted_text(
                    capture_inf,
                    Some(&chunk_store_inf),
                    assistant_text,
                );
                let tool_calls = crate::pep_capture::capture_redacted_text(
                    capture_inf,
                    Some(&chunk_store_inf),
                    tool_calls_json,
                );

                if let Ok(payload) = crate::events::inference_completed_json(
                    &run_id,
                    0,
                    model,
                    provider,
                    prompt_tokens,
                    completion_tokens,
                    duration_ms,
                    spend_micro,
                    outcome.preview.as_deref(),
                    tool_calls.preview.as_deref(),
                    outcome.cas_digest.as_deref(),
                    tool_calls.cas_digest.as_deref(),
                    cache,
                    extras,
                ) {
                    let _ = store_inf.append_outbox_event(
                        &run_id,
                        crate::events::TYPE_INFERENCE_COMPLETED,
                        &payload,
                    );
                }
            },
        );

        // Host-derived cache key: owner, agent digest and thread, hashed. It
        // is resolved when the first request arrives (the run and thread may
        // not exist when the broker is built) and never comes from the guest.
        let cache_key_source: vam_inference::CacheKeySource = {
            let store = store_clone_53.clone();
            let session_id = session_id_clone_53.clone();
            let owner = session_owner_cache.clone();
            let resolved: Arc<std::sync::OnceLock<vam_inference::CacheKey>> =
                Arc::new(std::sync::OnceLock::new());
            Arc::new(move || {
                if let Some(key) = resolved.get() {
                    return Some(key.clone());
                }
                let key = session_cache_key(&store, &session_id, &owner)?;
                Some(resolved.get_or_init(|| key).clone())
            })
        };

        // The run's adapter switches, read per request from its stored harness. The
        // guest cannot set or read them.
        let harness_source: vam_inference::HarnessSource = {
            let store = store_clone_53.clone();
            let session_id = session_id_clone_53.clone();
            Arc::new(move || {
                crate::thread_recorder::session_harness(&store, &session_id)
                    .map(|h| h.adapter)
                    .unwrap_or_default()
            })
        };

        let store_trace = store_clone_53.clone();
        let session_id_trace = session_id_clone_53.clone();
        let mock_trace_cb: vam_inference::MockTraceCallback =
            Arc::new(move |trace: &vam_inference::MockTrace| {
                let run_id = store_trace
                    .find_run_id_for_session(&session_id_trace)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| session_id_trace.clone());
                let (event_type, payload) = mock_trace_event(trace);
                crate::trajectory::append_trajectory_event(
                    &store_trace,
                    &run_id,
                    event_type,
                    &payload,
                );
            });

        // AR-4.8: the run's own switch and the operator allowlist decide, per request, whether a
        // failing model may give way to another; the one switch is written as `model.fallback`.
        let fallback_source: vam_inference::FallbackSource = {
            let store = store_clone_53.clone();
            let session_id = session_id_clone_53.clone();
            Arc::new(move || store.fallback_plan_for_session(&session_id))
        };
        let fallback_notice: vam_inference::FallbackCallback = {
            let store = store_clone_53.clone();
            let session_id = session_id_clone_53.clone();
            Arc::new(move |switch: &vam_inference::ModelFallback| {
                let run_id = store
                    .find_run_id_for_session(&session_id)
                    .ok()
                    .flatten()
                    .unwrap_or_else(|| session_id.clone());
                crate::events::append_run_event(
                    &store,
                    &run_id,
                    crate::events::TYPE_MODEL_FALLBACK,
                    &crate::events::model_fallback_payload(switch),
                );
            })
        };

        // `budget.hit` (spend): the first refusal of a run is recorded; later ones add nothing.
        let budget_refusal_cb: vam_inference::BudgetRefusalCallback = {
            let store = store_clone_53.clone();
            let session_id = session_id_clone_53.clone();
            Arc::new(move || {
                if let Ok(Some(run_id)) = store.find_run_id_for_session(&session_id) {
                    if let Err(error) = store.record_spend_budget_hit(&run_id) {
                        tracing::warn!(run_id, %error, "budget.hit not recorded");
                    }
                }
            })
        };

        let mut broker = InferenceBrokerServer::with_advertised_tools(
            catalog,
            budget_clone_53,
            secrets,
            Some(advertised),
        )
        .with_fallback(fallback_source, Some(fallback_notice))
        .with_cache_key_source(cache_key_source)
        .with_harness_source(harness_source)
        .with_completion_callback(inf_cb)
        .with_budget_refusal_callback(budget_refusal_cb)
        .with_mock_trace_callback(mock_trace_cb)
        .with_exchange_observer(recorder_53);
        if let Some(scripts) = installed_mock_scripts() {
            broker = broker.with_mock_scripts(scripts);
        }
        // Process-guest nodes serve mock-model only, whatever a guest asks for.
        #[cfg(feature = "process-guest")]
        if crate::process_mode::is_active() {
            broker = broker.with_only_model(crate::process_mode::ONLY_MODEL);
        }
        let broker_server = Arc::new(broker);
        eprintln!(
            "[orchestrator] Inference Broker listening on Port 53 for session {}...",
            session_id_clone_53
        );

        while let Ok(stream) = host_listener_53.accept().await {
            let broker_server_clone = broker_server.clone();
            tokio::spawn(async move {
                broker_server_clone.handle_connection(stream).await;
            });
        }
        eprintln!(
            "[orchestrator] Inference Broker accept loop terminated for session {}",
            session_id_clone_53
        );
    });

    let session_mode = resolved.mode;
    let session_id_clone_54 = session_id.clone();
    let vault_clone_54 = vault.clone();
    let store_clone_54 = store.clone();
    let session_owner_54 = session_owner.clone();
    store.register_session_ca(&session_id, Arc::clone(&session_ca));
    let session_ca_for_proxy = session_ca;
    tokio::spawn(async move {
        let egress_bindings = vault_clone_54
            .session_egress_bindings_for_caller(&session_owner_54)
            .unwrap_or_else(|e| {
                eprintln!(
                    "[orchestrator] failed to load egress secret bindings for session {}: {}",
                    session_id_clone_54, e
                );
                Vec::new()
            });
        let allowed_domains = crate::secrets::broker_allowed_domains(&egress_bindings);
        let authorizer = SessionEgressAuthorizer::build(
            &store_clone_54,
            &session_id_clone_54,
            resolved_principal.as_deref(),
            session_mode,
            &allowed_domains,
        );
        eprintln!(
            "[orchestrator] Egress Broker session {} bindings={} allowed_domains={:?}",
            session_id_clone_54,
            egress_bindings.len(),
            allowed_domains
        );

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let test_override = egress_test_override();
        let allowed_ports = test_override
            .as_ref()
            .map(|hook| hook.allowed_ports.clone())
            .unwrap_or_default();
        let initial_snap = vam_spec::v1::NetworkPolicySnapshot {
            session_id: session_id_clone_54.clone(),
            policy_generation: 1,
            compiled_at_timestamp: now,
            policy: Some(vam_spec::v1::NetworkPolicy {
                mode: vam_spec::v1::NetworkMode::Brokered as i32,
                max_egress_bps: 0,
                max_ingress_bps: 0,
                max_packet_rate_pps: 0,
                // Hosts are decided by the session egress policy, not this list.
                allowed_domains: vec![],
                allowed_cidr_destinations: vec![],
                allowed_ports,
            }),
            resolved_allowed_ips: vec![],
        };
        let policy_container = Arc::new(vam_network::ActivePolicyContainer::new(initial_snap));
        let connection_source = Arc::new(crate::connection_egress::SessionConnectionSource::new(
            call_window_54,
            store_clone_54.clone(),
            vault_clone_54.clone(),
            session_id_clone_54.clone(),
            session_owner_54.clone(),
        ));
        let secret_provider = Arc::new(
            crate::secrets::VaultSecretProvider::new(vault_clone_54)
                .with_caller(session_owner_54)
                .with_audit(store_clone_54.clone(), session_id_clone_54.clone()),
        );
        let bindings = Arc::new(std::sync::Mutex::new(egress_bindings));

        let store_egress = store_clone_54.clone();
        let session_id_egress = session_id_clone_54.clone();
        let egress_cb: vam_network::EgressDecisionCallback =
            Arc::new(move |domain, port, disposition, policy_id| {
                crate::events::append_egress_decided(
                    &store_egress,
                    &session_id_egress,
                    domain,
                    port,
                    disposition,
                    policy_id,
                );
                let blocked = disposition.eq_ignore_ascii_case("DENY")
                    || disposition.eq_ignore_ascii_case("BLOCK")
                    || disposition.eq_ignore_ascii_case("DENIED");
                if blocked {
                    let ocsf = vam_telemetry::ocsf::from_egress_block(
                        &session_id_egress,
                        domain,
                        port as u16,
                        policy_id,
                    );
                    crate::egress::emit_ocsf(&ocsf);
                }
            });

        let mut gate = vam_network::BrokerGate::new(policy_container, secret_provider, bindings)
            .with_authorizer(Arc::new(authorizer))
            .with_decision_callback(egress_cb)
            .with_connection_tokens(connection_source)
            .with_passthrough_hosts(installed_passthrough_hosts())
            .with_max_egress_bytes(installed_egress_max_bytes());
        // TEST ONLY (process-guest builds): `graph.microsoft.com` is answered by the runner's
        // loopback Graph mock. The code does not exist in any other build.
        #[cfg(feature = "process-guest")]
        if let Some(base) = crate::process_mode::graph_endpoint() {
            gate = gate.with_upstream_redirect("graph.microsoft.com", base.as_str());
        }
        if let Some(hook) = test_override {
            gate = gate.with_resolver(hook.resolver);
            if hook.exempt_ip_deny {
                gate = gate.with_ip_deny_exemption();
            }
        }
        #[cfg(feature = "process-guest")]
        let fixture = installed_egress_fixture();
        #[cfg(feature = "process-guest")]
        if let Some(fixture) = &fixture {
            gate =
                gate.with_fixture_origin(vam_network::proxy::fixture::FIXTURE_HOST, fixture.addr());
        }
        let gate = Arc::new(gate);
        let broker_server = Arc::new(vam_network::EgressBrokerServer::shared(Arc::clone(&gate)));
        let proxy_gate: Arc<dyn vam_network::EgressGate> =
            Arc::clone(&gate) as Arc<dyn vam_network::EgressGate>;
        let proxy_ca = session_ca_for_proxy;
        let session_id_55 = session_id_clone_54.clone();
        tokio::spawn(async move {
            eprintln!(
                "[orchestrator] Egress proxy listening on Port 55 for session {session_id_55}..."
            );
            while let Ok(stream) = host_listener_55.accept().await {
                let gate = Arc::clone(&proxy_gate);
                let ca = Arc::clone(&proxy_ca);
                let session_id_conn = session_id_55.clone();
                #[cfg(feature = "process-guest")]
                let fixture = fixture.clone();
                tokio::spawn(async move {
                    #[cfg(feature = "process-guest")]
                    let served = match &fixture {
                        Some(fixture) => {
                            vam_network::proxy::serve_proxy_with_fixture(
                                stream,
                                gate,
                                Some(ca),
                                fixture,
                            )
                            .await
                        }
                        None => vam_network::proxy::serve_proxy(stream, gate, Some(ca)).await,
                    };
                    #[cfg(not(feature = "process-guest"))]
                    let served = vam_network::proxy::serve_proxy(stream, gate, Some(ca)).await;
                    if let Err(err) = served {
                        eprintln!(
                                    "[orchestrator] port 55 proxy ended for session {session_id_conn}: {err}"
                                );
                    }
                });
            }
            eprintln!(
                "[orchestrator] Egress proxy accept loop terminated for session {session_id_55}"
            );
        });
        eprintln!(
            "[orchestrator] Egress Broker listening on Port 54 for session {}...",
            session_id_clone_54
        );

        while let Ok(stream) = host_listener_54.accept().await {
            let broker_server_clone = broker_server.clone();
            tokio::spawn(async move {
                broker_server_clone.handle_connection(stream).await;
            });
        }
        eprintln!(
            "[orchestrator] Egress Broker accept loop terminated for session {}",
            session_id_clone_54
        );
    });

    mark_shared(&timer, "broker_setup");
    Ok(())
}

/// Where a host rule came from, for the rule name on `egress.decided`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EgressRuleOrigin {
    /// The pinned agent's `internet` permissions.
    Agent,
    /// A legacy session: the secrets' own `allowed_destinations`.
    SecretDestination,
}

/// The session's `EgressRequest` policy, evaluated for every host the guest
/// asks to reach on port 54 and port 55 (AX-5.3d).
///
/// `BrokerGate` consults it only after DNS and the platform IP floor, so it can
/// narrow what the floor allows and never widen it. Secret injection is a
/// separate check against each secret's own `allowed_destinations`; an agent
/// allow rule does not touch it. The authorizer fails closed: a policy that
/// could not be built, or that errors at evaluation, denies.
pub(crate) struct SessionEgressAuthorizer {
    /// `None` when the policy could not be built.
    policy: Option<String>,
    origin: EgressRuleOrigin,
}

impl SessionEgressAuthorizer {
    /// Permissions mode compiles the pinned agent's internet rules (AX-5.2b).
    /// Legacy mode permits exactly `secret_destinations`.
    pub(crate) fn build(
        store: &SqliteWorkflowStore,
        session_id: &str,
        principal_urn: Option<&str>,
        mode: crate::security::SessionPolicyMode,
        secret_destinations: &[String],
    ) -> Self {
        let (policy, origin) = match mode {
            crate::security::SessionPolicyMode::Legacy => (
                crate::policy_legacy::compile_legacy_egress_policy(secret_destinations)
                    .map_err(|e| e.to_string()),
                EgressRuleOrigin::SecretDestination,
            ),
            crate::security::SessionPolicyMode::Permissions => (
                permissions_egress_policy(store, session_id, principal_urn),
                EgressRuleOrigin::Agent,
            ),
        };
        match policy {
            Ok(policy) => Self {
                policy: Some(policy),
                origin,
            },
            Err(message) => {
                eprintln!(
                    "[orchestrator] ERROR session {session_id} egress policy could not be built; denying all egress: {message}"
                );
                Self {
                    policy: None,
                    origin,
                }
            }
        }
    }

    fn label(&self, rule: &str) -> String {
        match (self.origin, rule) {
            (_, "") => "no egress rule matched".to_string(),
            (EgressRuleOrigin::Agent, rule) => format!("agent rule {rule}"),
            (EgressRuleOrigin::SecretDestination, "internet:*") => {
                "secret destination *".to_string()
            }
            (EgressRuleOrigin::SecretDestination, rule) => {
                format!("secret destination {rule}")
            }
        }
    }
}

fn permissions_egress_policy(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
) -> Result<String, String> {
    let session = crate::policy_legacy::load_session_agent(store, session_id, principal_urn)
        .map_err(|e| e.to_string())?;
    let permissions = session
        .agent
        .as_ref()
        .and_then(|agent| agent.permissions.as_ref())
        .ok_or_else(|| "permissions session has no agent permissions".to_string())?;
    vam_security::permissions::resolve_internet(permissions)
        .map(|internet| internet.cedar)
        .map_err(|e| e.to_string())
}

impl vam_network::EgressAuthorizer for SessionEgressAuthorizer {
    fn authorize(&self, host: &str) -> vam_network::EgressAuthorization {
        use vam_network::EgressAuthorization::{Allow, Deny};
        let Some(policy) = &self.policy else {
            return Deny {
                rule: "egress policy unavailable".to_string(),
            };
        };
        match vam_security::evaluate_egress(policy, host) {
            Ok(decision) if decision.allowed => Allow {
                rule: self.label(&decision.deciding_rule),
            },
            // Cedar denies by default when no permit matches; that is the
            // `internet:*` default in permissions mode.
            Ok(decision)
                if decision.deciding_rule.is_empty() && self.origin == EgressRuleOrigin::Agent =>
            {
                Deny {
                    rule: self.label("internet:*"),
                }
            }
            Ok(decision) => Deny {
                rule: self.label(&decision.deciding_rule),
            },
            Err(err) => {
                eprintln!("[orchestrator] ERROR egress policy evaluation failed for {host}: {err}");
                Deny {
                    rule: "egress policy error".to_string(),
                }
            }
        }
    }
}

/// Hosts that force `CONNECT` passthrough (`egress.passthroughHosts`).
/// Empty unless `vamd` installs the parsed daemon config.
static PASSTHROUGH_HOSTS: RwLock<Vec<String>> = RwLock::new(Vec::new());

/// Per-session egress byte cap (`egress.maxBytes`). Starts at the daemon
/// default so a process that never installs its config still enforces it.
static EGRESS_MAX_BYTES: AtomicU64 = AtomicU64::new(DEFAULT_EGRESS_MAX_BYTES);

/// Scripts for the scripted mock model. `None` unless `vamd` loaded them from
/// `VAMD_E2E_MOCK_SCRIPTS` (design/e2e-journeys.md D3).
/// The run event for one mock trace. The message preview goes through the telemetry redactor.
pub(crate) fn mock_trace_event(
    trace: &vam_inference::MockTrace,
) -> (&'static str, serde_json::Value) {
    use vam_telemetry::{DefaultRedactor, TelemetryRedactor};
    match trace {
        vam_inference::MockTrace::RuleMatched { script, rule, step } => (
            crate::events::TYPE_MOCK_RULE_MATCHED,
            crate::events::mock_rule_matched_payload(script, *rule, step.as_deref()),
        ),
        vam_inference::MockTrace::Unmatched {
            script,
            reason,
            last_role,
            preview,
            step,
        } => {
            let redacted = DefaultRedactor::default().redact_text(preview);
            (
                crate::events::TYPE_MOCK_UNMATCHED,
                crate::events::mock_unmatched_payload(
                    script,
                    reason,
                    last_role,
                    &redacted,
                    step.as_deref(),
                ),
            )
        }
    }
}

static MOCK_SCRIPTS: RwLock<Option<Arc<vam_inference::MockScripts>>> = RwLock::new(None);

/// Hermetic tests pin DNS and, when needed, skip the IP deny set so a local
/// server can stand in for an allowed host. `vamd` never sets this.
static EGRESS_TEST_OVERRIDE: Mutex<Option<EgressTestOverride>> = Mutex::new(None);

/// Resolver and port allowlist installed by hermetic tests for one process.
#[derive(Clone)]
pub struct EgressTestOverride {
    pub resolver: Arc<dyn vam_network::HostResolver>,
    pub exempt_ip_deny: bool,
    pub allowed_ports: Vec<vam_spec::v1::AllowedPortRange>,
}

pub fn install_egress_passthrough_hosts(hosts: Vec<String>) {
    let mut slot = PASSTHROUGH_HOSTS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = hosts;
}

/// Install `egress.maxBytes` for sessions spawned after this call. `0` means
/// unlimited.
pub fn install_egress_max_bytes(max_bytes: u64) {
    EGRESS_MAX_BYTES.store(max_bytes, Ordering::SeqCst);
}

/// Loads the scripts named by `VAMD_E2E_MOCK_SCRIPTS`. An unset or blank value
/// means scripted mode is off, so `vamd` never enables it by accident.
pub fn load_mock_scripts(
    dir: Option<&str>,
) -> Result<Option<Arc<vam_inference::MockScripts>>, vam_inference::MockScriptError> {
    match dir.map(str::trim).filter(|dir| !dir.is_empty()) {
        None => Ok(None),
        Some(dir) => {
            let scripts = vam_inference::MockScripts::load_dir(std::path::Path::new(dir))?;
            Ok(Some(Arc::new(scripts)))
        }
    }
}

/// Installs the scripts for sessions spawned after this call.
pub fn install_mock_scripts(scripts: Option<Arc<vam_inference::MockScripts>>) {
    let mut slot = MOCK_SCRIPTS
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = scripts;
}

/// True on a node started with mock scripts (a test node).
pub(crate) fn mock_scripts_loaded() -> bool {
    installed_mock_scripts().is_some()
}

fn installed_mock_scripts() -> Option<Arc<vam_inference::MockScripts>> {
    MOCK_SCRIPTS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The E2E egress fixture, set once at start by a test node (mock scripts
/// loaded). Compiled only into `process-guest` builds.
#[cfg(feature = "process-guest")]
static EGRESS_FIXTURE: RwLock<Option<Arc<vam_network::proxy::fixture::EgressFixture>>> =
    RwLock::new(None);

/// Starts the fixture origin and installs it for sessions spawned after this call.
#[cfg(feature = "process-guest")]
pub async fn install_egress_fixture() -> Result<(), vam_network::VamNetworkError> {
    let fixture = vam_network::proxy::fixture::EgressFixture::start().await?;
    *EGRESS_FIXTURE
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(fixture);
    Ok(())
}

#[cfg(feature = "process-guest")]
fn installed_egress_fixture() -> Option<Arc<vam_network::proxy::fixture::EgressFixture>> {
    EGRESS_FIXTURE
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

pub fn set_egress_test_override(hook: Option<EgressTestOverride>) {
    let mut slot = EGRESS_TEST_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *slot = hook;
}

fn installed_egress_max_bytes() -> u64 {
    EGRESS_MAX_BYTES.load(Ordering::SeqCst)
}

fn installed_passthrough_hosts() -> Vec<String> {
    PASSTHROUGH_HOSTS
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

fn egress_test_override() -> Option<EgressTestOverride> {
    EGRESS_TEST_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

#[cfg(test)]
mod egress_authorizer_tests {
    use super::*;
    use vam_network::{match_domain, EgressAuthorization, EgressAuthorizer};
    use vam_spec::v1::{
        AgentPermissions, HostDefault, InternetPermissions, PermissionDecision, PermissionRule,
    };

    fn permissions(default: HostDefault, rules: &[(&str, PermissionDecision)]) -> AgentPermissions {
        AgentPermissions {
            tools: None,
            internet: Some(InternetPermissions {
                default: default as i32,
                rules: rules
                    .iter()
                    .map(|(target, decision)| PermissionRule {
                        target: target.to_string(),
                        decision: *decision as i32,
                    })
                    .collect(),
            }),
            advanced: Vec::new(),
            web: Default::default(),
        }
    }

    fn agent_authorizer(permissions: &AgentPermissions) -> SessionEgressAuthorizer {
        SessionEgressAuthorizer {
            policy: Some(
                vam_security::permissions::resolve_internet(permissions)
                    .expect("resolve")
                    .cedar,
            ),
            origin: EgressRuleOrigin::Agent,
        }
    }

    fn legacy_authorizer(destinations: &[&str]) -> SessionEgressAuthorizer {
        let destinations: Vec<String> = destinations.iter().map(|d| d.to_string()).collect();
        SessionEgressAuthorizer {
            policy: Some(
                crate::policy_legacy::compile_legacy_egress_policy(&destinations).expect("legacy"),
            ),
            origin: EgressRuleOrigin::SecretDestination,
        }
    }

    fn allow(rule: &str) -> EgressAuthorization {
        EgressAuthorization::Allow {
            rule: rule.to_string(),
        }
    }

    fn deny(rule: &str) -> EgressAuthorization {
        EgressAuthorization::Deny {
            rule: rule.to_string(),
        }
    }

    #[test]
    fn default_permissions_reach_any_host_and_name_the_default_rule() {
        let authorizer = agent_authorizer(&permissions(HostDefault::Allow, &[]));
        assert_eq!(
            authorizer.authorize("example.com"),
            allow("agent rule internet:*")
        );
    }

    #[test]
    fn deny_rule_refuses_subdomains_and_names_itself() {
        let authorizer = agent_authorizer(&permissions(
            HostDefault::Allow,
            &[("*.pastebin.com", PermissionDecision::Deny)],
        ));
        assert_eq!(
            authorizer.authorize("paste.pastebin.com"),
            deny("agent rule *.pastebin.com")
        );
        // `*.pastebin.com` does not cover the apex.
        assert_eq!(
            authorizer.authorize("pastebin.com"),
            allow("agent rule internet:*")
        );
        assert_eq!(
            authorizer.authorize("api.github.com"),
            allow("agent rule internet:*")
        );
    }

    #[test]
    fn most_specific_rule_wins() {
        let authorizer = agent_authorizer(&permissions(
            HostDefault::Allow,
            &[
                ("*.com", PermissionDecision::Deny),
                ("*.pastebin.com", PermissionDecision::Allow),
            ],
        ));
        assert_eq!(
            authorizer.authorize("paste.pastebin.com"),
            allow("agent rule *.pastebin.com")
        );
        assert_eq!(authorizer.authorize("evil.com"), deny("agent rule *.com"));
    }

    #[test]
    fn default_deny_names_the_default_rule() {
        let authorizer = agent_authorizer(&permissions(
            HostDefault::Deny,
            &[("api.github.com", PermissionDecision::Allow)],
        ));
        assert_eq!(
            authorizer.authorize("example.com"),
            deny("agent rule internet:*")
        );
        assert_eq!(
            authorizer.authorize("api.github.com"),
            allow("agent rule api.github.com")
        );
    }

    #[test]
    fn legacy_agent_with_no_secrets_reaches_nothing() {
        let authorizer = legacy_authorizer(&[]);
        for host in ["example.com", "api.github.com", "a.b.c"] {
            assert!(
                matches!(authorizer.authorize(host), EgressAuthorization::Deny { .. }),
                "{host}"
            );
        }
    }

    #[test]
    fn legacy_agent_reaches_exactly_its_secret_destinations() {
        let authorizer = legacy_authorizer(&["api.github.com", "*.atlassian.net"]);
        assert_eq!(
            authorizer.authorize("api.github.com"),
            allow("secret destination api.github.com")
        );
        assert_eq!(
            authorizer.authorize("acme.atlassian.net"),
            allow("secret destination *.atlassian.net")
        );
        assert!(matches!(
            authorizer.authorize("atlassian.net"),
            EgressAuthorization::Deny { .. }
        ));
        assert!(matches!(
            authorizer.authorize("example.com"),
            EgressAuthorization::Deny { .. }
        ));
    }

    /// The legacy compile must agree with the matcher the allowlist used.
    #[test]
    fn legacy_compile_matches_match_domain_for_every_pattern_shape() {
        let patterns = [
            "api.github.com",
            "API.GitHub.com",
            "*.atlassian.net",
            "*",
            "foo.*.com",
            "*.",
            "*.*.com",
            "a\"b.com",
        ];
        let hosts = [
            "api.github.com",
            "github.com",
            "x.api.github.com",
            "acme.atlassian.net",
            "atlassian.net",
            "a.b.atlassian.net",
            "xatlassian.net",
            "foo.bar.com",
            "example.com",
            "a.b.com",
        ];
        for pattern in patterns {
            let authorizer = legacy_authorizer(&[pattern]);
            for host in hosts {
                let expected = pattern == "*" || match_domain(pattern, host);
                let got = matches!(
                    authorizer.authorize(host),
                    EgressAuthorization::Allow { .. }
                );
                assert_eq!(got, expected, "pattern {pattern:?} host {host:?}");
            }
        }
    }

    #[test]
    fn an_unbuildable_policy_denies_everything() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        // No such session or agent: permissions mode cannot build, so it denies.
        let authorizer = SessionEgressAuthorizer::build(
            &store,
            "no-such-session",
            Some("urn:vam:agent:local.vam.dev/acme/missing"),
            crate::security::SessionPolicyMode::Permissions,
            &[],
        );
        assert!(authorizer.policy.is_none());
        assert_eq!(
            authorizer.authorize("example.com"),
            deny("egress policy unavailable")
        );

        let broken = SessionEgressAuthorizer {
            policy: Some("this is not cedar".to_string()),
            origin: EgressRuleOrigin::Agent,
        };
        assert_eq!(broken.authorize("example.com"), deny("egress policy error"));
    }
}

#[cfg(test)]
mod port52_dispatch_tests {
    use super::{serve_port52_connection, Port52Dispatch};
    use prost::Message;
    use std::sync::{Arc, Mutex};
    use vam_spec::v1::{UploadRunCheckpointRequest, UploadRunCheckpointResponse, VsockEnvelope};
    use vam_vsock::frame::{Frame, FrameType};

    struct RecordingDispatch {
        methods: Arc<Mutex<Vec<String>>>,
    }

    impl Port52Dispatch for RecordingDispatch {
        async fn dispatch(&mut self, env: VsockEnvelope) -> VsockEnvelope {
            self.methods
                .lock()
                .expect("methods")
                .push(env.method.clone());
            let response = UploadRunCheckpointResponse {
                accepted: true,
                digest: "stored".to_string(),
                error: None,
                received: 1,
                complete: true,
            };
            let mut payload = Vec::new();
            response.encode(&mut payload).expect("encode");
            VsockEnvelope {
                method: format!("{}Response", env.method),
                correlation_id: env.correlation_id,
                payload,
                error: None,
                trace_context: None,
            }
        }
    }

    fn envelope(method: &str, correlation_id: u32, payload: Vec<u8>) -> VsockEnvelope {
        VsockEnvelope {
            method: method.to_string(),
            correlation_id,
            payload,
            error: None,
            trace_context: None,
        }
    }

    async fn exchange(client: &mut tokio::io::DuplexStream, env: VsockEnvelope) -> VsockEnvelope {
        let correlation_id = env.correlation_id;
        let mut payload = Vec::new();
        env.encode(&mut payload).expect("encode request");
        let frame = Frame {
            frame_type: FrameType::ControlGrpc,
            correlation_id,
            payload: bytes::Bytes::from(payload),
        };
        frame.write_to(client).await.expect("write");
        let reply = Frame::read_from(client).await.expect("read");
        assert_eq!(reply.frame_type, FrameType::ControlGrpc);
        assert_eq!(reply.correlation_id, correlation_id);
        VsockEnvelope::decode(&*reply.payload).expect("decode reply")
    }

    fn spawn_server(methods: Arc<Mutex<Vec<String>>>) -> tokio::io::DuplexStream {
        let (client, server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut dispatch = RecordingDispatch { methods };
            serve_port52_connection(server, None, &mut dispatch).await;
        });
        client
    }

    fn valid_upload() -> VsockEnvelope {
        let mut payload = Vec::new();
        UploadRunCheckpointRequest::default()
            .encode(&mut payload)
            .expect("encode");
        envelope("UploadRunCheckpoint", 2, payload)
    }

    #[tokio::test]
    async fn garbage_upload_is_answered_and_the_next_request_still_works() {
        let methods = Arc::new(Mutex::new(Vec::new()));
        let mut client = spawn_server(methods.clone());

        let error = exchange(&mut client, envelope("UploadRunCheckpoint", 1, vec![0x80])).await;
        assert_eq!(error.method, "UploadRunCheckpointResponse");
        assert_eq!(error.correlation_id, 1);
        assert!(error.error.is_none(), "status lives on the typed response");
        let response = UploadRunCheckpointResponse::decode(&*error.payload).expect("response");
        assert!(!response.accepted);
        let status = response.error.expect("error status");
        assert_eq!(status.code, 3);
        assert_eq!(status.message, "malformed UploadRunCheckpoint request");
        assert!(methods.lock().expect("methods").is_empty());

        let ok = exchange(&mut client, valid_upload()).await;
        assert_eq!(ok.correlation_id, 2);
        let response = UploadRunCheckpointResponse::decode(&*ok.payload).expect("response");
        assert!(response.accepted);
        assert_eq!(response.digest, "stored");
        assert_eq!(
            methods.lock().expect("methods").as_slice(),
            ["UploadRunCheckpoint"]
        );
    }

    #[tokio::test]
    async fn garbage_commit_turn_is_answered_with_the_typed_response() {
        use vam_spec::v1::CommitTurnResponse;
        let methods = Arc::new(Mutex::new(Vec::new()));
        let mut client = spawn_server(methods.clone());

        let error = exchange(&mut client, envelope("CommitTurn", 5, vec![0x80])).await;
        assert_eq!(error.method, "CommitTurnResponse");
        assert_eq!(error.correlation_id, 5);
        let response = CommitTurnResponse::decode(&*error.payload).expect("response");
        assert!(!response.accepted);
        let status = response.error.expect("error status");
        assert_eq!(status.code, 3);
        assert_eq!(status.message, "malformed CommitTurn request");
        assert!(methods.lock().expect("methods").is_empty());
    }

    #[tokio::test]
    async fn unknown_method_is_answered_and_the_connection_stays_open() {
        let methods = Arc::new(Mutex::new(Vec::new()));
        let mut client = spawn_server(methods.clone());

        let error = exchange(&mut client, envelope("NotARealMethod", 9, Vec::new())).await;
        assert_eq!(error.method, "NotARealMethodResponse");
        assert_eq!(error.correlation_id, 9);
        assert!(error.payload.is_empty());
        let status = error.error.expect("envelope error");
        assert_eq!(status.code, 12);
        assert_eq!(status.message, "unknown method NotARealMethod");
        assert!(methods.lock().expect("methods").is_empty());

        let ok = exchange(&mut client, valid_upload()).await;
        let response = UploadRunCheckpointResponse::decode(&*ok.payload).expect("response");
        assert!(response.accepted);
        assert_eq!(
            methods.lock().expect("methods").as_slice(),
            ["UploadRunCheckpoint"]
        );
    }

    /// Answers `ExecuteHostTool` with a payload far over one frame and everything else with
    /// the ordinary upload reply.
    struct OversizeDispatch;

    impl Port52Dispatch for OversizeDispatch {
        async fn dispatch(&mut self, env: VsockEnvelope) -> VsockEnvelope {
            if env.method == "ExecuteHostTool" {
                return envelope(
                    "ExecuteHostToolResponse",
                    env.correlation_id,
                    vec![b'x'; vam_vsock::frame::MAX_PAYLOAD_SIZE * 2],
                );
            }
            let response = UploadRunCheckpointResponse {
                accepted: true,
                digest: "stored".to_string(),
                error: None,
                received: 1,
                complete: true,
            };
            let mut payload = Vec::new();
            response.encode(&mut payload).expect("encode");
            envelope("UploadRunCheckpointResponse", env.correlation_id, payload)
        }
    }

    #[tokio::test]
    async fn a_reply_over_the_frame_becomes_an_error_envelope_and_the_connection_stays_open() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(async move {
            let mut dispatch = OversizeDispatch;
            serve_port52_connection(server, None, &mut dispatch).await;
        });
        let mut payload = Vec::new();
        vam_spec::v1::ExecuteHostToolRequest::default()
            .encode(&mut payload)
            .expect("encode");

        let too_big = exchange(
            &mut client,
            envelope("ExecuteHostTool", 11, payload.clone()),
        )
        .await;
        assert_eq!(too_big.method, "ExecuteHostToolResponse");
        assert_eq!(too_big.correlation_id, 11);
        assert!(too_big.payload.is_empty());
        let status = too_big.error.expect("error envelope");
        assert_eq!(status.code, 8);
        assert!(
            status.message.contains("exceeds the frame"),
            "{}",
            status.message
        );

        // The same connection answers the next request, oversize or not.
        let ok = exchange(&mut client, valid_upload()).await;
        let response = UploadRunCheckpointResponse::decode(&*ok.payload).expect("response");
        assert!(response.accepted);
        let again = exchange(&mut client, envelope("ExecuteHostTool", 12, payload)).await;
        assert_eq!(again.correlation_id, 12);
        assert_eq!(again.error.expect("error envelope").code, 8);
    }

    #[test]
    fn pull_inbox_is_a_known_method_and_a_malformed_request_is_rejected() {
        let mut payload = Vec::new();
        vam_spec::v1::PullInboxRequest::default()
            .encode(&mut payload)
            .expect("encode");
        assert!(super::port52_rejection(&envelope("PullInbox", 1, payload)).is_none());
        let rejected = super::port52_rejection(&envelope("PullInbox", 2, vec![0xff, 0xff, 0xff]))
            .expect("garbage is rejected");
        assert_eq!(rejected.method, "PullInboxResponse");
        assert_eq!(
            rejected.error.expect("status").code,
            super::PORT52_INVALID_ARGUMENT
        );
    }

    #[test]
    fn post_outbox_is_a_known_method_and_a_malformed_request_is_rejected() {
        let mut payload = Vec::new();
        vam_spec::v1::PostOutboxRequest::default()
            .encode(&mut payload)
            .expect("encode");
        assert!(super::port52_rejection(&envelope("PostOutbox", 1, payload)).is_none());
        let rejected = super::port52_rejection(&envelope("PostOutbox", 2, vec![0xff, 0xff, 0xff]))
            .expect("garbage is rejected");
        assert_eq!(rejected.method, "PostOutboxResponse");
        assert_eq!(
            rejected.error.expect("status").code,
            super::PORT52_INVALID_ARGUMENT
        );
    }

    #[tokio::test]
    async fn undecodable_envelope_is_answered_and_the_connection_stays_open() {
        let methods = Arc::new(Mutex::new(Vec::new()));
        let mut client = spawn_server(methods.clone());

        let frame = Frame {
            frame_type: FrameType::ControlGrpc,
            correlation_id: 4,
            payload: bytes::Bytes::from(vec![0x80]),
        };
        frame.write_to(&mut client).await.expect("write");
        let reply = Frame::read_from(&mut client).await.expect("read");
        let error = VsockEnvelope::decode(&*reply.payload).expect("envelope");
        assert_eq!(error.correlation_id, 4);
        assert_eq!(error.method, "Error");
        let status = error.error.expect("error");
        assert_eq!(status.code, 3);
        assert_eq!(status.message, "malformed request");

        let ok = exchange(&mut client, valid_upload()).await;
        let response = UploadRunCheckpointResponse::decode(&*ok.payload).expect("response");
        assert!(response.accepted);
        assert_eq!(
            methods.lock().expect("methods").as_slice(),
            ["UploadRunCheckpoint"]
        );
    }
}

#[cfg(test)]
mod mock_scripts_load_tests {
    #[test]
    fn mock_traces_become_run_events() {
        let (kind, payload) = super::mock_trace_event(&vam_inference::MockTrace::RuleMatched {
            script: "s".into(),
            rule: 1,
            step: Some("start".into()),
        });
        assert_eq!(kind, "vam.v1.mock.rule_matched");
        assert_eq!(payload["rule"], 1);
        let (kind, payload) = super::mock_trace_event(&vam_inference::MockTrace::Unmatched {
            script: "s".into(),
            reason: "no_rule",
            last_role: "user".into(),
            preview: "token sk-abcdefghijklmnopqrstuvwxyz0123456789 end".into(),
            step: None,
        });
        assert_eq!(kind, "vam.v1.mock.unmatched");
        assert_eq!(payload["reason"], "no_rule");
        assert!(!payload["preview"]
            .as_str()
            .unwrap()
            .contains("sk-abcdefghijkl"));
    }

    use super::load_mock_scripts;

    #[test]
    fn unset_or_blank_value_keeps_scripted_mode_off() {
        assert!(load_mock_scripts(None).expect("no load").is_none());
        assert!(load_mock_scripts(Some("   ")).expect("no load").is_none());
    }

    #[test]
    fn missing_directory_is_a_load_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let missing = dir.path().join("absent");
        let err = load_mock_scripts(Some(missing.to_str().expect("utf8 path")))
            .expect_err("missing directory");
        assert!(
            err.to_string()
                .contains("cannot read mock script directory"),
            "{err}"
        );
    }

    #[test]
    fn a_directory_of_scripts_loads_with_its_ids() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("smoke.json"),
            r#"{"id": "smoke", "rules": [{"reply": {"text": "ok"}}]}"#,
        )
        .expect("write script");
        let scripts = load_mock_scripts(Some(dir.path().to_str().expect("utf8 path")))
            .expect("loads")
            .expect("scripted mode on");
        assert_eq!(scripts.ids().collect::<Vec<_>>(), vec!["smoke"]);
    }
}
