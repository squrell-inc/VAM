//! The real `vam-runtime` binary built with `process-guest`, run as a host
//! child process. Compiled only with the feature (test-only build).
#![cfg(all(feature = "process-guest", unix))]

use prost::Message;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use vam_spec::v1::{ResumeFromSnapshotRequest, ShutdownRequest, VsockEnvelope};
use vam_vsock::frame::{Frame, FrameType};
use vam_vsock::{
    handle_registration_handshake, host_dial, host_ping, ChallengeRegistry,
    DevChallengeOnlyVerifier, HostListener,
};

const INSTANCE: &str = "urn:vam:agent:process-guest-test";

fn guest(envs: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vam-runtime"));
    cmd.env_clear()
        .env("PATH", "/usr/bin:/bin")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    cmd
}

fn wait_exit(child: &mut Child, within: Duration) -> Option<i32> {
    let start = Instant::now();
    while start.elapsed() < within {
        if let Ok(Some(status)) = child.try_wait() {
            return status.code();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = child.kill();
    None
}

#[test]
fn refuses_to_start_without_its_transport_variable() {
    let mut child = guest(&[]).spawn().unwrap();
    assert_eq!(wait_exit(&mut child, Duration::from_secs(10)), Some(2));
}

#[test]
fn refuses_a_missing_directory_and_a_wildcard_egress_address() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("gone/vsock.socket");
    let mut child = guest(&[
        ("VAM_GUEST_VSOCK_UDS", base.to_str().unwrap()),
        ("VAM_INSTANCE_ID", INSTANCE),
        ("VAM_GUEST_CID", "42"),
    ])
    .spawn()
    .unwrap();
    assert_eq!(wait_exit(&mut child, Duration::from_secs(10)), Some(2));

    let base = dir.path().join("vsock.socket");
    let sock = dir.path().join("egress.sock");
    let mut child = guest(&[
        ("VAM_GUEST_VSOCK_UDS", base.to_str().unwrap()),
        ("VAM_INSTANCE_ID", INSTANCE),
        ("VAM_GUEST_CID", "42"),
        ("VAM_EGRESS_PROXY_SOCK", sock.to_str().unwrap()),
        ("VAM_EGRESS_PROXY_ADDR", "0.0.0.0:3128"),
    ])
    .spawn()
    .unwrap();
    assert_eq!(wait_exit(&mut child, Duration::from_secs(10)), Some(2));
    assert!(!base.exists(), "a refused guest binds nothing");
}

async fn rpc(
    base: &std::path::Path,
    method: &str,
    payload: Vec<u8>,
) -> Result<VsockEnvelope, String> {
    let mut stream = host_dial(base, 52).await.map_err(|e| e.to_string())?;
    let env = VsockEnvelope {
        method: method.to_string(),
        correlation_id: 9,
        payload,
        error: None,
        trace_context: None,
    };
    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: 9,
        payload: env.encode_to_vec().into(),
    };
    frame
        .write_to(&mut stream)
        .await
        .map_err(|e| e.to_string())?;
    let resp = Frame::read_from(&mut stream)
        .await
        .map_err(|e| e.to_string())?;
    let _ = stream.shutdown().await;
    let mut sink = Vec::new();
    let _ = stream.read_to_end(&mut sink).await;
    VsockEnvelope::decode(&*resp.payload).map_err(|e| e.to_string())
}

#[tokio::test]
async fn registers_over_uds_answers_rpcs_and_exits_on_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("vam-vsock-test.socket");
    let host_listener = HostListener::bind(&base, 52).await.unwrap();

    let mut child = guest(&[
        ("VAM_GUEST_VSOCK_UDS", base.to_str().unwrap()),
        ("VAM_INSTANCE_ID", INSTANCE),
        ("VAM_GUEST_CID", "42"),
        ("VAM_WORKSPACE_ROOT", dir.path().to_str().unwrap()),
    ])
    .spawn()
    .unwrap();

    let reg = tokio::time::timeout(Duration::from_secs(10), host_listener.accept())
        .await
        .expect("guest dialed <base>_52")
        .unwrap();
    handle_registration_handshake(
        reg,
        INSTANCE,
        42,
        &ChallengeRegistry::new(),
        &DevChallengeOnlyVerifier,
    )
    .await
    .unwrap();

    // Host to guest through <base> with CONNECT 52, the Firecracker layout.
    let stream = host_dial(&base, 52).await.unwrap();
    host_ping(stream, 1, INSTANCE, Duration::from_secs(2))
        .await
        .unwrap();

    // Snapshot restore (mounts, entropy, clock) does not exist in this build.
    let resp = rpc(
        &base,
        "ResumeFromSnapshot",
        ResumeFromSnapshotRequest {
            entropy: vec![0; 256],
            ..Default::default()
        }
        .encode_to_vec(),
    )
    .await
    .unwrap();
    assert_eq!(resp.error.map(|e| e.code), Some(12), "UNIMPLEMENTED");

    let shutdown = ShutdownRequest {
        reason: "test".to_string(),
        grace_period: Some(prost_types::Duration {
            seconds: 0,
            nanos: 0,
        }),
    };
    let resp = rpc(&base, "Shutdown", shutdown.encode_to_vec())
        .await
        .unwrap();
    assert!(resp.error.is_none());
    assert_eq!(wait_exit(&mut child, Duration::from_secs(10)), Some(0));
    assert!(!base.exists(), "the guest removes its own socket on exit");
}
