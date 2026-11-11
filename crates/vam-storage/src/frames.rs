use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::symlink;
#[cfg(windows)]
use std::os::windows::fs::symlink_file as symlink;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, oneshot};

use prost::Message;
use prost_types::Timestamp;

use vam_spec::v1::{
    ArtifactRef, CheckpointRequest, CheckpointResponse, CloseRequest, CloseResponse, DirEntry,
    ErrorStatus, FileStat, FlushRequest, FlushResponse, FsyncRequest, FsyncResponse, MkdirRequest,
    MkdirResponse, OpenRequest, OpenResponse, ReadRequest, ReadResponse, ReaddirRequest,
    ReaddirResponse, RenameRequest, RenameResponse, ResourceId, RmdirRequest, RmdirResponse,
    StatRequest, StatResponse, SymlinkRequest, SymlinkResponse, UnlinkRequest, UnlinkResponse,
    VamErrorCode, VsockEnvelope, WriteRequest, WriteResponse,
};
use vam_vsock::frame::{Frame, FrameType};

use crate::error::Drive9Error;
use crate::path::sanitize_path;
use crate::FilesystemCheckpointCoordinator;

pub fn make_envelope<T: Message>(method: &str, correlation_id: u32, msg: &T) -> VsockEnvelope {
    let mut payload = Vec::new();
    msg.encode(&mut payload).unwrap();
    VsockEnvelope {
        method: method.to_string(),
        correlation_id,
        payload,
        error: None,
        trace_context: None,
    }
}

pub fn make_error_envelope(
    method: &str,
    correlation_id: u32,
    code: VamErrorCode,
    message: &str,
) -> VsockEnvelope {
    VsockEnvelope {
        method: method.to_string(),
        correlation_id,
        payload: Vec::new(),
        error: Some(ErrorStatus {
            code: code as i32,
            message: message.to_string(),
            retryable: false,
            details: Vec::new(),
        }),
        trace_context: None,
    }
}

pub fn make_error_status_envelope(
    method: &str,
    correlation_id: u32,
    err: ErrorStatus,
) -> VsockEnvelope {
    VsockEnvelope {
        method: method.to_string(),
        correlation_id,
        payload: Vec::new(),
        error: Some(err),
        trace_context: None,
    }
}

fn to_proto_timestamp(st: SystemTime) -> Option<Timestamp> {
    st.duration_since(UNIX_EPOCH).ok().map(|d| Timestamp {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    })
}

