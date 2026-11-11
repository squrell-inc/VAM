#![cfg(all(unix, feature = "uds-transport"))]

use std::time::Duration;
use tokio::net::UnixStream;
use vam_vsock::{
    guest_dial_with, handle_guest_connection, handle_registration_handshake, host_dial, host_ping,
    run_guest_registration, ChallengeRegistry, DevChallengeOnlyVerifier, GuestListener,
    GuestTransport, HostListener,
};

/// A per-test base path in a fresh temp dir (Firecracker's `<base>` layout).
fn uds_base(name: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join(name);
    (dir, base)
}

#[tokio::test]
async fn test_registration_success_and_ping() {
    // The explicit Uds transport, laid out the way Firecracker lays out vsock:
    // the guest dials `<base>_<port>` (the host listener), and the host reaches
    // the guest through `<base>` with `CONNECT <port>`. Real vsock is never used.
    let (_dir, base) = uds_base("vam-test-registration-success.socket");
    let transport = GuestTransport::Uds { base: base.clone() };
    let port = 52;
    let expected_cid = 42;
    let instance_id = "urn:vam:agent:test-instance";

    let host_listener = HostListener::bind(&base, port).await.unwrap();
    let challenge_registry = ChallengeRegistry::new();
    let verifier = DevChallengeOnlyVerifier;

    // The guest listener is bound before registration, as the real guest does.
    let mut guest_listener = GuestListener::bind_with(&transport, port).await.unwrap();
    let guest_transport = transport.clone();
    let guest_handle = tokio::spawn(async move {
        let guest_stream = guest_dial_with(&guest_transport, 2, port).await.unwrap();
        run_guest_registration(guest_stream, instance_id, expected_cid)
            .await
            .unwrap();

        let (guest_conn, _, accepted_port) = guest_listener.accept().await.unwrap();
        assert_eq!(accepted_port, port);
        handle_guest_connection(guest_conn, instance_id)
            .await
            .unwrap();
    });

    let host_reg_stream = host_listener.accept().await.unwrap();
    handle_registration_handshake(
        host_reg_stream,
        instance_id,
        expected_cid,
        &challenge_registry,
        &verifier,
    )
    .await
    .unwrap();

    let host_ping_stream = host_dial(&base, port).await.expect("host_dial to guest");
    let rtt = host_ping(host_ping_stream, 42, instance_id, Duration::from_secs(1))
        .await
        .unwrap();
    assert!(rtt > Duration::from_secs(0));

    guest_handle.await.unwrap();
}

#[tokio::test]
async fn uds_mode_creates_no_legacy_mock_socket() {
    let legacy = std::path::PathBuf::from(format!("/tmp/vam-mock-vsock-{}", 61));
    let _ = std::fs::remove_file(&legacy);
    let (_dir, base) = uds_base("vam-test-no-legacy.socket");
    let transport = GuestTransport::Uds { base: base.clone() };
    let _listener = GuestListener::bind_with(&transport, 61).await.unwrap();
    assert!(base.exists(), "the mux listens on <base>");
    assert!(!legacy.exists(), "no /tmp/vam-mock-vsock-* fallback file");
    // Dialing with nothing at <base>_61 fails; it does not reach any other path.
    assert!(guest_dial_with(&transport, 2, 61).await.is_err());
}

#[tokio::test]
async fn uds_mux_rejects_wrong_port_and_keeps_serving() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let (_dir, base) = uds_base("vam-test-mux-reject.socket");
    let transport = GuestTransport::Uds { base: base.clone() };
    let mut listener = GuestListener::bind_with(&transport, 52).await.unwrap();
    let accept = tokio::spawn(async move { listener.accept().await.map(|(_, _, p)| p) });

    let mut bad = UnixStream::connect(&base).await.unwrap();
    bad.write_all(b"CONNECT 99\n").await.unwrap();
    let mut buf = [0u8; 8];
    assert_eq!(
        bad.read(&mut buf).await.unwrap(),
        0,
        "wrong port is dropped"
    );

    let good = host_dial(&base, 52).await.unwrap();
    assert_eq!(accept.await.unwrap().unwrap(), 52);
    drop(good);
}

#[tokio::test]
async fn uds_bind_does_not_delete_a_non_socket() {
    let (_dir, base) = uds_base("vam-test-not-a-socket");
    std::fs::write(&base, b"keep me").unwrap();
    let transport = GuestTransport::Uds { base: base.clone() };
    assert!(GuestListener::bind_with(&transport, 52).await.is_err());
    assert_eq!(std::fs::read(&base).unwrap(), b"keep me");
    let rel = GuestTransport::Uds {
        base: "relative.socket".into(),
    };
    assert!(GuestListener::bind_with(&rel, 52).await.is_err());
}

#[tokio::test]
async fn vsock_mode_never_falls_back_to_a_unix_socket() {
    // Plant the old fallback path with a live listener. A Vsock dial must not
    // reach it, whether or not this machine has /dev/vsock.
    let legacy = "/tmp/vam-mock-vsock-62";
    let _ = std::fs::remove_file(legacy);
    let planted = tokio::net::UnixListener::bind(legacy).unwrap();
    let accepted = tokio::spawn(async move { planted.accept().await.is_ok() });
    let dial = tokio::time::timeout(
        Duration::from_secs(2),
        guest_dial_with(&GuestTransport::Vsock, 2, 62),
    )
    .await;
    assert!(
        !matches!(dial, Ok(Ok(_))),
        "Vsock transport must not connect to the legacy Unix socket"
    );
    assert!(
        !accepted.is_finished(),
        "nothing connected to the planted socket"
    );
    accepted.abort();
    let _ = std::fs::remove_file(legacy);
}

#[test]
fn guest_source_has_no_legacy_fallback_path() {
    let src = include_str!("../src/guest.rs");
    assert!(
        !src.contains(concat!("vam-mock", "-vsock")),
        "the /tmp mock-vsock fallback must stay deleted"
    );
}

#[tokio::test]
async fn test_registration_failures() {
    let uds_path = "/tmp/vam-test-registration-fail.socket";
    let port = 52;
    let expected_cid = 42;
    let instance_id = "urn:vam:agent:test-instance";

    let host_listener = HostListener::bind(uds_path, port).await.unwrap();
    let challenge_registry = ChallengeRegistry::new();
    let verifier = DevChallengeOnlyVerifier;

    // Test case 1: Wrong Instance ID
    let host_sock_path = host_listener.path().to_path_buf();
    tokio::spawn(async move {
        let guest_stream = UnixStream::connect(&host_sock_path).await.unwrap();
        // Register with wrong instance ID
        let _ = run_guest_registration(guest_stream, "urn:vam:agent:wrong-instance", expected_cid)
            .await;
    });

    let host_stream = host_listener.accept().await.unwrap();
    let res = handle_registration_handshake(
        host_stream,
        instance_id,
        expected_cid,
        &challenge_registry,
        &verifier,
    )
    .await;
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("Instance ID mismatch"));

    // Test case 2: CID Mismatch
    let host_sock_path = host_listener.path().to_path_buf();
    tokio::spawn(async move {
        let guest_stream = UnixStream::connect(&host_sock_path).await.unwrap();
        // Register with wrong CID
        let _ = run_guest_registration(guest_stream, instance_id, 999).await;
    });

    let host_stream = host_listener.accept().await.unwrap();
    let res = handle_registration_handshake(
        host_stream,
        instance_id,
        expected_cid,
        &challenge_registry,
        &verifier,
    )
    .await;
    assert!(res.is_err());
    assert!(res.unwrap_err().contains("CID mismatch"));
}
