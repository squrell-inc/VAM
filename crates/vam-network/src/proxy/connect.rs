//! `CONNECT` tunnel passthrough.
//!
//! The gate chooses passthrough. This module dials the pinned address from
//! that decision, then copies bytes until EOF, quota, or idle timeout.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use hyper_util::rt::TokioIo;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::net::TcpStream;
use tower_service::Service;

use crate::gate::{EgressGate, GateError};

/// No bytes in either direction for this long closes the tunnel.
const TUNNEL_IDLE: Duration = Duration::from_secs(120);

pub(super) enum PassthroughError {
    /// Dial failed. The `200` response has not been written.
    Dial,
    Io(std::io::Error),
}

/// Dial `addr`, announce the tunnel, and copy bytes both ways.
///
/// `addr` is the gate's pinned socket address. This function does not resolve
/// a name. Byte counts from both directions go through [`EgressGate::record_bytes`];
/// [`GateError::QuotaExceeded`] closes the tunnel.
pub(super) async fn tunnel_passthrough<S>(
    client: &mut S,
    addr: std::net::SocketAddr,
    gate: Arc<dyn EgressGate>,
) -> Result<(), PassthroughError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let upstream = match TcpStream::connect(addr).await {
        Ok(stream) => stream,
        Err(err) => {
            tracing::debug!(error = %err, "egress connect dial failed");
            return Err(PassthroughError::Dial);
        }
    };
    let _ = upstream.set_nodelay(true);
    write_established(client).await?;

    let mut metered = Metered::new(client, gate, TUNNEL_IDLE);
    let mut upstream = upstream;
    match tokio::io::copy_bidirectional(&mut metered, &mut upstream).await {
        Ok((to_upstream, to_client)) => {
            tracing::debug!(to_upstream, to_client, "egress connect tunnel closed");
            Ok(())
        }
        Err(err) => {
            tracing::debug!(error = %err, "egress connect tunnel closed");
            Ok(())
        }
    }
}

pub(super) async fn write_established<S>(io: &mut S) -> Result<(), PassthroughError>
where
    S: AsyncWrite + Unpin,
{
    let bytes = format!(
        "HTTP/1.1 200 Connection Established\r\nProxy-Agent: {}\r\n\r\n",
        super::PROXY_AGENT
    );
    io.write_all(bytes.as_bytes())
        .await
        .map_err(PassthroughError::Io)?;
    io.flush().await.map_err(PassthroughError::Io)?;
    Ok(())
}

/// Counts bytes that cross the guest socket and enforces the idle deadline.
///
/// Reads are guest → upstream. Writes are upstream → guest. Each direction is
/// counted once, as the bytes pass the guest socket.
struct Metered<'a, S> {
    inner: &'a mut S,
    gate: Arc<dyn EgressGate>,
    idle: Duration,
    sleep: Pin<Box<tokio::time::Sleep>>,
}

impl<'a, S> Metered<'a, S> {
    fn new(inner: &'a mut S, gate: Arc<dyn EgressGate>, idle: Duration) -> Self {
        Self {
            inner,
            gate,
            idle,
            sleep: Box::pin(tokio::time::sleep(idle)),
        }
    }

    fn touch(&mut self) {
        self.sleep
            .as_mut()
            .reset(tokio::time::Instant::now() + self.idle);
    }

    fn poll_idle(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.sleep.as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "tunnel idle timeout",
            ))),
            Poll::Pending => Poll::Pending,
        }
    }

    fn account(&self, n: usize) -> std::io::Result<()> {
        if n == 0 {
            return Ok(());
        }
        self.gate.record_bytes(n as u64).map_err(|err| {
            let msg = match err {
                GateError::QuotaExceeded => "quota exceeded",
                GateError::Denied(_) => "denied",
                GateError::Resolution(_) => "name resolution failed",
                GateError::Internal(_) => "internal error",
            };
            std::io::Error::new(std::io::ErrorKind::ConnectionAborted, msg)
        })
    }
}

impl<S> AsyncRead for Metered<'_, S>
where
    S: AsyncRead + Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut *this.inner).poll_read(cx, buf) {
            Poll::Ready(Ok(())) => {
                let n = buf.filled().len() - before;
                if n > 0 {
                    if let Err(err) = this.account(n) {
                        buf.set_filled(before);
                        return Poll::Ready(Err(err));
                    }
                    this.touch();
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => this.poll_idle(cx),
        }
    }
}

impl<S> AsyncWrite for Metered<'_, S>
where
    S: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        match Pin::new(&mut *this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(0)) => Poll::Ready(Ok(0)),
            Poll::Ready(Ok(n)) => {
                if let Err(err) = this.account(n) {
                    return Poll::Ready(Err(err));
                }
                this.touch();
                Poll::Ready(Ok(n))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => match this.poll_idle(cx) {
                Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
                Poll::Ready(Ok(())) => Poll::Ready(Ok(0)),
                Poll::Pending => Poll::Pending,
            },
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        match Pin::new(&mut *this.inner).poll_flush(cx) {
            Poll::Ready(result) => Poll::Ready(result),
            Poll::Pending => this.poll_idle(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        Pin::new(&mut *this.inner).poll_shutdown(cx)
    }
}

/// Dials one pinned address and ignores the request URI's host.
///
/// hyper-util's legacy client is generic over `tower_service::Service<Uri>`.
#[derive(Clone, Copy)]
struct PinnedConnector {
    addr: std::net::SocketAddr,
}

impl Service<hyper::Uri> for PinnedConnector {
    type Response = TokioIo<TcpStream>;
    type Error = std::io::Error;
    type Future = Pin<Box<dyn Future<Output = Result<TokioIo<TcpStream>, std::io::Error>> + Send>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _dst: hyper::Uri) -> Self::Future {
        let addr = self.addr;
        Box::pin(async move {
            let stream = TcpStream::connect(addr).await?;
            let _ = stream.set_nodelay(true);
            Ok(TokioIo::new(stream))
        })
    }
}

pub(super) async fn send_pinned(
    addr: std::net::SocketAddr,
    set_host: bool,
    request: hyper::Request<http_body_util::Full<bytes::Bytes>>,
) -> Result<hyper::Response<hyper::body::Incoming>, hyper_util::client::legacy::Error> {
    let client = hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new())
        .pool_max_idle_per_host(0)
        .pool_idle_timeout(None)
        .set_host(set_host)
        .build(PinnedConnector { addr });
    client.request(request).await
}
