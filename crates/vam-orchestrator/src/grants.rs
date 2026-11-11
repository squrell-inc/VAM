//! Grant issuance (E5, locked decisions 5, 6, 9, 10, 13): what `invoke_hosted_tool` does when a
//! broker tool in the approved `grant_tools` is called with a `run` argument.
//!
//! Two phases, so that nothing is left behind when a check fails:
//! 1. [`check_call`] only reads. It applies the loop guard, decodes `RunGrantRequest` and
//!    resolves everything by id (a foreign id answers like a missing one). Failures are typed
//!    host errors (`<code>: <advice>`) the agent can repair.
//! 2. [`mint`] runs after preflight and the secrets resolve: it creates the Space and thread
//!    rows a `NEW` choice asks for, inserts the grant credential and returns the `RunGrant`.
//!    The returned [`GrantGuard`] revokes the grant and deletes the rows it created unless
//!    [`GrantGuard::disarm`] is called, so a broker failure, an error or a dropped future leaves
//!    nothing behind.

use rusqlite::params;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use vam_spec::v1::{
    CapabilityManifest, GrantTarget, OnBusy, PlaceMode, ResourceRef, RunGrant, RunGrantRequest,
    RunSpec,
};

use crate::credentials::GrantInsert;
use crate::error::OrchestratorError;
use crate::keys::KeyKind;
use crate::spaces::{self, SpaceRow, ThreadRow};
use crate::workflow::SqliteWorkflowStore;

/// Largest `message` a grant binds.
pub const MAX_GRANT_MESSAGE_BYTES: usize = 32 * 1024;
/// A grant with no `expires_at` lives this long (decision 13).
pub const DEFAULT_GRANT_TTL_SECS: i64 = 90 * 86_400;
/// The longest a grant may live (decision 13).
pub const MAX_GRANT_TTL_SECS: i64 = 365 * 86_400;
/// Scope the grant carries (the gateway starts runs with it).
pub const GRANT_SCOPE: &str = "runs:start";
/// The message a loop-guarded tool answers inside a run a trigger started.
pub const LOOP_GUARD_MESSAGE: &str =
    "loop_guarded: this tool cannot be used in a run a trigger started";

/// A playbook pinned at issuance.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaybookPin {
    pub urn: String,
    pub version: String,
    pub digest: String,
}

/// The request a grant binds, stored in `api_keys.constraints_json` (keys are the contract the
/// grant list and the gateway read). The digest in `GrantTarget` covers exactly this.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GrantConstraints {
    pub agent_urn: String,
    pub space_id: String,
    pub thread_id: String,
    pub message: String,
    pub playbook_ref: Option<PlaybookPin>,
    pub inputs: serde_json::Value,
    /// `QUEUE` or `SKIP`.
    pub on_busy: String,
}

impl GrantConstraints {
    /// Sorted-key JSON, the bytes the request digest covers.
    pub fn canonical_json(&self) -> String {
        let value = serde_json::to_value(self).unwrap_or(serde_json::Value::Null);
        canonical(&value).to_string()
    }

    pub fn digest(&self) -> String {
        format!(
            "sha256:{:x}",
            Sha256::digest(self.canonical_json().as_bytes())
        )
    }
}

fn canonical(value: &serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let mut out = serde_json::Map::new();
            for key in keys {
                out.insert(key.clone(), canonical(&map[key]));
            }
            serde_json::Value::Object(out)
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.iter().map(canonical).collect())
        }
        other => other.clone(),
    }
}

/// What the call knows about itself.
pub struct CallFacts<'a> {
    pub broker_id: &'a str,
    /// The tool's export name, as listed in `grant_tools`.
    pub tool: &'a str,
    pub run_id: &'a str,
    /// The run's owner: the grant belongs to this user.
    pub owner: &'a str,
    pub calling_agent: &'a str,
    /// The calling session's workspace (= Space id), used when the run's spec has none.
    pub calling_space_fallback: &'a str,
}

enum SpacePlan {
    Existing(SpaceRow),
    New { label: String },
}

enum ThreadPlan {
    Existing(ThreadRow),
    New { label: String },
}

/// A validated request, ready to mint. Holds no rows.
pub struct GrantPlan {
    agent_urn: String,
    space: SpacePlan,
    thread: ThreadPlan,
    message: String,
    playbook: Option<(PlaybookPin, ResourceRef)>,
    inputs: serde_json::Value,
    on_busy: OnBusy,
    expires_at: i64,
}