fn get_dir_size(path: &Path) -> u64 {
    let mut total = 0;
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    total += metadata.len();
                } else if metadata.is_dir() {
                    total += get_dir_size(&entry.path());
                }
            }
        }
    }
    total
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
                    eprintln!("Drive9Client writer error: {}", e);
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
                            eprintln!("Drive9Client reader error: {}", e);
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

    async fn call<Req, Resp>(&self, method: &str, req: &Req) -> Result<Resp, Drive9Error>
    where
        Req: Message,
        Resp: Message + Default,
    {
        let correlation_id = self.next_correlation_id.fetch_add(1, Ordering::SeqCst) as u32;
        let mut req_bytes = Vec::new();
        req.encode(&mut req_bytes)
            .map_err(|e| Drive9Error::Serialization(e.to_string()))?;

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
            .map_err(|e| Drive9Error::Serialization(e.to_string()))?;

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
            .map_err(|_| Drive9Error::ConnectionClosed)?;

        let response_env = rx.await.map_err(|_| Drive9Error::ConnectionClosed)?;

        if let Some(err) = response_env.error {
            return Err(Drive9Error::RemoteError {
                code: err.code,
                message: err.message,
            });
        }

        let resp = Resp::decode(response_env.payload.as_slice())
            .map_err(|e| Drive9Error::Serialization(e.to_string()))?;

        Ok(resp)
    }

    pub async fn open(&self, path: &str, flags: u32, mode: u32) -> Result<u64, Drive9Error> {
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

    pub async fn read(&self, fd: u64, offset: u64, size: u32) -> Result<Vec<u8>, Drive9Error> {
        let resp: ReadResponse = self.call("Read", &ReadRequest { fd, offset, size }).await?;
        Ok(resp.data)
    }

    pub async fn write(&self, fd: u64, offset: u64, data: Vec<u8>) -> Result<u32, Drive9Error> {
        let resp: WriteResponse = self
            .call("Write", &WriteRequest { fd, offset, data })
            .await?;
        Ok(resp.bytes_written)
    }

    pub async fn close(&self, fd: u64) -> Result<(), Drive9Error> {
        let _resp: CloseResponse = self.call("Close", &CloseRequest { fd }).await?;
        Ok(())
    }

    pub async fn fsync(&self, fd: u64, datasync: bool) -> Result<(), Drive9Error> {
        let _resp: FsyncResponse = self.call("Fsync", &FsyncRequest { fd, datasync }).await?;
        Ok(())
    }

    pub async fn stat(&self, path: &str) -> Result<FileStat, Drive9Error> {
        let resp: StatResponse = self
            .call(
                "Stat",
                &StatRequest {
                    path: path.to_string(),
                },
            )
            .await?;
        resp.stat.ok_or_else(|| Drive9Error::RemoteError {
            code: VamErrorCode::NotFound as i32,
            message: "Stat response has no stat data".to_string(),
        })
    }

    pub async fn readdir(&self, path: &str) -> Result<Vec<DirEntry>, Drive9Error> {
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

    pub async fn mkdir(&self, path: &str, mode: u32) -> Result<(), Drive9Error> {
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

    pub async fn rmdir(&self, path: &str) -> Result<(), Drive9Error> {
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

    pub async fn unlink(&self, path: &str) -> Result<(), Drive9Error> {
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

    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<(), Drive9Error> {
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

    pub async fn symlink(&self, target: &str, link_path: &str) -> Result<(), Drive9Error> {
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

    pub async fn flush(&self, session_id: &str) -> Result<(), Drive9Error> {
        let _resp: FlushResponse = self
            .call(
                "Flush",
                &FlushRequest {
                    session_id: session_id.to_string(),
                },
            )
            .await?;
        Ok(())
    }

    pub async fn checkpoint(
        &self,
        session_id: &str,
        description: &str,
    ) -> Result<ArtifactRef, Drive9Error> {
        let resp: CheckpointResponse = self
            .call(
                "Checkpoint",
                &CheckpointRequest {
                    session_id: session_id.to_string(),
                    description: description.to_string(),
                },
            )
            .await?;
        resp.checkpoint_ref.ok_or_else(|| Drive9Error::RemoteError {
            code: VamErrorCode::Internal as i32,
            message: "Checkpoint response has no checkpoint_ref".to_string(),
        })
    }
}

pub struct Drive9Server {
    sandbox_root: PathBuf,
    quota_bytes: u64,
    checkpoint_coordinator: Arc<FilesystemCheckpointCoordinator>,
    fds: Arc<Mutex<HashMap<u64, File>>>,
    next_fd: AtomicU64,
}

impl Drive9Server {
    pub fn new(
        sandbox_root: PathBuf,
        quota_bytes: u64,
        checkpoint_coordinator: Arc<FilesystemCheckpointCoordinator>,
    ) -> Self {
        Self {
            sandbox_root,
            quota_bytes,
            checkpoint_coordinator,
            fds: Arc::new(Mutex::new(HashMap::new())),
            next_fd: AtomicU64::new(1),
        }
    }

    pub async fn handle_connection<S>(&self, stream: S)
    where
        S: AsyncRead + AsyncWrite + Send + Unpin + 'static,
    {
        let (mut reader, mut writer) = tokio::io::split(stream);
        let (tx, mut rx) = mpsc::channel::<Frame>(128);

        // Spawn writer loop
        tokio::spawn(async move {
            while let Some(frame) = rx.recv().await {
                if let Err(e) = frame.write_to(&mut writer).await {
                    eprintln!("Drive9Server writer error: {}", e);
                    break;
                }
            }
        });

        loop {
            match Frame::read_from(&mut reader).await {
                Ok(frame) => {
                    if frame.frame_type != FrameType::StreamStorage {
                        continue;
                    }
                    let envelope = match VsockEnvelope::decode(frame.payload.as_ref()) {
                        Ok(env) => env,
                        Err(e) => {
                            eprintln!("Failed to decode VsockEnvelope: {}", e);
                            continue;
                        }
                    };

                    let response_envelope = self.process_request(envelope).await;
                    let mut env_bytes = Vec::new();
                    if let Err(e) = response_envelope.encode(&mut env_bytes) {
                        eprintln!("Failed to encode response envelope: {}", e);
                        continue;
                    }

                    let response_frame = Frame {
                        frame_type: FrameType::StreamStorage,
                        correlation_id: frame.correlation_id,
                        payload: Bytes::from(env_bytes),
                    };

                    if tx.send(response_frame).await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    if e.kind() != io::ErrorKind::UnexpectedEof {
                        eprintln!("Drive9Server reader error: {}", e);
                    }
                    break;
                }
            }
        }
    }

    async fn process_request(&self, envelope: VsockEnvelope) -> VsockEnvelope {
        let method = envelope.method.as_str();
        let correlation_id = envelope.correlation_id;

        match method {
            "Open" => {
                let req = match OpenRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_open(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Read" => {
                let req = match ReadRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_read(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Write" => {
                let req = match WriteRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_write(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Close" => {
                let req = match CloseRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_close(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Fsync" => {
                let req = match FsyncRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_fsync(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Stat" => {
                let req = match StatRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_stat(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Readdir" => {
                let req = match ReaddirRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_readdir(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Mkdir" => {
                let req = match MkdirRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_mkdir(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Rmdir" => {
                let req = match RmdirRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_rmdir(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Unlink" => {
                let req = match UnlinkRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_unlink(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Rename" => {
                let req = match RenameRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_rename(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Symlink" => {
                let req = match SymlinkRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_symlink(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Flush" => {
                let req = match FlushRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_flush(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            "Checkpoint" => {
                let req = match CheckpointRequest::decode(envelope.payload.as_slice()) {
                    Ok(r) => r,
                    Err(_) => {
                        return make_error_envelope(
                            method,
                            correlation_id,
                            VamErrorCode::InvalidArgument,
                            "Invalid request payload",
                        )
                    }
                };
                match self.handle_checkpoint(req).await {
                    Ok(resp) => make_envelope(method, correlation_id, &resp),
                    Err(err) => make_error_status_envelope(method, correlation_id, err),
                }
            }
            _ => make_error_envelope(
                method,
                correlation_id,
                VamErrorCode::Unimplemented,
                "Unimplemented method",
            ),
        }
    }

    fn resolve_path(&self, rel_path_str: &str) -> Result<PathBuf, ErrorStatus> {
        let rel_path = Path::new(rel_path_str);
        sanitize_path(&self.sandbox_root, rel_path).map_err(|e| ErrorStatus {
            code: VamErrorCode::PermissionDenied as i32,
            message: format!("Sandbox violation: {}", e),
            retryable: false,
            details: Vec::new(),
        })
    }

    async fn handle_open(&self, req: OpenRequest) -> Result<OpenResponse, ErrorStatus> {
        let abs_path = self.resolve_path(&req.path)?;

        let mut options = OpenOptions::new();
        let read = (req.flags & 3) == 0 || (req.flags & 3) == 2;
        let write = (req.flags & 3) == 1 || (req.flags & 3) == 2;
        options.read(read);
        options.write(write);
        if (req.flags & 64) != 0 {
            options.create(true);
        }
        if (req.flags & 512) != 0 {
            options.truncate(true);
        }
        if (req.flags & 1024) != 0 {
            options.append(true);
        }

        let file = options.open(&abs_path).map_err(|e| ErrorStatus {
            code: VamErrorCode::NotFound as i32,
            message: format!("Failed to open file: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;

        let fd = self.next_fd.fetch_add(1, Ordering::SeqCst);
        self.fds.lock().unwrap().insert(fd, file);

        Ok(OpenResponse { fd, error: None })
    }

    async fn handle_read(&self, req: ReadRequest) -> Result<ReadResponse, ErrorStatus> {
        let mut fds = self.fds.lock().unwrap();
        let file = fds.get_mut(&req.fd).ok_or_else(|| ErrorStatus {
            code: VamErrorCode::InvalidArgument as i32,
            message: "Invalid file descriptor".to_string(),
            retryable: false,
            details: Vec::new(),
        })?;

        file.seek(SeekFrom::Start(req.offset))
            .map_err(|e| ErrorStatus {
                code: VamErrorCode::Internal as i32,
                message: format!("Seek failed: {}", e),
                retryable: false,
                details: Vec::new(),
            })?;

        let mut buf = vec![0u8; req.size as usize];
        let bytes_read = file.read(&mut buf).map_err(|e| ErrorStatus {
            code: VamErrorCode::Internal as i32,
            message: format!("Read failed: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;
        buf.truncate(bytes_read);

        Ok(ReadResponse {
            data: buf,
            error: None,
        })
    }

    async fn handle_write(&self, req: WriteRequest) -> Result<WriteResponse, ErrorStatus> {
        // Enforce quota
        let current_size = get_dir_size(&self.sandbox_root);
        if current_size + req.data.len() as u64 > self.quota_bytes {
            return Err(ErrorStatus {
                code: VamErrorCode::ResourceExhausted as i32,
                message: "Storage quota exceeded".to_string(),
                retryable: false,
                details: Vec::new(),
            });
        }

        // Host-hash bytes as required
        let _hash = crate::cas::sha256_hex(&req.data);

        let mut fds = self.fds.lock().unwrap();
        let file = fds.get_mut(&req.fd).ok_or_else(|| ErrorStatus {
            code: VamErrorCode::InvalidArgument as i32,
            message: "Invalid file descriptor".to_string(),
            retryable: false,
            details: Vec::new(),
        })?;

        file.seek(SeekFrom::Start(req.offset))
            .map_err(|e| ErrorStatus {
                code: VamErrorCode::Internal as i32,
                message: format!("Seek failed: {}", e),
                retryable: false,
                details: Vec::new(),
            })?;

        file.write_all(&req.data).map_err(|e| ErrorStatus {
            code: VamErrorCode::Internal as i32,
            message: format!("Write failed: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;

        Ok(WriteResponse {
            bytes_written: req.data.len() as u32,
            error: None,
        })
    }

    async fn handle_close(&self, req: CloseRequest) -> Result<CloseResponse, ErrorStatus> {
        let mut fds = self.fds.lock().unwrap();
        if fds.remove(&req.fd).is_none() {
            return Err(ErrorStatus {
                code: VamErrorCode::InvalidArgument as i32,
                message: "Invalid file descriptor".to_string(),
                retryable: false,
                details: Vec::new(),
            });
        }
        Ok(CloseResponse { error: None })
    }

    async fn handle_fsync(&self, req: FsyncRequest) -> Result<FsyncResponse, ErrorStatus> {
        let mut fds = self.fds.lock().unwrap();
        let file = fds.get_mut(&req.fd).ok_or_else(|| ErrorStatus {
            code: VamErrorCode::InvalidArgument as i32,
            message: "Invalid file descriptor".to_string(),
            retryable: false,
            details: Vec::new(),
        })?;

        if req.datasync {
            file.sync_data().map_err(|e| ErrorStatus {
                code: VamErrorCode::Internal as i32,
                message: format!("sync_data failed: {}", e),
                retryable: false,
                details: Vec::new(),
            })?;
        } else {
            file.sync_all().map_err(|e| ErrorStatus {
                code: VamErrorCode::Internal as i32,
                message: format!("sync_all failed: {}", e),
                retryable: false,
                details: Vec::new(),
            })?;
        }

        Ok(FsyncResponse { error: None })
    }

    async fn handle_stat(&self, req: StatRequest) -> Result<StatResponse, ErrorStatus> {
        let abs_path = self.resolve_path(&req.path)?;
        let metadata = fs::metadata(&abs_path).map_err(|e| ErrorStatus {
            code: VamErrorCode::NotFound as i32,
            message: format!("Not found: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;

        let stat = FileStat {
            size: metadata.len(),
            mode: 0,
            mtime: metadata.modified().ok().and_then(to_proto_timestamp),
            ctime: metadata.created().ok().and_then(to_proto_timestamp),
            is_dir: metadata.is_dir(),
        };

        Ok(StatResponse {
            stat: Some(stat),
            error: None,
        })
    }

    async fn handle_readdir(&self, req: ReaddirRequest) -> Result<ReaddirResponse, ErrorStatus> {
        let abs_path = self.resolve_path(&req.path)?;
        let mut entries = Vec::new();

        let rd = fs::read_dir(&abs_path).map_err(|e| ErrorStatus {
            code: VamErrorCode::Internal as i32,
            message: format!("Failed to read directory: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;

        for entry in rd.flatten() {
            if let Ok(metadata) = entry.metadata() {
                let name = entry.file_name().to_string_lossy().into_owned();
                let stat = FileStat {
                    size: metadata.len(),
                    mode: 0,
                    mtime: metadata.modified().ok().and_then(to_proto_timestamp),
                    ctime: metadata.created().ok().and_then(to_proto_timestamp),
                    is_dir: metadata.is_dir(),
                };
                entries.push(DirEntry {
                    name,
                    stat: Some(stat),
                });
            }
        }

        Ok(ReaddirResponse {
            entries,
            error: None,
        })
    }

    async fn handle_mkdir(&self, req: MkdirRequest) -> Result<MkdirResponse, ErrorStatus> {
        let abs_path = self.resolve_path(&req.path)?;
        fs::create_dir_all(&abs_path).map_err(|e| ErrorStatus {
            code: VamErrorCode::Internal as i32,
            message: format!("mkdir failed: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;
        Ok(MkdirResponse { error: None })
    }

    async fn handle_rmdir(&self, req: RmdirRequest) -> Result<RmdirResponse, ErrorStatus> {
        let abs_path = self.resolve_path(&req.path)?;
        fs::remove_dir(&abs_path).map_err(|e| ErrorStatus {
            code: VamErrorCode::Internal as i32,
            message: format!("rmdir failed: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;
        Ok(RmdirResponse { error: None })
    }

    async fn handle_unlink(&self, req: UnlinkRequest) -> Result<UnlinkResponse, ErrorStatus> {
        let abs_path = self.resolve_path(&req.path)?;
        fs::remove_file(&abs_path).map_err(|e| ErrorStatus {
            code: VamErrorCode::Internal as i32,
            message: format!("unlink failed: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;
        Ok(UnlinkResponse { error: None })
    }

    async fn handle_rename(&self, req: RenameRequest) -> Result<RenameResponse, ErrorStatus> {
        let old_abs = self.resolve_path(&req.old_path)?;
        let new_abs = self.resolve_path(&req.new_path)?;
        fs::rename(&old_abs, &new_abs).map_err(|e| ErrorStatus {
            code: VamErrorCode::Internal as i32,
            message: format!("rename failed: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;
        Ok(RenameResponse { error: None })
    }

    async fn handle_symlink(&self, req: SymlinkRequest) -> Result<SymlinkResponse, ErrorStatus> {
        let link_abs = self.resolve_path(&req.link_path)?;
        symlink(&req.target, &link_abs).map_err(|e| ErrorStatus {
            code: VamErrorCode::Internal as i32,
            message: format!("symlink failed: {}", e),
            retryable: false,
            details: Vec::new(),
        })?;
        Ok(SymlinkResponse { error: None })
    }

    async fn handle_flush(&self, _req: FlushRequest) -> Result<FlushResponse, ErrorStatus> {
        Ok(FlushResponse { error: None })
    }

    async fn handle_checkpoint(
        &self,
        req: CheckpointRequest,
    ) -> Result<CheckpointResponse, ErrorStatus> {
        use crate::CheckpointCoordinator;
        let coord_req = crate::CheckpointRequest {
            workspace_id: ResourceId {
                urn: req.session_id.clone(),
            },
            checkpoint_name: req.description.clone(),
        };
        let checkpoint_ref = self
            .checkpoint_coordinator
            .checkpoint(&self.sandbox_root, &coord_req)
            .map_err(|e| ErrorStatus {
                code: VamErrorCode::Internal as i32,
                message: format!("checkpoint failed: {}", e),
                retryable: false,
                details: Vec::new(),
            })?;
        Ok(CheckpointResponse {
            checkpoint_ref: Some(checkpoint_ref),
            error: None,
        })
    }
}
