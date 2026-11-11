//! Host-stored conversation checkpoints.
//!
//! The guest uploads the continuation in chunks when a tool call suspends.
//! The host assembles those chunks, checks the SHA-256 digest, and stores the
//! exact bytes per run and lifecycle generation (cap 4 MiB). Resume carries
//! only the digest. The guest fetches the bytes and checks the digest itself.
//! The workspace file `.vam/agent/hitl-continuation.json` is not authoritative.
//!
//! Only schema 1 and 2 (HITL continuations) are stored here. A thread's
//! conversation lives in the workspace thread log, recorded by the host
//! (decision 23); a schema 3 turn checkpoint upload is refused until the
//! suspend tail is accepted into that log. The host derives the workspace and
//! thread from the session, never from guest bytes, and a guest can fetch only
//! digests the host named for its run.

use rusqlite::{params, OptionalExtension};
use sha2::{Digest, Sha256};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;
use vam_spec::v1::{
    ErrorStatus, ExecutionLease, FetchRunCheckpointRequest, FetchRunCheckpointResponse,
    UploadRunCheckpointRequest, UploadRunCheckpointResponse,
};

/// Host cap for one stored checkpoint. The guest mirrors this.
pub const MAX_RUN_CHECKPOINT_BYTES: usize = 4 * 1024 * 1024;

/// Maximum checkpoint bytes in one upload or fetch message.
///
/// The guest mirrors this. A 32 KiB chunk plus a lease still fits in one
/// 64 KiB vsock frame.
pub const CHECKPOINT_CHUNK_BYTES: usize = 32 * 1024;

const SCHEMA_VERSION_V1: u32 = 1;
const SCHEMA_VERSION_V2: u32 = 2;
const SCHEMA_VERSION_TURN: u32 = 3;

/// Task-input key naming the digest of the thread's chain head (set by the host).
pub const THREAD_HEAD_DIGEST_INPUT: &str = "thread_head_digest";
/// Task-input key with the head's sequence number, as a string.
pub const THREAD_HEAD_SEQ_INPUT: &str = "thread_head_seq";
/// Task-input key with the model's context window in tokens (a number).
pub const CONTEXT_WINDOW_INPUT: &str = "context_window";

/// A digest-verified checkpoint stored for one run generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCheckpoint {
    pub run_id: String,
    pub lifecycle_generation: i64,
    pub digest: String,
    pub blob_ref: String,
    pub bytes: Vec<u8>,
}

/// Digest and generation referenced by a resume task.
///
/// The resume task does not carry the checkpoint bytes. The guest fetches
/// them with `FetchRunCheckpoint`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumeCheckpoint {
    pub digest: String,
    pub lifecycle_generation: i64,
}

/// Per-connection staging for a chunked `UploadRunCheckpoint`.
///
/// The first chunk fixes the run, lifecycle generation, digest, and
/// `total_size`. Later chunks must continue at `received` with the same lease
/// run, generation, and digest, and must not pass `total_size`. When
/// `received == total_size` the assembled bytes are stored. Any error drops
/// the staged bytes. `total_size = 0` stores one message immediately.
#[derive(Debug, Default)]
pub struct CheckpointUpload {
    run_id: Option<String>,
    lifecycle_generation: i64,
    digest: String,
    total_size: u64,
    buffer: Vec<u8>,
    /// A completed schema-3 suspend tail waiting for the async log step.
    suspend: Option<SuspendUpload>,
}

/// A verified suspend tail, ready to be recorded.
#[derive(Debug)]
pub struct SuspendUpload {
    pub digest: String,
    pub tail: vam_spec::thread_context::SuspendTail,
}

#[derive(Debug, serde::Deserialize)]
struct CheckpointHeader {
    schema_version: u32,
    run_id: String,
    lifecycle_generation: i64,
}

pub(crate) struct BoundLease {
    pub(crate) run_id: String,
    #[allow(dead_code)]
    pub(crate) lifecycle_generation: i64,
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn blob_ref_for(digest: &str) -> String {
    format!("vam://run-checkpoint/{digest}")
}

fn unix_timestamp() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn checkpoint_error(code: i32, message: impl Into<String>) -> UploadRunCheckpointResponse {
    UploadRunCheckpointResponse {
        accepted: false,
        digest: String::new(),
        error: Some(ErrorStatus {
            code,
            message: message.into(),
            details: vec![],
            retryable: false,
        }),
        received: 0,
        complete: false,
    }
}

fn fetch_error(code: i32, message: impl Into<String>) -> FetchRunCheckpointResponse {
    FetchRunCheckpointResponse {
        chunk: Vec::new(),
        total_size: 0,
        last: false,
        error: Some(ErrorStatus {
            code,
            message: message.into(),
            details: vec![],
            retryable: false,
        }),
    }
}

fn status_error(err: (i32, String)) -> UploadRunCheckpointResponse {
    checkpoint_error(err.0, err.1)
}

fn schema_supported(version: u32) -> bool {
    version == SCHEMA_VERSION_V1 || version == SCHEMA_VERSION_V2 || version == SCHEMA_VERSION_TURN
}

fn verify_checkpoint_bytes(
    bytes: &[u8],
    claimed_digest: &str,
    run_id: &str,
    lifecycle_generation: i64,
) -> Result<(), OrchestratorError> {
    if bytes.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "checkpoint is empty".into(),
        ));
    }
    if bytes.len() > MAX_RUN_CHECKPOINT_BYTES {
        return Err(OrchestratorError::InvalidContract(format!(
            "checkpoint is {} bytes; max is {MAX_RUN_CHECKPOINT_BYTES}",
            bytes.len()
        )));
    }
    if lifecycle_generation <= 0 {
        return Err(OrchestratorError::InvalidContract(
            "lifecycle_generation must be positive".into(),
        ));
    }
    let actual = sha256_hex(bytes);
    if actual != claimed_digest {
        return Err(OrchestratorError::InvalidContract(
            "checkpoint digest mismatch".into(),
        ));
    }
    let header: CheckpointHeader = serde_json::from_slice(bytes).map_err(|e| {
        OrchestratorError::InvalidContract(format!("checkpoint JSON is invalid: {e}"))
    })?;
    if !schema_supported(header.schema_version) {
        return Err(OrchestratorError::InvalidContract(format!(
            "checkpoint schema_version {} is not supported",
            header.schema_version
        )));
    }
    if header.run_id != run_id {
        return Err(OrchestratorError::InvalidContract(
            "checkpoint run_id does not match the lease".into(),
        ));
    }
    if header.lifecycle_generation != lifecycle_generation {
        return Err(OrchestratorError::InvalidContract(
            "checkpoint lifecycle_generation does not match the lease".into(),
        ));
    }
    Ok(())
}

