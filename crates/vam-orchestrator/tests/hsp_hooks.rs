//! Integration tests for HSP lifecycle hook dispatch (Phase 5).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tempfile::tempdir;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use vam_orchestrator::hsp::{HspClient, HspEndpoint};
use vam_orchestrator::hsp_echo::{
    bind_unix_listener, spawn_echo_unix, spawn_scripted_hook_unix, EchoBroker,
    ScriptedHookBehavior, ScriptedHookBroker,
};
use vam_orchestrator::hsp_hooks::{
    HookTimeouts, HspHookDispatcher, EVENT_HOOK_COMPLETED, EVENT_HOOK_DEGRADED, EVENT_HOOK_FAILED,
};
use vam_orchestrator::hsp_registry::HspRegistry;
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_spec::v1::hosted_broker_server::{HostedBroker, HostedBrokerServer};
use vam_spec::v1::{
    CapabilityManifest, ContextBlock, HookDisposition, HookParticipation, HookPhase, InvokeRequest,
    InvokeResponse, ListToolsRequest, ListToolsResponse, OnCompleteRequest, OnCompleteResponse,
    OnPausedRequest, OnPausedResponse, OnRequestRequest, OnRequestResponse, OnResponseRequest,
    OnResponseResponse, RegisterRequest, RegisterResponse, RunStatus, ToolExport,
};

fn hook_manifest(
    broker_id: &str,
    phase: HookPhase,
    required: bool,
    depends_on: Vec<&str>,
) -> CapabilityManifest {
    CapabilityManifest {
        broker_id: broker_id.to_string(),
        trust_tier: 2,
        secret_bindings: vec![],
        egress_domains: vec![],
        hooks: vec![HookParticipation {
            phase: phase as i32,
            required,
        }],
        tools: vec![ToolExport {
            tool_name: "echo.ping".to_string(),
            side_effect: 0,
            required_secrets: vec![],
            destination_policy: Default::default(),
            effect: None,
        }],
        depends_on: depends_on.into_iter().map(str::to_string).collect(),
        labels: Default::default(),
        identity_requirement: None,
        run_request_permission: None,
        notification_channel: None,
    }
}

fn register_request(manifest: CapabilityManifest) -> RegisterRequest {
    RegisterRequest {
        broker_id: manifest.broker_id.clone(),
        broker_version: "phase5".to_string(),
        manifest: Some(manifest),
    }
}

async fn approve_and_register(
    store: &SqliteWorkflowStore,
    sock: std::path::PathBuf,
    manifest: CapabilityManifest,
) {
    store.upsert_hsp_broker_approval(&manifest).unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(store)
        .register_admitted(&mut client, register_request(manifest))
        .await
        .unwrap();
}

fn create_run(store: &SqliteWorkflowStore, run_id: &str) {
    store
        .create_run(&vam_spec::v1::ResourceId {
            urn: run_id.to_string(),
        })
        .unwrap();
}

fn hook_events(store: &SqliteWorkflowStore, run_id: &str) -> Vec<(String, String)> {
    store
        .read_outbox_for_run(run_id, 0)
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type.starts_with("vam.v1.hsp.hook."))
        .map(|e| (e.event_type, e.payload))
        .collect()
}

#[tokio::test]
async fn required_on_request_fail_aborts_and_emits_failed() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("fail.sock");
    let broker = ScriptedHookBroker::default().with_on_request(ScriptedHookBehavior::Fail);
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    let manifest = hook_manifest("dlp", HookPhase::OnRequest, true, vec![]);
    approve_and_register(&store, sock, manifest).await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-fail";
    create_run(&store, run_id);

    let result = HspHookDispatcher::new(&store)
        .dispatch_on_request(run_id, "sess-1", None)
        .await
        .unwrap();
    assert!(result.aborted);
    assert!(result.injected_context.is_empty());

    let events = hook_events(&store, run_id);
    assert!(
        events.iter().any(|(t, p)| {
            t == EVENT_HOOK_FAILED && p.contains("\"broker_id\":\"dlp\"") && p.contains("failed")
        }),
        "expected failed outbox event, got {events:?}"
    );
}

