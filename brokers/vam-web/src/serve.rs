//! Transport helpers: serve any `HostedBroker` over UDS or TCP.

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

/// Bind a UDS listener, removing a stale socket file and setting mode 0600.
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

/// Serve over a Unix domain socket until the listener closes.
#[cfg(unix)]
pub async fn serve_unix<B: HostedBroker>(
    listener: UnixListener,
    broker: B,
) -> Result<(), tonic::transport::Error> {
    Server::builder()
        .add_service(HostedBrokerServer::new(broker))
        .serve_with_incoming(UnixListenerStream::new(listener))
        .await
}

/// Serve over TCP (tests and debugging).
pub async fn serve_tcp<B: HostedBroker>(
    addr: SocketAddr,
    broker: B,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let listener = TcpListener::bind(addr).await?;
    serve_tcp_listener(listener, broker).await?;
    Ok(())
}

/// Serve on an already bound TCP listener.
pub async fn serve_tcp_listener<B: HostedBroker>(
    listener: TcpListener,
    broker: B,
) -> Result<(), tonic::transport::Error> {
    Server::builder()
        .add_service(HostedBrokerServer::new(broker))
        .serve_with_incoming(TcpListenerStream::new(listener))
        .await
}
