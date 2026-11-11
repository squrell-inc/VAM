//! Reserved `.vam/` workspace layout for portable thread continuity.
//!
//! ```text
//! .vam/
//! ├── workspace.json
//! └── threads/<thread_id>/
//!     ├── thread.json
//!     └── timeline.jsonl   # host-only; includes trajectory digest + file pointers
//! ```
//!
//! Guest `.vam/agent/observations.jsonl` is observational only and is never replayed.

use std::fs::{self, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use prost::Message;
use prost_types::Timestamp;

use crate::cas::{sha256_hex, ChunkStore};
use crate::error::Drive9Error;
use crate::manifest::ManifestManager;
use vam_spec::v1::{ResourceId, ThreadTurnPayload, VamCloudEvent};

pub const VAM_DIR: &str = ".vam";
pub const WORKSPACE_JSON: &str = "workspace.json";
pub const WORKSPACE_JSON_PATH: &str = ".vam/workspace.json";
pub const THREADS_DIR: &str = "threads";
pub const THREAD_JSON: &str = "thread.json";
pub const TIMELINE_JSONL: &str = "timeline.jsonl";
pub const VAM_SPEC_VERSION: &str = "0.1";
pub const DEFAULT_THREAD_REF: &str = "main";
pub const THREAD_TURN_EVENT_TYPE: &str = "vam.v1.thread.turn";
/// Maximum prior turns replayed into the model prompt (full history stays in timeline.jsonl).
pub const DEFAULT_REPLAY_TURNS: usize = 10;
/// Maximum workspace-relative paths stored on a thread turn summary.
pub const MAX_TOUCHED_PATHS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrajectorySummary {
    pub trajectory_digest: String,
    pub touched_paths: Vec<String>,
    pub skill_ids: Vec<String>,
    pub tool_names: Vec<String>,
}

/// Catalog version pinned on a workspace (`1.0.0` or `1.0.0-draft`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WorkspaceAgentVersion {
    pub major: i32,
    pub minor: i32,
    pub patch: i32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub prerelease: String,
}

impl WorkspaceAgentVersion {
    pub fn from_spec(version: &vam_spec::v1::Version) -> Self {
        Self {
            major: version.major,
            minor: version.minor,
            patch: version.patch,
            prerelease: version.prerelease.clone(),
        }
    }

    pub fn to_spec(&self) -> vam_spec::v1::Version {
        vam_spec::v1::Version {
            major: self.major,
            minor: self.minor,
            patch: self.patch,
            prerelease: self.prerelease.clone(),
            build: String::new(),
        }
    }

    /// True for a draft pin (`prerelease == "draft"`). Its numbers are ignored.
    pub fn is_draft(&self) -> bool {
        self.prerelease == "draft"
    }

    pub fn label(&self) -> String {
        if self.prerelease.is_empty() {
            format!("{}.{}.{}", self.major, self.minor, self.patch)
        } else {
            format!(
                "{}.{}.{}-{}",
                self.major, self.minor, self.patch, self.prerelease
            )
        }
    }

    /// A draft pin has no version. Any two draft pins match, whatever their
    /// numbers. Released pins compare every field.
    pub fn matches(&self, other: &Self) -> bool {
        if self.is_draft() || other.is_draft() {
            return self.is_draft() && other.is_draft();
        }
        self.major == other.major
            && self.minor == other.minor
            && self.patch == other.patch
            && self.prerelease == other.prerelease
    }
}

/// First-run bind of an agent URN (and optional catalog version) onto a workspace.
#[derive(Debug, Clone, Copy)]
pub struct WorkspaceAgentBind<'a> {
    pub agent_urn: &'a str,
    pub agent_digest: &'a str,
    pub agent_version: Option<&'a WorkspaceAgentVersion>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct WorkspaceMeta {
    pub workspace_id: String,
    pub spec_version: String,
    pub created_at: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_urn: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<WorkspaceAgentVersion>,
}

/// Marks a thread as carrying a `vam.thread-log` under `log/`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ThreadLogInfo {
    pub format: String,
    pub version: u32,
}

