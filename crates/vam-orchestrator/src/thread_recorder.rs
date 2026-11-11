//! The host's recorder of a thread's conversation (decision 23).
//!
//! One recorder per session, fed by the port-53 inference broker (every request
//! carries the whole message list, every response the assistant message) and by
//! the run lifecycle (`task_start`, `task_end`). It appends to the thread log in
//! the session's materialized tree, `.vam/threads/<thread>/log/`, which the
//! session-end commit keeps with the files.
//!
//! * **Scope is host-derived.** The workspace and thread come from the session
//!   row and the run spec ([`crate::run_checkpoint::thread_scope_for_session`]),
//!   resolved on first use and cached once they succeed. Nothing in a request
//!   (message text, tool names) selects a path.
//! * **Request vs log.** The request's non-system messages are matched to the
//!   recorded context by an exact digest of each message. A pure extension
//!   appends the new tail. Any divergence appends `rollback{prefix_mismatch}`
//!   and then the messages after the matching prefix: nothing is lost, nothing
//!   fails. The system prompt and the tool definitions go to the host prompt
//!   store by digest (outside `.vam/`); the log keeps digests only.
//! * **Harness notes.** A request labelled `Lane::Note` carries a todo
//!   recitation as its last message. The host accepts the label only when the
//!   run it belongs to has `context.reciteTodo` on (read from that run's stored
//!   spec for each request, never cached) and the last message has the exact
//!   shape of a note. An accepted note is cut from the request before matching
//!   (no `rollback`) and appended to the `note` lane (parent `main`) when it
//!   differs from the thread's last note; each request saw the latest note
//!   recorded before it. A refused label is a `gap` on the note lane and the
//!   request is recorded as if unlabelled.
//! * **Resamples.** A request labelled `reason = "resample"` is the same request again
//!   after an invalid tool call, without the reply it replaces. The host accepts the
//!   label only when the run's stored harness has `adapter.resampleOnInvalid` and the
//!   request is the recorded context minus its last reply. An accepted one appends
//!   `rollback{reason: "resample", dropped: 1, discarded_seq}` (the discarded reply
//!   stays in the log; no `prefix_mismatch`, not counted as a rollback). A refused
//!   label is a `gap` on the `resample` lane and the request is recorded as if
//!   unlabelled.
//! * **Ordered appends.** All appends of a thread take one lock and each batch is
//!   written and fsynced under it, so concurrent broker requests never
//!   interleave a batch or tear a line.
//! * **Failure never fails the task.** An error is logged and counted and a
//!   `gap` record is attempted (or carried to the next append). Every request
//!   carries the full list, so what a failure missed is re-derived from the next
//!   request.
//! * **Sealed after `task_end`.** A late request is dropped and counted.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use vam_inference::{ExchangeObserver, ExchangeRefusal, ExchangeRequest, ExchangeResponse};
use vam_spec::compaction as cmp;
use vam_spec::thread_context::tools_blob;
use vam_storage::log_quota::{LogQuota, LogUsage};
use vam_storage::prompt_store::{PromptKind, PromptStore, PromptStoreError};
use vam_storage::thread_log::{
    assemble_context, kind, message_body, wire_sha256, Integrity, LogHead, LogMessage, NewRecord,
    Part, ThreadLogError, ThreadLogReader, ThreadStore, TreeThreadStore, WireMessage, MAIN_LANE,
    MAX_CONTEXT_BYTES,
};
use vam_storage::{sha256_hex, Drive9Layout};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// Where a session's run writes, as the host knows it.
#[derive(Clone)]
pub struct RecorderScope {
    pub workspace_id: String,
    pub thread_ref: String,
    pub run_id: String,
    pub agent_urn: String,
    pub prompts: PromptStore,
}

/// Whether the run now bound to the session has `context.reciteTodo` on, read
/// from that run's stored spec each time it is asked.
pub type NotePolicy = Arc<dyn Fn() -> bool + Send + Sync>;

type ScopeResolver = Arc<dyn Fn() -> Result<RecorderScope, String> + Send + Sync>;

/// Counters of what the recorder did and did not manage to do.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RecorderStats {
    pub failures: u64,
    /// Requests or responses seen with no resolvable scope.
    pub unscoped: u64,
    /// Requests or responses that arrived after `task_end`.
    pub dropped_after_end: u64,
    /// `rollback{prefix_mismatch}` records written.
    pub rollbacks: u64,
}

#[derive(Default)]
struct State {
    loaded: bool,
    /// `(seq, msg_sha256)` of the messages the model would see next.
    context: Vec<(u64, String)>,
    head: Option<LogHead>,
    system_sha: Option<String>,
    tools_sha: Option<String>,
    /// A `gap` that could not be written yet: `(reason, missed)`.
    carried_gap: Option<(String, u64)>,
    /// Digest of the suspend tail last accepted, so a retried upload is a no-op.
    last_suspend_tail: Option<String>,
    /// The last accepted suspend while nothing was logged after it: a task that resumed
    /// and parked again on the same call (a reconnect, then the tool's own ask) re-suspends
    /// without repeating results the log already holds.
    open_suspend: Option<OpenSuspend>,
    /// Set when a log limit was hit: every later request is refused at once.
    refusing: Option<String>,
    /// Failed writes in a row; reset by a good one.
    consecutive_failures: u32,
    /// Seq of the latest compaction record the context starts from.
    compaction_seq: Option<u64>,
    /// Digest of the last message on the thread's `note` lane.
    last_note: Option<String>,
}

struct OpenSuspend {
    pending_call_id: String,
    context_len: usize,
    unanswered: Vec<String>,
}

/// What the host verified about a declared summary exchange, kept until its
/// response arrives. Everything here is derived from the host's own record.
struct SummaryPending {
    /// Seq of the first kept message, read from the recorded context.
    keep_from_seq: u64,
    /// Index of that message in the recorded context.
    keep_index: usize,
    reason: String,
    /// User messages of the summarised part (all the summary message reads).
    summarised_users: Vec<vam_spec::v1::ChatMessage>,
    kept_bytes: usize,
    /// Bytes of the whole summary request (system, tools, messages).
    request_bytes: usize,
    fixed_bytes: usize,
    window: i32,
}

/// Recorder failures in a row after which requests are refused (Q27): a disk
/// that cannot take three writes in a row is broken, and a task that cannot be
/// recorded must not go on.
pub const MAX_CONSECUTIVE_RECORDER_FAILURES: u32 = 3;

pub const REFUSAL_QUOTA: &str = "thread_log_quota_exceeded";
pub const REFUSAL_UNAVAILABLE: &str = "thread_log_unavailable";
/// The request is over the 16 MiB context guard.
pub const REFUSAL_CONTEXT_TOO_LARGE: &str = "context_too_large";
/// A declared summary exchange the host could not verify against its record.
pub const REFUSAL_COMPACTION: &str = "compaction_refused";
/// A declared recitation note the host could not accept; the request is
/// recorded as if it carried no label.
pub const REFUSAL_NOTE: &str = "note_refused";
/// A resample label the host could not accept (the run's harness has the switch off, or
/// the request is not the recorded context minus the discarded reply). Recorded as a gap
/// on the `resample` lane; the request is then recorded as if unlabelled.
pub const REFUSAL_RESAMPLE: &str = "resample_refused";

/// Whether the run now bound to `session_id` has `context.reciteTodo` on, from
/// the host record in that run's stored spec. Any failure to read it is `false`.
/// A session can serve several runs (follow-ups, resumes), each with its own
/// stored harness, so this is read for every request and never cached.
pub fn session_recites_todo(store: &SqliteWorkflowStore, session_id: &str) -> bool {
    session_harness(store, session_id)
        .map(|h| h.context.recite_todo)
        .unwrap_or(false)
}

/// Whether the run now bound to `session_id` has `adapter.resampleOnInvalid` on. Any failure
/// to read it is `false`. Read for every request and never cached.
pub fn session_resamples(store: &SqliteWorkflowStore, session_id: &str) -> bool {
    session_harness(store, session_id)
        .map(|h| h.adapter.resample_on_invalid > 0)
        .unwrap_or(false)
}

/// The harness of the run now bound to `session_id`, from the host record in that
/// run's stored spec. `None` on any failure to read it. Read for every request and
/// never cached (see [`session_recites_todo`]).
pub fn session_harness(
    store: &SqliteWorkflowStore,
    session_id: &str,
) -> Option<vam_spec::harness::Harness> {
    let run_id = store.find_run_id_for_session(session_id).ok().flatten()?;
    let (json, _, _) = store.get_run_spec_context(&run_id).ok()?;
    let spec = vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &json?)
        .ok()?;
    vam_spec::harness::Harness::from_inputs(spec.inputs.as_ref()).ok()
}

pub struct ThreadRecorder {
    session_id: String,
    tree: Arc<TreeThreadStore>,
    resolver: ScopeResolver,
    note_policy: NotePolicy,
    resample_policy: NotePolicy,
    scope: Mutex<Option<RecorderScope>>,
    state: tokio::sync::Mutex<State>,
    /// `exchange_id -> round`, for pairing a response with its request.
    pending: Mutex<HashMap<u64, u64>>,
    /// `exchange_id -> prefix_stable`: the request extended the recorded
    /// context with the same system prompt and tools (a cache-read bench input).
    stable: Mutex<HashMap<u64, bool>>,
    /// Verified summary exchanges awaiting their response, by exchange id.
    summaries: Mutex<HashMap<u64, SummaryPending>>,
    round: AtomicU64,
    sealed: AtomicBool,
    /// The user stopped a task parked on an approval: the calls it never ran are answered
    /// "Not run: stopped by user" (AR-3.7), not with the cut-off text.
    user_stop: AtomicBool,
    failures: AtomicU64,
    unscoped: AtomicU64,
    dropped: AtomicU64,
    rollbacks: AtomicU64,
    /// Seq of this run's first `task_start` (0 = none yet).
    run_first_seq: AtomicU64,
    /// Turn-commit index the recorder records boundaries in (none in the
    /// recorder's own unit tests).
    commits: Option<Arc<SqliteWorkflowStore>>,
    /// Size limits of the log and prompt store, and the shared byte count.
    quota: Option<(LogQuota, LogUsage)>,
    /// The workspace HEAD and files hash the task started from (`task_start`).
    heads_before: Mutex<Option<(String, String)>>,
    /// The `commit` record the finalizer fixed for the end of the task.
    end_commit: Mutex<Option<Value>>,
}

