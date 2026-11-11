use bytes::Bytes;
use prost::Message;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;

use vam_spec::v1::{
    ExecuteTaskRequest, ExecuteTaskResponse, ExecutionLease, PromptAction, ShutdownRequest,
    ShutdownResponse, TaskOutcome, VsockEnvelope,
};
use vam_vsock::frame::{Frame, FrameType};
use vam_vsock::host_dial;

/// Connects to the guest agent at vsock_uds_path port 52, sends an ExecuteTaskRequest,
/// and awaits the ExecuteTaskResponse containing the task outcome.
///
/// The caller must issue the lease on the workflow store before invoking this function.
pub async fn run_task_on_guest<P: AsRef<Path>>(
    vsock_uds_path: P,
    lease: &ExecutionLease,
    prompt_text: &str,
    inputs: Option<prost_types::Struct>,
    timeout_secs: u64,
) -> Result<TaskOutcome, String> {
    run_task_on_guest_with_trace(
        vsock_uds_path,
        lease,
        prompt_text,
        inputs,
        timeout_secs,
        None,
    )
    .await
}

/// Why an `ExecuteTask` exchange ended without an outcome. The kind decides what the host does
/// next (`resume::classify`): only [`GuestTaskError::Transport`] can be a crash.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestTaskError {
    /// The guest could not be reached; the request never left. Not a crash of a task.
    Dial(String),
    /// The host stopped waiting (`RunSpec.max_duration`). Not a crash.
    Timeout(String),
    /// The connection to the guest failed after the request was written (it died, or the link
    /// dropped). A crash unless a stop was recorded.
    Transport(String),
    /// The guest answered with an error of its own. A reported end, not a crash.
    Reported(String),
    /// The guest's answer was not a valid `ExecuteTaskResponse`. Not a crash.
    Protocol(String),
}

impl GuestTaskError {
    pub fn message(&self) -> &str {
        match self {
            Self::Dial(m)
            | Self::Timeout(m)
            | Self::Transport(m)
            | Self::Reported(m)
            | Self::Protocol(m) => m,
        }
    }
}

impl std::fmt::Display for GuestTaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.message())
    }
}

impl From<GuestTaskError> for String {
    fn from(err: GuestTaskError) -> String {
        err.message().to_string()
    }
}

/// Connects to the guest agent with an optional parent TraceContext for end-to-end distributed tracing.
pub async fn run_task_on_guest_with_trace<P: AsRef<Path>>(
    vsock_uds_path: P,
    lease: &ExecutionLease,
    prompt_text: &str,
    inputs: Option<prost_types::Struct>,
    timeout_secs: u64,
    trace_context: Option<vam_spec::v1::TraceContext>,
) -> Result<TaskOutcome, String> {
    run_task_on_guest_classified(
        vsock_uds_path,
        lease,
        prompt_text,
        inputs,
        timeout_secs,
        trace_context,
    )
    .await
    .map_err(String::from)
}