impl ThreadLogInfo {
    pub fn v1() -> Self {
        Self {
            format: crate::thread_log::LOG_FORMAT.to_string(),
            version: crate::thread_log::LOG_VERSION,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct ThreadMeta {
    pub thread_id: String,
    pub agent_urn: String,
    pub agent_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_thread: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fork_turn: Option<u64>,
    pub created_at: i64,
    /// Present on threads that carry a thread log (older files omit it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<ThreadLogInfo>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct TurnRecord {
    pub seq: u64,
    pub prev_hash: String,
    pub run_id: String,
    pub prompt: String,
    pub answer: String,
    pub model: String,
    pub manifest_hash: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spend_micro: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trajectory_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub touched_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_names: Vec<String>,
    /// The run's range in the thread log and the log head after `task_end`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_seq_from: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_seq_to: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_head_digest: Option<String>,
    /// The workspace HEAD when the task started (same value as `manifest_hash`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_before: Option<String>,
    /// Hash of the files the task left, without the host `.vam/`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_hash: Option<String>,
    pub timestamp: i64,
}

/// A run's position in the thread log, recorded on its timeline turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnLogRange {
    pub seq_from: u64,
    pub seq_to: u64,
    pub head_digest: String,
}

/// The workspace on both sides of a task, recorded on its timeline turn. The
/// HEAD after the task cannot be here (it contains the timeline); the files
/// hash can.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnWorkspace {
    pub head_before: String,
    pub files_hash: String,
}

pub fn sanitize_thread_id(thread_id: &str) -> Result<String, Drive9Error> {
    let trimmed = thread_id.trim();
    if trimmed.is_empty() {
        return Ok(DEFAULT_THREAD_REF.to_string());
    }
    if trimmed.chars().all(|c| c == '.') || trimmed.contains('/') || trimmed.contains('\\') {
        return Err(Drive9Error::Layout(format!(
            "invalid thread id '{}'",
            thread_id
        )));
    }
    if !trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(Drive9Error::Layout(format!(
            "invalid thread id '{}'",
            thread_id
        )));
    }
    Ok(trimmed.to_string())
}

fn vam_root(materialized: &Path) -> PathBuf {
    materialized.join(VAM_DIR)
}

fn thread_dir(materialized: &Path, thread_id: &str) -> PathBuf {
    vam_root(materialized).join(THREADS_DIR).join(thread_id)
}

fn genesis_hash() -> String {
    sha256_hex(b"")
}

pub fn hash_turn_record(record: &TurnRecord) -> String {
    let json = serde_json::to_string(record).unwrap_or_default();
    sha256_hex(json.as_bytes())
}

/// Hashes the canonical JSON encoding of a timeline CloudEvent line.
pub fn hash_cloudevent_json(json: &str) -> String {
    sha256_hex(json.as_bytes())
}

pub fn read_workspace_meta(materialized: &Path) -> Result<Option<WorkspaceMeta>, Drive9Error> {
    let path = vam_root(materialized).join(WORKSPACE_JSON);
    if !path.exists() {
        return Ok(None);
    }
    let raw = fs::read_to_string(&path)?;
    let meta: WorkspaceMeta = serde_json::from_str(&raw)
        .map_err(|e| Drive9Error::Layout(format!("invalid workspace.json: {}", e)))?;
    Ok(Some(meta))
}

/// Reads bound workspace metadata from the Drive9 HEAD manifest without materializing a session.
pub fn read_workspace_meta_from_head(
    manifest_manager: &ManifestManager,
    chunk_store: &ChunkStore,
    workspace_id: &str,
) -> Result<Option<WorkspaceMeta>, Drive9Error> {
    let Some(head) = manifest_manager.read_head(workspace_id)? else {
        return Ok(None);
    };
    let manifest = manifest_manager.get_manifest(workspace_id, &head)?;
    let Some(file) = manifest
        .files
        .iter()
        .find(|f| f.path == WORKSPACE_JSON_PATH)
    else {
        return Ok(None);
    };
    let mut data = Vec::new();
    for chunk_hash in &file.chunks {
        data.extend_from_slice(&chunk_store.get_chunk(chunk_hash)?);
    }
    let meta: WorkspaceMeta = serde_json::from_slice(&data)
        .map_err(|e| Drive9Error::Layout(format!("invalid workspace.json: {}", e)))?;
    Ok(Some(meta))
}