impl std::fmt::Debug for ThreadRecorder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadRecorder")
            .field("session_id", &self.session_id)
            .finish_non_exhaustive()
    }
}

fn to_wire(m: &vam_spec::v1::ChatMessage) -> WireMessage {
    WireMessage {
        role: m.role.clone(),
        content: m.content.clone(),
        tool_calls: m
            .tool_calls
            .iter()
            .map(|c| (c.id.clone(), c.name.clone(), c.arguments.clone()))
            .collect(),
        tool_call_id: m.tool_call_id.clone(),
    }
}

/// The request message in the spec's wire shape (what the guest hashes).
fn spec_wire(m: &vam_spec::v1::ChatMessage) -> vam_spec::thread_context::WireMessage {
    vam_spec::thread_context::WireMessage {
        role: m.role.clone(),
        content: m.content.clone(),
        tool_calls: m
            .tool_calls
            .iter()
            .map(|c| vam_spec::thread_context::WireToolCall {
                id: c.id.clone(),
                name: c.name.clone(),
                arguments: c.arguments.clone(),
            })
            .collect(),
        tool_call_id: m.tool_call_id.clone(),
    }
}

/// The tool results first seen in this request (`rest[start..]`) as (tool name, call id, text).
/// The name comes from the assistant message that made the call.
fn new_tool_results(
    rest: &[vam_spec::v1::ChatMessage],
    start: usize,
) -> Vec<(String, String, String)> {
    rest.iter()
        .skip(start)
        .filter(|m| m.role == "tool")
        .filter_map(|m| {
            let name = rest.iter().find_map(|a| {
                a.tool_calls
                    .iter()
                    .find(|c| c.id == m.tool_call_id)
                    .map(|c| c.name.clone())
            })?;
            Some((name, m.tool_call_id.clone(), m.content.clone()))
        })
        .collect()
}