/// [`run_task_on_guest_with_trace`] with the failure kept typed.
pub async fn run_task_on_guest_classified<P: AsRef<Path>>(
    vsock_uds_path: P,
    lease: &ExecutionLease,
    prompt_text: &str,
    inputs: Option<prost_types::Struct>,
    timeout_secs: u64,
    trace_context: Option<vam_spec::v1::TraceContext>,
) -> Result<TaskOutcome, GuestTaskError> {
    let mut guest_conn = None;
    for _ in 0..30 {
        if let Ok(stream) = host_dial(vsock_uds_path.as_ref(), 52).await {
            guest_conn = Some(stream);
            break;
        }
        sleep(Duration::from_millis(200)).await;
    }

    let mut guest_conn = guest_conn.ok_or_else(|| {
        GuestTaskError::Dial(format!(
            "Failed to dial guest vsock Port 52 at {}",
            vsock_uds_path.as_ref().display()
        ))
    })?;

    let req = ExecuteTaskRequest {
        lease: Some(lease.clone()),
        task_action: Some(
            vam_spec::v1::execute_task_request::TaskAction::PromptAction(PromptAction {
                instruction_template: prompt_text.to_string(),
                inject_context_keys: vec![],
            }),
        ),
        inputs,
        timeout: Some(prost_types::Duration {
            seconds: timeout_secs as i64,
            nanos: 0,
        }),
        agent_ref: None,
        trace_context: trace_context.clone(),
    };

    let mut payload = Vec::new();
    req.encode(&mut payload)
        .map_err(|e| GuestTaskError::Protocol(format!("Failed to encode request: {}", e)))?;

    let env = VsockEnvelope {
        method: "ExecuteTask".to_string(),
        correlation_id: 102,
        payload,
        error: None,
        trace_context,
    };

    let mut env_payload = Vec::new();
    env.encode(&mut env_payload)
        .map_err(|e| GuestTaskError::Protocol(format!("Failed to encode envelope: {}", e)))?;

    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: 102,
        payload: Bytes::from(env_payload),
    };

    frame
        .write_to(&mut guest_conn)
        .await
        .map_err(|e| GuestTaskError::Transport(format!("Failed to write frame: {}", e)))?;

    // Enforce RunSpec.max_duration on the host wait; the request timeout alone does
    // not cut off an indefinitely hanging guest Frame::read_from.
    let frame_in = match tokio::time::timeout(
        Duration::from_secs(timeout_secs),
        Frame::read_from(&mut guest_conn),
    )
    .await
    {
        Ok(Ok(frame)) => frame,
        Ok(Err(e)) => {
            return Err(GuestTaskError::Transport(format!(
                "Failed to read frame: {}",
                e
            )))
        }
        Err(_) => {
            return Err(GuestTaskError::Timeout(format!(
                "Guest task timed out after {}s (RunSpec.max_duration)",
                timeout_secs
            )));
        }
    };
    if frame_in.frame_type != FrameType::ControlGrpc {
        return Err(GuestTaskError::Protocol(format!(
            "Unexpected frame type from guest response: {:?}",
            frame_in.frame_type
        )));
    }

    let env_in = VsockEnvelope::decode(&*frame_in.payload)
        .map_err(|e| GuestTaskError::Protocol(format!("Failed to decode envelope: {}", e)))?;
    if env_in.method != "ExecuteTaskResponse" {
        return Err(GuestTaskError::Protocol(format!(
            "Expected ExecuteTaskResponse method, got '{}'",
            env_in.method
        )));
    }

    if let Some(err) = env_in.error {
        return Err(GuestTaskError::Reported(format!(
            "ExecuteTask failed with guest error (code {}): {}",
            err.code, err.message
        )));
    }

    let response = ExecuteTaskResponse::decode(&*env_in.payload)
        .map_err(|e| GuestTaskError::Protocol(format!("Failed to decode response: {}", e)))?;
    response.outcome.ok_or_else(|| {
        GuestTaskError::Protocol("Missing outcome in ExecuteTaskResponse".to_string())
    })
}

