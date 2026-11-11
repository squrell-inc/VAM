use std::io;
use std::panic;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use vam_runtime::{
    cancel_active_lease, execute_tool_subprocess_with_trace, global_telemetry,
    run_react_agent_loop_with_trace, start_host_flush_loop, LeasedTaskExecutor, RuntimeError,
    TaskHandler,
};
use vam_vsock::{guest_dial, run_guest_registration, GuestListener};

// Exactly one init is compiled. The default build is the PID-1 init of the
// rootfs guest. `process-guest` (test-only) swaps it for an init that acts on
// nothing outside this process and drops the PID-1 code entirely.
#[cfg(not(feature = "process-guest"))]
#[path = "guest_init/pid1.rs"]
mod init;
#[cfg(feature = "process-guest")]
#[path = "guest_init/process.rs"]
mod init;

use prost::bytes::Bytes;
use prost::Message;
use prost_types::Timestamp;
use std::time::{SystemTime, UNIX_EPOCH};
use vam_spec::v1::{
    CancelTaskRequest, CancelTaskResponse, ErrorStatus, ExecuteTaskRequest, ExecuteTaskResponse,
    ExecuteToolRequest, ExecuteToolResponse, PingRequest, PingResponse, ShutdownRequest,
    ShutdownResponse, TaskOutcome, TaskOutcomeState, ToolResult, VsockEnvelope,
};
#[cfg(not(feature = "process-guest"))]
use vam_spec::v1::{ResumeFromSnapshotRequest, ResumeFromSnapshotResponse};
use vam_vsock::frame::{Frame, FrameType};

/// Seconds of CLOCK_MONOTONIC with millisecond precision, for console checkpoints.
#[cfg(not(feature = "process-guest"))]
fn mono_ts() -> String {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe {
        libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts);
    }
    format!("{}.{:03}", ts.tv_sec, ts.tv_nsec / 1_000_000)
}

/// One unbuffered console line: the last one printed shows how far a resume got.
#[cfg(not(feature = "process-guest"))]
fn resume_checkpoint(step: &str) {
    eprintln!("[guest-agent] resume: {} t_mono={}", step, mono_ts());
}

/// One console line in the golden bake VM, with the guest monotonic clock. The bake
/// VM's console is the only place to see how far its guest got before the host
/// paused it. Nothing prints on a cold boot or in a restored guest.
#[cfg(not(feature = "process-guest"))]
fn bake_checkpoint(step: &str) {
    eprintln!("[guest-agent] bake: {} t_mono={}", step, mono_ts());
}

#[cfg(feature = "process-guest")]
fn bake_checkpoint(_step: &str) {}

/// What the guest starts once it has registered with the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PostRegistrationPlan {
    /// The long-lived telemetry flush to host port 52.
    flush_loop: bool,
    /// The dial to the host egress broker on port 54.
    egress_connector: bool,
}

/// The golden bake VM starts neither: both are background tasks with outstanding
/// vsock activity, and the host pauses the VM for its snapshot while they run. The
/// resume handler starts both after restore. Every other guest starts both now.
fn post_registration_plan(golden_bake: bool) -> PostRegistrationPlan {
    PostRegistrationPlan {
        flush_loop: !golden_bake,
        egress_connector: !golden_bake,
    }
}

