//! Idle-session TTL (IT-2.6).
//!
//! A session that holds a VM (or a process guest) and a workspace lock but does nothing is ended
//! after `VAMD_SESSION_IDLE_TTL_SECS` (default 24 h) through the same terminate path the other
//! sweepers use.
//!
//! **Idle** means all of:
//! - the session is not terminal, terminating, or marked for termination;
//! - it has no run that is not terminal (a running run, and a `PAUSED` run parked on an approval
//!   or gate, both count as live; the keep-warm sweeper owns parked runs);
//! - it has no undecided approval or gate (those keep their own expiry: 1 h to `EXPIRED`);
//! - no terminal run of it still waits for its receipt (its finalizer is not done);
//! - its last activity is at least the TTL old. Last activity is the newest of the session's
//!   `updated_at`, the creation time of any of its runs, and the time of any receipt for them.
//!
//! The query reads only durable rows, so a restart changes nothing: the clock keeps running
//! from the stored times, and a second pass finds the sessions already ended and returns 0.

use std::future::Future;

use rusqlite::params;

use crate::error::OrchestratorError;
use crate::manager::SessionManager;
use crate::receipt::TERMINAL_STATES;
use crate::workflow::SqliteWorkflowStore;

/// An idle session the sweep will end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleSession {
    pub session_id: String,
    pub workspace_id: String,
}