fn verified_checkpoint(
    run_id: &str,
    lifecycle_generation: i64,
    digest: String,
    blob_ref: String,
    bytes: Vec<u8>,
) -> Result<RunCheckpoint, OrchestratorError> {
    let actual = sha256_hex(&bytes);
    if actual != digest || blob_ref != blob_ref_for(&digest) {
        return Err(OrchestratorError::CorruptState(format!(
            "stored checkpoint for {run_id} generation {lifecycle_generation} failed digest verification"
        )));
    }
    Ok(RunCheckpoint {
        run_id: run_id.to_string(),
        lifecycle_generation,
        digest,
        blob_ref,
        bytes,
    })
}

pub(crate) fn bind_checkpoint_lease(
    store: &SqliteWorkflowStore,
    session_id: &str,
    lease: Option<&ExecutionLease>,
) -> Result<BoundLease, (i32, String)> {
    let Some(lease) = lease else {
        return Err((3, "lease is required".into()));
    };
    let Some(run_id) = lease.run_id.as_ref().map(|id| id.urn.clone()) else {
        return Err((3, "lease run_id is required".into()));
    };
    if run_id.is_empty() {
        return Err((3, "lease run_id is required".into()));
    }
    if lease.lifecycle_generation <= 0 {
        return Err((3, "lifecycle_generation must be positive".into()));
    }
    match store.find_run_id_for_session(session_id) {
        Ok(Some(bound)) if bound == run_id => {}
        Ok(Some(_)) => {
            return Err((7, "checkpoint run does not match this session".into()));
        }
        Ok(None) => return Err((7, "session has no run".into())),
        Err(err) => return Err((13, err.to_string())),
    }
    Ok(BoundLease {
        run_id,
        lifecycle_generation: lease.lifecycle_generation,
    })
}

fn require_sha256(digest: Option<&vam_spec::v1::Digest>) -> Result<&str, (i32, String)> {
    let Some(digest) = digest else {
        return Err((3, "checkpoint digest is required".into()));
    };
    if digest.algorithm != 1 {
        return Err((3, "checkpoint digest must be SHA-256".into()));
    }
    if digest.hex_hash.is_empty() {
        return Err((3, "checkpoint digest is required".into()));
    }
    Ok(digest.hex_hash.as_str())
}

impl SqliteWorkflowStore {
    /// Store `bytes` for `(run_id, lifecycle_generation)` after digest checks.
    ///
    /// A repeat upload of the same bytes is accepted. A different payload for
    /// the same generation is rejected.
    pub fn put_run_checkpoint(
        &self,
        run_id: &str,
        lifecycle_generation: i64,
        claimed_digest: &str,
        bytes: &[u8],
    ) -> Result<RunCheckpoint, OrchestratorError> {
        if run_id.is_empty() {
            return Err(OrchestratorError::InvalidContract(
                "run_id is required".into(),
            ));
        }
        verify_checkpoint_bytes(bytes, claimed_digest, run_id, lifecycle_generation)?;

        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let transaction = connection.unchecked_transaction()?;
        let existing = transaction
            .query_row(
                "SELECT lifecycle_generation, digest, blob_ref, checkpoint_bytes
                 FROM run_checkpoints
                 WHERE run_id = ?1 AND lifecycle_generation = ?2",
                params![run_id, lifecycle_generation],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()?;
        if let Some((generation, digest, blob_ref, stored)) = existing {
            if digest != claimed_digest || stored.as_slice() != bytes {
                return Err(OrchestratorError::Conflict(format!(
                    "run {run_id} generation {lifecycle_generation} already has a different checkpoint"
                )));
            }
            if blob_ref != blob_ref_for(&digest) {
                return Err(OrchestratorError::CorruptState(format!(
                    "stored checkpoint for {run_id} generation {generation} failed digest verification"
                )));
            }
            return Ok(RunCheckpoint {
                run_id: run_id.to_string(),
                lifecycle_generation: generation,
                digest,
                blob_ref,
                bytes: stored,
            });
        }

        let digest = claimed_digest.to_string();
        let blob_ref = blob_ref_for(&digest);
        transaction.execute(
            "INSERT INTO run_checkpoints (
                run_id, lifecycle_generation, digest, blob_ref, checkpoint_bytes, created_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                run_id,
                lifecycle_generation,
                digest,
                blob_ref,
                bytes,
                unix_timestamp(),
            ],
        )?;
        transaction.commit()?;
        Ok(RunCheckpoint {
            run_id: run_id.to_string(),
            lifecycle_generation,
            digest,
            blob_ref,
            bytes: bytes.to_vec(),
        })
    }

    /// Newest checkpoint for `run_id`.
    ///
    /// Resume uses this rather than the new lease generation: the continuation
    /// was stored under the generation that suspended.
    pub fn latest_run_checkpoint(
        &self,
        run_id: &str,
    ) -> Result<Option<RunCheckpoint>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = connection
            .query_row(
                "SELECT lifecycle_generation, digest, blob_ref, checkpoint_bytes
                 FROM run_checkpoints
                 WHERE run_id = ?1
                 ORDER BY lifecycle_generation DESC
                 LIMIT 1",
                [run_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                },
            )
            .optional()?;
        let Some((generation, digest, blob_ref, bytes)) = row else {
            return Ok(None);
        };
        verified_checkpoint(run_id, generation, digest, blob_ref, bytes).map(Some)
    }
}

struct RunCheckpointStored {
    digest: String,
    size: u64,
}

/// The `(workspace_id, thread_ref)` this session's run writes to. Derived on the
/// host from the session row and the run spec; the guest never names either.
pub fn thread_scope_for_session(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
) -> Result<(String, String), OrchestratorError> {
    let workspace_id: Option<String> = {
        let connection = store
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT workspace_id FROM sessions WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .optional()?
    };
    let Some(workspace_id) = workspace_id.filter(|w| !w.is_empty()) else {
        return Err(OrchestratorError::InvalidContract(
            "session has no workspace for a thread checkpoint".into(),
        ));
    };
    let (spec_json, _, _) = store.get_run_spec_context(run_id)?;
    let thread_ref = match spec_json {
        None => crate::run_context::DEFAULT_THREAD_REF.to_string(),
        Some(json) => {
            let spec =
                vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &json)
                    .map_err(OrchestratorError::Core)?;
            crate::run_context::extract_thread_ref(&spec)
                .unwrap_or_else(|_| crate::run_context::DEFAULT_THREAD_REF.to_string())
        }
    };
    Ok((workspace_id, thread_ref))
}

impl SqliteWorkflowStore {
    /// Bytes of `digest` if it is `run_id`'s newest HITL checkpoint. `None`
    /// otherwise: the same answer for a digest that does not exist and one this
    /// run was not given.
    ///
    /// A thread's conversation lives in the workspace thread log (decision 23),
    /// not here. `run_checkpoint_grants` stays for the host-granted fetch of the
    /// assembled context that the thread-log resume adds.
    pub fn checkpoint_bytes_for_run(
        &self,
        run_id: &str,
        digest: &str,
    ) -> Result<Option<Vec<u8>>, OrchestratorError> {
        if let Some(stored) = self.latest_run_checkpoint(run_id)? {
            if stored.digest == digest {
                return Ok(Some(stored.bytes));
            }
        }
        Ok(None)
    }
}