fn refuse(code: &str, advice: impl std::fmt::Display) -> String {
    format!("{code}: {advice}")
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

impl SqliteWorkflowStore {
    /// `runs.source` of a run: `broker:<id>` when a trigger started it, `None` for a person.
    /// A missing run is an error (callers that guard fail closed).
    pub fn run_source(&self, run_id: &str) -> Result<Option<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection
            .query_row(
                "SELECT source FROM runs WHERE run_id = ?1",
                [run_id],
                |row| row.get::<_, Option<String>>(0),
            )
            .map_err(|e| {
                if e == rusqlite::Error::QueryReturnedNoRows {
                    OrchestratorError::NotFound(format!("run '{run_id}' not found"))
                } else {
                    OrchestratorError::Database(e)
                }
            })
    }

    /// Live grants (not revoked, not expired) the owner holds for this broker.
    pub fn live_grant_count(
        &self,
        owner: &str,
        broker_id: &str,
    ) -> Result<usize, OrchestratorError> {
        let holder = format!("broker:{broker_id}");
        let now = unix_now();
        Ok(self
            .list_keys_of_kind(owner, false, KeyKind::Grants)?
            .iter()
            .filter(|k| {
                k.holder() == holder
                    && k.revoked_at.is_none()
                    && k.expires_at.is_some_and(|at| at > now)
            })
            .count())
    }
}

