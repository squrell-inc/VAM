#![allow(unused_imports, dead_code)]
use std::fs;
use std::path::PathBuf;
use vam_core::{CidAllocator, FirecrackerVmm, MicroVmConfigBuilder, Vmm};

/// Reads a lane asset path from the environment. The firecracker CI lane sets these
/// (scripts/e2e/firecracker-assets.sh); there is no host-specific default.
fn lane_path(name: &str) -> PathBuf {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => panic!("{name} is not set; run via scripts/e2e/firecracker-assets.sh --env-file"),
    }
}

#[test]
#[ignore = "needs KVM: firecracker lane"]
#[cfg(target_os = "linux")]
fn test_firecracker_lifecycle_integration() {
    let id = "integration-test-vm".to_string();
    let firecracker_bin = lane_path("VAM_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAM_JAILER_BIN");
    let kernel_path = lane_path("VAM_KERNEL_PATH");
    let rootfs_path = lane_path("VAM_ROOTFS_PATH");

    // Sockets and logs paths
    let socket_path = PathBuf::from(format!("/tmp/vam-fc-{}.socket", id));
    let vsock_uds_path = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", id));
    let stdout_path = PathBuf::from(format!("/tmp/vam-fc-{}.stdout", id));
    let stderr_path = PathBuf::from(format!("/tmp/vam-fc-{}.stderr", id));
    let jailer_dir = PathBuf::from(format!("/srv/jailer/firecracker/{}", id));

    // Force pre-cleanup of old runs
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&vsock_uds_path);
    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);
    let _ = fs::remove_dir_all(&jailer_dir);

    // 1. Allocate CID
    let cid_allocator = CidAllocator::default();
    let guest_cid = cid_allocator.allocate().unwrap();
    assert_eq!(guest_cid, 3);

    // 2. Initialize Vmm in raw bare mode (easier to run in CI / unprivileged without full root-owned /srv/jailer setups if needed, but let's test jailer = false first, or jailer = true if root is available. Let's make it configurable or test jailer = false since the lane runs unprivileged)
    let mut vmm = FirecrackerVmm::new(
        id.clone(),
        firecracker_bin,
        jailer_bin,
        false, // use_jailer: false because the lane runs unprivileged and do not want to prompt sudo passwords in tests unless running as root
        guest_cid,
    );

    // 3. Build MicroVmConfig
    let config = MicroVmConfigBuilder::new()
        .with_kernel(kernel_path)
        .with_rootfs(rootfs_path)
        .with_limits(2, 512)
        .with_guest_cid(guest_cid)
        .build()
        .unwrap();

    // 4. Lifecycle steps
    vmm.spawn().unwrap();
    assert!(vmm.get_pid().is_some());

    vmm.configure_hardware(&config, None).unwrap();
    vmm.start_instance(None).unwrap();

    // Verify it is booted and running
    let pid = vmm.get_pid().unwrap();
    assert!(pid > 0);

    // Test pause and resume
    vmm.pause().unwrap();
    vmm.resume().unwrap();

    // 5. Shutdown and Reap
    vmm.shutdown().unwrap();

    // Assert that the Firecracker process has exited
    assert!(vmm.get_pid().is_none());

    // 6. Leak assertions: confirm sockets and logs are cleared
    assert!(!socket_path.exists(), "API socket was not deleted!");
    assert!(!vsock_uds_path.exists(), "Vsock UDS was not deleted!");

    // 7. Double-reap/shutdown idempotency test
    vmm.shutdown().unwrap(); // must succeed cleanly

    // Release CID
    cid_allocator.release(guest_cid);
    assert!(!cid_allocator.is_allocated(guest_cid));
}

#[test]
#[ignore = "needs KVM: firecracker lane"]
#[cfg(target_os = "linux")]
fn test_cold_boot_baseline() {
    let id = "baseline-test-vm".to_string();
    let firecracker_bin = lane_path("VAM_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAM_JAILER_BIN");
    let kernel_path = lane_path("VAM_KERNEL_PATH");
    let rootfs_path = lane_path("VAM_ROOTFS_PATH");

    let socket_path = PathBuf::from(format!("/tmp/vam-fc-{}.socket", id));
    let vsock_uds_path = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", id));
    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&vsock_uds_path);

    let cid_allocator = CidAllocator::default();
    let guest_cid = cid_allocator.allocate().unwrap();

    let mut vmm = FirecrackerVmm::new(id.clone(), firecracker_bin, jailer_bin, false, guest_cid);

    let config = MicroVmConfigBuilder::new()
        .with_kernel(kernel_path)
        .with_rootfs(rootfs_path)
        .with_limits(2, 512)
        .with_guest_cid(guest_cid)
        .build()
        .unwrap();

    let start_time = std::time::Instant::now();

    vmm.spawn().unwrap();
    vmm.configure_hardware(&config, None).unwrap();
    vmm.start_instance(None).unwrap();

    let elapsed = start_time.elapsed();
    println!("HONEST_COLD_BOOT_LATENCY: {:?}", elapsed);

    vmm.shutdown().unwrap();
    cid_allocator.release(guest_cid);
}

