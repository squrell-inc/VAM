use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

#[cfg(target_os = "linux")]
use tokio_vsock::{VsockAddr, VsockListener, VsockStream};

/// Unified stream type: real Linux KVM VSOCK, Unix sockets (the `Uds` transport), or the Windows TCP stub.
pub enum GenericStream {
    #[cfg(target_os = "linux")]
    Vsock(VsockStream),
    #[cfg(unix)]
    Unix(tokio::net::UnixStream),
    #[cfg(windows)]
    Tcp(tokio::net::TcpStream),
}

impl AsyncRead for GenericStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_read(cx, buf),
            #[cfg(windows)]
            Self::Tcp(s) => Pin::new(s).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for GenericStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        match self.get_mut() {
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_write(cx, buf),
            #[cfg(windows)]
            Self::Tcp(s) => Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_flush(cx),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_flush(cx),
            #[cfg(windows)]
            Self::Tcp(s) => Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        match self.get_mut() {
            #[cfg(target_os = "linux")]
            Self::Vsock(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(unix)]
            Self::Unix(s) => Pin::new(s).poll_shutdown(cx),
            #[cfg(windows)]
            Self::Tcp(s) => Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// How the guest reaches the host. The choice is explicit and never changes
/// by fallback: `Vsock` only ever uses AF_VSOCK, `Uds` only ever uses Unix
/// sockets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuestTransport {
    /// Real AF_VSOCK. The default, and the only transport of a rootfs guest.
    Vsock,
    /// Firecracker's hybrid vsock layout over Unix sockets, for a guest that
    /// runs as a plain host process (tests only; cargo feature `uds-transport`).
    /// Guest to host dials `<base>_<port>`. Host to guest connects to `<base>`
    /// and sends `CONNECT <port>\n`, answered with `OK <port>\n`.
    #[cfg(all(unix, feature = "uds-transport"))]
    Uds { base: std::path::PathBuf },
}

#[cfg(all(unix, feature = "uds-transport"))]
static TRANSPORT: std::sync::RwLock<GuestTransport> = std::sync::RwLock::new(GuestTransport::Vsock);

/// The transport used by [`GuestListener::bind`] and [`guest_dial`].
pub fn transport() -> GuestTransport {
    #[cfg(all(unix, feature = "uds-transport"))]
    {
        TRANSPORT
            .read()
            .map(|t| t.clone())
            .unwrap_or(GuestTransport::Vsock)
    }
    #[cfg(not(all(unix, feature = "uds-transport")))]
    {
        GuestTransport::Vsock
    }
}

/// Selects the process-wide transport. A process guest calls this once at
/// startup, before any bind or dial. Tests call it under their own lock.
#[cfg(all(unix, feature = "uds-transport"))]
pub fn set_transport(t: GuestTransport) {
    if let Ok(mut g) = TRANSPORT.write() {
        *g = t;
    }
}

/// Longest wait for the `CONNECT <port>` line of a `Uds` connection.
#[cfg(all(unix, feature = "uds-transport"))]
const UDS_PREAMBLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// Unified listener: real KVM VSOCK, the Unix-socket mux (feature
/// `uds-transport`), or a Windows TCP stub.
pub enum GuestListener {
    #[cfg(target_os = "linux")]
    Vsock(VsockListener),
    /// Emulates Firecracker's host-initiated vsock mux on `<base>`.
    #[cfg(all(unix, feature = "uds-transport"))]
    Uds {
        listener: tokio::net::UnixListener,
        path: std::path::PathBuf,
        port: u32,
    },
    #[cfg(windows)]
    Tcp {
        listener: tokio::net::TcpListener,
        port: u16,
    },
}

impl GuestListener {
    /// Binds on the process-wide transport ([`transport`]).
    pub async fn bind(port: u32) -> io::Result<Self> {
        Self::bind_with(&transport(), port).await
    }

    /// Binds on an explicit transport. There is no fallback to another one.
    pub async fn bind_with(transport: &GuestTransport, port: u32) -> io::Result<Self> {
        match transport {
            GuestTransport::Vsock => Self::bind_vsock(port).await,
            #[cfg(all(unix, feature = "uds-transport"))]
            GuestTransport::Uds { base } => Self::bind_uds(base, port),
        }
    }

    async fn bind_vsock(port: u32) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        {
            let addr = VsockAddr::new(tokio_vsock::VMADDR_CID_ANY, port);
            VsockListener::bind(addr).map(Self::Vsock).map_err(|e| {
                io::Error::new(e.kind(), format!("VSOCK bind on port {port} failed: {e}"))
            })
        }

        #[cfg(all(unix, not(target_os = "linux")))]
        {
            let _ = port;
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "VSOCK is not available on this platform",
            ))
        }

        #[cfg(windows)]
        {
            let bind_port = (10000 + (port % 50000)) as u16;
            let listener = tokio::net::TcpListener::bind(("127.0.0.1", bind_port)).await?;
            Ok(Self::Tcp {
                listener,
                port: bind_port,
            })
        }
    }

    #[cfg(all(unix, feature = "uds-transport"))]
    fn bind_uds(base: &std::path::Path, port: u32) -> io::Result<Self> {
        use std::os::unix::fs::FileTypeExt;
        if !base.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("UDS base {} must be an absolute path", base.display()),
            ));
        }
        // Remove a stale socket from an earlier run, and nothing else.
        match std::fs::symlink_metadata(base) {
            Ok(meta) if meta.file_type().is_socket() => {
                let _ = std::fs::remove_file(base);
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{} exists and is not a socket", base.display()),
                ));
            }
            Err(_) => {}
        }
        let listener = tokio::net::UnixListener::bind(base)?;
        Ok(Self::Uds {
            listener,
            path: base.to_path_buf(),
            port,
        })
    }

    pub async fn accept(&mut self) -> io::Result<(GenericStream, u32, u32)> {
        match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(listener) => {
                let (stream, addr) = listener.accept().await?;
                Ok((GenericStream::Vsock(stream), addr.cid(), addr.port()))
            }
            #[cfg(all(unix, feature = "uds-transport"))]
            Self::Uds { listener, port, .. } => loop {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let (mut stream, _) = listener.accept().await?;
                // Firecracker mux preamble: "CONNECT <port>\n" -> "OK <port>\n".
                let read_line = async {
                    let mut line = Vec::new();
                    let mut byte = [0u8; 1];
                    while line.len() < 64 {
                        stream.read_exact(&mut byte).await?;
                        if byte[0] == b'\n' {
                            return Ok::<_, io::Error>(Some(line));
                        }
                        line.push(byte[0]);
                    }
                    Ok(None)
                };
                let line = match tokio::time::timeout(UDS_PREAMBLE_TIMEOUT, read_line).await {
                    Ok(Ok(Some(line))) => String::from_utf8_lossy(&line).into_owned(),
                    _ => {
                        log::warn!("uds-mux: no valid CONNECT line, dropping connection");
                        continue;
                    }
                };
                if line.trim() != format!("CONNECT {}", port) {
                    log::warn!("uds-mux: rejected preamble {:?}", line);
                    continue;
                }
                if stream
                    .write_all(format!("OK {}\n", port).as_bytes())
                    .await
                    .is_err()
                {
                    continue;
                }
                return Ok((GenericStream::Unix(stream), 2, *port));
            },
            #[cfg(windows)]
            Self::Tcp { listener, .. } => {
                let (stream, _) = listener.accept().await?;
                Ok((GenericStream::Tcp(stream), 2, 52))
            }
        }
    }
}

