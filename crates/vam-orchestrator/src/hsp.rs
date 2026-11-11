//! HSP (Host Service Protocol) client over UDS or TCP loopback.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use tonic::transport::{Channel, Endpoint};
use vam_spec::v1::hosted_broker_client::HostedBrokerClient;
use vam_spec::v1::hosted_notification_channel_client::HostedNotificationChannelClient;
use vam_spec::v1::{
    DeliverNotificationRequest, DeliverNotificationResponse, InvokeRequest, InvokeResponse,
    ListToolsRequest, ListToolsResponse, OnCompleteRequest, OnCompleteResponse, OnPausedRequest,
    OnPausedResponse, OnRequestRequest, OnRequestResponse, OnResponseRequest, OnResponseResponse,
    RegisterRequest, RegisterResponse,
};

use crate::error::OrchestratorError;

/// Where to reach a hosted broker process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HspEndpoint {
    /// Product transport: gRPC over a Unix domain socket path.
    Unix(PathBuf),
    /// Tests/debug only: gRPC over TCP loopback.
    Tcp(SocketAddr),
}

/// What became of a request sent with [`HspClient::invoke_once`] that did not succeed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InvokeFate {
    /// The request never left the host (no connection).
    NotSent,
    /// The broker answered with a refusal: a definite failure.
    Answered,
    /// The request left and no definite answer came back (timeout, transport loss).
    Lost,
}

/// gRPC client for the `HostedBroker` service with hard RPC timeouts and one-shot reconnect.
#[derive(Debug)]
pub struct HspClient {
    endpoint: HspEndpoint,
    timeout: Duration,
    inner: Option<HostedBrokerClient<Channel>>,
}

impl HspClient {
    /// Default RPC timeout when none is configured.
    pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

