use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use prost::bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use prost::Message;

use vam_spec::v1::{
    ArtifactRef, CheckpointRequest, CheckpointResponse, CloseRequest, CloseResponse, DirEntry,
    FileStat, FlushRequest, FlushResponse, FsyncRequest, FsyncResponse, MkdirRequest,
    MkdirResponse, OpenRequest, OpenResponse, ReadRequest, ReadResponse, ReaddirRequest,
    ReaddirResponse, RenameRequest, RenameResponse, RmdirRequest, RmdirResponse, StatRequest,
    StatResponse, SymlinkRequest, SymlinkResponse, UnlinkRequest, UnlinkResponse, VamErrorCode,
    VsockEnvelope, WriteRequest, WriteResponse,
};
use vam_vsock::frame::{Frame, FrameType};

#[derive(Debug, thiserror::Error)]
pub enum Drive9GuestError {
    #[error("serialization/deserialization failed: {0}")]
    Serialization(String),

    #[error("connection closed")]
    ConnectionClosed,

    #[error("remote error: code {code}, message: {message}")]
    RemoteError { code: i32, message: String },
}

pub struct Drive9Client {
    request_tx: mpsc::Sender<Frame>,
    pending_requests: Arc<Mutex<HashMap<u32, oneshot::Sender<VsockEnvelope>>>>,
    next_correlation_id: AtomicU64,
}

impl Drive9Client {
    pub fn new<S>(stream: S) -> Self
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (request_tx, mut request_rx) = mpsc::channel::<Frame>(128);
        let pending_requests = Arc::new(Mutex::new(
            HashMap::<u32, oneshot::Sender<VsockEnvelope>>::new(),
        ));
        let pending_requests_clone = pending_requests.clone();

        // Spawn writer task
        tokio::spawn(async move {
            while let Some(frame) = request_rx.recv().await {
                if let Err(e) = frame.write_to(&mut writer).await {
                    eprintln!("Drive9GuestClient writer error: {}", e);
                    break;
                }
            }
        });

        // Spawn reader task
        tokio::spawn(async move {
            loop {
                match Frame::read_from(&mut reader).await {
                    Ok(frame) => {
                        if frame.frame_type != FrameType::StreamStorage {
                            continue;
                        }
                        if let Ok(envelope) = VsockEnvelope::decode(frame.payload.as_ref()) {
                            let mut map = pending_requests_clone.lock().unwrap();
                            if let Some(tx) = map.remove(&frame.correlation_id) {
                                let _ = tx.send(envelope);
                            }
                        }
                    }
                    Err(e) => {
                        if e.kind() != io::ErrorKind::UnexpectedEof {
                            eprintln!("Drive9GuestClient reader error: {}", e);
                        }
                        break;
                    }
                }
            }
        });

