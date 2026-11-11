//! The trigger gateway client: `RequestRunTrigger` and `RevokeRunGrant` over vamd's UDS.
//!
//! Its own module on the generated `vam-spec` client (work-outside-chat Q6); the shape of
//! `hsp_echo::fire_grant`, not shared with it. It connects per call: vamd opens the gateway after
//! the broker is registered and the socket can come and go, so no channel is kept.

use std::path::{Path, PathBuf};
use std::time::Duration;

use vam_spec::v1::hosted_trigger_gateway_client::HostedTriggerGatewayClient;
pub use vam_spec::v1::FireKind;
use vam_spec::v1::{
    BusyReason, DenyReason, RequestRunTriggerRequest, RevokeRunGrantRequest, RunTriggerOutcome,
};

use crate::grant::GrantToken;

/// The default for one connect plus one call.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

/// One fire. Times are UTC seconds.
#[derive(Debug, Clone)]
pub struct FireRequest {
    pub token: GrantToken,
    pub idempotency_key: String,
    pub fire_at: i64,
    pub trigger_ref: String,
    pub kind: FireKind,
    pub not_after: Option<i64>,
}

/// What the gateway answered, typed. Wave 3 maps these onto the design's outcome table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    Accepted {
        run_id: String,
    },
    Queued {
        run_id: String,
    },
    Duplicate {
        run_id: String,
    },
    Busy {
        reason: BusyReason,
        retry_after_ms: u32,
    },
    RateLimited {
        retry_after_ms: u32,
    },
    Unavailable {
        retry_after_ms: u32,
    },
    Denied {
        reason: DenyReason,
    },
}

/// Why a call produced no answer. All of these are retryable except `BadResponse`, which is a
/// gateway bug and is treated by the caller as unavailable.
#[derive(Debug, thiserror::Error)]
pub enum GatewayError {
    #[error("no gateway socket configured (start the broker with --gateway-socket)")]
    NotConfigured,
    #[error("gateway socket {}: {message}", path.display())]
    Connect { path: PathBuf, message: String },
    #[error("gateway call timed out after {0:?}")]
    Timeout(Duration),
    #[error("gateway status {code}: {message}")]
    Status { code: String, message: String },
    #[error("gateway answered something this broker cannot read: {0}")]
    BadResponse(String),
}

/// The client. Cheap to clone; holds only the socket path.
#[derive(Debug, Clone)]
pub struct GatewayClient {
    socket: Option<PathBuf>,
    timeout: Duration,
}

