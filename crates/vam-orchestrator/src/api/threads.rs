//! Thread read API: a workspace's conversations, read from the thread log
//! (`/v1/workspaces/:id/threads`). Workspace access is conversation access
//! (locked decision 23), so auth and id resolution match the workspace files API.
//!
//! Source: while a session holds the workspace the live session tree is read
//! (records appear per round), else Drive9 HEAD. A thread the live tree does not
//! hold falls back to HEAD. Tool and user content is returned as data; system
//! prompt and tool definitions are only ever digests in the log.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::get;
use axum::Router;
use serde_json::{json, Map, Value};
use vam_spec::v1::VamErrorCode;
use vam_storage::thread_log::{
    assemble_context, kind, records_without_rollbacks, HeadThreadReader, Integrity, LogMessage,
    Part, Record, ThreadLogError, ThreadLogReader, ThreadSummary, TreeThreadStore, MAIN_LANE,
};
use vam_storage::vam_dir::{THREADS_DIR, VAM_DIR};
use vam_storage::{sanitize_thread_id, ChunkStore, Drive9Layout, ManifestManager, TurnRecord};

use crate::api::auth::AuthContext;
use crate::api::error::ApiError;
use crate::api::{drive9_root, ensure_ready, json_http_response, AppState};
use crate::error::OrchestratorError;

const SCOPE: &str = "sessions:write";

/// Messages per page: default and cap.
pub const DEFAULT_LIMIT: usize = 100;
pub const MAX_LIMIT: usize = 500;
/// A page stops once its serialized records pass this many bytes.
pub const PAGE_BYTE_BUDGET: usize = 8 * 1024 * 1024;

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/v1/workspaces/:id/threads", get(list_threads))
        .route("/v1/workspaces/:id/threads/:thread", get(get_thread))
        .route(
            "/v1/workspaces/:id/threads/:thread/messages",
            get(list_messages),
        )
}

// ---------------------------------------------------------------------------
// Sources

/// Where a request reads from.
#[derive(Clone)]
enum Plan {
    /// Live session tree (`.../materialized`), HEAD behind it per thread.
    Live(PathBuf),
    Head,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Source {
    Live,
    Head,
}

impl Source {
    fn as_str(self) -> &'static str {
        match self {
            Source::Live => "live",
            Source::Head => "head",
        }
    }
}

/// What the endpoints need from either backend.
trait Access: ThreadLogReader {
    fn timeline(&self, thread: &str) -> Result<Vec<TurnRecord>, ThreadLogError>;
}

impl Access for TreeThreadStore {
    fn timeline(&self, thread: &str) -> Result<Vec<TurnRecord>, ThreadLogError> {
        TreeThreadStore::timeline(self, thread)
    }
}

impl Access for HeadThreadReader<'_> {
    fn timeline(&self, thread: &str) -> Result<Vec<TurnRecord>, ThreadLogError> {
        HeadThreadReader::timeline(self, thread)
    }
}

fn with_head<T>(
    workspace_id: &str,
    f: impl FnOnce(&dyn Access, Option<String>) -> Result<T, ThreadLogError>,
) -> Result<T, ThreadLogError> {
    let layout = Drive9Layout::new(drive9_root());
    let chunks = ChunkStore::new(layout.clone());
    let manifests = ManifestManager::new(layout, chunks.clone());
    let head = manifests
        .read_head(workspace_id)
        .map_err(|e| ThreadLogError::Corrupt(e.to_string()))?;
    let reader = HeadThreadReader::new(&manifests, &chunks, workspace_id);
    f(&reader, head)
}

fn live_plan(state: &AppState, workspace_id: &str) -> Result<Plan, OrchestratorError> {
    if let Some(active) = super::find_active_session_on_workspace(&state.store, workspace_id)? {
        let tree = Drive9Layout::new(drive9_root())
            .session_dir(&active.session_id)
            .join("materialized");
        if tree.join(VAM_DIR).join(THREADS_DIR).is_dir() {
            return Ok(Plan::Live(tree));
        }
    }
    Ok(Plan::Head)
}

fn map_log_error(err: ThreadLogError) -> ApiError {
    match err {
        ThreadLogError::InvalidThread(msg) => OrchestratorError::InvalidContract(msg).into(),
        ThreadLogError::NotFound(msg) => OrchestratorError::NotFound(msg).into(),
        ThreadLogError::UnsupportedVersion { seq, version } => ApiError {
            status: StatusCode::UNPROCESSABLE_ENTITY,
            code: VamErrorCode::InvalidArgument,
            message: format!("thread log record {seq} has unsupported version {version}"),
            retryable: false,
        },
        other => OrchestratorError::Core(other.to_string()).into(),
    }
}