impl ThreadRecorder {
    /// A recorder for `session_id` whose scope comes from the session and run
    /// rows. `layout` locates the session tree and the prompt store.
    pub fn new(session_id: String, store: Arc<SqliteWorkflowStore>, layout: Drive9Layout) -> Self {
        let tree_root = layout.session_dir(&session_id).join("materialized");
        let resolver_session = session_id.clone();
        let commit_store = store.clone();
        let scan_layout_source = layout.clone();
        let resolver: ScopeResolver = Arc::new(move || {
            let run_id = store
                .find_run_id_for_session(&resolver_session)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "session has no run yet".to_string())?;
            let (workspace_id, thread_ref) =
                crate::run_checkpoint::thread_scope_for_session(&store, &resolver_session, &run_id)
                    .map_err(|e: OrchestratorError| e.to_string())?;
            let agent_urn = store
                .get_run_spec_context(&run_id)
                .ok()
                .and_then(|(_, _, agent)| agent)
                .unwrap_or_default();
            let prompts = PromptStore::new(&layout, &workspace_id).map_err(|e| e.to_string())?;
            Ok(RecorderScope {
                workspace_id,
                thread_ref,
                run_id,
                agent_urn,
                prompts,
            })
        });
        let usage = {
            let (scan_store, scan_session, scan_layout, scan_root) = (
                commit_store.clone(),
                session_id.clone(),
                scan_layout_source,
                tree_root.clone(),
            );
            LogUsage::new(move || {
                let logs = TreeThreadStore::new(&scan_root).log_bytes();
                let prompts = scan_store
                    .find_run_id_for_session(&scan_session)
                    .ok()
                    .flatten()
                    .and_then(|run| {
                        crate::run_checkpoint::thread_scope_for_session(
                            &scan_store,
                            &scan_session,
                            &run,
                        )
                        .ok()
                    })
                    .and_then(|(workspace, _)| PromptStore::new(&scan_layout, &workspace).ok())
                    .map(|p| p.bytes_used())
                    .unwrap_or(0);
                logs + prompts
            })
        };
        let policy_store = commit_store.clone();
        let policy_session = session_id.clone();
        let note_policy: NotePolicy =
            Arc::new(move || session_recites_todo(&policy_store, &policy_session));
        let resample_store = commit_store.clone();
        let resample_session = session_id.clone();
        let resample_policy: NotePolicy =
            Arc::new(move || session_resamples(&resample_store, &resample_session));
        Self::with_resolver(session_id, tree_root, resolver)
            .with_note_policy(note_policy)
            .with_resample_policy(resample_policy)
            .with_commit_store(commit_store)
            .with_quota(LogQuota::default(), usage)
    }

    /// Where the recorder learns whether the run it records has the todo
    /// recitation on. Without one, a note label is always refused.
    pub fn with_note_policy(mut self, policy: NotePolicy) -> Self {
        self.note_policy = policy;
        self
    }

    /// Where the recorder learns whether the run it records has `adapter.resampleOnInvalid`
    /// on. Without one, a resample label is always refused.
    pub fn with_resample_policy(mut self, policy: NotePolicy) -> Self {
        self.resample_policy = policy;
        self
    }

    /// Lets the recorder complete turn commits: when a request carries the
    /// tool-result tail a commit announced, its log boundary is recorded.
    pub fn with_commit_store(mut self, store: Arc<SqliteWorkflowStore>) -> Self {
        self.commits = Some(store);
        self
    }

    /// The task has ended (`task_end` written, or a stop recorded): late
    /// requests are dropped.
    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::SeqCst)
    }

    /// Model requests this recorder has seen in the task so far.
    pub fn rounds(&self) -> u64 {
        self.round.load(Ordering::SeqCst)
    }

    /// A recorder with its own scope source (the tree root is the session's
    /// `materialized` directory).
    pub fn with_resolver(
        session_id: String,
        tree_root: std::path::PathBuf,
        resolver: ScopeResolver,
    ) -> Self {
        Self {
            session_id,
            tree: Arc::new(TreeThreadStore::new(tree_root)),
            resolver,
            note_policy: Arc::new(|| false),
            resample_policy: Arc::new(|| false),
            scope: Mutex::new(None),
            state: tokio::sync::Mutex::new(State::default()),
            pending: Mutex::new(HashMap::new()),
            stable: Mutex::new(HashMap::new()),
            summaries: Mutex::new(HashMap::new()),
            round: AtomicU64::new(0),
            sealed: AtomicBool::new(false),
            user_stop: AtomicBool::new(false),
            failures: AtomicU64::new(0),
            unscoped: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            rollbacks: AtomicU64::new(0),
            run_first_seq: AtomicU64::new(0),
            commits: None,
            quota: None,
            heads_before: Mutex::new(None),
            end_commit: Mutex::new(None),
        }
    }

    /// The workspace the task started from: written into `task_start`.
    pub fn set_heads_before(&self, head_before: &str, files_hash_before: &str) {
        if let Ok(mut slot) = self.heads_before.lock() {
            *slot = Some((head_before.to_string(), files_hash_before.to_string()));
        }
    }

    /// The files the task leaves (their hash, the base they come from, and what
    /// the end commit drops): written as a `commit` record in the batch of the
    /// task's end, before the files are composed into HEAD.
    pub fn set_end_commit(
        &self,
        files_hash: &str,
        base: &str,
        dropped: &vam_storage::DroppedSummary,
    ) {
        let body = json!({
            "files_hash": files_hash,
            "base": base,
            "dropped_count": dropped.count,
            "dropped": dropped
                .entries
                .iter()
                .map(|e| json!({"path": e.path, "size": e.size}))
                .collect::<Vec<_>>(),
        });
        if let Ok(mut slot) = self.end_commit.lock() {
            *slot = Some(body);
        }
    }

    fn end_commit_record(&self, scope: &RecorderScope, round: u64) -> Option<NewRecord> {
        let body = self.end_commit.lock().ok()?.take()?;
        Some(Self::record(scope, kind::COMMIT, "host", round, body))
    }

    /// Bounds the thread log and the prompt store this recorder writes (see
    /// `vam_storage::log_quota`). `usage` is the workspace's shared byte count.
    pub fn with_quota(mut self, quota: LogQuota, usage: LogUsage) -> Self {
        let root = self.tree.root().to_path_buf();
        self.tree = Arc::new(TreeThreadStore::new(root).with_quota(quota, usage.clone()));
        self.quota = Some((quota, usage));
        self
    }

    pub fn stats(&self) -> RecorderStats {
        RecorderStats {
            failures: self.failures.load(Ordering::Relaxed),
            unscoped: self.unscoped.load(Ordering::Relaxed),
            dropped_after_end: self.dropped.load(Ordering::Relaxed),
            rollbacks: self.rollbacks.load(Ordering::Relaxed),
        }
    }

    /// Seq of the run's first `task_start`, the start of its log range.
    pub fn run_first_seq(&self) -> Option<u64> {
        match self.run_first_seq.load(Ordering::SeqCst) {
            0 => None,
            n => Some(n),
        }
    }

    /// The log head this recorder last wrote, without waiting for a write in
    /// progress. `None` when nothing was recorded yet or a write is running.
    pub fn cached_head(&self) -> Option<LogHead> {
        self.state.try_lock().ok().and_then(|st| st.head.clone())
    }

    /// The scope, resolved on first use and cached once it succeeds.
    fn scope(&self) -> Option<RecorderScope> {
        let mut cached = self.scope.lock().ok()?;
        if let Some(scope) = cached.as_ref() {
            return Some(scope.clone());
        }
        match (self.resolver)() {
            Ok(mut scope) => {
                if let Some((quota, usage)) = &self.quota {
                    scope.prompts = scope.prompts.clone().with_quota(*quota, usage.clone());
                }
                *cached = Some(scope.clone());
                Some(scope)
            }
            Err(reason) => {
                self.unscoped.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(session = %self.session_id, %reason, "thread recorder has no scope");
                None
            }
        }
    }

    async fn append(
        &self,
        scope: &RecorderScope,
        batch: Vec<NewRecord>,
    ) -> Result<vam_storage::thread_log::AppendReceipt, ThreadLogError> {
        let tree = self.tree.clone();
        let thread = scope.thread_ref.clone();
        tokio::task::spawn_blocking(move || tree.append(&thread, batch))
            .await
            .map_err(|e| ThreadLogError::Corrupt(format!("append task failed: {e}")))?
    }

    fn record(
        scope: &RecorderScope,
        kind_name: &str,
        src: &str,
        round: u64,
        body: Value,
    ) -> NewRecord {
        let mut rec = NewRecord::new(kind_name, src, body);
        rec.run = scope.run_id.clone();
        rec.agent = scope.agent_urn.clone();
        rec.round = round;
        rec
    }

    /// Reads the thread's recorded context once; later calls reuse it.
    async fn load(&self, scope: &RecorderScope, st: &mut State) -> Result<(), String> {
        if st.loaded {
            return Ok(());
        }
        let tree = self.tree.clone();
        let thread = scope.thread_ref.clone();
        let assembled = tokio::task::spawn_blocking(move || {
            let mut records = Vec::new();
            let mut after = 0;
            loop {
                let page = tree.read(&thread, after, 500)?;
                if let Integrity::BrokenAt { seq, reason } = &page.integrity {
                    return Err(ThreadLogError::Corrupt(format!(
                        "log_broken at {seq}: {reason}"
                    )));
                }
                if page.records.is_empty() {
                    break;
                }
                after = page.next_after;
                records.extend(page.records);
            }
            let last_note = records
                .iter()
                .rev()
                .find(|r| r.lane == cmp::NOTE_LANE && r.kind == kind::MESSAGE)
                .and_then(|r| r.message())
                .map(|m| m.msg_sha256());
            assemble_context(&records, MAIN_LANE).map(|ctx| (ctx, last_note))
        })
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
        let (assembled, last_note) = assembled;
        st.last_note = last_note;
        st.context = assembled
            .seqs
            .iter()
            .zip(&assembled.messages)
            .map(|(seq, m)| (*seq, m.msg_sha256()))
            .collect();
        st.head = assembled.head;
        st.compaction_seq = assembled.compaction_seq;
        st.system_sha = assembled.system_sha256;
        st.tools_sha = assembled.tools_sha256;
        st.loaded = true;
        Ok(())
    }

    /// Counts a failure, forgets the in-memory view (disk is the truth), and
    /// tries to leave a `gap` so readers see the log is incomplete. If the gap
    /// cannot be written either, it is carried to the next append.
    async fn note_failure(
        &self,
        scope: &RecorderScope,
        st: &mut State,
        reason: &str,
        detail: &str,
    ) {
        self.failures.fetch_add(1, Ordering::Relaxed);
        st.consecutive_failures += 1;
        tracing::warn!(session = %self.session_id, %reason, %detail, "thread recorder failed");
        st.loaded = false;
        let missed = st.carried_gap.take().map(|(_, m)| m).unwrap_or(0) + 1;
        if reason == "log_broken" {
            // Never write onto a broken chain.
            st.carried_gap = Some((reason.to_string(), missed));
            return;
        }
        let gap = Self::record(
            scope,
            kind::GAP,
            "host",
            self.round.load(Ordering::Relaxed),
            json!({"reason": reason, "missed": missed, "detail": detail}),
        );
        match self.append(scope, vec![gap]).await {
            Ok(_) => st.carried_gap = None,
            Err(_) => st.carried_gap = Some((reason.to_string(), missed)),
        }
    }

    /// A log limit was hit. Writes `gap{reason: quota}` from the reserve (so
    /// the log says it is full), remembers it so every later request is
    /// refused at once, and returns the refusal for this one.
    async fn note_quota(
        &self,
        scope: &RecorderScope,
        st: &mut State,
        which: &str,
        limit: u64,
        used: u64,
    ) -> ExchangeRefusal {
        self.failures.fetch_add(1, Ordering::Relaxed);
        let message = format!(
            "the {which} log is full ({used} of {limit} bytes); start a new thread to go on"
        );
        tracing::warn!(session = %self.session_id, %which, limit, used, "thread log full");
        if st.refusing.is_none() {
            let missed = st.carried_gap.take().map(|(_, m)| m).unwrap_or(0) + 1;
            let gap = Self::record(
                scope,
                kind::GAP,
                "host",
                self.round.load(Ordering::Relaxed),
                json!({
                    "reason": "quota",
                    "scope": which,
                    "limit": limit,
                    "used": used,
                    "missed": missed,
                }),
            );
            if let Err(e) = self.append(scope, vec![gap]).await {
                tracing::error!(session = %self.session_id, "could not record the quota gap: {e}");
            }
            st.refusing = Some(message.clone());
        }
        ExchangeRefusal {
            reason: REFUSAL_QUOTA.to_string(),
            message,
        }
    }

    /// Writes the `task_start` record. A resume (`resumed`) continues the run's
    /// round numbering; a new task restarts it.
    pub async fn record_task_start(
        &self,
        prompt: &str,
        model: &str,
        agent_digest: &str,
        resumed: bool,
    ) {
        self.sealed.store(false, Ordering::SeqCst);
        if !resumed {
            self.round.store(0, Ordering::SeqCst);
        }
        let Some(scope) = self.scope() else {
            return;
        };
        let mut st = self.state.lock().await;
        if let Err(e) = self.load(&scope, &mut st).await {
            let reason = if e.contains("log_broken") {
                "log_broken"
            } else {
                "recorder_error"
            };
            self.note_failure(&scope, &mut st, reason, &e).await;
            return;
        }
        let mut start_body = json!({
            "prompt": prompt,
            "model": model,
            "agent_digest": agent_digest,
            "resumed": resumed,
        });
        if !resumed {
            let heads = self.heads_before.lock().ok().and_then(|h| h.clone());
            if let (Some((head, files)), Some(map)) = (heads, start_body.as_object_mut()) {
                map.insert(
                    "workspace".to_string(),
                    json!({"head_before": head, "files_hash_before": files}),
                );
            }
        }
        let rec = Self::record(&scope, kind::TASK_START, "host", 0, start_body);
        let batch = self.with_carried_gap(&scope, &mut st, vec![rec]);
        let start_index = batch.len() as u64 - 1;
        match self.append(&scope, batch).await {
            Ok(receipt) => {
                if !resumed || self.run_first_seq.load(Ordering::SeqCst) == 0 {
                    self.run_first_seq
                        .store(receipt.first_seq + start_index, Ordering::SeqCst);
                }
                st.head = Some(receipt.head)
            }
            Err(e) => {
                self.note_failure(&scope, &mut st, "recorder_error", &e.to_string())
                    .await
            }
        }
    }

    /// A recorder for a session that takes over a task after a crash. The model request that
    /// produced the decided round was seen by the earlier session, so the round count starts at
    /// one when there is such a round (a commit of its calls may be asked before any new
    /// request), and the run's first `task_start` is the one in the log.
    pub async fn seed_for_resume(&self, dangling_round: bool) {
        if dangling_round {
            self.round.store(1, Ordering::SeqCst);
        }
        if let Some(first) = self.first_task_start_seq().await {
            self.run_first_seq.store(first, Ordering::SeqCst);
        }
    }

    /// Seq of the run's first `task_start` in the thread log.
    async fn first_task_start_seq(&self) -> Option<u64> {
        let scope = self.scope()?;
        let tree = self.tree.clone();
        let thread = scope.thread_ref.clone();
        let run = scope.run_id.clone();
        tokio::task::spawn_blocking(move || tree.read(&thread, 0, usize::MAX))
            .await
            .ok()
            .and_then(|page| page.ok())
            .and_then(|page| {
                page.records
                    .iter()
                    .filter(|r| r.kind == kind::TASK_START && r.run == run)
                    .map(|r| r.seq)
                    .min()
            })
    }

    /// Whether the thread log holds this run's `task_start`: the task began.
    pub async fn run_has_task_start(&self) -> bool {
        self.first_task_start_seq().await.is_some()
    }

    /// Writes `task_end` (with the head it follows), seals the recorder, and
    /// returns the log head after it, for the attestation.
    pub async fn record_task_end(
        &self,
        outcome: &str,
        answer: &str,
        reason: &str,
    ) -> Option<LogHead> {
        let result = self.record_task_end_inner(outcome, answer, reason).await;
        self.sealed.store(true, Ordering::SeqCst);
        result
    }

    async fn record_task_end_inner(
        &self,
        outcome: &str,
        answer: &str,
        reason: &str,
    ) -> Option<LogHead> {
        let scope = self.scope()?;
        let mut st = self.state.lock().await;
        if let Err(e) = self.load(&scope, &mut st).await {
            let why = if e.contains("log_broken") {
                "log_broken"
            } else {
                "recorder_error"
            };
            self.note_failure(&scope, &mut st, why, &e).await;
            return None;
        }
        let head_before = st
            .head
            .as_ref()
            .map(|h| json!({"seq": h.seq, "digest": h.digest}))
            .unwrap_or(Value::Null);
        let rec = Self::record(
            &scope,
            kind::TASK_END,
            "host",
            self.round.load(Ordering::Relaxed),
            json!({
                "outcome": outcome,
                "answer": answer,
                "reason": reason,
                "head_before": head_before,
            }),
        );
        let mut records = Vec::new();
        if let Some(commit) = self.end_commit_record(&scope, self.round.load(Ordering::Relaxed)) {
            records.push(commit);
        }
        records.push(rec);
        match self
            .append(&scope, self.with_carried_gap(&scope, &mut st, records))
            .await
        {
            Ok(receipt) => {
                st.head = Some(receipt.head.clone());
                Some(receipt.head)
            }
            Err(e) => {
                self.note_failure(&scope, &mut st, "recorder_error", &e.to_string())
                    .await;
                None
            }
        }
    }

    /// Puts a pending carried `gap` in front of a batch.
    fn with_carried_gap(
        &self,
        scope: &RecorderScope,
        st: &mut State,
        mut batch: Vec<NewRecord>,
    ) -> Vec<NewRecord> {
        if let Some((reason, missed)) = st.carried_gap.clone() {
            batch.insert(
                0,
                Self::record(
                    scope,
                    kind::GAP,
                    "host",
                    self.round.load(Ordering::Relaxed),
                    json!({"reason": reason, "missed": missed}),
                ),
            );
        }
        batch
    }

    /// Records a request. `Err` refuses it: the thread log is full, or the disk
    /// has failed [`MAX_CONSECUTIVE_RECORDER_FAILURES`] writes in a row. Any
    /// other single failure stays a `gap` (the next request carries the whole
    /// list and re-derives what was missed).
    /// The skill's `show` card for a tool the guest ran (CN-3.4): at the first sight of its
    /// result in the thread. Host-run tools were handled where they ran (`catalog.command ==
    /// "host"`), and a call is only ever carded once. The result is the guest's word, like `show`.
    fn attach_guest_tool_cards(&self, rest: &[vam_spec::v1::ChatMessage], start: usize) {
        let Some(store) = self.commits.as_ref() else {
            return;
        };
        let mut run_id: Option<Option<String>> = None;
        for (name, call_id, content) in new_tool_results(rest, start) {
            if crate::skill_cards::cards_for(&self.session_id, &name).is_none() {
                continue;
            }
            match store.get_tool(&self.session_id, &name) {
                Ok(Some(entry)) if entry.command != "host" && entry.command != "builtin" => {}
                _ => continue,
            }
            let run = run_id.get_or_insert_with(|| {
                store
                    .find_run_id_for_session(&self.session_id)
                    .ok()
                    .flatten()
            });
            if let Some(run) = run {
                crate::outbox::attach_show_card(
                    store,
                    &self.session_id,
                    run,
                    &name,
                    &call_id,
                    &content,
                );
            }
        }
    }

    async fn record_request(&self, req: ExchangeRequest) -> Result<(), ExchangeRefusal> {
        if self.sealed.load(Ordering::SeqCst) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return Ok(());
        }
        let Some(scope) = self.scope() else {
            return Ok(());
        };
        let mut st = self.state.lock().await;
        if let Some(message) = st.refusing.clone() {
            return Err(ExchangeRefusal {
                reason: REFUSAL_QUOTA.to_string(),
                message,
            });
        }
        if st.consecutive_failures >= MAX_CONSECUTIVE_RECORDER_FAILURES {
            return Err(Self::unavailable());
        }
        if let Err(e) = self.load(&scope, &mut st).await {
            let reason = if e.contains("log_broken") {
                "log_broken"
            } else {
                "recorder_error"
            };
            self.note_failure(&scope, &mut st, reason, &e).await;
            return self.after_failure(&st);
        }
        // The 16 MiB context guard. The guest compacts well before it; this is
        // the host's own limit, whatever the guest believes.
        let context_bytes = cmp::approx_bytes_all(&req.messages);
        if context_bytes > MAX_CONTEXT_BYTES {
            return Err(ExchangeRefusal {
                reason: REFUSAL_CONTEXT_TOO_LARGE.to_string(),
                message: format!(
                    "the conversation is {context_bytes} bytes, over the {MAX_CONTEXT_BYTES} \
                     byte limit; it could not be compacted enough"
                ),
            });
        }

        let (system, all_rest) = match req.messages.first() {
            Some(m) if m.role == "system" => (Some(m.content.clone()), &req.messages[1..]),
            _ => (None, &req.messages[..]),
        };
        let system_len = system.as_ref().map_or(0, String::len);
        let tools_len = tools_blob(&req.tools).map_or(0, |b| b.len());

        // A declared summary exchange is verified against the recorded context
        // before anything is written. The summary exchange never rewrites the
        // main lane: only a pure extension of it is appended.
        let hint = req
            .context_hint
            .as_ref()
            .filter(|h| h.lane == vam_spec::v1::context_hint::Lane::Summary as i32);
        let mut summary_plan: Option<SummaryPending> = None;
        // A declared todo recitation note: checked against the run's stored
        // harness and the note's shape. A refused label is a gap on the note
        // lane; the request is then recorded as if it had no label.
        let mut note: Option<&vam_spec::v1::ChatMessage> = None;
        if req
            .context_hint
            .as_ref()
            .is_some_and(|h| h.lane == vam_spec::v1::context_hint::Lane::Note as i32)
        {
            match self.check_note_hint(all_rest) {
                Ok(()) => note = all_rest.last(),
                Err(why) => {
                    let round = self.round.load(Ordering::Relaxed);
                    let mut gap = Self::record(
                        &scope,
                        kind::GAP,
                        "host",
                        round,
                        json!({"reason": REFUSAL_NOTE, "detail": why, "missed": 0}),
                    );
                    gap.lane = cmp::NOTE_LANE.to_string();
                    gap.parent_lane = Some(MAIN_LANE.to_string());
                    let _ = self.append(&scope, vec![gap]).await;
                }
            }
        }
        // A declared resample: the same request again, without the reply it replaces. The
        // run's own harness must have the switch; the shape is checked after matching.
        let resample_declared = cmp::is_resample_hint(req.context_hint.as_ref())
            && req.context_hint.as_ref().is_some_and(|h| {
                h.lane == vam_spec::v1::context_hint::Lane::Main as i32
                    || h.lane == vam_spec::v1::context_hint::Lane::Note as i32
            });
        let resample_allowed = resample_declared && (self.resample_policy)();
        let all_rest: &[vam_spec::v1::ChatMessage] = match note {
            Some(_) => &all_rest[..all_rest.len() - 1],
            None => all_rest,
        };
        let rest: &[vam_spec::v1::ChatMessage] = match hint {
            None => all_rest,
            Some(hint) => match Self::check_summary_request(
                &st,
                hint,
                all_rest,
                system_len + tools_len,
                req.model.context_window,
            ) {
                Ok(plan) => {
                    summary_plan = Some(plan);
                    &all_rest[..all_rest.len() - 1]
                }
                Err(why) => {
                    let round = self.round.load(Ordering::Relaxed);
                    let note = Self::record(
                        &scope,
                        kind::GAP,
                        "host",
                        round,
                        json!({"reason": REFUSAL_COMPACTION, "detail": why, "missed": 0}),
                    );
                    let mut note = note;
                    note.lane = cmp::SUMMARY_LANE.to_string();
                    note.parent_lane = Some(MAIN_LANE.to_string());
                    let _ = self.append(&scope, vec![note]).await;
                    return Err(ExchangeRefusal {
                        reason: REFUSAL_COMPACTION.to_string(),
                        message: why,
                    });
                }
            },
        };

        let round = self.round.fetch_add(1, Ordering::SeqCst) + 1;
        if let Ok(mut pending) = self.pending.lock() {
            pending.insert(req.exchange_id, round);
        }

        // System prompt and tools: stored once by digest, the log keeps digests.
        let system_sha = system.as_ref().map(|s| sha256_hex(s.as_bytes()));
        let blob = tools_blob(&req.tools);
        let tools_sha = blob.as_ref().map(|b| sha256_hex(b.as_bytes()));
        let changed = system_sha != st.system_sha || tools_sha != st.tools_sha;
        if changed {
            let prompts = scope.prompts.clone();
            let (sys, tools) = (system.clone(), blob.clone());
            let stored = tokio::task::spawn_blocking(move || -> Result<(), PromptStoreError> {
                if let Some(sys) = sys {
                    prompts.put(PromptKind::System, sys.as_bytes())?;
                }
                if let Some(tools) = tools {
                    prompts.put(PromptKind::Tools, tools.as_bytes())?;
                }
                Ok(())
            })
            .await;
            match stored {
                Ok(Ok(())) => {}
                Ok(Err(PromptStoreError::QuotaExceeded { limit, used })) => {
                    return Err(self
                        .note_quota(&scope, &mut st, "workspace", limit, used)
                        .await);
                }
                Ok(Err(e)) => {
                    self.note_failure(&scope, &mut st, "prompt_store_error", &e.to_string())
                        .await;
                    return self.after_failure(&st);
                }
                Err(e) => {
                    self.note_failure(&scope, &mut st, "prompt_store_error", &e.to_string())
                        .await;
                    return self.after_failure(&st);
                }
            }
        }

        // Match the request against the recorded context.
        let hashes: Vec<String> = rest.iter().map(|m| wire_sha256(&to_wire(m))).collect();
        let mut k = 0;
        while k < st.context.len() && k < hashes.len() && st.context[k].1 == hashes[k] {
            k += 1;
        }
        let mut batch: Vec<NewRecord> = Vec::new();
        batch = self.with_carried_gap(&scope, &mut st, batch);
        if changed {
            batch.push(Self::record(
                &scope,
                kind::PROMPT_CHANGE,
                "broker",
                round,
                json!({"system_sha256": system_sha, "tools_sha256": tools_sha}),
            ));
        }
        // Accepted only when the request is the recorded context minus its last reply.
        let resample_ok =
            resample_allowed && k == hashes.len() && st.context.len() == k + 1 && k > 0;
        if resample_declared && !resample_ok {
            let why = if !resample_allowed {
                "the run's harness has adapter.resampleOnInvalid off"
            } else {
                "the request is not the recorded context without its last reply"
            };
            let mut gap = Self::record(
                &scope,
                kind::GAP,
                "host",
                round,
                json!({"reason": REFUSAL_RESAMPLE, "detail": why, "missed": 0}),
            );
            gap.lane = cmp::RESAMPLE_LANE.to_string();
            gap.parent_lane = Some(MAIN_LANE.to_string());
            batch.push(gap);
        }
        let diverged = k < st.context.len();
        if let Ok(mut stable) = self.stable.lock() {
            stable.insert(
                req.exchange_id,
                !changed && (!diverged || resample_ok) && !st.context.is_empty(),
            );
        }
        // The head of a compacted context: the summary (at the compaction's
        // seq) and the kept messages (lower seqs). A divergence inside it is a
        // rebase: a fresh compaction takes the request's own start, and older
        // messages stay hidden. A rollback may never reach below the
        // compaction, or the messages it hid would come back.
        let head_len = st.compaction_seq.map_or(0, |c| {
            st.context.iter().take_while(|(seq, _)| *seq <= c).count()
        });
        let rebase = diverged && k < head_len && !rest.is_empty();
        let mut start = k;
        if rebase {
            let message = LogMessage::from_wire(&to_wire(&rest[0]));
            batch.push(Self::record(
                &scope,
                kind::COMPACTION,
                "broker",
                round,
                json!({
                    "summary": message,
                    "reason": cmp::REASON_REBASE,
                    "dropped": st.context.len() - k,
                    "from_compaction": st.compaction_seq,
                }),
            ));
            start = 1;
        } else if diverged {
            let floor = st.compaction_seq.unwrap_or(0);
            let to = if k > 0 {
                st.context[k - 1].0.max(floor)
            } else {
                st.context[0].0.saturating_sub(1).max(floor)
            };
            // A resample drops the reply it replaces; the reply stays in the append-only
            // log, named by `discarded_seq`, so nothing is lost.
            let body = if resample_ok {
                json!({
                    "to": to,
                    "reason": cmp::REASON_RESAMPLE,
                    "dropped": 1,
                    "discarded_seq": st.context[k].0,
                })
            } else {
                json!({
                    "to": to,
                    "reason": "prefix_mismatch",
                    "dropped": st.context.len() - k,
                })
            };
            batch.push(Self::record(&scope, kind::ROLLBACK, "broker", round, body));
        }
        let rebase_sha = rebase.then(|| hashes[0].clone());
        let rebase_index = batch.len().saturating_sub(1);
        let mut added: Vec<(usize, String)> = Vec::new();
        for (i, m) in rest.iter().enumerate().skip(start) {
            let message = LogMessage::from_wire(&to_wire(m));
            added.push((batch.len(), hashes[i].clone()));
            batch.push(Self::record(
                &scope,
                kind::MESSAGE,
                "broker",
                round,
                message_body(&message, None),
            ));
        }
        self.attach_guest_tool_cards(rest, start);
        // The instruction of a verified summary request goes to the summary
        // lane, after the main-lane messages it extends.
        if summary_plan.is_some() {
            if let Some(instruction) = all_rest.last() {
                let mut rec = Self::record(
                    &scope,
                    kind::MESSAGE,
                    "broker",
                    round,
                    message_body(&LogMessage::from_wire(&to_wire(instruction)), None),
                );
                rec.lane = cmp::SUMMARY_LANE.to_string();
                rec.parent_lane = Some(MAIN_LANE.to_string());
                batch.push(rec);
            }
        }
        // The note goes to its own lane when it is new: each request saw the
        // latest note recorded before it.
        let mut new_note: Option<String> = None;
        if let Some(note) = note {
            let sha = wire_sha256(&to_wire(note));
            if st.last_note.as_deref() != Some(sha.as_str()) {
                let mut rec = Self::record(
                    &scope,
                    kind::MESSAGE,
                    "broker",
                    round,
                    message_body(&LogMessage::from_wire(&to_wire(note)), None),
                );
                rec.lane = cmp::NOTE_LANE.to_string();
                rec.parent_lane = Some(MAIN_LANE.to_string());
                batch.push(rec);
                new_note = Some(sha);
            }
        }
        if batch.is_empty() {
            st.system_sha = system_sha;
            st.tools_sha = tools_sha;
            return Ok(());
        }
        let appended_messages = !added.is_empty();
        match self.append(&scope, batch).await {
            Ok(receipt) => {
                if diverged && !resample_ok {
                    self.rollbacks.fetch_add(1, Ordering::Relaxed);
                }
                if let Some(sha) = rebase_sha {
                    let seq = receipt.first_seq + rebase_index as u64;
                    st.context.clear();
                    st.context.push((seq, sha));
                    st.compaction_seq = Some(seq);
                } else {
                    st.context.truncate(k);
                }
                for (index, sha) in added {
                    st.context.push((receipt.first_seq + index as u64, sha));
                }
                if let Some(mut plan) = summary_plan.take() {
                    // The kept message's seq is read from the host's record.
                    if let Some((seq, _)) = st.context.get(plan.keep_index) {
                        plan.keep_from_seq = *seq;
                        if let Ok(mut map) = self.summaries.lock() {
                            map.insert(req.exchange_id, plan);
                        }
                    }
                }
                st.head = Some(receipt.head);
                if new_note.is_some() {
                    st.last_note = new_note;
                }
                st.system_sha = system_sha;
                st.tools_sha = tools_sha;
                st.carried_gap = None;
                st.consecutive_failures = 0;
                if appended_messages {
                    self.note_tool_result_boundary(&scope, &st, rest);
                    self.clear_recorded_results(&scope.run_id, rest);
                }
                Ok(())
            }
            Err(ThreadLogError::QuotaExceeded {
                scope: which,
                limit,
                used,
            }) => Err(self
                .note_quota(&scope, &mut st, which.as_str(), limit, used)
                .await),
            Err(e) => {
                self.note_failure(&scope, &mut st, "recorder_error", &e.to_string())
                    .await;
                self.after_failure(&st)
            }
        }
    }

    /// Verifies a declared recitation note. The run's own stored harness must
    /// have the recitation on, and the last message must have exactly the shape
    /// of a note with at least one other non-system message before it.
    fn check_note_hint(&self, all_rest: &[vam_spec::v1::ChatMessage]) -> Result<(), String> {
        if !(self.note_policy)() {
            return Err("the run's harness has context.reciteTodo off".to_string());
        }
        let Some((last, before)) = all_rest.split_last() else {
            return Err("a note request has a message before the note".to_string());
        };
        if before.is_empty() {
            return Err("a note request has a message before the note".to_string());
        }
        if !cmp::is_recitation_note(last) {
            return Err("the last message is not a todo recitation note".to_string());
        }
        Ok(())
    }

    /// Verifies a declared summary request against the recorded context. Every
    /// number and index the result holds comes from the host's record; the
    /// hint is only checked against it. `Err` carries a reason without paths.
    fn check_summary_request(
        st: &State,
        hint: &vam_spec::v1::ContextHint,
        all_rest: &[vam_spec::v1::ChatMessage],
        fixed_bytes: usize,
        window: i32,
    ) -> Result<SummaryPending, String> {
        if !cmp::is_declarable_reason(&hint.reason) {
            return Err("unknown compaction reason".to_string());
        }
        let Some((instruction, prefix)) = all_rest.split_last() else {
            return Err("a summary request ends with the compaction instruction".to_string());
        };
        if !cmp::is_instruction(instruction) {
            return Err("a summary request ends with the compaction instruction".to_string());
        }
        let idx = hint.keep_from_index as usize;
        // With an earlier compaction, its summary (index 0) is always replaced.
        let first_allowed = usize::from(st.compaction_seq.is_some());
        if idx < first_allowed || idx >= prefix.len() {
            return Err("the kept tail does not start inside the conversation".to_string());
        }
        if prefix[idx].role == "tool" {
            return Err("the kept tail cannot start with a tool result".to_string());
        }
        if cmp::message_digest(&prefix[idx]) != hint.keep_from_msg_sha256 {
            return Err("the kept tail start does not match the message".to_string());
        }
        if prefix.len() < st.context.len()
            || st
                .context
                .iter()
                .zip(prefix)
                .any(|((_, sha), m)| *sha != wire_sha256(&to_wire(m)))
        {
            return Err(
                "the conversation before the summary request does not extend the recorded one"
                    .to_string(),
            );
        }
        Ok(SummaryPending {
            keep_from_seq: 0,
            keep_index: idx,
            reason: hint.reason.clone(),
            summarised_users: prefix[..idx]
                .iter()
                .filter(|m| m.role == "user")
                .cloned()
                .collect(),
            kept_bytes: cmp::approx_bytes_all(&prefix[idx..]),
            request_bytes: fixed_bytes + cmp::approx_bytes_all(all_rest),
            fixed_bytes,
            window,
        })
    }

    /// The response of a verified summary exchange: the reply goes to the
    /// summary lane; a usable reply (text, no tool calls) also writes the
    /// `compaction` record on the main lane, built by the host from the
    /// verified plan, never from guest-claimed numbers.
    async fn record_summary_response(
        &self,
        scope: &RecorderScope,
        st: &mut State,
        resp: ExchangeResponse,
        plan: SummaryPending,
        round: u64,
    ) {
        let tool_calls: Vec<(String, String, String)> = resp
            .tool_calls
            .iter()
            .map(|c| (c.id.clone(), c.name.clone(), c.arguments.clone()))
            .collect();
        let usable = !resp.text.trim().is_empty() && tool_calls.is_empty();
        let reply = LogMessage::from_wire(&WireMessage {
            role: "assistant".to_string(),
            content: resp.text.clone(),
            tool_calls,
            tool_call_id: String::new(),
        });
        let usage = resp.usage.as_ref().map(|u| {
            json!({
                "prompt": u.prompt_tokens,
                "completion": u.completion_tokens,
                "cached": u.cached_tokens,
                "cache_write": u.cache_creation_tokens,
            })
        });
        let mut reply_rec = Self::record(
            scope,
            kind::MESSAGE,
            "broker",
            round,
            message_body(&reply, usage),
        );
        reply_rec.lane = cmp::SUMMARY_LANE.to_string();
        reply_rec.parent_lane = Some(MAIN_LANE.to_string());
        let mut batch = self.with_carried_gap(scope, st, vec![reply_rec]);

        // The recorded context must still be what the request was checked
        // against, or the compaction is not written.
        let still_valid = st
            .context
            .get(plan.keep_index)
            .is_some_and(|(seq, _)| *seq == plan.keep_from_seq);
        let mut compaction: Option<(usize, String, i64, i64)> = None;
        if usable && still_valid {
            let summary = cmp::summary_message(&resp.text, &plan.summarised_users);
            let before = resp
                .usage
                .as_ref()
                .map(|u| i64::from(u.prompt_tokens))
                .filter(|t| *t > 0)
                .unwrap_or((plan.request_bytes / 4) as i64);
            let after =
                ((plan.fixed_bytes + cmp::approx_bytes(&summary) + plan.kept_bytes) / 4) as i64;
            let kept_messages = st.context.len() - plan.keep_index;
            let message = LogMessage::from_wire(&to_wire(&summary));
            let sha = wire_sha256(&to_wire(&summary));
            compaction = Some((batch.len(), sha, before, after));
            batch.push(Self::record(
                scope,
                kind::COMPACTION,
                "broker",
                round,
                json!({
                    "summary": message,
                    "keep_from": plan.keep_from_seq,
                    "tokens_before": before,
                    "tokens_after": after,
                    "tokens_after_estimated": true,
                    "reason": plan.reason,
                    "window": plan.window,
                    "kept_messages": kept_messages,
                }),
            ));
        }
        match self.append(scope, batch).await {
            Ok(receipt) => {
                st.head = Some(receipt.head);
                st.carried_gap = None;
                st.consecutive_failures = 0;
                if let Some((index, sha, before, after)) = compaction {
                    let seq = receipt.first_seq + index as u64;
                    let kept: Vec<(u64, String)> = st.context[plan.keep_index..].to_vec();
                    st.context = std::iter::once((seq, sha)).chain(kept).collect();
                    st.compaction_seq = Some(seq);
                    self.emit_compacted(scope, seq, before, after, &plan);
                }
            }
            Err(ThreadLogError::QuotaExceeded {
                scope: which,
                limit,
                used,
            }) => {
                let _ = self
                    .note_quota(scope, st, which.as_str(), limit, used)
                    .await;
            }
            Err(e) => {
                self.note_failure(scope, st, "recorder_error", &e.to_string())
                    .await
            }
        }
    }

    /// `vam.v1.context.compacted`: after the record is durable.
    fn emit_compacted(
        &self,
        scope: &RecorderScope,
        seq: u64,
        before: i64,
        after: i64,
        plan: &SummaryPending,
    ) {
        let Some(store) = self.commits.as_ref() else {
            return;
        };
        let payload = json!({
            "run": scope.run_id,
            "thread": scope.thread_ref,
            "compactionSeq": seq,
            "keepFromSeq": plan.keep_from_seq,
            "tokensBefore": before,
            "tokensAfter": after,
            "reason": plan.reason,
            "window": plan.window,
        });
        if let Err(e) = store.append_outbox_event(
            &scope.run_id,
            crate::events::TYPE_CONTEXT_COMPACTED,
            &payload.to_string(),
        ) {
            tracing::warn!(session = %self.session_id, "could not emit context.compacted: {e}");
        }
    }

    fn unavailable() -> ExchangeRefusal {
        ExchangeRefusal {
            reason: REFUSAL_UNAVAILABLE.to_string(),
            message: "the host could not record this conversation".to_string(),
        }
    }

    /// After a single recorder failure: a gap, unless it was the last straw.
    fn after_failure(&self, st: &State) -> Result<(), ExchangeRefusal> {
        if st.consecutive_failures >= MAX_CONSECUTIVE_RECORDER_FAILURES {
            Err(Self::unavailable())
        } else {
            Ok(())
        }
    }

    /// The host effects of a stopped run that were sent and have no recorded answer become
    /// `unknown` (nothing is in flight to settle them).
    fn settle_orphaned_sends(&self, run_id: &str) {
        let Some(store) = self.commits.as_ref() else {
            return;
        };
        if crate::effect_path::inflight_count(store, run_id) > 0 {
            return;
        }
        let Ok(rows) = store.effects_for_run(run_id) else {
            return;
        };
        for row in rows
            .rows
            .iter()
            .filter(|r| r.state == crate::effect_store::EffectState::Sent)
        {
            let _ = store.effect_mark_unknown(run_id, &row.call_id);
        }
    }

    /// What the conversation says about a journaled host call that left the VM, or `None` when
    /// the journal has nothing to say (it never left, or it is not a host call).
    fn journaled_outcome_text(&self, run_id: &str, call_id: &str) -> Option<String> {
        use crate::effect_store::EffectState;
        let row = self.commits.as_ref()?.effect_get(run_id, call_id).ok()??;
        match row.state {
            EffectState::Confirmed => Some(
                row.result_bytes
                    .as_deref()
                    .and_then(|bytes| {
                        <vam_spec::v1::ExecuteHostToolResponse as prost::Message>::decode(bytes)
                            .ok()
                    })
                    .map(|response| response.text_result)
                    .unwrap_or_else(|| {
                        format!(
                            "{} finished before the task was stopped; its result is already in                              the conversation.",
                            row.tool
                        )
                    }),
            ),
            EffectState::Sent | EffectState::Unknown => Some(format!(
                "Outcome unknown: {} was sent, but the task was stopped before an answer came                  back. It may or may not have taken effect. Check with a read before doing it                  again.",
                row.tool
            )),
            EffectState::Intended | EffectState::Failed => None,
        }
    }

    /// [`Self::journaled_outcome_text`] for a call a rollback is about to hide: only effects
    /// that are not reads.
    fn journaled_effect_text(&self, run_id: &str, call_id: &str) -> Option<String> {
        let row = self.commits.as_ref()?.effect_get(run_id, call_id).ok()??;
        if row.class == crate::effect_class::EffectClass::Read {
            return None;
        }
        self.journaled_outcome_text(run_id, call_id)
    }

    /// The log now holds the tool messages that end `rest`: the journal's stored result bytes
    /// for those calls are no longer needed (the digest stays). The log's text is the guest's
    /// formatting, so the call id, not the digest, ties them.
    fn clear_recorded_results(&self, run_id: &str, rest: &[vam_spec::v1::ChatMessage]) {
        let Some(store) = self.commits.as_ref() else {
            return;
        };
        for message in rest.iter().rev().take_while(|m| m.role == "tool") {
            if message.tool_call_id.is_empty() {
                continue;
            }
            if let Ok(Some(row)) = store.effect_get(run_id, &message.tool_call_id) {
                if row.result_bytes.is_some() {
                    let _ = store.effect_clear_result(run_id, &message.tool_call_id);
                }
            }
        }
    }

    /// A request just extended the log and ends with the tool results of the
    /// previous round. If a turn commit announced exactly those results, the
    /// log position right after them is its boundary; with its manifest the
    /// commit is complete.
    fn note_tool_result_boundary(
        &self,
        scope: &RecorderScope,
        st: &State,
        rest: &[vam_spec::v1::ChatMessage],
    ) {
        let Some(store) = self.commits.as_ref() else {
            return;
        };
        let wires: Vec<vam_spec::thread_context::WireMessage> =
            rest.iter().map(spec_wire).collect();
        let tail = vam_spec::thread_context::trailing_tool_results(&wires);
        if tail.is_empty() {
            return;
        }
        let (Some((boundary_seq, _)), Some(head)) = (st.context.last(), st.head.as_ref()) else {
            return;
        };
        let digest = vam_spec::thread_context::tail_digest(tail);
        match store.match_turn_commit_tail(&scope.run_id, &digest, *boundary_seq, &head.digest) {
            Ok(Some((round, completed))) => {
                tracing::debug!(
                    run = %scope.run_id, round, completed,
                    "turn commit boundary recorded at log seq {boundary_seq}"
                );
            }
            Ok(None) => {}
            Err(e) => {
                tracing::warn!(session = %self.session_id, "turn commit boundary not recorded: {e}");
            }
        }
    }

    async fn record_response(&self, resp: ExchangeResponse) {
        let round = self
            .pending
            .lock()
            .ok()
            .and_then(|mut p| p.remove(&resp.exchange_id));
        if self.sealed.load(Ordering::SeqCst) {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(scope) = self.scope() else {
            return;
        };
        let mut st = self.state.lock().await;
        if let Err(e) = self.load(&scope, &mut st).await {
            let reason = if e.contains("log_broken") {
                "log_broken"
            } else {
                "recorder_error"
            };
            self.note_failure(&scope, &mut st, reason, &e).await;
            return;
        }
        let summary_plan = self
            .summaries
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&resp.exchange_id));
        if let Some(plan) = summary_plan {
            if let Ok(mut stable) = self.stable.lock() {
                stable.remove(&resp.exchange_id);
            }
            let round = round.unwrap_or_else(|| self.round.load(Ordering::Relaxed));
            self.record_summary_response(&scope, &mut st, resp, plan, round)
                .await;
            return;
        }
        // A reply whose text holds the calls as JSON is recorded the way the
        // guest will send it back (calls, no text), so the next request extends
        // the log instead of rewriting its last message.
        let mut content = resp.text.clone();
        let mut raw_text: Option<String> = None;
        let mut tool_calls: Vec<(String, String, String)> = resp
            .tool_calls
            .iter()
            .map(|c| (c.id.clone(), c.name.clone(), c.arguments.clone()))
            .collect();
        if tool_calls.is_empty() {
            let parsed = vam_spec::text_tool_calls::extract_tool_calls_from_text(&content);
            if !parsed.is_empty() {
                // The model's own text stays in the record for audit; assembly
                // never sends it back (only `msg` is read).
                raw_text = Some(content.clone());
                content.clear();
                tool_calls = parsed
                    .into_iter()
                    .map(|c| (c.id, c.name, c.arguments))
                    .collect();
            }
        }
        let wire = WireMessage {
            role: "assistant".to_string(),
            content,
            tool_calls,
            tool_call_id: String::new(),
        };
        let message = LogMessage::from_wire(&wire);
        let prefix_stable = self
            .stable
            .lock()
            .ok()
            .and_then(|mut m| m.remove(&resp.exchange_id));
        let usage = resp.usage.as_ref().map(|u| {
            let mut usage = json!({
                "prompt": u.prompt_tokens,
                "completion": u.completion_tokens,
                "cached": u.cached_tokens,
                "cache_write": u.cache_creation_tokens,
            });
            if u.cached_tokens_unknown {
                usage["cached_unknown"] = json!(true);
            }
            if let Some(stable) = prefix_stable {
                usage["prefix_stable"] = json!(stable);
            }
            usage
        });
        let round = round.unwrap_or_else(|| self.round.load(Ordering::Relaxed));
        let mut body = message_body(&message, usage);
        if let (Some(raw), Some(map)) = (raw_text, body.as_object_mut()) {
            map.insert("raw_text".to_string(), Value::String(raw));
        }
        let rec = Self::record(&scope, kind::MESSAGE, "broker", round, body);
        let mut batch = self.with_carried_gap(&scope, &mut st, vec![rec]);
        let message_index = batch.len() - 1;
        // `adapter.thinking: strip`: the reasoning text goes to its own lane (parent
        // `main`), after the reply it came with. Assembly of the main lane never reads
        // it, and it never reaches the guest or the model. Cut to a fixed size with the
        // true size named.
        if let Some(text) = resp.reasoning.as_deref().filter(|t| !t.is_empty()) {
            let mut kept = text.to_string();
            if resp.reasoning_bytes > text.len() {
                kept.push_str(&format!(
                    "\n[cut: {} of {} bytes kept]",
                    text.len(),
                    resp.reasoning_bytes
                ));
            }
            let msg = LogMessage {
                role: "assistant".to_string(),
                parts: vec![Part::Reasoning {
                    text: kept,
                    signature: None,
                }],
            };
            let mut reasoning_rec = Self::record(
                &scope,
                kind::MESSAGE,
                "broker",
                round,
                message_body(&msg, None),
            );
            reasoning_rec.lane = cmp::REASONING_LANE.to_string();
            reasoning_rec.parent_lane = Some(MAIN_LANE.to_string());
            batch.push(reasoning_rec);
        }
        match self.append(&scope, batch).await {
            Ok(receipt) => {
                st.context.push((
                    receipt.first_seq + message_index as u64,
                    message.msg_sha256(),
                ));
                st.head = Some(receipt.head);
                st.carried_gap = None;
                st.consecutive_failures = 0;
            }
            Err(ThreadLogError::QuotaExceeded {
                scope: which,
                limit,
                used,
            }) => {
                // The reply cannot be recorded; the next request is refused.
                let _ = self
                    .note_quota(&scope, &mut st, which.as_str(), limit, used)
                    .await;
            }
            Err(e) => {
                self.note_failure(&scope, &mut st, "recorder_error", &e.to_string())
                    .await
            }
        }
    }
}