impl SqliteWorkflowStore {
    /// Lets `run_id` fetch the assembled context with this digest (and no other
    /// run). The bytes live beside the session, not here.
    pub fn grant_run_context(&self, run_id: &str, digest: &str) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT OR IGNORE INTO run_checkpoint_grants (run_id, digest) VALUES (?1, ?2)",
            params![run_id, digest],
        )?;
        Ok(())
    }

    pub fn run_context_granted(&self, run_id: &str, digest: &str) -> bool {
        let Ok(connection) = self.connection.lock() else {
            return false;
        };
        connection
            .query_row(
                "SELECT 1 FROM run_checkpoint_grants WHERE run_id = ?1 AND digest = ?2",
                params![run_id, digest],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }

    /// Digests granted to a run, to remove their files when the run ends.
    pub fn revoke_run_context_grants(&self, run_id: &str) -> Vec<String> {
        let Ok(connection) = self.connection.lock() else {
            return Vec::new();
        };
        let digests: Vec<String> = connection
            .prepare("SELECT digest FROM run_checkpoint_grants WHERE run_id = ?1")
            .and_then(|mut stmt| {
                stmt.query_map([run_id], |row| row.get::<_, String>(0))
                    .map(|rows| rows.flatten().collect())
            })
            .unwrap_or_default();
        let _ = connection.execute(
            "DELETE FROM run_checkpoint_grants WHERE run_id = ?1",
            [run_id],
        );
        digests
    }
}

/// Digest and generation of the checkpoint a resume task should reference.
pub fn resume_checkpoint(
    store: &SqliteWorkflowStore,
    run_id: &str,
) -> Result<Option<ResumeCheckpoint>, OrchestratorError> {
    let Some(stored) = store.latest_run_checkpoint(run_id)? else {
        return Ok(None);
    };
    Ok(Some(ResumeCheckpoint {
        digest: stored.digest,
        lifecycle_generation: stored.lifecycle_generation,
    }))
}

impl CheckpointUpload {
    /// Accept one chunk on this connection.
    ///
    /// `total_size` above [`MAX_RUN_CHECKPOINT_BYTES`] is rejected before the
    /// chunk is appended. A failure drops any bytes already staged.
    pub fn accept_chunk(
        &mut self,
        store: &SqliteWorkflowStore,
        session_id: &str,
        request: &UploadRunCheckpointRequest,
    ) -> UploadRunCheckpointResponse {
        match self.accept_chunk_inner(store, session_id, request) {
            Ok(_) if self.suspend.take().is_some() => checkpoint_error(
                3,
                "schema 3 suspend tails are accepted on the session broker only",
            ),
            Ok(response) => response,
            Err(response) => {
                self.clear();
                response
            }
        }
    }

    /// [`Self::accept_chunk`] for the session broker: a completed suspend tail
    /// is validated and recorded in the thread log (decision 23). It is not
    /// stored in `run_checkpoints`.
    pub async fn accept_chunk_with_log(
        &mut self,
        store: &SqliteWorkflowStore,
        session_id: &str,
        request: &UploadRunCheckpointRequest,
    ) -> UploadRunCheckpointResponse {
        match self.accept_chunk_inner(store, session_id, request) {
            Ok(response) => match self.suspend.take() {
                None => response,
                Some(upload) => {
                    let Some(recorder) = store.thread_recorder(session_id) else {
                        return checkpoint_error(3, "the session has no thread recorder");
                    };
                    match recorder.record_suspend(&upload.tail, &upload.digest).await {
                        Ok(()) => {
                            // The tail names the last steer the task applied.
                            crate::inbox::ack_through(
                                store,
                                &upload.tail.run_id,
                                upload.tail.state.inbox_seq,
                                Some(recorder.rounds()),
                            );
                            response
                        }
                        Err(message) => checkpoint_error(3, message),
                    }
                }
            },
            Err(response) => {
                self.clear();
                response
            }
        }
    }

    fn clear(&mut self) {
        *self = Self::default();
    }

    #[cfg(test)]
    fn staged_len(&self) -> usize {
        self.buffer.len()
    }

    fn accept_chunk_inner(
        &mut self,
        store: &SqliteWorkflowStore,
        session_id: &str,
        request: &UploadRunCheckpointRequest,
    ) -> Result<UploadRunCheckpointResponse, UploadRunCheckpointResponse> {
        let bound = bind_checkpoint_lease(store, session_id, request.lease.as_ref())
            .map_err(status_error)?;
        let digest = require_sha256(request.digest.as_ref()).map_err(status_error)?;

        if request.total_size == 0 {
            if self.run_id.is_some() {
                return Err(checkpoint_error(
                    3,
                    "checkpoint total_size changed during upload",
                ));
            }
            return store_assembled(
                store,
                &mut self.suspend,
                &bound.run_id,
                bound.lifecycle_generation,
                digest,
                &request.checkpoint,
            );
        }

        // Reject an over-cap declaration before this chunk is appended.
        if request.total_size > MAX_RUN_CHECKPOINT_BYTES as u64 {
            return Err(checkpoint_error(
                3,
                format!(
                    "checkpoint is {} bytes; max is {MAX_RUN_CHECKPOINT_BYTES}",
                    request.total_size
                ),
            ));
        }

        let chunk_len = request.checkpoint.len();
        if chunk_len > CHECKPOINT_CHUNK_BYTES {
            return Err(checkpoint_error(
                3,
                format!("checkpoint chunk is {chunk_len} bytes; max is {CHECKPOINT_CHUNK_BYTES}"),
            ));
        }
        if chunk_len == 0 {
            return Err(checkpoint_error(3, "checkpoint chunk is empty"));
        }
        let chunk_end = request
            .offset
            .checked_add(chunk_len as u64)
            .ok_or_else(|| checkpoint_error(3, "checkpoint chunk offset overflows"))?;
        if chunk_end > request.total_size {
            return Err(checkpoint_error(3, "checkpoint chunk exceeds total_size"));
        }

        if let Some(run_id) = self.run_id.as_deref() {
            if run_id != bound.run_id.as_str() {
                return Err(checkpoint_error(3, "checkpoint run changed during upload"));
            }
            if self.lifecycle_generation != bound.lifecycle_generation {
                return Err(checkpoint_error(
                    3,
                    "checkpoint lifecycle_generation changed during upload",
                ));
            }
            if self.digest != digest {
                return Err(checkpoint_error(
                    3,
                    "checkpoint digest changed during upload",
                ));
            }
            if self.total_size != request.total_size {
                return Err(checkpoint_error(
                    3,
                    "checkpoint total_size changed during upload",
                ));
            }
            if request.offset != self.buffer.len() as u64 {
                return Err(checkpoint_error(
                    3,
                    format!(
                        "checkpoint chunk offset {} does not continue at {}",
                        request.offset,
                        self.buffer.len()
                    ),
                ));
            }
        } else if request.offset != 0 {
            return Err(checkpoint_error(
                3,
                format!(
                    "checkpoint chunk offset {} does not continue at 0",
                    request.offset
                ),
            ));
        } else {
            self.run_id = Some(bound.run_id.clone());
            self.lifecycle_generation = bound.lifecycle_generation;
            self.digest = digest.to_string();
            self.total_size = request.total_size;
        }

        self.buffer.extend_from_slice(&request.checkpoint);
        if (self.buffer.len() as u64) < self.total_size {
            return Ok(UploadRunCheckpointResponse {
                accepted: true,
                digest: digest.to_string(),
                error: None,
                received: self.buffer.len() as u64,
                complete: false,
            });
        }

        let bytes = std::mem::take(&mut self.buffer);
        let run_id = match self.run_id.clone() {
            Some(run_id) => run_id,
            None => return Err(checkpoint_error(13, "checkpoint upload has no run")),
        };
        let generation = self.lifecycle_generation;
        let stored_digest = std::mem::take(&mut self.digest);
        self.clear();
        store_assembled(
            store,
            &mut self.suspend,
            &run_id,
            generation,
            &stored_digest,
            &bytes,
        )
    }
}