/// Phase 1. `Ok(None)` means this call issues no grant (no permission, tool outside
/// `grant_tools`, or no `run` argument). `run_arg` is the `run` argument as JSON.
pub fn check_call(
    store: &SqliteWorkflowStore,
    manifest: &CapabilityManifest,
    facts: &CallFacts<'_>,
    run_arg: Option<serde_json::Value>,
) -> Result<Option<GrantPlan>, String> {
    let Some(permission) = manifest.run_request_permission.as_ref() else {
        return Ok(None);
    };
    // Loop guard (decision 6): the authoritative `runs.source`, never the evidence table.
    if permission
        .loop_guarded_tools
        .iter()
        .any(|t| t == facts.tool)
    {
        match store.run_source(facts.run_id) {
            Ok(Some(source)) if source.starts_with("broker:") => {
                return Err(LOOP_GUARD_MESSAGE.to_string())
            }
            Ok(_) => {}
            Err(_) => {
                return Err(refuse(
                    "loop_guard_unavailable",
                    "the run's origin could not be read, so this tool is refused",
                ))
            }
        }
    }
    if !permission.grant_tools.iter().any(|t| t == facts.tool) {
        return Ok(None);
    }
    let Some(run_arg) = run_arg.filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    if facts.owner.trim().is_empty() {
        return Err(refuse("run_invalid", "the call has no owner"));
    }
    let text = run_arg.to_string();
    let request: RunGrantRequest =
        vam_spec::encoding::json_to_proto("vam.v1.RunGrantRequest", &text)
            .map_err(|e| refuse("run_invalid", format!("the run request is not valid: {e}")))?;
    let owner = facts.owner;

    // Agent.
    let agent_urn = match request.agent_urn.trim() {
        "" => facts.calling_agent.trim().to_string(),
        other => other.to_string(),
    };
    if agent_urn.is_empty() {
        return Err(refuse("agent_not_found", "the calling agent is unknown"));
    }
    if agent_urn != facts.calling_agent.trim()
        && store.get_agent_visible(&agent_urn, owner, false).is_err()
    {
        return Err(refuse(
            "agent_not_found",
            "no agent with that urn that you may run",
        ));
    }

    // The calling run's place (for SAME).
    let (call_space, call_thread) = calling_place(store, facts);

    // Space.
    let workspace = request.workspace.clone().unwrap_or_default();
    let ws_mode = workspace.mode;
    let space = if ws_mode == PlaceMode::New as i32 {
        let fallback = format!("schedule-{}", &spaces::mint_id("")[..6]);
        let label = spaces::clean_label(&workspace.name, &fallback)
            .map_err(|e| refuse("run_invalid", e))?;
        SpacePlan::New { label }
    } else {
        let id = if ws_mode == PlaceMode::Id as i32 {
            let id = workspace.id.trim().to_string();
            if !spaces::is_space_id(&id) {
                return Err(refuse(
                    "space_not_found",
                    "workspace.id must be an sp_ id you hold",
                ));
            }
            id
        } else if ws_mode == PlaceMode::Unspecified as i32 || ws_mode == PlaceMode::Same as i32 {
            if call_space.is_empty() {
                return Err(refuse("space_not_found", "this run has no Space to reuse"));
            }
            call_space.clone()
        } else {
            return Err(refuse("run_invalid", "unknown workspace mode"));
        };
        let row = store
            .get_space(owner, &id)
            .map_err(|_| refuse("space_not_found", "no such Space among yours"))?;
        if row.agent_urn.as_deref() != Some(agent_urn.as_str()) {
            return Err(refuse(
                "space_agent_mismatch",
                "that Space belongs to another agent; use a NEW Space or one bound to this agent",
            ));
        }
        SpacePlan::Existing(row)
    };

    // Thread.
    let thread = request.thread.clone().unwrap_or_default();
    let th_mode = thread.mode;
    let thread_plan =
        if th_mode == PlaceMode::Unspecified as i32 || th_mode == PlaceMode::New as i32 {
            let label = spaces::clean_label(&thread.name, spaces::UNTITLED)
                .map_err(|e| refuse("run_invalid", e))?;
            ThreadPlan::New { label }
        } else if th_mode == PlaceMode::Same as i32 {
            let SpacePlan::Existing(row) = &space else {
                return Err(refuse(
                    "thread_needs_space",
                    "thread SAME needs workspace SAME",
                ));
            };
            if ws_mode != PlaceMode::Same as i32 && ws_mode != PlaceMode::Unspecified as i32 {
                return Err(refuse(
                    "thread_needs_space",
                    "thread SAME needs workspace SAME",
                ));
            }
            if call_thread.is_empty() {
                return Err(refuse(
                    "thread_not_found",
                    "this run has no thread to reuse",
                ));
            }
            ThreadPlan::Existing(
                store
                    .get_thread(owner, &row.space_id, &call_thread)
                    .map_err(|_| refuse("thread_not_found", "no such thread in that Space"))?,
            )
        } else if th_mode == PlaceMode::Id as i32 {
            let SpacePlan::Existing(row) = &space else {
                return Err(refuse(
                    "thread_needs_space",
                    "a thread id needs a workspace SAME or ID, not NEW",
                ));
            };
            let id = thread.id.trim();
            if !spaces::is_thread_id(id) {
                return Err(refuse(
                    "thread_not_found",
                    "thread.id must be a th_ id of that Space",
                ));
            }
            ThreadPlan::Existing(
                store
                    .get_thread(owner, &row.space_id, id)
                    .map_err(|_| refuse("thread_not_found", "no such thread in that Space"))?,
            )
        } else {
            return Err(refuse("run_invalid", "unknown thread mode"));
        };

    // Playbook, inputs and message.
    let inputs_json = run_arg
        .get("inputs")
        .filter(|v| !v.is_null())
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let playbook = match request.playbook_ref.as_ref() {
        Some(reference) => {
            let pinned = crate::catalog_resolve::resolve_ref_for_owner(
                store,
                crate::catalog_resolve::ArtifactKind::Playbook,
                reference,
                owner,
                crate::catalog_resolve::ResolveMode::Release,
            )
            .map_err(|e| refuse("playbook_invalid", e))?;
            let prepared = crate::run_context::prepare_playbook_for_ref(
                store,
                &pinned.reference,
                request.inputs.as_ref(),
                owner,
                false,
            )
            .map_err(|e| refuse("playbook_invalid", e))?;
            Some((
                PlaybookPin {
                    urn: prepared.pin.urn,
                    version: prepared.pin.version,
                    digest: prepared.pin.digest,
                },
                pinned.reference,
            ))
        }
        None => {
            if inputs_json.as_object().is_some_and(|m| !m.is_empty()) {
                return Err(refuse("run_invalid", "inputs need a playbookRef"));
            }
            None
        }
    };
    if playbook.is_none() && request.message.trim().is_empty() {
        return Err(refuse(
            "run_invalid",
            "message is required without a playbookRef",
        ));
    }
    if request.message.len() > MAX_GRANT_MESSAGE_BYTES {
        return Err(refuse(
            "message_too_large",
            format!("message is at most {MAX_GRANT_MESSAGE_BYTES} bytes"),
        ));
    }

    let on_busy = if request.on_busy == OnBusy::Skip as i32 {
        OnBusy::Skip
    } else if request.on_busy == OnBusy::Unspecified as i32
        || request.on_busy == OnBusy::Queue as i32
    {
        OnBusy::Queue
    } else {
        return Err(refuse("run_invalid", "unknown onBusy"));
    };

    // Expiry (decision 13).
    let now = unix_now();
    let expires_at = match request.expires_at.as_ref() {
        None => now + DEFAULT_GRANT_TTL_SECS,
        Some(ts) if ts.seconds == 0 && ts.nanos == 0 => now + DEFAULT_GRANT_TTL_SECS,
        Some(ts) if ts.seconds <= now => {
            return Err(refuse("expiry_in_past", "expiresAt must be in the future"))
        }
        Some(ts) => ts.seconds.min(now + MAX_GRANT_TTL_SECS),
    };

    // Live-grant cap (best effort under a race).
    let (_, _, cap) = vam_spec::validation::effective_run_request_limits(permission);
    let live = store
        .live_grant_count(owner, facts.broker_id)
        .map_err(|e| refuse("grant_unavailable", e))?;
    if live >= usize::try_from(cap).unwrap_or(0) {
        return Err(refuse(
            "grant_limit",
            format!("you already hold {live} live grants for this broker (limit {cap}); revoke one first"),
        ));
    }

    Ok(Some(GrantPlan {
        agent_urn,
        space,
        thread: thread_plan,
        message: request.message.clone(),
        playbook,
        inputs: inputs_json,
        on_busy,
        expires_at,
    }))
}