fn join_error(err: tokio::task::JoinError) -> ApiError {
    OrchestratorError::Core(format!("thread read task: {err}")).into()
}

fn thread_id(raw: &str) -> Result<String, ApiError> {
    sanitize_thread_id(raw).map_err(|e| OrchestratorError::InvalidContract(e.to_string()).into())
}

fn respond(value: Value) -> Result<Response, ApiError> {
    let body = serde_json::to_string(&value)
        .map_err(|e| OrchestratorError::Core(format!("thread json: {e}")))?;
    Ok(json_http_response(StatusCode::OK, body))
}

// ---------------------------------------------------------------------------
// Views

fn head_json(head: &Option<vam_storage::thread_log::LogHead>) -> Value {
    match head {
        Some(h) => json!({"seq": h.seq, "digest": h.digest}),
        None => Value::Null,
    }
}

fn meta_json(summary: &ThreadSummary, out: &mut Map<String, Value>) {
    let meta = summary.meta.as_ref();
    out.insert("agentUrn".into(), json!(meta.map(|m| m.agent_urn.clone())));
    out.insert(
        "agentDigest".into(),
        json!(meta.map(|m| m.agent_digest.clone())),
    );
    out.insert(
        "parentThread".into(),
        json!(meta.and_then(|m| m.parent_thread.clone())),
    );
    out.insert("forkTurn".into(), json!(meta.and_then(|m| m.fork_turn)));
    out.insert("createdAt".into(), json!(meta.map(|m| m.created_at)));
}

fn summary_json(summary: &ThreadSummary, legacy: bool) -> Map<String, Value> {
    let mut out = Map::new();
    out.insert("threadId".into(), json!(summary.thread_id));
    out.insert("legacy".into(), json!(legacy));
    out.insert("head".into(), head_json(&summary.head));
    out.insert("logBytes".into(), json!(summary.log_bytes));
    meta_json(summary, &mut out);
    out
}

fn message_value(message: &LogMessage) -> Value {
    serde_json::to_value(message).unwrap_or(Value::Null)
}

/// One log record as the API shows it. Message records carry `message` (and
/// `usage`); other kinds carry their small structured body as `detail`; a
/// compaction summary is shown as a `message` with the rest as `detail`.
fn record_json(record: &Record) -> Value {
    let mut out = Map::new();
    out.insert("seq".into(), json!(record.seq));
    out.insert("id".into(), json!(record.id));
    out.insert("ts".into(), json!(record.ts));
    out.insert("run".into(), json!(record.run));
    out.insert("lane".into(), json!(record.lane));
    out.insert("agent".into(), json!(record.agent));
    if let Some(parent) = &record.parent_lane {
        out.insert("parentLane".into(), json!(parent));
    }
    out.insert("round".into(), json!(record.round));
    out.insert("src".into(), json!(record.src));
    out.insert("kind".into(), json!(record.kind));
    match record.kind.as_str() {
        kind::MESSAGE => {
            if let Some(message) = record.message() {
                out.insert("message".into(), message_value(&message));
            }
            if let Some(usage) = record.body.as_ref().and_then(|b| b.get("usage")) {
                out.insert("usage".into(), usage.clone());
            }
        }
        kind::COMPACTION => {
            let mut detail = record.body.clone().unwrap_or(Value::Null);
            if let Some(obj) = detail.as_object_mut() {
                if let Some(summary) = obj.remove("summary") {
                    if let Ok(m) = serde_json::from_value::<LogMessage>(summary) {
                        out.insert("message".into(), message_value(&m));
                    }
                }
            }
            out.insert("detail".into(), detail);
        }
        _ => {
            out.insert("detail".into(), record.body.clone().unwrap_or(Value::Null));
        }
    }
    Value::Object(out)
}

/// Legacy timeline turns as user and assistant messages: turn n is seq 2n-1
/// (prompt) and 2n (answer).
fn legacy_records(turns: &[TurnRecord]) -> Vec<Value> {
    let mut out = Vec::with_capacity(turns.len() * 2);
    for (i, turn) in turns.iter().enumerate() {
        let n = (i as u64) + 1;
        for (offset, role, text) in [(1, "user", &turn.prompt), (0, "assistant", &turn.answer)] {
            let mut rec = Map::new();
            rec.insert("seq".into(), json!(2 * n - offset));
            rec.insert("kind".into(), json!("message"));
            rec.insert("run".into(), json!(turn.run_id));
            rec.insert("lane".into(), json!(MAIN_LANE));
            rec.insert("legacy".into(), json!(true));
            rec.insert("ts".into(), json!(turn.timestamp));
            rec.insert(
                "message".into(),
                json!({"role": role, "parts": [{"type": "text", "text": text}]}),
            );
            out.push(Value::Object(rec));
        }
    }
    out
}