impl ThreadRecorder {
    /// Records the guest's report at an approval suspend: the results of the
    /// calls that ran after its last model request (`src: guest`), then a
    /// `suspend` record. Every tail message must be a `tool` result answering a
    /// call of the last recorded assistant message; the host works out which
    /// calls are still unanswered and ignores any list the guest sends.
    pub async fn record_suspend(
        &self,
        tail: &vam_spec::thread_context::SuspendTail,
        tail_sha256: &str,
    ) -> Result<(), String> {
        let scope = self
            .scope()
            .ok_or_else(|| "the session has no thread scope".to_string())?;
        let mut st = self.state.lock().await;
        if st.last_suspend_tail.as_deref() == Some(tail_sha256) {
            return Ok(());
        }
        self.load(&scope, &mut st).await?;
        let reparked = st.open_suspend.as_ref().is_some_and(|open| {
            open.pending_call_id == tail.pending.tool_call_id
                && open.context_len == st.context.len()
        });
        if reparked {
            // The same call parks again: its results are already in the log. Only a new
            // `suspend` record is written; the tail's messages are ignored.
            let unanswered = st
                .open_suspend
                .as_ref()
                .map(|open| open.unanswered.clone())
                .unwrap_or_default();
            let round = self.round.load(Ordering::Relaxed);
            let batch = vec![Self::record(
                &scope,
                kind::SUSPEND,
                "host",
                round,
                json!({
                    "pending": {
                        "tool_call_id": tail.pending.tool_call_id,
                        "tool_name": tail.pending.tool_name,
                        "args_json": tail.pending.args_json,
                    },
                    "unanswered": unanswered,
                    "lifecycle_generation": tail.lifecycle_generation,
                    "state": tail.state,
                    "tail_sha256": tail_sha256,
                }),
            )];
            let batch = self.with_carried_gap(&scope, &mut st, batch);
            return match self.append(&scope, batch).await {
                Ok(receipt) => {
                    st.head = Some(receipt.head);
                    st.carried_gap = None;
                    st.last_suspend_tail = Some(tail_sha256.to_string());
                    Ok(())
                }
                Err(e) => {
                    self.note_failure(&scope, &mut st, "recorder_error", &e.to_string())
                        .await;
                    Err(e.to_string())
                }
            };
        }
        let (last_seq, _) = st
            .context
            .last()
            .cloned()
            .ok_or_else(|| "no recorded assistant message awaits results".to_string())?;
        let tree = self.tree.clone();
        let thread = scope.thread_ref.clone();
        let page = tokio::task::spawn_blocking(move || tree.read(&thread, last_seq - 1, 1))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())?;
        let last = page
            .records
            .first()
            .filter(|r| r.seq == last_seq)
            .and_then(|r| r.message())
            .filter(|m| m.role == "assistant")
            .map(|m| m.to_wire())
            .ok_or_else(|| "the last recorded message is not an assistant message".to_string())?;
        let call_ids: Vec<&str> = last.tool_calls.iter().map(|c| c.0.as_str()).collect();
        let pending = &tail.pending;
        if !call_ids.contains(&pending.tool_call_id.as_str()) {
            return Err("the pending call is not a call of the last assistant message".into());
        }
        let mut answered: Vec<&str> = Vec::new();
        for message in &tail.messages {
            if message.role != "tool" {
                return Err("a suspend tail may hold tool results only".into());
            }
            let id = message.tool_call_id.as_str();
            if !call_ids.contains(&id) || id == pending.tool_call_id || answered.contains(&id) {
                return Err(format!(
                    "tail result for call `{id}` does not answer an open call"
                ));
            }
            answered.push(id);
        }
        let unanswered: Vec<&str> = call_ids
            .iter()
            .copied()
            .filter(|id| *id != pending.tool_call_id && !answered.contains(id))
            .collect();