    pub fn new(endpoint: HspEndpoint) -> Self {
        Self {
            endpoint,
            timeout: Self::DEFAULT_TIMEOUT,
            inner: None,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn endpoint(&self) -> &HspEndpoint {
        &self.endpoint
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Connect to the broker and return a ready client.
    pub async fn connect(endpoint: HspEndpoint) -> Result<Self, OrchestratorError> {
        let mut client = Self::new(endpoint);
        client.reconnect().await?;
        Ok(client)
    }

    pub async fn connect_with_timeout(
        endpoint: HspEndpoint,
        timeout: Duration,
    ) -> Result<Self, OrchestratorError> {
        Ok(Self::connect(endpoint).await?.with_timeout(timeout))
    }

    /// Phase 1: forward register RPC; no manifest approval or persistence.
    pub async fn register(
        &mut self,
        request: RegisterRequest,
    ) -> Result<RegisterResponse, OrchestratorError> {
        match self.try_register(&request).await {
            Ok(response) => Ok(response),
            Err(err) if is_transport_error(&err) => {
                self.inner = None;
                self.reconnect().await?;
                self.try_register(&request).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn invoke(
        &mut self,
        request: InvokeRequest,
    ) -> Result<InvokeResponse, OrchestratorError> {
        match self.try_invoke(&request).await {
            Ok(response) => Ok(response),
            Err(err) if is_transport_error(&err) => {
                self.inner = None;
                self.reconnect().await?;
                self.try_invoke(&request).await
            }
            Err(err) => Err(err),
        }
    }

    /// Sends an invoke exactly once: no reconnect and no second try. A call that may mutate
    /// uses this, so a lost connection leaves the outcome unknown instead of repeating the call.
    /// The error says whether the request left the host and whether the broker answered.
    pub async fn invoke_once(
        &mut self,
        request: InvokeRequest,
    ) -> Result<InvokeResponse, (OrchestratorError, InvokeFate)> {
        if let Err(err) = self.ensure_connected().await {
            return Err((err, InvokeFate::NotSent));
        }
        let Some(client) = self.inner.as_mut() else {
            return Err((not_connected(), InvokeFate::NotSent));
        };
        match tokio::time::timeout(self.timeout, client.invoke(request)).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => {
                let answered = matches!(
                    status.code(),
                    tonic::Code::InvalidArgument
                        | tonic::Code::NotFound
                        | tonic::Code::AlreadyExists
                        | tonic::Code::PermissionDenied
                        | tonic::Code::Unauthenticated
                        | tonic::Code::FailedPrecondition
                        | tonic::Code::OutOfRange
                        | tonic::Code::Unimplemented
                        | tonic::Code::ResourceExhausted
                );
                Err((
                    map_status(status),
                    if answered {
                        InvokeFate::Answered
                    } else {
                        InvokeFate::Lost
                    },
                ))
            }
            Err(_) => Err((
                OrchestratorError::Hsp(format!(
                    "invoke timed out after {}ms",
                    self.timeout.as_millis()
                )),
                InvokeFate::Lost,
            )),
        }
    }

    pub async fn list_tools(
        &mut self,
        request: ListToolsRequest,
    ) -> Result<ListToolsResponse, OrchestratorError> {
        match self.try_list_tools(&request).await {
            Ok(response) => Ok(response),
            Err(err) if is_transport_error(&err) => {
                self.inner = None;
                self.reconnect().await?;
                self.try_list_tools(&request).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn on_request(
        &mut self,
        request: OnRequestRequest,
    ) -> Result<OnRequestResponse, OrchestratorError> {
        match self.try_on_request(&request).await {
            Ok(response) => Ok(response),
            Err(err) if is_transport_error(&err) => {
                self.inner = None;
                self.reconnect().await?;
                self.try_on_request(&request).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn on_complete(
        &mut self,
        request: OnCompleteRequest,
    ) -> Result<OnCompleteResponse, OrchestratorError> {
        match self.try_on_complete(&request).await {
            Ok(response) => Ok(response),
            Err(err) if is_transport_error(&err) => {
                self.inner = None;
                self.reconnect().await?;
                self.try_on_complete(&request).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn on_response(
        &mut self,
        request: OnResponseRequest,
    ) -> Result<OnResponseResponse, OrchestratorError> {
        match self.try_on_response(&request).await {
            Ok(response) => Ok(response),
            Err(err) if is_transport_error(&err) => {
                self.inner = None;
                self.reconnect().await?;
                self.try_on_response(&request).await
            }
            Err(err) => Err(err),
        }
    }

    pub async fn on_paused(
        &mut self,
        request: OnPausedRequest,
    ) -> Result<OnPausedResponse, OrchestratorError> {
        match self.try_on_paused(&request).await {
            Ok(response) => Ok(response),
            Err(err) if is_transport_error(&err) => {
                self.inner = None;
                self.reconnect().await?;
                self.try_on_paused(&request).await
            }
            Err(err) => Err(err),
        }
    }

    async fn try_register(
        &mut self,
        request: &RegisterRequest,
    ) -> Result<RegisterResponse, OrchestratorError> {
        self.ensure_connected().await?;
        let Some(client) = self.inner.as_mut() else {
            return Err(not_connected());
        };
        match tokio::time::timeout(self.timeout, client.register(request.clone())).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => Err(map_status(status)),
            Err(_) => Err(OrchestratorError::Hsp(format!(
                "register timed out after {}ms",
                self.timeout.as_millis()
            ))),
        }
    }

    async fn try_invoke(
        &mut self,
        request: &InvokeRequest,
    ) -> Result<InvokeResponse, OrchestratorError> {
        self.ensure_connected().await?;
        let Some(client) = self.inner.as_mut() else {
            return Err(not_connected());
        };
        match tokio::time::timeout(self.timeout, client.invoke(request.clone())).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => Err(map_status(status)),
            Err(_) => Err(OrchestratorError::Hsp(format!(
                "invoke timed out after {}ms",
                self.timeout.as_millis()
            ))),
        }
    }

    async fn try_list_tools(
        &mut self,
        request: &ListToolsRequest,
    ) -> Result<ListToolsResponse, OrchestratorError> {
        self.ensure_connected().await?;
        let Some(client) = self.inner.as_mut() else {
            return Err(not_connected());
        };
        match tokio::time::timeout(self.timeout, client.list_tools(request.clone())).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => Err(map_status(status)),
            Err(_) => Err(OrchestratorError::Hsp(format!(
                "list_tools timed out after {}ms",
                self.timeout.as_millis()
            ))),
        }
    }

    async fn try_on_request(
        &mut self,
        request: &OnRequestRequest,
    ) -> Result<OnRequestResponse, OrchestratorError> {
        self.ensure_connected().await?;
        let Some(client) = self.inner.as_mut() else {
            return Err(not_connected());
        };
        match tokio::time::timeout(self.timeout, client.on_request(request.clone())).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => Err(map_status(status)),
            Err(_) => Err(OrchestratorError::Hsp(format!(
                "on_request timed out after {}ms",
                self.timeout.as_millis()
            ))),
        }
    }

    async fn try_on_complete(
        &mut self,
        request: &OnCompleteRequest,
    ) -> Result<OnCompleteResponse, OrchestratorError> {
        self.ensure_connected().await?;
        let Some(client) = self.inner.as_mut() else {
            return Err(not_connected());
        };
        match tokio::time::timeout(self.timeout, client.on_complete(request.clone())).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => Err(map_status(status)),
            Err(_) => Err(OrchestratorError::Hsp(format!(
                "on_complete timed out after {}ms",
                self.timeout.as_millis()
            ))),
        }
    }

    async fn try_on_response(
        &mut self,
        request: &OnResponseRequest,
    ) -> Result<OnResponseResponse, OrchestratorError> {
        self.ensure_connected().await?;
        let Some(client) = self.inner.as_mut() else {
            return Err(not_connected());
        };
        match tokio::time::timeout(self.timeout, client.on_response(request.clone())).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => Err(map_status(status)),
            Err(_) => Err(OrchestratorError::Hsp(format!(
                "on_response timed out after {}ms",
                self.timeout.as_millis()
            ))),
        }
    }

    async fn try_on_paused(
        &mut self,
        request: &OnPausedRequest,
    ) -> Result<OnPausedResponse, OrchestratorError> {
        self.ensure_connected().await?;
        let Some(client) = self.inner.as_mut() else {
            return Err(not_connected());
        };
        match tokio::time::timeout(self.timeout, client.on_paused(request.clone())).await {
            Ok(Ok(response)) => Ok(response.into_inner()),
            Ok(Err(status)) => Err(map_status(status)),
            Err(_) => Err(OrchestratorError::Hsp(format!(
                "on_paused timed out after {}ms",
                self.timeout.as_millis()
            ))),
        }
    }

    async fn ensure_connected(&mut self) -> Result<(), OrchestratorError> {
        if self.inner.is_none() {
            self.reconnect().await?;
        }
        Ok(())
    }

    async fn reconnect(&mut self) -> Result<(), OrchestratorError> {
        let channel = match &self.endpoint {
            HspEndpoint::Unix(path) => connect_unix(path).await?,
            HspEndpoint::Tcp(addr) => connect_tcp(*addr).await?,
        };
        self.inner = Some(HostedBrokerClient::new(channel));
        Ok(())
    }
}

/// One `HostedNotificationChannel.DeliverNotification` call on the broker's HSP endpoint (NT-2.3).
///
/// A fresh connection per call: the delivery worker keeps no client, and the request carries a
/// bot token that must not outlive the call. The request is consumed and dropped here; nothing in
/// this function logs it. Idempotency is the broker's, by `delivery_id`; this sends exactly once
/// and leaves any retry to the caller's schedule.
pub async fn deliver_notification(
    endpoint: &HspEndpoint,
    request: DeliverNotificationRequest,
    timeout: Duration,
) -> Result<DeliverNotificationResponse, OrchestratorError> {
    let channel = match endpoint {
        HspEndpoint::Unix(path) => connect_unix(path).await?,
        HspEndpoint::Tcp(addr) => connect_tcp(*addr).await?,
    };
    let mut client = HostedNotificationChannelClient::new(channel);
    match tokio::time::timeout(timeout, client.deliver_notification(request)).await {
        Ok(Ok(response)) => Ok(response.into_inner()),
        Ok(Err(status)) => Err(map_status(status)),
        Err(_) => Err(OrchestratorError::Hsp(format!(
            "deliver_notification timed out after {}ms",
            timeout.as_millis()
        ))),
    }
}

#[allow(clippy::ptr_arg)] // the connector closure below needs an owned PathBuf clone for 'static
async fn connect_unix(path: &PathBuf) -> Result<Channel, OrchestratorError> {
    #[cfg(unix)]
    {
        let path = path.clone();
        Endpoint::try_from("http://127.0.0.1")
            .map_err(|err| OrchestratorError::Hsp(format!("invalid endpoint uri: {err}")))?
            .connect_with_connector(tower::service_fn(move |_uri: http::Uri| {
                let path = path.clone();
                async move { tokio::net::UnixStream::connect(path).await }
            }))
            .await
            .map_err(|err| OrchestratorError::Hsp(format!("uds connect failed: {err}")))
    }
    #[cfg(not(unix))]
    {
        Err(OrchestratorError::Hsp(format!(
            "UDS transport not supported on non-Unix platforms (path: {})",
            path.display()
        )))
    }
}

async fn connect_tcp(addr: SocketAddr) -> Result<Channel, OrchestratorError> {
    Endpoint::from_shared(format!("http://{addr}"))
        .map_err(|err| OrchestratorError::Hsp(format!("invalid tcp uri: {err}")))?
        .connect()
        .await
        .map_err(|err| OrchestratorError::Hsp(format!("tcp connect failed: {err}")))
}

fn map_status(status: tonic::Status) -> OrchestratorError {
    OrchestratorError::Hsp(format!("rpc {}: {}", status.code(), status.message()))
}

/// The broker client has no live connection after `ensure_connected`: an error, never a panic.
fn not_connected() -> OrchestratorError {
    OrchestratorError::Hsp("broker client is not connected".to_string())
}

fn is_transport_error(err: &OrchestratorError) -> bool {
    match err {
        OrchestratorError::Hsp(message) => {
            // Deliberate RPC timeouts must fail closed without a second full wait.
            if message.contains("timed out") {
                return false;
            }
            message.contains("uds connect failed")
                || message.contains("tcp connect failed")
                || message.contains("client is not connected")
                || message.contains("Unavailable")
                || message.contains("Unknown")
        }
        _ => false,
    }
}

#[cfg(all(test, unix))]
mod tests {
    #[test]
    fn a_missing_connection_is_an_error_not_a_panic() {
        let err = super::not_connected();
        assert!(matches!(err, OrchestratorError::Hsp(m) if m.contains("not connected")));
    }

    use super::*;
    use crate::hsp_echo::spawn_echo_unix;
    use prost_types::value::Kind;
    use tempfile::tempdir;
    use vam_spec::v1::CapabilityManifest;

    #[tokio::test]
    async fn client_register_and_invoke_round_trip() {
        let dir = tempdir().unwrap();
        let sock = dir.path().join("echo.sock");
        let (_path, _handle) = spawn_echo_unix(sock.clone()).unwrap();

        let mut client = HspClient::connect(HspEndpoint::Unix(sock)).await.unwrap();
        let register = client
            .register(RegisterRequest {
                broker_id: "echo".to_string(),
                broker_version: "0.1.0".to_string(),
                manifest: Some(CapabilityManifest {
                    broker_id: "echo".to_string(),
                    trust_tier: 2,
                    secret_bindings: vec![],
                    egress_domains: vec![],
                    hooks: vec![],
                    tools: vec![],
                    depends_on: vec![],
                    labels: Default::default(),
                    identity_requirement: None,
                    run_request_permission: None,
                    notification_channel: None,
                }),
            })
            .await
            .unwrap();
        assert!(register.error.is_none());

        let invoke = client
            .invoke(InvokeRequest {
                session_id: "sess".to_string(),
                run_id: "run".to_string(),
                tool_name: "echo.ping".to_string(),
                args: Some(prost_types::Struct {
                    fields: [(
                        "message".to_string(),
                        prost_types::Value {
                            kind: Some(Kind::StringValue("hello".to_string())),
                        },
                    )]
                    .into(),
                }),
                resolved_secrets: Default::default(),
                context: None,
                trace_context: None,
                caller_context: None,
                run_grant: None,
                idempotency_key: String::new(),
            })
            .await
            .unwrap();

        let output = invoke.result.unwrap().output.unwrap();
        let tool_name = output.fields.get("tool_name").unwrap();
        assert!(matches!(
            tool_name.kind,
            Some(Kind::StringValue(ref name)) if name == "echo.ping"
        ));
    }
}
