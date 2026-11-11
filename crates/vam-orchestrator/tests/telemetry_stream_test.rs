use std::sync::Arc;
use tokio_stream::StreamExt;
use tonic::Request;
use vam_orchestrator::api::telemetry_stream::TelemetryStreamServer;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_spec::v1::telemetry_stream_service_server::TelemetryStreamService;
use vam_spec::v1::StreamEventsRequest;
use vam_telemetry::BroadcastEventBus;

#[tokio::test]
async fn test_telemetry_stream_historical_and_live() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let bus = Arc::new(BroadcastEventBus::new());
    store.set_event_bus(bus.clone());

    // 1. Seed historical events before subscription
    store
        .append_outbox_event("session-1", "vam.v1.run.created", "payload1")
        .unwrap();
    store
        .append_outbox_event("session-1", "vam.v1.session.booted", "payload2")
        .unwrap();

    let server = TelemetryStreamServer::new(store.clone(), bus.clone());

    // 2. Subscribe from sequence 0 (replay historical + receive live)
    let req = Request::new(StreamEventsRequest {
        filter_session_id: "session-1".to_string(),
        filter_event_types: vec![],
        resume_from_sequence: 0,
    });

    let resp = server.stream_events(req).await.unwrap();
    let mut stream = resp.into_inner();

    // Verify first 2 historical events
    let ev1 = stream.next().await.unwrap().unwrap();
    assert_eq!(ev1.sequence_number, 1);
    assert_eq!(ev1.r#type, "vam.v1.run.created");

    let ev2 = stream.next().await.unwrap().unwrap();
    assert_eq!(ev2.sequence_number, 2);
    assert_eq!(ev2.r#type, "vam.v1.session.booted");

    // 3. Emit live event
    store
        .append_outbox_event("session-1", "vam.v1.inference.completed", "payload3")
        .unwrap();

    let ev3 = stream.next().await.unwrap().unwrap();
    assert_eq!(ev3.sequence_number, 3);
    assert_eq!(ev3.r#type, "vam.v1.inference.completed");
}

#[tokio::test]
async fn test_telemetry_stream_filters() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let bus = Arc::new(BroadcastEventBus::new());
    store.set_event_bus(bus.clone());

    store
        .append_outbox_event("session-A", "vam.v1.egress.decided", "egressA")
        .unwrap();
    store
        .append_outbox_event("session-B", "vam.v1.egress.decided", "egressB")
        .unwrap();
    store
        .append_outbox_event("session-A", "vam.v1.secret.accessed", "secretA")
        .unwrap();

    let server = TelemetryStreamServer::new(store.clone(), bus.clone());

    // Filter by session-A and event type secret.accessed
    let req = Request::new(StreamEventsRequest {
        filter_session_id: "session-A".to_string(),
        filter_event_types: vec!["vam.v1.secret.accessed".to_string()],
        resume_from_sequence: 0,
    });

    let resp = server.stream_events(req).await.unwrap();
    let mut stream = resp.into_inner();

    let ev = stream.next().await.unwrap().unwrap();
    assert_eq!(ev.sequence_number, 3);
    assert_eq!(ev.r#type, "vam.v1.secret.accessed");
    assert_eq!(ev.run_id.unwrap().urn, "session-A");
}

#[tokio::test]
async fn test_telemetry_stream_survives_collector_outage() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let bus = Arc::new(BroadcastEventBus::new());
    store.set_event_bus(bus.clone());

    let server = TelemetryStreamServer::new(store.clone(), bus.clone());

    // Initial events
    store
        .append_outbox_event("session-1", "vam.v1.event.1", "p1")
        .unwrap();
    store
        .append_outbox_event("session-1", "vam.v1.event.2", "p2")
        .unwrap();

    // Collector 1 connects and reads events 1 and 2
    let mut stream1 = server
        .stream_events(Request::new(StreamEventsRequest {
            filter_session_id: "session-1".to_string(),
            filter_event_types: vec![],
            resume_from_sequence: 0,
        }))
        .await
        .unwrap()
        .into_inner();

    let e1 = stream1.next().await.unwrap().unwrap();
    assert_eq!(e1.sequence_number, 1);
    let e2 = stream1.next().await.unwrap().unwrap();
    assert_eq!(e2.sequence_number, 2);

    let last_received_seq = e2.sequence_number;

    // Simulate collector outage / disconnect
    drop(stream1);

    // Host continues to emit events 3, 4, 5 while collector is offline
    store
        .append_outbox_event("session-1", "vam.v1.event.3", "p3")
        .unwrap();
    store
        .append_outbox_event("session-1", "vam.v1.event.4", "p4")
        .unwrap();
    store
        .append_outbox_event("session-1", "vam.v1.event.5", "p5")
        .unwrap();

    // Collector reconnects with resume_from_sequence = 2
    let mut stream2 = server
        .stream_events(Request::new(StreamEventsRequest {
            filter_session_id: "session-1".to_string(),
            filter_event_types: vec![],
            resume_from_sequence: last_received_seq,
        }))
        .await
        .unwrap()
        .into_inner();

    // Verify events 3, 4, 5 are received in order without gap
    let e3 = stream2.next().await.unwrap().unwrap();
    assert_eq!(e3.sequence_number, 3);
    assert_eq!(e3.r#type, "vam.v1.event.3");

    let e4 = stream2.next().await.unwrap().unwrap();
    assert_eq!(e4.sequence_number, 4);
    assert_eq!(e4.r#type, "vam.v1.event.4");

    let e5 = stream2.next().await.unwrap().unwrap();
    assert_eq!(e5.sequence_number, 5);
    assert_eq!(e5.r#type, "vam.v1.event.5");
}

#[tokio::test]
async fn test_telemetry_stream_server_bind_smoke() {
    use tokio_stream::wrappers::TcpListenerStream;
    use tonic::transport::Server;
    use vam_spec::v1::telemetry_stream_service_client::TelemetryStreamServiceClient;
    use vam_spec::v1::telemetry_stream_service_server::TelemetryStreamServiceServer;

    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let bus = Arc::new(BroadcastEventBus::new());
    store.set_event_bus(bus.clone());
    store
        .append_outbox_event("session-bind", "vam.v1.session.booted", "boot")
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let incoming = TcpListenerStream::new(listener);

    let svc_store = store.clone();
    let svc_bus = bus.clone();
    let server = tokio::spawn(async move {
        Server::builder()
            .add_service(TelemetryStreamServiceServer::new(
                TelemetryStreamServer::new(svc_store, svc_bus),
            ))
            .serve_with_incoming(incoming)
            .await
    });

    let mut client = None;
    for _ in 0..40 {
        match TelemetryStreamServiceClient::connect(format!("http://{addr}")).await {
            Ok(c) => {
                client = Some(c);
                break;
            }
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(25)).await,
        }
    }
    let mut client = client.expect("telemetry gRPC should accept connections");
    let resp = client
        .stream_events(Request::new(StreamEventsRequest {
            filter_session_id: "session-bind".to_string(),
            filter_event_types: vec![],
            resume_from_sequence: 0,
        }))
        .await
        .expect("StreamEvents");
    let mut stream = resp.into_inner();
    let ev = stream.next().await.unwrap().unwrap();
    assert_eq!(ev.r#type, "vam.v1.session.booted");

    server.abort();
}