/// Sends the guest `Shutdown` RPC and waits for the grace period so the guest can flush Drive9.
pub async fn request_guest_shutdown<P: AsRef<Path>>(
    vsock_uds_path: P,
    grace_secs: u64,
) -> Result<(), String> {
    let mut stream = host_dial(vsock_uds_path.as_ref(), 52)
        .await
        .map_err(|e| format!("Failed to dial guest vsock Port 52: {}", e))?;

    let req = ShutdownRequest {
        reason: "host terminate".to_string(),
        grace_period: Some(prost_types::Duration {
            seconds: grace_secs as i64,
            nanos: 0,
        }),
    };

    let mut payload = Vec::new();
    req.encode(&mut payload)
        .map_err(|e| format!("Failed to encode ShutdownRequest: {}", e))?;

    let env = VsockEnvelope {
        method: "Shutdown".to_string(),
        correlation_id: 201,
        payload,
        error: None,
        trace_context: None,
    };

    let mut env_payload = Vec::new();
    env.encode(&mut env_payload)
        .map_err(|e| format!("Failed to encode envelope: {}", e))?;

    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: 201,
        payload: Bytes::from(env_payload),
    };

    frame
        .write_to(&mut stream)
        .await
        .map_err(|e| format!("Failed to write Shutdown frame: {}", e))?;

    let frame_in = tokio::time::timeout(
        Duration::from_secs(grace_secs + 2),
        Frame::read_from(&mut stream),
    )
    .await
    .map_err(|_| "Timed out waiting for ShutdownResponse".to_string())?
    .map_err(|e| format!("Failed to read ShutdownResponse frame: {}", e))?;

    if frame_in.frame_type != FrameType::ControlGrpc {
        return Err(format!(
            "Unexpected frame type from guest Shutdown response: {:?}",
            frame_in.frame_type
        ));
    }

    let env_in = VsockEnvelope::decode(&*frame_in.payload)
        .map_err(|e| format!("Failed to decode Shutdown envelope: {}", e))?;

    if let Some(err) = env_in.error {
        return Err(format!(
            "Shutdown failed with guest error (code {}): {}",
            err.code, err.message
        ));
    }

    let _response = ShutdownResponse::decode(&*env_in.payload)
        .map_err(|e| format!("Failed to decode ShutdownResponse: {}", e))?;

    sleep(Duration::from_millis(grace_secs * 1000 + 500)).await;
    Ok(())
}

/// Longest wait for the restored guest to accept the connection and answer
/// `ResumeFromSnapshot`. A healthy restore answers in under 100 ms; the 30 s is
/// slack for a slow runner. A guest that never answers (a bad bake) is the only
/// case that waits it out, then the session boots cold.
pub const RESEED_TIMEOUT: Duration = Duration::from_secs(30);

/// Env override for [`RESEED_TIMEOUT`], in whole seconds.
pub const RESEED_TIMEOUT_ENV: &str = "VAM_RESEED_TIMEOUT_SECS";

/// The reseed timeout: `VAM_RESEED_TIMEOUT_SECS` when it is a positive integer,
/// else [`RESEED_TIMEOUT`].
pub fn reseed_timeout() -> Duration {
    std::env::var(RESEED_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(RESEED_TIMEOUT)
}

/// Re-seeds guest entropy, clock, and lease generation after golden snapshot restore.
pub async fn reseed_guest_after_snapshot<P: AsRef<Path>>(
    vsock_uds_path: P,
    lease_min_generation: i64,
) -> Result<(), String> {
    reseed_with_timeout(vsock_uds_path, lease_min_generation, reseed_timeout()).await
}

/// [`reseed_guest_after_snapshot`] with an explicit timeout, applied to the dial
/// (the `CONNECT` answer) and, separately, to the response.
pub async fn reseed_with_timeout<P: AsRef<Path>>(
    vsock_uds_path: P,
    lease_min_generation: i64,
    timeout: Duration,
) -> Result<(), String> {
    use rand::RngCore;
    let t_dial = std::time::Instant::now();
    let mut stream = tokio::time::timeout(timeout, host_dial(vsock_uds_path.as_ref(), 52))
        .await
        .map_err(|_| "Timed out dialing guest vsock Port 52 for reseed".to_string())?
        .map_err(|e| format!("Failed to dial guest vsock Port 52 for reseed: {}", e))?;
    let dial_ms = t_dial.elapsed().as_millis();

    let mut entropy = [0u8; 256];
    rand::thread_rng().fill_bytes(&mut entropy);
    let clock_unix_nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);

    let req = vam_spec::v1::ResumeFromSnapshotRequest {
        entropy: entropy.to_vec(),
        clock_unix_nanos,
        lease_min_generation,
    };

    let mut payload = Vec::new();
    req.encode(&mut payload)
        .map_err(|e| format!("Failed to encode ResumeFromSnapshotRequest: {}", e))?;

    let env = VsockEnvelope {
        method: "ResumeFromSnapshot".to_string(),
        correlation_id: 301,
        payload,
        error: None,
        trace_context: None,
    };

    let mut env_payload = Vec::new();
    env.encode(&mut env_payload)
        .map_err(|e| format!("Failed to encode envelope: {}", e))?;

    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: 301,
        payload: Bytes::from(env_payload),
    };

    let t_write = std::time::Instant::now();
    frame
        .write_to(&mut stream)
        .await
        .map_err(|e| format!("Failed to write ResumeFromSnapshot frame: {}", e))?;
    let write_ms = t_write.elapsed().as_millis();

    let t_resp = std::time::Instant::now();
    let frame_in = tokio::time::timeout(timeout, Frame::read_from(&mut stream))
        .await
        .map_err(|_| "Timed out waiting for ResumeFromSnapshotResponse".to_string())?
        .map_err(|e| format!("Failed to read ResumeFromSnapshotResponse: {}", e))?;
    eprintln!(
        "[orchestrator] reseed timing: dial={}ms write={}ms response={}ms",
        dial_ms,
        write_ms,
        t_resp.elapsed().as_millis()
    );

    if frame_in.frame_type != FrameType::ControlGrpc {
        return Err(format!(
            "Unexpected frame type from guest reseed response: {:?}",
            frame_in.frame_type
        ));
    }

    let env_in = VsockEnvelope::decode(&*frame_in.payload)
        .map_err(|e| format!("Failed to decode reseed envelope: {}", e))?;

    if let Some(err) = env_in.error {
        return Err(format!(
            "ResumeFromSnapshot failed (code {}): {}",
            err.code, err.message
        ));
    }

    if env_in.method != "ResumeFromSnapshotResponse" {
        return Err(format!(
            "Expected ResumeFromSnapshotResponse, got '{}'",
            env_in.method
        ));
    }

    let response = vam_spec::v1::ResumeFromSnapshotResponse::decode(&*env_in.payload)
        .map_err(|e| format!("Failed to decode ResumeFromSnapshotResponse: {}", e))?;

    if !response.accepted {
        return Err("guest rejected ResumeFromSnapshot".to_string());
    }

    Ok(())
}

