//! Facts a run receipt prints, recorded as they happen: who asked and from
//! where, the isolation each session ran under, the models the broker served,
//! and every tool decision with the rule that made it.
//!
//! Everything here is evidence derived from decisions made elsewhere. Writing
//! a row never changes a decision and a failed write never fails a tool call
//! (see [`SqliteWorkflowStore::record_run_decision`]).

use rusqlite::{params, Connection, OptionalExtension};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// Surface values a client may declare. Anything else is `api`.
pub const SURFACE_API: &str = "api";
pub const SURFACE_CLI: &str = "cli";
pub const SURFACE_APP: &str = "app";
/// Decisions the host took itself (a timeout or expiry).
pub const SURFACE_SYSTEM: &str = "system";
/// Header a client uses to declare where a request came from.
pub const SURFACE_HEADER: &str = "x-vam-surface";

/// The surface a client declared. It is the client's word, not a proof; the
/// identity that decided is the authenticated one.
pub fn surface_from_header(value: Option<&str>) -> &'static str {
    match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("cli") => SURFACE_CLI,
        Some("app") => SURFACE_APP,
        _ => SURFACE_API,
    }
}

/// What a tool decision was made by.
pub mod decided_by {
    pub const CEDAR_RULE: &str = "cedar_rule";
    pub const PLAYBOOK_GUARD: &str = "playbook_guard";
    pub const PATH_LIMB: &str = "path_limb";
    pub const MISSING_PERMISSIONS_ROW: &str = "missing_permissions_row";
    pub const MISSING_CATALOG_ENTRY: &str = "missing_catalog_entry";
    pub const GATE: &str = "gate";
    pub const SCRIPT_DIGEST_MISMATCH: &str = "script_digest_mismatch";
    pub const HOST_ERROR: &str = "host_error";
}

/// One tool decision, as the PEP made it. No arguments, paths or previews.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewRunDecision {
    pub session_id: String,
    pub tool_name: String,
    pub target: String,
    pub side_effect: i64,
    /// `allowed`, `asked` or `denied`.
    pub disposition: &'static str,
    pub decided_by: &'static str,
    pub deciding_scope: String,
    pub deciding_rule: String,
    pub args_digest: String,
    pub approval_request_id: Option<String>,
    /// The call passed because an earlier ask was approved for these arguments.
    pub via_grant: bool,
    /// The playbook guard condition this call passed (`args_within <arg> in
    /// <ref>` or `allowed_when <token>`). Names a step, never a value.
    pub constraint_rule: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDecision {
    pub seq: i64,
    pub session_id: String,
    pub decided_at: i64,
    pub tool_name: String,
    pub target: String,
    pub side_effect: i64,
    pub disposition: String,
    pub decided_by: String,
    pub deciding_scope: String,
    pub deciding_rule: String,
    pub args_digest: String,
    pub approval_request_id: Option<String>,
    pub via_grant: bool,
    pub constraint_rule: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunIsolationFact {
    pub session_id: String,
    pub guest_mode: String,
    pub vmm: String,
    pub jailer: bool,
    pub api_tls: bool,
    pub summary: String,
    pub kernel_sha256: Option<String>,
    pub rootfs_sha256: Option<String>,
    pub lifecycle_generation: i64,
    pub recorded_at: i64,
    /// The API rate-limit setting the node ran under when the run started; `None` for a run
    /// recorded before the node reported it (read as "unknown", never as on).
    pub rate_limits: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunModelFact {
    pub model: String,
    pub provider: String,
    pub endpoint_host: String,
    pub calls: i64,
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub first_at: i64,
    pub last_at: i64,
}

/// Who asked for the run, when it started and ended, and what was recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunFacts {
    pub created_at: Option<i64>,
    pub ended_at: Option<i64>,
    pub requested_by_user_id: Option<String>,
    /// Client-declared (`cli`, `app`), else `api`.
    pub surface: Option<String>,
    /// The trigger that started the run; `None` for a person.
    pub trigger: Option<TriggerFact>,
    pub decision_write_failures: i64,
    pub isolation: Vec<RunIsolationFact>,
    pub models: Vec<RunModelFact>,
}

/// The broker trigger behind a run (evidence only; the loop guard reads `runs.source`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggerFact {
    pub broker_id: String,
    pub grant_id: String,
    pub trigger_ref: String,
    /// Unix seconds.
    pub fire_at: i64,
    pub fire_kind: String,
}

/// Who decided an approval or gate, from where, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApprovalProvenance {
    pub request_id: String,
    pub step_id: String,
    pub kind: String,
    pub decision: Option<String>,
    pub decided_at: Option<i64>,
    /// Client-declared; `system` for a timeout or expiry.
    pub decided_surface: Option<String>,
    /// Authenticated user who decided.
    pub decided_by_user_id: Option<String>,
    pub reviewer_principal: Option<String>,
    /// The response the gate accepted (validated against its schema), if any.
    pub response_json: Option<String>,
    /// What was asked: the approval's requested arguments (a question's `{jig, data}`).
    pub requested_args_json: Option<String>,
}