pub fn ensure_workspace_json(
    materialized: &Path,
    workspace_id: &str,
    bind_agent: Option<WorkspaceAgentBind<'_>>,
) -> Result<WorkspaceMeta, Drive9Error> {
    let vam = vam_root(materialized);
    fs::create_dir_all(&vam)?;
    let path = vam.join(WORKSPACE_JSON);
    if path.exists() {
        let raw = fs::read_to_string(&path)?;
        let mut meta: WorkspaceMeta = serde_json::from_str(&raw)
            .map_err(|e| Drive9Error::Layout(format!("invalid workspace.json: {}", e)))?;
        if let Some(bind) = bind_agent {
            if let Some(bound) = meta.agent_urn.as_deref() {
                if bound != bind.agent_urn {
                    return Err(Drive9Error::Layout(format!(
                        "workspace '{}' is bound to agent '{}', cannot bind '{}'",
                        workspace_id, bound, bind.agent_urn
                    )));
                }
            } else {
                meta.agent_urn = Some(bind.agent_urn.to_string());
                meta.agent_digest = Some(bind.agent_digest.to_string());
            }
            if let Some(pin) = bind.agent_version {
                if let Some(bound_version) = meta.agent_version.as_ref() {
                    if !bound_version.matches(pin) {
                        return Err(Drive9Error::Layout(format!(
                            "workspace '{}' is bound to agent version '{}', cannot bind '{}'",
                            workspace_id,
                            bound_version.label(),
                            pin.label()
                        )));
                    }
                } else {
                    meta.agent_version = Some(pin.clone());
                }
            }
            fs::write(
                &path,
                serde_json::to_string_pretty(&meta)
                    .map_err(|e| Drive9Error::Serialization(e.to_string()))?,
            )?;
        }
        return Ok(meta);
    }
    let mut meta = WorkspaceMeta {
        workspace_id: workspace_id.to_string(),
        spec_version: VAM_SPEC_VERSION.to_string(),
        created_at: now_secs(),
        agent_urn: None,
        agent_digest: None,
        agent_version: None,
    };
    if let Some(bind) = bind_agent {
        meta.agent_urn = Some(bind.agent_urn.to_string());
        meta.agent_digest = Some(bind.agent_digest.to_string());
        meta.agent_version = bind.agent_version.cloned();
    }
    fs::write(
        &path,
        serde_json::to_string_pretty(&meta)
            .map_err(|e| Drive9Error::Serialization(e.to_string()))?,
    )?;
    Ok(meta)
}

pub fn ensure_thread_json(
    materialized: &Path,
    thread_id: &str,
    agent_urn: &str,
    agent_digest: &str,
) -> Result<ThreadMeta, Drive9Error> {
    let dir = thread_dir(materialized, thread_id);
    fs::create_dir_all(&dir)?;
    let path = dir.join(THREAD_JSON);
    if path.exists() {
        let raw = fs::read_to_string(&path)?;
        return serde_json::from_str(&raw)
            .map_err(|e| Drive9Error::Layout(format!("invalid thread.json: {}", e)));
    }
    let meta = ThreadMeta {
        thread_id: thread_id.to_string(),
        agent_urn: agent_urn.to_string(),
        agent_digest: agent_digest.to_string(),
        parent_thread: None,
        fork_turn: None,
        created_at: now_secs(),
        log: Some(ThreadLogInfo::v1()),
    };
    fs::write(
        &path,
        serde_json::to_string_pretty(&meta)
            .map_err(|e| Drive9Error::Serialization(e.to_string()))?,
    )?;
    Ok(meta)
}

/// Parses the text of a `timeline.jsonl`.
pub fn parse_timeline(text: &str) -> Result<Vec<TurnRecord>, Drive9Error> {
    let mut turns = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        turns.push(parse_timeline_line(trimmed)?);
    }
    Ok(turns)
}