impl GatewayClient {
    pub fn new(socket: Option<PathBuf>) -> Self {
        Self {
            socket,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn is_configured(&self) -> bool {
        self.socket.is_some()
    }

    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    /// `RequestRunTrigger`.
    pub async fn fire(&self, req: &FireRequest) -> Result<FireOutcome, GatewayError> {
        let timeout = self.timeout;
        let call = async {
            let mut client = self.connect().await?;
            let resp = client
                .request_run_trigger(RequestRunTriggerRequest {
                    run_grant: req.token.expose().to_string(),
                    idempotency_key: req.idempotency_key.clone(),
                    fire_at: Some(timestamp(req.fire_at)),
                    trigger_ref: req.trigger_ref.clone(),
                    kind: req.kind as i32,
                    not_after: req.not_after.map(timestamp),
                    trace_context: None,
                })
                .await
                .map_err(status_error)?
                .into_inner();
            let run_id = resp.run_id.map(|r| r.urn).unwrap_or_default();
            let outcome = RunTriggerOutcome::try_from(resp.outcome)
                .map_err(|_| GatewayError::BadResponse(format!("outcome {}", resp.outcome)))?;
            let retry_after_ms = u32::try_from(resp.retry_after_ms).unwrap_or(0);
            Ok(match outcome {
                RunTriggerOutcome::Accepted => FireOutcome::Accepted { run_id },
                RunTriggerOutcome::Queued => FireOutcome::Queued { run_id },
                RunTriggerOutcome::Duplicate => FireOutcome::Duplicate { run_id },
                RunTriggerOutcome::Busy => FireOutcome::Busy {
                    reason: BusyReason::try_from(resp.busy_reason)
                        .unwrap_or(BusyReason::Unspecified),
                    retry_after_ms,
                },
                RunTriggerOutcome::RateLimited => FireOutcome::RateLimited { retry_after_ms },
                RunTriggerOutcome::Unavailable => FireOutcome::Unavailable { retry_after_ms },
                RunTriggerOutcome::Denied => FireOutcome::Denied {
                    reason: DenyReason::try_from(resp.deny_reason)
                        .unwrap_or(DenyReason::Unspecified),
                },
                RunTriggerOutcome::Unspecified => {
                    return Err(GatewayError::BadResponse("outcome UNSPECIFIED".into()))
                }
            })
        };
        let outcome = within(timeout, call).await;
        // Ids and outcomes only: never the token, the message or the args.
        match &outcome {
            Ok(o) => {
                tracing::info!(trigger_ref = %req.trigger_ref, outcome = ?o, "gateway fire answered")
            }
            Err(e) => {
                tracing::warn!(trigger_ref = %req.trigger_ref, error = %e, "gateway fire failed")
            }
        }
        outcome
    }

    /// `RevokeRunGrant`. `Ok` means the gateway answered without an error (it is idempotent).
    pub async fn revoke(&self, token: &GrantToken) -> Result<(), GatewayError> {
        let timeout = self.timeout;
        let call = async {
            let mut client = self.connect().await?;
            let resp = client
                .revoke_run_grant(RevokeRunGrantRequest {
                    run_grant: token.expose().to_string(),
                })
                .await
                .map_err(status_error)?
                .into_inner();
            match resp.error {
                None => Ok(()),
                Some(e) => Err(GatewayError::Status {
                    code: format!("{}", e.code),
                    message: e.message,
                }),
            }
        };
        let outcome = within(timeout, call).await;
        match &outcome {
            Ok(()) => tracing::info!("gateway revoke answered"),
            Err(e) => tracing::warn!(error = %e, "gateway revoke failed"),
        }
        outcome
    }

    async fn connect(
        &self,
    ) -> Result<HostedTriggerGatewayClient<tonic::transport::Channel>, GatewayError> {
        let path = self.socket.clone().ok_or(GatewayError::NotConfigured)?;
        let dial = path.clone();
        let channel = tonic::transport::Endpoint::try_from("http://127.0.0.1")
            .map_err(|e| GatewayError::Connect {
                path: path.clone(),
                message: e.to_string(),
            })?
            .connect_with_connector(tower::service_fn(move |_uri: http::Uri| {
                let dial = dial.clone();
                async move { tokio::net::UnixStream::connect(dial).await }
            }))
            .await
            .map_err(|e| GatewayError::Connect {
                path,
                message: e.to_string(),
            })?;
        Ok(HostedTriggerGatewayClient::new(channel))
    }
}

async fn within<T>(
    timeout: Duration,
    fut: impl std::future::Future<Output = Result<T, GatewayError>>,
) -> Result<T, GatewayError> {
    tokio::time::timeout(timeout, fut)
        .await
        .unwrap_or(Err(GatewayError::Timeout(timeout)))
}

fn timestamp(secs: i64) -> prost_types::Timestamp {
    prost_types::Timestamp {
        seconds: secs,
        nanos: 0,
    }
}

fn status_error(s: tonic::Status) -> GatewayError {
    GatewayError::Status {
        code: format!("{:?}", s.code()),
        message: s.message().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn no_socket_is_not_configured() {
        let c = GatewayClient::new(None);
        assert!(!c.is_configured());
        let err = c.revoke(&GrantToken::new("t")).await.unwrap_err();
        assert!(matches!(err, GatewayError::NotConfigured));
    }

    #[tokio::test]
    async fn a_missing_socket_is_a_connect_error_without_the_token() {
        let t = tempfile::tempdir().unwrap();
        let c = GatewayClient::new(Some(t.path().join("none.sock")));
        let err = c
            .fire(&FireRequest {
                token: GrantToken::new("vam_rg_x.topsecret"),
                idempotency_key: "k".into(),
                fire_at: 1,
                trigger_ref: "sch_x".into(),
                kind: FireKind::Scheduled,
                not_after: None,
            })
            .await
            .unwrap_err();
        assert!(matches!(err, GatewayError::Connect { .. }), "{err}");
        assert!(!format!("{err} {err:?}").contains("topsecret"));
    }
}