impl Drop for GuestListener {
    fn drop(&mut self) {
        match self {
            #[cfg(target_os = "linux")]
            Self::Vsock(_) => {}
            #[cfg(all(unix, feature = "uds-transport"))]
            Self::Uds { path, .. } => {
                let _ = std::fs::remove_file(path);
            }
            #[cfg(windows)]
            Self::Tcp { .. } => {}
        }
    }
}

/// Dial out from the guest to the host on the process-wide transport.
pub async fn guest_dial(cid: u32, port: u32) -> io::Result<GenericStream> {
    guest_dial_with(&transport(), cid, port).await
}

/// Dial out on an explicit transport. There is no fallback to another one.
pub async fn guest_dial_with(
    transport: &GuestTransport,
    _cid: u32,
    port: u32,
) -> io::Result<GenericStream> {
    match transport {
        GuestTransport::Vsock => {
            #[cfg(target_os = "linux")]
            {
                let addr = VsockAddr::new(_cid, port);
                VsockStream::connect(addr)
                    .await
                    .map(GenericStream::Vsock)
                    .map_err(|e| {
                        io::Error::new(
                            e.kind(),
                            format!("VSOCK connect to CID {_cid} port {port} failed: {e}"),
                        )
                    })
            }
            #[cfg(all(unix, not(target_os = "linux")))]
            {
                let _ = port;
                Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "VSOCK is not available on this platform",
                ))
            }
            #[cfg(windows)]
            {
                let dial_port = (10000 + (port % 50000)) as u16;
                let stream = tokio::net::TcpStream::connect(("127.0.0.1", dial_port)).await?;
                Ok(GenericStream::Tcp(stream))
            }
        }
        #[cfg(all(unix, feature = "uds-transport"))]
        GuestTransport::Uds { base } => {
            let path = format!("{}_{}", base.display(), port);
            let stream = tokio::net::UnixStream::connect(&path).await?;
            Ok(GenericStream::Unix(stream))
        }
    }
}