fn parse_timeline_line(line: &str) -> Result<TurnRecord, Drive9Error> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return Err(Drive9Error::Layout("empty timeline line".into()));
    }
    if trimmed.contains("\"type\"") || trimmed.contains("\"eventType\"") {
        return cloudevent_line_to_turn(trimmed);
    }
    serde_json::from_str(trimmed)
        .map_err(|e| Drive9Error::Layout(format!("invalid timeline record: {}", e)))
}

fn cloudevent_line_to_turn(json: &str) -> Result<TurnRecord, Drive9Error> {
    let event: VamCloudEvent = vam_spec::encoding::json_to_proto("vam.v1.VamCloudEvent", json)
        .map_err(|e| Drive9Error::Layout(format!("invalid CloudEvent timeline line: {}", e)))?;
    let payload = event
        .data
        .and_then(|any| ThreadTurnPayload::decode(any.value.as_slice()).ok())
        .ok_or_else(|| {
            Drive9Error::Layout("timeline CloudEvent missing ThreadTurnPayload".into())
        })?;
    let run_id = event.run_id.map(|r| r.urn).unwrap_or_default();
    let seq = event.sequence_number.max(0) as u64;
    Ok(TurnRecord {
        seq,
        prev_hash: payload.prev_hash,
        run_id,
        prompt: payload.prompt,
        answer: payload.answer,
        model: payload.model,
        manifest_hash: payload.manifest_hash,
        spend_micro: payload.spend_micro,
        trajectory_digest: payload.trajectory_digest,
        touched_paths: payload.touched_paths,
        skill_ids: payload.skill_ids,
        tool_names: payload.tool_names,
        log_seq_from: payload.log_seq_from,
        log_seq_to: payload.log_seq_to,
        log_head_digest: payload.log_head_digest,
        head_before: payload.head_before,
        files_hash: payload.files_hash,
        timestamp: event.time.map(|t| t.seconds).unwrap_or_else(now_secs),
    })
}

pub fn read_timeline(materialized: &Path, thread_id: &str) -> Result<Vec<TurnRecord>, Drive9Error> {
    let path = thread_dir(materialized, thread_id).join(TIMELINE_JSONL);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);
    let mut turns = Vec::new();
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        turns.push(parse_timeline_line(trimmed)?);
    }
    Ok(turns)
}

#[allow(clippy::too_many_arguments)] // public API used by vam-orchestrator; argument struct is a backlog item
fn build_thread_turn_event(
    run_id: &str,
    seq: u64,
    prev_hash: &str,
    prompt: &str,
    answer: &str,
    model: &str,
    manifest_hash: &str,
    spend_micro: Option<i64>,
    trajectory: Option<&TrajectorySummary>,
    log: Option<&TurnLogRange>,
    workspace: Option<&TurnWorkspace>,
) -> Result<(VamCloudEvent, String), Drive9Error> {
    let payload = ThreadTurnPayload {
        prompt: prompt.to_string(),
        answer: answer.to_string(),
        model: model.to_string(),
        manifest_hash: manifest_hash.to_string(),
        prev_hash: prev_hash.to_string(),
        spend_micro,
        trajectory_digest: trajectory.map(|t| t.trajectory_digest.clone()),
        touched_paths: trajectory
            .map(|t| t.touched_paths.clone())
            .unwrap_or_default(),
        skill_ids: trajectory.map(|t| t.skill_ids.clone()).unwrap_or_default(),
        tool_names: trajectory.map(|t| t.tool_names.clone()).unwrap_or_default(),
        log_seq_from: log.map(|l| l.seq_from),
        log_seq_to: log.map(|l| l.seq_to),
        log_head_digest: log.map(|l| l.head_digest.clone()),
        head_before: workspace.map(|w| w.head_before.clone()),
        files_hash: workspace.map(|w| w.files_hash.clone()),
    };
    let mut buf = Vec::new();
    payload
        .encode(&mut buf)
        .map_err(|e| Drive9Error::Serialization(e.to_string()))?;
    let now = now_secs();
    let event = VamCloudEvent {
        id: format!("turn-{}-{}", run_id, seq),
        source: Some(ResourceId {
            urn: "urn:vam:emitter:local.vam.dev/host/storage".to_string(),
        }),
        spec_version: "1.0".to_string(),
        r#type: THREAD_TURN_EVENT_TYPE.to_string(),
        time: Some(Timestamp {
            seconds: now,
            nanos: 0,
        }),
        subject: None,
        data_content_type: "application/json".to_string(),
        data: Some(prost_types::Any {
            type_url: "type.googleapis.com/vam.v1.ThreadTurnPayload".to_string(),
            value: buf,
        }),
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        sequence_number: seq as i64,
        trace_context: None,
    };
    let json = vam_spec::encoding::proto_to_json("vam.v1.VamCloudEvent", &event)
        .map_err(Drive9Error::Serialization)?;
    Ok((event, json))
}