/// The calling run's Space and thread ids (empty when unknown).
fn calling_place(store: &SqliteWorkflowStore, facts: &CallFacts<'_>) -> (String, String) {
    let spec = store
        .get_run_spec_context(facts.run_id)
        .ok()
        .and_then(|(json, _, _)| json)
        .and_then(|json| {
            vam_spec::encoding::json_to_proto::<RunSpec>("vam.v1.RunSpec", &json).ok()
        });
    let space = spec
        .as_ref()
        .map(|s| s.space_id.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| facts.calling_space_fallback.trim().to_string());
    let thread = spec
        .map(|s| s.thread_id.trim().to_string())
        .unwrap_or_default();
    (space, thread)
}

/// Undoes an issuance unless disarmed: revokes the grant and deletes the Space and thread rows
/// this issuance created.
pub struct GrantGuard<'a> {
    store: &'a SqliteWorkflowStore,
    owner: String,
    grant_id: Option<String>,
    new_space: Option<String>,
    new_thread: Option<(String, String)>,
    armed: bool,
}

impl GrantGuard<'_> {
    /// The call succeeded: keep the grant and its rows.
    pub fn disarm(mut self) {
        self.armed = false;
    }

    fn undo(&mut self) {
        if let Some(id) = self.grant_id.take() {
            let _ = self.store.revoke_api_key_for_owner(&id, &self.owner);
        }
        let _ = self.store.delete_grant_created_rows(
            &self.owner,
            self.new_space.as_deref(),
            self.new_thread
                .as_ref()
                .map(|(s, t)| (s.as_str(), t.as_str())),
        );
        self.new_space = None;
        self.new_thread = None;
    }
}

impl Drop for GrantGuard<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.undo();
        }
    }
}