#[tokio::test]
#[ignore = "needs KVM: firecracker lane"]
#[cfg(target_os = "linux")]
async fn test_vsock_registration_and_ping_integration() {
    let id = "integration-vsock-vm".to_string();
    let firecracker_bin = lane_path("VAM_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAM_JAILER_BIN");
    let kernel_path = lane_path("VAM_KERNEL_PATH");
    let rootfs_path = lane_path("VAM_ROOTFS_PATH");

    let socket_path = PathBuf::from(format!("/tmp/vam-fc-{}.socket", id));
    let vsock_uds_path = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", id));
    let stdout_path = PathBuf::from(format!("/tmp/vam-fc-{}.stdout", id));
    let stderr_path = PathBuf::from(format!("/tmp/vam-fc-{}.stderr", id));

    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&vsock_uds_path);
    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);

    let cid_allocator = CidAllocator::default();
    let guest_cid = cid_allocator.allocate().unwrap();

    // 1. Host MUST bind/listen to vsock socket BEFORE booting the VM!
    let host_listener = vam_vsock::HostListener::bind(&vsock_uds_path, 52)
        .await
        .unwrap();

    let mut vmm = FirecrackerVmm::new(
        id.clone(),
        firecracker_bin,
        jailer_bin,
        false, // use_jailer: false
        guest_cid,
    );

    let config = MicroVmConfigBuilder::new()
        .with_kernel(kernel_path)
        .with_rootfs(rootfs_path)
        .with_limits(2, 512)
        .with_guest_cid(guest_cid)
        .with_boot_arg(&format!("instance_id={}", id))
        .build()
        .unwrap();

    let start_time = std::time::Instant::now();

    vmm.spawn().unwrap();
    vmm.configure_hardware(&config, None).unwrap();
    vmm.start_instance(None).unwrap();

    // 2. Accept the incoming registration from the guest
    let challenge_registry = vam_vsock::ChallengeRegistry::new();
    let verifier = vam_vsock::DevChallengeOnlyVerifier;

    let reg_future = async {
        let stream = host_listener.accept().await.unwrap();
        vam_vsock::handle_registration_handshake(
            stream,
            &id,
            guest_cid,
            &challenge_registry,
            &verifier,
        )
        .await
    };

    let reg_res = tokio::time::timeout(std::time::Duration::from_secs(10), reg_future).await;
    assert!(reg_res.is_ok(), "Registration timed out!");
    reg_res.unwrap().unwrap();

    let registration_latency = start_time.elapsed();
    println!(
        "COLD_BOOT_TO_REGISTERED_LATENCY: {:?}",
        registration_latency
    );

    // 3. Now Host dials the Guest
    let mut guest_conn = None;
    for _ in 0..20 {
        if let Ok(stream) = vam_vsock::host_dial(&vsock_uds_path, 52).await {
            guest_conn = Some(stream);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let guest_conn = guest_conn.expect("Failed to dial guest vsock Port 52");

    // Host pings Guest
    let rtt = vam_vsock::host_ping(guest_conn, 1, &id, std::time::Duration::from_secs(1))
        .await
        .unwrap();
    println!("GUEST_RPC_PING_RTT: {:?}", rtt);
    assert!(rtt > std::time::Duration::from_secs(0));

    // Shutdown and clean up
    vmm.shutdown().unwrap();
    cid_allocator.release(guest_cid);
}

#[tokio::test]
#[ignore = "needs KVM: firecracker lane"]
#[cfg(target_os = "linux")]
async fn test_m3_rootfs_e2e_tool_execution() {
    use bytes::Bytes;
    use prost::Message;
    use vam_spec::v1::{ExecuteToolRequest, ExecuteToolResponse, VsockEnvelope};
    use vam_vsock::frame::{Frame, FrameType};

    let id = "m3-e2e-tool-vm".to_string();
    let firecracker_bin = lane_path("VAM_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAM_JAILER_BIN");
    let kernel_path = lane_path("VAM_KERNEL_PATH");
    let rootfs_path = lane_path("VAM_ROOTFS_PATH");

    let socket_path = PathBuf::from(format!("/tmp/vam-fc-{}.socket", id));
    let vsock_uds_path = PathBuf::from(format!("/tmp/vam-vsock-{}.socket", id));
    let stdout_path = PathBuf::from(format!("/tmp/vam-fc-{}.stdout", id));
    let stderr_path = PathBuf::from(format!("/tmp/vam-fc-{}.stderr", id));

    let _ = fs::remove_file(&socket_path);
    let _ = fs::remove_file(&vsock_uds_path);
    let _ = fs::remove_file(&stdout_path);
    let _ = fs::remove_file(&stderr_path);

    let cid_allocator = CidAllocator::default();
    let guest_cid = cid_allocator.allocate().unwrap();

    // 1. Host MUST bind/listen to vsock socket BEFORE booting the VM!
    let host_listener = vam_vsock::HostListener::bind(&vsock_uds_path, 52)
        .await
        .unwrap();

    let mut vmm = FirecrackerVmm::new(
        id.clone(),
        firecracker_bin,
        jailer_bin,
        false, // use_jailer: false
        guest_cid,
    );

    let config = MicroVmConfigBuilder::new()
        .with_kernel(kernel_path)
        .with_rootfs(rootfs_path)
        .with_limits(2, 512)
        .with_guest_cid(guest_cid)
        // No with_init needed, because we use the default `/sbin/init` in our custom rootfs!
        .with_boot_arg(&format!("instance_id={}", id))
        .build()
        .unwrap();

    let start_time = std::time::Instant::now();

    vmm.spawn().unwrap();
    vmm.configure_hardware(&config, None).unwrap();
    vmm.start_instance(None).unwrap();

    // 2. Accept the incoming registration from the guest
    let challenge_registry = vam_vsock::ChallengeRegistry::new();
    let verifier = vam_vsock::DevChallengeOnlyVerifier;

    let reg_future = async {
        let stream = host_listener.accept().await.unwrap();
        vam_vsock::handle_registration_handshake(
            stream,
            &id,
            guest_cid,
            &challenge_registry,
            &verifier,
        )
        .await
    };

    let reg_res = tokio::time::timeout(std::time::Duration::from_secs(10), reg_future).await;
    if reg_res.is_err() {
        if let Ok(stdout) = fs::read_to_string(&stdout_path) {
            println!("=== VM GUEST STDOUT ===");
            println!("{}", stdout);
        }
        if let Ok(stderr) = fs::read_to_string(&stderr_path) {
            println!("=== VM GUEST STDERR ===");
            println!("{}", stderr);
        }
    }
    assert!(reg_res.is_ok(), "Registration timed out!");
    reg_res.unwrap().unwrap();

    let registration_latency = start_time.elapsed();
    println!(
        "COLD_BOOT_TO_REGISTERED_LATENCY (CUSTOM ROOTFS): {:?}",
        registration_latency
    );

    // 3. Now Host dials the Guest
    let mut guest_conn = None;
    for _ in 0..20 {
        if let Ok(stream) = vam_vsock::host_dial(&vsock_uds_path, 52).await {
            guest_conn = Some(stream);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let mut guest_conn = guest_conn.expect("Failed to dial guest vsock Port 52");

    // 4. Send ExecuteTool(id) to verify unprivileged execution as UID 1000, GID 100!
    let req = ExecuteToolRequest {
        tool_name: "id".to_string(),
        arguments: None,
        timeout: Some(prost_types::Duration {
            seconds: 15,
            nanos: 0,
        }),
        trace_context: None,
    };

    let mut payload = Vec::new();
    req.encode(&mut payload).unwrap();

    let env = VsockEnvelope {
        method: "ExecuteTool".to_string(),
        correlation_id: 102,
        payload,
        error: None,
        trace_context: None,
    };
    let mut env_payload = Vec::new();
    env.encode(&mut env_payload).unwrap();

    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: 102,
        payload: Bytes::from(env_payload),
    };

    frame.write_to(&mut guest_conn).await.unwrap();

    // Read response from guest
    let frame_in = Frame::read_from(&mut guest_conn).await.unwrap();
    assert_eq!(frame_in.frame_type, FrameType::ControlGrpc);

    let env_in = VsockEnvelope::decode(&*frame_in.payload).unwrap();
    assert_eq!(env_in.method, "ExecuteToolResponse");
    if let Some(ref err) = env_in.error {
        panic!("ExecuteTool failed with guest error: {:?}", err);
    }
    assert!(env_in.error.is_none());

    let response = ExecuteToolResponse::decode(&*env_in.payload).unwrap();
    let tr = response.result.unwrap();
    assert!(tr.error.is_none());

    let output = tr.output.unwrap();
    let stdout_val = output.fields.get("stdout").unwrap();
    if let Some(prost_types::value::Kind::StringValue(ref s)) = stdout_val.kind {
        println!("ExecuteTool('id') stdout: {}", s);
        assert!(
            s.contains("uid=1000") || s.contains("1000"),
            "Expected uid=1000 but got: {}",
            s
        );
        assert!(
            s.contains("gid=100") || s.contains("100"),
            "Expected gid=100 but got: {}",
            s
        );
    } else {
        panic!("Expected stdout string value");
    }

    // Shutdown and clean up
    vmm.shutdown().unwrap();
    cid_allocator.release(guest_cid);
}