#[allow(clippy::too_many_arguments)] // public API used by vam-orchestrator; argument struct is a backlog item
pub fn append_turn(
    materialized: &Path,
    thread_id: &str,
    run_id: &str,
    prompt: &str,
    answer: &str,
    model: &str,
    manifest_hash: &str,
    spend_micro: Option<i64>,
    trajectory: Option<&TrajectorySummary>,
) -> Result<TurnRecord, Drive9Error> {
    append_turn_with_log(
        materialized,
        thread_id,
        run_id,
        prompt,
        answer,
        model,
        manifest_hash,
        spend_micro,
        trajectory,
        None,
    )
}

/// [`append_turn`] that also records the run's thread-log range.
#[allow(clippy::too_many_arguments)] // public API used by vam-orchestrator; argument struct is a backlog item
pub fn append_turn_with_log(
    materialized: &Path,
    thread_id: &str,
    run_id: &str,
    prompt: &str,
    answer: &str,
    model: &str,
    manifest_hash: &str,
    spend_micro: Option<i64>,
    trajectory: Option<&TrajectorySummary>,
    log: Option<&TurnLogRange>,
) -> Result<TurnRecord, Drive9Error> {
    append_turn_full(
        materialized,
        thread_id,
        run_id,
        prompt,
        answer,
        model,
        manifest_hash,
        spend_micro,
        trajectory,
        log,
        None,
    )
}

/// [`append_turn_with_log`] that also records the workspace on both sides of
/// the task (`manifest_hash` is the HEAD before it).
#[allow(clippy::too_many_arguments)] // public API used by vam-orchestrator; argument struct is a backlog item
pub fn append_turn_full(
    materialized: &Path,
    thread_id: &str,
    run_id: &str,
    prompt: &str,
    answer: &str,
    model: &str,
    manifest_hash: &str,
    spend_micro: Option<i64>,
    trajectory: Option<&TrajectorySummary>,
    log: Option<&TurnLogRange>,
    workspace: Option<&TurnWorkspace>,
) -> Result<TurnRecord, Drive9Error> {
    let timeline = read_timeline(materialized, thread_id)?;
    let (seq, prev_hash) = if let Some(last) = timeline.last() {
        let last_json = last_cloudevent_json(materialized, thread_id, last.seq)?
            .unwrap_or_else(|| serde_json::to_string(last).unwrap_or_default());
        (last.seq + 1, hash_cloudevent_json(&last_json))
    } else {
        (1, genesis_hash())
    };
    let (_, event_json) = build_thread_turn_event(
        run_id,
        seq,
        &prev_hash,
        prompt,
        answer,
        model,
        manifest_hash,
        spend_micro,
        trajectory,
        log,
        workspace,
    )?;
    let dir = thread_dir(materialized, thread_id);
    fs::create_dir_all(&dir)?;
    let path = dir.join(TIMELINE_JSONL);
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "{}", event_json)?;
    Ok(TurnRecord {
        seq,
        prev_hash,
        run_id: run_id.to_string(),
        prompt: prompt.to_string(),
        answer: answer.to_string(),
        model: model.to_string(),
        manifest_hash: manifest_hash.to_string(),
        spend_micro,
        trajectory_digest: trajectory.map(|t| t.trajectory_digest.clone()),
        touched_paths: trajectory
            .map(|t| t.touched_paths.clone())
            .unwrap_or_default(),
        skill_ids: trajectory.map(|t| t.skill_ids.clone()).unwrap_or_default(),
        tool_names: trajectory.map(|t| t.tool_names.clone()).unwrap_or_default(),
        log_seq_from: log.map(|l| l.seq_from),
        log_seq_to: log.map(|l| l.seq_to),
        log_head_digest: log.map(|l| l.head_digest.clone()),
        head_before: workspace.map(|w| w.head_before.clone()),
        files_hash: workspace.map(|w| w.files_hash.clone()),
        timestamp: now_secs(),
    })
}

