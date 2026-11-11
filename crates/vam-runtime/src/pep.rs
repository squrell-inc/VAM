use std::time::Duration;

use crate::RuntimeError;
use bytes::Bytes;
use prost::Message;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use vam_spec::v1::{
    Digest as SpecDigest, ExecutionLease, FetchRunCheckpointRequest, FetchRunCheckpointResponse,
    ToolIntentRequest, ToolIntentResponse, UploadRunCheckpointRequest, UploadRunCheckpointResponse,
    VsockEnvelope,
};
use vam_vsock::frame::{Frame, FrameType};
use vam_vsock::guest_dial;

/// Largest checkpoint the host stores. Mirrors
/// `vam_orchestrator::run_checkpoint::MAX_RUN_CHECKPOINT_BYTES`.
pub const MAX_CHECKPOINT_BYTES: usize = 4 * 1024 * 1024;

/// Checkpoint bytes in one upload or fetch message. Mirrors
/// `vam_orchestrator::run_checkpoint::CHECKPOINT_CHUNK_BYTES`.
pub const CHECKPOINT_CHUNK_BYTES: usize = 32 * 1024;

/// How long the guest waits for one port-52 reply that has no tool budget of
/// its own: `RequestToolAuthorization`, `ListAdvertisedTools`, and each
/// `UploadRunCheckpoint` or `FetchRunCheckpoint` chunk.
///
/// `ExecuteHostTool` waits the tool's own timeout plus
/// [`PORT52_HOST_TOOL_MARGIN`]. Host MCP calls may run as long as
/// `vam_orchestrator::mcp_registry::MAX_MCP_TIMEOUT_MS` (600_000 ms, H1-3);
/// [`MAX_HOST_TOOL_TIMEOUT`] is that ceiling, so a guest wait is never shorter
/// than the host call.
pub const PORT52_ROUND_TRIP_TIMEOUT: Duration = Duration::from_secs(30);

/// Spare time after a host tool's own timeout before `ExecuteHostTool` fails.
pub const PORT52_HOST_TOOL_MARGIN: Duration = Duration::from_secs(30);

/// Longest host-tool timeout the guest will wait out. Matches
/// `vam_orchestrator::mcp_registry::MAX_MCP_TIMEOUT_MS` (600_000).
pub const MAX_HOST_TOOL_TIMEOUT: Duration = Duration::from_millis(600_000);

pub async fn request_tool_authorization(
    lease: &ExecutionLease,
    tool_name: &str,
    arguments: Option<&prost_types::Struct>,
    script_digest: Option<&str>,
    call_id: &str,
) -> Result<ToolIntentResponse, RuntimeError> {
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    request_tool_authorization_on(
        &mut stream,
        lease,
        tool_name,
        arguments,
        script_digest,
        call_id,
    )
    .await
}

