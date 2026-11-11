//! Thread log v1 (`vam.thread-log`): an append-only, hash-chained record of a
//! thread's conversation, stored in the workspace under
//! `.vam/threads/<thread>/log/`.
//!
//! ```text
//! .vam/threads/<thread>/log/<first_seq:08>.jsonl
//! ```
//!
//! One JSON object per line, newline-terminated. A line is committed only when
//! its newline is on disk. Every record carries a header
//! (`v, seq, id, prev, ts, run, lane, agent, parent_lane, round, src, kind,
//! body_sha256`) and a `body`. The chain covers the canonical header, which
//! includes `body_sha256`, not raw line bytes: a body can later be removed
//! (`redact`) without breaking the chain.
//!
//! * Segments are named by the first seq they hold. A segment is sealed once a
//!   later one exists and is never rewritten, so Drive9 CAS dedupes it.
//! * Rotation at [`SEGMENT_MAX_BYTES`]; a record larger than that gets a
//!   segment of its own. A record above [`MAX_RECORD_BYTES`] is not written; a
//!   `gap` record takes its place.
//! * Readers verify the chain, skip a torn (unterminated) last line, skip
//!   unknown kinds, ignore unknown fields inside `v: 1`, and refuse `v: 2`.
//! * The writer repairs a torn tail before the next append.
//!
//! Path handling: thread ids go through [`sanitize_thread_id`]; every directory
//! on the way (`.vam`, `threads`, `<thread>`, `log`) must be a real directory
//! and files open with `O_NOFOLLOW`. With the `Directory` backend the guest
//! process shares `.vam` writable, so a check-then-open race remains in theory;
//! with `Ext4Image` the guest never touches the host tree (decision 3).
//!
//! A batch passed to [`ThreadStore::append`] is fsynced once at its end. A crash
//! inside a batch can leave a prefix of it; the next request's full message list
//! restores what is missing, so the log self-heals.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use thiserror::Error;

use crate::cas::{sha256_hex, ChunkStore};
use crate::manifest::ManifestManager;
use crate::vam_dir::{sanitize_thread_id, ThreadMeta, THREADS_DIR, THREAD_JSON, VAM_DIR};

pub const LOG_FORMAT: &str = "vam.thread-log";
pub const LOG_VERSION: u32 = 1;
pub const LOG_DIR: &str = "log";
/// A segment rotates once adding a record would pass this size.
pub const SEGMENT_MAX_BYTES: u64 = 4 * 1024 * 1024;
/// One record line above this is never written.
pub const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;
/// The assembled live context is refused above this.
pub const MAX_CONTEXT_BYTES: usize = 16 * 1024 * 1024;
/// A segment is read whole; this bounds what a hostile file can make us allocate.
const MAX_SEGMENT_READ_BYTES: u64 = (MAX_RECORD_BYTES as u64) * 2 + SEGMENT_MAX_BYTES;
pub const MAIN_LANE: &str = "main";

pub mod kind {
    pub const TASK_START: &str = "task_start";
    pub const MESSAGE: &str = "message";
    pub const PROMPT_CHANGE: &str = "prompt_change";
    pub const SUSPEND: &str = "suspend";
    pub const TASK_END: &str = "task_end";
    pub const COMPACTION: &str = "compaction";
    pub const ROLLBACK: &str = "rollback";
    pub const COMMIT: &str = "commit";
    pub const FORK: &str = "fork";
    pub const REDACT: &str = "redact";
    pub const GAP: &str = "gap";
    /// Reserved: text a context source injected (source, kind, digest, size;
    /// the text lives in the host prompt store by digest). Parsed and ignored by
    /// assembly; not written yet.
    pub const CONTEXT_SOURCE: &str = "context_source";
}

#[derive(Debug, Error)]
pub enum ThreadLogError {
    #[error("thread log I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid thread id: {0}")]
    InvalidThread(String),
    #[error("unsafe path in thread log: {0}")]
    UnsafePath(String),
    #[error("thread log record {seq} has unsupported version {version} (this reader supports v1)")]
    UnsupportedVersion { seq: u64, version: u64 },
    #[error("thread log is corrupt: {0}")]
    Corrupt(String),
    #[error("assembled context is {bytes} bytes, over the {MAX_CONTEXT_BYTES} byte guard")]
    ContextTooLarge { bytes: usize },
    #[error("thread log serialization failed: {0}")]
    Serialization(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error(
        "the {} log is full: {used} of {limit} bytes in use",
        scope.as_str()
    )]
    QuotaExceeded {
        scope: crate::log_quota::QuotaScope,
        limit: u64,
        used: u64,
    },
}