/// Helper for the guest to run the handshake and RegisterAgent protocol.
pub async fn run_guest_registration<S>(
    mut stream: S,
    instance_id: &str,
    my_cid: u32,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::frame::{Frame, FrameType};
    use bytes::Bytes;
    use prost::Message;
    use vam_spec::v1::{
        Handshake, RegisterAgentRequest, RegisterAgentResponse, ResourceId, SignedEnvelope,
        Version, VsockEnvelope,
    };

    // 1. Read Handshake frame
    let frame_in = Frame::read_from(&mut stream)
        .await
        .map_err(|e| e.to_string())?;
    if frame_in.frame_type != FrameType::ControlGrpc {
        return Err("Expected ControlGrpc frame for handshake".to_string());
    }
    let env_in = VsockEnvelope::decode(&*frame_in.payload).map_err(|e| e.to_string())?;
    if env_in.method != "Handshake" {
        return Err(format!(
            "Expected Handshake method, got '{}'",
            env_in.method
        ));
    }
    let handshake = Handshake::decode(&*env_in.payload).map_err(|e| e.to_string())?;

    // 2. Prepare RegisterAgentRequest
    let request = RegisterAgentRequest {
        instance_id: Some(ResourceId {
            urn: instance_id.to_string(),
        }),
        runtime_version: Some(Version {
            major: 0,
            minor: 1,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        }),
        guest_cid: my_cid.to_string(),
        host_challenge: handshake.host_challenge.clone(),
        attestation: Some(SignedEnvelope {
            domain: "vam.v1.guest-registration".to_string(),
            envelope_version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            claims: handshake.host_challenge.clone(), // dummy claims
            signature: None,                          // No signature for dev
        }),
    };

    let mut req_payload = Vec::new();
    request
        .encode(&mut req_payload)
        .map_err(|e| e.to_string())?;

    let env_out = VsockEnvelope {
        method: "RegisterAgent".to_string(),
        correlation_id: 1,
        payload: req_payload,
        error: None,
        trace_context: None,
    };

    let mut env_payload = Vec::new();
    env_out
        .encode(&mut env_payload)
        .map_err(|e| e.to_string())?;

    let frame_out = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: 1,
        payload: Bytes::from(env_payload),
    };
    frame_out
        .write_to(&mut stream)
        .await
        .map_err(|e| e.to_string())?;

    // 3. Read RegisterAgentResponse
    let frame_resp = Frame::read_from(&mut stream)
        .await
        .map_err(|e| e.to_string())?;
    if frame_resp.frame_type != FrameType::ControlGrpc {
        return Err("Expected ControlGrpc frame for response".to_string());
    }
    let env_resp = VsockEnvelope::decode(&*frame_resp.payload).map_err(|e| e.to_string())?;
    if env_resp.method != "RegisterAgentResponse" {
        return Err(format!(
            "Expected RegisterAgentResponse, got '{}'",
            env_resp.method
        ));
    }
    let response = RegisterAgentResponse::decode(&*env_resp.payload).map_err(|e| e.to_string())?;

    if response.accepted {
        Ok(())
    } else {
        Err(response
            .error
            .map(|e| e.message)
            .unwrap_or_else(|| "Registration rejected".to_string()))
    }
}