fn store_assembled(
    store: &SqliteWorkflowStore,
    suspend_out: &mut Option<SuspendUpload>,
    run_id: &str,
    lifecycle_generation: i64,
    digest: &str,
    bytes: &[u8],
) -> Result<UploadRunCheckpointResponse, UploadRunCheckpointResponse> {
    let is_turn = serde_json::from_slice::<CheckpointHeader>(bytes)
        .map(|h| h.schema_version == SCHEMA_VERSION_TURN)
        .unwrap_or(false);
    if is_turn {
        verify_checkpoint_bytes(bytes, digest, run_id, lifecycle_generation)
            .map_err(|e| checkpoint_error(3, e.to_string()))?;
        let tail: vam_spec::thread_context::SuspendTail = serde_json::from_slice(bytes)
            .map_err(|e| checkpoint_error(3, format!("suspend tail is invalid: {e}")))?;
        if tail.kind != "suspend" || tail.run_id != run_id {
            return Err(checkpoint_error(3, "suspend tail does not match the lease"));
        }
        *suspend_out = Some(SuspendUpload {
            digest: digest.to_string(),
            tail,
        });
        return Ok(UploadRunCheckpointResponse {
            accepted: true,
            digest: digest.to_string(),
            error: None,
            received: bytes.len() as u64,
            complete: true,
        });
    }
    let stored = store
        .put_run_checkpoint(run_id, lifecycle_generation, digest, bytes)
        .map(|c| RunCheckpointStored {
            digest: c.digest,
            size: c.bytes.len() as u64,
        });
    match stored {
        Ok(stored) => Ok(UploadRunCheckpointResponse {
            accepted: true,
            digest: stored.digest,
            error: None,
            received: stored.size,
            complete: true,
        }),
        Err(OrchestratorError::Conflict(message) | OrchestratorError::InvalidContract(message)) => {
            Err(checkpoint_error(3, message))
        }
        Err(err) => Err(checkpoint_error(13, err.to_string())),
    }
}

/// Accept a guest upload on the session's vsock broker.
///
/// One-shot helper for a legacy (`total_size = 0`) upload or a single chunk.
/// A multi-chunk upload must reuse one [`CheckpointUpload`] for the connection.
pub fn accept_run_checkpoint_upload(
    store: &SqliteWorkflowStore,
    session_id: &str,
    request: &UploadRunCheckpointRequest,
) -> UploadRunCheckpointResponse {
    CheckpointUpload::default().accept_chunk(store, session_id, request)
}

/// Return one chunk of the lease run's latest checkpoint.
///
/// The lease run must be the run bound to `session_id`, and the stored digest
/// must equal the requested digest. The slice is
/// `bytes[offset .. offset + CHECKPOINT_CHUNK_BYTES]`. Stateless.
pub fn fetch_run_checkpoint_chunk(
    store: &SqliteWorkflowStore,
    session_id: &str,
    request: &FetchRunCheckpointRequest,
) -> FetchRunCheckpointResponse {
    fetch_run_checkpoint_chunk_with(store, None, session_id, request)
}

