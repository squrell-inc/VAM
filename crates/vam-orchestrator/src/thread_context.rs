//! The host builds a task's starting context (AR-2.4/2.5).
//!
//! Ordered [`ContextSource`]s each contribute history messages and injected
//! blocks; today only [`ThreadLogSource`] exists (a memory broker is a later
//! source). The result is a [`vam_spec::thread_context::ThreadContext`]. Its
//! bytes are written **beside** the session (never under `materialized/`, so
//! never extracted into the workspace), and the SHA-256 of those bytes is
//! granted to one run in `run_checkpoint_grants`. The guest fetches them with
//! `FetchRunCheckpoint`; it never names a thread, a workspace or a path.

use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use vam_spec::thread_context::{
    sha256_hex, ContextBlock, SuspendResume, ThreadContext, WireMessage, WireToolCall,
    MAX_THREAD_CONTEXT_BYTES, THREAD_CONTEXT_FORMAT, THREAD_CONTEXT_VERSION,
};
use vam_storage::thread_log::{
    assemble_context, read_all_records, Integrity, ThreadLogError, TreeThreadStore, MAIN_LANE,
};
use vam_storage::Drive9Layout;

/// A built context with the facts the host needs about it.
#[derive(Debug, Clone)]
pub struct BuiltContext {
    pub context: ThreadContext,
    pub bytes: Vec<u8>,
    pub digest: String,
    /// Runs whose records are in the history (their context URLs are inherited).
    pub runs: Vec<String>,
    /// Non-fatal findings (for example an integrity break that was cut off).
    pub warnings: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("thread context could not be built: {0}")]
    Source(String),
    #[error("thread context is {0} bytes, over the {MAX_THREAD_CONTEXT_BYTES} byte guard")]
    TooLarge(usize),
    #[error("thread context I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid context digest")]
    InvalidDigest,
}

/// What one source contributes.
#[derive(Debug, Default, Clone)]
pub struct SourceOutput {
    pub messages: Vec<WireMessage>,
    pub blocks: Vec<ContextBlock>,
    pub head: Option<(u64, String)>,
    pub system_sha256: Option<String>,
    pub tools_sha256: Option<String>,
    pub runs: Vec<String>,
    pub warnings: Vec<String>,
}

pub trait ContextSource: Send + Sync {
    fn name(&self) -> &str;
    fn load(&self) -> Result<SourceOutput, ContextError>;
}

/// The thread log of one thread, read from a session tree.
pub struct ThreadLogSource {
    store: TreeThreadStore,
    thread: String,
}

impl ThreadLogSource {
    pub fn new(materialized: &Path, thread: &str) -> Self {
        Self {
            store: TreeThreadStore::new(materialized),
            thread: thread.to_string(),
        }
    }
}

fn to_spec(message: &vam_storage::thread_log::LogMessage) -> WireMessage {
    let wire = message.to_wire();
    WireMessage {
        role: wire.role,
        content: wire.content,
        tool_calls: wire
            .tool_calls
            .into_iter()
            .map(|(id, name, arguments)| WireToolCall {
                id,
                name,
                arguments,
            })
            .collect(),
        tool_call_id: wire.tool_call_id,
    }
}

fn map_assemble_error(e: ThreadLogError) -> ContextError {
    match e {
        ThreadLogError::ContextTooLarge { bytes } => ContextError::TooLarge(bytes),
        other => ContextError::Source(other.to_string()),
    }
}

impl ContextSource for ThreadLogSource {
    fn name(&self) -> &str {
        "thread_log"
    }

    fn load(&self) -> Result<SourceOutput, ContextError> {
        let (records, integrity) = read_all_records(&self.store, &self.thread)
            .map_err(|e| ContextError::Source(format!("thread log unreadable: {e}")))?;
        let mut out = SourceOutput::default();
        if let Integrity::BrokenAt { seq, reason } = &integrity {
            out.warnings.push(format!(
                "thread log broken at seq {seq}: {reason}; using the records before it"
            ));
        }
        if records.is_empty() {
            return Ok(out);
        }
        let assembled = assemble_context(&records, MAIN_LANE).map_err(map_assemble_error)?;
        out.messages = assembled.messages.iter().map(to_spec).collect();
        out.head = assembled.head.map(|h| (h.seq, h.digest));
        out.system_sha256 = assembled.system_sha256;
        out.tools_sha256 = assembled.tools_sha256;
        out.runs = assembled.runs;
        Ok(out)
    }
}