/// Longest the golden bake waits for the guest to confirm it is ready to be
/// snapshotted (`PrepareForSnapshot`), dial, reply and close together.
pub const BAKE_READY_TIMEOUT: Duration = Duration::from_secs(20);

/// Env override for [`BAKE_READY_TIMEOUT`], in whole seconds.
pub const BAKE_READY_TIMEOUT_ENV: &str = "VAM_BAKE_READY_TIMEOUT_SECS";

/// The bake's ready timeout: `VAM_BAKE_READY_TIMEOUT_SECS` when it is a positive
/// integer, else [`BAKE_READY_TIMEOUT`].
pub fn bake_ready_timeout() -> Duration {
    std::env::var(BAKE_READY_TIMEOUT_ENV)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(BAKE_READY_TIMEOUT)
}

/// What [`prepare_guest_for_snapshot`] saw, for the bake's timing line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PrepareTiming {
    pub dial_ms: u128,
    pub reply_ms: u128,
    /// False when the guest answered but its end never closed within the wait. The
    /// bake still goes on (a short settle replaces the EOF) and logs it.
    pub eof_seen: bool,
}

/// How long the host waits for the guest's EOF after its ready reply.
const PREPARE_EOF_WAIT: Duration = Duration::from_secs(2);

/// Asks the golden bake guest to confirm it is quiescent: the guest answers from
/// its accept loop (so no dial is outstanding and, in the bake VM, no flush loop or
/// egress connector exists), then closes its end. Returns once the guest's close
/// reached the host, so the caller can pause the VM with no connection mid
/// teardown. The whole exchange is bounded by `timeout`; a guest that never
/// answers is an error, and the bake must then be thrown away.
pub async fn prepare_guest_for_snapshot<P: AsRef<Path>>(
    vsock_uds_path: P,
    timeout: Duration,
) -> Result<PrepareTiming, String> {
    use tokio::io::AsyncReadExt;
    let deadline = tokio::time::Instant::now() + timeout;
    let remaining = |what: &str| -> Result<Duration, String> {
        deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|d| !d.is_zero())
            .ok_or_else(|| format!("Timed out {what} (PrepareForSnapshot, {timeout:?})"))
    };

    // The guest listener exists from before registration, but a refused dial (the
    // mux is still coming up) is retried until the deadline.
    let t_dial = std::time::Instant::now();
    let mut stream = loop {
        let left = remaining("dialing the guest")?;
        match tokio::time::timeout(left, host_dial(vsock_uds_path.as_ref(), 52)).await {
            Ok(Ok(s)) => break s,
            Ok(Err(_)) => sleep(Duration::from_millis(50)).await,
            Err(_) => {
                return Err(format!(
                    "Timed out dialing the guest (PrepareForSnapshot, {timeout:?})"
                ))
            }
        }
    };
    let dial_ms = t_dial.elapsed().as_millis();

    let env = VsockEnvelope {
        method: "PrepareForSnapshot".to_string(),
        correlation_id: 302,
        payload: Vec::new(),
        error: None,
        trace_context: None,
    };
    let mut env_payload = Vec::new();
    env.encode(&mut env_payload)
        .map_err(|e| format!("Failed to encode envelope: {}", e))?;
    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: 302,
        payload: Bytes::from(env_payload),
    };
    frame
        .write_to(&mut stream)
        .await
        .map_err(|e| format!("Failed to write PrepareForSnapshot frame: {}", e))?;

    let t_reply = std::time::Instant::now();
    let left = remaining("waiting for the guest's ready reply")?;
    let frame_in = tokio::time::timeout(left, Frame::read_from(&mut stream))
        .await
        .map_err(|_| format!("Timed out waiting for PrepareForSnapshotResponse ({timeout:?})"))?
        .map_err(|e| format!("Failed to read PrepareForSnapshotResponse: {}", e))?;
    let reply_ms = t_reply.elapsed().as_millis();
    if frame_in.frame_type != FrameType::ControlGrpc {
        return Err(format!(
            "Unexpected frame type from guest prepare response: {:?}",
            frame_in.frame_type
        ));
    }
    let env_in = VsockEnvelope::decode(&*frame_in.payload)
        .map_err(|e| format!("Failed to decode prepare envelope: {}", e))?;
    if let Some(err) = env_in.error {
        return Err(format!(
            "PrepareForSnapshot refused (code {}): {}",
            err.code, err.message
        ));
    }
    if env_in.method != "PrepareForSnapshotResponse" {
        return Err(format!(
            "Expected PrepareForSnapshotResponse, got '{}'",
            env_in.method
        ));
    }

    // The guest closes its end right after replying. EOF (or a reset) here means
    // that close has reached the host.
    let eof_wait =
        PREPARE_EOF_WAIT.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
    let mut buf = [0u8; 64];
    let eof_seen = tokio::time::timeout(eof_wait, async {
        loop {
            match stream.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    })
    .await
    .is_ok();
    // Settle: the guest has issued its close and has nothing left to run.
    sleep(Duration::from_millis(if eof_seen { 20 } else { 100 })).await;
    Ok(PrepareTiming {
        dial_ms,
        reply_ms,
        eof_seen,
    })
}