/// Adds `name` to `table` unless it is there. Errors surface.
fn add_column_if_missing(
    connection: &Connection,
    table: &str,
    name: &str,
    definition: &str,
) -> Result<(), OrchestratorError> {
    let present: bool = connection.query_row(
        &format!("SELECT COUNT(*) FROM pragma_table_info('{table}') WHERE name = ?1"),
        [name],
        |row| Ok(row.get::<_, i64>(0)? > 0),
    )?;
    if !present {
        connection.execute_batch(&format!(
            "ALTER TABLE {table} ADD COLUMN {name} {definition};"
        ))?;
    }
    Ok(())
}

/// Creates the tables, columns and trigger. Safe to run on every start.
pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    add_column_if_missing(connection, "runs", "created_at", "INTEGER")?;
    add_column_if_missing(connection, "runs", "ended_at", "INTEGER")?;
    add_column_if_missing(connection, "approvals", "decided_at", "INTEGER")?;
    add_column_if_missing(connection, "approvals", "decided_surface", "TEXT")?;
    add_column_if_missing(connection, "approvals", "decided_by_user_id", "TEXT")?;
    connection.execute_batch(
        "
        -- ended_at is when the terminal state is published; the first write wins.
        CREATE TRIGGER IF NOT EXISTS runs_ended_at
        AFTER UPDATE OF state ON runs
        WHEN NEW.state IN ('SUCCEEDED','FAILED','CANCELLED','TIMED_OUT','EXPIRED','REJECTED')
             AND NEW.ended_at IS NULL
        BEGIN
            UPDATE runs SET ended_at = CAST(strftime('%s','now') AS INTEGER)
            WHERE run_id = NEW.run_id;
        END;

        CREATE TABLE IF NOT EXISTS run_requests (
            run_id TEXT PRIMARY KEY NOT NULL,
            requested_by_user_id TEXT NOT NULL,
            surface TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            decision_write_failures INTEGER NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS run_isolation (
            run_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            guest_mode TEXT NOT NULL,
            vmm TEXT NOT NULL,
            jailer INTEGER NOT NULL,
            api_tls INTEGER NOT NULL,
            summary TEXT NOT NULL,
            kernel_sha256 TEXT,
            rootfs_sha256 TEXT,
            lifecycle_generation INTEGER NOT NULL,
            recorded_at INTEGER NOT NULL,
            PRIMARY KEY (run_id, session_id)
        );

        CREATE TABLE IF NOT EXISTS run_models (
            run_id TEXT NOT NULL,
            model TEXT NOT NULL,
            provider TEXT NOT NULL,
            endpoint_host TEXT NOT NULL,
            calls INTEGER NOT NULL,
            prompt_tokens INTEGER NOT NULL,
            completion_tokens INTEGER NOT NULL,
            first_at INTEGER NOT NULL,
            last_at INTEGER NOT NULL,
            PRIMARY KEY (run_id, model, provider, endpoint_host)
        );

        CREATE TABLE IF NOT EXISTS run_decisions (
            run_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            session_id TEXT NOT NULL,
            decided_at INTEGER NOT NULL,
            tool_name TEXT NOT NULL,
            target TEXT NOT NULL,
            side_effect INTEGER NOT NULL,
            disposition TEXT NOT NULL,
            decided_by TEXT NOT NULL,
            deciding_scope TEXT NOT NULL,
            deciding_rule TEXT NOT NULL,
            args_digest TEXT NOT NULL,
            approval_request_id TEXT,
            via_grant INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (run_id, seq)
        );
        ",
    )?;
    add_column_if_missing(connection, "run_decisions", "constraint_rule", "TEXT")?;
    for (name, ty) in [
        ("broker_id", "TEXT"),
        ("grant_id", "TEXT"),
        ("trigger_ref", "TEXT"),
        ("fire_at", "INTEGER"),
        ("fire_kind", "TEXT"),
    ] {
        add_column_if_missing(connection, "run_requests", name, ty)?;
    }
    // IT-2.4: the rate-limit setting a run started under. NULL on rows written before it.
    add_column_if_missing(connection, "run_isolation", "rate_limits", "TEXT")?;
    Ok(())
}