/// Builds the context for `run_id` from `sources`, in order.
pub fn build_context(
    run_id: &str,
    sources: &[Box<dyn ContextSource>],
) -> Result<BuiltContext, ContextError> {
    build_context_with(run_id, sources, None)
}

/// [`build_context`] for a task that resumes a suspended tool call.
pub fn build_context_with(
    run_id: &str,
    sources: &[Box<dyn ContextSource>],
    suspend: Option<SuspendResume>,
) -> Result<BuiltContext, ContextError> {
    let mut messages = Vec::new();
    let mut blocks = Vec::new();
    let mut head = None;
    let (mut system_sha256, mut tools_sha256) = (None, None);
    let mut runs: Vec<String> = Vec::new();
    let mut warnings = Vec::new();
    for source in sources {
        let out = source.load()?;
        messages.extend(out.messages);
        blocks.extend(out.blocks);
        head = head.or(out.head);
        system_sha256 = system_sha256.or(out.system_sha256);
        tools_sha256 = tools_sha256.or(out.tools_sha256);
        for run in out.runs {
            if !runs.contains(&run) {
                runs.push(run);
            }
        }
        warnings.extend(out.warnings);
    }
    let (head_seq, head_digest) = head.unwrap_or_default();
    let context = ThreadContext {
        format: THREAD_CONTEXT_FORMAT.to_string(),
        version: THREAD_CONTEXT_VERSION,
        run_id: run_id.to_string(),
        head_seq,
        head_digest,
        system_sha256,
        tools_sha256,
        messages,
        blocks,
        suspend,
        crash: None,
    };
    let bytes = serde_json::to_vec(&context).map_err(|e| ContextError::Source(e.to_string()))?;
    if bytes.len() > MAX_THREAD_CONTEXT_BYTES {
        return Err(ContextError::TooLarge(bytes.len()));
    }
    let digest = sha256_hex(&bytes);
    Ok(BuiltContext {
        context,
        bytes,
        digest,
        runs,
        warnings,
    })
}

/// The same context with the crash-resume facts set (E10 Phase 4): bytes and digest are
/// recomputed, so the digest the guest fetches by is the one of the final context.
pub fn with_crash(
    mut built: BuiltContext,
    crash: vam_spec::thread_context::CrashResume,
) -> Result<BuiltContext, ContextError> {
    built.context.crash = Some(crash);
    let bytes =
        serde_json::to_vec(&built.context).map_err(|e| ContextError::Source(e.to_string()))?;
    if bytes.len() > MAX_THREAD_CONTEXT_BYTES {
        return Err(ContextError::TooLarge(bytes.len()));
    }
    built.digest = sha256_hex(&bytes);
    built.bytes = bytes;
    Ok(built)
}

/// The latest `suspend` record `run_id` wrote on this thread (after rollbacks),
/// as the host serves it back to the resuming guest.
pub fn latest_suspend(
    materialized: &Path,
    thread: &str,
    run_id: &str,
) -> Result<Option<SuspendResume>, ContextError> {
    let store = TreeThreadStore::new(materialized);
    let (records, _) = read_all_records(&store, thread)
        .map_err(|e| ContextError::Source(format!("thread log unreadable: {e}")))?;
    let included = vam_storage::thread_log::records_without_rollbacks(&records, MAIN_LANE);
    for record in included.into_iter().rev() {
        if record.kind != vam_storage::thread_log::kind::SUSPEND || record.run != run_id {
            continue;
        }
        let Some(body) = record.body.as_ref() else {
            continue;
        };
        let parse = |key: &str| body.get(key).cloned();
        let pending = parse("pending").and_then(|v| serde_json::from_value(v).ok());
        let state = parse("state").and_then(|v| serde_json::from_value(v).ok());
        let (Some(pending), Some(state)) = (pending, state) else {
            return Err(ContextError::Source("suspend record is malformed".into()));
        };
        let unanswered: Vec<String> = parse("unanswered")
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        return Ok(Some(SuspendResume {
            lifecycle_generation: body
                .get("lifecycle_generation")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0),
            pending,
            unanswered,
            state,
        }));
    }
    Ok(None)
}