#[cfg(test)]
mod reseed_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;
    use vam_spec::v1::ResumeFromSnapshotResponse;

    #[derive(Clone, Copy)]
    enum Mux {
        /// Answers `OK 52`, then replies after the delay.
        ReplyAfter(Duration),
        /// Answers `OK 52`, then never replies.
        NoReply,
        /// Never answers the `CONNECT` line.
        NoOk,
    }

    /// A stand-in for Firecracker's vsock mux plus the guest's reseed handler.
    fn spawn_fake_mux(dir: &tempfile::TempDir, mode: Mux) -> std::path::PathBuf {
        let path = dir.path().join("vsock.socket");
        let listener = UnixListener::bind(&path).expect("bind fake mux");
        tokio::spawn(async move {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut line = [0u8; 1];
            loop {
                if s.read_exact(&mut line).await.is_err() || line[0] == b'\n' {
                    break;
                }
            }
            if matches!(mode, Mux::NoOk) {
                tokio::time::sleep(Duration::from_secs(60)).await;
                return;
            }
            let _ = s.write_all(b"OK 52\n").await;
            let Ok(req) = Frame::read_from(&mut s).await else {
                return;
            };
            match mode {
                Mux::ReplyAfter(d) => tokio::time::sleep(d).await,
                _ => {
                    tokio::time::sleep(Duration::from_secs(60)).await;
                    return;
                }
            }
            let mut payload = Vec::new();
            ResumeFromSnapshotResponse { accepted: true }
                .encode(&mut payload)
                .unwrap();
            let mut env = Vec::new();
            VsockEnvelope {
                method: "ResumeFromSnapshotResponse".to_string(),
                correlation_id: req.correlation_id,
                payload,
                error: None,
                trace_context: None,
            }
            .encode(&mut env)
            .unwrap();
            let _ = Frame {
                frame_type: FrameType::ControlGrpc,
                correlation_id: req.correlation_id,
                payload: Bytes::from(env),
            }
            .write_to(&mut s)
            .await;
        });
        path
    }

    #[tokio::test]
    async fn slow_guest_reply_is_accepted_within_the_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = spawn_fake_mux(&dir, Mux::ReplyAfter(Duration::from_millis(1500)));
        let r = reseed_with_timeout(&path, 1, Duration::from_secs(5)).await;
        assert!(r.is_ok(), "{r:?}");
    }

    #[tokio::test]
    async fn silent_guest_times_out_on_the_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = spawn_fake_mux(&dir, Mux::NoReply);
        let r = reseed_with_timeout(&path, 1, Duration::from_millis(300)).await;
        assert_eq!(
            r.unwrap_err(),
            "Timed out waiting for ResumeFromSnapshotResponse"
        );
    }

    #[tokio::test]
    async fn dial_without_ok_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = spawn_fake_mux(&dir, Mux::NoOk);
        let r = reseed_with_timeout(&path, 1, Duration::from_millis(300)).await;
        assert_eq!(
            r.unwrap_err(),
            "Timed out dialing guest vsock Port 52 for reseed"
        );
    }

    #[test]
    fn reseed_timeout_defaults_and_honours_the_env_override() {
        // One test owns the variable so parallel tests cannot race on it.
        std::env::remove_var(RESEED_TIMEOUT_ENV);
        assert_eq!(reseed_timeout(), RESEED_TIMEOUT);
        std::env::set_var(RESEED_TIMEOUT_ENV, "45");
        assert_eq!(reseed_timeout(), Duration::from_secs(45));
        std::env::set_var(RESEED_TIMEOUT_ENV, "0");
        assert_eq!(reseed_timeout(), RESEED_TIMEOUT);
        std::env::set_var(RESEED_TIMEOUT_ENV, "soon");
        assert_eq!(reseed_timeout(), RESEED_TIMEOUT);
        std::env::remove_var(RESEED_TIMEOUT_ENV);
    }
}

