//! The fire loop against the in-process fake gateway on a UDS: the real `GatewayClient`, a real
//! store file, a `TestClock`.

use std::sync::Arc;

use vam_schedule::fire::Scheduler;
use vam_schedule::gateway::GatewayClient;
use vam_schedule::grant::GrantToken;
use vam_schedule::idempotency::{idempotency_key, KeyKind};
use vam_schedule::store::{NewSchedule, Store};
use vam_schedule::TestClock;
use vam_spec::v1::{ResourceId, RunTriggerOutcome};

mod common;
use common::*;

fn ts(s: &str) -> i64 {
    s.parse::<chrono::DateTime<chrono::Utc>>()
        .unwrap()
        .timestamp()
}

fn schedule(next: &str) -> NewSchedule {
    NewSchedule {
        schedule_id: "sch_ABC".into(),
        grant_id: "rg_ABC".into(),
        token: GrantToken::new(TOKEN),
        grant_expires_at: ts("2030-01-01T00:00:00Z"),
        target_json: "{}".into(),
        message_preview: String::new(),
        owner_user_id: "u".into(),
        creator_agent_urn: "a".into(),
        title: "t".into(),
        kind: "cron".into(),
        expression: "0 * * * *".into(),
        timezone: "UTC".into(),
        catch_up: "latest".into(),
        next_fire_at: Some(ts(next)),
        now: 0,
    }
}

fn scheduler(store: &Arc<Store>, sock: &std::path::Path, now: &str) -> (Scheduler, TestClock) {
    let clock = TestClock::new(now.parse().unwrap());
    let s = Scheduler::new(
        store.clone(),
        Arc::new(GatewayClient::new(Some(sock.to_path_buf()))),
        Arc::new(clock.clone()),
    )
    .with_jitter(|| 0.0);
    (s, clock)
}

#[tokio::test]
async fn a_retry_after_an_error_and_a_restart_sends_the_same_key_and_dedupes() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gw.sock");
    let db = dir.path().join("db");
    let gw = FakeGateway::default();
    let _server = serve_gateway(gw.clone(), &sock);
    let t = ts("2026-01-01T10:00:00Z");
    let key = idempotency_key("sch_ABC", t, KeyKind::Scheduled);

    // First process: the gateway fails the call at the transport level, so the fire is retried.
    {
        let store = Arc::new(Store::open(&db).unwrap());
        store
            .insert_schedule(&schedule("2026-01-01T10:00:00Z"))
            .unwrap();
        gw.script
            .lock()
            .unwrap()
            .push_back(Err(tonic::Status::unavailable("down")));
        let (s, _clock) = scheduler(&store, &sock, "2026-01-01T10:00:01Z");
        s.run_pass().await;
        let row = store.get_schedule("sch_ABC").unwrap().unwrap();
        assert_eq!(row.attempts, 1);
        assert_eq!(row.next_fire_at, Some(t), "not advanced");
    }

    // Second process on the same file: the backoff held, then the retry is DUPLICATE.
    let store = Arc::new(Store::open(&db).unwrap());
    let (s, clock) = scheduler(&store, &sock, "2026-01-01T10:00:01Z");
    s.run_pass().await;
    assert_eq!(gw.fires.lock().unwrap().len(), 1, "still backing off");
    clock.set("2026-01-01T10:00:10Z".parse().unwrap());
    let mut dup = resp(RunTriggerOutcome::Duplicate);
    dup.run_id = Some(ResourceId {
        urn: "urn:vam:run:local/x/r1".into(),
    });
    gw.script.lock().unwrap().push_back(Ok(dup));
    s.run_pass().await;

    let fires = gw.fires.lock().unwrap();
    assert_eq!(fires.len(), 2);
    assert!(fires.iter().all(|f| f.idempotency_key == key));
    assert!(fires.iter().all(|f| f.trigger_ref == "sch_ABC"));
    assert_eq!(fires[1].fire_at.as_ref().unwrap().seconds, t);
    assert_eq!(
        fires[1].not_after.as_ref().unwrap().seconds,
        ts("2026-01-01T11:00:00Z")
    );
    let row = store.fires_for("sch_ABC").unwrap();
    assert_eq!(row[0].outcome.as_deref(), Some("duplicate"));
    assert_eq!(row[0].run_id.as_deref(), Some("urn:vam:run:local/x/r1"));
    assert_eq!(
        store.get_schedule("sch_ABC").unwrap().unwrap().next_fire_at,
        Some(ts("2026-01-01T11:00:00Z"))
    );
}

#[tokio::test]
async fn a_one_off_revoke_goes_through_the_real_gateway() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("gw.sock");
    let gw = FakeGateway::default();
    let _server = serve_gateway(gw.clone(), &sock);
    let store = Arc::new(Store::open_in_memory().unwrap());
    let mut n = schedule("2026-01-01T10:00:00Z");
    n.kind = "once".into();
    n.expression = "x".into();
    store.insert_schedule(&n).unwrap();
    gw.script
        .lock()
        .unwrap()
        .push_back(Ok(resp(RunTriggerOutcome::Accepted)));
    let (s, _c) = scheduler(&store, &sock, "2026-01-01T10:00:01Z");
    s.run_pass().await;
    assert_eq!(gw.revokes.lock().unwrap().as_slice(), [TOKEN.to_string()]);
    assert_eq!(
        store.get_schedule("sch_ABC").unwrap().unwrap().state,
        "completed"
    );
}