pub fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// `<session dir>/thread-context/<digest>.json`, outside `materialized/`.
pub fn context_path(
    layout: &Drive9Layout,
    session_id: &str,
    digest: &str,
) -> Result<PathBuf, ContextError> {
    if !valid_digest(digest)
        || session_id.is_empty()
        || session_id.contains('/')
        || session_id.contains("..")
    {
        return Err(ContextError::InvalidDigest);
    }
    Ok(layout
        .session_dir(session_id)
        .join("thread-context")
        .join(format!("{digest}.json")))
}

/// Writes the bytes atomically and returns the path.
pub fn write_context(
    layout: &Drive9Layout,
    session_id: &str,
    built: &BuiltContext,
) -> Result<PathBuf, ContextError> {
    let path = context_path(layout, session_id, &built.digest)?;
    let dir = path.parent().ok_or(ContextError::InvalidDigest)?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!("{}.tmp", built.digest));
    {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(&built.bytes)?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Removes a written context (best effort).
pub fn remove_context(layout: &Drive9Layout, session_id: &str, digest: &str) {
    if let Ok(path) = context_path(layout, session_id, digest) {
        let _ = std::fs::remove_file(path);
    }
}

/// `(total_size, chunk)` of the stored context at `offset`.
pub fn read_context_chunk(
    path: &Path,
    offset: u64,
    chunk_bytes: usize,
) -> Result<(u64, Vec<u8>), ContextError> {
    let mut file = std::fs::File::open(path)?;
    let total = file.metadata()?.len();
    if total == 0 || total > MAX_THREAD_CONTEXT_BYTES as u64 {
        return Err(ContextError::TooLarge(total as usize));
    }
    if offset > total {
        return Err(ContextError::Source("offset is past the end".into()));
    }
    file.seek(SeekFrom::Start(offset))?;
    let want = (total - offset).min(chunk_bytes as u64) as usize;
    let mut buf = vec![0u8; want];
    file.read_exact(&mut buf)?;
    Ok((total, buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use vam_spec::thread_context::{BlockKind, BlockPlacement, BlockTrust};
    use vam_storage::thread_log::{
        kind, message_body, LogMessage, NewRecord, ThreadStore, WireMessage as StoreWire,
    };

    fn msg(run: &str, role: &str, text: &str) -> NewRecord {
        let m = LogMessage::from_wire(&StoreWire {
            role: role.into(),
            content: text.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        });
        let mut r = NewRecord::new(kind::MESSAGE, "broker", message_body(&m, None));
        r.run = run.into();
        r
    }

    struct Fake;
    impl ContextSource for Fake {
        fn name(&self) -> &str {
            "fake"
        }
        fn load(&self) -> Result<SourceOutput, ContextError> {
            Ok(SourceOutput {
                blocks: vec![ContextBlock {
                    source: "fake".into(),
                    kind: BlockKind::UserMemory,
                    placement: BlockPlacement::Stable,
                    trust: BlockTrust::Untrusted,
                    text: "remember".into(),
                }],
                ..Default::default()
            })
        }
    }

    fn log_with_two_runs() -> (tempfile::TempDir, ThreadLogSource) {
        let dir = tempfile::tempdir().unwrap();
        let store = TreeThreadStore::new(dir.path());
        store
            .append(
                "main",
                vec![
                    NewRecord::new(
                        kind::TASK_START,
                        "host",
                        json!({"system_sha256": "s", "tools_sha256": "t"}),
                    ),
                    msg("run-1", "user", "hello"),
                    msg("run-1", "assistant", "hi"),
                    msg("run-2", "user", "again"),
                ],
            )
            .unwrap();
        let source = ThreadLogSource::new(dir.path(), "main");
        (dir, source)
    }

    #[test]
    fn builds_history_runs_and_digests_deterministically() {
        let (_dir, source) = log_with_two_runs();
        let sources: Vec<Box<dyn ContextSource>> = vec![Box::new(source), Box::new(Fake)];
        let a = build_context("run-3", &sources).unwrap();
        let b = build_context("run-3", &sources).unwrap();
        assert_eq!(a.digest, b.digest);
        assert_eq!(a.digest, sha256_hex(&a.bytes));
        assert_eq!(a.context.messages.len(), 3);
        assert_eq!(a.context.head_seq, 4);
        assert_eq!(a.context.system_sha256.as_deref(), Some("s"));
        assert_eq!(a.runs, ["run-1", "run-2"]);
        assert_eq!(a.context.blocks.len(), 1);
        let texts: Vec<String> = a
            .context
            .placed_messages()
            .into_iter()
            .map(|m| m.content)
            .collect();
        assert_eq!(texts, ["remember", "hello", "hi", "again"]);
    }

    #[test]
    fn an_empty_log_gives_no_messages() {
        let dir = tempfile::tempdir().unwrap();
        let sources: Vec<Box<dyn ContextSource>> =
            vec![Box::new(ThreadLogSource::new(dir.path(), "main"))];
        let built = build_context("r", &sources).unwrap();
        assert!(built.context.messages.is_empty());
    }

    #[test]
    fn a_broken_chain_uses_the_records_before_the_break_and_warns() {
        let (dir, source) = log_with_two_runs();
        let seg = dir.path().join(".vam/threads/main/log/00000001.jsonl");
        let text = std::fs::read_to_string(&seg).unwrap();
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        lines[3] = lines[3].replace("again", "AGAIN");
        std::fs::write(&seg, format!("{}\n", lines.join("\n"))).unwrap();
        let out = source.load().unwrap();
        assert_eq!(out.messages.len(), 2);
        assert!(out.warnings[0].contains("broken at seq 4"));
    }

    #[test]
    fn an_oversize_context_maps_to_too_large_without_writing_megabytes() {
        // The 16 MiB guard itself is tested in vam-storage; here only the mapping.
        assert!(matches!(
            map_assemble_error(ThreadLogError::ContextTooLarge { bytes: 17 << 20 }),
            ContextError::TooLarge(_)
        ));
        assert!(matches!(
            map_assemble_error(ThreadLogError::Corrupt("x".into())),
            ContextError::Source(_)
        ));
    }

    #[test]
    fn written_context_is_outside_the_tree_and_reads_back_in_chunks() {
        let (_dir, source) = log_with_two_runs();
        let sources: Vec<Box<dyn ContextSource>> = vec![Box::new(source)];
        let built = build_context("r", &sources).unwrap();
        let root = tempfile::tempdir().unwrap();
        let layout = Drive9Layout::new(root.path());
        let path = write_context(&layout, "sess-1", &built).unwrap();
        assert!(!path.starts_with(layout.session_dir("sess-1").join("materialized")));
        let mut got = Vec::new();
        loop {
            let (total, chunk) = read_context_chunk(&path, got.len() as u64, 16).unwrap();
            assert_eq!(total as usize, built.bytes.len());
            if chunk.is_empty() {
                break;
            }
            got.extend(chunk);
            if got.len() as u64 == total {
                break;
            }
        }
        assert_eq!(got, built.bytes);
        remove_context(&layout, "sess-1", &built.digest);
        assert!(!path.exists());
    }

    #[test]
    fn context_paths_reject_bad_digests_and_sessions() {
        let layout = Drive9Layout::new("/tmp/x");
        assert!(context_path(&layout, "s", "../../etc/passwd").is_err());
        assert!(context_path(&layout, "s", &"A".repeat(64)).is_err());
        assert!(context_path(&layout, "../s", &"a".repeat(64)).is_err());
        assert!(context_path(&layout, "s", &"a".repeat(64)).is_ok());
    }
}