impl SqliteWorkflowStore {
    /// Sessions idle for at least `ttl_secs` at `now` (unix seconds). See the module docs.
    pub fn idle_sessions(
        &self,
        now: i64,
        ttl_secs: u64,
    ) -> Result<Vec<IdleSession>, OrchestratorError> {
        let cutoff = now.saturating_sub(i64::try_from(ttl_secs).unwrap_or(i64::MAX));
        let terminal = TERMINAL_STATES
            .iter()
            .map(|s| format!("'{s}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT s.session_id, s.workspace_id
             FROM sessions s
             WHERE s.observed_state NOT IN ('TERMINATING', 'TERMINATED', 'FAILED')
               AND s.desired_state != 'TERMINATED'
               AND MAX(
                     s.updated_at,
                     COALESCE((SELECT MAX(r.created_at) FROM runs r
                               WHERE r.session_id = s.session_id), 0),
                     COALESCE((SELECT MAX(rr.created_at) FROM run_receipts rr
                               JOIN runs r ON r.run_id = rr.run_id
                               WHERE r.session_id = s.session_id), 0)
                   ) <= ?1
               AND NOT EXISTS (SELECT 1 FROM runs r
                               WHERE r.session_id = s.session_id
                                 AND r.state NOT IN ({terminal}))
               AND NOT EXISTS (SELECT 1 FROM runs r
                               WHERE r.session_id = s.session_id
                                 AND NOT EXISTS (SELECT 1 FROM run_receipts rr
                                                 WHERE rr.run_id = r.run_id))
               AND NOT EXISTS (SELECT 1 FROM approvals a
                               WHERE a.session_id = s.session_id AND a.decision IS NULL)
             ORDER BY s.updated_at"
        );
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(&sql)?;
        let rows = stmt.query_map(params![cutoff], |row| {
            Ok(IdleSession {
                session_id: row.get(0)?,
                workspace_id: row.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// End every idle session through `end` and return how many it ended. A session whose run is
/// being finalized is skipped; an `end` that fails is logged and retried on the next pass. The
/// clock is `now`, so tests drive it.
pub async fn sweep_idle_sessions_with<F, Fut>(
    store: &SqliteWorkflowStore,
    now: i64,
    ttl_secs: u64,
    mut end: F,
) -> Result<usize, OrchestratorError>
where
    F: FnMut(IdleSession) -> Fut,
    Fut: Future<Output = Result<(), OrchestratorError>>,
{
    let mut ended = 0usize;
    for idle in store.idle_sessions(now, ttl_secs)? {
        if store.session_ends.is_finalizing(&idle.session_id) {
            continue;
        }
        let session_id = idle.session_id.clone();
        match end(idle).await {
            Ok(()) => ended += 1,
            Err(e) => tracing::error!(
                "[orchestrator] idle TTL: failed to end session '{session_id}', will retry: {e}"
            ),
        }
    }
    Ok(ended)
}

/// The daemon's sweep: terminate idle sessions and release their workspace locks (a failed
/// terminate keeps the lock, as the other sweepers do).
pub async fn sweep_idle_sessions(
    store: &SqliteWorkflowStore,
    manager: &SessionManager,
    layout: &vam_storage::Drive9Layout,
    ttl_secs: u64,
) -> Result<usize, OrchestratorError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    sweep_idle_sessions_with(store, now, ttl_secs, |idle| async move {
        manager.terminate_session(idle.session_id.clone()).await?;
        if !idle.workspace_id.is_empty() {
            if let Err(e) = vam_storage::release_workspace_lock(
                layout,
                &idle.workspace_id,
                Some(&idle.session_id),
            ) {
                tracing::error!(
                    "[orchestrator] idle TTL: failed to release workspace '{}' lock held by session '{}': {}",
                    idle.workspace_id,
                    idle.session_id,
                    e
                );
            }
        }
        Ok(())
    })
    .await
}

/// Seconds ahead of a grant's expiry that `grant.expiring` is sent (7 days).
pub const GRANT_EXPIRING_LEAD_SECS: i64 = 7 * 24 * 3600;
/// A test node (`VAMD_E2E_MOCK_SCRIPTS` set) may change the lead with this variable, so a
/// scenario can see a grant "about to expire" without waiting days.
pub const GRANT_EXPIRING_LEAD_ENV: &str = "VAMD_E2E_GRANT_EXPIRING_LEAD_SECS";

/// The lead in force: [`GRANT_EXPIRING_LEAD_SECS`], or the override on a test node.
pub fn grant_expiring_lead_secs() -> i64 {
    lead_for(
        std::env::var_os("VAMD_E2E_MOCK_SCRIPTS").is_some(),
        std::env::var(GRANT_EXPIRING_LEAD_ENV).ok().as_deref(),
    )
}

/// The override counts only on a test node (the existing `VAMD_E2E_MOCK_SCRIPTS` gate).
fn lead_for(test_node: bool, override_secs: Option<&str>) -> i64 {
    if test_node {
        if let Some(v) = override_secs
            .and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
        {
            return v;
        }
    }
    GRANT_EXPIRING_LEAD_SECS
}

impl SqliteWorkflowStore {
    /// Sends `grant.expiring` for every live broker grant that expires within the lead
    /// ([`grant_expiring_lead_secs`]), once per grant, to the outbox of the run that created it
    /// and to the grant owner's notifications. A grant whose lifetime is shorter than the lead
    /// is never told (open question 12: it is always inside the lead). The claim, the outbox
    /// append and the record are one transaction. Returns how many were sent.
    pub fn sweep_expiring_grants(&self, now: i64) -> Result<usize, OrchestratorError> {
        let lead = grant_expiring_lead_secs();
        let bus = self.event_bus();
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS grant_expiry_notices (
                grant_id TEXT PRIMARY KEY NOT NULL,
                sent_at INTEGER NOT NULL
             );",
        )?;
        // Records of grants that reached their expiry close as `expired`.
        crate::notifications::resolve_expired_grants(&connection, now)?;
        type Due = (String, String, String, i64, String, String);
        let due: Vec<Due> = {
            let mut stmt = connection.prepare(
                "SELECT k.key_id, k.holder, COALESCE(k.tool_name, ''), k.expires_at,
                        k.source_run_id, COALESCE(k.owner_user_id, '')
                 FROM api_keys k
                 WHERE k.holder LIKE 'broker:%'
                   AND k.revoked_at IS NULL
                   AND k.source_run_id IS NOT NULL
                   AND k.expires_at IS NOT NULL
                   AND k.expires_at > ?1 AND k.expires_at <= ?2
                   AND k.expires_at - k.created_at > ?3
                   AND NOT EXISTS (SELECT 1 FROM grant_expiry_notices n WHERE n.grant_id = k.key_id)
                 ORDER BY k.expires_at, k.key_id",
            )?;
            let rows = stmt
                .query_map(params![now, now.saturating_add(lead), lead], |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows
        };
        let mut sent = 0usize;
        for (grant_id, holder, tool, expires_at, run_id, owner) in due {
            let payload = serde_json::json!({
                "grantId": grant_id,
                "brokerId": holder.strip_prefix("broker:").unwrap_or(&holder),
                "tool": tool,
                "expiresAt": expires_at,
            })
            .to_string();
            let result = (|| -> Result<Option<i64>, OrchestratorError> {
                let tx = connection.unchecked_transaction()?;
                let claimed = tx.execute(
                    "INSERT OR IGNORE INTO grant_expiry_notices (grant_id, sent_at) VALUES (?1, ?2)",
                    params![grant_id, now],
                )? == 1;
                if !claimed {
                    return Ok(None);
                }
                tx.execute(
                    "INSERT INTO workflow_outbox (run_id, event_type, payload) VALUES (?1, ?2, ?3)",
                    params![run_id, crate::events::TYPE_GRANT_EXPIRING, payload],
                )?;
                let seq = tx.last_insert_rowid();
                crate::notifications::record_grant_expiring(
                    &tx, &grant_id, &owner, &tool, &run_id, expires_at,
                )?;
                tx.commit()?;
                Ok(Some(seq))
            })();
            match result {
                Ok(Some(seq)) => {
                    sent += 1;
                    if let Some(bus) = bus.as_ref() {
                        bus.publish(crate::events::cloud_event_from_raw(
                            &run_id,
                            seq,
                            crate::events::TYPE_GRANT_EXPIRING,
                            &payload,
                        ));
                    }
                }
                Ok(None) => {}
                // The transaction rolled back, claim included: the next pass retries.
                Err(e) => tracing::warn!("grant.expiring write failed for {grant_id}: {e}"),
            }
        }
        Ok(sent)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_lead_override_is_ignored_off_a_test_node() {
        assert_eq!(lead_for(false, Some("60")), GRANT_EXPIRING_LEAD_SECS);
        assert_eq!(lead_for(true, Some("60")), 60);
        assert_eq!(lead_for(true, Some("junk")), GRANT_EXPIRING_LEAD_SECS);
        assert_eq!(lead_for(true, None), GRANT_EXPIRING_LEAD_SECS);
    }
    use crate::session::DesiredState;

    const TTL: u64 = 86_400;
    const T0: i64 = 1_000_000;

    fn store_with_session(id: &str) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        add_session(&store, id);
        store
    }

    fn add_session(store: &SqliteWorkflowStore, id: &str) {
        store
            .create_session(id, None, &format!("vm-{id}"), &format!("ws-{id}"), "prof")
            .expect("create session");
        set(
            store,
            "UPDATE sessions SET updated_at = ?1 WHERE session_id = ?2",
            T0,
            id,
        );
    }

    fn set(store: &SqliteWorkflowStore, sql: &str, n: i64, id: &str) {
        store
            .connection
            .lock()
            .expect("lock")
            .execute(sql, params![n, id])
            .expect("update");
    }

    fn add_run(store: &SqliteWorkflowStore, session: &str, run: &str, state: &str, at: i64) {
        let c = store.connection.lock().expect("lock");
        c.execute(
            "INSERT INTO runs (run_id, state, revision, session_id, created_at)
             VALUES (?1, ?2, 0, ?3, ?4)",
            params![run, state, session, at],
        )
        .expect("run");
    }

    fn add_receipt(store: &SqliteWorkflowStore, run: &str, at: i64) {
        store
            .connection
            .lock()
            .expect("lock")
            .execute(
                "INSERT INTO run_receipts (run_id, generation, document, digest, key_id, created_at)
                 VALUES (?1, 1, '{}', 'd', 'k', ?2)",
                params![run, at],
            )
            .expect("receipt");
    }

    fn add_approval(store: &SqliteWorkflowStore, session: &str, run: &str, decision: Option<&str>) {
        store
            .connection
            .lock()
            .expect("lock")
            .execute(
                "INSERT INTO approvals (request_id, run_id, step_id, attempt_id, checkpoint_digest,
                                        continuation_generation, decision, session_id)
                 VALUES (?1, ?2, 's', 'a', 'd', 0, ?3, ?4)",
                params![format!("req-{run}"), run, decision, session],
            )
            .expect("approval");
    }

    fn ids(store: &SqliteWorkflowStore, now: i64) -> Vec<String> {
        store
            .idle_sessions(now, TTL)
            .expect("query")
            .into_iter()
            .map(|s| s.session_id)
            .collect()
    }

    #[test]
    fn bare_session_is_idle_only_after_the_ttl() {
        let store = store_with_session("a");
        assert!(ids(&store, T0 + TTL as i64 - 1).is_empty());
        assert_eq!(ids(&store, T0 + TTL as i64), vec!["a"]);
    }

    #[test]
    fn live_parked_and_waiting_sessions_are_never_idle() {
        let far = T0 + 10 * TTL as i64;
        for (case, state, approval) in [
            ("running", "RUNNING", None),
            ("parked", "PAUSED", None),
            ("waiting", "SUCCEEDED", Some(None)),
        ] {
            let store = store_with_session("a");
            add_run(&store, "a", "r1", state, T0);
            if state == "SUCCEEDED" {
                add_receipt(&store, "r1", T0);
            }
            if let Some(decision) = approval {
                add_approval(&store, "a", "r1", decision);
            }
            assert!(ids(&store, far).is_empty(), "{case}");
        }
    }

    #[test]
    fn a_finished_run_resets_the_clock_and_needs_its_receipt() {
        let store = store_with_session("a");
        add_run(&store, "a", "r1", "SUCCEEDED", T0 + 5 * TTL as i64);
        let now = T0 + 5 * TTL as i64 + 10;
        // The run ended but its receipt is not issued yet: not idle.
        assert!(ids(&store, now + 10 * TTL as i64).is_empty());
        add_receipt(&store, "r1", T0 + 5 * TTL as i64 + 5);
        // Receipt issued: idle once the TTL has passed since the last activity.
        assert!(ids(&store, now).is_empty());
        assert_eq!(ids(&store, T0 + 6 * TTL as i64 + 5), vec!["a"]);
    }

    #[test]
    fn a_decided_approval_does_not_hold_a_session() {
        let store = store_with_session("a");
        add_run(&store, "a", "r1", "EXPIRED", T0);
        add_receipt(&store, "r1", T0);
        add_approval(&store, "a", "r1", Some("EXPIRED"));
        assert_eq!(ids(&store, T0 + TTL as i64), vec!["a"]);
    }

    #[test]
    fn terminal_or_terminating_sessions_are_left_alone() {
        let store = store_with_session("a");
        store
            .update_desired_state("a", DesiredState::Terminated)
            .expect("desired");
        set(
            &store,
            "UPDATE sessions SET updated_at = ?1 WHERE session_id = ?2",
            T0,
            "a",
        );
        assert!(ids(&store, T0 + 2 * TTL as i64).is_empty());
    }

    #[tokio::test]
    async fn sweep_ends_idle_sessions_once_and_survives_a_restart() {
        let store = store_with_session("a");
        add_session(&store, "b");
        // b is live.
        add_run(&store, "b", "r1", "RUNNING", T0);
        let now = T0 + TTL as i64;
        let ended = std::sync::Mutex::new(Vec::new());
        let end = |idle: IdleSession| {
            let store = &store;
            let ended = &ended;
            async move {
                store
                    .update_desired_state(&idle.session_id, DesiredState::Terminated)
                    .expect("terminate");
                ended.lock().expect("lock").push(idle.session_id);
                Ok(())
            }
        };
        assert_eq!(
            sweep_idle_sessions_with(&store, now, TTL, end)
                .await
                .unwrap(),
            1
        );
        assert_eq!(*ended.lock().unwrap(), vec!["a".to_string()]);
        // A second pass, and a pass by a restarted daemon (same rows, new process), end nothing.
        assert_eq!(
            sweep_idle_sessions_with(&store, now, TTL, end)
                .await
                .unwrap(),
            0
        );
        assert_eq!(ended.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_failed_end_is_retried_and_not_counted() {
        let store = store_with_session("a");
        let now = T0 + TTL as i64;
        let fail = |_: IdleSession| async { Err(OrchestratorError::Core("boom".to_string())) };
        assert_eq!(
            sweep_idle_sessions_with(&store, now, TTL, fail)
                .await
                .unwrap(),
            0
        );
        assert_eq!(ids(&store, now), vec!["a"]);
    }

    #[test]
    fn grant_expiring_is_sent_once_inside_the_window_only() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let now = 10_000_000;
        let owner = store.create_user("o", "o").expect("user").user_id;
        let mk = |expires_at: i64, run: &str| {
            let (rec, _) = store
                .insert_grant_credential(&crate::credentials::GrantInsert {
                    name: "g".into(),
                    owner_user_id: owner.clone(),
                    holder: "broker:b1".into(),
                    target_kind: "agent".into(),
                    target_id: "urn:a".into(),
                    constraints_json: "{}".into(),
                    issued_via: "approval".into(),
                    source_run_id: Some(run.into()),
                    tool_name: Some("t".into()),
                    scopes: vec![],
                    expires_at,
                })
                .expect("grant");
            rec.key_id
        };
        let near = mk(now + 3600, "run-near");
        let _far = mk(now + 30 * 86_400, "run-far");
        let revoked = mk(now + 3600, "run-revoked");
        let short = mk(now + 3600, "run-short");
        {
            let c = store.connection.lock().expect("lock");
            c.execute(
                "UPDATE api_keys SET revoked_at = ?1 WHERE key_id = ?2",
                params![now, revoked],
            )
            .expect("revoke");
            // Issued 30 days ago, so its lifetime is longer than the lead; `short` was issued
            // an hour ago, so its whole life is inside the lead (open question 12).
            c.execute(
                "UPDATE api_keys SET created_at = ?1 WHERE key_id != ?2",
                params![now - 30 * 86_400, short],
            )
            .expect("age");
            c.execute(
                "UPDATE api_keys SET created_at = ?1 WHERE key_id = ?2",
                params![now - 3600, short],
            )
            .expect("age short");
        }
        assert_eq!(store.sweep_expiring_grants(now).unwrap(), 1);
        assert_eq!(store.sweep_expiring_grants(now + 60).unwrap(), 0);
        // One record for the owner, with the renew action, and none for the short grant.
        let c = store.connection.lock().expect("lock");
        let rows: Vec<(String, String, String)> = c
            .prepare("SELECT grant_id, severity, action FROM notifications WHERE kind = 'grant.expiring'")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, near);
        assert_eq!(rows[0].1, "warn");
        assert!(rows[0].2.contains("\"renew\""), "{}", rows[0].2);
        drop(c);
        let events = store.read_outbox_for_run("run-near", 0).expect("outbox");
        let hit: Vec<_> = events
            .iter()
            .filter(|e| format!("{e:?}").contains("grant.expiring"))
            .collect();
        assert_eq!(hit.len(), 1);
        assert!(format!("{:?}", hit[0]).contains(&near));
        assert!(store
            .read_outbox_for_run("run-far", 0)
            .expect("outbox")
            .is_empty());
    }
}
