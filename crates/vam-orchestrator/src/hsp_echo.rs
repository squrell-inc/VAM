//! In-repo echo/stub `HostedBroker` for Phase 1 transport proof.

use std::net::SocketAddr;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;

use tokio::net::TcpListener;
#[cfg(unix)]
use tokio::net::UnixListener;
use tokio_stream::wrappers::TcpListenerStream;
#[cfg(unix)]
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use vam_spec::v1::hosted_broker_server::{HostedBroker, HostedBrokerServer};
use vam_spec::v1::{
    HookDisposition, HostExecSpec, InvokeRequest, InvokeResponse, ListToolsRequest,
    ListToolsResponse, OnCompleteRequest, OnCompleteResponse, OnPausedRequest, OnPausedResponse,
    OnRequestRequest, OnRequestResponse, OnResponseRequest, OnResponseResponse, RegisterRequest,
    RegisterResponse, ToolResult, ToolSpec,
};

/// Echo broker: accepts any registration and echoes `Invoke` payloads.
#[derive(Debug, Default, Clone)]
pub struct EchoBroker;

#[tonic::async_trait]
impl HostedBroker for EchoBroker {
    async fn register(
        &self,
        _request: tonic::Request<RegisterRequest>,
    ) -> Result<tonic::Response<RegisterResponse>, tonic::Status> {
        Ok(tonic::Response::new(RegisterResponse {
            error: None,
            session_policy_version: String::new(),
        }))
    }

    async fn list_tools(
        &self,
        _request: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListToolsResponse {
            tools: vec![ToolSpec {
                name: "echo.ping".to_string(),
                description: "Echo ping for HSP transport tests".to_string(),
                input_schema_json:
                    r#"{"type":"object","properties":{"message":{"type":"string"}}}"#.to_string(),
                execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
                    HostExecSpec {
                        adapter_id: "broker-echo".to_string(),
                        config: Default::default(),
                    },
                )),
                required_secrets: vec![],
                side_effect: 0,
                output_schema_json: String::new(),
                timeout: None,
                ..Default::default()
            }],
        }))
    }

    async fn invoke(
        &self,
        request: tonic::Request<InvokeRequest>,
    ) -> Result<tonic::Response<InvokeResponse>, tonic::Status> {
        let req = request.into_inner();
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "tool_name".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(req.tool_name)),
            },
        );
        if let Some(args) = req.args {
            fields.insert(
                "args".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StructValue(args)),
                },
            );
        }
        if !req.resolved_secrets.is_empty() {
            let mut secret_fields = std::collections::BTreeMap::new();
            for (key, value) in req.resolved_secrets {
                secret_fields.insert(
                    key,
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(value)),
                    },
                );
            }
            fields.insert(
                "resolved_secrets".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StructValue(prost_types::Struct {
                        fields: secret_fields,
                    })),
                },
            );
        }

        if let Some(tc) = req.trace_context {
            if let Ok(tp) = vam_telemetry::validate_traceparent(&tc.traceparent) {
                tracing::info!(
                    target: "hsp_echo_broker",
                    trace_id = %tp.trace_id,
                    parent_id = %tp.parent_id,
                    traceparent = %tc.traceparent,
                    "EchoBroker continuing traceparent on invoke"
                );
                fields.insert(
                    "traceparent".to_string(),
                    prost_types::Value {
                        kind: Some(prost_types::value::Kind::StringValue(tc.traceparent)),
                    },
                );
            }
        }

        if let Some(caller) = req.caller_context {
            fields.insert(
                "caller_user_id".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(caller.user_id)),
                },
            );
            fields.insert(
                "caller_tenant_id".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(caller.tenant_id)),
                },
            );
            fields.insert(
                "caller_agent_urn".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(caller.agent_urn)),
                },
            );
        }

        Ok(tonic::Response::new(InvokeResponse {
            result: Some(ToolResult {
                output: Some(prost_types::Struct { fields }),
                error: None,
            }),
            output_blocks: vec![],
            artifact: None,
            sources: Default::default(),
        }))
    }

    async fn on_request(
        &self,
        request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        let req = request.into_inner();
        if let Some(tc) = req.trace_context {
            if let Ok(tp) = vam_telemetry::validate_traceparent(&tc.traceparent) {
                tracing::info!(
                    target: "hsp_echo_broker",
                    trace_id = %tp.trace_id,
                    parent_id = %tp.parent_id,
                    "EchoBroker continuing traceparent on on_request"
                );
            }
        }
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
        let req = request.into_inner();
        if let Some(tc) = req.trace_context {
            if let Ok(tp) = vam_telemetry::validate_traceparent(&tc.traceparent) {
                tracing::info!(
                    target: "hsp_echo_broker",
                    trace_id = %tp.trace_id,
                    parent_id = %tp.parent_id,
                    "EchoBroker continuing traceparent on on_complete"
                );
            }
        }
        Ok(tonic::Response::new(OnCompleteResponse {
            error: None,
            retention_labels: vec![],
            disposition: HookDisposition::Ok as i32,
        }))
    }

    async fn on_response(
        &self,
        request: tonic::Request<OnResponseRequest>,
    ) -> Result<tonic::Response<OnResponseResponse>, tonic::Status> {
        let req = request.into_inner();
        if let Some(tc) = req.trace_context {
            if let Ok(tp) = vam_telemetry::validate_traceparent(&tc.traceparent) {
                tracing::info!(
                    target: "hsp_echo_broker",
                    trace_id = %tp.trace_id,
                    parent_id = %tp.parent_id,
                    "EchoBroker continuing traceparent on on_response"
                );
            }
        }
        Ok(tonic::Response::new(OnResponseResponse {
            error: None,
            response_blocks: vec![],
            disposition: HookDisposition::Ok as i32,
        }))
    }

    async fn on_paused(
        &self,
        request: tonic::Request<OnPausedRequest>,
    ) -> Result<tonic::Response<OnPausedResponse>, tonic::Status> {
        let req = request.into_inner();
        if let Some(tc) = req.trace_context {
            if let Ok(tp) = vam_telemetry::validate_traceparent(&tc.traceparent) {
                tracing::info!(
                    target: "hsp_echo_broker",
                    trace_id = %tp.trace_id,
                    parent_id = %tp.parent_id,
                    "EchoBroker continuing traceparent on on_paused"
                );
            }
        }
        Ok(tonic::Response::new(OnPausedResponse {
            error: None,
            disposition: HookDisposition::Ok as i32,
        }))
    }
}