pub(crate) async fn request_tool_authorization_on<S>(
    stream: &mut S,
    lease: &ExecutionLease,
    tool_name: &str,
    arguments: Option<&prost_types::Struct>,
    script_digest: Option<&str>,
    call_id: &str,
) -> Result<ToolIntentResponse, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let args_digest = vam_spec::canonical_args_digest(arguments)
        .map_err(|e| RuntimeError::Handler(format!("arguments digest: {e}")))?;

    let script_digest_spec = script_digest.map(|d| SpecDigest {
        algorithm: 1, // ALGORITHM_SHA256
        hex_hash: d.to_string(),
    });

    let req = ToolIntentRequest {
        lease: Some(lease.clone()),
        tool_name: tool_name.to_string(),
        arguments: arguments.cloned(),
        arguments_digest: Some(SpecDigest {
            algorithm: 1, // ALGORITHM_SHA256
            hex_hash: args_digest,
        }),
        script_digest: script_digest_spec,
        call_id: call_id.to_string(),
    };

    let mut req_payload = Vec::new();
    req.encode(&mut req_payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;

    let env_in = round_trip(
        stream,
        "RequestToolAuthorization",
        101,
        req_payload,
        round_trip_timeout(),
    )
    .await?;
    if let Some(err) = env_in.error {
        return Ok(ToolIntentResponse {
            disposition: vam_spec::v1::tool_intent_response::Disposition::Denied as i32,
            approval_request_id: None,
            error: Some(err),
            capability_token: None,
            resource_envelope: None,
            replayed: None,
        });
    }

    let start = std::time::Instant::now();
    let resp = ToolIntentResponse::decode(&*env_in.payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    let mut attrs = std::collections::HashMap::new();
    attrs.insert("tool.name".to_string(), tool_name.to_string());
    attrs.insert("pep.disposition".to_string(), resp.disposition.to_string());
    crate::telemetry::global_telemetry().record_span("pep.authorize", start.elapsed(), None, attrs);
    Ok(resp)
}

pub async fn list_advertised_tools() -> Result<Vec<String>, RuntimeError> {
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    list_advertised_tools_on(&mut stream).await
}

pub(crate) async fn list_advertised_tools_on<S>(stream: &mut S) -> Result<Vec<String>, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use vam_spec::v1::{ListAdvertisedToolsRequest, ListAdvertisedToolsResponse};

    let req = ListAdvertisedToolsRequest { lease: None };
    let mut req_payload = Vec::new();
    req.encode(&mut req_payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;

    let env_in = round_trip(
        stream,
        "ListAdvertisedTools",
        102,
        req_payload,
        round_trip_timeout(),
    )
    .await?;
    if let Some(err) = env_in.error {
        return Err(RuntimeError::Handler(format!(
            "ListAdvertisedTools failed: {}",
            err.message
        )));
    }

    let resp = ListAdvertisedToolsResponse::decode(&*env_in.payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    Ok(resp.tool_names)
}

/// What the host answered to a `CommitTurn`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitTurnOutcome {
    /// Recorded; it completes when the host sees the round's tool results.
    Accepted,
    /// The host has no turn committer for this session (code 12). Not an
    /// error: the guest stops asking.
    Unavailable,
    /// The host tried and its storage failed, or it would not take the commit.
    Refused(String),
}

/// How long the guest waits for a `CommitTurn` reply: the host builds the
/// snapshot while the guest waits.
pub const COMMIT_TURN_TIMEOUT: Duration = Duration::from_secs(120);

/// Ask the host to record a turn commit for `round`. `tail_digest_hex` is
/// [`vam_spec::thread_context::tail_digest`] of the round's tool results.
pub async fn commit_turn(
    lease: &ExecutionLease,
    round: u32,
    tail_digest_hex: &str,
    wrote_files: bool,
) -> Result<CommitTurnOutcome, RuntimeError> {
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    commit_turn_on(&mut stream, lease, round, tail_digest_hex, wrote_files).await
}

pub(crate) async fn commit_turn_on<S>(
    stream: &mut S,
    lease: &ExecutionLease,
    round: u32,
    tail_digest_hex: &str,
    wrote_files: bool,
) -> Result<CommitTurnOutcome, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use vam_spec::v1::{CommitTurnRequest, CommitTurnResponse};

    let request = CommitTurnRequest {
        lease: Some(lease.clone()),
        checkpoint_digest: Some(SpecDigest {
            algorithm: 1,
            hex_hash: tail_digest_hex.to_string(),
        }),
        round,
        wrote_files,
    };
    let mut payload = Vec::new();
    request
        .encode(&mut payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    let envelope = round_trip(stream, "CommitTurn", 105, payload, COMMIT_TURN_TIMEOUT).await?;
    if let Some(err) = envelope.error {
        return Err(RuntimeError::Handler(format!(
            "host CommitTurn failed: {}",
            err.message
        )));
    }
    let response = CommitTurnResponse::decode(&*envelope.payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    if response.accepted {
        return Ok(CommitTurnOutcome::Accepted);
    }
    Ok(match response.error {
        Some(err) if err.code == 12 => CommitTurnOutcome::Unavailable,
        Some(err) => CommitTurnOutcome::Refused(err.message),
        None => CommitTurnOutcome::Refused("the host did not accept the turn commit".into()),
    })
}

/// What the host answered to `PullInbox`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PullInboxOutcome {
    /// Pending steers after the asked seq, in seq order. May be empty.
    Messages(Vec<(i64, String)>),
    /// The host does not have an inbox (an older vamd): the guest stops asking.
    Unavailable,
}

/// How long the guest waits for one `PullInbox` reply.
pub const PULL_INBOX_TIMEOUT: Duration = Duration::from_secs(30);

/// Ask the host for the steers after `after_seq`. `ack_seq` is the highest seq the
/// guest has applied to its conversation: the host acknowledges through it, and
/// never through `after_seq`.
pub async fn pull_inbox(
    lease: &ExecutionLease,
    after_seq: i64,
    ack_seq: i64,
) -> Result<PullInboxOutcome, RuntimeError> {
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    pull_inbox_on(&mut stream, lease, after_seq, ack_seq).await
}

pub(crate) async fn pull_inbox_on<S>(
    stream: &mut S,
    lease: &ExecutionLease,
    after_seq: i64,
    ack_seq: i64,
) -> Result<PullInboxOutcome, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use vam_spec::v1::{PullInboxRequest, PullInboxResponse};

    let request = PullInboxRequest {
        lease: Some(lease.clone()),
        after_seq,
        ack_seq,
    };
    let mut payload = Vec::new();
    request
        .encode(&mut payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    let envelope = round_trip(stream, "PullInbox", 106, payload, PULL_INBOX_TIMEOUT).await?;
    if let Some(err) = envelope.error {
        if err.code == 12 {
            return Ok(PullInboxOutcome::Unavailable);
        }
        return Err(RuntimeError::Handler(format!(
            "host PullInbox failed: {}",
            err.message
        )));
    }
    let response = PullInboxResponse::decode(&*envelope.payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    if let Some(err) = response.error {
        if err.code == 12 {
            return Ok(PullInboxOutcome::Unavailable);
        }
        return Err(RuntimeError::Handler(format!(
            "host PullInbox refused: {}",
            err.message
        )));
    }
    Ok(PullInboxOutcome::Messages(
        response
            .messages
            .into_iter()
            .map(|m| (m.seq, m.text))
            .collect(),
    ))
}

/// What the host answered to `PostOutbox`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PostOutboxOutcome {
    /// The host recorded the post.
    Posted,
    /// The host refused it; the text is the typed tool result for the model.
    Refused(String),
    /// The host has no outbox (an older vamd).
    Unavailable,
}

/// How long the guest waits for one `PostOutbox` reply.
pub const POST_OUTBOX_TIMEOUT: Duration = Duration::from_secs(30);

/// Post a message (`kind` message, `text`) or a card (`kind` card, `jig`,
/// `data_json`). `token` is the capability token the intent for this exact call
/// returned; the host refuses a post without it.
pub async fn post_outbox(
    lease: &ExecutionLease,
    request: vam_spec::v1::PostOutboxRequest,
) -> Result<PostOutboxOutcome, RuntimeError> {
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    let mut request = request;
    request.lease = Some(lease.clone());
    post_outbox_on(&mut stream, request).await
}

pub(crate) async fn post_outbox_on<S>(
    stream: &mut S,
    request: vam_spec::v1::PostOutboxRequest,
) -> Result<PostOutboxOutcome, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use vam_spec::v1::PostOutboxResponse;

    let mut payload = Vec::new();
    request
        .encode(&mut payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    let envelope = round_trip(stream, "PostOutbox", 107, payload, POST_OUTBOX_TIMEOUT).await?;
    let error = match envelope.error {
        Some(err) => Some(err),
        None => {
            PostOutboxResponse::decode(&*envelope.payload)
                .map_err(|e| RuntimeError::Serialization(e.to_string()))?
                .error
        }
    };
    Ok(match error {
        None => PostOutboxOutcome::Posted,
        Some(err) if err.code == 12 => PostOutboxOutcome::Unavailable,
        Some(err) => PostOutboxOutcome::Refused(err.message),
    })
}

/// Upload the checkpoint for this lease's run and generation.
///
/// The bytes go up in chunks of at most [`CHECKPOINT_CHUNK_BYTES`] on one
/// port-52 connection. `digest_hex` is the SHA-256 of the whole checkpoint; the
/// host stores the assembled bytes and checks the digest itself. Resume reads
/// that copy, not the workspace file. Each chunk must be answered within
/// [`PORT52_ROUND_TRIP_TIMEOUT`]. A timeout is an error: the caller fails the
/// task and must not park, because the host has no stored checkpoint.
pub async fn upload_run_checkpoint(
    lease: &ExecutionLease,
    checkpoint: &[u8],
    digest_hex: &str,
) -> Result<(), RuntimeError> {
    check_checkpoint_len(checkpoint.len())?;
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    upload_checkpoint_chunks(&mut stream, lease, checkpoint, digest_hex).await
}

/// Send `checkpoint` in order over an open port-52 stream, waiting for each ack.
///
/// Each ack must advance `received` to the end of its chunk, and only the last
/// ack may say `complete`. Any refusal stops the upload with the host's message.
pub async fn upload_checkpoint_chunks<S>(
    stream: &mut S,
    lease: &ExecutionLease,
    checkpoint: &[u8],
    digest_hex: &str,
) -> Result<(), RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    check_checkpoint_len(checkpoint.len())?;
    let total = checkpoint.len();
    let mut offset = 0usize;
    let mut correlation_id = 104u32;
    while offset < total {
        let end = (offset + CHECKPOINT_CHUNK_BYTES).min(total);
        let request = UploadRunCheckpointRequest {
            lease: Some(lease.clone()),
            checkpoint: checkpoint[offset..end].to_vec(),
            digest: Some(SpecDigest {
                algorithm: 1, // ALGORITHM_SHA256
                hex_hash: digest_hex.to_string(),
            }),
            offset: offset as u64,
            total_size: total as u64,
        };
        let ack: UploadRunCheckpointResponse =
            exchange(stream, "UploadRunCheckpoint", correlation_id, &request).await?;
        correlation_id = correlation_id.wrapping_add(1);

        if let Some(err) = ack.error {
            return Err(RuntimeError::Handler(format!(
                "host rejected checkpoint upload: {}",
                err.message
            )));
        }
        if !ack.accepted {
            return Err(RuntimeError::Handler(
                "host rejected checkpoint upload".to_string(),
            ));
        }
        if ack.received != end as u64 {
            return Err(RuntimeError::Handler(format!(
                "host acknowledged {} checkpoint bytes; expected {end}",
                ack.received
            )));
        }
        let last = end == total;
        if ack.complete != last {
            return Err(RuntimeError::Handler(format!(
                "host reported complete={} at byte {end} of {total}",
                ack.complete
            )));
        }
        if last && ack.digest != digest_hex {
            return Err(RuntimeError::Handler(
                "host stored a different checkpoint digest".to_string(),
            ));
        }
        offset = end;
    }
    Ok(())
}