/// The latest plan: every `todo_write` call on the main lane in log order,
/// merged by id and sorted by id like the guest tool; `seq` is the last call's.
/// Rolled-back calls are excluded. A failed call cannot be told from a good one.
pub fn plan_from_records(records: &[Record]) -> Value {
    let mut items: BTreeMap<String, Value> = BTreeMap::new();
    let mut last_seq = None;
    for record in records_without_rollbacks(records, MAIN_LANE) {
        let Some(message) = record.message() else {
            continue;
        };
        for part in &message.parts {
            let Part::ToolUse {
                name, arguments, ..
            } = part
            else {
                continue;
            };
            if name != "todo_write" {
                continue;
            }
            let Ok(args) = serde_json::from_str::<Value>(arguments) else {
                continue;
            };
            let Some(todos) = args.get("todos").and_then(Value::as_array) else {
                continue;
            };
            let mut applied = false;
            for todo in todos {
                let field = |k: &str| todo.get(k).and_then(Value::as_str);
                if let (Some(id), Some(content), Some(status)) =
                    (field("id"), field("content"), field("status"))
                {
                    items.insert(
                        id.to_string(),
                        json!({"id": id, "content": content, "status": status}),
                    );
                    applied = true;
                }
            }
            if applied {
                last_seq = Some(record.seq);
            }
        }
    }
    match last_seq {
        Some(seq) => json!({"seq": seq, "items": items.into_values().collect::<Vec<_>>()}),
        None => Value::Null,
    }
}

// ---------------------------------------------------------------------------
// Thread resolution

struct Loaded {
    source: Source,
    manifest_head: Option<String>,
    summary: ThreadSummary,
    legacy: bool,
    /// Log records (empty for a legacy thread) and their integrity.
    records: Vec<Record>,
    integrity: Integrity,
    turns: Vec<TurnRecord>,
}

fn load_from(
    reader: &dyn Access,
    thread: &str,
    manifest_head: Option<String>,
    source: Source,
) -> Result<Option<Loaded>, ThreadLogError> {
    let Some(summary) = reader.list()?.into_iter().find(|s| s.thread_id == thread) else {
        return Ok(None);
    };
    if summary.head.is_some() {
        let page = reader.read(thread, 0, usize::MAX)?;
        return Ok(Some(Loaded {
            source,
            manifest_head,
            summary,
            legacy: false,
            records: page.records,
            integrity: page.integrity,
            turns: Vec::new(),
        }));
    }
    let turns = reader.timeline(thread)?;
    if turns.is_empty() && summary.meta.is_none() {
        return Ok(None);
    }
    Ok(Some(Loaded {
        source,
        manifest_head,
        summary,
        legacy: true,
        records: Vec::new(),
        integrity: Integrity::Ok,
        turns,
    }))
}

fn load_thread(
    plan: Plan,
    workspace_id: String,
    thread: String,
) -> Result<Option<Loaded>, ThreadLogError> {
    if let Plan::Live(root) = &plan {
        let tree = TreeThreadStore::new(root);
        if let Some(found) = load_from(&tree, &thread, None, Source::Live)? {
            return Ok(Some(found));
        }
    }
    with_head(&workspace_id, |reader, head| {
        load_from(reader, &thread, head, Source::Head)
    })
}

// ---------------------------------------------------------------------------
// Handlers