// ---------------------------------------------------------------------------
// Canonical JSON and digests

/// Compact JSON with object keys sorted; the form every digest is taken over.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&n.to_string()),
        Value::String(s) => out.push_str(&json_string(s)),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, key) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&json_string(key));
                out.push(':');
                if let Some(v) = map.get(key) {
                    write_canonical(v, out);
                }
            }
            out.push('}');
        }
    }
}

fn json_string(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// SHA-256 of the canonical header: the value `prev` of the next record holds.
pub fn header_digest(header: &Map<String, Value>) -> String {
    sha256_hex(canonical_json(&Value::Object(header.clone())).as_bytes())
}

/// `prev` of record 1 of a thread.
pub fn genesis_digest() -> String {
    sha256_hex(b"")
}

const CROCKFORD: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// 26-character ULID: a 48-bit millisecond timestamp and an 80-bit tail taken
/// from `sha256(prev || seq)`, so ids are deterministic and sort by time.
pub fn record_id(ts_ms: u64, prev: &str, seq: u64) -> String {
    let tail = sha256_hex(format!("{prev}:{seq}").as_bytes());
    let mut value: u128 = ((ts_ms & 0xFFFF_FFFF_FFFF) as u128) << 80;
    let mut low: u128 = 0;
    for i in 0..10 {
        let byte = u8::from_str_radix(&tail[i * 2..i * 2 + 2], 16).unwrap_or(0);
        low = (low << 8) | byte as u128;
    }
    value |= low;
    let mut chars = [b'0'; 26];
    for (i, slot) in chars.iter_mut().enumerate() {
        let shift = 5 * (25 - i);
        *slot = CROCKFORD[((value >> shift) & 0x1F) as usize];
    }
    String::from_utf8_lossy(&chars).into_owned()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// `2026-10-19T10:00:00.123Z`
pub fn iso_timestamp(ms: u64) -> String {
    let secs = (ms / 1000) as i64;
    let millis = ms % 1000;
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

// ---------------------------------------------------------------------------
// Messages as typed parts

/// A content part of a logged message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Part {
    Text {
        text: String,
    },
    Image {
        #[serde(rename = "ref")]
        reference: String,
    },
    ToolUse {
        id: String,
        name: String,
        /// The model's arguments string, byte-exact.
        arguments: String,
    },
    ToolResult {
        tool_use_id: String,
        content: Vec<Part>,
    },
    Reasoning {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    Opaque {
        provider: String,
        #[serde(rename = "kind")]
        item_type: String,
        data: Value,
    },
    /// A part type this reader does not know. Kept out of the wire form.
    #[serde(other)]
    Unknown,
}

/// A message as the model saw it, in the plain chat wire shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireMessage {
    pub role: String,
    pub content: String,
    /// `(id, name, arguments)`
    pub tool_calls: Vec<(String, String, String)>,
    pub tool_call_id: String,
}

/// A logged message: a role and typed parts.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogMessage {
    pub role: String,
    pub parts: Vec<Part>,
}

impl LogMessage {
    /// Typed parts from the chat wire shape (lossless for what the guest sends).
    pub fn from_wire(wire: &WireMessage) -> Self {
        let mut parts = Vec::new();
        match wire.role.as_str() {
            "tool" => parts.push(Part::ToolResult {
                tool_use_id: wire.tool_call_id.clone(),
                content: vec![Part::Text {
                    text: wire.content.clone(),
                }],
            }),
            _ => {
                if !wire.content.is_empty() || wire.tool_calls.is_empty() {
                    parts.push(Part::Text {
                        text: wire.content.clone(),
                    });
                }
                for (id, name, arguments) in &wire.tool_calls {
                    parts.push(Part::ToolUse {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    });
                }
            }
        }
        Self {
            role: wire.role.clone(),
            parts,
        }
    }

    /// The chat wire shape. `reasoning`, `image`, `opaque` and unknown parts
    /// carry no wire text and are dropped here (they stay in the log).
    pub fn to_wire(&self) -> WireMessage {
        let mut content = String::new();
        let mut tool_calls = Vec::new();
        let mut tool_call_id = String::new();
        for part in &self.parts {
            match part {
                Part::Text { text } => content.push_str(text),
                Part::ToolUse {
                    id,
                    name,
                    arguments,
                } => tool_calls.push((id.clone(), name.clone(), arguments.clone())),
                Part::ToolResult {
                    tool_use_id,
                    content: inner,
                } => {
                    tool_call_id = tool_use_id.clone();
                    for p in inner {
                        if let Part::Text { text } = p {
                            content.push_str(text);
                        }
                    }
                }
                _ => {}
            }
        }
        WireMessage {
            role: self.role.clone(),
            content,
            tool_calls,
            tool_call_id,
        }
    }

    /// Identity of this message for matching a request against the log.
    pub fn msg_sha256(&self) -> String {
        wire_sha256(&self.to_wire())
    }
}

/// Digest of the canonical wire form: `(role, content, tool_calls, tool_call_id)`.
pub fn wire_sha256(wire: &WireMessage) -> String {
    let calls: Vec<Value> = wire
        .tool_calls
        .iter()
        .map(|(id, name, arguments)| {
            serde_json::json!({"id": id, "name": name, "arguments": arguments})
        })
        .collect();
    let value = serde_json::json!({
        "role": wire.role,
        "content": wire.content,
        "tool_calls": calls,
        "tool_call_id": wire.tool_call_id,
    });
    sha256_hex(canonical_json(&value).as_bytes())
}

/// Body of a `message` record.
pub fn message_body(message: &LogMessage, usage: Option<Value>) -> Value {
    let mut body = Map::new();
    body.insert(
        "msg".into(),
        serde_json::to_value(message).unwrap_or(Value::Null),
    );
    body.insert("msg_sha256".into(), Value::String(message.msg_sha256()));
    if let Some(usage) = usage {
        body.insert("usage".into(), usage);
    }
    Value::Object(body)
}

// ---------------------------------------------------------------------------
// Records

/// One parsed, header-verified record.
#[derive(Debug, Clone, PartialEq)]
pub struct Record {
    pub v: u32,
    pub seq: u64,
    pub id: String,
    pub prev: String,
    pub ts: String,
    pub run: String,
    pub lane: String,
    pub agent: String,
    pub parent_lane: Option<String>,
    pub round: u64,
    pub src: String,
    pub kind: String,
    pub body_sha256: String,
    /// `None` when the body was removed (see `redact`).
    pub body: Option<Value>,
    /// Digest of this record's canonical header; the next record's `prev`.
    pub digest: String,
}

impl Record {
    /// The logged message of a `message` record.
    pub fn message(&self) -> Option<LogMessage> {
        if self.kind != kind::MESSAGE {
            return None;
        }
        let msg = self.body.as_ref()?.get("msg")?;
        serde_json::from_value(msg.clone()).ok()
    }

    pub fn head(&self) -> LogHead {
        LogHead {
            seq: self.seq,
            digest: self.digest.clone(),
        }
    }
}

/// A record to append. The store assigns `seq`, `id`, `prev`, `ts` and the digests.
#[derive(Debug, Clone)]
pub struct NewRecord {
    pub run: String,
    pub lane: String,
    pub agent: String,
    pub parent_lane: Option<String>,
    pub round: u64,
    pub src: String,
    pub kind: String,
    pub body: Value,
}

impl NewRecord {
    pub fn new(kind: &str, src: &str, body: Value) -> Self {
        Self {
            run: String::new(),
            lane: MAIN_LANE.to_string(),
            agent: String::new(),
            parent_lane: None,
            round: 0,
            src: src.to_string(),
            kind: kind.to_string(),
            body,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogHead {
    pub seq: u64,
    pub digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppendReceipt {
    pub first_seq: u64,
    pub head: LogHead,
}

/// Result of verifying a chain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    Ok,
    BrokenAt { seq: u64, reason: String },
}

impl Integrity {
    /// `ok` or `broken_at:<seq>`, as the read API reports it.
    pub fn as_api_string(&self) -> String {
        match self {
            Integrity::Ok => "ok".to_string(),
            Integrity::BrokenAt { seq, .. } => format!("broken_at:{seq}"),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReadPage {
    /// Records with `seq > after`, up to the limit or the first break.
    pub records: Vec<Record>,
    pub next_after: u64,
    pub integrity: Integrity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ThreadSummary {
    pub thread_id: String,
    pub meta: Option<ThreadMeta>,
    pub head: Option<LogHead>,
    pub log_bytes: u64,
}

enum LineFail {
    Broken(String),
    Version(u64),
}

fn get_str(map: &Map<String, Value>, key: &str) -> Result<String, LineFail> {
    match map.get(key) {
        Some(Value::String(s)) => Ok(s.clone()),
        _ => Err(LineFail::Broken(format!("missing or invalid `{key}`"))),
    }
}

fn get_u64(map: &Map<String, Value>, key: &str) -> Result<u64, LineFail> {
    map.get(key)
        .and_then(Value::as_u64)
        .ok_or_else(|| LineFail::Broken(format!("missing or invalid `{key}`")))
}

fn parse_line(line: &[u8]) -> Result<Record, LineFail> {
    let value: Value = serde_json::from_slice(line)
        .map_err(|e| LineFail::Broken(format!("not valid JSON: {e}")))?;
    let Value::Object(mut map) = value else {
        return Err(LineFail::Broken("record is not an object".into()));
    };
    let version = get_u64(&map, "v")?;
    if version != LOG_VERSION as u64 {
        return Err(LineFail::Version(version));
    }
    let body = map.remove("body").filter(|b| !b.is_null());
    let header_digest = header_digest(&map);
    Ok(Record {
        v: LOG_VERSION,
        seq: get_u64(&map, "seq")?,
        id: get_str(&map, "id")?,
        prev: get_str(&map, "prev")?,
        ts: get_str(&map, "ts")?,
        run: get_str(&map, "run")?,
        lane: get_str(&map, "lane")?,
        agent: get_str(&map, "agent")?,
        parent_lane: match map.get("parent_lane") {
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        },
        round: get_u64(&map, "round")?,
        src: get_str(&map, "src")?,
        kind: get_str(&map, "kind")?,
        body_sha256: get_str(&map, "body_sha256")?,
        body,
        digest: header_digest,
    })
}

/// Builds one record line (without the newline) and its digest.
fn build_line(
    rec: &NewRecord,
    seq: u64,
    prev: &str,
    ts_ms: u64,
) -> (String, String, Map<String, Value>) {
    let mut header = Map::new();
    header.insert("v".into(), Value::from(LOG_VERSION));
    header.insert("seq".into(), Value::from(seq));
    header.insert("id".into(), Value::String(record_id(ts_ms, prev, seq)));
    header.insert("prev".into(), Value::String(prev.to_string()));
    header.insert("ts".into(), Value::String(iso_timestamp(ts_ms)));
    header.insert("run".into(), Value::String(rec.run.clone()));
    header.insert("lane".into(), Value::String(rec.lane.clone()));
    header.insert("agent".into(), Value::String(rec.agent.clone()));
    if let Some(parent) = &rec.parent_lane {
        header.insert("parent_lane".into(), Value::String(parent.clone()));
    }
    header.insert("round".into(), Value::from(rec.round));
    header.insert("src".into(), Value::String(rec.src.clone()));
    header.insert("kind".into(), Value::String(rec.kind.clone()));
    header.insert(
        "body_sha256".into(),
        Value::String(sha256_hex(canonical_json(&rec.body).as_bytes())),
    );
    let digest = header_digest(&header);
    let mut full = header.clone();
    full.insert("body".into(), rec.body.clone());
    (canonical_json(&Value::Object(full)), digest, header)
}

// ---------------------------------------------------------------------------
// Segment sources and the shared read path

/// One segment as the source lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentInfo {
    pub first_seq: u64,
    pub len: u64,
}

/// Where segment bytes come from: a session tree or the HEAD manifest.
pub trait SegmentSource {
    fn thread_ids(&self) -> Result<Vec<String>, ThreadLogError>;
    /// Segments of a validated thread, sorted by `first_seq`.
    fn segments(&self, thread: &str) -> Result<Vec<SegmentInfo>, ThreadLogError>;
    fn read_segment(&self, thread: &str, first_seq: u64) -> Result<Vec<u8>, ThreadLogError>;
    fn thread_meta(&self, thread: &str) -> Result<Option<ThreadMeta>, ThreadLogError>;
}

fn parse_segment_name(name: &str) -> Option<u64> {
    let stem = name.strip_suffix(".jsonl")?;
    if stem.len() < 8 || !stem.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    stem.parse::<u64>().ok().filter(|n| *n >= 1)
}

fn segment_file_name(first_seq: u64) -> String {
    format!("{first_seq:08}.jsonl")
}

fn validate_thread(thread: &str) -> Result<String, ThreadLogError> {
    let id =
        sanitize_thread_id(thread).map_err(|e| ThreadLogError::InvalidThread(e.to_string()))?;
    Ok(id)
}

mod read;
mod tree;

pub use read::{
    assemble_context, read_all_records, records_without_rollbacks, verify_records, verify_thread,
    AssembledContext, ThreadLogReader, ThreadStore,
};
pub use tree::{HeadThreadReader, TreeThreadStore};

#[cfg(test)]
mod tests;