/// Fetch the stored checkpoint with digest `digest_hex` over one port-52 connection.
///
/// Replies must be contiguous and each carry at most [`CHECKPOINT_CHUNK_BYTES`].
/// `total_size` must not change and must stay within [`MAX_CHECKPOINT_BYTES`].
/// The assembled bytes must match the digest. Each chunk must be answered within
/// [`PORT52_ROUND_TRIP_TIMEOUT`]. A timeout fails the fetch before any resume
/// runs the approved tool, the same way a digest mismatch does.
pub async fn fetch_run_checkpoint(
    lease: &ExecutionLease,
    digest_hex: &str,
) -> Result<Vec<u8>, RuntimeError> {
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    fetch_checkpoint_chunks(&mut stream, lease, digest_hex).await
}

/// Fetch the assembled thread context the host granted to this run. Same wire
/// as a checkpoint fetch, with the larger context bound.
pub async fn fetch_thread_context(
    lease: &ExecutionLease,
    digest_hex: &str,
) -> Result<Vec<u8>, RuntimeError> {
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    fetch_chunks_bounded(
        &mut stream,
        lease,
        digest_hex,
        vam_spec::thread_context::MAX_THREAD_CONTEXT_BYTES,
    )
    .await
}

/// Read the checkpoint with digest `digest_hex` in order over an open port-52 stream.
pub async fn fetch_checkpoint_chunks<S>(
    stream: &mut S,
    lease: &ExecutionLease,
    digest_hex: &str,
) -> Result<Vec<u8>, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    fetch_chunks_bounded(stream, lease, digest_hex, MAX_CHECKPOINT_BYTES).await
}

/// [`fetch_checkpoint_chunks`] with an explicit size bound.
pub async fn fetch_chunks_bounded<S>(
    stream: &mut S,
    lease: &ExecutionLease,
    digest_hex: &str,
    max_bytes: usize,
) -> Result<Vec<u8>, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let mut bytes: Vec<u8> = Vec::new();
    let mut declared_size: Option<u64> = None;
    let mut correlation_id = 201u32;
    loop {
        let request = FetchRunCheckpointRequest {
            lease: Some(lease.clone()),
            digest: Some(SpecDigest {
                algorithm: 1, // ALGORITHM_SHA256
                hex_hash: digest_hex.to_string(),
            }),
            offset: bytes.len() as u64,
        };
        let reply: FetchRunCheckpointResponse =
            exchange(stream, "FetchRunCheckpoint", correlation_id, &request).await?;
        correlation_id = correlation_id.wrapping_add(1);

        if let Some(err) = reply.error {
            return Err(RuntimeError::Handler(format!(
                "host could not return checkpoint {digest_hex}: {}",
                err.message
            )));
        }
        if reply.total_size == 0 || reply.total_size > max_bytes as u64 {
            return Err(RuntimeError::Handler(format!(
                "checkpoint {digest_hex} declares {} bytes; allowed is 1 to {max_bytes}",
                reply.total_size
            )));
        }
        let size = *declared_size.get_or_insert(reply.total_size);
        if size != reply.total_size {
            return Err(RuntimeError::Handler(format!(
                "checkpoint size changed during fetch: {size} then {}",
                reply.total_size
            )));
        }
        let chunk_len = reply.chunk.len();
        if chunk_len == 0 || chunk_len > CHECKPOINT_CHUNK_BYTES {
            return Err(RuntimeError::Handler(format!(
                "checkpoint chunk is {chunk_len} bytes; expected 1 to {CHECKPOINT_CHUNK_BYTES}"
            )));
        }
        let end = bytes.len() + chunk_len;
        if end as u64 > size {
            return Err(RuntimeError::Handler(format!(
                "checkpoint chunk runs to byte {end}, past the declared {size}"
            )));
        }
        if reply.last != (end as u64 == size) {
            return Err(RuntimeError::Handler(format!(
                "checkpoint chunk is not contiguous: reply ends at byte {end} of {size}, last={}",
                reply.last
            )));
        }
        if !reply.last && chunk_len != CHECKPOINT_CHUNK_BYTES {
            return Err(RuntimeError::Handler(format!(
                "checkpoint chunk at byte {} is {chunk_len} bytes, not a full chunk",
                bytes.len()
            )));
        }
        bytes.extend_from_slice(&reply.chunk);
        if reply.last {
            break;
        }
    }
    if sha256_hex(&bytes) != digest_hex {
        return Err(RuntimeError::Handler(format!(
            "checkpoint {digest_hex} failed digest verification after fetch"
        )));
    }
    Ok(bytes)
}