/// Echo broker that advertises an undeclared tool for catalog drift tests.
#[derive(Debug, Default, Clone)]
pub struct ExtraToolEchoBroker;

#[tonic::async_trait]
impl HostedBroker for ExtraToolEchoBroker {
    async fn register(
        &self,
        request: tonic::Request<RegisterRequest>,
    ) -> Result<tonic::Response<RegisterResponse>, tonic::Status> {
        EchoBroker.register(request).await
    }

    async fn list_tools(
        &self,
        _request: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListToolsResponse {
            tools: vec![
                ToolSpec {
                    name: "echo.ping".to_string(),
                    description: "Echo ping".to_string(),
                    input_schema_json: "{}".to_string(),
                    execution_provider: None,
                    required_secrets: vec![],
                    side_effect: 0,
                    output_schema_json: String::new(),
                    timeout: None,
                    ..Default::default()
                },
                ToolSpec {
                    name: "echo.extra".to_string(),
                    description: "Undeclared tool".to_string(),
                    input_schema_json: "{}".to_string(),
                    execution_provider: None,
                    required_secrets: vec![],
                    side_effect: 0,
                    output_schema_json: String::new(),
                    timeout: None,
                    ..Default::default()
                },
            ],
        }))
    }

    async fn invoke(
        &self,
        request: tonic::Request<InvokeRequest>,
    ) -> Result<tonic::Response<InvokeResponse>, tonic::Status> {
        EchoBroker.invoke(request).await
    }

    async fn on_request(
        &self,
        request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        EchoBroker.on_request(request).await
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

/// Spawn an echo broker that returns an extra undeclared tool from `ListTools`.
#[cfg(unix)]
pub fn spawn_extra_tool_echo_unix(
    path: std::path::PathBuf,
) -> std::io::Result<(std::path::PathBuf, tokio::task::JoinHandle<()>)> {
    let listener = bind_unix_listener(&path)?;
    let handle = tokio::spawn(async move {
        if let Err(err) = serve_unix_with_broker(listener, ExtraToolEchoBroker).await {
            eprintln!("hsp extra-tool echo unix server exited: {err}");
        }
    });
    Ok((path, handle))
}

#[cfg(unix)]
pub async fn serve_unix_with_broker<B: HostedBroker + Send + Sync + 'static>(
    listener: UnixListener,
    broker: B,
) -> Result<(), tonic::transport::Error> {
    let incoming = UnixListenerStream::new(listener);
    Server::builder()
        .add_service(HostedBrokerServer::new(broker))
        .serve_with_incoming(incoming)
        .await
}