/// Accept loop helper on the guest side (listening to host-initiated RPCs).
pub async fn handle_guest_connection<S>(mut stream: S, _instance_id: &str) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use crate::frame::{Frame, FrameType};
    use bytes::Bytes;
    use prost::Message;
    use prost_types::Timestamp;
    use std::time::{SystemTime, UNIX_EPOCH};
    use vam_spec::v1::{ErrorStatus, PingRequest, PingResponse, VsockEnvelope};

    loop {
        let frame = match Frame::read_from(&mut stream).await {
            Ok(f) => f,
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Connection closed gracefully
                break;
            }
            Err(e) => return Err(e.to_string()),
        };

        if frame.frame_type != FrameType::ControlGrpc {
            return Err(format!("Unexpected frame type: {:?}", frame.frame_type));
        }

        let env = VsockEnvelope::decode(&*frame.payload).map_err(|e| e.to_string())?;
        match env.method.as_str() {
            "Ping" => {
                let _req = PingRequest::decode(&*env.payload).map_err(|e| e.to_string())?;

                let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
                let response = PingResponse {
                    received_at: Some(Timestamp {
                        seconds: now.as_secs() as i64,
                        nanos: now.subsec_nanos() as i32,
                    }),
                };

                let mut resp_payload = Vec::new();
                response
                    .encode(&mut resp_payload)
                    .map_err(|e| e.to_string())?;

                let env_out = VsockEnvelope {
                    method: "PingResponse".to_string(),
                    correlation_id: env.correlation_id,
                    payload: resp_payload,
                    error: None,
                    trace_context: None,
                };

                let mut env_payload = Vec::new();
                env_out
                    .encode(&mut env_payload)
                    .map_err(|e| e.to_string())?;

                let frame_out = Frame {
                    frame_type: FrameType::ControlGrpc,
                    correlation_id: env.correlation_id,
                    payload: Bytes::from(env_payload),
                };
                frame_out
                    .write_to(&mut stream)
                    .await
                    .map_err(|e| e.to_string())?;
            }
            other => {
                // Send error status
                let err_status = ErrorStatus {
                    code: 12, // UNIMPLEMENTED
                    message: format!("Method '{}' is unimplemented on the guest", other),
                    details: Vec::new(),
                    retryable: false,
                };

                let env_out = VsockEnvelope {
                    method: format!("{}Response", other),
                    correlation_id: env.correlation_id,
                    payload: Vec::new(),
                    error: Some(err_status),
                    trace_context: None,
                };

                let mut env_payload = Vec::new();
                env_out
                    .encode(&mut env_payload)
                    .map_err(|e| e.to_string())?;

                let frame_out = Frame {
                    frame_type: FrameType::ControlGrpc,
                    correlation_id: env.correlation_id,
                    payload: Bytes::from(env_payload),
                };
                frame_out
                    .write_to(&mut stream)
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }
    Ok(())
}