        let round = self.round.load(Ordering::Relaxed);
        let mut batch = self.with_carried_gap(&scope, &mut st, Vec::new());
        let first_message = batch.len();
        let mut added: Vec<String> = Vec::new();
        for message in &tail.messages {
            let wire = WireMessage {
                role: message.role.clone(),
                content: message.content.clone(),
                tool_calls: vec![],
                tool_call_id: message.tool_call_id.clone(),
            };
            let log_message = LogMessage::from_wire(&wire);
            added.push(log_message.msg_sha256());
            let mut rec = Self::record(
                &scope,
                kind::MESSAGE,
                "guest",
                round,
                message_body(&log_message, None),
            );
            rec.src = "guest".to_string();
            batch.push(rec);
        }
        batch.push(Self::record(
            &scope,
            kind::SUSPEND,
            "host",
            round,
            json!({
                "pending": {
                    "tool_call_id": pending.tool_call_id,
                    "tool_name": pending.tool_name,
                    "args_json": pending.args_json,
                },
                "unanswered": unanswered,
                "lifecycle_generation": tail.lifecycle_generation,
                "state": tail.state,
                "tail_sha256": tail_sha256,
            }),
        ));
        match self.append(&scope, batch).await {
            Ok(receipt) => {
                for (index, sha) in added.into_iter().enumerate() {
                    st.context
                        .push((receipt.first_seq + (first_message + index) as u64, sha));
                }
                st.head = Some(receipt.head);
                st.carried_gap = None;
                st.last_suspend_tail = Some(tail_sha256.to_string());
                st.open_suspend = Some(OpenSuspend {
                    pending_call_id: pending.tool_call_id.clone(),
                    context_len: st.context.len(),
                    unanswered: unanswered.iter().map(|id| id.to_string()).collect(),
                });
                Ok(())
            }
            Err(e) => {
                self.note_failure(&scope, &mut st, "recorder_error", &e.to_string())
                    .await;
                Err(e.to_string())
            }
        }
    }
}