fn check_checkpoint_len(len: usize) -> Result<(), RuntimeError> {
    if len == 0 {
        return Err(RuntimeError::Handler("checkpoint is empty".to_string()));
    }
    if len > MAX_CHECKPOINT_BYTES {
        return Err(RuntimeError::Handler(format!(
            "checkpoint is {len} bytes; max is {MAX_CHECKPOINT_BYTES}"
        )));
    }
    Ok(())
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// One request and its reply on an open port-52 stream.
///
/// The wait is [`PORT52_ROUND_TRIP_TIMEOUT`]. Checkpoint upload and fetch use
/// this for every chunk.
async fn exchange<S, Req, Resp>(
    stream: &mut S,
    method: &str,
    correlation_id: u32,
    request: &Req,
) -> Result<Resp, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
    Req: Message,
    Resp: Message + Default,
{
    let mut payload = Vec::new();
    request
        .encode(&mut payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    let reply_envelope = round_trip(
        stream,
        method,
        correlation_id,
        payload,
        round_trip_timeout(),
    )
    .await?;
    if let Some(err) = reply_envelope.error {
        return Err(RuntimeError::Handler(format!(
            "host {method} failed: {}",
            err.message
        )));
    }
    Resp::decode(&*reply_envelope.payload).map_err(|e| RuntimeError::Serialization(e.to_string()))
}

/// Write one port-52 request and read its reply, giving up after `timeout`.
///
/// A timeout names `method`. The connection is whatever the caller passed; this
/// does not dial.
async fn round_trip<S>(
    stream: &mut S,
    method: &str,
    correlation_id: u32,
    payload: Vec<u8>,
    timeout: Duration,
) -> Result<VsockEnvelope, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    let outcome = tokio::time::timeout(timeout, async {
        let envelope = VsockEnvelope {
            method: method.to_string(),
            correlation_id,
            payload,
            error: None,
            trace_context: None,
        };
        let mut envelope_bytes = Vec::new();
        envelope
            .encode(&mut envelope_bytes)
            .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
        let frame = Frame {
            frame_type: FrameType::ControlGrpc,
            correlation_id,
            payload: Bytes::from(envelope_bytes),
        };
        frame
            .write_to(stream)
            .await
            .map_err(|e| RuntimeError::Handler(e.to_string()))?;

        let reply = Frame::read_from(stream)
            .await
            .map_err(|e| RuntimeError::Handler(e.to_string()))?;
        if reply.frame_type != FrameType::ControlGrpc {
            return Err(RuntimeError::Handler(format!(
                "Unexpected frame type from host for {method}"
            )));
        }
        let reply_envelope = VsockEnvelope::decode(&*reply.payload)
            .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
        if reply_envelope.correlation_id != correlation_id {
            return Err(RuntimeError::Handler(format!(
                "host replied to {method} with correlation id {}",
                reply_envelope.correlation_id
            )));
        }
        Ok(reply_envelope)
    })
    .await;

    match outcome {
        Ok(result) => result,
        Err(_elapsed) => Err(RuntimeError::Handler(format!(
            "{method} timed out after {}s",
            timeout.as_secs()
        ))),
    }
}

/// Guest budget for one `ExecuteHostTool` round trip.
///
/// The tool's own timeout is `max_wall_time_ms` on the capability token when
/// that is longer than [`MAX_HOST_TOOL_TIMEOUT`]. Otherwise the budget is the
/// MCP ceiling: host MCP calls ignore the capability tier and use the
/// connection timeout, which can be as long as
/// `vam_orchestrator::mcp_registry::MAX_MCP_TIMEOUT_MS`. The guest then adds
/// [`PORT52_HOST_TOOL_MARGIN`] so the call is not cut off while the host is
/// still inside that limit.
fn execute_host_tool_timeout(token: &vam_spec::v1::SignedEnvelope) -> Duration {
    let ceiling = max_host_tool_timeout();
    let from_token = vam_security::parse_capability_claims(token)
        .ok()
        .and_then(|claims| claims.resource_envelope)
        .map(|envelope| Duration::from_millis(envelope.max_wall_time_ms))
        .filter(|timeout| !timeout.is_zero());
    let tool_timeout = from_token.unwrap_or(ceiling).max(ceiling);
    tool_timeout.saturating_add(host_tool_margin())
}

fn round_trip_timeout() -> Duration {
    test_round_trip_timeout().unwrap_or(PORT52_ROUND_TRIP_TIMEOUT)
}

fn host_tool_margin() -> Duration {
    test_host_tool_margin().unwrap_or(PORT52_HOST_TOOL_MARGIN)
}

fn max_host_tool_timeout() -> Duration {
    test_max_host_tool_timeout().unwrap_or(MAX_HOST_TOOL_TIMEOUT)
}

#[cfg(not(test))]
fn test_round_trip_timeout() -> Option<Duration> {
    None
}

#[cfg(not(test))]
fn test_host_tool_margin() -> Option<Duration> {
    None
}

#[cfg(not(test))]
fn test_max_host_tool_timeout() -> Option<Duration> {
    None
}

/// Short budgets for timeout tests. Production waits use the constants above.
#[cfg(test)]
mod test_budget {
    use super::Duration;
    use std::cell::Cell;

    #[derive(Clone, Copy)]
    pub(super) struct Budget {
        pub round_trip: Duration,
        pub margin: Duration,
        pub max_host_tool: Duration,
    }

    thread_local! {
        static BUDGET: Cell<Option<Budget>> = const { Cell::new(None) };
    }

    pub(super) struct Guard;

    impl Drop for Guard {
        fn drop(&mut self) {
            BUDGET.with(|slot| slot.set(None));
        }
    }

    pub(super) fn install(budget: Budget) -> Guard {
        BUDGET.with(|slot| slot.set(Some(budget)));
        Guard
    }

    pub(super) fn current() -> Option<Budget> {
        BUDGET.with(|slot| slot.get())
    }
}

#[cfg(test)]
fn test_round_trip_timeout() -> Option<Duration> {
    test_budget::current().map(|budget| budget.round_trip)
}

#[cfg(test)]
fn test_host_tool_margin() -> Option<Duration> {
    test_budget::current().map(|budget| budget.margin)
}

#[cfg(test)]
fn test_max_host_tool_timeout() -> Option<Duration> {
    test_budget::current().map(|budget| budget.max_host_tool)
}

pub async fn execute_host_tool(
    lease: &ExecutionLease,
    tool_name: &str,
    arguments: Option<&prost_types::Struct>,
    capability_token: &vam_spec::v1::SignedEnvelope,
    call_id: &str,
) -> Result<vam_spec::v1::ExecuteHostToolResponse, RuntimeError> {
    let mut stream = guest_dial(2, 52).await.map_err(|e| {
        RuntimeError::Handler(format!("Failed to connect to host PEP on Port 52: {}", e))
    })?;
    execute_host_tool_on(
        &mut stream,
        lease,
        tool_name,
        arguments,
        capability_token,
        call_id,
    )
    .await
}

pub(crate) async fn execute_host_tool_on<S>(
    stream: &mut S,
    lease: &ExecutionLease,
    tool_name: &str,
    arguments: Option<&prost_types::Struct>,
    capability_token: &vam_spec::v1::SignedEnvelope,
    call_id: &str,
) -> Result<vam_spec::v1::ExecuteHostToolResponse, RuntimeError>
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    use vam_spec::v1::{ExecuteHostToolRequest, ExecuteHostToolResponse};

    let req = ExecuteHostToolRequest {
        lease: Some(lease.clone()),
        tool_name: tool_name.to_string(),
        arguments: arguments.cloned(),
        capability_token: Some(capability_token.clone()),
        call_id: call_id.to_string(),
    };

    let mut req_payload = Vec::new();
    req.encode(&mut req_payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;

    let env_in = round_trip(
        stream,
        "ExecuteHostTool",
        103,
        req_payload,
        execute_host_tool_timeout(capability_token),
    )
    .await?;
    if let Some(err) = env_in.error {
        return Ok(ExecuteHostToolResponse {
            text_result: String::new(),
            artifact_relpath: String::new(),
            artifact_bytes: Vec::new(),
            extra_artifacts: Vec::new(),
            error: Some(err),
        });
    }

    let start = std::time::Instant::now();
    let resp = ExecuteHostToolResponse::decode(&*env_in.payload)
        .map_err(|e| RuntimeError::Serialization(e.to_string()))?;
    let mut attrs = std::collections::HashMap::new();
    attrs.insert("tool.name".to_string(), tool_name.to_string());
    crate::telemetry::global_telemetry().record_span(
        "pep.execute_host_tool",
        start.elapsed(),
        None,
        attrs,
    );
    Ok(resp)
}

use ed25519_dalek::VerifyingKey;
use once_cell::sync::Lazy;
use vam_security::{verify_capability_token, CapabilityClaims, InMemoryReplayStore, ReplayStore};

static GUEST_REPLAY_STORE: Lazy<InMemoryReplayStore> = Lazy::new(InMemoryReplayStore::default);

pub fn verify_guest_capability_token(
    token: &vam_spec::v1::SignedEnvelope,
    verifying_key: Option<&VerifyingKey>,
    expected_tool_name: &str,
    expected_args_digest: &str,
    expected_script_digest: &str,
    expected_envelope: Option<&vam_spec::v1::ResourceEnvelope>,
    expected_lease_id: Option<&str>,
) -> Result<CapabilityClaims, RuntimeError> {
    let claims = vam_security::parse_capability_claims(token)
        .map_err(|e| RuntimeError::Handler(format!("Malformed capability token: {}", e)))?;

    if let Some(vk) = verifying_key {
        let key_id = token
            .signature
            .as_ref()
            .map(|s| s.key_id.as_str())
            .unwrap_or("");
        let lease = expected_lease_id.unwrap_or(&claims.lease_id);
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);

        verify_capability_token(
            key_id,
            vk,
            token,
            &claims.session_id,
            lease,
            claims.lifecycle_generation,
            expected_tool_name,
            expected_args_digest,
            expected_script_digest,
            expected_envelope,
            now_secs,
            &*GUEST_REPLAY_STORE,
        )
        .map_err(|e| {
            RuntimeError::Handler(format!("Capability token verification failed: {}", e))
        })?;
    } else {
        // If no explicit key provided, verify claims consistency and replay store
        if claims.tool_name != expected_tool_name {
            return Err(RuntimeError::Handler(format!(
                "Token tool '{}' does not match requested '{}'",
                claims.tool_name, expected_tool_name
            )));
        }
        if claims.arguments_digest != expected_args_digest {
            return Err(RuntimeError::Handler(format!(
                "Token arguments digest mismatch: token={}, actual={}",
                claims.arguments_digest, expected_args_digest
            )));
        }
        if claims.script_digest != expected_script_digest {
            return Err(RuntimeError::Handler(format!(
                "Token script digest mismatch: token={}, actual={}",
                claims.script_digest, expected_script_digest
            )));
        }
        if let Some(expected_env) = expected_envelope {
            if claims.resource_envelope.as_ref() != Some(expected_env) {
                return Err(RuntimeError::Handler(
                    "Token resource envelope mismatch".to_string(),
                ));
            }
        }
        let now_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if claims.expires_at < now_secs {
            return Err(RuntimeError::Handler(
                "Capability token expired".to_string(),
            ));
        }
        GUEST_REPLAY_STORE.consume(&claims.nonce).map_err(|e| {
            RuntimeError::Handler(format!("Capability token replay detected: {}", e))
        })?;
    }

    Ok(claims)
}