/// Phase 2: create the rows a `NEW` choice asks for, insert the grant and build the `RunGrant`.
/// On an error here the partial work is undone before returning.
pub fn mint<'a>(
    store: &'a SqliteWorkflowStore,
    plan: GrantPlan,
    facts: &CallFacts<'_>,
) -> Result<(RunGrant, GrantGuard<'a>), String> {
    let owner = facts.owner;
    let mut guard = GrantGuard {
        store,
        owner: owner.to_string(),
        grant_id: None,
        new_space: None,
        new_thread: None,
        armed: true,
    };
    let fail = |e: OrchestratorError| refuse("grant_unavailable", e);

    let space = match plan.space {
        SpacePlan::Existing(row) => row,
        SpacePlan::New { label } => {
            let row = store
                .create_space(owner, Some(&plan.agent_urn), &label, "grant", None)
                .map_err(fail)?;
            guard.new_space = Some(row.space_id.clone());
            row
        }
    };
    let thread = match plan.thread {
        ThreadPlan::Existing(row) => row,
        ThreadPlan::New { label } => {
            let row = store
                .create_thread(owner, &space.space_id, &label, "grant")
                .map_err(fail)?;
            guard.new_thread = Some((space.space_id.clone(), row.thread_id.clone()));
            row
        }
    };

    let constraints = GrantConstraints {
        agent_urn: plan.agent_urn.clone(),
        space_id: space.space_id.clone(),
        thread_id: thread.thread_id.clone(),
        message: plan.message.clone(),
        playbook_ref: plan.playbook.as_ref().map(|(pin, _)| pin.clone()),
        inputs: plan.inputs.clone(),
        on_busy: if plan.on_busy == OnBusy::Skip {
            "SKIP".to_string()
        } else {
            "QUEUE".to_string()
        },
    };
    let constraints_json =
        serde_json::to_string(&constraints).map_err(|e| refuse("grant_unavailable", e))?;
    let (record, token) = store
        .insert_grant_credential(&GrantInsert {
            name: format!("{} {}", facts.broker_id, facts.tool),
            owner_user_id: owner.to_string(),
            holder: format!("broker:{}", facts.broker_id),
            target_kind: "space".to_string(),
            target_id: space.space_id.clone(),
            constraints_json,
            issued_via: "invoke".to_string(),
            source_run_id: Some(facts.run_id.to_string()),
            tool_name: Some(facts.tool.to_string()),
            scopes: vec![GRANT_SCOPE.to_string()],
            expires_at: plan.expires_at,
        })
        .map_err(fail)?;
    guard.grant_id = Some(record.key_id.clone());
    store
        .set_grant_created_rows(
            owner,
            guard.new_space.as_deref(),
            guard
                .new_thread
                .as_ref()
                .map(|(s, t)| (s.as_str(), t.as_str())),
            &record.key_id,
        )
        .map_err(fail)?;

    let grant = RunGrant {
        token,
        grant_id: record.key_id,
        issued_at: Some(prost_types::Timestamp {
            seconds: record.created_at,
            nanos: 0,
        }),
        expires_at: Some(prost_types::Timestamp {
            seconds: plan.expires_at,
            nanos: 0,
        }),
        target: Some(GrantTarget {
            agent_urn: plan.agent_urn,
            space_id: space.space_id,
            space_name: space.label,
            thread_id: thread.thread_id,
            thread_name: thread.label,
            on_busy: plan.on_busy as i32,
            playbook_ref: plan.playbook.map(|(_, reference)| reference),
            request_digest: constraints.digest(),
        }),
    };
    Ok((grant, guard))
}

impl SqliteWorkflowStore {
    /// Stamps `created_by_grant_id` on the rows a grant's issuance created.
    pub fn set_grant_created_rows(
        &self,
        owner: &str,
        new_space: Option<&str>,
        new_thread: Option<(&str, &str)>,
        grant_id: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        if let Some(space_id) = new_space {
            connection.execute(
                "UPDATE spaces SET created_by_grant_id = ?3 WHERE space_id = ?1 \
                 AND owner_user_id = ?2 AND created_via = 'grant'",
                params![space_id, owner, grant_id],
            )?;
        }
        if let Some((space_id, thread_id)) = new_thread {
            connection.execute(
                "UPDATE threads SET created_by_grant_id = ?3 WHERE space_id = ?1 \
                 AND thread_id = ?2 AND created_via = 'grant' \
                 AND space_id IN (SELECT space_id FROM spaces WHERE owner_user_id = ?4)",
                params![space_id, thread_id, grant_id, owner],
            )?;
        }
        Ok(())
    }