/// What the host writes as the result of a call the user's stop kept from running
/// (the task was parked on its approval).
pub const STOPPED_BY_USER_CALL_RESULT: &str = "Not run: stopped by user";

/// What the host writes as the result of a call a stop cut off.
pub const STOPPED_CALL_RESULT: &str = "Not run: the task was stopped; check whether it took \
     effect before retrying. Workspace changes after the last finished step were rolled back.";

impl ThreadRecorder {
    /// Records the end of a stopped task and seals the recorder (a late request
    /// is dropped). Files and conversation roll back together (decision 12):
    ///
    /// * the floor is the log boundary of the session's newest complete turn
    ///   commit, or, with none, the task's `task_start`;
    /// * if a file-writing tool result lies after the floor, the files it wrote
    ///   are rolled back with the workspace, so a `rollback{to: floor, reason:
    ///   stopped}` hides everything after the floor;
    /// * otherwise the calls the model made and never got an answer to receive a
    ///   host result ([`STOPPED_CALL_RESULT`]), so the log stays a valid
    ///   conversation;
    /// * then `task_end{stopped}`. Commits that never completed are marked failed.
    ///
    /// Returns the log head after `task_end`. A failure is counted and leaves a
    /// `gap`, never a panic.
    pub async fn record_stop(&self) -> Option<LogHead> {
        self.record_interrupted("stopped", "stopped").await
    }