/// A mock host that answers port-52 checkpoint requests the way the host does.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use bytes::Bytes;
    use prost::Message;
    use std::sync::{Arc, Mutex};
    use tokio::io::{AsyncRead, AsyncWrite};
    use vam_spec::v1::{
        ErrorStatus, ListAdvertisedToolsResponse, ResourceId, SignedEnvelope, ToolIntentResponse,
    };
    use vam_vsock::frame::{Frame, FrameType};

    /// What the mock host holds, and switches that make it misbehave.
    #[derive(Default)]
    pub struct MockCheckpointHost {
        staged: Vec<u8>,
        staged_total: u64,
        staged_digest: String,
        /// The stored checkpoint, once an upload completes.
        pub stored: Option<Vec<u8>>,
        /// Refuse the upload chunk that starts at this offset.
        pub refuse_upload_at: Option<u64>,
        /// Cut every non-final fetch chunk short.
        pub short_fetch: bool,
        /// Flip a byte in every fetched chunk.
        pub corrupt_fetch: bool,
        /// Disposition returned for tool-intent requests (see `ToolIntentResponse`).
        pub intent_disposition: i32,
        /// Attach a capability token to tool-intent replies.
        pub capability_granted: bool,
        /// Tool names the host advertises (`ListAdvertisedTools`); the model is offered only these.
        pub advertised_tools: Vec<String>,
        /// Methods received, in order.
        pub methods_seen: Vec<String>,
        /// Tools the host refuses at intent time, with the reason it gives.
        pub denied_tools: std::collections::HashMap<String, String>,
        /// Text the host returns for `ExecuteHostTool`, by tool name.
        pub host_tool_text: std::collections::HashMap<String, String>,
        /// Tool names asked about in `RequestToolAuthorization`, in order.
        pub intents_seen: Vec<String>,
        /// Tools the host answers at intent as already made (`REPLAYED`), with the recorded text.
        pub replayed_tools: std::collections::HashMap<String, String>,
        /// How the host answers `CommitTurn`: `0` sends an empty reply, `1`
        /// accepts, `2` says turn commits are unavailable (code 12), `3` fails
        /// with a storage error (code 13, `storage_commit_failed`).
        pub commit_turn_mode: u8,
        /// `(round, tail digest)` of every `CommitTurn` received.
        pub commit_turns_seen: Vec<(u32, String)>,
        /// Steers the host holds, by seq. A `PullInbox` returns those after its seq.
        pub inbox: Vec<(i64, String)>,
        /// Answer this many `PullInbox` calls with nothing before the steers show.
        pub inbox_hold_pulls: usize,
        /// Most text bytes in one `PullInbox` reply (0: no limit; one message at least).
        pub inbox_batch_bytes: usize,
        /// `(after_seq, ack_seq)` of every `PullInbox` received, in order.
        pub inbox_pulls_seen: Vec<(i64, i64)>,
        /// Every `PostOutbox` received, in order.
        pub posts_seen: Vec<vam_spec::v1::PostOutboxRequest>,
        /// How the host answers `PostOutbox`: `None` accepts; `Some((code, text))` refuses.
        pub post_outbox_error: Option<(i32, String)>,
    }

    pub type SharedMockHost = Arc<Mutex<MockCheckpointHost>>;

    pub fn shared_host(stored: Option<Vec<u8>>) -> SharedMockHost {
        Arc::new(Mutex::new(MockCheckpointHost {
            stored,
            ..MockCheckpointHost::default()
        }))
    }

    /// A client stream whose far end is answered by `host`.
    pub fn connect(host: &SharedMockHost) -> tokio::io::DuplexStream {
        let (client, server) = tokio::io::duplex(256 * 1024);
        tokio::spawn(serve(server, host.clone()));
        client
    }

    /// Answer port-52 requests on `stream` until the peer closes it.
    pub async fn serve<S>(mut stream: S, host: SharedMockHost)
    where
        S: AsyncRead + AsyncWrite + Unpin,
    {
        while let Ok(frame) = Frame::read_from(&mut stream).await {
            let Ok(request) = VsockEnvelope::decode(&*frame.payload) else {
                break;
            };
            let reply = VsockEnvelope {
                method: format!("{}Response", request.method),
                correlation_id: request.correlation_id,
                payload: answer(&host, &request),
                error: None,
                trace_context: None,
            };
            let mut reply_bytes = Vec::new();
            if reply.encode(&mut reply_bytes).is_err() {
                break;
            }
            let out = Frame {
                frame_type: FrameType::ControlGrpc,
                correlation_id: request.correlation_id,
                payload: Bytes::from(reply_bytes),
            };
            if out.write_to(&mut stream).await.is_err() {
                break;
            }
        }
    }

    fn answer(host: &SharedMockHost, request: &VsockEnvelope) -> Vec<u8> {
        let mut host = host.lock().expect("mock host lock");
        host.methods_seen.push(request.method.clone());
        let mut out = Vec::new();
        match request.method.as_str() {
            "UploadRunCheckpoint" => {
                if let Ok(req) = UploadRunCheckpointRequest::decode(&*request.payload) {
                    let _ = handle_upload(&mut host, &req).encode(&mut out);
                }
            }
            "FetchRunCheckpoint" => {
                if let Ok(req) = FetchRunCheckpointRequest::decode(&*request.payload) {
                    let _ = handle_fetch(&host, &req).encode(&mut out);
                }
            }
            "RequestToolAuthorization" => {
                let tool = vam_spec::v1::ToolIntentRequest::decode(&*request.payload)
                    .map(|req| req.tool_name)
                    .unwrap_or_default();
                host.intents_seen.push(tool.clone());
                let response = if let Some(recorded) = host.replayed_tools.get(&tool) {
                    ToolIntentResponse {
                        disposition: vam_spec::v1::tool_intent_response::Disposition::Replayed
                            as i32,
                        approval_request_id: None,
                        error: None,
                        capability_token: None,
                        resource_envelope: None,
                        replayed: Some(vam_spec::v1::ExecuteHostToolResponse {
                            text_result: recorded.clone(),
                            ..Default::default()
                        }),
                    }
                } else if let Some(reason) = host.denied_tools.get(&tool) {
                    ToolIntentResponse {
                        disposition: vam_spec::v1::tool_intent_response::Disposition::Denied as i32,
                        approval_request_id: None,
                        error: Some(ErrorStatus {
                            code: 7,
                            message: reason.clone(),
                            details: vec![],
                            retryable: false,
                        }),
                        capability_token: None,
                        resource_envelope: None,
                        replayed: None,
                    }
                } else {
                    ToolIntentResponse {
                        disposition: host.intent_disposition,
                        approval_request_id: Some(ResourceId {
                            urn: "urn:vam:approval:mock".to_string(),
                        }),
                        error: None,
                        capability_token: host.capability_granted.then(SignedEnvelope::default),
                        resource_envelope: None,
                        replayed: None,
                    }
                };
                let _ = response.encode(&mut out);
            }
            "ExecuteHostTool" => {
                let tool = vam_spec::v1::ExecuteHostToolRequest::decode(&*request.payload)
                    .map(|req| req.tool_name)
                    .unwrap_or_default();
                let response = vam_spec::v1::ExecuteHostToolResponse {
                    text_result: host.host_tool_text.get(&tool).cloned().unwrap_or_default(),
                    ..Default::default()
                };
                let _ = response.encode(&mut out);
            }
            "ListAdvertisedTools" => {
                let response = ListAdvertisedToolsResponse {
                    tool_names: host.advertised_tools.clone(),
                };
                let _ = response.encode(&mut out);
            }
            "PullInbox" => {
                if let Ok(req) = vam_spec::v1::PullInboxRequest::decode(&*request.payload) {
                    host.inbox_pulls_seen.push((req.after_seq, req.ack_seq));
                    let mut messages = Vec::new();
                    if host.inbox_hold_pulls > 0 {
                        host.inbox_hold_pulls -= 1;
                    } else {
                        let mut bytes = 0usize;
                        for (seq, text) in host.inbox.iter().filter(|(s, _)| *s > req.after_seq) {
                            if host.inbox_batch_bytes > 0
                                && !messages.is_empty()
                                && bytes + text.len() > host.inbox_batch_bytes
                            {
                                break;
                            }
                            bytes += text.len();
                            messages.push(vam_spec::v1::InboxMessage {
                                seq: *seq,
                                text: text.clone(),
                            });
                        }
                    }
                    let _ = vam_spec::v1::PullInboxResponse {
                        messages,
                        error: None,
                    }
                    .encode(&mut out);
                }
            }
            "PostOutbox" => {
                if let Ok(req) = vam_spec::v1::PostOutboxRequest::decode(&*request.payload) {
                    host.posts_seen.push(req);
                }
                let error = host
                    .post_outbox_error
                    .clone()
                    .map(|(code, message)| ErrorStatus {
                        code,
                        message,
                        details: vec![],
                        retryable: false,
                    });
                let _ = vam_spec::v1::PostOutboxResponse { error }.encode(&mut out);
            }
            "CommitTurn" => {
                if let Ok(req) = vam_spec::v1::CommitTurnRequest::decode(&*request.payload) {
                    host.commit_turns_seen.push((
                        req.round,
                        req.checkpoint_digest
                            .map(|d| d.hex_hash)
                            .unwrap_or_default(),
                    ));
                }
                let reply = match host.commit_turn_mode {
                    1 => Some(vam_spec::v1::CommitTurnResponse {
                        accepted: true,
                        ..Default::default()
                    }),
                    2 | 3 => {
                        let (code, message) = if host.commit_turn_mode == 2 {
                            (12, "turn commits not enabled")
                        } else {
                            (13, "storage_commit_failed: the host could not snapshot")
                        };
                        Some(vam_spec::v1::CommitTurnResponse {
                            accepted: false,
                            error: Some(ErrorStatus {
                                code,
                                message: message.to_string(),
                                details: vec![],
                                retryable: false,
                            }),
                            ..Default::default()
                        })
                    }
                    _ => None,
                };
                if let Some(reply) = reply {
                    let _ = reply.encode(&mut out);
                }
            }
            _ => {}
        }
        out
    }

    fn error_status(message: &str) -> ErrorStatus {
        ErrorStatus {
            code: 3,
            message: message.to_string(),
            details: vec![],
            retryable: false,
        }
    }

    fn upload_refusal(message: &str, received: u64) -> UploadRunCheckpointResponse {
        UploadRunCheckpointResponse {
            accepted: false,
            digest: String::new(),
            error: Some(error_status(message)),
            received,
            complete: false,
        }
    }

    fn handle_upload(
        host: &mut MockCheckpointHost,
        req: &UploadRunCheckpointRequest,
    ) -> UploadRunCheckpointResponse {
        let digest = req
            .digest
            .as_ref()
            .map(|d| d.hex_hash.clone())
            .unwrap_or_default();
        let received = host.staged.len() as u64;
        if host.refuse_upload_at == Some(req.offset) {
            return upload_refusal("refused by mock host", received);
        }
        if req.offset == 0 {
            host.staged.clear();
            host.staged_total = req.total_size;
            host.staged_digest = digest.clone();
        }
        let in_order = req.offset == received
            && req.total_size == host.staged_total
            && digest == host.staged_digest;
        if !in_order {
            return upload_refusal("chunk is out of order", received);
        }
        host.staged.extend_from_slice(&req.checkpoint);
        let received = host.staged.len() as u64;
        if received < host.staged_total {
            return UploadRunCheckpointResponse {
                accepted: true,
                digest,
                error: None,
                received,
                complete: false,
            };
        }
        let assembled = std::mem::take(&mut host.staged);
        if sha256_hex(&assembled) != digest {
            return upload_refusal("assembled checkpoint fails its digest", received);
        }
        host.stored = Some(assembled);
        UploadRunCheckpointResponse {
            accepted: true,
            digest,
            error: None,
            received,
            complete: true,
        }
    }

    fn handle_fetch(
        host: &MockCheckpointHost,
        req: &FetchRunCheckpointRequest,
    ) -> FetchRunCheckpointResponse {
        let refusal = |message: &str| FetchRunCheckpointResponse {
            chunk: Vec::new(),
            total_size: 0,
            last: false,
            error: Some(error_status(message)),
        };
        let digest = req
            .digest
            .as_ref()
            .map(|d| d.hex_hash.as_str())
            .unwrap_or_default();
        let Some(stored) = host.stored.as_ref() else {
            return refusal("run has no checkpoint");
        };
        if sha256_hex(stored) != digest {
            return refusal("checkpoint digest mismatch");
        }
        let total = stored.len();
        let offset = req.offset as usize;
        if offset > total {
            return refusal("checkpoint offset is past the end");
        }
        let end = offset.saturating_add(CHECKPOINT_CHUNK_BYTES).min(total);
        let mut chunk = stored[offset..end].to_vec();
        if host.corrupt_fetch && !chunk.is_empty() {
            chunk[0] ^= 0xff;
        }
        if host.short_fetch && end < total {
            chunk.truncate(1024);
        }
        FetchRunCheckpointResponse {
            chunk,
            total_size: total as u64,
            last: end == total,
            error: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_budget;
    use super::test_support::{connect, shared_host, SharedMockHost};
    use super::*;
    use bytes::Bytes;
    use prost::Message;
    use std::time::Duration;
    use vam_spec::v1::ResourceId;
    use vam_vsock::frame::{Frame, FrameType, MAX_PAYLOAD_SIZE};

    fn lease() -> ExecutionLease {
        ExecutionLease {
            run_id: Some(ResourceId {
                urn: "urn:vam:run:pep-checkpoint".to_string(),
            }),
            step_id: "step-checkpoint".to_string(),
            attempt_id: Some(ResourceId {
                urn: "urn:vam:attempt:pep-checkpoint".to_string(),
            }),
            lease_id: Some(ResourceId {
                urn: "urn:vam:lease:pep-checkpoint".to_string(),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: None,
            idempotency_key: "pep-checkpoint".to_string(),
        }
    }

    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    fn upload_count(host: &SharedMockHost) -> usize {
        host.lock()
            .expect("host")
            .methods_seen
            .iter()
            .filter(|m| m.as_str() == "UploadRunCheckpoint")
            .count()
    }

    fn envelope_len<M: Message>(method: &str, message: &M) -> usize {
        let mut payload = Vec::new();
        message.encode(&mut payload).expect("payload");
        let envelope = VsockEnvelope {
            method: method.to_string(),
            correlation_id: 1,
            payload,
            error: None,
            trace_context: None,
        };
        let mut bytes = Vec::new();
        envelope.encode(&mut bytes).expect("envelope");
        bytes.len()
    }

    #[tokio::test]
    async fn one_mib_checkpoint_uploads_in_32_kib_chunks_and_fetches_back_identical() {
        let bytes = pattern(1024 * 1024);
        let digest = sha256_hex(&bytes);
        let host = shared_host(None);

        let mut uploading = connect(&host);
        upload_checkpoint_chunks(&mut uploading, &lease(), &bytes, &digest)
            .await
            .expect("upload");
        assert_eq!(upload_count(&host), 32);
        assert_eq!(
            host.lock().expect("host").stored.as_deref(),
            Some(bytes.as_slice())
        );

        let mut fetching = connect(&host);
        let fetched = fetch_checkpoint_chunks(&mut fetching, &lease(), &digest)
            .await
            .expect("fetch");
        assert_eq!(fetched, bytes);
    }

    #[tokio::test]
    async fn commit_turn_maps_the_hosts_answers_and_sends_round_and_digest() {
        let digest = "ab".repeat(32);
        for (mode, want) in [
            (1u8, CommitTurnOutcome::Accepted),
            (2, CommitTurnOutcome::Unavailable),
            (
                3,
                CommitTurnOutcome::Refused(
                    "storage_commit_failed: the host could not snapshot".to_string(),
                ),
            ),
            (
                0,
                CommitTurnOutcome::Refused("the host did not accept the turn commit".to_string()),
            ),
        ] {
            let host = shared_host(None);
            host.lock().expect("host").commit_turn_mode = mode;
            let mut stream = connect(&host);
            let got = commit_turn_on(&mut stream, &lease(), 4, &digest, true)
                .await
                .expect("answered");
            assert_eq!(got, want, "mode {mode}");
            assert_eq!(
                host.lock().expect("host").commit_turns_seen,
                vec![(4, digest.clone())]
            );
        }
    }

    #[tokio::test]
    async fn refused_chunk_stops_the_upload_with_the_host_message() {
        let bytes = pattern(3 * CHECKPOINT_CHUNK_BYTES);
        let digest = sha256_hex(&bytes);
        let host = shared_host(None);
        host.lock().expect("host").refuse_upload_at = Some(CHECKPOINT_CHUNK_BYTES as u64);

        let mut uploading = connect(&host);
        let err = upload_checkpoint_chunks(&mut uploading, &lease(), &bytes, &digest)
            .await
            .expect_err("refused");
        assert!(err.to_string().contains("refused by mock host"), "{err}");
        assert_eq!(upload_count(&host), 2, "no chunk is sent after a refusal");
        assert!(host.lock().expect("host").stored.is_none());
    }

    #[tokio::test]
    async fn fetch_rejects_a_short_middle_chunk() {
        let bytes = pattern(2 * CHECKPOINT_CHUNK_BYTES + 100);
        let digest = sha256_hex(&bytes);
        let host = shared_host(Some(bytes));
        host.lock().expect("host").short_fetch = true;

        let mut fetching = connect(&host);
        let err = fetch_checkpoint_chunks(&mut fetching, &lease(), &digest)
            .await
            .expect_err("non-contiguous fetch");
        assert!(err.to_string().contains("not a full chunk"), "{err}");
    }

    #[tokio::test]
    async fn fetch_rejects_bytes_that_fail_the_digest() {
        let bytes = pattern(CHECKPOINT_CHUNK_BYTES + 5);
        let digest = sha256_hex(&bytes);
        let host = shared_host(Some(bytes));
        host.lock().expect("host").corrupt_fetch = true;

        let mut fetching = connect(&host);
        let err = fetch_checkpoint_chunks(&mut fetching, &lease(), &digest)
            .await
            .expect_err("digest mismatch");
        assert!(
            err.to_string().contains("failed digest verification"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn oversized_checkpoint_is_refused_before_dialing() {
        let bytes = vec![0u8; MAX_CHECKPOINT_BYTES + 1];
        let err = upload_run_checkpoint(&lease(), &bytes, &"00".repeat(32))
            .await
            .expect_err("too large");
        assert!(
            err.to_string().contains(&MAX_CHECKPOINT_BYTES.to_string()),
            "{err}"
        );
    }

    #[test]
    fn full_chunk_envelopes_fit_in_one_frame() {
        let upload = UploadRunCheckpointRequest {
            lease: Some(lease()),
            checkpoint: vec![0xab; CHECKPOINT_CHUNK_BYTES],
            digest: Some(SpecDigest {
                algorithm: 1,
                hex_hash: "ab".repeat(32),
            }),
            offset: (MAX_CHECKPOINT_BYTES - CHECKPOINT_CHUNK_BYTES) as u64,
            total_size: MAX_CHECKPOINT_BYTES as u64,
        };
        let fetch = FetchRunCheckpointResponse {
            chunk: vec![0xab; CHECKPOINT_CHUNK_BYTES],
            total_size: MAX_CHECKPOINT_BYTES as u64,
            last: false,
            error: None,
        };
        assert!(envelope_len("UploadRunCheckpoint", &upload) <= MAX_PAYLOAD_SIZE);
        assert!(envelope_len("FetchRunCheckpointResponse", &fetch) <= MAX_PAYLOAD_SIZE);
    }

    fn token_with_wall_time(max_wall_time_ms: u64) -> vam_spec::v1::SignedEnvelope {
        let claims = vam_security::CapabilityClaims {
            session_id: "sess".to_string(),
            lease_id: "lease".to_string(),
            lifecycle_generation: 1,
            tool_name: "demo".to_string(),
            arguments_digest: String::new(),
            script_digest: String::new(),
            resource_envelope: Some(vam_spec::v1::ResourceEnvelope {
                max_wall_time_ms,
                max_cpu_time_ms: 0,
                max_memory_bytes: 0,
                max_procs: 0,
                max_open_files: 0,
                max_file_size_bytes: 0,
                max_egress_bytes: 0,
                max_output_bytes: 0,
            }),
            expires_at: u64::MAX,
            nonce: "nonce".to_string(),
            call_id: String::new(),
        };
        vam_spec::v1::SignedEnvelope {
            domain: String::new(),
            envelope_version: None,
            claims: serde_json::to_vec(&claims).expect("claims"),
            signature: None,
        }
    }

    async fn answer_host_tool_after(mut stream: tokio::io::DuplexStream, delay: Duration) {
        tokio::time::sleep(delay).await;
        let frame = Frame::read_from(&mut stream).await.expect("request frame");
        let request = VsockEnvelope::decode(&*frame.payload).expect("request");
        let response = vam_spec::v1::ExecuteHostToolResponse {
            text_result: "ok".to_string(),
            artifact_relpath: String::new(),
            artifact_bytes: Vec::new(),
            extra_artifacts: Vec::new(),
            error: None,
        };
        let mut payload = Vec::new();
        response.encode(&mut payload).expect("encode");
        let reply = VsockEnvelope {
            method: "ExecuteHostToolResponse".to_string(),
            correlation_id: request.correlation_id,
            payload,
            error: None,
            trace_context: None,
        };
        let mut bytes_out = Vec::new();
        reply.encode(&mut bytes_out).expect("envelope");
        let out = Frame {
            frame_type: FrameType::ControlGrpc,
            correlation_id: request.correlation_id,
            payload: Bytes::from(bytes_out),
        };
        out.write_to(&mut stream).await.expect("reply");
    }

    /// Reads one request frame and returns the decoded envelope payload.
    async fn read_request(stream: &mut tokio::io::DuplexStream) -> Vec<u8> {
        let frame = Frame::read_from(stream).await.expect("request frame");
        VsockEnvelope::decode(&*frame.payload)
            .expect("request")
            .payload
    }

    #[tokio::test]
    async fn the_models_call_id_goes_out_on_the_intent_and_on_the_execute() {
        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let intent = tokio::spawn(async move {
            // The reply never comes; the client times out. Only the request matters.
            read_request(&mut server).await
        });
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            request_tool_authorization_on(&mut client, &lease(), "echo", None, None, "call_7"),
        )
        .await;
        let payload = intent.await.unwrap();
        let request = ToolIntentRequest::decode(&*payload).expect("intent");
        assert_eq!(request.call_id, "call_7");

        let (mut client, mut server) = tokio::io::duplex(64 * 1024);
        let execute = tokio::spawn(async move { read_request(&mut server).await });
        let token = vam_spec::v1::SignedEnvelope::default();
        let _ = tokio::time::timeout(
            Duration::from_millis(200),
            execute_host_tool_on(&mut client, &lease(), "echo", None, &token, "call_7"),
        )
        .await;
        let payload = execute.await.unwrap();
        let request = vam_spec::v1::ExecuteHostToolRequest::decode(&*payload).expect("execute");
        assert_eq!(request.call_id, "call_7");
    }

    #[test]
    fn execute_host_tool_budget_is_the_tool_timeout_plus_margin() {
        let unset = vam_spec::v1::SignedEnvelope::default();
        assert_eq!(
            execute_host_tool_timeout(&unset),
            MAX_HOST_TOOL_TIMEOUT + PORT52_HOST_TOOL_MARGIN
        );
        let short = token_with_wall_time(10_000);
        assert_eq!(
            execute_host_tool_timeout(&short),
            MAX_HOST_TOOL_TIMEOUT + PORT52_HOST_TOOL_MARGIN
        );
        let long = token_with_wall_time(700_000);
        assert_eq!(
            execute_host_tool_timeout(&long),
            Duration::from_millis(700_000) + PORT52_HOST_TOOL_MARGIN
        );
        assert!(execute_host_tool_timeout(&long) > Duration::from_millis(700_000));
    }

    fn install_short_budget() -> test_budget::Guard {
        test_budget::install(test_budget::Budget {
            round_trip: Duration::from_millis(40),
            margin: Duration::from_millis(30),
            max_host_tool: Duration::from_millis(60),
        })
    }

    fn assert_within(started: std::time::Instant, budget: Duration) {
        let elapsed = started.elapsed();
        assert!(
            elapsed + Duration::from_millis(15) >= budget,
            "returned in {elapsed:?}, before the {budget:?} budget"
        );
        assert!(
            elapsed < budget + Duration::from_secs(2),
            "waited {elapsed:?}, past the {budget:?} budget"
        );
    }

    #[tokio::test]
    async fn silent_host_fails_each_port52_call_within_its_timeout() {
        let _budget = install_short_budget();
        let round_trip = round_trip_timeout();

        let (mut auth, _auth_server) = tokio::io::duplex(64 * 1024);
        let started = std::time::Instant::now();
        let err = request_tool_authorization_on(&mut auth, &lease(), "echo", None, None, "")
            .await
            .expect_err("authorization");
        assert!(
            err.to_string().contains("RequestToolAuthorization"),
            "{err}"
        );
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_within(started, round_trip);

        let (mut tools, _tools_server) = tokio::io::duplex(64 * 1024);
        let started = std::time::Instant::now();
        let err = list_advertised_tools_on(&mut tools)
            .await
            .expect_err("advertised tools");
        assert!(err.to_string().contains("ListAdvertisedTools"), "{err}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_within(started, round_trip);

        let bytes = pattern(16);
        let digest = sha256_hex(&bytes);
        let (mut upload, _upload_server) = tokio::io::duplex(64 * 1024);
        let started = std::time::Instant::now();
        let err = upload_checkpoint_chunks(&mut upload, &lease(), &bytes, &digest)
            .await
            .expect_err("upload");
        assert!(err.to_string().contains("UploadRunCheckpoint"), "{err}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_within(started, round_trip);

        let (mut fetch, _fetch_server) = tokio::io::duplex(64 * 1024);
        let err = fetch_checkpoint_chunks(&mut fetch, &lease(), &digest)
            .await
            .expect_err("fetch");
        assert!(err.to_string().contains("FetchRunCheckpoint"), "{err}");
        assert!(err.to_string().contains("timed out"), "{err}");

        let (mut tool, _tool_server) = tokio::io::duplex(64 * 1024);
        let token = vam_spec::v1::SignedEnvelope::default();
        let budget = execute_host_tool_timeout(&token);
        assert!(budget > max_host_tool_timeout());
        let started = std::time::Instant::now();
        let err = execute_host_tool_on(&mut tool, &lease(), "echo", None, &token, "")
            .await
            .expect_err("host tool");
        assert!(err.to_string().contains("ExecuteHostTool"), "{err}");
        assert!(err.to_string().contains("timed out"), "{err}");
        assert_within(started, budget);
    }

    #[tokio::test]
    async fn host_tool_call_is_not_cut_short_before_the_tool_timeout() {
        let _budget = install_short_budget();
        let token = vam_spec::v1::SignedEnvelope::default();
        let tool_timeout = max_host_tool_timeout();
        assert!(execute_host_tool_timeout(&token) > tool_timeout);
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(answer_host_tool_after(server, tool_timeout));
        let response = execute_host_tool_on(&mut client, &lease(), "echo", None, &token, "")
            .await
            .expect("answered at the tool timeout, inside the margin");
        assert!(response.error.is_none());
        assert_eq!(response.text_result, "ok");
    }

    #[tokio::test]
    async fn host_tool_call_waits_out_a_tool_timeout_above_the_mcp_ceiling() {
        let _budget = install_short_budget();
        let tool_timeout = Duration::from_millis(80);
        let token = token_with_wall_time(tool_timeout.as_millis() as u64);
        assert!(execute_host_tool_timeout(&token) > tool_timeout);
        assert!(execute_host_tool_timeout(&token) > max_host_tool_timeout() + host_tool_margin());
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        tokio::spawn(answer_host_tool_after(server, tool_timeout));
        let response = execute_host_tool_on(&mut client, &lease(), "mcp.demo", None, &token, "")
            .await
            .expect("answered at the tool's own timeout");
        assert_eq!(response.text_result, "ok");
    }
}