async fn list_threads(
    State(state): State<AppState>,
    auth: AuthContext,
    Path(guest_workspace_id): Path<String>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "ListThreads", SCOPE)?;
    let workspace_id =
        crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &guest_workspace_id)?;
    let plan = live_plan(&state, &workspace_id)?;
    let ws = workspace_id.clone();

    let listing = tokio::task::spawn_blocking(move || {
        let mut threads: BTreeMap<String, (ThreadSummary, bool)> = BTreeMap::new();
        let mut live = false;
        if let Plan::Live(root) = &plan {
            let tree = TreeThreadStore::new(root);
            for s in tree.list()? {
                let legacy = s.head.is_none() && !tree.timeline(&s.thread_id)?.is_empty();
                threads.insert(s.thread_id.clone(), (s, legacy));
                live = true;
            }
        }
        let (manifest_head, from_head) = with_head(&ws, |reader, head| {
            let mut out = Vec::new();
            for s in reader.list()? {
                let legacy = s.head.is_none() && !reader.timeline(&s.thread_id)?.is_empty();
                out.push((s, legacy));
            }
            Ok((head, out))
        })?;
        let found = live || manifest_head.is_some();
        for (s, legacy) in from_head {
            threads.entry(s.thread_id.clone()).or_insert((s, legacy));
        }
        Ok::<_, ThreadLogError>((found, live, manifest_head, threads))
    })
    .await
    .map_err(join_error)?
    .map_err(map_log_error)?;

    let (found, live, manifest_head, mut threads) = listing;
    let rows = state
        .store
        .list_thread_rows(auth.owner_user_id(), &guest_workspace_id)?;
    // A thread the Space holds (a grant makes one before its first run) has no log yet: it is
    // listed with an empty history.
    for row in &rows {
        threads
            .entry(row.thread_id.clone())
            .or_insert_with(|| (empty_summary(&row.thread_id), false));
    }
    if !found && rows.is_empty() {
        return Err(OrchestratorError::NotFound(format!(
            "workspace '{guest_workspace_id}' has no committed state"
        ))
        .into());
    }
    let labels: HashMap<String, String> = rows
        .into_iter()
        .map(|row| (row.thread_id, row.label))
        .collect();
    let threads: Vec<Value> = threads
        .values()
        .map(|(s, legacy)| {
            let mut out = summary_json(s, *legacy);
            out.insert("spaceId".into(), json!(guest_workspace_id));
            out.insert("name".into(), json!(labels.get(&s.thread_id)));
            Value::Object(out)
        })
        .collect();
    respond(json!({
        "spaceId": guest_workspace_id,
        "workspaceId": guest_workspace_id,
        "source": if live { "live" } else { "head" },
        "head": manifest_head,
        "threads": threads,
    }))
}

async fn get_thread(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((guest_workspace_id, raw_thread)): Path<(String, String)>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "GetThread", SCOPE)?;
    let workspace_id =
        crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &guest_workspace_id)?;
    let thread = thread_id(&raw_thread)?;
    let plan = live_plan(&state, &workspace_id)?;

    let loaded = {
        let thread = thread.clone();
        tokio::task::spawn_blocking(move || load_thread(plan, workspace_id, thread))
            .await
            .map_err(join_error)?
            .map_err(map_log_error)?
    }
    .map_or_else(
        || empty_loaded(&state, &auth, &guest_workspace_id, &thread),
        Ok,
    )?;

    let mut out = summary_json(&loaded.summary, loaded.legacy);
    out.insert("workspaceId".into(), json!(guest_workspace_id));
    out.insert("spaceId".into(), json!(guest_workspace_id));
    out.insert(
        "name".into(),
        json!(state
            .store
            .get_thread(auth.owner_user_id(), &guest_workspace_id, &thread)
            .ok()
            .map(|row| row.label)),
    );
    out.insert("source".into(), json!(loaded.source.as_str()));
    out.insert("manifestHead".into(), json!(loaded.manifest_head));
    out.insert("integrity".into(), json!(loaded.integrity.as_api_string()));
    out.insert(
        "turns".into(),
        json!(if loaded.legacy { loaded.turns.len() } else { 0 }),
    );
    out.insert("plan".into(), plan_from_records(&loaded.records));
    respond(Value::Object(out))
}

fn empty_summary(thread: &str) -> ThreadSummary {
    ThreadSummary {
        thread_id: thread.to_string(),
        meta: None,
        head: None,
        log_bytes: 0,
    }
}

/// A thread the Space holds but that has no log yet (made by a grant, not yet run): an empty
/// history. A thread the Space does not hold stays "not found".
fn empty_loaded(
    state: &AppState,
    auth: &AuthContext,
    space_id: &str,
    thread: &str,
) -> Result<Loaded, OrchestratorError> {
    state
        .store
        .get_thread(auth.owner_user_id(), space_id, thread)
        .map_err(|_| OrchestratorError::NotFound(format!("thread '{thread}' not found")))?;
    Ok(Loaded {
        source: Source::Head,
        manifest_head: None,
        summary: empty_summary(thread),
        legacy: false,
        records: Vec::new(),
        integrity: Integrity::Ok,
        turns: Vec::new(),
    })
}

struct MessagesQuery {
    view_context: bool,
    after: u64,
    limit: usize,
}