    /// The stop that follows answers the calls it kept from running with
    /// [`STOPPED_BY_USER_CALL_RESULT`].
    pub fn mark_user_stop(&self) {
        self.user_stop.store(true, Ordering::SeqCst);
    }

    /// [`record_stop`](Self::record_stop) for an end nobody asked for (vamd
    /// restarted under the task): `task_end{interrupted}` with the reason.
    pub async fn record_interrupted(&self, outcome: &str, reason: &str) -> Option<LogHead> {
        self.record_interrupted_inner(outcome, reason, false).await
    }

    /// The end of a session whose task is picked up again (a guest crash, a `vamd` restart;
    /// E10 Phase 4). Files and conversation roll back together exactly as for a stop, but the
    /// task is not over: no call is answered "Not run" and no `task_end` is written, so the
    /// conversation ends where the host last recorded it. The model's decided round (an
    /// assistant message with calls not yet answered) stays, to be acted on by the next guest.
    /// Host effects after a rolled-back floor are appended again, as for a stop (Q6). Returns
    /// the log head.
    pub async fn record_interrupted_for_resume(&self) -> Option<LogHead> {
        self.record_interrupted_inner("interrupted", "resume", true)
            .await
    }

    async fn record_interrupted_inner(
        &self,
        outcome: &str,
        reason: &str,
        for_resume: bool,
    ) -> Option<LogHead> {
        self.sealed.store(true, Ordering::SeqCst);
        let scope = self.scope()?;
        // A stop during a host call lets the call finish and be journaled (V.3): the end is
        // recorded after it. A `sent` row with nothing in flight (vamd restarted under it) is
        // an outcome nobody knows.
        if let Some(store) = self.commits.as_ref() {
            crate::effect_path::wait_idle(
                store,
                &scope.run_id,
                crate::effect_path::INFLIGHT_WAIT_CAP,
            )
            .await;
        }
        self.settle_orphaned_sends(&scope.run_id);
        let mut st = self.state.lock().await;
        if let Err(e) = self.load(&scope, &mut st).await {
            let why = if e.contains("log_broken") {
                "log_broken"
            } else {
                "recorder_error"
            };
            self.note_failure(&scope, &mut st, why, &e).await;
            return None;
        }
        let boundary = self.commits.as_ref().and_then(|store| {
            store
                .last_complete_turn_commit_for_run(&scope.run_id, &self.session_id)
                .ok()
                .flatten()
                .and_then(|row| row.boundary_seq)
        });
        let mut floor = boundary.or_else(|| self.run_first_seq());
        if floor.is_none() {
            // A recorder built after a restart never saw `task_start`: the run's
            // first one is in the log.
            let tree = self.tree.clone();
            let thread = scope.thread_ref.clone();
            let run = scope.run_id.clone();
            floor = tokio::task::spawn_blocking(move || tree.read(&thread, 0, usize::MAX))
                .await
                .ok()
                .and_then(|page| page.ok())
                .and_then(|page| {
                    page.records
                        .iter()
                        .filter(|r| r.kind == kind::TASK_START && r.run == run)
                        .map(|r| r.seq)
                        .min()
                });
        }

        if for_resume && boundary.is_none() {
            // No step finished yet. A task that is picked up again needs its own prompt: the
            // floor is just after it, not `task_start`.
            let tree = self.tree.clone();
            let thread = scope.thread_ref.clone();
            let run = scope.run_id.clone();
            let prompt_seq = tokio::task::spawn_blocking(move || tree.read(&thread, 0, usize::MAX))
                .await
                .ok()
                .and_then(|page| page.ok())
                .and_then(|page| {
                    page.records
                        .iter()
                        .filter(|r| {
                            r.run == run
                                && r.lane == vam_storage::thread_log::MAIN_LANE
                                && r.message().is_some_and(|m| m.role == "user")
                        })
                        .map(|r| r.seq)
                        .min()
                });
            if prompt_seq.is_some() {
                floor = prompt_seq;
            }
        }

        // The messages after the floor that are still in the context.
        let after: Vec<(u64, WireMessage)> = match floor {
            Some(floor) => {
                let tree = self.tree.clone();
                let thread = scope.thread_ref.clone();
                let page =
                    tokio::task::spawn_blocking(move || tree.read(&thread, floor, usize::MAX))
                        .await
                        .map_err(|e| e.to_string())
                        .and_then(|r| r.map_err(|e| e.to_string()));
                match page {
                    Ok(page) => {
                        let live: std::collections::HashSet<u64> =
                            st.context.iter().map(|(seq, _)| *seq).collect();
                        page.records
                            .iter()
                            .filter(|r| r.kind == kind::MESSAGE && live.contains(&r.seq))
                            .filter_map(|r| r.message().map(|m| (r.seq, m.to_wire())))
                            .collect()
                    }
                    Err(e) => {
                        self.note_failure(&scope, &mut st, "recorder_error", &e)
                            .await;
                        return None;
                    }
                }
            }
            None => Vec::new(),
        };

        let mut call_names: HashMap<String, String> = HashMap::new();
        for (_, m) in &after {
            if m.role == "assistant" {
                for (id, name, _) in &m.tool_calls {
                    call_names.insert(id.clone(), name.clone());
                }
            }
        }
        let wrote_after_floor = after.iter().any(|(_, m)| {
            m.role == "tool"
                && call_names
                    .get(&m.tool_call_id)
                    .is_some_and(|name| vam_spec::thread_context::tool_may_write_files(name))
        });

        let round = self.round.load(Ordering::Relaxed);
        let mut batch = self.with_carried_gap(&scope, &mut st, Vec::new());
        let mut host_results: Vec<String> = Vec::new();
        match floor {
            Some(to) if wrote_after_floor => {
                batch.push(Self::record(
                    &scope,
                    kind::ROLLBACK,
                    "host",
                    round,
                    json!({"to": to, "reason": reason, "dropped": after.len()}),
                ));
                // The files roll back; an effect outside the VM does not. Each journaled host
                // effect after the floor is appended again, the call and its outcome (Q6).
                let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
                for (_, message) in &after {
                    if message.role != "assistant" {
                        continue;
                    }
                    for call in &message.tool_calls {
                        if !seen.insert(call.0.clone()) {
                            continue;
                        }
                        let Some(content) = self.journaled_effect_text(&scope.run_id, &call.0)
                        else {
                            continue;
                        };
                        let call_wire = WireMessage {
                            role: "assistant".to_string(),
                            content: String::new(),
                            tool_calls: vec![call.clone()],
                            tool_call_id: String::new(),
                        };
                        let result_wire = WireMessage {
                            role: "tool".to_string(),
                            content,
                            tool_calls: vec![],
                            tool_call_id: call.0.clone(),
                        };
                        for wire in [call_wire, result_wire] {
                            let message = LogMessage::from_wire(&wire);
                            host_results.push(message.msg_sha256());
                            batch.push(Self::record(
                                &scope,
                                kind::MESSAGE,
                                "host",
                                round,
                                message_body(&message, None),
                            ));
                        }
                    }
                }
            }
            // A task that is picked up again keeps its decided round open: nothing is answered.
            _ if for_resume => {}
            _ => {
                let last_assistant = after
                    .iter()
                    .rposition(|(_, m)| m.role == "assistant" && !m.tool_calls.is_empty());
                if let Some(index) = last_assistant {
                    let answered: Vec<&str> = after[index + 1..]
                        .iter()
                        .filter(|(_, m)| m.role == "tool")
                        .map(|(_, m)| m.tool_call_id.as_str())
                        .collect();
                    for (call_id, _, _) in &after[index].1.tool_calls {
                        if !answered.contains(&call_id.as_str()) {
                            let wire = WireMessage {
                                role: "tool".to_string(),
                                // A host call that left is answered from the journal, never
                                // with "Not run": the conversation must not forget it.
                                content: self
                                    .journaled_outcome_text(&scope.run_id, call_id)
                                    .unwrap_or_else(|| {
                                        if self.user_stop.load(Ordering::SeqCst) {
                                            STOPPED_BY_USER_CALL_RESULT.to_string()
                                        } else {
                                            STOPPED_CALL_RESULT.to_string()
                                        }
                                    }),
                                tool_calls: vec![],
                                tool_call_id: call_id.clone(),
                            };
                            let message = LogMessage::from_wire(&wire);
                            host_results.push(message.msg_sha256());
                            batch.push(Self::record(
                                &scope,
                                kind::MESSAGE,
                                "host",
                                round,
                                message_body(&message, None),
                            ));
                        }
                    }
                }
            }
        }
        let head_before = st
            .head
            .as_ref()
            .map(|h| json!({"seq": h.seq, "digest": h.digest}))
            .unwrap_or(Value::Null);
        if for_resume {
            if let Some(store) = self.commits.as_ref() {
                // A commit that never completed is void; the next guest numbers its rounds past
                // every row the run has, so the round it runs again cannot collide with one.
                let _ = store.fail_pending_turn_commits(
                    &scope.run_id,
                    &self.session_id,
                    "the task was interrupted before the commit completed",
                );
            }
            if batch.is_empty() {
                return st.head.clone();
            }
            return match self.append(&scope, batch).await {
                Ok(receipt) => {
                    if wrote_after_floor && floor.is_some() {
                        self.rollbacks.fetch_add(1, Ordering::Relaxed);
                    }
                    st.head = Some(receipt.head.clone());
                    st.carried_gap = None;
                    Some(receipt.head)
                }
                Err(e) => {
                    self.note_failure(&scope, &mut st, "recorder_error", &e.to_string())
                        .await;
                    None
                }
            };
        }
        if let Some(commit) = self.end_commit_record(&scope, round) {
            batch.push(commit);
        }
        batch.push(Self::record(
            &scope,
            kind::TASK_END,
            "host",
            round,
            json!({
                "outcome": outcome,
                "answer": "",
                "reason": reason,
                "head_before": head_before,
                "rolled_back": wrote_after_floor && floor.is_some(),
                "host_results": host_results.len(),
            }),
        ));
        if let Some(store) = self.commits.as_ref() {
            let _ = store.fail_pending_turn_commits(
                &scope.run_id,
                &self.session_id,
                "the task was stopped before the commit completed",
            );
        }
        match self.append(&scope, batch).await {
            Ok(receipt) => {
                if wrote_after_floor && floor.is_some() {
                    self.rollbacks.fetch_add(1, Ordering::Relaxed);
                }
                st.head = Some(receipt.head.clone());
                st.carried_gap = None;
                Some(receipt.head)
            }
            Err(e) => {
                self.note_failure(&scope, &mut st, "recorder_error", &e.to_string())
                    .await;
                None
            }
        }
    }
}