        Self {
            request_tx,
            pending_requests,
            next_correlation_id: AtomicU64::new(1),
        }
    }

    async fn call<Req, Resp>(&self, method: &str, req: &Req) -> Result<Resp, Drive9GuestError>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let correlation_id = self.next_correlation_id.fetch_add(1, Ordering::SeqCst) as u32;
        let mut req_bytes = Vec::new();
        req.encode(&mut req_bytes)
            .map_err(|e| Drive9GuestError::Serialization(e.to_string()))?;

        let envelope = VsockEnvelope {
            method: method.to_string(),
            correlation_id,
            payload: req_bytes,
            error: None,
            trace_context: None,
        };

        let mut env_bytes = Vec::new();
        envelope
            .encode(&mut env_bytes)
            .map_err(|e| Drive9GuestError::Serialization(e.to_string()))?;

        let frame = Frame {
            frame_type: FrameType::StreamStorage,
            correlation_id,
            payload: Bytes::from(env_bytes),
        };

        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending_requests.lock().unwrap();
            map.insert(correlation_id, tx);
        }

        self.request_tx
            .send(frame)
            .await
            .map_err(|_| Drive9GuestError::ConnectionClosed)?;

        let response_env = rx.await.map_err(|_| Drive9GuestError::ConnectionClosed)?;

        if let Some(err) = response_env.error {
            return Err(Drive9GuestError::RemoteError {
                code: err.code,
                message: err.message,
            });
        }

        let resp = Resp::decode(response_env.payload.as_slice())
            .map_err(|e| Drive9GuestError::Serialization(e.to_string()))?;

        Ok(resp)
    }

    pub async fn open(&self, path: &str, flags: u32, mode: u32) -> Result<u64, Drive9GuestError> {
        let resp: OpenResponse = self
            .call(
                "Open",
                &OpenRequest {
                    path: path.to_string(),
                    flags,
                    mode,
                },
            )
            .await?;
        Ok(resp.fd)
    }

    pub async fn read(&self, fd: u64, offset: u64, size: u32) -> Result<Vec<u8>, Drive9GuestError> {
        let resp: ReadResponse = self.call("Read", &ReadRequest { fd, offset, size }).await?;
        Ok(resp.data)
    }

    pub async fn write(
        &self,
        fd: u64,
        offset: u64,
        data: Vec<u8>,
    ) -> Result<u32, Drive9GuestError> {
        let resp: WriteResponse = self
            .call("Write", &WriteRequest { fd, offset, data })
            .await?;
        Ok(resp.bytes_written)
    }

    pub async fn close(&self, fd: u64) -> Result<(), Drive9GuestError> {
        let _resp: CloseResponse = self.call("Close", &CloseRequest { fd }).await?;
        Ok(())
    }

    pub async fn fsync(&self, fd: u64, datasync: bool) -> Result<(), Drive9GuestError> {
        let _resp: FsyncResponse = self.call("Fsync", &FsyncRequest { fd, datasync }).await?;
        Ok(())
    }

    pub async fn stat(&self, path: &str) -> Result<FileStat, Drive9GuestError> {
        let resp: StatResponse = self
            .call(
                "Stat",
                &StatRequest {
                    path: path.to_string(),
                },
            )
            .await?;
        resp.stat.ok_or_else(|| Drive9GuestError::RemoteError {
            code: VamErrorCode::NotFound as i32,
            message: "Stat response has no stat data".to_string(),
        })
    }

    pub async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, Drive9GuestError> {
        let resp: ReaddirResponse = self
            .call(
                "Readdir",
                &ReaddirRequest {
                    path: path.to_string(),
                },
            )
            .await?;
        Ok(resp.entries)
    }

    pub async fn mkdir(&self, path: &str, mode: u32) -> Result<(), Drive9GuestError> {
        let _resp: MkdirResponse = self
            .call(
                "Mkdir",
                &MkdirRequest {
                    path: path.to_string(),
                    mode,
                },
            )
            .await?;
        Ok(())
    }

    pub async fn rmdir(&self, path: &str) -> Result<(), Drive9GuestError> {
        let _resp: RmdirResponse = self
            .call(
                "Rmdir",
                &RmdirRequest {
                    path: path.to_string(),
                },
            )
            .await?;
        Ok(())
    }

    pub async fn unlink(&self, path: &str) -> Result<(), Drive9GuestError> {
        let _resp: UnlinkResponse = self
            .call(
                "Unlink",
                &UnlinkRequest {
                    path: path.to_string(),
                },
            )
            .await?;
        Ok(())
    }

    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<(), Drive9GuestError> {
        let _resp: RenameResponse = self
            .call(
                "Rename",
                &RenameRequest {
                    old_path: old_path.to_string(),
                    new_path: new_path.to_string(),
                },
            )
            .await?;
        Ok(())
    }

    pub async fn symlink(&self, target: &str, link_path: &str) -> Result<(), Drive9GuestError> {
        let _resp: SymlinkResponse = self
            .call(
                "Symlink",
                &SymlinkRequest {
                    target: target.to_string(),
                    link_path: link_path.to_string(),
                },
            )
            .await?;
        Ok(())
    }

    pub async fn flush(&self, session_id: &str) -> Result<(), Drive9GuestError> {
        let start = std::time::Instant::now();
        let _resp: FlushResponse = self
            .call(
                "Flush",
                &FlushRequest {
                    session_id: session_id.to_string(),
                },
            )
            .await?;
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("drive9.session_id".to_string(), session_id.to_string());
        attrs.insert("drive9.op".to_string(), "flush".to_string());
        crate::telemetry::global_telemetry().record_span(
            "drive9.sync",
            start.elapsed(),
            None,
            attrs,
        );
        Ok(())
    }

    pub async fn checkpoint(
        &self,
        session_id: &str,
        description: &str,
    ) -> Result<ArtifactRef, Drive9GuestError> {
        let start = std::time::Instant::now();
        let resp: CheckpointResponse = self
            .call(
                "Checkpoint",
                &CheckpointRequest {
                    session_id: session_id.to_string(),
                    description: description.to_string(),
                },
            )
            .await?;
        let mut attrs = std::collections::HashMap::new();
        attrs.insert("drive9.session_id".to_string(), session_id.to_string());
        attrs.insert("drive9.op".to_string(), "checkpoint".to_string());
        crate::telemetry::global_telemetry().record_span(
            "drive9.sync",
            start.elapsed(),
            None,
            attrs,
        );
        resp.checkpoint_ref
            .ok_or_else(|| Drive9GuestError::RemoteError {
                code: VamErrorCode::Internal as i32,
                message: "Checkpoint response has no checkpoint_ref".to_string(),
            })
    }
}