#[tokio::test]
async fn optional_on_request_timeout_degrades_and_continues() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("slow.sock");
    let broker = ScriptedHookBroker::default()
        .with_on_request(ScriptedHookBehavior::Sleep(Duration::from_secs(5)));
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    let manifest = hook_manifest("rag", HookPhase::OnRequest, false, vec![]);
    approve_and_register(&store, sock, manifest).await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-timeout";
    create_run(&store, run_id);

    let result = HspHookDispatcher::new(&store)
        .with_timeouts(HookTimeouts {
            on_request: Duration::from_millis(80),
            on_complete: Duration::from_secs(1),
            on_response: Duration::from_secs(1),
            on_paused: Duration::from_secs(1),
        })
        .dispatch_on_request(run_id, "sess-1", None)
        .await
        .unwrap();

    assert!(!result.aborted, "optional timeout must not abort");
    let events = hook_events(&store, run_id);
    assert!(
        events.iter().any(|(t, p)| {
            t == EVENT_HOOK_DEGRADED
                && p.contains("\"broker_id\":\"rag\"")
                && p.contains("degraded")
        }),
        "expected degraded outbox event, got {events:?}"
    );
}

#[tokio::test]
async fn scripted_inject_collects_context() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("inject.sock");
    let broker = ScriptedHookBroker::default().with_on_request(ScriptedHookBehavior::Ok {
        context: Some(ContextBlock {
            content: "injected-knowledge".to_string(),
            citations: vec![],
            source_file: None,
        }),
    });
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    let manifest = hook_manifest("knowledge", HookPhase::OnRequest, false, vec![]);
    approve_and_register(&store, sock, manifest).await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-inject";
    create_run(&store, run_id);

    let result = HspHookDispatcher::new(&store)
        .dispatch_on_request(run_id, "sess-1", None)
        .await
        .unwrap();
    assert!(!result.aborted);
    assert_eq!(result.injected_context.len(), 1);
    assert_eq!(result.injected_context[0].content, "injected-knowledge");
}

#[tokio::test]
async fn independent_brokers_run_in_same_wave() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let started = Arc::new(AtomicU64::new(0));

    let sock_a = dir.path().join("a.sock");
    let sock_b = dir.path().join("b.sock");
    let broker_a = ScriptedHookBroker {
        on_request_started: Some(started.clone()),
        wait_for_parallel: Some((started.clone(), 2)),
        ..ScriptedHookBroker::default()
    };
    let broker_b = ScriptedHookBroker {
        on_request_started: Some(started.clone()),
        wait_for_parallel: Some((started.clone(), 2)),
        ..ScriptedHookBroker::default()
    };
    let (_pa, _ha) = spawn_scripted_hook_unix(sock_a.clone(), broker_a).unwrap();
    let (_pb, _hb) = spawn_scripted_hook_unix(sock_b.clone(), broker_b).unwrap();

    approve_and_register(
        &store,
        sock_a,
        hook_manifest("broker-a", HookPhase::OnRequest, false, vec![]),
    )
    .await;
    approve_and_register(
        &store,
        sock_b,
        hook_manifest("broker-b", HookPhase::OnRequest, false, vec![]),
    )
    .await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-parallel";
    create_run(&store, run_id);

    let start = Instant::now();
    let result = HspHookDispatcher::new(&store)
        .with_timeouts(HookTimeouts {
            on_request: Duration::from_secs(5),
            ..HookTimeouts::default()
        })
        .dispatch_on_request(run_id, "sess-1", None)
        .await
        .unwrap();
    let elapsed = start.elapsed();

    assert!(!result.aborted);
    assert_eq!(started.load(Ordering::SeqCst), 2);
    assert!(
        elapsed < Duration::from_secs(3),
        "expected parallel dispatch, took {elapsed:?}"
    );
}