async fn spawn_egress_proxy_connector() {
    if !init::egress_enabled() {
        eprintln!("[guest-agent] Egress proxy not configured; not starting it.");
        return;
    }
    tokio::spawn(async move {
        for retry in 0..15 {
            match guest_dial(2, 54).await {
                Ok(stream) => {
                    eprintln!(
                        "[guest-agent] Reconnected to Host Egress Broker on Port 54 after snapshot restore."
                    );
                    if let Err(e) = init::start_egress_proxy(stream).await {
                        eprintln!("[guest-agent] Guest proxy error: {}", e);
                    }
                    break;
                }
                Err(e) => {
                    if retry == 14 {
                        eprintln!(
                            "[guest-agent] Notice: Host Port 54 egress broker not available after restore: {}",
                            e
                        );
                    }
                    sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
}

/// Local Task Handler implementing TaskHandler to route ExecuteTask to execute_tool_subprocess
struct LocalExecTaskHandler;

impl TaskHandler for LocalExecTaskHandler {
    fn execute(&self, request: &ExecuteTaskRequest) -> Result<TaskOutcome, RuntimeError> {
        let lease = request.lease.as_ref().ok_or(RuntimeError::MissingLease)?;
        let lease_id = lease
            .lease_id
            .as_ref()
            .map(|id| id.urn.clone())
            .unwrap_or_default();

        match request.task_action.as_ref() {
            Some(vam_spec::v1::execute_task_request::TaskAction::ToolAction(ref ta)) => {
                let timeout_dur = match request.timeout {
                    Some(ref d) => Duration::new(d.seconds as u64, d.nanos as u32),
                    None => Duration::from_secs(30),
                };

                // Create blocking execution loop
                let result = tokio::task::block_in_place(|| {
                    let runtime = tokio::runtime::Handle::current();
                    runtime.block_on(async {
                        execute_tool_subprocess_with_trace(
                            &ta.tool_name,
                            ta.arguments.as_ref(),
                            timeout_dur,
                            Some(lease_id),
                            request.trace_context.as_ref(),
                        )
                        .await
                    })
                });

                match result {
                    Ok(tr) => {
                        let state = if tr.exit_code == 0 {
                            TaskOutcomeState::Succeeded
                        } else {
                            TaskOutcomeState::Failed
                        };

                        let mut out_struct = prost_types::Struct::default();
                        out_struct.fields.insert(
                            "stdout".to_string(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(
                                    String::from_utf8_lossy(&tr.stdout).into_owned(),
                                )),
                            },
                        );
                        out_struct.fields.insert(
                            "stderr".to_string(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(
                                    String::from_utf8_lossy(&tr.stderr).into_owned(),
                                )),
                            },
                        );

                        Ok(TaskOutcome {
                            lease: request.lease.clone(),
                            state: state as i32,
                            output: Some(out_struct),
                            output_artifacts: Vec::new(),
                            continuation: None,
                            evidence: None,
                            observed_usage: None,
                            error: tr.error,
                        })
                    }
                    Err(e) => Err(e),
                }
            }
            Some(vam_spec::v1::execute_task_request::TaskAction::PromptAction(ref pa)) => {
                let result = tokio::task::block_in_place(|| {
                    let runtime = tokio::runtime::Handle::current();
                    runtime.block_on(async {
                        run_react_agent_loop_with_trace(
                            lease,
                            pa,
                            request.inputs.as_ref(),
                            request.trace_context.as_ref(),
                        )
                        .await
                    })
                });

                match result {
                    Ok(outcome) => Ok(outcome),
                    Err(e) => Err(RuntimeError::Handler(format!("ReAct loop error: {}", e))),
                }
            }
            None => Err(RuntimeError::MissingAction),
        }
    }
}

/// Wrap the entire main function in a panic hook and catch_unwind so it never returns.
fn main() {
    // Install a panic hook that logs before calling halt
    panic::set_hook(Box::new(|panic_info| {
        let payload = panic_info.payload();
        let message = if let Some(s) = payload.downcast_ref::<&str>() {
            *s
        } else if let Some(s) = payload.downcast_ref::<String>() {
            s.as_str()
        } else {
            "Unknown panic"
        };
        eprintln!("[guest-agent] CRITICAL PANIC DETECTED: {}", message);
        init::halt(format!("CRITICAL PANIC: {}", message));
    }));

    let result = panic::catch_unwind(|| {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();

        rt.block_on(async_main()).unwrap();
    });

    match result {
        Ok(_) => init::halt("async_main returned normally (violates RI-1)".to_string()),
        Err(e) => {
            let msg = if let Some(s) = e.downcast_ref::<&str>() {
                *s
            } else if let Some(s) = e.downcast_ref::<String>() {
                s.as_str()
            } else {
                "Unknown panic payload"
            };
            init::halt(format!("Main loop panic: {}", msg));
        }
    }
}

async fn async_main() -> io::Result<()> {
    eprintln!("[guest-agent] Initializing VAM guest agent...");

    let start_mounts = std::time::Instant::now();
    init::startup();
    global_telemetry().record_span(
        "init.mounts",
        start_mounts.elapsed(),
        None,
        std::collections::HashMap::new(),
    );

    let instance_id = init::instance_id();
    eprintln!("[guest-agent] Operating under instance ID: {}", instance_id);

    let my_cid = init::local_cid();

    // RI-3 Readiness Ordering: Must bind VSOCK Port 52 BEFORE registering!
    eprintln!("[guest-agent] Binding VSOCK listener on Port 52...");
    let mut listener = GuestListener::bind(52).await?;
    let golden_bake = init::golden_bake();
    if golden_bake {
        bake_checkpoint("listener bound on port 52");
    }

    // Dial out to Host (CID 2, port 52) to register
    let mut connected = false;
    let mut retries = 0;
    let mut backoff = Duration::from_millis(1);
    let max_backoff = Duration::from_millis(50);
    while !connected && retries < 20 {
        eprintln!(
            "[guest-agent] Attempting to dial Host (CID 2, port 52) to register... (retry {})",
            retries
        );
        match guest_dial(2, 52).await {
            Ok(stream) => {
                eprintln!("[guest-agent] Connected to Host. Initiating registration handshake...");
                match run_guest_registration(stream, &instance_id, my_cid).await {
                    Ok(_) => {
                        eprintln!("[guest-agent] Registration accepted by Host!");
                        connected = true;
                    }
                    Err(e) => {
                        eprintln!(
                            "[guest-agent] Registration handshake failed: {}. Retrying...",
                            e
                        );
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "[guest-agent] Connection to Host failed: {}. Retrying...",
                    e
                );
            }
        }
        if !connected {
            sleep(backoff).await;
            backoff = std::cmp::min(backoff * 2, max_backoff);
            retries += 1;
        }
    }

    if !connected {
        init::halt("Failed to register with Host after 20 attempts.".to_string());
    }

    if golden_bake {
        bake_checkpoint("registered with host");
    }
    let plan = post_registration_plan(golden_bake);

    // Long-lived Port 52 StreamLogs flush (fail-open; does not affect Cedar/workflow).
    if plan.flush_loop {
        start_host_flush_loop();
    }

    // Dial Host (CID 2, port 54) to start the guest egress HTTP proxy in the background
    if plan.egress_connector {
        spawn_egress_proxy_connector().await;
    }

    // Now accept loop for host-initiated connections
    let leased_executor = Arc::new(LeasedTaskExecutor::new(1)); // Initial lifecycle generation 1
    if golden_bake {
        bake_checkpoint("flush loop and egress connector deferred until resume");
        bake_checkpoint("entering accept loop");
    }

    loop {
        eprintln!("[guest-agent] Waiting for host connection on Port 52...");
        match listener.accept().await {
            Ok((stream, peer_cid, peer_port)) => {
                eprintln!(
                    "[guest-agent] Accepted host connection from CID {}, Port {} t_mono={}",
                    peer_cid,
                    peer_port,
                    {
                        #[cfg(not(feature = "process-guest"))]
                        {
                            mono_ts()
                        }
                        #[cfg(feature = "process-guest")]
                        {
                            String::new()
                        }
                    }
                );
                let inst_id = instance_id.clone();
                let executor_clone = leased_executor.clone();
                tokio::spawn(handle_guest_rpc_connection(stream, inst_id, executor_clone));
            }
            Err(e) => {
                eprintln!("[guest-agent] Accept error: {}", e);
                sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Whether this guest flushes disks before it answers `Shutdown`. A process guest
/// (test-only) runs on the host and acts on nothing outside its own process: a
/// host-wide `sync(2)` is not its business, and on a busy machine (a CI runner that
/// has just built gigabytes) it would hold the reply, which the stop path waits
/// for, for seconds.
const FLUSHES_DISKS: bool = cfg!(not(feature = "process-guest"));

/// Flushes every filesystem the guest has mounted to its disk. The host stops the
/// VM right after a shutdown reply (SIGKILL, once its grace is up), so what the
/// workspace holds must be on the image before the host is told.
fn flush_disks() {
    if !FLUSHES_DISKS {
        return;
    }
    #[cfg(target_os = "linux")]
    // SAFETY: sync(2) takes no arguments and touches no memory of ours.
    unsafe {
        libc::sync();
    }
}

/// Answers a `Shutdown` request: runs `flush` first, then sends the reply, so a
/// host that has the reply knows the disk is current. Returns the grace period
/// and the reason the host gave.
async fn answer_shutdown<S>(
    stream: &mut S,
    env: &VsockEnvelope,
    flush: impl FnOnce(),
) -> Result<(Duration, String), String>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    let req = ShutdownRequest::decode(&*env.payload).map_err(|e| e.to_string())?;
    let grace_dur = match req.grace_period {
        Some(ref d) => Duration::new(d.seconds as u64, d.nanos as u32),
        None => Duration::from_millis(100),
    };

    let response = ShutdownResponse { accepted: true };
    let mut resp_payload = Vec::new();
    response
        .encode(&mut resp_payload)
        .map_err(|e| e.to_string())?;
    let env_out = VsockEnvelope {
        method: "ShutdownResponse".to_string(),
        correlation_id: env.correlation_id,
        payload: resp_payload,
        error: None,
        trace_context: None,
    };
    let mut env_payload = Vec::new();
    env_out
        .encode(&mut env_payload)
        .map_err(|e| e.to_string())?;
    let frame_out = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: env.correlation_id,
        payload: Bytes::from(env_payload),
    };

    flush();
    let _ = frame_out.write_to(stream).await;
    Ok((grace_dur, req.reason))
}

/// Standard async RPC dispatcher for in-guest connections.
async fn handle_guest_rpc_connection<S>(
    mut stream: S,
    _instance_id: String,
    executor: Arc<LeasedTaskExecutor>,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    loop {
        let frame = match Frame::read_from(&mut stream).await {
            Ok(f) => f,
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(e) => return Err(e.to_string()),
        };

        if frame.frame_type != FrameType::ControlGrpc {
            return Err(format!("Unexpected frame type: {:?}", frame.frame_type));
        }

        let env = VsockEnvelope::decode(&*frame.payload).map_err(|e| e.to_string())?;
        match env.method.as_str() {
            "Ping" => {
                let _req = PingRequest::decode(&*env.payload).map_err(|e| e.to_string())?;
                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
                let response = PingResponse {
                    received_at: Some(Timestamp {
                        seconds: now.as_secs() as i64,
                        nanos: now.subsec_nanos() as i32,
                    }),
                };
                let mut resp_payload = Vec::new();
                response
                    .encode(&mut resp_payload)
                    .map_err(|e| e.to_string())?;

                let env_out = VsockEnvelope {
                    method: "PingResponse".to_string(),
                    correlation_id: env.correlation_id,
                    payload: resp_payload,
                    error: None,
                    trace_context: None,
                };
                let mut env_payload = Vec::new();
                env_out
                    .encode(&mut env_payload)
                    .map_err(|e| e.to_string())?;

                let frame_out = Frame {
                    frame_type: FrameType::ControlGrpc,
                    correlation_id: env.correlation_id,
                    payload: Bytes::from(env_payload),
                };
                frame_out
                    .write_to(&mut stream)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            "ExecuteTool" => {
                let req = ExecuteToolRequest::decode(&*env.payload).map_err(|e| e.to_string())?;
                let timeout_dur = match req.timeout {
                    Some(ref d) => Duration::new(d.seconds as u64, d.nanos as u32),
                    None => Duration::from_secs(30),
                };

                // Spawn tool subprocess
                match execute_tool_subprocess_with_trace(
                    &req.tool_name,
                    req.arguments.as_ref(),
                    timeout_dur,
                    None,
                    req.trace_context.as_ref(),
                )
                .await
                {
                    Ok(tr) => {
                        let mut out_struct = prost_types::Struct::default();
                        out_struct.fields.insert(
                            "stdout".to_string(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(
                                    String::from_utf8_lossy(&tr.stdout).into_owned(),
                                )),
                            },
                        );
                        out_struct.fields.insert(
                            "stderr".to_string(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::StringValue(
                                    String::from_utf8_lossy(&tr.stderr).into_owned(),
                                )),
                            },
                        );
                        out_struct.fields.insert(
                            "exit_code".to_string(),
                            prost_types::Value {
                                kind: Some(prost_types::value::Kind::NumberValue(
                                    tr.exit_code as f64,
                                )),
                            },
                        );
                        if !tr.system_logs.is_empty() {
                            out_struct.fields.insert(
                                "system_logs".to_string(),
                                prost_types::Value {
                                    kind: Some(prost_types::value::Kind::StringValue(
                                        tr.system_logs,
                                    )),
                                },
                            );
                        }

                        let response = ExecuteToolResponse {
                            result: Some(ToolResult {
                                output: Some(out_struct),
                                error: tr.error,
                            }),
                            resource_usage: tr.resource_usage,
                        };
                        let mut resp_payload = Vec::new();
                        response
                            .encode(&mut resp_payload)
                            .map_err(|e| e.to_string())?;

                        let env_out = VsockEnvelope {
                            method: "ExecuteToolResponse".to_string(),
                            correlation_id: env.correlation_id,
                            payload: resp_payload,
                            error: None,
                            trace_context: None,
                        };
                        let mut env_payload = Vec::new();
                        env_out
                            .encode(&mut env_payload)
                            .map_err(|e| e.to_string())?;

                        let frame_out = Frame {
                            frame_type: FrameType::ControlGrpc,
                            correlation_id: env.correlation_id,
                            payload: Bytes::from(env_payload),
                        };
                        frame_out
                            .write_to(&mut stream)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    Err(e) => {
                        let env_out = VsockEnvelope {
                            method: "ExecuteToolResponse".to_string(),
                            correlation_id: env.correlation_id,
                            payload: Vec::new(),
                            error: Some(ErrorStatus {
                                code: 13, // INTERNAL
                                message: format!("Tool exec failed: {}", e),
                                details: Vec::new(),
                                retryable: false,
                            }),
                            trace_context: None,
                        };
                        let mut env_payload = Vec::new();
                        env_out
                            .encode(&mut env_payload)
                            .map_err(|e| e.to_string())?;

                        let frame_out = Frame {
                            frame_type: FrameType::ControlGrpc,
                            correlation_id: env.correlation_id,
                            payload: Bytes::from(env_payload),
                        };
                        frame_out
                            .write_to(&mut stream)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            "ExecuteTask" => {
                let req = ExecuteTaskRequest::decode(&*env.payload).map_err(|e| e.to_string())?;
                let handler = LocalExecTaskHandler;
                match executor.execute(&req, &handler) {
                    Ok(outcome) => {
                        let response = ExecuteTaskResponse {
                            outcome: Some(outcome),
                            resource_usage: None,
                        };
                        let mut resp_payload = Vec::new();
                        response
                            .encode(&mut resp_payload)
                            .map_err(|e| e.to_string())?;

                        let env_out = VsockEnvelope {
                            method: "ExecuteTaskResponse".to_string(),
                            correlation_id: env.correlation_id,
                            payload: resp_payload,
                            error: None,
                            trace_context: None,
                        };
                        let mut env_payload = Vec::new();
                        env_out
                            .encode(&mut env_payload)
                            .map_err(|e| e.to_string())?;

                        let frame_out = Frame {
                            frame_type: FrameType::ControlGrpc,
                            correlation_id: env.correlation_id,
                            payload: Bytes::from(env_payload),
                        };
                        frame_out
                            .write_to(&mut stream)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                    Err(e) => {
                        let env_out = VsockEnvelope {
                            method: "ExecuteTaskResponse".to_string(),
                            correlation_id: env.correlation_id,
                            payload: Vec::new(),
                            error: Some(ErrorStatus {
                                code: 13, // INTERNAL
                                message: format!("Task execution error: {}", e),
                                details: Vec::new(),
                                retryable: false,
                            }),
                            trace_context: None,
                        };
                        let mut env_payload = Vec::new();
                        env_out
                            .encode(&mut env_payload)
                            .map_err(|e| e.to_string())?;

                        let frame_out = Frame {
                            frame_type: FrameType::ControlGrpc,
                            correlation_id: env.correlation_id,
                            payload: Bytes::from(env_payload),
                        };
                        frame_out
                            .write_to(&mut stream)
                            .await
                            .map_err(|e| e.to_string())?;
                    }
                }
            }
            "CancelTask" => {
                let req = CancelTaskRequest::decode(&*env.payload).map_err(|e| e.to_string())?;
                let lease = req
                    .lease
                    .as_ref()
                    .ok_or("Missing lease in cancel request")?;
                let lease_id = lease
                    .lease_id
                    .as_ref()
                    .map(|id| id.urn.clone())
                    .unwrap_or_default();

                let accepted = cancel_active_lease(&lease_id);
                let response = CancelTaskResponse {
                    accepted,
                    error: if accepted {
                        None
                    } else {
                        Some(ErrorStatus {
                            code: 5, // NOT_FOUND
                            message: "Task lease not currently active or already completed"
                                .to_string(),
                            details: Vec::new(),
                            retryable: false,
                        })
                    },
                };
                let mut resp_payload = Vec::new();
                response
                    .encode(&mut resp_payload)
                    .map_err(|e| e.to_string())?;

                let env_out = VsockEnvelope {
                    method: "CancelTaskResponse".to_string(),
                    correlation_id: env.correlation_id,
                    payload: resp_payload,
                    error: None,
                    trace_context: None,
                };
                let mut env_payload = Vec::new();
                env_out
                    .encode(&mut env_payload)
                    .map_err(|e| e.to_string())?;

                let frame_out = Frame {
                    frame_type: FrameType::ControlGrpc,
                    correlation_id: env.correlation_id,
                    payload: Bytes::from(env_payload),
                };
                frame_out
                    .write_to(&mut stream)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            "Shutdown" => {
                let (grace_dur, reason) = answer_shutdown(&mut stream, &env, flush_disks).await?;

                // Wait grace duration and halt
                sleep(grace_dur).await;
                init::halt(format!("Graceful shutdown requested. Reason: {}", reason));
            }
            // Host asks the bake VM to confirm it is quiescent before it pauses the
            // VM. Reaching this arm means the accept loop is running; nothing else
            // is in flight because the bake guest started no background vsock task.
            // Only the golden bake VM answers it.
            "PrepareForSnapshot" if init::golden_bake() => {
                bake_checkpoint("prepare for snapshot received");
                #[cfg(target_os = "linux")]
                unsafe {
                    libc::sync();
                }
                let env_out = VsockEnvelope {
                    method: "PrepareForSnapshotResponse".to_string(),
                    correlation_id: env.correlation_id,
                    payload: Vec::new(),
                    error: None,
                    trace_context: None,
                };
                let mut env_payload = Vec::new();
                env_out
                    .encode(&mut env_payload)
                    .map_err(|e| e.to_string())?;
                let frame_out = Frame {
                    frame_type: FrameType::ControlGrpc,
                    correlation_id: env.correlation_id,
                    payload: Bytes::from(env_payload),
                };
                frame_out
                    .write_to(&mut stream)
                    .await
                    .map_err(|e| e.to_string())?;
                // Close our end now: the host reads EOF as "the guest closed", so no
                // connection is mid-teardown when the VM is paused.
                let _ = tokio::io::AsyncWriteExt::shutdown(&mut stream).await;
                bake_checkpoint("ready to snapshot");
                return Ok(());
            }
            // Snapshot restore remounts disks, writes entropy and sets the clock.
            // None of that exists in the process-guest build, which answers
            // UNIMPLEMENTED through the generic arm below.
            #[cfg(not(feature = "process-guest"))]
            "ResumeFromSnapshot" => {
                let req =
                    ResumeFromSnapshotRequest::decode(&*env.payload).map_err(|e| e.to_string())?;
                if req.entropy.len() != 256 {
                    return Err(format!(
                        "ResumeFromSnapshot entropy must be 256 bytes, got {}",
                        req.entropy.len()
                    ));
                }
                resume_checkpoint("request received");
                let t_resume = std::time::Instant::now();
                resume_checkpoint("remount start");
                init::remount_drive9_after_snapshot();
                resume_checkpoint("remount done");
                let remount_ms = t_resume.elapsed().as_millis();
                init::write_entropy_to_urandom(&req.entropy)?;
                resume_checkpoint("entropy done");
                let entropy_ms = t_resume.elapsed().as_millis();
                init::set_guest_clock_unix_nanos(req.clock_unix_nanos)?;
                resume_checkpoint("clock done");
                let clock_ms = t_resume.elapsed().as_millis();
                executor.fence_min_generation(req.lease_min_generation);
                // The bake VM deferred both (see `post_registration_plan`).
                start_host_flush_loop();
                spawn_egress_proxy_connector().await;
                resume_checkpoint("fence and egress done");
                eprintln!(
                    "[guest-agent] resume timing: remount={}ms entropy_at={}ms clock_at={}ms total={}ms",
                    remount_ms,
                    entropy_ms,
                    clock_ms,
                    t_resume.elapsed().as_millis()
                );

                let response = ResumeFromSnapshotResponse { accepted: true };
                let mut resp_payload = Vec::new();
                response
                    .encode(&mut resp_payload)
                    .map_err(|e| e.to_string())?;

                let env_out = VsockEnvelope {
                    method: "ResumeFromSnapshotResponse".to_string(),
                    correlation_id: env.correlation_id,
                    payload: resp_payload,
                    error: None,
                    trace_context: None,
                };
                let mut env_payload = Vec::new();
                env_out
                    .encode(&mut env_payload)
                    .map_err(|e| e.to_string())?;

                let frame_out = Frame {
                    frame_type: FrameType::ControlGrpc,
                    correlation_id: env.correlation_id,
                    payload: Bytes::from(env_payload),
                };
                frame_out
                    .write_to(&mut stream)
                    .await
                    .map_err(|e| e.to_string())?;
                resume_checkpoint("answer sent");
            }
            other => {
                let err_status = ErrorStatus {
                    code: 12, // UNIMPLEMENTED
                    message: format!("Method '{}' is unimplemented on the guest", other),
                    details: Vec::new(),
                    retryable: false,
                };
                let env_out = VsockEnvelope {
                    method: format!("{}Response", other),
                    correlation_id: env.correlation_id,
                    payload: Vec::new(),
                    error: Some(err_status),
                    trace_context: None,
                };
                let mut env_payload = Vec::new();
                env_out
                    .encode(&mut env_payload)
                    .map_err(|e| e.to_string())?;

                let frame_out = Frame {
                    frame_type: FrameType::ControlGrpc,
                    correlation_id: env.correlation_id,
                    payload: Bytes::from(env_payload),
                };
                frame_out
                    .write_to(&mut stream)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::bytes::Bytes;
    use prost::Message;
    use tokio::io::duplex;
    use vam_spec::v1::{ExecuteToolRequest, ExecuteToolResponse, PingRequest, PingResponse};
    use vam_vsock::frame::{Frame, FrameType};

    async fn send_request<M: Message, S: tokio::io::AsyncWrite + Unpin>(
        stream: &mut S,
        method: &str,
        correlation_id: u32,
        req: &M,
    ) {
        let mut req_payload = Vec::new();
        req.encode(&mut req_payload).unwrap();

        let env = VsockEnvelope {
            method: method.to_string(),
            correlation_id,
            payload: req_payload,
            error: None,
            trace_context: None,
        };
        let mut env_payload = Vec::new();
        env.encode(&mut env_payload).unwrap();

        let frame = Frame {
            frame_type: FrameType::ControlGrpc,
            correlation_id,
            payload: Bytes::from(env_payload),
        };
        frame.write_to(stream).await.unwrap();
    }

    async fn recv_response<M: Message + Default, S: tokio::io::AsyncRead + Unpin>(
        stream: &mut S,
    ) -> (VsockEnvelope, Result<M, String>) {
        let frame = Frame::read_from(stream).await.unwrap();
        assert_eq!(frame.frame_type, FrameType::ControlGrpc);

        let env = VsockEnvelope::decode(&*frame.payload).unwrap();
        if let Some(err) = &env.error {
            let msg = err.message.clone();
            return (env, Err(msg));
        }

        let resp = M::decode(&*env.payload).unwrap();
        (env, Ok(resp))
    }

    /// A writer that notes, once, when the first byte is written.
    struct Recording {
        log: Arc<std::sync::Mutex<Vec<&'static str>>>,
        written: Vec<u8>,
    }

    impl tokio::io::AsyncWrite for Recording {
        fn poll_write(
            mut self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
            buf: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            if self.written.is_empty() {
                self.log.lock().unwrap().push("reply written");
            }
            self.written.extend_from_slice(buf);
            std::task::Poll::Ready(Ok(buf.len()))
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Ok(()))
        }
    }

    #[tokio::test]
    async fn the_disk_is_flushed_before_the_shutdown_reply_is_sent() {
        let log = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut out = Recording {
            log: log.clone(),
            written: Vec::new(),
        };
        let req = ShutdownRequest {
            reason: "session end".to_string(),
            grace_period: Some(prost_types::Duration {
                seconds: 0,
                nanos: 50_000_000,
            }),
        };
        let mut payload = Vec::new();
        req.encode(&mut payload).unwrap();
        let env = VsockEnvelope {
            method: "Shutdown".to_string(),
            correlation_id: 9,
            payload,
            error: None,
            trace_context: None,
        };
        let flush_log = log.clone();
        let (grace, reason) = answer_shutdown(&mut out, &env, move || {
            flush_log.lock().unwrap().push("disks flushed");
        })
        .await
        .unwrap();
        assert_eq!(*log.lock().unwrap(), vec!["disks flushed", "reply written"]);
        assert_eq!(grace, Duration::from_millis(50));
        assert_eq!(reason, "session end");
        // The reply is a well-formed ShutdownResponse frame.
        let frame = Frame::read_from(&mut &out.written[..]).await.unwrap();
        let envelope = VsockEnvelope::decode(&*frame.payload).unwrap();
        assert_eq!(envelope.method, "ShutdownResponse");
        assert_eq!(envelope.correlation_id, 9);
        assert!(
            ShutdownResponse::decode(&*envelope.payload)
                .unwrap()
                .accepted
        );
    }

    #[test]
    fn flushing_the_disks_is_safe_to_call() {
        flush_disks();
    }

    #[test]
    fn only_a_vm_guest_flushes_the_disks_of_its_machine() {
        // The process guest shares the host's kernel: it never calls sync(2).
        assert_eq!(FLUSHES_DISKS, !cfg!(feature = "process-guest"));
    }

    #[tokio::test]
    async fn test_rpc_ping() {
        let (mut client, server) = duplex(1024);
        let executor = Arc::new(LeasedTaskExecutor::new(7));

        let handle = tokio::spawn(handle_guest_rpc_connection(
            server,
            "test".to_string(),
            executor,
        ));

        let req = PingRequest {
            instance_id: None,
            sent_at: None,
        };
        send_request(&mut client, "Ping", 101, &req).await;

        let (env, res) = recv_response::<PingResponse, _>(&mut client).await;
        assert_eq!(env.correlation_id, 101);
        assert_eq!(env.method, "PingResponse");
        assert!(res.is_ok());
        let resp = res.unwrap();
        assert!(resp.received_at.is_some());

        drop(client);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_rpc_execute_tool_success() {
        let (mut client, server) = duplex(4096);
        let executor = Arc::new(LeasedTaskExecutor::new(7));

        let handle = tokio::spawn(handle_guest_rpc_connection(
            server,
            "test".to_string(),
            executor,
        ));

        // Create /work if we can
        let _ = std::fs::create_dir_all("/work");

        let mut args_struct = prost_types::Struct::default();
        args_struct.fields.insert(
            "args".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::ListValue(
                    prost_types::ListValue {
                        values: vec![prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(
                                "hello-rpc-test".to_string(),
                            )),
                        }],
                    },
                )),
            },
        );

        let req = ExecuteToolRequest {
            tool_name: "echo".to_string(),
            arguments: Some(args_struct),
            timeout: Some(prost_types::Duration {
                seconds: 5,
                nanos: 0,
            }),
            trace_context: None,
        };
        send_request(&mut client, "ExecuteTool", 102, &req).await;

        let (env, res) = recv_response::<ExecuteToolResponse, _>(&mut client).await;
        assert_eq!(env.correlation_id, 102);
        assert_eq!(env.method, "ExecuteToolResponse");
        if let Err(ref e) = res {
            panic!("test_rpc_execute_tool_success failed with: {}", e);
        }
        assert!(res.is_ok());

        let resp = res.unwrap();
        let tr = resp.result.unwrap();
        assert!(tr.error.is_none());

        let output = tr.output.unwrap();
        let stdout_val = output.fields.get("stdout").unwrap();
        if let Some(prost_types::value::Kind::StringValue(ref s)) = stdout_val.kind {
            assert!(s.contains("hello-rpc-test"));
        } else {
            panic!("Expected stdout string");
        }

        drop(client);
        let _ = handle.await;
    }

    #[tokio::test]
    async fn test_rpc_execute_tool_unimplemented() {
        let (mut client, server) = duplex(1024);
        let executor = Arc::new(LeasedTaskExecutor::new(7));

        let handle = tokio::spawn(handle_guest_rpc_connection(
            server,
            "test".to_string(),
            executor,
        ));

        let req = ExecuteToolRequest {
            tool_name: "nonexistent_tool".to_string(),
            arguments: None,
            timeout: None,
            trace_context: None,
        };
        send_request(&mut client, "ExecuteTool", 103, &req).await;

        let (env, res) = recv_response::<ExecuteToolResponse, _>(&mut client).await;
        assert_eq!(env.correlation_id, 103);
        assert!(res.is_err());
        let err_msg = res.unwrap_err();
        assert!(
            err_msg.contains("Unimplemented or forbidden tool"),
            "expected error message but got: {}",
            err_msg
        );

        drop(client);
        let _ = handle.await;
    }

    #[test]
    fn only_the_bake_vm_defers_its_background_vsock_tasks() {
        assert_eq!(
            post_registration_plan(true),
            PostRegistrationPlan {
                flush_loop: false,
                egress_connector: false
            }
        );
        assert_eq!(
            post_registration_plan(false),
            PostRegistrationPlan {
                flush_loop: true,
                egress_connector: true
            }
        );
    }

    #[test]
    fn a_test_process_is_not_the_bake_vm() {
        // `init::golden_bake` reads the kernel command line of the host running the tests.
        assert!(!init::golden_bake());
    }

    #[test]
    fn init_matches_the_cargo_feature() {
        // The default build is the PID-1 init and has no process-guest code.
        if cfg!(feature = "process-guest") {
            assert_eq!(init::MODE, "process");
        } else {
            assert_eq!(init::MODE, "pid1");
        }
    }
}
