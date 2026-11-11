use bytes::Bytes;
use prost::Message;
use std::io;
use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(windows)]
use tokio::net::{TcpListener, TcpStream};
#[cfg(unix)]
use tokio::net::{UnixListener, UnixStream};

use crate::frame::{Frame, FrameType};
use crate::security::{AttestationVerifier, ChallengeRegistry};
use vam_spec::v1::{
    ErrorStatus, Handshake, PingRequest, PingResponse, RegisterAgentRequest, RegisterAgentResponse,
    Version, VsockEnvelope,
};

pub struct HostListener {
    #[cfg(unix)]
    listener: UnixListener,
    #[cfg(windows)]
    listener: TcpListener,
    path: PathBuf,
}

impl HostListener {
    pub async fn bind<P: AsRef<Path>>(uds_path: P, port: u32) -> io::Result<Self> {
        let path = PathBuf::from(format!("{}_{}", uds_path.as_ref().display(), port));
        #[cfg(unix)]
        {
            // Remove old socket if exists
            let _ = std::fs::remove_file(&path);
            let listener = UnixListener::bind(&path)?;
            Ok(Self { listener, path })
        }
        #[cfg(windows)]
        {
            let bind_port = (10000 + (port % 50000)) as u16;
            let listener = TcpListener::bind(("127.0.0.1", bind_port)).await?;
            Ok(Self { listener, path })
        }
    }

    #[cfg(unix)]
    pub async fn accept(&self) -> io::Result<UnixStream> {
        let (stream, _) = self.listener.accept().await?;
        Ok(stream)
    }

    #[cfg(windows)]
    pub async fn accept(&self) -> io::Result<TcpStream> {
        let (stream, _) = self.listener.accept().await?;
        Ok(stream)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for HostListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(unix)]
pub async fn host_dial<P: AsRef<Path>>(uds_path: P, port: u32) -> io::Result<UnixStream> {
    let mut stream = UnixStream::connect(uds_path.as_ref()).await?;

    // Write connect request
    let req = format!("CONNECT {}\n", port);
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // Read response up to '\n'. It should be "OK <port>\n"
    let mut resp = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        resp.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }

    let resp_str = String::from_utf8_lossy(&resp);
    if resp_str.starts_with("OK ") {
        Ok(stream)
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "Failed to connect to guest port {}: {}",
                port,
                resp_str.trim()
            ),
        ))
    }
}

#[cfg(windows)]
pub async fn host_dial<P: AsRef<Path>>(_uds_path: P, port: u32) -> io::Result<TcpStream> {
    let dial_port = (10000 + (port % 50000)) as u16;
    let mut stream = TcpStream::connect(("127.0.0.1", dial_port)).await?;

    // Write connect request
    let req = format!("CONNECT {}\n", port);
    stream.write_all(req.as_bytes()).await?;
    stream.flush().await?;

    // Read response up to '\n'. It should be "OK <port>\n"
    let mut resp = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        stream.read_exact(&mut byte).await?;
        resp.push(byte[0]);
        if byte[0] == b'\n' {
            break;
        }
    }

    let resp_str = String::from_utf8_lossy(&resp);
    if resp_str.starts_with("OK ") {
        Ok(stream)
    } else {
        Err(io::Error::new(
            io::ErrorKind::ConnectionRefused,
            format!(
                "Failed to connect to guest port {}: {}",
                port,
                resp_str.trim()
            ),
        ))
    }
}

