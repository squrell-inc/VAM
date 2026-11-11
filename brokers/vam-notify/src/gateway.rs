//! The notification gateway client: `ActOnNotification`, `LinkChannelAccount`, `GetChannelConfig`
//! over vamd's per-broker UDS. Connects per call; the socket can come and go.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tonic::transport::{Channel, Endpoint, Uri};
use vam_spec::v1::hosted_notification_gateway_client::HostedNotificationGatewayClient;
use vam_spec::v1::{
    ActOnNotificationRequest, ActOnNotificationResponse, GetChannelConfigRequest,
    GetChannelConfigResponse, LinkChannelAccountRequest, LinkChannelAccountResponse,
};

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(10);

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
}

#[derive(Debug, Clone, Default)]
pub struct GatewayClient {
    socket: Option<PathBuf>,
    timeout: Option<Duration>,
}

impl GatewayClient {
    pub fn new(socket: Option<PathBuf>) -> Self {
        Self {
            socket,
            timeout: None,
        }
    }

    pub fn is_configured(&self) -> bool {
        self.socket.is_some()
    }

    pub fn socket(&self) -> Option<&Path> {
        self.socket.as_deref()
    }

    async fn connect(&self) -> Result<HostedNotificationGatewayClient<Channel>, GatewayError> {
        let path = self.socket.clone().ok_or(GatewayError::NotConfigured)?;
        let dial = path.clone();
        let channel = Endpoint::try_from("http://[::]:50051")
            .map_err(|e| GatewayError::Connect {
                path: path.clone(),
                message: e.to_string(),
            })?
            .connect_with_connector(tower::service_fn(move |_uri: Uri| {
                let dial = dial.clone();
                async move { tokio::net::UnixStream::connect(dial).await }
            }))
            .await
            .map_err(|e| GatewayError::Connect {
                path,
                message: e.to_string(),
            })?;
        Ok(HostedNotificationGatewayClient::new(channel))
    }

    async fn within<T>(
        &self,
        fut: impl std::future::Future<Output = Result<T, GatewayError>>,
    ) -> Result<T, GatewayError> {
        let t = self.timeout.unwrap_or(DEFAULT_TIMEOUT);
        tokio::time::timeout(t, fut)
            .await
            .map_err(|_| GatewayError::Timeout(t))?
    }

    pub async fn act(
        &self,
        req: ActOnNotificationRequest,
    ) -> Result<ActOnNotificationResponse, GatewayError> {
        self.within(async {
            let mut c = self.connect().await?;
            Ok(c.act_on_notification(req)
                .await
                .map_err(status)?
                .into_inner())
        })
        .await
    }

    pub async fn link(
        &self,
        req: LinkChannelAccountRequest,
    ) -> Result<LinkChannelAccountResponse, GatewayError> {
        self.within(async {
            let mut c = self.connect().await?;
            Ok(c.link_channel_account(req)
                .await
                .map_err(status)?
                .into_inner())
        })
        .await
    }

    pub async fn config(
        &self,
        req: GetChannelConfigRequest,
    ) -> Result<GetChannelConfigResponse, GatewayError> {
        self.within(async {
            let mut c = self.connect().await?;
            Ok(c.get_channel_config(req)
                .await
                .map_err(status)?
                .into_inner())
        })
        .await
    }
}

fn status(s: tonic::Status) -> GatewayError {
    GatewayError::Status {
        code: format!("{:?}", s.code()),
        message: s.message().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn unconfigured_client_refuses() {
        let c = GatewayClient::new(None);
        assert!(!c.is_configured());
        let err = c
            .config(GetChannelConfigRequest::default())
            .await
            .unwrap_err();
        assert!(matches!(err, GatewayError::NotConfigured));
    }
}