fn parse_query(q: &HashMap<String, String>) -> Result<MessagesQuery, ApiError> {
    let bad = |m: String| ApiError::from(OrchestratorError::InvalidContract(m));
    let view_context = match q.get("view").map(String::as_str).unwrap_or("all") {
        "all" => false,
        "context" => true,
        other => {
            return Err(bad(format!(
                "view must be 'all' or 'context', got '{other}'"
            )))
        }
    };
    let after = match q.get("after") {
        None => 0,
        Some(v) => v
            .parse::<u64>()
            .map_err(|_| bad(format!("after must be a non-negative integer, got '{v}'")))?,
    };
    let limit = match q.get("limit") {
        None => DEFAULT_LIMIT,
        Some(v) => v
            .parse::<usize>()
            .ok()
            .filter(|n| *n >= 1)
            .ok_or_else(|| bad(format!("limit must be an integer from 1, got '{v}'")))?
            .min(MAX_LIMIT),
    };
    Ok(MessagesQuery {
        view_context,
        after,
        limit,
    })
}

async fn list_messages(
    State(state): State<AppState>,
    auth: AuthContext,
    Path((guest_workspace_id, raw_thread)): Path<(String, String)>,
    Query(raw_query): Query<HashMap<String, String>>,
) -> Result<Response, ApiError> {
    ensure_ready(&state, false)?;
    state.pdp.authorize(&auth, "ListThreadMessages", SCOPE)?;
    let workspace_id =
        crate::spaces::space_for_route(&state.store, auth.owner_user_id(), &guest_workspace_id)?;
    let thread = thread_id(&raw_thread)?;
    let query = parse_query(&raw_query)?;
    let plan = live_plan(&state, &workspace_id)?;

    let loaded = {
        let thread = thread.clone();
        tokio::task::spawn_blocking(move || load_thread(plan, workspace_id, thread))
            .await
            .map_err(join_error)?
            .map_err(map_log_error)?
    }
    .map_or_else(
        || empty_loaded(&state, &auth, &guest_workspace_id, &thread),
        Ok,
    )?;

    // Every candidate record of the view, in seq order, as (seq, json).
    let mut context_info = Value::Null;
    let candidates: Vec<(u64, Value)> = if loaded.legacy {
        legacy_records(&loaded.turns)
            .into_iter()
            .map(|v| (v["seq"].as_u64().unwrap_or(0), v))
            .collect()
    } else if query.view_context {
        let ctx = assemble_context(&loaded.records, MAIN_LANE).map_err(map_log_error)?;
        context_info = json!({
            "headSeq": ctx.head.as_ref().map(|h| h.seq),
            "headDigest": ctx.head.as_ref().map(|h| h.digest.clone()),
            "bytes": ctx.bytes,
            "systemSha256": ctx.system_sha256,
            "toolsSha256": ctx.tools_sha256,
            "gaps": ctx.gaps,
        });
        ctx.seqs
            .iter()
            .zip(ctx.messages.iter())
            .map(|(seq, m)| {
                (
                    *seq,
                    json!({"seq": seq, "kind": "message", "lane": MAIN_LANE, "message": message_value(m)}),
                )
            })
            .collect()
    } else {
        loaded
            .records
            .iter()
            .map(|r| (r.seq, record_json(r)))
            .collect()
    };

    let mut records = Vec::new();
    let mut bytes = 0usize;
    let mut has_more = false;
    let mut next_after = query.after;
    // The cursor is the last seq returned. A context view that starts at a
    // compaction is not in seq order (the summary carries the compaction's
    // seq, the kept messages lower ones), so it resumes after the entry with
    // that seq instead of filtering by magnitude.
    let skip = if query.after == 0 {
        0
    } else if query.view_context {
        candidates
            .iter()
            .position(|(seq, _)| *seq == query.after)
            .map(|p| p + 1)
            .unwrap_or_else(|| {
                candidates
                    .iter()
                    .take_while(|(seq, _)| *seq <= query.after)
                    .count()
            })
    } else {
        candidates
            .iter()
            .take_while(|(seq, _)| *seq <= query.after)
            .count()
    };
    for (seq, value) in candidates.into_iter().skip(skip) {
        let size = serde_json::to_string(&value).map(|s| s.len()).unwrap_or(0);
        if records.len() >= query.limit || (!records.is_empty() && bytes + size > PAGE_BYTE_BUDGET)
        {
            has_more = true;
            break;
        }
        bytes += size;
        next_after = seq;
        records.push(value);
    }

    let mut out = json!({
        "workspaceId": guest_workspace_id,
        "threadId": thread,
        "source": loaded.source.as_str(),
        "legacy": loaded.legacy,
        "integrity": loaded.integrity.as_api_string(),
        "head": head_json(&loaded.summary.head),
        "nextAfter": next_after,
        "hasMore": has_more,
        "records": records,
        "plan": plan_from_records(&loaded.records),
    });
    if !context_info.is_null() {
        out["context"] = context_info;
    }
    respond(out)
}