impl ExchangeObserver for ThreadRecorder {
    fn on_request(
        &self,
        request: ExchangeRequest,
    ) -> futures_util::future::BoxFuture<'_, Result<(), ExchangeRefusal>> {
        Box::pin(self.record_request(request))
    }

    fn on_response(&self, response: ExchangeResponse) -> futures_util::future::BoxFuture<'_, ()> {
        Box::pin(self.record_response(response))
    }

    fn on_abort(&self, exchange_id: u64) {
        if let Ok(mut pending) = self.pending.lock() {
            pending.remove(&exchange_id);
        }
        if let Ok(mut summaries) = self.summaries.lock() {
            summaries.remove(&exchange_id);
        }
        if let Ok(mut stable) = self.stable.lock() {
            stable.remove(&exchange_id);
        }
    }
}

/// `CommitTurn` reads the log position from the session's recorder.
pub struct RecorderHeads {
    pub store: Arc<SqliteWorkflowStore>,
}

impl crate::turn_commit::ThreadLogHeads for RecorderHeads {
    fn log_head(&self, session_id: &str) -> Option<(u64, String)> {
        self.store
            .thread_recorder(session_id)?
            .cached_head()
            .map(|h| (h.seq, h.digest))
    }
}

#[cfg(test)]
mod tests;