#[derive(Clone)]
struct OrderBroker {
    id: String,
    order: Arc<std::sync::Mutex<Vec<String>>>,
    delay: Duration,
}

#[tonic::async_trait]
impl HostedBroker for OrderBroker {
    async fn register(
        &self,
        request: tonic::Request<RegisterRequest>,
    ) -> Result<tonic::Response<RegisterResponse>, tonic::Status> {
        EchoBroker.register(request).await
    }

    async fn list_tools(
        &self,
        request: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        EchoBroker.list_tools(request).await
    }

    async fn invoke(
        &self,
        request: tonic::Request<InvokeRequest>,
    ) -> Result<tonic::Response<InvokeResponse>, tonic::Status> {
        EchoBroker.invoke(request).await
    }

    async fn on_request(
        &self,
        _request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        self.order
            .lock()
            .unwrap()
            .push(format!("{}-start", self.id));
        tokio::time::sleep(self.delay).await;
        self.order.lock().unwrap().push(format!("{}-end", self.id));
        Ok(tonic::Response::new(OnRequestResponse {
            error: None,
            injected_context: vec![],
            disposition: HookDisposition::Ok as i32,
        }))
    }

    async fn on_complete(
        &self,
        request: tonic::Request<OnCompleteRequest>,
    ) -> Result<tonic::Response<OnCompleteResponse>, tonic::Status> {
        EchoBroker.on_complete(request).await
    }

    async fn on_response(
        &self,
        request: tonic::Request<OnResponseRequest>,
    ) -> Result<tonic::Response<OnResponseResponse>, tonic::Status> {
        EchoBroker.on_response(request).await
    }

    async fn on_paused(
        &self,
        request: tonic::Request<OnPausedRequest>,
    ) -> Result<tonic::Response<OnPausedResponse>, tonic::Status> {
        EchoBroker.on_paused(request).await
    }
}

fn spawn_order_unix(
    path: std::path::PathBuf,
    broker: OrderBroker,
) -> std::io::Result<(std::path::PathBuf, tokio::task::JoinHandle<()>)> {
    let listener = bind_unix_listener(&path)?;
    let handle = tokio::spawn(async move {
        let incoming = UnixListenerStream::new(listener);
        let _ = Server::builder()
            .add_service(HostedBrokerServer::new(broker))
            .serve_with_incoming(incoming)
            .await;
    });
    Ok((path, handle))
}

#[tokio::test]
async fn depends_on_sequences_brokers() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let order = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));

    let sock_a = dir.path().join("seq-a.sock");
    let sock_b = dir.path().join("seq-b.sock");
    let (_pa, _ha) = spawn_order_unix(
        sock_a.clone(),
        OrderBroker {
            id: "a".into(),
            order: order.clone(),
            delay: Duration::from_millis(80),
        },
    )
    .unwrap();
    let (_pb, _hb) = spawn_order_unix(
        sock_b.clone(),
        OrderBroker {
            id: "b".into(),
            order: order.clone(),
            delay: Duration::from_millis(10),
        },
    )
    .unwrap();

    approve_and_register(
        &store,
        sock_a,
        hook_manifest("a", HookPhase::OnRequest, true, vec![]),
    )
    .await;
    approve_and_register(
        &store,
        sock_b,
        hook_manifest("b", HookPhase::OnRequest, true, vec!["a"]),
    )
    .await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-seq";
    create_run(&store, run_id);

    let result = HspHookDispatcher::new(&store)
        .dispatch_on_request(run_id, "sess-1", None)
        .await
        .unwrap();
    assert!(!result.aborted);

    let observed = order.lock().unwrap().clone();
    assert_eq!(
        observed,
        vec![
            "a-start".to_string(),
            "a-end".to_string(),
            "b-start".to_string(),
            "b-end".to_string(),
        ],
        "expected a fully before b, got {observed:?}"
    );
}