#[cfg(test)]
mod prepare_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::UnixListener;

    #[derive(Clone, Copy)]
    enum Guest {
        /// `OK 52`, reply, close.
        ReplyAndClose,
        /// `OK 52`, reply, keep the connection open.
        ReplyKeepOpen,
        /// `OK 52`, then never reply.
        Silent,
        /// Never answers the `CONNECT` line.
        NoOk,
        /// `OK 52`, then an error envelope.
        Refuse,
    }

    fn fake_mux(dir: &tempfile::TempDir, mode: Guest) -> std::path::PathBuf {
        let path = dir.path().join("vsock.socket");
        let listener = UnixListener::bind(&path).expect("bind fake mux");
        tokio::spawn(async move {
            let Ok((mut s, _)) = listener.accept().await else {
                return;
            };
            let mut b = [0u8; 1];
            loop {
                if s.read_exact(&mut b).await.is_err() || b[0] == b'\n' {
                    break;
                }
            }
            if matches!(mode, Guest::NoOk) {
                tokio::time::sleep(Duration::from_secs(60)).await;
                return;
            }
            let _ = s.write_all(b"OK 52\n").await;
            let Ok(req) = Frame::read_from(&mut s).await else {
                return;
            };
            let env_in = VsockEnvelope::decode(&*req.payload).unwrap();
            assert_eq!(env_in.method, "PrepareForSnapshot");
            assert!(env_in.payload.is_empty());
            if matches!(mode, Guest::Silent) {
                tokio::time::sleep(Duration::from_secs(60)).await;
                return;
            }
            let error = matches!(mode, Guest::Refuse).then(|| vam_spec::v1::ErrorStatus {
                code: 12,
                message: "not a bake guest".to_string(),
                details: Vec::new(),
                retryable: false,
            });
            let mut env = Vec::new();
            VsockEnvelope {
                method: "PrepareForSnapshotResponse".to_string(),
                correlation_id: req.correlation_id,
                payload: Vec::new(),
                error,
                trace_context: None,
            }
            .encode(&mut env)
            .unwrap();
            let _ = Frame {
                frame_type: FrameType::ControlGrpc,
                correlation_id: req.correlation_id,
                payload: Bytes::from(env),
            }
            .write_to(&mut s)
            .await;
            if matches!(mode, Guest::ReplyKeepOpen) {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            // ReplyAndClose and Refuse: dropping `s` closes it.
        });
        path
    }

    #[tokio::test]
    async fn guest_that_replies_and_closes_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_mux(&dir, Guest::ReplyAndClose);
        let t = prepare_guest_for_snapshot(&path, Duration::from_secs(5))
            .await
            .unwrap();
        assert!(t.eof_seen, "the guest's close must be seen: {t:?}");
    }

    #[tokio::test]
    async fn guest_that_replies_but_stays_connected_is_ready_without_eof() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_mux(&dir, Guest::ReplyKeepOpen);
        // The reply is what counts; the missing EOF is reported, not fatal. The wait
        // for EOF is bounded by the overall timeout.
        let t = prepare_guest_for_snapshot(&path, Duration::from_millis(900))
            .await
            .unwrap();
        assert!(!t.eof_seen);
    }

    #[tokio::test]
    async fn silent_guest_times_out_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_mux(&dir, Guest::Silent);
        let started = std::time::Instant::now();
        let err = prepare_guest_for_snapshot(&path, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(
            err.contains("Timed out waiting for PrepareForSnapshotResponse"),
            "{err}"
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[tokio::test]
    async fn dial_without_ok_times_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_mux(&dir, Guest::NoOk);
        let err = prepare_guest_for_snapshot(&path, Duration::from_millis(300))
            .await
            .unwrap_err();
        assert!(err.contains("Timed out dialing the guest"), "{err}");
    }

    #[tokio::test]
    async fn missing_mux_times_out_instead_of_spinning() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            prepare_guest_for_snapshot(dir.path().join("none.socket"), Duration::from_millis(300))
                .await
                .unwrap_err();
        assert!(err.contains("Timed out dialing the guest"), "{err}");
    }

    #[tokio::test]
    async fn error_reply_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = fake_mux(&dir, Guest::Refuse);
        let err = prepare_guest_for_snapshot(&path, Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.contains("PrepareForSnapshot refused"), "{err}");
    }

    #[test]
    fn bake_ready_timeout_defaults_and_honours_the_env_override() {
        // One test owns the variable so parallel tests cannot race on it.
        std::env::remove_var(BAKE_READY_TIMEOUT_ENV);
        assert_eq!(bake_ready_timeout(), BAKE_READY_TIMEOUT);
        std::env::set_var(BAKE_READY_TIMEOUT_ENV, "7");
        assert_eq!(bake_ready_timeout(), Duration::from_secs(7));
        std::env::set_var(BAKE_READY_TIMEOUT_ENV, "0");
        assert_eq!(bake_ready_timeout(), BAKE_READY_TIMEOUT);
        std::env::remove_var(BAKE_READY_TIMEOUT_ENV);
    }
}