/// The host part of an endpoint URL; never a path, query or credentials.
pub fn endpoint_host(endpoint: &str) -> String {
    url::Url::parse(endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

impl SqliteWorkflowStore {
    /// Records who asked for a run and from which surface. The first write wins.
    /// [`Self::record_run_request`] for a caller: who asked is the caller's owner, from where
    /// is its source's surface. The whole caller is in scope so the source can be recorded
    /// here without new plumbing.
    pub fn record_run_request_for(
        &self,
        run_id: &str,
        caller: &crate::api::run_start::RunCaller,
    ) -> Result<(), OrchestratorError> {
        self.record_run_request(run_id, &caller.owner_user_id, caller.source.surface())?;
        if let crate::api::run_start::RunSource::Trigger {
            broker_id,
            grant_id,
            trigger_ref,
            fire_at,
            fire_kind,
            ..
        } = &caller.source
        {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            connection.execute(
                "UPDATE run_requests SET broker_id = ?2, grant_id = ?3, trigger_ref = ?4,
                        fire_at = ?5, fire_kind = ?6
                 WHERE run_id = ?1 AND broker_id IS NULL",
                params![run_id, broker_id, grant_id, trigger_ref, fire_at, fire_kind],
            )?;
        }
        Ok(())
    }

    pub fn record_run_request(
        &self,
        run_id: &str,
        requested_by_user_id: &str,
        surface: &str,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT OR IGNORE INTO run_requests (run_id, requested_by_user_id, surface, created_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                run_id,
                requested_by_user_id,
                surface,
                crate::catalog::unix_timestamp()
            ],
        )?;
        Ok(())
    }

    /// Records the isolation a run's session started under. One row per session.
    #[allow(clippy::too_many_arguments)]
    pub fn record_run_isolation(
        &self,
        run_id: &str,
        session_id: &str,
        isolation: &vam_spec::v1::NodeIsolation,
        guest_mode: &str,
        images: Option<(String, String)>,
        lifecycle_generation: i64,
    ) -> Result<(), OrchestratorError> {
        let (kernel, rootfs) = match images {
            Some((k, r)) => (Some(k), Some(r)),
            None => (None, None),
        };
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT OR IGNORE INTO run_isolation
               (run_id, session_id, guest_mode, vmm, jailer, api_tls, summary,
                kernel_sha256, rootfs_sha256, lifecycle_generation, recorded_at, rate_limits)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
            params![
                run_id,
                session_id,
                guest_mode,
                isolation.vmm,
                isolation.jailer,
                isolation.api_tls,
                isolation.summary,
                kernel,
                rootfs,
                lifecycle_generation,
                crate::catalog::unix_timestamp(),
                (!isolation.rate_limits.is_empty()).then_some(isolation.rate_limits.as_str())
            ],
        )?;
        Ok(())
    }

    /// Counts one served inference call against the model that served it.
    pub fn record_model_call(
        &self,
        run_id: &str,
        model: &str,
        provider: &str,
        endpoint_host: &str,
        prompt_tokens: i64,
        completion_tokens: i64,
    ) -> Result<(), OrchestratorError> {
        let now = crate::catalog::unix_timestamp();
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO run_models
               (run_id, model, provider, endpoint_host, calls, prompt_tokens,
                completion_tokens, first_at, last_at)
             VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, ?7)
             ON CONFLICT (run_id, model, provider, endpoint_host) DO UPDATE SET
               calls = calls + 1,
               prompt_tokens = prompt_tokens + excluded.prompt_tokens,
               completion_tokens = completion_tokens + excluded.completion_tokens,
               last_at = excluded.last_at",
            params![
                run_id,
                model,
                provider,
                endpoint_host,
                prompt_tokens.max(0),
                completion_tokens.max(0),
                now
            ],
        )?;
        Ok(())
    }

    /// Stores one tool decision for the run.
    pub fn insert_run_decision(
        &self,
        run_id: &str,
        decision: &NewRunDecision,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO run_decisions
               (run_id, seq, session_id, decided_at, tool_name, target, side_effect,
                disposition, decided_by, deciding_scope, deciding_rule, args_digest,
                approval_request_id, via_grant, constraint_rule)
             VALUES (?1,
                     (SELECT IFNULL(MAX(seq), 0) + 1 FROM run_decisions WHERE run_id = ?1),
                     ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                run_id,
                decision.session_id,
                crate::catalog::unix_timestamp(),
                decision.tool_name,
                decision.target,
                decision.side_effect,
                decision.disposition,
                decision.decided_by,
                decision.deciding_scope,
                decision.deciding_rule,
                decision.args_digest,
                decision.approval_request_id,
                decision.via_grant,
                decision.constraint_rule,
            ],
        )?;
        Ok(())
    }

    /// Records a PEP decision for the run. This is evidence only: it is called
    /// after the decision is final, returns nothing, and never lets a failure
    /// reach the caller. A failure is logged and counted on the run, so the
    /// receipt can say decisions are missing instead of claiming completeness.
    pub fn record_run_decision(&self, run_id: &str, decision: &NewRunDecision) {
        if run_id.is_empty() {
            return;
        }
        if let Err(error) = self.insert_run_decision(run_id, decision) {
            tracing::warn!(
                run_id,
                tool = decision.tool_name.as_str(),
                %error,
                "could not record a run decision; the tool call is not affected"
            );
            if let Err(error) = self.count_decision_write_failure(run_id) {
                tracing::warn!(run_id, %error, "could not count the missed run decision");
            }
        }
    }

    fn count_decision_write_failure(&self, run_id: &str) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE run_requests SET decision_write_failures = decision_write_failures + 1
             WHERE run_id = ?1",
            [run_id],
        )?;
        Ok(())
    }

    /// Every decision recorded for the run, in order.
    pub fn run_decisions(&self, run_id: &str) -> Result<Vec<RunDecision>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(
            "SELECT seq, session_id, decided_at, tool_name, target, side_effect, disposition,
                    decided_by, deciding_scope, deciding_rule, args_digest,
                    approval_request_id, via_grant, constraint_rule
             FROM run_decisions WHERE run_id = ?1 ORDER BY seq",
        )?;
        let rows = statement.query_map([run_id], |row| {
            Ok(RunDecision {
                seq: row.get(0)?,
                session_id: row.get(1)?,
                decided_at: row.get(2)?,
                tool_name: row.get(3)?,
                target: row.get(4)?,
                side_effect: row.get(5)?,
                disposition: row.get(6)?,
                decided_by: row.get(7)?,
                deciding_scope: row.get(8)?,
                deciding_rule: row.get(9)?,
                args_digest: row.get(10)?,
                approval_request_id: row.get(11)?,
                via_grant: row.get::<_, i64>(12)? != 0,
                constraint_rule: row.get(13)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The run's start and end, requester, isolation and models.
    pub fn run_facts(&self, run_id: &str) -> Result<RunFacts, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let (created_at, ended_at): (Option<i64>, Option<i64>) = connection
            .query_row(
                // Chat runs created before S11 have no `runs.created_at`; their request
                // row's time stands in, the same fallback `run_created_at` uses.
                "SELECT COALESCE(r.created_at,
                          (SELECT q.created_at FROM run_requests q WHERE q.run_id = r.run_id)),
                        r.ended_at
                 FROM runs r WHERE r.run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));
        let request: Option<(String, String, i64)> = connection
            .query_row(
                "SELECT requested_by_user_id, surface, decision_write_failures
                 FROM run_requests WHERE run_id = ?1",
                [run_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let trigger: Option<TriggerFact> = connection
            .query_row(
                "SELECT broker_id, grant_id, trigger_ref, fire_at, fire_kind
                 FROM run_requests WHERE run_id = ?1 AND broker_id IS NOT NULL",
                [run_id],
                |row| {
                    Ok(TriggerFact {
                        broker_id: row.get(0)?,
                        grant_id: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                        trigger_ref: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                        fire_at: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                        fire_kind: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                    })
                },
            )
            .optional()?;
        let mut isolation_stmt = connection.prepare(
            "SELECT session_id, guest_mode, vmm, jailer, api_tls, summary, kernel_sha256,
                    rootfs_sha256, lifecycle_generation, recorded_at, rate_limits
             FROM run_isolation WHERE run_id = ?1 ORDER BY recorded_at, session_id",
        )?;
        let isolation = isolation_stmt
            .query_map([run_id], |row| {
                Ok(RunIsolationFact {
                    session_id: row.get(0)?,
                    guest_mode: row.get(1)?,
                    vmm: row.get(2)?,
                    jailer: row.get::<_, i64>(3)? != 0,
                    api_tls: row.get::<_, i64>(4)? != 0,
                    summary: row.get(5)?,
                    kernel_sha256: row.get(6)?,
                    rootfs_sha256: row.get(7)?,
                    lifecycle_generation: row.get(8)?,
                    recorded_at: row.get(9)?,
                    rate_limits: row.get(10)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut models_stmt = connection.prepare(
            "SELECT model, provider, endpoint_host, calls, prompt_tokens, completion_tokens,
                    first_at, last_at
             FROM run_models WHERE run_id = ?1 ORDER BY first_at, model, provider",
        )?;
        let models = models_stmt
            .query_map([run_id], |row| {
                Ok(RunModelFact {
                    model: row.get(0)?,
                    provider: row.get(1)?,
                    endpoint_host: row.get(2)?,
                    calls: row.get(3)?,
                    prompt_tokens: row.get(4)?,
                    completion_tokens: row.get(5)?,
                    first_at: row.get(6)?,
                    last_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(RunFacts {
            created_at,
            ended_at,
            requested_by_user_id: request.as_ref().map(|r| r.0.clone()),
            surface: request.as_ref().map(|r| r.1.clone()),
            trigger,
            decision_write_failures: request.map(|r| r.2).unwrap_or(0),
            isolation,
            models,
        })
    }

    /// Who decided each approval and gate of the run, from where, and when.
    pub fn approval_provenance(
        &self,
        run_id: &str,
    ) -> Result<Vec<ApprovalProvenance>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut statement = connection.prepare(
            "SELECT request_id, step_id, kind, decision, decided_at, decided_surface,
                    decided_by_user_id, reviewer_principal, amended_args_json, requested_args_json
             FROM approvals WHERE run_id = ?1 ORDER BY created_at, request_id",
        )?;
        let rows = statement.query_map([run_id], |row| {
            Ok(ApprovalProvenance {
                request_id: row.get(0)?,
                step_id: row.get(1)?,
                kind: row.get(2)?,
                decision: row.get(3)?,
                decided_at: row.get(4)?,
                decided_surface: row.get(5)?,
                decided_by_user_id: row.get(6)?,
                reviewer_principal: row.get(7)?,
                response_json: row.get(8)?,
                requested_args_json: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
}

#[cfg(test)]
#[allow(clippy::module_inception)] // existing test module name kept
mod run_facts {
    use super::*;
    use vam_spec::v1::ResourceId;

    fn store() -> SqliteWorkflowStore {
        SqliteWorkflowStore::in_memory().expect("store")
    }

    fn rid(urn: &str) -> ResourceId {
        ResourceId { urn: urn.into() }
    }

    fn new_run(store: &SqliteWorkflowStore, urn: &str) {
        store.create_run(&rid(urn)).expect("create");
    }

    fn decision(tool: &str, disposition: &'static str) -> NewRunDecision {
        NewRunDecision {
            session_id: "sess-1".into(),
            tool_name: tool.into(),
            target: format!("builtin:{tool}"),
            side_effect: 2,
            disposition,
            decided_by: decided_by::CEDAR_RULE,
            deciding_scope: "agent".into(),
            deciding_rule: format!("builtin:{tool}"),
            args_digest: "d".into(),
            approval_request_id: None,
            via_grant: false,
            constraint_rule: None,
        }
    }

    #[test]
    fn a_trigger_request_keeps_its_trigger_and_a_person_has_none() {
        use crate::api::run_start::{RunCaller, RunSource};
        let store = store();
        let mut caller = RunCaller {
            owner_user_id: "u1".into(),
            principal: "p".into(),
            role: crate::users::UserRole::Member,
            scopes: vec![],
            credential_id: None,
            users_admin: false,
            source: RunSource::Trigger {
                broker_id: "b1".into(),
                grant_id: "rg_1".into(),
                trigger_ref: "daily".into(),
                fire_at: 42,
                fire_kind: "scheduled".into(),
                surface: "echo".into(),
            },
            not_after: None,
            key_id: String::new(),
            name: String::new(),
            session_token: false,
            key_owner_user_id: "u1".into(),
        };
        new_run(&store, "urn:vam:run:local.vam.dev/acme/t1");
        store
            .record_run_request_for("urn:vam:run:local.vam.dev/acme/t1", &caller)
            .unwrap();
        let facts = store
            .run_facts("urn:vam:run:local.vam.dev/acme/t1")
            .unwrap();
        assert_eq!(facts.surface.as_deref(), Some("echo"));
        let t = facts.trigger.expect("trigger");
        assert_eq!((t.broker_id.as_str(), t.grant_id.as_str()), ("b1", "rg_1"));
        assert_eq!((t.trigger_ref.as_str(), t.fire_at), ("daily", 42));
        assert_eq!(t.fire_kind, "scheduled");

        caller.source = RunSource::Person {
            surface: "cli".into(),
        };
        new_run(&store, "urn:vam:run:local.vam.dev/acme/t2");
        store
            .record_run_request_for("urn:vam:run:local.vam.dev/acme/t2", &caller)
            .unwrap();
        assert!(store
            .run_facts("urn:vam:run:local.vam.dev/acme/t2")
            .unwrap()
            .trigger
            .is_none());
    }

    #[test]
    fn a_decision_keeps_the_guard_condition_it_passed() {
        let store = store();
        new_run(&store, "urn:vam:run:local.vam.dev/acme/cr");
        let mut d = decision("close_issue", "allowed");
        d.constraint_rule = Some("args_within k in steps.g.output.keys".into());
        store.record_run_decision("urn:vam:run:local.vam.dev/acme/cr", &d);
        store.record_run_decision(
            "urn:vam:run:local.vam.dev/acme/cr",
            &decision("read_file", "allowed"),
        );
        let rows = store
            .run_decisions("urn:vam:run:local.vam.dev/acme/cr")
            .unwrap();
        assert_eq!(
            rows[0].constraint_rule.as_deref(),
            Some("args_within k in steps.g.output.keys")
        );
        assert_eq!(rows[1].constraint_rule, None);
    }

    #[test]
    fn schema_is_idempotent() {
        let store = store();
        let connection = store.connection.lock().unwrap();
        initialize_schema(&connection).expect("second run");
        initialize_schema(&connection).expect("third run");
    }

    #[test]
    fn old_run_isolation_rows_gain_a_null_rate_limits_not_an_on() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE runs (run_id TEXT PRIMARY KEY NOT NULL, state TEXT NOT NULL,
                    revision INTEGER NOT NULL);
                 CREATE TABLE approvals (request_id TEXT PRIMARY KEY NOT NULL, run_id TEXT);
                 CREATE TABLE run_isolation (
                    run_id TEXT NOT NULL, session_id TEXT NOT NULL, guest_mode TEXT NOT NULL,
                    vmm TEXT NOT NULL, jailer INTEGER NOT NULL, api_tls INTEGER NOT NULL,
                    summary TEXT NOT NULL, kernel_sha256 TEXT, rootfs_sha256 TEXT,
                    lifecycle_generation INTEGER NOT NULL, recorded_at INTEGER NOT NULL,
                    PRIMARY KEY (run_id, session_id));
                 INSERT INTO run_isolation VALUES
                    ('urn:old', 's', 'vm', 'firecracker', 1, 1, 'x', NULL, NULL, 1, 1);",
            )
            .unwrap();
        initialize_schema(&connection).expect("upgrade");
        initialize_schema(&connection).expect("and again");
        let v: Option<String> = connection
            .query_row(
                "SELECT rate_limits FROM run_isolation WHERE run_id = 'urn:old'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(v, None);
    }

    #[test]
    fn a_run_records_the_rate_limit_setting_it_started_under() {
        let store = store();
        new_run(&store, "urn:vam:run:local.vam.dev/acme/rl");
        let mut iso = crate::isolation::describe(&crate::isolation::IsolationInputs {
            guest_mode: "process",
            use_jailer: false,
            api_tls: false,
            rate_limits: Default::default(),
        });
        store
            .record_run_isolation(
                "urn:vam:run:local.vam.dev/acme/rl",
                "s1",
                &iso,
                "process",
                None,
                1,
            )
            .unwrap();
        iso.rate_limits = "off".into();
        store
            .record_run_isolation(
                "urn:vam:run:local.vam.dev/acme/rl",
                "s2",
                &iso,
                "process",
                None,
                2,
            )
            .unwrap();
        let facts = store
            .run_facts("urn:vam:run:local.vam.dev/acme/rl")
            .unwrap();
        let got: Vec<_> = facts
            .isolation
            .iter()
            .map(|f| f.rate_limits.clone())
            .collect();
        assert!(got.contains(&Some("on (30/s burst 100, runs 2/s burst 20)".to_string())));
        assert!(got.contains(&Some("off".to_string())));
    }

    #[test]
    fn old_database_upgrades_and_old_rows_have_no_times() {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE runs (run_id TEXT PRIMARY KEY NOT NULL, state TEXT NOT NULL,
                    revision INTEGER NOT NULL);
                 CREATE TABLE approvals (request_id TEXT PRIMARY KEY NOT NULL, run_id TEXT);
                 INSERT INTO runs VALUES ('urn:old', 'SUCCEEDED', 3);",
            )
            .unwrap();
        initialize_schema(&connection).expect("upgrade");
        initialize_schema(&connection).expect("again");
        let (created, ended): (Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT created_at, ended_at FROM runs WHERE run_id = 'urn:old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((created, ended), (None, None));
    }

    #[test]
    fn create_sets_created_at_and_a_resumed_run_keeps_it() {
        let store = store();
        new_run(&store, "urn:vam:run:local.vam.dev/acme/r1");
        {
            let c = store.connection.lock().unwrap();
            c.execute("UPDATE runs SET created_at = 100 WHERE run_id = 'urn:vam:run:local.vam.dev/acme/r1'", [])
                .unwrap();
        }
        new_run(&store, "urn:vam:run:local.vam.dev/acme/r1");
        let facts = store
            .run_facts("urn:vam:run:local.vam.dev/acme/r1")
            .unwrap();
        assert_eq!(facts.created_at, Some(100));
        assert_eq!(facts.ended_at, None);
    }

    #[test]
    fn new_run_has_a_start_time() {
        let store = store();
        new_run(&store, "urn:vam:run:local.vam.dev/acme/r0");
        assert!(
            store
                .run_facts("urn:vam:run:local.vam.dev/acme/r0")
                .unwrap()
                .created_at
                .unwrap()
                > 0
        );
    }

    #[test]
    fn chat_run_has_a_start_time() {
        let store = store();
        let run = "urn:vam:run:local.vam.dev/acme/chat1";
        store
            .persist_run_spec_with_owner(run, "{}", Some("sess-1"), "urn:a", None, "p", "local")
            .unwrap();
        assert!(store.run_facts(run).unwrap().created_at.unwrap() > 0);
    }

    #[test]
    fn old_chat_run_with_no_created_at_uses_its_request_time() {
        let store = store();
        let run = "urn:vam:run:local.vam.dev/acme/chat-old";
        {
            let c = store.connection.lock().unwrap();
            c.execute(
                "INSERT INTO runs (run_id, state, revision) VALUES (?1, 'SUCCEEDED', 1)",
                [run],
            )
            .unwrap();
            c.execute(
                "INSERT INTO run_requests (run_id, requested_by_user_id, surface, created_at)
                 VALUES (?1, 'local', 'chat', 42)",
                [run],
            )
            .unwrap();
        }
        assert_eq!(store.run_facts(run).unwrap().created_at, Some(42));
    }

    #[test]
    fn ended_at_is_set_once_on_terminal_states_only() {
        let store = store();
        new_run(&store, "urn:vam:run:local.vam.dev/acme/r2");
        store
            .update_run_state("urn:vam:run:local.vam.dev/acme/r2", "PAUSED")
            .unwrap();
        assert_eq!(
            store
                .run_facts("urn:vam:run:local.vam.dev/acme/r2")
                .unwrap()
                .ended_at,
            None
        );
        store
            .update_run_state("urn:vam:run:local.vam.dev/acme/r2", "RUNNING")
            .unwrap();
        assert_eq!(
            store
                .run_facts("urn:vam:run:local.vam.dev/acme/r2")
                .unwrap()
                .ended_at,
            None
        );
        store
            .update_run_state("urn:vam:run:local.vam.dev/acme/r2", "SUCCEEDED")
            .unwrap();
        let first = store
            .run_facts("urn:vam:run:local.vam.dev/acme/r2")
            .unwrap()
            .ended_at
            .expect("ended");
        {
            let c = store.connection.lock().unwrap();
            c.execute(
                "UPDATE runs SET ended_at = 5 WHERE run_id = 'urn:vam:run:local.vam.dev/acme/r2'",
                [],
            )
            .unwrap();
        }
        store
            .update_run_state("urn:vam:run:local.vam.dev/acme/r2", "FAILED")
            .unwrap();
        assert_eq!(
            store
                .run_facts("urn:vam:run:local.vam.dev/acme/r2")
                .unwrap()
                .ended_at,
            Some(5)
        );
        assert!(first > 0);
        for state in ["FAILED", "CANCELLED", "TIMED_OUT", "EXPIRED", "REJECTED"] {
            let urn = format!("urn:vam:run:local.vam.dev/acme/r-{}", state.to_lowercase());
            new_run(&store, &urn);
            store.update_run_state(&urn, state).unwrap();
            assert!(store.run_facts(&urn).unwrap().ended_at.is_some(), "{state}");
        }
    }

    #[test]
    fn complete_run_with_result_sets_ended_at() {
        let store = store();
        new_run(&store, "urn:vam:run:local.vam.dev/acme/r3");
        store
            .complete_run_with_result_now("urn:vam:run:local.vam.dev/acme/r3", "SUCCEEDED", "{}")
            .unwrap();
        assert!(store
            .run_facts("urn:vam:run:local.vam.dev/acme/r3")
            .unwrap()
            .ended_at
            .is_some());
    }

    #[test]
    fn request_first_write_wins() {
        let store = store();
        store
            .record_run_request("urn:vam:run:local.vam.dev/acme/r4", "u1", "cli")
            .unwrap();
        store
            .record_run_request("urn:vam:run:local.vam.dev/acme/r4", "u2", "api")
            .unwrap();
        let facts = store
            .run_facts("urn:vam:run:local.vam.dev/acme/r4")
            .unwrap();
        assert_eq!(facts.requested_by_user_id.as_deref(), Some("u1"));
        assert_eq!(facts.surface.as_deref(), Some("cli"));
    }

    #[test]
    fn process_isolation_records_no_image() {
        let store = store();
        let iso = crate::isolation::describe(&crate::isolation::IsolationInputs {
            guest_mode: "process",
            use_jailer: false,
            api_tls: false,
            rate_limits: Default::default(),
        });
        store
            .record_run_isolation(
                "urn:vam:run:local.vam.dev/acme/r5",
                "sess-a",
                &iso,
                "process",
                None,
                1,
            )
            .unwrap();
        store
            .record_run_isolation(
                "urn:vam:run:local.vam.dev/acme/r5",
                "sess-b",
                &iso,
                "process",
                None,
                2,
            )
            .unwrap();
        let facts = store
            .run_facts("urn:vam:run:local.vam.dev/acme/r5")
            .unwrap();
        assert_eq!(facts.isolation.len(), 2);
        let first = &facts.isolation[0];
        assert_eq!(first.guest_mode, "process");
        assert_eq!(first.summary, "process guest (no isolation)");
        assert_eq!(first.kernel_sha256, None);
        assert_eq!(first.rootfs_sha256, None);
    }

    #[test]
    fn vm_isolation_records_image_digests() {
        let store = store();
        let iso = crate::isolation::describe(&crate::isolation::IsolationInputs {
            guest_mode: "vm",
            use_jailer: true,
            api_tls: true,
            rate_limits: Default::default(),
        });
        store
            .record_run_isolation(
                "urn:vam:run:local.vam.dev/acme/r6",
                "sess-a",
                &iso,
                "vm",
                Some(("k".into(), "r".into())),
                1,
            )
            .unwrap();
        let fact = &store
            .run_facts("urn:vam:run:local.vam.dev/acme/r6")
            .unwrap()
            .isolation[0];
        assert_eq!(fact.kernel_sha256.as_deref(), Some("k"));
        assert_eq!(fact.rootfs_sha256.as_deref(), Some("r"));
        assert!(fact.jailer);
    }

    #[test]
    fn models_count_calls_and_a_fallback_is_its_own_row() {
        let store = store();
        store
            .record_model_call(
                "urn:vam:run:local.vam.dev/acme/r7",
                "a",
                "p",
                "api.example.com",
                10,
                5,
            )
            .unwrap();
        store
            .record_model_call(
                "urn:vam:run:local.vam.dev/acme/r7",
                "a",
                "p",
                "api.example.com",
                1,
                2,
            )
            .unwrap();
        store
            .record_model_call(
                "urn:vam:run:local.vam.dev/acme/r7",
                "b",
                "p",
                "api.example.com",
                3,
                4,
            )
            .unwrap();
        let models = store
            .run_facts("urn:vam:run:local.vam.dev/acme/r7")
            .unwrap()
            .models;
        assert_eq!(models.len(), 2);
        let a = models.iter().find(|m| m.model == "a").unwrap();
        assert_eq!((a.calls, a.prompt_tokens, a.completion_tokens), (2, 11, 7));
        assert_eq!(models.iter().find(|m| m.model == "b").unwrap().calls, 1);
    }

    #[test]
    fn endpoint_host_drops_path_query_and_credentials() {
        assert_eq!(
            endpoint_host("https://user:pw@api.example.com:8443/v1/chat?key=secret"),
            "api.example.com"
        );
        assert_eq!(endpoint_host("not a url"), "");
        assert_eq!(endpoint_host(""), "");
    }

    #[test]
    fn surface_header_accepts_cli_and_app_only() {
        assert_eq!(surface_from_header(Some("cli")), "cli");
        assert_eq!(surface_from_header(Some(" App ")), "app");
        assert_eq!(surface_from_header(Some("slack")), "api");
        assert_eq!(surface_from_header(Some("")), "api");
        assert_eq!(surface_from_header(None), "api");
    }

    #[test]
    fn decisions_are_numbered_per_run_in_order() {
        let store = store();
        store
            .insert_run_decision("urn:a", &decision("x", "allowed"))
            .unwrap();
        store
            .insert_run_decision("urn:b", &decision("y", "denied"))
            .unwrap();
        store
            .insert_run_decision("urn:a", &decision("z", "asked"))
            .unwrap();
        let a = store.run_decisions("urn:a").unwrap();
        assert_eq!(a.iter().map(|d| d.seq).collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(a[1].tool_name, "z");
        assert_eq!(store.run_decisions("urn:b").unwrap().len(), 1);
    }

    #[test]
    fn a_failed_decision_write_is_counted_and_does_not_panic() {
        let store = store();
        store.record_run_request("urn:f", "u", "api").unwrap();
        {
            let c = store.connection.lock().unwrap();
            c.execute_batch("DROP TABLE run_decisions").unwrap();
        }
        store.record_run_decision("urn:f", &decision("x", "denied"));
        store.record_run_decision("urn:f", &decision("x", "denied"));
        assert_eq!(store.run_facts("urn:f").unwrap().decision_write_failures, 2);
    }

    #[test]
    fn approval_provenance_reads_the_decision_columns() {
        let store = store();
        {
            let c = store.connection.lock().unwrap();
            c.execute(
                "INSERT INTO approvals (request_id, run_id, step_id, attempt_id,
                   checkpoint_digest, continuation_generation, kind, decision,
                   decided_at, decided_surface, decided_by_user_id, amended_args_json)
                 VALUES ('q1','urn:p','s','a','d',0,'gate','SUBMITTED',7,'cli','u9','{\"option\":\"go\"}')",
                [],
            )
            .unwrap();
        }
        let rows = store.approval_provenance("urn:p").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].decided_at, Some(7));
        assert_eq!(rows[0].decided_surface.as_deref(), Some("cli"));
        assert_eq!(rows[0].decided_by_user_id.as_deref(), Some("u9"));
        assert_eq!(
            rows[0].response_json.as_deref(),
            Some("{\"option\":\"go\"}")
        );
    }
}