#[tokio::test]
async fn required_timeout_aborts() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("req-timeout.sock");
    let broker = ScriptedHookBroker::default()
        .with_on_request(ScriptedHookBehavior::Sleep(Duration::from_secs(5)));
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    approve_and_register(
        &store,
        sock,
        hook_manifest("strict", HookPhase::OnRequest, true, vec![]),
    )
    .await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-req-timeout";
    create_run(&store, run_id);

    let result = HspHookDispatcher::new(&store)
        .with_timeouts(HookTimeouts {
            on_request: Duration::from_millis(50),
            ..HookTimeouts::default()
        })
        .dispatch_on_request(run_id, "sess-1", None)
        .await
        .unwrap();
    assert!(result.aborted);
    let events = hook_events(&store, run_id);
    assert!(events.iter().any(|(t, _)| t == EVENT_HOOK_FAILED));
}

#[tokio::test]
async fn undeclared_phase_denied_without_dial() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("echo.sock");
    let calls = Arc::new(AtomicU64::new(0));
    let broker = ScriptedHookBroker {
        on_request_calls: calls.clone(),
        on_complete: ScriptedHookBehavior::Ok { context: None },
        ..ScriptedHookBroker::default()
    };
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    let manifest = hook_manifest("echo", HookPhase::OnComplete, false, vec![]);
    approve_and_register(&store, sock, manifest).await;

    let err = HspRegistry::new(&store)
        .admit_hook("echo", HookPhase::OnRequest, None)
        .unwrap_err();
    assert!(
        err.to_string().contains("undeclared hook"),
        "expected undeclared hook, got {err}"
    );

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-undeclared";
    create_run(&store, run_id);
    let result = HspHookDispatcher::new(&store)
        .dispatch_on_request(run_id, "sess-1", None)
        .await
        .unwrap();
    assert!(!result.aborted);
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn echo_ok_emits_completed_event() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("echo-ok.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();

    let manifest = hook_manifest("echo", HookPhase::OnRequest, false, vec![]);
    approve_and_register(&store, sock, manifest).await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-echo-ok";
    create_run(&store, run_id);

    let result = HspHookDispatcher::new(&store)
        .dispatch_on_request(run_id, "sess-1", None)
        .await
        .unwrap();
    assert!(!result.aborted);
    assert!(result.injected_context.is_empty());

    let events = hook_events(&store, run_id);
    assert!(
        events.iter().any(|(t, p)| {
            t == EVENT_HOOK_COMPLETED
                && p.contains("\"broker_id\":\"echo\"")
                && p.contains("\"disposition\":\"ok\"")
        }),
        "expected completed outbox event, got {events:?}"
    );
}

#[tokio::test]
async fn on_paused_notifies_broker_with_tool_and_phase() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("paused.sock");
    let tool = Arc::new(std::sync::Mutex::new(None));
    let calls = Arc::new(AtomicU64::new(0));
    let broker = ScriptedHookBroker {
        on_paused_calls: calls.clone(),
        last_paused_tool: tool.clone(),
        ..ScriptedHookBroker::default()
    };
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    let manifest = hook_manifest("pager", HookPhase::OnPaused, true, vec![]);
    approve_and_register(&store, sock, manifest).await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-paused";
    create_run(&store, run_id);

    let result = HspHookDispatcher::new(&store)
        .dispatch_on_paused(
            run_id,
            "sess-1",
            "urn:vam:hitl:local.vam.dev/acme/approval-1",
            "jira_create_issue",
            "waiting on tool approval",
        )
        .await
        .unwrap();
    assert!(!result.aborted);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(tool.lock().unwrap().as_deref(), Some("jira_create_issue"));
    let events = hook_events(&store, run_id);
    assert!(
        events.iter().any(|(event_type, payload)| {
            event_type == EVENT_HOOK_COMPLETED && payload.contains("\"phase\":\"on_paused\"")
        }),
        "expected on_paused completed event, got {events:?}"
    );
}

