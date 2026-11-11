//! The gateway client and the broker process pieces against an in-process fake gateway on a UDS.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UnixListener;
use vam_schedule::broker::NO_IDENTITY;
use vam_schedule::gateway::{FireKind, FireOutcome, GatewayClient, GatewayError};
use vam_schedule::grant::GrantToken;
use vam_schedule::idempotency::{idempotency_key, KeyKind};
use vam_schedule::serve::{bind_unix_listener, serve_unix};
use vam_schedule::state::StateLock;
use vam_schedule::store::Store;
use vam_schedule::ScheduleBroker;
use vam_spec::v1::hosted_broker_client::HostedBrokerClient;
use vam_spec::v1::{
    BusyReason, DenyReason, InvokeRequest, ListToolsRequest, ResourceId, RunTriggerOutcome,
    VamErrorCode,
};

mod common;
use common::*;

#[tokio::test]
async fn every_outcome_is_typed_and_the_request_arrives_whole() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gw.sock");
    let gw = FakeGateway::default();
    let _server = serve_gateway(gw.clone(), &sock);
    let run = Some(ResourceId {
        urn: "urn:vam:run:local/x/r1".into(),
    });
    {
        let mut s = gw.script.lock().unwrap();
        for (outcome, extra) in [
            (RunTriggerOutcome::Accepted, 0),
            (RunTriggerOutcome::Queued, 0),
            (RunTriggerOutcome::Duplicate, 0),
            (RunTriggerOutcome::Busy, 0),
            (RunTriggerOutcome::Busy, 1500),
            (RunTriggerOutcome::RateLimited, 2000),
            (RunTriggerOutcome::Unavailable, 3000),
            (RunTriggerOutcome::Denied, 0),
        ] {
            let mut r = resp(outcome);
            r.run_id = run.clone();
            r.retry_after_ms = extra;
            if outcome == RunTriggerOutcome::Busy {
                r.busy_reason = if extra == 0 {
                    BusyReason::Skipped as i32
                } else {
                    BusyReason::ActiveRun as i32
                };
            }
            if outcome == RunTriggerOutcome::Denied {
                r.deny_reason = DenyReason::Revoked as i32;
            }
            s.push_back(Ok(r));
        }
    }
    let client = GatewayClient::new(Some(sock));
    let run_id = "urn:vam:run:local/x/r1".to_string();
    let key = idempotency_key("sch_ABC", 1_000, KeyKind::Scheduled);
    let mut got = Vec::new();
    for _ in 0..8 {
        got.push(client.fire(&request(&key)).await.unwrap());
    }
    assert_eq!(
        got,
        vec![
            FireOutcome::Accepted {
                run_id: run_id.clone()
            },
            FireOutcome::Queued {
                run_id: run_id.clone()
            },
            FireOutcome::Duplicate { run_id },
            FireOutcome::Busy {
                reason: BusyReason::Skipped,
                retry_after_ms: 0
            },
            FireOutcome::Busy {
                reason: BusyReason::ActiveRun,
                retry_after_ms: 1500
            },
            FireOutcome::RateLimited {
                retry_after_ms: 2000
            },
            FireOutcome::Unavailable {
                retry_after_ms: 3000
            },
            FireOutcome::Denied {
                reason: DenyReason::Revoked
            },
        ]
    );

    let fires = gw.fires.lock().unwrap();
    assert_eq!(fires.len(), 8);
    let f = &fires[0];
    assert_eq!(f.run_grant, TOKEN);
    assert_eq!(f.idempotency_key, key);
    assert_eq!(f.trigger_ref, "sch_ABC");
    assert_eq!(f.kind, FireKind::Scheduled as i32);
    assert_eq!(f.fire_at.as_ref().unwrap().seconds, 1_000);
    assert_eq!(f.not_after.as_ref().unwrap().seconds, 1_000 + 86_400);
    // A retry of one fire carries the same key.
    assert!(fires.iter().all(|r| r.idempotency_key == key));
}

#[tokio::test]
async fn run_now_kind_and_missing_not_after_are_sent_as_given() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gw.sock");
    let gw = FakeGateway::default();
    let _server = serve_gateway(gw.clone(), &sock);
    gw.script
        .lock()
        .unwrap()
        .push_back(Ok(resp(RunTriggerOutcome::Accepted)));
    let mut r = request("k");
    r.kind = FireKind::RunNow;
    r.not_after = None;
    GatewayClient::new(Some(sock)).fire(&r).await.unwrap();
    let fires = gw.fires.lock().unwrap();
    assert_eq!(fires[0].kind, FireKind::RunNow as i32);
    assert!(fires[0].not_after.is_none());
}

