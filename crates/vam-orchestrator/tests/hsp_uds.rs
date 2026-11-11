//! Integration tests for HSP gRPC over Unix domain sockets.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use prost_types::value::Kind;
use tempfile::tempdir;
use vam_orchestrator::error::OrchestratorError;
use vam_orchestrator::hsp::{HspClient, HspEndpoint};
use vam_orchestrator::hsp_echo::{spawn_echo_tcp, spawn_echo_unix};
use vam_spec::v1::{CapabilityManifest, InvokeRequest, RegisterRequest};

fn sample_register(broker_id: &str) -> RegisterRequest {
    RegisterRequest {
        broker_id: broker_id.to_string(),
        broker_version: "phase1".to_string(),
        manifest: Some(CapabilityManifest {
            broker_id: broker_id.to_string(),
            trust_tier: 2,
            secret_bindings: vec![],
            egress_domains: vec![],
            hooks: vec![],
            tools: vec![],
            depends_on: vec![],
            labels: Default::default(),
            identity_requirement: None,
            run_request_permission: None,
            notification_channel: None,
        }),
    }
}

fn sample_invoke(tool_name: &str, message: &str) -> InvokeRequest {
    InvokeRequest {
        session_id: "sess-1".to_string(),
        run_id: "run-1".to_string(),
        tool_name: tool_name.to_string(),
        args: Some(prost_types::Struct {
            fields: [(
                "message".to_string(),
                prost_types::Value {
                    kind: Some(Kind::StringValue(message.to_string())),
                },
            )]
            .into(),
        }),
        resolved_secrets: Default::default(),
        context: None,
        trace_context: None,
        caller_context: None,
        run_grant: None,
        idempotency_key: String::new(),
    }
}

#[tokio::test]
async fn uds_register_invoke_round_trip() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("echo.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();

    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();

    let register = client.register(sample_register("echo")).await.unwrap();
    assert!(register.error.is_none());

    let invoke = client
        .invoke(sample_invoke("echo.ping", "hello"))
        .await
        .unwrap();
    let output = invoke.result.unwrap().output.unwrap();
    let tool_name = output.fields.get("tool_name").unwrap();
    assert!(matches!(
        tool_name.kind,
        Some(Kind::StringValue(ref name)) if name == "echo.ping"
    ));
    let args = output.fields.get("args").unwrap();
    let args_struct = match args.kind {
        Some(Kind::StructValue(ref s)) => s,
        _ => panic!("expected struct args"),
    };
    let message = args_struct.fields.get("message").unwrap();
    assert!(matches!(
        message.kind,
        Some(Kind::StringValue(ref value)) if value == "hello"
    ));
}

#[tokio::test]
async fn uds_socket_acl_deny_fails_fast() {
    // Root ignores file permissions (CAP_DAC_OVERRIDE), so a 0o000 socket does not stop
    // its connect and this test cannot observe the denial. Cloud sessions run as root;
    // GitHub Actions runs as a normal user and keeps checking the real behaviour.
    // SAFETY: geteuid has no preconditions and cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("skipped: running as root, which ignores the socket's 0o000 permissions");
        return;
    }
    let dir = tempdir().unwrap();
    let sock = dir.path().join("locked.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();

    std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o000)).unwrap();

    let started = Instant::now();
    let result =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2)).await;
    let elapsed = started.elapsed();

    assert!(result.is_err());
    assert!(
        elapsed < Duration::from_secs(2),
        "connect should fail fast, took {:?}",
        elapsed
    );
    assert!(matches!(result.unwrap_err(), OrchestratorError::Hsp(_)));
}

#[tokio::test]
async fn uds_broker_crash_fail_closed() {
    let dir = tempdir().unwrap();
    let sock = dir.path().join("crash.sock");
    let broker_bin = env!("CARGO_BIN_EXE_hsp-echo-broker");

    let mut child = Command::new(broker_bin)
        .arg("--socket")
        .arg(&sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn hsp-echo-broker");

    // Wait until the broker accepts gRPC connections.
    let mut client = None;
    for _ in 0..100 {
        match HspClient::connect_with_timeout(
            HspEndpoint::Unix(sock.clone()),
            Duration::from_millis(500),
        )
        .await
        {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
        }
    }
    let mut client = client.expect("broker did not become ready");
    client.register(sample_register("echo")).await.unwrap();

    child.kill().expect("kill broker process");
    let _ = child.wait();

    let started = Instant::now();
    let result = client
        .invoke(sample_invoke("echo.ping", "after-crash"))
        .await;
    let elapsed = started.elapsed();

    assert!(result.is_err(), "invoke after crash must error");
    assert!(
        elapsed < Duration::from_secs(2),
        "invoke must not hang after broker crash, took {:?}",
        elapsed
    );
    assert!(matches!(result.unwrap_err(), OrchestratorError::Hsp(_)));
}

#[tokio::test]
async fn tcp_register_invoke_round_trip() {
    let (addr, _handle) = spawn_echo_tcp("127.0.0.1:0".parse::<SocketAddr>().unwrap())
        .await
        .unwrap();

    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Tcp(addr), Duration::from_secs(2))
            .await
            .unwrap();

    client.register(sample_register("echo-tcp")).await.unwrap();
    let invoke = client
        .invoke(sample_invoke("echo.tcp", "via-tcp"))
        .await
        .unwrap();
    let output = invoke.result.unwrap().output.unwrap();
    let tool_name = output.fields.get("tool_name").unwrap();
    assert!(matches!(
        tool_name.kind,
        Some(Kind::StringValue(ref name)) if name == "echo.tcp"
    ));
}
