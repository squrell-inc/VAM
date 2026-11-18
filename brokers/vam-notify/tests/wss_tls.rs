//! The Socket Mode client builds its rustls config with the process crypto provider. If the
//! dependency graph ever enables two providers, that build panics at the first wss:// connect;
//! this test makes it a failure here instead of in the broker.

use tokio::io::AsyncReadExt;
use tokio::net::TcpListener;

#[tokio::test]
async fn a_wss_handshake_against_a_non_tls_peer_is_an_error_not_a_panic() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        while let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 512];
            let _ = sock.read(&mut buf).await;
            // Close without speaking TLS.
        }
    });
    let result = tokio_tungstenite::connect_async(format!("wss://localhost:{port}/")).await;
    assert!(
        result.is_err(),
        "a non-TLS peer cannot complete a wss handshake"
    );
}
