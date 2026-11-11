//! AR-3.3 contract checks: each new field and message round-trips, and the JSON
//! names are the ones the e2e runner (`vam-e2e/src/steer.rs`) sends and reads.

use prost::Message;
use vam_spec::encoding::{json_to_proto, proto_to_json};
use vam_spec::v1::*;

fn round_trip<T: Message + Default + PartialEq + std::fmt::Debug>(msg: &T) {
    let bytes = msg.encode_to_vec();
    assert_eq!(&T::decode(&*bytes).unwrap(), msg);
}

fn rid(urn: &str) -> Option<ResourceId> {
    Some(ResourceId { urn: urn.into() })
}

#[test]
fn run_spec_inbox_mode_uses_the_names_the_runner_sends() {
    for (name, expected) in [
        ("STEER", run_spec::InboxMode::Steer),
        ("QUEUE", run_spec::InboxMode::Queue),
    ] {
        let body = format!(r#"{{"spec":{{"inboxMode":"{name}","threadRef":"main"}}}}"#);
        let req: CreateRunRequest = json_to_proto("vam.v1.CreateRunRequest", &body).unwrap();
        assert_eq!(req.spec.unwrap().inbox_mode, expected as i32);
    }
    // Unset reads as UNSPECIFIED, which means STEER.
    let req: CreateRunRequest = json_to_proto("vam.v1.CreateRunRequest", r#"{"spec":{}}"#).unwrap();
    assert_eq!(
        req.spec.unwrap().inbox_mode,
        run_spec::InboxMode::Unspecified as i32
    );
    assert!(json_to_proto::<CreateRunRequest>(
        "vam.v1.CreateRunRequest",
        r#"{"spec":{"inboxMode":"MAYBE"}}"#
    )
    .is_err());
}

#[test]
fn create_run_response_inbox_json_names() {
    let resp = CreateRunResponse {
        status: None,
        inbox: Some(InboxReceipt {
            seq: 3,
            mode: run_spec::InboxMode::Queue as i32,
            run_id: rid("urn:vam:run:local.vam.dev/acme/run-2"),
        }),
    };
    round_trip(&resp);
    let json = proto_to_json("vam.v1.CreateRunResponse", &resp).unwrap();
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert_eq!(
        v["inbox"]["runId"]["urn"],
        "urn:vam:run:local.vam.dev/acme/run-2"
    );
    assert_eq!(v["inbox"]["mode"], "QUEUE");
    assert_eq!(v["inbox"]["seq"].to_string().trim_matches('"'), "3");
}

#[test]
fn send_and_stop_messages_round_trip() {
    round_trip(&SendRunMessageRequest {
        run_id: rid("urn:vam:run:local.vam.dev/acme/r"),
        text: "use the title Final".into(),
        mode: run_spec::InboxMode::Steer as i32,
        idempotency_key: "k1".into(),
    });
    round_trip(&SendRunMessageResponse {
        status: None,
        inbox: Some(InboxReceipt {
            seq: 1,
            mode: 1,
            run_id: None,
        }),
    });
    round_trip(&StopRunRequest {
        run_id: rid("urn:vam:run:local.vam.dev/acme/r"),
        idempotency_key: "k2".into(),
    });
    let stop = StopRunResponse {
        status: None,
        discarded: vec![DiscardedMessage {
            seq: 2,
            mode: run_spec::InboxMode::Queue as i32,
            run_id: rid("urn:vam:run:local.vam.dev/acme/q"),
            text: "put me back".into(),
        }],
    };
    round_trip(&stop);
    let v: serde_json::Value =
        serde_json::from_str(&proto_to_json("vam.v1.StopRunResponse", &stop).unwrap()).unwrap();
    assert_eq!(v["discarded"][0]["mode"], "QUEUE");
    assert_eq!(v["discarded"][0]["text"], "put me back");
}

#[test]
fn question_kind_and_waiting_for() {
    assert_eq!(ApprovalKind::Question as i32, 3);
    assert_eq!(
        ApprovalKind::try_from(3).unwrap().as_str_name(),
        "APPROVAL_KIND_QUESTION"
    );
    let status = RunStatus {
        waiting_for: WaitingFor::Input as i32,
        ..Default::default()
    };
    round_trip(&status);
    let v: serde_json::Value =
        serde_json::from_str(&proto_to_json("vam.v1.RunStatus", &status).unwrap()).unwrap();
    assert_eq!(v["waitingFor"], "WAITING_FOR_INPUT");
}

#[test]
fn port_52_inbox_and_outbox_round_trip() {
    round_trip(&PullInboxRequest {
        lease: Some(ExecutionLease::default()),
        after_seq: 4,
        ack_seq: 3,
    });
    round_trip(&PullInboxResponse {
        messages: vec![InboxMessage {
            seq: 5,
            text: "hello".into(),
        }],
        error: None,
    });
    round_trip(&PostOutboxRequest {
        lease: Some(ExecutionLease::default()),
        kind: OutboxKind::Card as i32,
        text: String::new(),
        jig: "vam.progress".into(),
        data_json: r#"{"label":"x","done":1,"total":2}"#.into(),
        capability_token: Some(vam_spec::v1::SignedEnvelope {
            domain: "vam.v1.tool-capability".into(),
            ..Default::default()
        }),
    });
    round_trip(&PostOutboxResponse { error: None });
}