/// Helper to handle the low-level handshake and RegisterAgent protocol on the host.
pub async fn handle_registration_handshake<S>(
    mut stream: S,
    instance_id: &str,
    expected_cid: u32,
    challenge_registry: &ChallengeRegistry,
    verifier: &dyn AttestationVerifier,
) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    // 1. Generate challenge
    let challenge = challenge_registry.generate_challenge(instance_id);

    // 2. Send Handshake
    let handshake = Handshake {
        protocol_version: Some(Version {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
            build: String::new(),
        }),
        supported_features: vec!["hitl".to_string()],
        metadata: std::collections::HashMap::new(),
        host_challenge: challenge.to_vec(),
    };
    let mut payload = Vec::new();
    handshake.encode(&mut payload).map_err(|e| e.to_string())?;

    let envelope = VsockEnvelope {
        method: "Handshake".to_string(),
        correlation_id: 0,
        payload,
        error: None,
        trace_context: None,
    };
    let mut env_payload = Vec::new();
    envelope
        .encode(&mut env_payload)
        .map_err(|e| e.to_string())?;

    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: 0,
        payload: Bytes::from(env_payload),
    };
    frame
        .write_to(&mut stream)
        .await
        .map_err(|e| e.to_string())?;

    // 3. Read RegisterAgentRequest
    let frame_in = Frame::read_from(&mut stream)
        .await
        .map_err(|e| e.to_string())?;
    if frame_in.frame_type != FrameType::ControlGrpc {
        return Err("Expected ControlGrpc frame for registration".to_string());
    }
    let env_in = VsockEnvelope::decode(&*frame_in.payload).map_err(|e| e.to_string())?;
    if env_in.method != "RegisterAgent" {
        return Err(format!(
            "Expected RegisterAgent method, got '{}'",
            env_in.method
        ));
    }
    let request = RegisterAgentRequest::decode(&*env_in.payload).map_err(|e| e.to_string())?;

    // 4. Validate request
    let mut validation_err = None;

    // Check challenge
    let stored_challenge = challenge_registry.consume_challenge(instance_id);
    match stored_challenge {
        None => {
            validation_err = Some("Challenge not found or already consumed".to_string());
        }
        Some(stored) => {
            if request.host_challenge != stored {
                validation_err = Some("Challenge mismatch".to_string());
            }
        }
    }

    // Check instance ID
    if validation_err.is_none() {
        let req_instance_id = request
            .instance_id
            .as_ref()
            .map(|id| id.urn.as_str())
            .unwrap_or("");
        if req_instance_id != instance_id {
            validation_err = Some(format!(
                "Instance ID mismatch: got '{}', expected '{}'",
                req_instance_id, instance_id
            ));
        }
    }

    // Check CID. Some Firecracker guests always report vsock CID 3 (the
    // historic guest CID) even when the host allocated a different value.
    // Instance ID is the binding identity; accept that fallback.
    if validation_err.is_none() && request.guest_cid != expected_cid.to_string() {
        let reported_historic_guest_cid = request.guest_cid.parse::<u32>().ok() == Some(3);
        if reported_historic_guest_cid && expected_cid >= 3 {
            eprintln!(
                "[vsock] guest reported CID 3 (vsock default); host allocated {}. Accepting (instance_id matched).",
                expected_cid
            );
        } else {
            validation_err = Some(format!(
                "CID mismatch: got '{}', expected '{}'",
                request.guest_cid, expected_cid
            ));
        }
    }

    // Check Attestation
    if validation_err.is_none() {
        if let Some(attestation) = &request.attestation {
            if let Err(e) = verifier.verify(instance_id, attestation) {
                validation_err = Some(format!("Attestation verifier failed: {}", e));
            }
        } else {
            validation_err = Some("Missing attestation envelope".to_string());
        }
    }

    // 5. Send Response
    let (accepted, error_status) = match validation_err {
        None => (true, None),
        Some(msg) => (
            false,
            Some(ErrorStatus {
                code: 3, // INVALID_ARGUMENT
                message: msg.clone(),
                details: Vec::new(),
                retryable: false,
            }),
        ),
    };

    let response = RegisterAgentResponse {
        accepted,
        error: error_status.clone(),
    };
    let mut resp_payload = Vec::new();
    response
        .encode(&mut resp_payload)
        .map_err(|e| e.to_string())?;

    let env_out = VsockEnvelope {
        method: "RegisterAgentResponse".to_string(),
        correlation_id: env_in.correlation_id,
        payload: resp_payload,
        error: error_status,
        trace_context: None,
    };
    let mut env_payload = Vec::new();
    env_out
        .encode(&mut env_payload)
        .map_err(|e| e.to_string())?;

    let frame_out = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id: env_in.correlation_id,
        payload: Bytes::from(env_payload),
    };
    frame_out
        .write_to(&mut stream)
        .await
        .map_err(|e| e.to_string())?;

    if accepted {
        Ok(())
    } else {
        Err(response.error.unwrap().message)
    }
}

/// Host-side Ping implementation to dial the guest and measure liveness RTT.
pub async fn host_ping<S>(
    mut stream: S,
    correlation_id: u32,
    instance_id: &str,
    timeout: std::time::Duration,
) -> Result<std::time::Duration, String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use prost_types::Timestamp;
    use std::time::{Instant, SystemTime, UNIX_EPOCH};

    let now_system = SystemTime::now().duration_since(UNIX_EPOCH).unwrap();
    let request = PingRequest {
        instance_id: Some(vam_spec::v1::ResourceId {
            urn: instance_id.to_string(),
        }),
        sent_at: Some(Timestamp {
            seconds: now_system.as_secs() as i64,
            nanos: now_system.subsec_nanos() as i32,
        }),
    };

    let mut payload = Vec::new();
    request.encode(&mut payload).map_err(|e| e.to_string())?;

    let env = VsockEnvelope {
        method: "Ping".to_string(),
        correlation_id,
        payload,
        error: None,
        trace_context: None,
    };

    let mut env_payload = Vec::new();
    env.encode(&mut env_payload).map_err(|e| e.to_string())?;

    let frame = Frame {
        frame_type: FrameType::ControlGrpc,
        correlation_id,
        payload: Bytes::from(env_payload),
    };

    let start_inst = Instant::now();

    // Write Ping to guest
    tokio::time::timeout(timeout, frame.write_to(&mut stream))
        .await
        .map_err(|_| "Ping write timed out".to_string())?
        .map_err(|e| e.to_string())?;

    // Read response from guest
    let frame_in = tokio::time::timeout(timeout, Frame::read_from(&mut stream))
        .await
        .map_err(|_| "Ping read timed out".to_string())?
        .map_err(|e| e.to_string())?;

    let elapsed = start_inst.elapsed();

    if frame_in.frame_type != FrameType::ControlGrpc {
        return Err("Expected ControlGrpc frame type for Ping response".to_string());
    }

    let env_in = VsockEnvelope::decode(&*frame_in.payload).map_err(|e| e.to_string())?;
    if env_in.method != "PingResponse" {
        return Err(format!(
            "Expected PingResponse method, got '{}'",
            env_in.method
        ));
    }

    let _response = PingResponse::decode(&*env_in.payload).map_err(|e| e.to_string())?;

    Ok(elapsed)
}