/// [`fetch_run_checkpoint_chunk`] that also serves an assembled thread context
/// the host granted to this run (`layout` locates it beside the session).
pub fn fetch_run_checkpoint_chunk_with(
    store: &SqliteWorkflowStore,
    layout: Option<&vam_storage::Drive9Layout>,
    session_id: &str,
    request: &FetchRunCheckpointRequest,
) -> FetchRunCheckpointResponse {
    let bound = match bind_checkpoint_lease(store, session_id, request.lease.as_ref()) {
        Ok(bound) => bound,
        Err((code, message)) => return fetch_error(code, message),
    };
    let digest = match require_sha256(request.digest.as_ref()) {
        Ok(digest) => digest,
        Err((code, message)) => return fetch_error(code, message),
    };
    let bytes = match store.checkpoint_bytes_for_run(&bound.run_id, digest) {
        Ok(Some(bytes)) => bytes,
        Ok(None) if store.run_context_granted(&bound.run_id, digest) && layout.is_some() => {
            let Some(layout) = layout else {
                return fetch_error(3, "checkpoint digest mismatch");
            };
            let path = match crate::thread_context::context_path(layout, session_id, digest) {
                Ok(path) => path,
                Err(_) => return fetch_error(3, "checkpoint digest mismatch"),
            };
            return match crate::thread_context::read_context_chunk(
                &path,
                request.offset,
                CHECKPOINT_CHUNK_BYTES,
            ) {
                Ok((total, chunk)) => FetchRunCheckpointResponse {
                    last: request.offset + chunk.len() as u64 == total,
                    chunk,
                    total_size: total,
                    error: None,
                },
                Err(_) => fetch_error(3, "checkpoint digest mismatch"),
            };
        }
        Ok(None) => {
            // Same answer for a digest that does not exist and one this run was
            // not given. The legacy messages describe only this run's own state.
            return match store.latest_run_checkpoint(&bound.run_id) {
                Ok(Some(_)) => fetch_error(3, "checkpoint digest mismatch"),
                Ok(None) => fetch_error(5, "run has no checkpoint"),
                Err(err) => fetch_error(13, err.to_string()),
            };
        }
        Err(err) => return fetch_error(13, err.to_string()),
    };
    if request.offset > bytes.len() as u64 {
        return fetch_error(3, "checkpoint offset is past the end");
    }
    let offset = request.offset as usize;
    let end = offset
        .saturating_add(CHECKPOINT_CHUNK_BYTES)
        .min(bytes.len());
    FetchRunCheckpointResponse {
        chunk: bytes[offset..end].to_vec(),
        total_size: bytes.len() as u64,
        last: end == bytes.len(),
        error: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prost::Message;
    use vam_spec::v1::{FetchRunCheckpointRequest, ResourceId, VsockEnvelope};
    use vam_vsock::frame::MAX_PAYLOAD_SIZE;

    const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-checkpoint";
    const SESSION: &str = "sess-checkpoint";

    fn checkpoint_body(run_id: &str, generation: i64, marker: &str) -> Vec<u8> {
        format!(
            r#"{{"schema_version":1,"run_id":"{run_id}","lifecycle_generation":{generation},"marker":"{marker}"}}"#
        )
        .into_bytes()
    }

    fn checkpoint_body_with_len(
        run_id: &str,
        generation: i64,
        schema_version: u32,
        len: usize,
    ) -> Vec<u8> {
        let prefix = format!(
            r#"{{"schema_version":{schema_version},"run_id":"{run_id}","lifecycle_generation":{generation},"pad":""#
        );
        let suffix = "\"}";
        assert!(
            len >= prefix.len() + suffix.len(),
            "len {len} is shorter than the checkpoint header"
        );
        let mut body = Vec::with_capacity(len);
        body.extend_from_slice(prefix.as_bytes());
        body.resize(len - suffix.len(), b'a');
        body.extend_from_slice(suffix.as_bytes());
        assert_eq!(body.len(), len);
        body
    }

    fn store_with_run() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .persist_run_spec(
                RUN,
                r#"{"inputs":{"prompt":"continue"}}"#,
                Some(SESSION),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .expect("run");
        store
    }

    fn lease(generation: i64) -> ExecutionLease {
        ExecutionLease {
            run_id: Some(ResourceId {
                urn: RUN.to_string(),
            }),
            step_id: "live-step".to_string(),
            attempt_id: Some(ResourceId {
                urn: "urn:vam:attempt:local.vam.dev/acme/attempt-1".to_string(),
            }),
            lease_id: Some(ResourceId {
                urn: "urn:vam:lease:local.vam.dev/acme/lease-1".to_string(),
            }),
            lifecycle_generation: generation,
            fencing_token: vec![1],
            expires_at: None,
            idempotency_key: "checkpoint-lease".to_string(),
        }
    }

    fn proto_digest(hex_hash: &str) -> vam_spec::v1::Digest {
        vam_spec::v1::Digest {
            algorithm: 1,
            hex_hash: hex_hash.to_string(),
        }
    }

    fn chunk_request(
        bytes: Vec<u8>,
        generation: i64,
        digest_hex: &str,
        offset: u64,
        total_size: u64,
    ) -> UploadRunCheckpointRequest {
        UploadRunCheckpointRequest {
            lease: Some(lease(generation)),
            checkpoint: bytes,
            digest: Some(proto_digest(digest_hex)),
            offset,
            total_size,
        }
    }

    fn upload(bytes: Vec<u8>, generation: i64) -> UploadRunCheckpointRequest {
        let digest = sha256_hex(&bytes);
        chunk_request(bytes, generation, &digest, 0, 0)
    }

    fn stored_checkpoint_count(store: &SqliteWorkflowStore) -> i64 {
        let connection = store.connection.lock().expect("lock");
        connection
            .query_row("SELECT COUNT(*) FROM run_checkpoints", [], |row| row.get(0))
            .expect("count")
    }

    fn assert_nothing_stored(store: &SqliteWorkflowStore) {
        assert!(store.latest_run_checkpoint(RUN).expect("load").is_none());
        assert_eq!(stored_checkpoint_count(store), 0);
    }

    fn envelope_len<M: Message>(method: &str, message: &M) -> usize {
        let mut payload = Vec::new();
        message.encode(&mut payload).expect("payload");
        let envelope = VsockEnvelope {
            method: method.to_string(),
            correlation_id: 7,
            payload,
            error: None,
            trace_context: None,
        };
        let mut bytes = Vec::new();
        envelope.encode(&mut bytes).expect("envelope");
        bytes.len()
    }

    #[test]
    fn upload_stores_per_generation_and_resume_returns_latest() {
        let store = store_with_run();
        let first = checkpoint_body(RUN, 1, "first-suspend");
        let second = checkpoint_body(RUN, 2, "second-suspend");
        let first_digest = sha256_hex(&first);

        let accepted = accept_run_checkpoint_upload(&store, SESSION, &upload(first.clone(), 1));
        assert!(accepted.accepted, "{:?}", accepted.error);
        assert!(accepted.complete);
        assert_eq!(accepted.digest, first_digest);

        let again = accept_run_checkpoint_upload(&store, SESSION, &upload(first.clone(), 1));
        assert!(again.accepted, "identical re-upload is idempotent");

        let replaced = upload(checkpoint_body(RUN, 1, "tampered"), 1);
        let rejected = accept_run_checkpoint_upload(&store, SESSION, &replaced);
        assert!(!rejected.accepted);
        assert!(rejected
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("different checkpoint")));

        let second_accepted =
            accept_run_checkpoint_upload(&store, SESSION, &upload(second.clone(), 2));
        assert!(second_accepted.accepted, "{:?}", second_accepted.error);

        let resumed = resume_checkpoint(&store, RUN)
            .expect("load")
            .expect("checkpoint");
        assert_eq!(resumed.lifecycle_generation, 2);
        assert_eq!(resumed.digest, sha256_hex(&second));
        let stored = store
            .latest_run_checkpoint(RUN)
            .expect("load")
            .expect("row");
        assert_eq!(stored.bytes, second);
        assert_ne!(stored.bytes, first);
    }

    #[test]
    fn upload_rejects_digest_mismatch_and_foreign_session() {
        let store = store_with_run();
        let mut request = upload(checkpoint_body(RUN, 1, "body"), 1);
        request.digest.as_mut().expect("digest").hex_hash = "ab".repeat(32);
        let mismatch = accept_run_checkpoint_upload(&store, SESSION, &request);
        assert!(!mismatch.accepted);
        assert!(mismatch
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("digest mismatch")));
        assert_nothing_stored(&store);

        let foreign = accept_run_checkpoint_upload(
            &store,
            "sess-other",
            &upload(checkpoint_body(RUN, 1, "body"), 1),
        );
        assert!(!foreign.accepted);
        assert!(foreign.error.as_ref().is_some_and(
            |err| err.message.contains("no run") || err.message.contains("does not match")
        ));
    }

    #[test]
    fn upload_rejects_run_id_that_does_not_match_lease() {
        let store = store_with_run();
        let bytes = checkpoint_body("urn:vam:run:local.vam.dev/acme/other", 1, "body");
        let rejected = accept_run_checkpoint_upload(&store, SESSION, &upload(bytes, 1));
        assert!(!rejected.accepted);
        assert!(rejected
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("run_id")));
    }

    #[test]
    fn chunked_one_mib_upload_stores_once_and_fetches_identically() {
        let store = store_with_run();
        let body = checkpoint_body_with_len(RUN, 1, 1, 1024 * 1024);
        let digest = sha256_hex(&body);
        let mut upload_state = CheckpointUpload::default();
        let mut offset = 0usize;
        while offset < body.len() {
            let end = (offset + CHECKPOINT_CHUNK_BYTES).min(body.len());
            let response = upload_state.accept_chunk(
                &store,
                SESSION,
                &chunk_request(
                    body[offset..end].to_vec(),
                    1,
                    &digest,
                    offset as u64,
                    body.len() as u64,
                ),
            );
            assert!(response.accepted, "{:?}", response.error);
            assert_eq!(response.received, end as u64);
            assert_eq!(response.digest, digest);
            let complete = end == body.len();
            assert_eq!(response.complete, complete);
            if !complete {
                assert_nothing_stored(&store);
            }
            offset = end;
        }
        assert_eq!(stored_checkpoint_count(&store), 1);
        let stored = store
            .latest_run_checkpoint(RUN)
            .expect("load")
            .expect("row");
        assert_eq!(stored.bytes, body);
        assert_eq!(stored.digest, digest);

        let mut fetched = Vec::new();
        let mut fetch_offset = 0u64;
        let mut saw_last = false;
        for _ in 0..(body.len() / CHECKPOINT_CHUNK_BYTES + 2) {
            let response = fetch_run_checkpoint_chunk(
                &store,
                SESSION,
                &FetchRunCheckpointRequest {
                    lease: Some(lease(1)),
                    digest: Some(proto_digest(&digest)),
                    offset: fetch_offset,
                },
            );
            assert!(response.error.is_none(), "{:?}", response.error);
            assert_eq!(response.total_size, body.len() as u64);
            assert!(!response.chunk.is_empty());
            assert!(response.chunk.len() <= CHECKPOINT_CHUNK_BYTES);
            let expect_last = fetch_offset + response.chunk.len() as u64 == body.len() as u64;
            assert_eq!(response.last, expect_last);
            fetched.extend_from_slice(&response.chunk);
            fetch_offset += response.chunk.len() as u64;
            if response.last {
                saw_last = true;
                break;
            }
        }
        assert!(saw_last);
        assert_eq!(fetched, body);
        assert_eq!(sha256_hex(&fetched), digest);
    }

    #[test]
    fn total_size_above_cap_is_rejected_before_buffering() {
        let store = store_with_run();
        let mut upload_state = CheckpointUpload::default();
        let total = (MAX_RUN_CHECKPOINT_BYTES as u64) + 1;
        let response = upload_state.accept_chunk(
            &store,
            SESSION,
            &chunk_request(vec![0x1], 1, &"ab".repeat(32), 0, total),
        );
        assert!(!response.accepted);
        assert!(!response.complete);
        assert_eq!(response.received, 0);
        let message = response.error.expect("error").message;
        assert!(
            message.contains(&total.to_string()),
            "cap rejection should name the declared size, got {message}"
        );
        assert!(
            message.contains(&MAX_RUN_CHECKPOINT_BYTES.to_string()),
            "cap rejection should name the max, got {message}"
        );
        assert_eq!(upload_state.staged_len(), 0);
        assert_nothing_stored(&store);
    }

    #[test]
    fn out_of_order_offset_stores_nothing() {
        let store = store_with_run();
        let body = checkpoint_body_with_len(RUN, 1, 1, CHECKPOINT_CHUNK_BYTES * 3);
        let digest = sha256_hex(&body);
        let mut upload_state = CheckpointUpload::default();
        let first = upload_state.accept_chunk(
            &store,
            SESSION,
            &chunk_request(
                body[..CHECKPOINT_CHUNK_BYTES].to_vec(),
                1,
                &digest,
                0,
                body.len() as u64,
            ),
        );
        assert!(first.accepted && !first.complete, "{:?}", first.error);
        assert_nothing_stored(&store);

        let skipped_at = (CHECKPOINT_CHUNK_BYTES * 2) as u64;
        let skipped = upload_state.accept_chunk(
            &store,
            SESSION,
            &chunk_request(
                body[CHECKPOINT_CHUNK_BYTES * 2..].to_vec(),
                1,
                &digest,
                skipped_at,
                body.len() as u64,
            ),
        );
        assert!(!skipped.accepted);
        assert!(skipped
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("does not continue")));
        assert_eq!(upload_state.staged_len(), 0);
        assert_nothing_stored(&store);
    }

    #[test]
    fn digest_change_mid_upload_stores_nothing() {
        let store = store_with_run();
        let body = checkpoint_body_with_len(RUN, 1, 1, CHECKPOINT_CHUNK_BYTES * 2);
        let digest = sha256_hex(&body);
        let mut upload_state = CheckpointUpload::default();
        let first = upload_state.accept_chunk(
            &store,
            SESSION,
            &chunk_request(
                body[..CHECKPOINT_CHUNK_BYTES].to_vec(),
                1,
                &digest,
                0,
                body.len() as u64,
            ),
        );
        assert!(first.accepted && !first.complete, "{:?}", first.error);

        let mut changed = chunk_request(
            body[CHECKPOINT_CHUNK_BYTES..].to_vec(),
            1,
            &digest,
            CHECKPOINT_CHUNK_BYTES as u64,
            body.len() as u64,
        );
        changed.digest.as_mut().expect("digest").hex_hash = "ab".repeat(32);
        let rejected = upload_state.accept_chunk(&store, SESSION, &changed);
        assert!(!rejected.accepted);
        assert!(rejected
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("digest changed")));
        assert_nothing_stored(&store);
    }

    #[test]
    fn run_change_mid_upload_stores_nothing() {
        let store = store_with_run();
        let body = checkpoint_body_with_len(RUN, 1, 1, CHECKPOINT_CHUNK_BYTES * 2);
        let digest = sha256_hex(&body);
        let mut upload_state = CheckpointUpload::default();
        let first = upload_state.accept_chunk(
            &store,
            SESSION,
            &chunk_request(
                body[..CHECKPOINT_CHUNK_BYTES].to_vec(),
                1,
                &digest,
                0,
                body.len() as u64,
            ),
        );
        assert!(first.accepted && !first.complete, "{:?}", first.error);

        let mut changed = chunk_request(
            body[CHECKPOINT_CHUNK_BYTES..].to_vec(),
            1,
            &digest,
            CHECKPOINT_CHUNK_BYTES as u64,
            body.len() as u64,
        );
        changed.lease.as_mut().expect("lease").run_id = Some(ResourceId {
            urn: "urn:vam:run:local.vam.dev/acme/other".to_string(),
        });
        let rejected = upload_state.accept_chunk(&store, SESSION, &changed);
        assert!(!rejected.accepted);
        assert_eq!(upload_state.staged_len(), 0);
        assert_nothing_stored(&store);
    }

    #[test]
    fn final_digest_mismatch_stores_nothing() {
        let store = store_with_run();
        let body = checkpoint_body_with_len(RUN, 1, 1, CHECKPOINT_CHUNK_BYTES * 2);
        let wrong = "cd".repeat(32);
        let mut upload_state = CheckpointUpload::default();
        let first = upload_state.accept_chunk(
            &store,
            SESSION,
            &chunk_request(
                body[..CHECKPOINT_CHUNK_BYTES].to_vec(),
                1,
                &wrong,
                0,
                body.len() as u64,
            ),
        );
        assert!(first.accepted && !first.complete, "{:?}", first.error);
        assert_nothing_stored(&store);

        let rejected = upload_state.accept_chunk(
            &store,
            SESSION,
            &chunk_request(
                body[CHECKPOINT_CHUNK_BYTES..].to_vec(),
                1,
                &wrong,
                CHECKPOINT_CHUNK_BYTES as u64,
                body.len() as u64,
            ),
        );
        assert!(!rejected.accepted);
        assert!(rejected
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("digest mismatch")));
        assert_eq!(upload_state.staged_len(), 0);
        assert_nothing_stored(&store);
    }

    #[test]
    fn fetch_rejects_foreign_session_wrong_digest_and_missing_checkpoint() {
        let store = store_with_run();
        let missing = fetch_run_checkpoint_chunk(
            &store,
            SESSION,
            &FetchRunCheckpointRequest {
                lease: Some(lease(1)),
                digest: Some(proto_digest(&"ab".repeat(32))),
                offset: 0,
            },
        );
        assert_fetch_has_no_bytes(&missing);
        assert!(missing
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("no checkpoint")));

        let body = checkpoint_body(RUN, 1, "stored");
        let digest = sha256_hex(&body);
        store
            .put_run_checkpoint(RUN, 1, &digest, &body)
            .expect("store");

        let foreign = fetch_run_checkpoint_chunk(
            &store,
            "sess-other",
            &FetchRunCheckpointRequest {
                lease: Some(lease(1)),
                digest: Some(proto_digest(&digest)),
                offset: 0,
            },
        );
        assert_fetch_has_no_bytes(&foreign);

        let wrong = fetch_run_checkpoint_chunk(
            &store,
            SESSION,
            &FetchRunCheckpointRequest {
                lease: Some(lease(1)),
                digest: Some(proto_digest(&"ab".repeat(32))),
                offset: 0,
            },
        );
        assert_fetch_has_no_bytes(&wrong);
        assert!(wrong
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("digest mismatch")));
    }

    fn assert_fetch_has_no_bytes(response: &FetchRunCheckpointResponse) {
        assert!(response.error.is_some(), "expected an error: {response:?}");
        assert!(response.chunk.is_empty());
        assert_eq!(response.total_size, 0);
        assert!(!response.last);
    }

    #[test]
    fn legacy_single_message_upload_stores() {
        let store = store_with_run();
        let body = checkpoint_body_with_len(RUN, 1, 1, 40 * 1024);
        assert!(body.len() > CHECKPOINT_CHUNK_BYTES);
        let digest = sha256_hex(&body);
        let mut upload_state = CheckpointUpload::default();
        let response = upload_state.accept_chunk(
            &store,
            SESSION,
            &chunk_request(body.clone(), 1, &digest, 0, 0),
        );
        assert!(response.accepted, "{:?}", response.error);
        assert!(response.complete);
        assert_eq!(response.received, body.len() as u64);
        assert_eq!(response.digest, digest);
        let stored = store
            .latest_run_checkpoint(RUN)
            .expect("load")
            .expect("row");
        assert_eq!(stored.bytes, body);
        assert_eq!(stored_checkpoint_count(&store), 1);
    }

    #[test]
    fn schema_versions_one_and_two_store_and_four_is_rejected() {
        let store = store_with_run();
        for (version, generation) in [(1u32, 1i64), (2, 2)] {
            let body = format!(
                r#"{{"schema_version":{version},"run_id":"{RUN}","lifecycle_generation":{generation},"marker":"schema"}}"#
            )
            .into_bytes();
            let digest = sha256_hex(&body);
            let response = CheckpointUpload::default().accept_chunk(
                &store,
                SESSION,
                &chunk_request(body, generation, &digest, 0, 0),
            );
            assert!(
                response.accepted && response.complete,
                "schema {version}: {:?}",
                response.error
            );
        }

        let rejected_body = format!(
            r#"{{"schema_version":4,"run_id":"{RUN}","lifecycle_generation":3,"marker":"schema"}}"#
        )
        .into_bytes();
        let rejected_digest = sha256_hex(&rejected_body);
        let rejected = CheckpointUpload::default().accept_chunk(
            &store,
            SESSION,
            &chunk_request(rejected_body, 3, &rejected_digest, 0, 0),
        );
        assert!(!rejected.accepted);
        assert!(rejected
            .error
            .as_ref()
            .is_some_and(|err| err.message.contains("schema_version")));
        let latest = store
            .latest_run_checkpoint(RUN)
            .expect("load")
            .expect("schema 2 remains");
        assert_eq!(latest.lifecycle_generation, 2);
        assert_eq!(stored_checkpoint_count(&store), 2);
    }

    #[test]
    fn full_chunk_envelopes_fit_under_max_payload_size() {
        let chunk = vec![0xab; CHECKPOINT_CHUNK_BYTES];
        let request = UploadRunCheckpointRequest {
            lease: Some(lease(1)),
            checkpoint: chunk.clone(),
            digest: Some(proto_digest(&"ab".repeat(32))),
            offset: 0,
            total_size: CHECKPOINT_CHUNK_BYTES as u64,
        };
        let request_len = envelope_len("UploadRunCheckpoint", &request);
        let response = FetchRunCheckpointResponse {
            chunk,
            total_size: CHECKPOINT_CHUNK_BYTES as u64,
            last: false,
            error: None,
        };
        let response_len = envelope_len("FetchRunCheckpointResponse", &response);
        assert!(request_len > CHECKPOINT_CHUNK_BYTES);
        assert!(response_len > CHECKPOINT_CHUNK_BYTES);
        assert!(
            request_len < MAX_PAYLOAD_SIZE,
            "upload envelope is {request_len} bytes"
        );
        assert!(
            response_len < MAX_PAYLOAD_SIZE,
            "fetch envelope is {response_len} bytes"
        );
    }

    fn suspend_tail_bytes() -> Vec<u8> {
        serde_json::to_vec(&vam_spec::thread_context::SuspendTail {
            schema_version: 3,
            run_id: RUN.to_string(),
            lifecycle_generation: 1,
            kind: "suspend".into(),
            messages: vec![],
            pending: vam_spec::thread_context::PendingCall {
                tool_call_id: "c1".into(),
                tool_name: "write_file".into(),
                args_json: "{}".into(),
            },
            state: Default::default(),
        })
        .unwrap()
    }

    #[test]
    fn a_suspend_tail_is_never_stored_as_a_run_checkpoint_and_the_sync_path_refuses_it() {
        let store = store_with_run();
        let body = suspend_tail_bytes();
        let digest = sha256_hex(&body);
        let response = CheckpointUpload::default().accept_chunk(
            &store,
            SESSION,
            &chunk_request(body, 1, &digest, 0, 0),
        );
        assert!(!response.accepted);
        assert!(response
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("session broker")));
        assert_nothing_stored(&store);
    }

    #[tokio::test]
    async fn a_suspend_tail_needs_the_sessions_recorder_and_a_well_formed_body() {
        let store = store_with_run();
        // No recorder registered for the session.
        let body = suspend_tail_bytes();
        let digest = sha256_hex(&body);
        let response = CheckpointUpload::default()
            .accept_chunk_with_log(&store, SESSION, &chunk_request(body, 1, &digest, 0, 0))
            .await;
        assert!(!response.accepted);
        assert!(response
            .error
            .as_ref()
            .is_some_and(|e| e.message.contains("recorder")));
        // The old per-round shape is not a suspend tail.
        let old = format!(
            r#"{{"schema_version":3,"run_id":"{RUN}","lifecycle_generation":1,"thread_ref":"main","parent_digest":"","seq":1,"kind":"round","messages":[]}}"#
        )
        .into_bytes();
        let digest = sha256_hex(&old);
        let response = CheckpointUpload::default()
            .accept_chunk_with_log(&store, SESSION, &chunk_request(old, 1, &digest, 0, 0))
            .await;
        assert!(!response.accepted);
        assert_nothing_stored(&store);
    }

    #[test]
    fn the_wave_one_tables_are_dropped_on_open_and_run_checkpoints_are_kept() {
        let dir = tempfile::tempdir().expect("dir");
        let path = dir.path().join("vamd.sqlite");
        {
            let store = SqliteWorkflowStore::open(&path).expect("open");
            store
                .persist_run_spec(
                    RUN,
                    r#"{"inputs":{}}"#,
                    Some(SESSION),
                    "urn:vam:agent:test",
                    None,
                    "User::\"t\"",
                )
                .expect("run");
            let body = checkpoint_body(RUN, 1, "legacy");
            let digest = sha256_hex(&body);
            store
                .put_run_checkpoint(RUN, 1, &digest, &body)
                .expect("legacy");
        }
        // A database from Wave 1 has the conversation tables.
        {
            let connection = rusqlite::Connection::open(&path).expect("raw");
            connection
                .execute_batch(
                    "CREATE TABLE checkpoint_blobs (digest TEXT PRIMARY KEY);
                     CREATE TABLE thread_checkpoints (seq INTEGER);",
                )
                .expect("create");
        }
        for _ in 0..2 {
            let store = SqliteWorkflowStore::open(&path).expect("reopen");
            let legacy = store
                .latest_run_checkpoint(RUN)
                .expect("read")
                .expect("row");
            assert_eq!(legacy.lifecycle_generation, 1);
            let connection = store.connection.lock().expect("lock");
            for (table, want) in [
                ("checkpoint_blobs", 0),
                ("thread_checkpoints", 0),
                ("turn_commits", 1),
                ("run_checkpoint_grants", 1),
                ("run_checkpoints", 1),
            ] {
                let found: i64 = connection
                    .query_row(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                        [table],
                        |r| r.get(0),
                    )
                    .expect("master");
                assert_eq!(found, want, "{table}");
            }
        }
    }

    #[test]
    fn a_granted_thread_context_is_served_in_chunks_to_that_run_and_session_only() {
        use crate::thread_context::{write_context, BuiltContext};
        let store = store_with_run();
        let root = tempfile::tempdir().expect("root");
        let layout = vam_storage::Drive9Layout::new(root.path());
        let bytes = vec![b'c'; CHECKPOINT_CHUNK_BYTES * 2 + 10];
        let digest = sha256_hex(&bytes);
        let built = BuiltContext {
            context: vam_spec::thread_context::ThreadContext {
                format: String::new(),
                version: 1,
                run_id: RUN.into(),
                head_seq: 0,
                head_digest: String::new(),
                system_sha256: None,
                tools_sha256: None,
                messages: vec![],
                blocks: vec![],
                suspend: None,
                crash: None,
            },
            bytes: bytes.clone(),
            digest: digest.clone(),
            runs: vec![],
            warnings: vec![],
        };
        write_context(&layout, SESSION, &built).expect("write");

        let fetch = |session: &str, digest: &str, offset: u64| {
            fetch_run_checkpoint_chunk_with(
                &store,
                Some(&layout),
                session,
                &FetchRunCheckpointRequest {
                    lease: Some(lease(1)),
                    digest: Some(proto_digest(digest)),
                    offset,
                },
            )
        };
        // Not granted yet: the same answer as a digest that does not exist.
        assert!(fetch(SESSION, &digest, 0).error.is_some());
        store.grant_run_context(RUN, &digest).expect("grant");

        let mut got = Vec::new();
        loop {
            let reply = fetch(SESSION, &digest, got.len() as u64);
            assert!(reply.error.is_none(), "{:?}", reply.error);
            assert_eq!(reply.total_size as usize, bytes.len());
            assert!(reply.chunk.len() <= CHECKPOINT_CHUNK_BYTES);
            got.extend(reply.chunk);
            if reply.last {
                break;
            }
        }
        assert_eq!(got, bytes);
        // A session the file was not written for, a made-up digest and a
        // path-shaped digest are all refused.
        assert!(fetch("sess-other", &digest, 0).error.is_some());
        assert!(fetch(SESSION, &"f".repeat(64), 0).error.is_some());
        assert!(fetch(SESSION, "../../etc/passwd", 0).error.is_some());
        // Without a layout (the legacy entry point) nothing is served.
        let legacy = fetch_run_checkpoint_chunk(
            &store,
            SESSION,
            &FetchRunCheckpointRequest {
                lease: Some(lease(1)),
                digest: Some(proto_digest(&digest)),
                offset: 0,
            },
        );
        assert!(legacy.error.is_some());
        // Revoking removes the grant.
        assert_eq!(store.revoke_run_context_grants(RUN), vec![digest.clone()]);
        assert!(fetch(SESSION, &digest, 0).error.is_some());
    }
}