/// Bind a UDS listener, removing stale socket files and setting mode `0o600`.
#[cfg(unix)]
pub fn bind_unix_listener(path: &Path) -> std::io::Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let listener = UnixListener::bind(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

/// Serve `HostedBroker` over a Unix domain socket until the listener is closed.
#[cfg(unix)]
pub async fn serve_unix(
    listener: UnixListener,
    broker: EchoBroker,
) -> Result<(), tonic::transport::Error> {
    let incoming = UnixListenerStream::new(listener);
    Server::builder()
        .add_service(HostedBrokerServer::new(broker))
        .serve_with_incoming(incoming)
        .await
}

/// Serve `HostedBroker` over TCP (tests/debug only).
pub async fn serve_tcp(
    addr: SocketAddr,
    broker: EchoBroker,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(addr).await?;
    let incoming = TcpListenerStream::new(listener);
    Server::builder()
        .add_service(HostedBrokerServer::new(broker))
        .serve_with_incoming(incoming)
        .await?;
    Ok(())
}

/// Spawn an in-process echo broker on a temp UDS path; returns `(socket_path, join_handle)`.
#[cfg(unix)]
pub fn spawn_echo_unix(
    path: std::path::PathBuf,
) -> std::io::Result<(std::path::PathBuf, tokio::task::JoinHandle<()>)> {
    let listener = bind_unix_listener(&path)?;
    let handle = tokio::spawn(async move {
        if let Err(err) = serve_unix(listener, EchoBroker).await {
            eprintln!("hsp echo unix server exited: {err}");
        }
    });
    Ok((path, handle))
}

/// Spawn an in-process echo broker on TCP; returns `(bound_addr, join_handle)`.
pub async fn spawn_echo_tcp(
    addr: SocketAddr,
) -> Result<(SocketAddr, tokio::task::JoinHandle<()>), std::io::Error> {
    let listener = TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let handle = tokio::spawn(async move {
        let incoming = TcpListenerStream::new(listener);
        if let Err(err) = Server::builder()
            .add_service(HostedBrokerServer::new(EchoBroker))
            .serve_with_incoming(incoming)
            .await
        {
            eprintln!("hsp echo tcp server exited: {err}");
        }
    });
    Ok((bound, handle))
}

/// Counting wrapper around `EchoBroker` for dial/invoke assertions in tests.
#[derive(Debug, Clone)]
pub struct CountingEchoBroker {
    pub invokes: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

#[tonic::async_trait]
impl HostedBroker for CountingEchoBroker {
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
        self.invokes
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        EchoBroker.invoke(request).await
    }

    async fn on_request(
        &self,
        request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        EchoBroker.on_request(request).await
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

/// Broker that hangs on Invoke until cancelled (timeout tests).
#[derive(Debug, Default, Clone)]
pub struct HangingEchoBroker;

#[tonic::async_trait]
impl HostedBroker for HangingEchoBroker {
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
        _request: tonic::Request<InvokeRequest>,
    ) -> Result<tonic::Response<InvokeResponse>, tonic::Status> {
        // Park forever; client-side timeout must fail closed.
        std::future::pending::<()>().await;
        unreachable!()
    }

    async fn on_request(
        &self,
        request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        EchoBroker.on_request(request).await
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

/// Spawn a counting echo broker on UDS.
#[cfg(unix)]
pub fn spawn_counting_echo_unix(
    path: std::path::PathBuf,
    invokes: std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> std::io::Result<(std::path::PathBuf, tokio::task::JoinHandle<()>)> {
    let listener = bind_unix_listener(&path)?;
    let broker = CountingEchoBroker { invokes };
    let handle = tokio::spawn(async move {
        if let Err(err) = serve_unix_with_broker(listener, broker).await {
            eprintln!("hsp counting echo unix server exited: {err}");
        }
    });
    Ok((path, handle))
}

/// Spawn a hanging echo broker on UDS (Invoke never returns).
#[cfg(unix)]
pub fn spawn_hanging_echo_unix(
    path: std::path::PathBuf,
) -> std::io::Result<(std::path::PathBuf, tokio::task::JoinHandle<()>)> {
    let listener = bind_unix_listener(&path)?;
    let handle = tokio::spawn(async move {
        if let Err(err) = serve_unix_with_broker(listener, HangingEchoBroker).await {
            eprintln!("hsp hanging echo unix server exited: {err}");
        }
    });
    Ok((path, handle))
}

/// Shared server handle for tests that need explicit shutdown.
pub struct EchoServerHandle {
    pub shutdown: tokio::sync::oneshot::Sender<()>,
    pub join: tokio::task::JoinHandle<()>,
}

impl EchoServerHandle {
    pub async fn shutdown(self) {
        let _ = self.shutdown.send(());
        let _ = self.join.await;
    }
}

/// Scripted per-phase hook behavior for integration tests.
#[derive(Debug, Clone)]
#[allow(clippy::large_enum_variant)] // test-only scripting enum; boxing would change the public type
pub enum ScriptedHookBehavior {
    /// Return OK, optionally with an injected context block (OnRequest).
    Ok {
        context: Option<vam_spec::v1::ContextBlock>,
    },
    /// Return FAIL disposition.
    Fail,
    /// Return DEGRADED disposition.
    Degraded,
    /// Sleep past the client budget (timeout).
    Sleep(std::time::Duration),
}

impl Default for ScriptedHookBehavior {
    fn default() -> Self {
        Self::Ok { context: None }
    }
}

/// Configurable test double for hook disposition / timing / inject.
#[derive(Debug, Clone)]
pub struct ScriptedHookBroker {
    pub on_request: ScriptedHookBehavior,
    pub on_complete: ScriptedHookBehavior,
    pub on_response: ScriptedHookBehavior,
    pub on_paused: ScriptedHookBehavior,
    pub on_request_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub on_complete_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub on_response_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
    pub on_paused_calls: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// Tool name from the last `OnPaused` request, when one arrived.
    pub last_paused_tool: std::sync::Arc<std::sync::Mutex<Option<String>>>,
    /// Optional barrier: increment when OnRequest handler starts (parallelism tests).
    pub on_request_started: Option<std::sync::Arc<std::sync::atomic::AtomicU64>>,
    /// Optional hold: wait until this many OnRequest handlers have started before continuing.
    pub wait_for_parallel: Option<(std::sync::Arc<std::sync::atomic::AtomicU64>, u64)>,
    /// Last host-stamped caller context observed on any hook RPC.
    pub last_caller: std::sync::Arc<std::sync::Mutex<Option<vam_spec::v1::CallerContext>>>,
}

impl Default for ScriptedHookBroker {
    fn default() -> Self {
        Self {
            on_request: ScriptedHookBehavior::default(),
            on_complete: ScriptedHookBehavior::default(),
            on_response: ScriptedHookBehavior::default(),
            on_paused: ScriptedHookBehavior::default(),
            on_request_calls: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            on_complete_calls: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            on_response_calls: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            on_paused_calls: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            last_paused_tool: std::sync::Arc::new(std::sync::Mutex::new(None)),
            on_request_started: None,
            wait_for_parallel: None,
            last_caller: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }
}

impl ScriptedHookBroker {
    pub fn with_on_request(mut self, behavior: ScriptedHookBehavior) -> Self {
        self.on_request = behavior;
        self
    }

    pub fn with_on_complete(mut self, behavior: ScriptedHookBehavior) -> Self {
        self.on_complete = behavior;
        self
    }

    pub fn with_on_response(mut self, behavior: ScriptedHookBehavior) -> Self {
        self.on_response = behavior;
        self
    }

    pub fn with_on_paused(mut self, behavior: ScriptedHookBehavior) -> Self {
        self.on_paused = behavior;
        self
    }
}

async fn apply_scripted_on_request(
    broker: &ScriptedHookBroker,
) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
    if let Some(started) = &broker.on_request_started {
        started.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
    if let Some((counter, need)) = &broker.wait_for_parallel {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        while counter.load(std::sync::atomic::Ordering::SeqCst) < *need {
            if tokio::time::Instant::now() > deadline {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    broker
        .on_request_calls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    match &broker.on_request {
        ScriptedHookBehavior::Ok { context } => {
            let mut injected = Vec::new();
            if let Some(block) = context.clone() {
                injected.push(block);
            }
            Ok(tonic::Response::new(OnRequestResponse {
                error: None,
                injected_context: injected,
                disposition: HookDisposition::Ok as i32,
            }))
        }
        ScriptedHookBehavior::Fail => Ok(tonic::Response::new(OnRequestResponse {
            error: None,
            injected_context: vec![],
            disposition: HookDisposition::Fail as i32,
        })),
        ScriptedHookBehavior::Degraded => Ok(tonic::Response::new(OnRequestResponse {
            error: None,
            injected_context: vec![],
            disposition: HookDisposition::Degraded as i32,
        })),
        ScriptedHookBehavior::Sleep(dur) => {
            tokio::time::sleep(*dur).await;
            Ok(tonic::Response::new(OnRequestResponse {
                error: None,
                injected_context: vec![],
                disposition: HookDisposition::Ok as i32,
            }))
        }
    }
}

async fn apply_scripted_on_complete(
    broker: &ScriptedHookBroker,
) -> Result<tonic::Response<OnCompleteResponse>, tonic::Status> {
    broker
        .on_complete_calls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    match &broker.on_complete {
        ScriptedHookBehavior::Ok { .. } => Ok(tonic::Response::new(OnCompleteResponse {
            error: None,
            retention_labels: vec![],
            disposition: HookDisposition::Ok as i32,
        })),
        ScriptedHookBehavior::Fail => Ok(tonic::Response::new(OnCompleteResponse {
            error: None,
            retention_labels: vec![],
            disposition: HookDisposition::Fail as i32,
        })),
        ScriptedHookBehavior::Degraded => Ok(tonic::Response::new(OnCompleteResponse {
            error: None,
            retention_labels: vec![],
            disposition: HookDisposition::Degraded as i32,
        })),
        ScriptedHookBehavior::Sleep(dur) => {
            tokio::time::sleep(*dur).await;
            Ok(tonic::Response::new(OnCompleteResponse {
                error: None,
                retention_labels: vec![],
                disposition: HookDisposition::Ok as i32,
            }))
        }
    }
}

async fn apply_scripted_on_paused(
    broker: &ScriptedHookBroker,
) -> Result<tonic::Response<OnPausedResponse>, tonic::Status> {
    broker
        .on_paused_calls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    match &broker.on_paused {
        ScriptedHookBehavior::Ok { .. } => Ok(tonic::Response::new(OnPausedResponse {
            error: None,
            disposition: HookDisposition::Ok as i32,
        })),
        ScriptedHookBehavior::Fail => Ok(tonic::Response::new(OnPausedResponse {
            error: None,
            disposition: HookDisposition::Fail as i32,
        })),
        ScriptedHookBehavior::Degraded => Ok(tonic::Response::new(OnPausedResponse {
            error: None,
            disposition: HookDisposition::Degraded as i32,
        })),
        ScriptedHookBehavior::Sleep(dur) => {
            tokio::time::sleep(*dur).await;
            Ok(tonic::Response::new(OnPausedResponse {
                error: None,
                disposition: HookDisposition::Ok as i32,
            }))
        }
    }
}

async fn apply_scripted_on_response(
    broker: &ScriptedHookBroker,
) -> Result<tonic::Response<OnResponseResponse>, tonic::Status> {
    broker
        .on_response_calls
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    match &broker.on_response {
        ScriptedHookBehavior::Ok { .. } => Ok(tonic::Response::new(OnResponseResponse {
            error: None,
            response_blocks: vec![],
            disposition: HookDisposition::Ok as i32,
        })),
        ScriptedHookBehavior::Fail => Ok(tonic::Response::new(OnResponseResponse {
            error: None,
            response_blocks: vec![],
            disposition: HookDisposition::Fail as i32,
        })),
        ScriptedHookBehavior::Degraded => Ok(tonic::Response::new(OnResponseResponse {
            error: None,
            response_blocks: vec![],
            disposition: HookDisposition::Degraded as i32,
        })),
        ScriptedHookBehavior::Sleep(dur) => {
            tokio::time::sleep(*dur).await;
            Ok(tonic::Response::new(OnResponseResponse {
                error: None,
                response_blocks: vec![],
                disposition: HookDisposition::Ok as i32,
            }))
        }
    }
}

#[tonic::async_trait]
impl HostedBroker for ScriptedHookBroker {
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
        request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        if let Ok(mut slot) = self.last_caller.lock() {
            *slot = request.get_ref().caller_context.clone();
        }
        apply_scripted_on_request(self).await
    }

    async fn on_complete(
        &self,
        request: tonic::Request<OnCompleteRequest>,
    ) -> Result<tonic::Response<OnCompleteResponse>, tonic::Status> {
        if let Ok(mut slot) = self.last_caller.lock() {
            *slot = request.get_ref().caller_context.clone();
        }
        apply_scripted_on_complete(self).await
    }

    async fn on_response(
        &self,
        request: tonic::Request<OnResponseRequest>,
    ) -> Result<tonic::Response<OnResponseResponse>, tonic::Status> {
        if let Ok(mut slot) = self.last_caller.lock() {
            *slot = request.get_ref().caller_context.clone();
        }
        apply_scripted_on_response(self).await
    }

    async fn on_paused(
        &self,
        request: tonic::Request<OnPausedRequest>,
    ) -> Result<tonic::Response<OnPausedResponse>, tonic::Status> {
        if let Ok(mut slot) = self.last_caller.lock() {
            *slot = request.get_ref().caller_context.clone();
        }
        if let Ok(mut tool) = self.last_paused_tool.lock() {
            *tool = Some(request.get_ref().tool_name.clone());
        }
        apply_scripted_on_paused(self).await
    }
}

/// Spawn a scripted hook broker on UDS.
#[cfg(unix)]
pub fn spawn_scripted_hook_unix(
    path: std::path::PathBuf,
    broker: ScriptedHookBroker,
) -> std::io::Result<(std::path::PathBuf, tokio::task::JoinHandle<()>)> {
    let listener = bind_unix_listener(&path)?;
    let handle = tokio::spawn(async move {
        if let Err(err) = serve_unix_with_broker(listener, broker).await {
            eprintln!("hsp scripted hook unix server exited: {err}");
        }
    });
    Ok((path, handle))
}

/// How a [`ScriptedInvokeBroker`] answers one `Invoke`.
pub type InvokeScript =
    std::sync::Arc<dyn Fn(&InvokeRequest) -> Result<InvokeResponse, tonic::Status> + Send + Sync>;

/// Test broker: registers and lists tools like the echo broker, and answers
/// `Invoke` with whatever `script` returns (a reply with sources, an error
/// that echoes a secret, a large artifact, a gRPC status).
#[derive(Clone)]
pub struct ScriptedInvokeBroker {
    pub script: InvokeScript,
}

impl ScriptedInvokeBroker {
    pub fn new(
        script: impl Fn(&InvokeRequest) -> Result<InvokeResponse, tonic::Status> + Send + Sync + 'static,
    ) -> Self {
        Self {
            script: std::sync::Arc::new(script),
        }
    }
}

#[tonic::async_trait]
impl HostedBroker for ScriptedInvokeBroker {
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
        (self.script)(request.get_ref()).map(tonic::Response::new)
    }

    async fn on_request(
        &self,
        request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        EchoBroker.on_request(request).await
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

/// Spawn a scripted-invoke broker on UDS.
#[cfg(unix)]
pub fn spawn_scripted_invoke_unix(
    path: std::path::PathBuf,
    broker: ScriptedInvokeBroker,
) -> std::io::Result<(std::path::PathBuf, tokio::task::JoinHandle<()>)> {
    let listener = bind_unix_listener(&path)?;
    let handle = tokio::spawn(async move {
        if let Err(err) = serve_unix_with_broker(listener, broker).await {
            eprintln!("hsp scripted invoke unix server exited: {err}");
        }
    });
    Ok((path, handle))
}

/// Spawn echo broker with graceful shutdown via oneshot (tests).
#[cfg(unix)]
pub fn spawn_echo_unix_controlled(
    path: std::path::PathBuf,
) -> std::io::Result<(EchoServerHandle, std::path::PathBuf)> {
    let listener = bind_unix_listener(&path)?;
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        let incoming = UnixListenerStream::new(listener);
        let server = Server::builder().add_service(HostedBrokerServer::new(EchoBroker));
        tokio::select! {
            res = server.serve_with_incoming(incoming) => {
                if let Err(err) = res {
                    eprintln!("hsp echo unix server exited: {err}");
                }
            }
            _ = rx => {}
        }
    });
    Ok((EchoServerHandle { shutdown: tx, join }, path))
}

// ---------------------------------------------------------------------------
// Trigger test broker (E5): `hsp-echo-broker --triggers`.
//
// This stays a test binary in vam-orchestrator, not a crate in `brokers/`: decision 22 puts
// *first-party production* brokers in `brokers/`, where they never depend on vam-orchestrator.
// This one is test scaffolding for the e2e runner (the binary is copied by `scripts/e2e/run.sh`),
// shares `EchoBroker`'s listener helpers, and is never shipped.
// ---------------------------------------------------------------------------

/// The one tool the trigger test broker serves.
pub const ARM_TRIGGER_TOOL: &str = "echo_arm_trigger";

/// File (under the broker's state dir) that holds the grant token, one line, mode 0600.
pub const GRANT_TOKEN_FILE: &str = "grant-token";

/// Firing the broker does on its own after it is armed (`after_ms` in the tool args).
#[derive(Debug, Clone)]
pub struct ArmedFire {
    pub after_ms: u64,
    pub times: u32,
}

/// Fire parameters for [`fire_grant`].
#[derive(Debug, Clone)]
pub struct FireParams {
    pub key: String,
    pub times: u32,
    pub kind: vam_spec::v1::FireKind,
}

/// A broker that exports `echo_arm_trigger` and keeps the `RunGrant` it is handed.
#[derive(Debug, Clone)]
pub struct TriggerTestBroker {
    state_dir: std::path::PathBuf,
    gateway_socket: Option<std::path::PathBuf>,
}

impl TriggerTestBroker {
    pub fn new(state_dir: std::path::PathBuf, gateway_socket: Option<std::path::PathBuf>) -> Self {
        Self {
            state_dir,
            gateway_socket,
        }
    }
}

fn str_value(s: impl Into<String>) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(s.into())),
    }
}

/// Write the token to `<dir>/grant-token` (mode 0600), never anywhere else.
pub fn write_grant_token(dir: &std::path::Path, token: &str) -> std::io::Result<()> {
    use std::io::Write;
    std::fs::create_dir_all(dir)?;
    let path = dir.join(GRANT_TOKEN_FILE);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&path)?;
    #[cfg(unix)]
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    writeln!(f, "{token}")
}

fn num_arg(args: &Option<prost_types::Struct>, key: &str) -> Option<f64> {
    match args.as_ref()?.fields.get(key)?.kind.as_ref()? {
        prost_types::value::Kind::NumberValue(n) => Some(*n),
        _ => None,
    }
}

#[tonic::async_trait]
impl HostedBroker for TriggerTestBroker {
    async fn register(
        &self,
        request: tonic::Request<RegisterRequest>,
    ) -> Result<tonic::Response<RegisterResponse>, tonic::Status> {
        EchoBroker.register(request).await
    }

    async fn list_tools(
        &self,
        _request: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListToolsResponse {
            tools: vec![ToolSpec {
                name: ARM_TRIGGER_TOOL.to_string(),
                description: "Arm a trigger: asks vamd for a run grant".to_string(),
                input_schema_json: r#"{"type":"object","properties":{"run":{"type":"object"},"after_ms":{"type":"number"},"times":{"type":"number"}},"required":["run"]}"#.to_string(),
                execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(
                    HostExecSpec {
                        adapter_id: "broker-echo".to_string(),
                        config: Default::default(),
                    },
                )),
                ..Default::default()
            }],
        }))
    }

    async fn invoke(
        &self,
        request: tonic::Request<InvokeRequest>,
    ) -> Result<tonic::Response<InvokeResponse>, tonic::Status> {
        let req = request.into_inner();
        if req.tool_name != ARM_TRIGGER_TOOL {
            return Err(tonic::Status::not_found(format!(
                "tool {} is not served",
                req.tool_name
            )));
        }
        let Some(grant) = req.run_grant else {
            return Err(tonic::Status::failed_precondition(
                "no run grant on this call",
            ));
        };
        write_grant_token(&self.state_dir, &grant.token)
            .map_err(|e| tonic::Status::internal(format!("grant-token: {e}")))?;

        // Optional firing on its own, after `after_ms`, `times` times with one key.
        if let Some(ms) = num_arg(&req.args, "after_ms") {
            let times = num_arg(&req.args, "times").unwrap_or(1.0).max(1.0) as u32;
            if let Some(gateway) = self.gateway_socket.clone() {
                let token = grant.token.clone();
                let key = format!("armed-{}", grant.grant_id);
                let after = std::time::Duration::from_millis(ms.max(0.0) as u64);
                tokio::spawn(async move {
                    tokio::time::sleep(after).await;
                    let p = FireParams {
                        key,
                        times,
                        kind: vam_spec::v1::FireKind::Scheduled,
                    };
                    match fire_grant(&gateway, &token, &p).await {
                        Ok(lines) => {
                            for l in lines {
                                eprintln!("armed fire: {l}");
                            }
                        }
                        Err(e) => eprintln!("armed fire failed: {e}"),
                    }
                });
            } else {
                eprintln!("after_ms given but the broker has no --gateway-socket");
            }
        }

        // The token is never part of the answer: labels only.
        let mut fields = std::collections::BTreeMap::new();
        fields.insert("grant_id".to_string(), str_value(grant.grant_id));
        if let Some(t) = grant.target {
            fields.insert("agent".to_string(), str_value(t.agent_urn));
            fields.insert("space".to_string(), str_value(t.space_name));
            fields.insert("thread".to_string(), str_value(t.thread_name));
            fields.insert("space_id".to_string(), str_value(t.space_id));
            fields.insert("thread_id".to_string(), str_value(t.thread_id));
        }
        Ok(tonic::Response::new(InvokeResponse {
            result: Some(ToolResult {
                output: Some(prost_types::Struct { fields }),
                error: None,
            }),
            output_blocks: vec![],
            artifact: None,
            sources: Default::default(),
        }))
    }

    async fn on_request(
        &self,
        request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        EchoBroker.on_request(request).await
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

/// Serve the trigger test broker over a Unix socket.
#[cfg(unix)]
pub async fn serve_unix_triggers(
    listener: UnixListener,
    broker: TriggerTestBroker,
) -> Result<(), tonic::transport::Error> {
    serve_unix_with_broker(listener, broker).await
}

/// Fire the grant `times` times against the gateway socket; one JSON line per fire:
/// `{"outcome","runId","denyReason","busyReason"}` (outcome without its enum prefix).
#[cfg(unix)]
pub async fn fire_grant(
    gateway_socket: &Path,
    token: &str,
    p: &FireParams,
) -> Result<Vec<String>, String> {
    use vam_spec::v1::hosted_trigger_gateway_client::HostedTriggerGatewayClient;
    use vam_spec::v1::{BusyReason, DenyReason, RequestRunTriggerRequest, RunTriggerOutcome};

    let path = gateway_socket.to_path_buf();
    let channel = tonic::transport::Endpoint::try_from("http://127.0.0.1")
        .map_err(|e| format!("endpoint: {e}"))?
        .connect_with_connector(tower::service_fn(move |_uri: http::Uri| {
            let path = path.clone();
            async move { tokio::net::UnixStream::connect(path).await }
        }))
        .await
        .map_err(|e| format!("gateway socket {}: {e}", gateway_socket.display()))?;
    let mut client = HostedTriggerGatewayClient::new(channel);
    let mut lines = Vec::new();
    for _ in 0..p.times.max(1) {
        let resp = client
            .request_run_trigger(RequestRunTriggerRequest {
                run_grant: token.to_string(),
                idempotency_key: p.key.clone(),
                fire_at: Some(prost_types::Timestamp {
                    seconds: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                    nanos: 0,
                }),
                kind: p.kind as i32,
                ..Default::default()
            })
            .await
            .map_err(|e| format!("RequestRunTrigger: {e}"))?
            .into_inner();
        let word = |s: &str, prefix: &str| s.strip_prefix(prefix).unwrap_or(s).to_string();
        let outcome = RunTriggerOutcome::try_from(resp.outcome)
            .map(|o| word(o.as_str_name(), "RUN_TRIGGER_OUTCOME_"))
            .unwrap_or_default();
        let deny = DenyReason::try_from(resp.deny_reason)
            .map(|o| word(o.as_str_name(), "DENY_REASON_"))
            .unwrap_or_default();
        let busy = BusyReason::try_from(resp.busy_reason)
            .map(|o| word(o.as_str_name(), "BUSY_REASON_"))
            .unwrap_or_default();
        let unset = |s: String| if s == "UNSPECIFIED" { String::new() } else { s };
        lines.push(
            serde_json::json!({
                "outcome": outcome,
                "runId": resp.run_id.map(|r| r.urn).unwrap_or_default(),
                "denyReason": unset(deny),
                "busyReason": unset(busy),
            })
            .to_string(),
        );
    }
    Ok(lines)
}

#[cfg(all(test, unix))]
mod trigger_broker_tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use vam_spec::v1::hosted_broker_client::HostedBrokerClient;
    use vam_spec::v1::hosted_trigger_gateway_server::{
        HostedTriggerGateway, HostedTriggerGatewayServer,
    };
    use vam_spec::v1::{
        GrantTarget, RequestRunTriggerRequest, RequestRunTriggerResponse, ResourceId,
        RevokeRunGrantRequest, RevokeRunGrantResponse, RunGrant, RunTriggerOutcome,
    };

    #[derive(Clone, Default)]
    struct FakeGateway {
        seen: Arc<Mutex<Vec<(String, String, i32)>>>,
    }

    #[tonic::async_trait]
    impl HostedTriggerGateway for FakeGateway {
        async fn request_run_trigger(
            &self,
            request: tonic::Request<RequestRunTriggerRequest>,
        ) -> Result<tonic::Response<RequestRunTriggerResponse>, tonic::Status> {
            let r = request.into_inner();
            let mut seen = self.seen.lock().unwrap();
            let dup = seen.iter().any(|s| s.1 == r.idempotency_key);
            seen.push((r.run_grant, r.idempotency_key, r.kind));
            Ok(tonic::Response::new(RequestRunTriggerResponse {
                outcome: if dup {
                    RunTriggerOutcome::Duplicate as i32
                } else {
                    RunTriggerOutcome::Accepted as i32
                },
                run_id: Some(ResourceId {
                    urn: "urn:vam:run:local/x/r1".into(),
                }),
                ..Default::default()
            }))
        }
        async fn revoke_run_grant(
            &self,
            _r: tonic::Request<RevokeRunGrantRequest>,
        ) -> Result<tonic::Response<RevokeRunGrantResponse>, tonic::Status> {
            Ok(tonic::Response::new(RevokeRunGrantResponse::default()))
        }
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
    async fn hsp_echo_trigger_broker_keeps_token_and_fires_through_fake_gateway() {
        let dir = tempfile::tempdir().unwrap();
        let gw_sock = dir.path().join("gw.sock");
        let gw = FakeGateway::default();
        let seen = gw.seen.clone();
        let l = bind_unix_listener(&gw_sock).unwrap();
        tokio::spawn(
            Server::builder()
                .add_service(HostedTriggerGatewayServer::new(gw))
                .serve_with_incoming(UnixListenerStream::new(l)),
        );
        let b_sock = dir.path().join("b.sock");
        let l = bind_unix_listener(&b_sock).unwrap();
        tokio::spawn(serve_unix_triggers(
            l,
            TriggerTestBroker::new(dir.path().to_path_buf(), Some(gw_sock.clone())),
        ));

        let mut c = broker_client(&b_sock).await;
        let tools = c.list_tools(ListToolsRequest::default()).await.unwrap();
        assert_eq!(tools.into_inner().tools[0].name, ARM_TRIGGER_TOOL);

        // No grant on the call: refused, nothing written.
        let none = InvokeRequest {
            tool_name: ARM_TRIGGER_TOOL.into(),
            ..Default::default()
        };
        assert!(c.invoke(none).await.is_err());
        assert!(!dir.path().join(GRANT_TOKEN_FILE).exists());

        let token = "vam_rg_rg_ABC.secretsecret";
        let req = InvokeRequest {
            tool_name: ARM_TRIGGER_TOOL.into(),
            run_grant: Some(RunGrant {
                token: token.into(),
                grant_id: "rg_ABC".into(),
                target: Some(GrantTarget {
                    space_name: "Ops".into(),
                    thread_name: "Daily".into(),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };
        let resp = c.invoke(req).await.unwrap().into_inner();
        let out = format!("{:?}", resp.result);
        assert!(out.contains("Ops") && out.contains("Daily"), "{out}");
        assert!(!out.contains("secretsecret"), "token leaked: {out}");

        let file = dir.path().join(GRANT_TOKEN_FILE);
        assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), token);
        let mode = std::fs::metadata(&file).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let lines = fire_grant(
            &gw_sock,
            token,
            &FireParams {
                key: "k1".into(),
                times: 2,
                kind: vam_spec::v1::FireKind::Scheduled,
            },
        )
        .await
        .unwrap();
        let v: Vec<serde_json::Value> = lines
            .iter()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(v[0]["outcome"], "ACCEPTED");
        assert_eq!(v[1]["outcome"], "DUPLICATE");
        assert_eq!(v[0]["runId"], "urn:vam:run:local/x/r1");
        assert_eq!(v[0]["denyReason"], "");
        let s = seen.lock().unwrap();
        assert!(s.iter().all(|x| x.0 == token && x.1 == "k1"));
    }
}
