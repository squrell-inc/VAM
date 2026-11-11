//! The grant token never reaches the log, even at TRACE. Its own test binary, so no other test
//! caches these callsites as disabled before the subscriber is set.

mod common;

use std::sync::{Arc, Mutex};

use common::*;
use vam_schedule::gateway::GatewayClient;
use vam_spec::v1::RunTriggerOutcome;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

#[tokio::test]
async fn the_token_never_reaches_the_log_even_at_trace() {
    let cap = Capture::default();
    let sub = tracing_subscriber::fmt()
        .with_writer(cap.clone())
        .with_max_level(tracing::Level::TRACE)
        .finish();
    let _guard = tracing::subscriber::set_default(sub);

    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gw.sock");
    let gw = FakeGateway::default();
    let _server = serve_gateway(gw.clone(), &sock);
    gw.script
        .lock()
        .unwrap()
        .push_back(Ok(resp(RunTriggerOutcome::Accepted)));
    let client = GatewayClient::new(Some(sock));
    let req = request("k");
    client.fire(&req).await.unwrap();
    client.revoke(&req.token).await.unwrap();
    let _ = client.fire(&req).await; // script exhausted: an error path is logged too
    let _ = format!("{req:?}");

    let logged = String::from_utf8(cap.0.lock().unwrap().clone()).unwrap();
    assert!(logged.contains("gateway fire answered"), "{logged}");
    assert!(logged.contains("gateway fire failed"), "{logged}");
    assert!(!logged.contains("topsecret"), "token leaked: {logged}");
    assert!(!logged.contains(TOKEN), "token leaked: {logged}");
}