#[tokio::test]
async fn status_errors_unspecified_outcomes_and_timeouts_are_typed_errors() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gw.sock");
    let gw = FakeGateway::default();
    let _server = serve_gateway(gw.clone(), &sock);
    {
        let mut s = gw.script.lock().unwrap();
        s.push_back(Err(tonic::Status::unavailable("gateway closed")));
        s.push_back(Ok(resp(RunTriggerOutcome::Unspecified)));
    }
    let client = GatewayClient::new(Some(sock.clone())).with_timeout(Duration::from_millis(300));
    let err = client.fire(&request("k")).await.unwrap_err();
    assert!(matches!(err, GatewayError::Status { .. }), "{err}");
    let err = client.fire(&request("k")).await.unwrap_err();
    assert!(matches!(err, GatewayError::BadResponse(_)), "{err}");
    *gw.hang.lock().unwrap() = true;
    let err = client.fire(&request("k")).await.unwrap_err();
    assert!(matches!(err, GatewayError::Timeout(_)), "{err}");
}

#[tokio::test]
async fn revoke_sends_the_token_and_reports_a_gateway_error() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gw.sock");
    let gw = FakeGateway::default();
    let _server = serve_gateway(gw.clone(), &sock);
    let client = GatewayClient::new(Some(sock));
    client.revoke(&GrantToken::new(TOKEN)).await.unwrap();
    // Idempotent: a second revoke answers the same.
    client.revoke(&GrantToken::new(TOKEN)).await.unwrap();
    assert_eq!(*gw.revokes.lock().unwrap(), vec![TOKEN.to_string(); 2]);
    *gw.revoke_error.lock().unwrap() = true;
    let err = client.revoke(&GrantToken::new(TOKEN)).await.unwrap_err();
    assert!(matches!(err, GatewayError::Status { .. }), "{err}");
}

#[tokio::test]
async fn connects_per_call_across_a_gateway_restart() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gw.sock");
    let client = GatewayClient::new(Some(sock.clone()));
    // Nothing listening yet.
    let err = client.fire(&request("k")).await.unwrap_err();
    assert!(matches!(err, GatewayError::Connect { .. }), "{err}");

    let gw = FakeGateway::default();
    gw.script
        .lock()
        .unwrap()
        .push_back(Ok(resp(RunTriggerOutcome::Accepted)));
    let server = serve_gateway(gw.clone(), &sock);
    assert!(matches!(
        client.fire(&request("k")).await.unwrap(),
        FireOutcome::Accepted { .. }
    ));

    // The gateway goes away and a new one comes up on the same path: the same client works.
    server.abort();
    let _ = server.await;
    let gw2 = FakeGateway::default();
    gw2.script
        .lock()
        .unwrap()
        .push_back(Ok(resp(RunTriggerOutcome::Duplicate)));
    let _server2 = serve_gateway(gw2, &sock);
    assert!(matches!(
        client.fire(&request("k")).await.unwrap(),
        FireOutcome::Duplicate { .. }
    ));
}

async fn broker_client(sock: &Path) -> HostedBrokerClient<tonic::transport::Channel> {
    let p = sock.to_path_buf();
    let ch = tonic::transport::Endpoint::try_from("http://127.0.0.1")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_u: http::Uri| {
            let p = p.clone();
            async move { tokio::net::UnixStream::connect(p).await }
        }))
        .await
        .unwrap();
    HostedBrokerClient::new(ch)
}

#[tokio::test]
async fn the_broker_serves_over_uds_with_a_locked_store() {
    let dir = tempfile::tempdir().unwrap();
    let state = StateLock::open(&dir.path().join("state")).unwrap();
    let store = Arc::new(Store::open(&state.db_path()).unwrap());
    let broker = ScheduleBroker::new(store, GatewayClient::new(None));
    let sock = dir.path().join("b.sock");
    let listener: UnixListener = bind_unix_listener(&sock).unwrap();
    tokio::spawn(serve_unix(listener, broker));

    // A second process on the same state dir is refused while this one holds it.
    assert!(StateLock::open(state.dir()).is_err());

    let mut c = broker_client(&sock).await;
    let tools = c
        .list_tools(ListToolsRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(tools.tools.len(), 6);

    let out = c
        .invoke(InvokeRequest {
            tool_name: "schedule_list".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    let err = out.result.and_then(|r| r.error).unwrap();
    assert_eq!(err.code, VamErrorCode::InvalidArgument as i32);
    assert_eq!(err.message, NO_IDENTITY);
}