fn last_cloudevent_json(
    materialized: &Path,
    thread_id: &str,
    seq: u64,
) -> Result<Option<String>, Drive9Error> {
    let path = thread_dir(materialized, thread_id).join(TIMELINE_JSONL);
    if !path.exists() {
        return Ok(None);
    }
    let file = fs::File::open(&path)?;
    let reader = BufReader::new(file);
    for line in reader.lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains("\"type\"") || trimmed.contains("\"eventType\"") {
            if let Ok(record) = cloudevent_line_to_turn(trimmed) {
                if record.seq == seq {
                    return Ok(Some(trimmed.to_string()));
                }
            }
        }
    }
    Ok(None)
}

/// Replays a bounded window of prior turns, then appends the current user prompt.
pub fn replay_prompt(timeline: &[TurnRecord], current_prompt: &str, max_turns: usize) -> String {
    if timeline.is_empty() {
        return current_prompt.to_string();
    }
    let start = timeline.len().saturating_sub(max_turns);
    let window = &timeline[start..];
    let mut out = String::from("Previous conversation (most recent last):\n\n");
    for turn in window {
        out.push_str(&format!("User: {}\n", turn.prompt));
        out.push_str(&format!("Assistant: {}\n", turn.answer));
        if let Some(ref digest) = turn.trajectory_digest {
            if !digest.is_empty() {
                let skills = if turn.skill_ids.is_empty() {
                    String::new()
                } else {
                    format!(" skills={}", turn.skill_ids.join(","))
                };
                let tools = if turn.tool_names.is_empty() {
                    String::new()
                } else {
                    format!(" tools={}", turn.tool_names.join(","))
                };
                out.push_str(&format!(
                    "  [run {} trajectory digest={}{}{}]\n",
                    turn.run_id, digest, skills, tools
                ));
            }
        }
        if !turn.touched_paths.is_empty() {
            out.push_str(&format!(
                "  [workspace files: {}]\n",
                turn.touched_paths.join(", ")
            ));
        }
        out.push('\n');
    }
    out.push_str("Durable workspace files live in PLAN.md and notes/ — read them with read_file if relevant.\n\n");
    out.push_str(&format!("User: {}", current_prompt));
    out
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn timeline_append_and_replay() {
        let dir = tempdir().unwrap();
        let mat = dir.path();
        let thread = "main";

        append_turn(
            mat,
            thread,
            "run-1",
            "hello",
            "hi there",
            "openai/gpt-4o-mini",
            "abc123",
            None,
            None,
        )
        .unwrap();
        append_turn(
            mat,
            thread,
            "run-2",
            "follow up",
            "sure thing",
            "openai/gpt-4o-mini",
            "def456",
            Some(100),
            None,
        )
        .unwrap();

        let timeline = read_timeline(mat, thread).unwrap();
        assert_eq!(timeline.len(), 2);
        assert_eq!(timeline[1].spend_micro, Some(100));

        let raw = fs::read_to_string(thread_dir(mat, thread).join(TIMELINE_JSONL)).unwrap();
        assert!(raw.contains(THREAD_TURN_EVENT_TYPE));

        let replayed = replay_prompt(&timeline, "third question", 10);
        assert!(replayed.contains("hello"));
        assert!(replayed.contains("follow up"));
        assert!(replayed.contains("third question"));
        assert!(replayed.contains("PLAN.md"));
    }

    #[test]
    fn a_turn_records_its_log_range() {
        let dir = tempdir().unwrap();
        let range = TurnLogRange {
            seq_from: 4,
            seq_to: 9,
            head_digest: "h".repeat(64),
        };
        append_turn_with_log(
            dir.path(),
            "main",
            "run-1",
            "p",
            "a",
            "m",
            "mh",
            None,
            None,
            Some(&range),
        )
        .unwrap();
        append_turn(
            dir.path(),
            "main",
            "run-2",
            "p2",
            "a2",
            "m",
            "mh",
            None,
            None,
        )
        .unwrap();
        let turns = read_timeline(dir.path(), "main").unwrap();
        assert_eq!(turns[0].log_seq_from, Some(4));
        assert_eq!(turns[0].log_seq_to, Some(9));
        assert_eq!(turns[0].log_head_digest.as_deref(), Some(&*"h".repeat(64)));
        assert_eq!(turns[1].log_seq_from, None);
    }

    #[test]
    fn a_turn_records_the_workspace_before_and_the_files_hash_after() {
        let dir = tempdir().unwrap();
        let workspace = TurnWorkspace {
            head_before: "b".repeat(64),
            files_hash: "f".repeat(64),
        };
        append_turn_full(
            dir.path(),
            "main",
            "run-1",
            "p",
            "a",
            "m",
            &workspace.head_before,
            None,
            None,
            None,
            Some(&workspace),
        )
        .unwrap();
        append_turn(
            dir.path(),
            "main",
            "run-2",
            "p2",
            "a2",
            "m",
            "mh",
            None,
            None,
        )
        .unwrap();
        let turns = read_timeline(dir.path(), "main").unwrap();
        assert_eq!(turns[0].head_before.as_deref(), Some(&*"b".repeat(64)));
        assert_eq!(turns[0].files_hash.as_deref(), Some(&*"f".repeat(64)));
        assert_eq!(turns[0].manifest_hash, "b".repeat(64));
        assert_eq!(turns[1].head_before, None);
        assert_eq!(turns[1].files_hash, None);
    }

    #[test]
    fn legacy_turn_record_still_replays() {
        let dir = tempdir().unwrap();
        let mat = dir.path();
        let thread = "main";
        let td = thread_dir(mat, thread);
        fs::create_dir_all(&td).unwrap();
        let legacy = TurnRecord {
            seq: 1,
            prev_hash: genesis_hash(),
            run_id: "run-legacy".into(),
            prompt: "old prompt".into(),
            answer: "old answer".into(),
            model: "test".into(),
            manifest_hash: "abc".into(),
            spend_micro: None,
            trajectory_digest: None,
            touched_paths: Vec::new(),
            skill_ids: Vec::new(),
            tool_names: Vec::new(),
            log_seq_from: None,
            log_seq_to: None,
            log_head_digest: None,
            head_before: None,
            files_hash: None,
            timestamp: 1,
        };
        fs::write(
            td.join(TIMELINE_JSONL),
            format!("{}\n", serde_json::to_string(&legacy).unwrap()),
        )
        .unwrap();

        let timeline = read_timeline(mat, thread).unwrap();
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].prompt, "old prompt");
    }

    #[test]
    fn workspace_binds_agent_on_first_write() {
        let dir = tempdir().unwrap();
        let mat = dir.path();
        let meta = ensure_workspace_json(
            mat,
            "assistant",
            Some(WorkspaceAgentBind {
                agent_urn: "urn:vam:agent:test",
                agent_digest: "digest123",
                agent_version: None,
            }),
        )
        .unwrap();
        assert_eq!(meta.agent_urn.as_deref(), Some("urn:vam:agent:test"));

        let again = ensure_workspace_json(
            mat,
            "assistant",
            Some(WorkspaceAgentBind {
                agent_urn: "urn:vam:agent:test",
                agent_digest: "digest456",
                agent_version: None,
            }),
        )
        .unwrap();
        assert_eq!(again.agent_urn.as_deref(), Some("urn:vam:agent:test"));
    }

    #[test]
    fn workspace_binds_agent_version_on_first_write() {
        let dir = tempdir().unwrap();
        let mat = dir.path();
        let draft = WorkspaceAgentVersion {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: "draft".into(),
        };
        let meta = ensure_workspace_json(
            mat,
            "assistant",
            Some(WorkspaceAgentBind {
                agent_urn: "urn:vam:agent:test",
                agent_digest: "digest123",
                agent_version: Some(&draft),
            }),
        )
        .unwrap();
        assert_eq!(
            meta.agent_version.as_ref().map(|v| v.label()).as_deref(),
            Some("1.0.0-draft")
        );

        let released = WorkspaceAgentVersion {
            major: 1,
            minor: 0,
            patch: 0,
            prerelease: String::new(),
        };
        let err = ensure_workspace_json(
            mat,
            "assistant",
            Some(WorkspaceAgentBind {
                agent_urn: "urn:vam:agent:test",
                agent_digest: "digest123",
                agent_version: Some(&released),
            }),
        )
        .unwrap_err();
        assert!(err.to_string().contains("1.0.0-draft"));
    }

    #[test]
    fn draft_pins_match_regardless_of_numbers() {
        let pin = |major, minor, patch, prerelease: &str| WorkspaceAgentVersion {
            major,
            minor,
            patch,
            prerelease: prerelease.into(),
        };
        let draft_old = pin(0, 1, 1, "draft");
        let draft_new = pin(1, 0, 0, "draft");
        let released = pin(1, 0, 0, "");
        assert!(draft_old.matches(&draft_new));
        assert!(draft_new.matches(&draft_old));
        assert!(!draft_old.matches(&released));
        assert!(!released.matches(&draft_new));
        assert!(pin(0, 1, 1, "").matches(&pin(0, 1, 1, "")));
        assert!(!pin(0, 1, 1, "").matches(&pin(0, 1, 2, "")));
    }

    #[test]
    fn sanitize_rejects_path_traversal() {
        assert!(sanitize_thread_id("../evil").is_err());
        assert_eq!(sanitize_thread_id("").unwrap(), "main");
        assert_eq!(sanitize_thread_id("  feature-x  ").unwrap(), "feature-x");
    }

    #[test]
    fn replay_includes_trajectory_digest_and_paths() {
        let dir = tempdir().unwrap();
        let mat = dir.path();
        let thread = "main";
        let summary = TrajectorySummary {
            trajectory_digest: "abc123def456".repeat(4),
            touched_paths: vec!["notes/hello.txt".into()],
            skill_ids: vec!["db-tools".into()],
            tool_names: vec!["load_skill".into(), "write_file".into()],
        };
        append_turn(
            mat,
            thread,
            "run-1",
            "write a note",
            "done",
            "mock-model",
            "manifest1",
            None,
            Some(&summary),
        )
        .unwrap();

        let timeline = read_timeline(mat, thread).unwrap();
        let replayed = replay_prompt(&timeline, "follow up", 10);
        assert!(replayed.contains("digest="));
        assert!(replayed.contains("notes/hello.txt"));
        assert!(replayed.contains("db-tools"));
        assert!(replayed.contains("load_skill,write_file"));
        assert!(replayed.contains("follow up"));
        assert!(replayed.contains("PLAN.md"));
        assert!(replayed.contains("notes/"));
    }

    #[test]
    fn hash_chain_still_links_turns() {
        let dir = tempdir().unwrap();
        let mat = dir.path();
        let thread = "main";
        let first = append_turn(mat, thread, "run-1", "one", "a1", "m", "h1", None, None).unwrap();
        let second = append_turn(mat, thread, "run-2", "two", "a2", "m", "h2", None, None).unwrap();
        assert_ne!(first.prev_hash, second.prev_hash);
        assert_eq!(first.prev_hash, genesis_hash());
        assert_ne!(second.prev_hash, genesis_hash());
    }
}