#[tokio::test]
async fn required_on_paused_fail_is_audited() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("paused-fail.sock");
    let broker = ScriptedHookBroker::default().with_on_paused(ScriptedHookBehavior::Fail);
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    approve_and_register(
        &store,
        sock,
        hook_manifest("pager", HookPhase::OnPaused, true, vec![]),
    )
    .await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-paused-fail";
    create_run(&store, run_id);
    let result = HspHookDispatcher::new(&store)
        .dispatch_on_paused(run_id, "sess-1", "urn:vam:hitl:req", "tool", "wait")
        .await
        .unwrap();
    assert!(result.aborted);
    let events = hook_events(&store, run_id);
    assert!(
        events.iter().any(|(event_type, payload)| {
            event_type == EVENT_HOOK_FAILED && payload.contains("\"phase\":\"on_paused\"")
        }),
        "expected on_paused failed event, got {events:?}"
    );
}

#[tokio::test]
async fn on_response_dispatch_collects_completed() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    let dir = tempdir().unwrap();
    let sock = dir.path().join("resp.sock");
    let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();

    let manifest = hook_manifest("echo", HookPhase::OnResponse, false, vec![]);
    approve_and_register(&store, sock, manifest).await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-response";
    create_run(&store, run_id);

    let status = RunStatus {
        run_id: Some(vam_spec::v1::ResourceId {
            urn: run_id.to_string(),
        }),
        state: 3, // SUCCEEDED
        ..Default::default()
    };
    let result = HspHookDispatcher::new(&store)
        .dispatch_on_response(run_id, "sess-1", status, vec![])
        .await
        .unwrap();
    assert!(!result.aborted);
    let events = hook_events(&store, run_id);
    assert!(events.iter().any(|(t, _)| t == EVENT_HOOK_COMPLETED));
}

#[tokio::test]
async fn hooks_receive_host_stamped_caller_context() {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store.create_user("alice", "alice").unwrap();
    store
        .create_session_with_owner(
            "sess-alice",
            None,
            "vm",
            "alice--assistant",
            "storage-default",
            "alice",
        )
        .unwrap();
    store
        .persist_run_spec_with_owner(
            "urn:vam:run:local.vam.dev/acme/hook-caller",
            "{}",
            Some("sess-alice"),
            "urn:vam:agent:hsp-hooks",
            None,
            "User::\"alice\"",
            "alice",
        )
        .unwrap();

    let dir = tempdir().unwrap();
    let sock = dir.path().join("caller.sock");
    let broker = ScriptedHookBroker::default();
    let last_caller = broker.last_caller.clone();
    let (_path, _handle) = spawn_scripted_hook_unix(sock.clone(), broker).unwrap();

    let mut manifest = hook_manifest("echo", HookPhase::OnRequest, false, vec![]);
    manifest.hooks.push(HookParticipation {
        phase: HookPhase::OnComplete as i32,
        required: false,
    });
    manifest.hooks.push(HookParticipation {
        phase: HookPhase::OnResponse as i32,
        required: false,
    });
    approve_and_register(&store, sock, manifest).await;

    let run_id = "urn:vam:run:local.vam.dev/acme/hook-caller";
    HspHookDispatcher::new(&store)
        .dispatch_on_request(run_id, "sess-alice", None)
        .await
        .unwrap();
    let on_request = last_caller
        .lock()
        .unwrap()
        .clone()
        .expect("on_request caller");
    assert_eq!(on_request.user_id, "alice");
    assert_eq!(on_request.workspace_handle, "alice--assistant");
    assert_eq!(on_request.agent_urn, "urn:vam:agent:hsp-hooks");

    HspHookDispatcher::new(&store)
        .dispatch_on_complete(run_id, "sess-alice", RunStatus::default())
        .await
        .unwrap();
    let on_complete = last_caller
        .lock()
        .unwrap()
        .clone()
        .expect("on_complete caller");
    assert_eq!(on_complete.user_id, "alice");

    HspHookDispatcher::new(&store)
        .dispatch_on_response(run_id, "sess-alice", RunStatus::default(), vec![])
        .await
        .unwrap();
    let on_response = last_caller
        .lock()
        .unwrap()
        .clone()
        .expect("on_response caller");
    assert_eq!(on_response.user_id, "alice");
}