    /// Deletes the rows a failed issuance created (only rows made `created_via 'grant'`).
    pub fn delete_grant_created_rows(
        &self,
        owner: &str,
        new_space: Option<&str>,
        new_thread: Option<(&str, &str)>,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        if let Some((space_id, thread_id)) = new_thread {
            connection.execute(
                "DELETE FROM threads WHERE space_id = ?1 AND thread_id = ?2 \
                 AND created_via = 'grant' \
                 AND space_id IN (SELECT space_id FROM spaces WHERE owner_user_id = ?3)",
                params![space_id, thread_id, owner],
            )?;
            connection.execute(
                "UPDATE spaces SET default_thread_id = NULL WHERE space_id = ?1 \
                 AND owner_user_id = ?2 AND default_thread_id = ?3",
                params![space_id, owner, thread_id],
            )?;
        }
        if let Some(space_id) = new_space {
            connection.execute(
                "DELETE FROM threads WHERE space_id = ?1 AND created_via = 'grant' \
                 AND space_id IN (SELECT space_id FROM spaces WHERE owner_user_id = ?2 \
                 AND created_via = 'grant')",
                params![space_id, owner],
            )?;
            connection.execute(
                "DELETE FROM spaces WHERE space_id = ?1 AND owner_user_id = ?2 \
                 AND created_via = 'grant'",
                params![space_id, owner],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn constraints() -> GrantConstraints {
        GrantConstraints {
            agent_urn: "urn:a".into(),
            space_id: "sp_x".into(),
            thread_id: "th_y".into(),
            message: "m".into(),
            playbook_ref: None,
            inputs: serde_json::json!({ "b": 1, "a": { "d": 2, "c": 3 } }),
            on_busy: "QUEUE".into(),
        }
    }

    #[test]
    fn the_digest_covers_the_whole_request_in_canonical_order() {
        let a = constraints();
        let mut b = constraints();
        b.inputs = serde_json::json!({ "a": { "c": 3, "d": 2 }, "b": 1 });
        assert_eq!(a.digest(), b.digest());
        assert!(a
            .canonical_json()
            .contains(r#""inputs":{"a":{"c":3,"d":2},"b":1}"#));
        for change in [
            |c: &mut GrantConstraints| c.message = "other".into(),
            |c: &mut GrantConstraints| c.thread_id = "th_z".into(),
            |c: &mut GrantConstraints| c.on_busy = "SKIP".into(),
            |c: &mut GrantConstraints| c.agent_urn = "urn:b".into(),
        ] {
            let mut c = constraints();
            change(&mut c);
            assert_ne!(c.digest(), a.digest());
        }
    }

    #[test]
    fn run_source_reads_the_runs_table_and_a_missing_run_is_an_error() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let id = vam_spec::v1::ResourceId {
            urn: "urn:vam:run:local.vam.dev/acme/r1".into(),
        };
        store.create_run(&id).unwrap();
        assert_eq!(store.run_source(&id.urn).unwrap(), None);
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE runs SET source = 'broker:b' WHERE run_id = ?1",
                [&id.urn],
            )
            .unwrap();
        assert_eq!(
            store.run_source(&id.urn).unwrap().as_deref(),
            Some("broker:b")
        );
        assert!(store.run_source("urn:vam:run:none").is_err());
    }

    #[test]
    fn dropping_an_armed_guard_undoes_the_issuance_and_disarm_keeps_it() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let user = store.create_user("alice", "alice").unwrap().user_id;
        for keep in [false, true] {
            let space = store
                .create_space(&user, Some("urn:a"), "s", "grant", None)
                .unwrap();
            let thread = store
                .create_thread(&user, &space.space_id, "t", "grant")
                .unwrap();
            let (rec, _) = store
                .insert_grant_credential(&GrantInsert {
                    name: "g".into(),
                    owner_user_id: user.clone(),
                    holder: "broker:b".into(),
                    target_kind: "space".into(),
                    target_id: space.space_id.clone(),
                    constraints_json: "{}".into(),
                    issued_via: "invoke".into(),
                    source_run_id: None,
                    tool_name: None,
                    scopes: vec![GRANT_SCOPE.into()],
                    expires_at: unix_now() + 3600,
                })
                .unwrap();
            let guard = GrantGuard {
                store: &store,
                owner: user.clone(),
                grant_id: Some(rec.key_id.clone()),
                new_space: Some(space.space_id.clone()),
                new_thread: Some((space.space_id.clone(), thread.thread_id.clone())),
                armed: true,
            };
            if keep {
                guard.disarm();
            } else {
                drop(guard);
            }
            let revoked = store
                .get_api_key_for_owner(&rec.key_id, &user)
                .unwrap()
                .revoked_at
                .is_some();
            assert_eq!(revoked, !keep);
            assert_eq!(store.get_space(&user, &space.space_id).is_ok(), keep);
        }
    }
}
