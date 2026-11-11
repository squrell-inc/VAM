//! Serve `HostedBroker` and `HostedNotificationChannel` on one socket.

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tokio::net::UnixListener;
use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use vam_spec::v1::hosted_broker_server::HostedBrokerServer;
use vam_spec::v1::hosted_notification_channel_server::HostedNotificationChannelServer;

use crate::broker::NotifyBroker;

/// Bind a UDS listener, removing a stale socket file and setting mode 0600.
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
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}

pub async fn serve_unix(
    listener: UnixListener,
    broker: NotifyBroker,
) -> Result<(), tonic::transport::Error> {
    Server::builder()
        .add_service(HostedBrokerServer::new(broker.clone()))
        .add_service(HostedNotificationChannelServer::new(broker))
        .serve_with_incoming(UnixListenerStream::new(listener))
        .await
}
