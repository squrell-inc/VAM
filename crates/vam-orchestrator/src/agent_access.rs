//! Agent access (Phase 1, wave 1): who may see and use an agent.
//!
//! Three tables (design/agent-access.md §2.2), a store API on [`SqliteWorkflowStore`], the one
//! rule ([`agent_access`], §2.3) and the SQL predicate the list read will use
//! ([`usable_predicate_sql`]). Nothing in the catalog reads, routes or admission calls the rule
//! yet; that is wave 2. No backfill: the schema is `IF NOT EXISTS` only, and no row is derived
//! from `agents` (no policy row = `(org, everyone)`, today's behaviour).

use rusqlite::{params, Connection, OptionalExtension};

use crate::catalog::unix_timestamp;
use crate::error::OrchestratorError;
use crate::owner::resolved_owner;
use crate::spaces::mint_id;
use crate::workflow::SqliteWorkflowStore;

/// Prefix of a minted access request id.
pub const REQUEST_ID_PREFIX: &str = "acr_";

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS agent_access_policy (
            urn          TEXT PRIMARY KEY,
            visibility   TEXT NOT NULL CHECK (visibility IN ('org','restricted')),
            access_mode  TEXT NOT NULL CHECK (access_mode IN ('everyone','open','approval')),
            updated_by   TEXT NOT NULL,
            updated_at   INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS agent_admins (
            urn       TEXT NOT NULL,
            user_id   TEXT NOT NULL REFERENCES users(user_id),
            added_by  TEXT NOT NULL,
            added_at  INTEGER NOT NULL,
            PRIMARY KEY (urn, user_id)
        );
        CREATE TABLE IF NOT EXISTS agent_access (
            request_id   TEXT PRIMARY KEY,
            urn          TEXT NOT NULL,
            user_id      TEXT NOT NULL REFERENCES users(user_id),
            state        TEXT NOT NULL CHECK (state IN ('pending','approved','denied','revoked','withdrawn')),
            message      TEXT,
            requested_at INTEGER NOT NULL,
            decided_by   TEXT,
            decided_at   INTEGER,
            reason       TEXT
        );
        CREATE UNIQUE INDEX IF NOT EXISTS agent_access_live ON agent_access(urn, user_id)
            WHERE state IN ('pending','approved');
        ",
    )?;
    Ok(())
}

/// Whether an agent is listed for the whole org or only for those with access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visibility {
    Org,
    Restricted,
}

impl Visibility {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Org => "org",
            Self::Restricted => "restricted",
        }
    }

    fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "org" => Ok(Self::Org),
            "restricted" => Ok(Self::Restricted),
            other => Err(OrchestratorError::CorruptState(format!(
                "agent_access_policy.visibility '{other}'"
            ))),
        }
    }
}

/// How a caller gets access.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessMode {
    Everyone,
    Open,
    Approval,
}

impl AccessMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Everyone => "everyone",
            Self::Open => "open",
            Self::Approval => "approval",
        }
    }

    fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "everyone" => Ok(Self::Everyone),
            "open" => Ok(Self::Open),
            "approval" => Ok(Self::Approval),
            other => Err(OrchestratorError::CorruptState(format!(
                "agent_access_policy.access_mode '{other}'"
            ))),
        }
    }
}

/// State of an `agent_access` row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestState {
    Pending,
    Approved,
    Denied,
    Revoked,
    Withdrawn,
}

impl RequestState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Approved => "approved",
            Self::Denied => "denied",
            Self::Revoked => "revoked",
            Self::Withdrawn => "withdrawn",
        }
    }

    fn parse(value: &str) -> Result<Self, OrchestratorError> {
        match value {
            "pending" => Ok(Self::Pending),
            "approved" => Ok(Self::Approved),
            "denied" => Ok(Self::Denied),
            "revoked" => Ok(Self::Revoked),
            "withdrawn" => Ok(Self::Withdrawn),
            other => Err(OrchestratorError::CorruptState(format!(
                "agent_access.state '{other}'"
            ))),
        }
    }

    /// A live state: the one the partial unique index covers.
    pub fn is_live(self) -> bool {
        matches!(self, Self::Pending | Self::Approved)
    }
}

/// A decision on a pending request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    Approve,
    Deny,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentAccessPolicy {
    pub urn: String,
    pub visibility: Visibility,
    pub mode: AccessMode,
    pub updated_by: String,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessRow {
    pub request_id: String,
    pub urn: String,
    pub user_id: String,
    pub state: RequestState,
    pub message: Option<String>,
    pub requested_at: i64,
    pub decided_by: Option<String>,
    pub decided_at: Option<i64>,
    pub reason: Option<String>,
}

/// The caller's live row state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LiveState {
    Pending,
    Approved,
}

/// Outcome of the rule (design §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAccess {
    /// Treat as a missing URN.
    Hidden,
    /// Listed in the browse, refused at admission; `state` is the caller's live row.
    Visible {
        state: Option<LiveState>,
    },
    Usable,
}

/// The rule over already-read facts. `policy` is `None` for no row (= `(org, everyone)`);
/// `(restricted, everyone)` cannot be written (AA-1.6) and reads as `(restricted, approval)`.
pub fn decide_access(
    policy: Option<(Visibility, AccessMode)>,
    org_admin: bool,
    is_agent_admin: bool,
    row_is_own_draft: bool,
    live: Option<LiveState>,
) -> AgentAccess {
    let visibility = match policy {
        None | Some((Visibility::Org, AccessMode::Everyone)) => return AgentAccess::Usable,
        Some((visibility, _)) => visibility,
    };
    if org_admin || is_agent_admin || row_is_own_draft || live == Some(LiveState::Approved) {
        return AgentAccess::Usable;
    }
    match visibility {
        Visibility::Org => AgentAccess::Visible { state: live },
        Visibility::Restricted => AgentAccess::Hidden,
    }
}

/// The one rule: `Hidden | Visible { state } | Usable` for `caller` on `urn`.
/// `org_admin` is the existing `include_foreign_drafts` flag (`users:admin` on the key);
/// `row_is_own_draft` is true when the row read is the caller's own draft.
pub fn agent_access(
    store: &SqliteWorkflowStore,
    urn: &str,
    caller: &str,
    org_admin: bool,
    row_is_own_draft: bool,
) -> Result<AgentAccess, OrchestratorError> {
    let connection = store
        .connection
        .lock()
        .map_err(|_| OrchestratorError::StorageUnavailable)?;
    agent_access_on(&connection, urn, caller, org_admin, row_is_own_draft)
}

/// [`agent_access`] on a connection the caller already holds.
pub(crate) fn agent_access_on(
    connection: &Connection,
    urn: &str,
    caller: &str,
    org_admin: bool,
    row_is_own_draft: bool,
) -> Result<AgentAccess, OrchestratorError> {
    let caller = resolved_owner(caller);
    let Some(policy) = read_policy(connection, urn)? else {
        return Ok(AgentAccess::Usable);
    };
    let is_admin = is_admin_on(connection, urn, caller)?;
    let live = read_live_row(connection, urn, caller)?.and_then(|row| match row.state {
        RequestState::Pending => Some(LiveState::Pending),
        RequestState::Approved => Some(LiveState::Approved),
        _ => None,
    });
    Ok(decide_access(
        Some((policy.visibility, policy.mode)),
        org_admin,
        is_admin,
        row_is_own_draft,
        live,
    ))
}

/// SQL boolean expression: the agent whose URN is `urn_column` is `Usable` to the user bound as
/// `user_param` (for example `?3`), for a caller that is not an org admin and reads no own draft.
/// Mirrors [`decide_access`]: no policy row or `(org, everyone)`, or an agent admin, or an
/// approved row.
pub fn usable_predicate_sql(urn_column: &str, user_param: &str) -> String {
    format!(
        "(NOT EXISTS (SELECT 1 FROM agent_access_policy p WHERE p.urn = {urn_column}
              AND NOT (p.visibility = 'org' AND p.access_mode = 'everyone'))
          OR EXISTS (SELECT 1 FROM agent_admins a
              WHERE a.urn = {urn_column} AND a.user_id = {user_param})
          OR EXISTS (SELECT 1 FROM agent_access x
              WHERE x.urn = {urn_column} AND x.user_id = {user_param} AND x.state = 'approved'))"
    )
}

fn read_policy(
    connection: &Connection,
    urn: &str,
) -> Result<Option<AgentAccessPolicy>, OrchestratorError> {
    let raw = connection
        .query_row(
            "SELECT urn, visibility, access_mode, updated_by, updated_at
             FROM agent_access_policy WHERE urn = ?1",
            params![urn],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, i64>(4)?,
                ))
            },
        )
        .optional()?;
    raw.map(|(urn, visibility, mode, updated_by, updated_at)| {
        Ok(AgentAccessPolicy {
            urn,
            visibility: Visibility::parse(&visibility)?,
            mode: AccessMode::parse(&mode)?,
            updated_by,
            updated_at,
        })
    })
    .transpose()
}

fn is_admin_on(connection: &Connection, urn: &str, user: &str) -> Result<bool, OrchestratorError> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM agent_admins WHERE urn = ?1 AND user_id = ?2",
            params![urn, user],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

const ROW_COLUMNS: &str =
    "request_id, urn, user_id, state, message, requested_at, decided_by, decided_at, reason";

fn map_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(AccessRow, String)> {
    let state: String = row.get(3)?;
    Ok((
        AccessRow {
            request_id: row.get(0)?,
            urn: row.get(1)?,
            user_id: row.get(2)?,
            state: RequestState::Pending,
            message: row.get(4)?,
            requested_at: row.get(5)?,
            decided_by: row.get(6)?,
            decided_at: row.get(7)?,
            reason: row.get(8)?,
        },
        state,
    ))
}

fn finish_row((mut row, state): (AccessRow, String)) -> Result<AccessRow, OrchestratorError> {
    row.state = RequestState::parse(&state)?;
    Ok(row)
}

fn read_live_row(
    connection: &Connection,
    urn: &str,
    user: &str,
) -> Result<Option<AccessRow>, OrchestratorError> {
    connection
        .query_row(
            &format!(
                "SELECT {ROW_COLUMNS} FROM agent_access
                 WHERE urn = ?1 AND user_id = ?2 AND state IN ('pending','approved')"
            ),
            params![urn, user],
            map_row,
        )
        .optional()?
        .map(finish_row)
        .transpose()
}

fn read_request(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<AccessRow>, OrchestratorError> {
    connection
        .query_row(
            &format!("SELECT {ROW_COLUMNS} FROM agent_access WHERE request_id = ?1"),
            params![request_id],
            map_row,
        )
        .optional()?
        .map(finish_row)
        .transpose()
}

/// The A6 hook's variant: seed only when `user` has a `users` row, so a release by a principal
/// that is not a user (a bare key, a test fixture) is never failed by the foreign key. Runs on
/// the connection `release_agent_with_meta` already holds.
pub(crate) fn seed_releaser_on(
    connection: &Connection,
    urn: &str,
    user: &str,
) -> Result<bool, OrchestratorError> {
    let user = resolved_owner(user);
    let added = connection.execute(
        "INSERT OR IGNORE INTO agent_admins (urn, user_id, added_by, added_at)
         SELECT ?1, user_id, user_id, ?3 FROM users WHERE user_id = ?2",
        params![urn, user, unix_timestamp()],
    )?;
    Ok(added > 0)
}

/// Insert `(urn, user)` as an admin unless it already is one. Returns whether a row was added.
/// A user id with no `users` row is a foreign-key error.
pub(crate) fn seed_first_admin_on(
    connection: &Connection,
    urn: &str,
    user: &str,
) -> Result<bool, OrchestratorError> {
    let user = resolved_owner(user);
    let added = connection.execute(
        "INSERT OR IGNORE INTO agent_admins (urn, user_id, added_by, added_at)
         VALUES (?1, ?2, ?2, ?3)",
        params![urn, user, unix_timestamp()],
    )?;
    Ok(added > 0)
}

fn map_unique(error: rusqlite::Error, urn: &str, user: &str) -> OrchestratorError {
    if let rusqlite::Error::SqliteFailure(failure, _) = &error {
        if failure.code == rusqlite::ErrorCode::ConstraintViolation {
            return OrchestratorError::Conflict(format!(
                "user '{user}' already has a live access row for agent '{urn}'"
            ));
        }
    }
    OrchestratorError::Database(error)
}

impl SqliteWorkflowStore {
    fn with_access_connection<T>(
        &self,
        f: impl FnOnce(&Connection) -> Result<T, OrchestratorError>,
    ) -> Result<T, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        f(&connection)
    }

    /// The policy row of `urn`; `None` means `(org, everyone)`.
    pub fn agent_access_policy(
        &self,
        urn: &str,
    ) -> Result<Option<AgentAccessPolicy>, OrchestratorError> {
        self.with_access_connection(|c| read_policy(c, urn))
    }

    /// Upsert the policy of `urn`. Refusing `(restricted, everyone)` is the route's job (AA-1.6);
    /// the rule reads such a row as `(restricted, approval)`.
    pub fn put_agent_access_policy(
        &self,
        urn: &str,
        visibility: Visibility,
        mode: AccessMode,
        by: &str,
    ) -> Result<AgentAccessPolicy, OrchestratorError> {
        let by = resolved_owner(by);
        self.with_access_connection(|c| {
            c.execute(
                "INSERT INTO agent_access_policy (urn, visibility, access_mode, updated_by, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(urn) DO UPDATE SET visibility = excluded.visibility,
                   access_mode = excluded.access_mode, updated_by = excluded.updated_by,
                   updated_at = excluded.updated_at",
                params![urn, visibility.as_str(), mode.as_str(), by, unix_timestamp()],
            )?;
            read_policy(c, urn)?.ok_or_else(|| {
                OrchestratorError::CorruptState(format!("policy of '{urn}' missing after write"))
            })
        })
    }

    pub fn is_agent_admin(&self, urn: &str, user: &str) -> Result<bool, OrchestratorError> {
        let user = resolved_owner(user);
        self.with_access_connection(|c| is_admin_on(c, urn, user))
    }

    /// Make `user` an admin of `urn` (A6). Returns whether a row was added.
    pub fn seed_first_admin(&self, urn: &str, user: &str) -> Result<bool, OrchestratorError> {
        self.with_access_connection(|c| seed_first_admin_on(c, urn, user))
    }

    /// The caller's `pending` or `approved` row for `urn`, if any.
    pub fn live_access_row(
        &self,
        urn: &str,
        user: &str,
    ) -> Result<Option<AccessRow>, OrchestratorError> {
        let user = resolved_owner(user);
        self.with_access_connection(|c| read_live_row(c, urn, user))
    }

    /// Insert a live row for `(urn, user)`: `Pending` for approval mode, `Approved` for `open`
    /// mode (decided by the user themself). A second live row is `Conflict`.
    pub fn insert_access_request(
        &self,
        urn: &str,
        user: &str,
        message: Option<&str>,
        initial: RequestState,
    ) -> Result<AccessRow, OrchestratorError> {
        if !initial.is_live() {
            return Err(OrchestratorError::InvalidContract(
                "an access request starts pending or approved".to_string(),
            ));
        }
        let user = resolved_owner(user);
        let now = unix_timestamp();
        let (decided_by, decided_at) = match initial {
            RequestState::Approved => (Some(user), Some(now)),
            _ => (None, None),
        };
        self.with_access_connection(|c| {
            let id = mint_id(REQUEST_ID_PREFIX);
            c.execute(
                "INSERT INTO agent_access
                   (request_id, urn, user_id, state, message, requested_at, decided_by, decided_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    id,
                    urn,
                    user,
                    initial.as_str(),
                    message,
                    now,
                    decided_by,
                    decided_at
                ],
            )
            .map_err(|e| map_unique(e, urn, user))?;
            read_request(c, &id)?.ok_or_else(|| {
                OrchestratorError::CorruptState(format!(
                    "access request '{id}' missing after write"
                ))
            })
        })
    }

    pub fn get_access_request(
        &self,
        request_id: &str,
    ) -> Result<Option<AccessRow>, OrchestratorError> {
        self.with_access_connection(|c| read_request(c, request_id))
    }

    /// Approve or deny a `pending` request. Any other state is `Conflict`, an unknown id `NotFound`.
    pub fn decide_access_request(
        &self,
        request_id: &str,
        decision: Decision,
        by: &str,
        reason: Option<&str>,
    ) -> Result<AccessRow, OrchestratorError> {
        let by = resolved_owner(by);
        let state = match decision {
            Decision::Approve => RequestState::Approved,
            Decision::Deny => RequestState::Denied,
        };
        self.with_access_connection(|c| {
            let changed = c.execute(
                "UPDATE agent_access SET state = ?2, decided_by = ?3, decided_at = ?4, reason = ?5
                 WHERE request_id = ?1 AND state = 'pending'",
                params![request_id, state.as_str(), by, unix_timestamp(), reason],
            )?;
            let row = read_request(c, request_id)?.ok_or_else(|| {
                OrchestratorError::NotFound(format!("access request '{request_id}' not found"))
            })?;
            if changed == 0 {
                return Err(OrchestratorError::Conflict(format!(
                    "access request '{request_id}' is {}, not pending",
                    row.state.as_str()
                )));
            }
            Ok(row)
        })
    }

    /// Revoke the `approved` row of `(urn, user)`. No live row is `NotFound`; a pending row is
    /// `Conflict` (it is decided, not revoked).
    pub fn revoke_access(
        &self,
        urn: &str,
        user: &str,
        by: &str,
        reason: Option<&str>,
    ) -> Result<AccessRow, OrchestratorError> {
        let user = resolved_owner(user);
        let by = resolved_owner(by);
        self.with_access_connection(|c| {
            let live = read_live_row(c, urn, user)?.ok_or_else(|| {
                OrchestratorError::NotFound(format!(
                    "user '{user}' has no access row for agent '{urn}'"
                ))
            })?;
            if live.state != RequestState::Approved {
                return Err(OrchestratorError::Conflict(format!(
                    "access row of user '{user}' for agent '{urn}' is {}, not approved",
                    live.state.as_str()
                )));
            }
            c.execute(
                "UPDATE agent_access SET state = 'revoked', decided_by = ?2, decided_at = ?3, reason = ?4
                 WHERE request_id = ?1",
                params![live.request_id, by, unix_timestamp(), reason],
            )?;
            read_request(c, &live.request_id)?.ok_or_else(|| {
                OrchestratorError::CorruptState("access row missing after revoke".to_string())
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const URN: &str = "urn:vam:agent:local.vam.dev/acme/gated";

    fn store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_catalog_schema().expect("catalog schema");
        // `agent_admins` and `agent_access` reference `users(user_id)` (enforced).
        for user in ["u1", "u2", "u3", "owner", "adm"] {
            store.create_user(user, user).expect("user");
        }
        store
    }

    fn rule(store: &SqliteWorkflowStore, user: &str, admin: bool, own: bool) -> AgentAccess {
        agent_access(store, URN, user, admin, own).expect("rule")
    }

    fn table_count(store: &SqliteWorkflowStore, table: &str) -> i64 {
        let c = store.connection.lock().unwrap();
        c.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn second_schema_init_is_a_noop_and_keeps_rows() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Org, AccessMode::Approval, "u1")
            .unwrap();
        {
            let c = store.connection.lock().unwrap();
            initialize_schema(&c).expect("second init");
            initialize_schema(&c).expect("third init");
        }
        assert_eq!(table_count(&store, "agent_access_policy"), 1);
        assert_eq!(table_count(&store, "agent_admins"), 0);
        assert_eq!(table_count(&store, "agent_access"), 0);
        let c = store.connection.lock().unwrap();
        let index: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='agent_access_live'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index, 1);
    }

    #[test]
    fn fresh_store_has_empty_tables_no_backfill() {
        let store = store();
        assert_eq!(table_count(&store, "agent_access_policy"), 0);
        assert_eq!(table_count(&store, "agent_admins"), 0);
        assert_eq!(table_count(&store, "agent_access"), 0);
    }

    #[test]
    fn policy_roundtrip_and_upsert() {
        let store = store();
        assert_eq!(store.agent_access_policy(URN).unwrap(), None);
        let first = store
            .put_agent_access_policy(URN, Visibility::Org, AccessMode::Approval, "u1")
            .unwrap();
        assert_eq!(first.visibility, Visibility::Org);
        assert_eq!(first.mode, AccessMode::Approval);
        assert_eq!(first.updated_by, "u1");
        let second = store
            .put_agent_access_policy(URN, Visibility::Restricted, AccessMode::Open, "")
            .unwrap();
        assert_eq!(second.visibility, Visibility::Restricted);
        assert_eq!(second.mode, AccessMode::Open);
        assert_eq!(second.updated_by, crate::users::LOCAL_USER_ID);
        assert_eq!(table_count(&store, "agent_access_policy"), 1);
    }

    #[test]
    fn admin_seed_is_idempotent_and_per_urn() {
        let store = store();
        assert!(!store.is_agent_admin(URN, "u1").unwrap());
        assert!(store.seed_first_admin(URN, "u1").unwrap());
        assert!(!store.seed_first_admin(URN, "u1").unwrap());
        assert!(store.is_agent_admin(URN, "u1").unwrap());
        assert!(!store.is_agent_admin(URN, "u2").unwrap());
        assert!(!store.is_agent_admin("urn:other", "u1").unwrap());
        // An empty user id resolves to the local user.
        assert!(store.seed_first_admin("urn:other", "").unwrap());
        assert!(store
            .is_agent_admin("urn:other", crate::users::LOCAL_USER_ID)
            .unwrap());
    }

    #[test]
    fn request_insert_get_and_live_row() {
        let store = store();
        assert_eq!(store.live_access_row(URN, "u1").unwrap(), None);
        let row = store
            .insert_access_request(URN, "u1", Some("please"), RequestState::Pending)
            .unwrap();
        assert!(row.request_id.starts_with(REQUEST_ID_PREFIX));
        assert_eq!(row.state, RequestState::Pending);
        assert_eq!(row.message.as_deref(), Some("please"));
        assert_eq!(row.decided_by, None);
        assert_eq!(
            store.get_access_request(&row.request_id).unwrap(),
            Some(row.clone())
        );
        assert_eq!(store.live_access_row(URN, "u1").unwrap(), Some(row));
        assert_eq!(store.get_access_request("acr_missing").unwrap(), None);
        let open = store
            .insert_access_request(URN, "u2", None, RequestState::Approved)
            .unwrap();
        assert_eq!(open.state, RequestState::Approved);
        assert_eq!(open.decided_by.as_deref(), Some("u2"));
        assert!(matches!(
            store.insert_access_request(URN, "u3", None, RequestState::Denied),
            Err(OrchestratorError::InvalidContract(_))
        ));
    }

    #[test]
    fn second_live_row_is_conflict_but_history_is_kept() {
        let store = store();
        let first = store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .unwrap();
        let err = store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .expect_err("second live row");
        assert!(matches!(err, OrchestratorError::Conflict(_)), "{err:?}");
        let err = store
            .insert_access_request(URN, "u1", None, RequestState::Approved)
            .expect_err("approved over pending");
        assert!(matches!(err, OrchestratorError::Conflict(_)));
        // Another user or another agent is fine.
        store
            .insert_access_request(URN, "u2", None, RequestState::Pending)
            .unwrap();
        store
            .insert_access_request("urn:other", "u1", None, RequestState::Pending)
            .unwrap();
        // After a denial the user may ask again; the old row stays.
        store
            .decide_access_request(&first.request_id, Decision::Deny, "adm", Some("no"))
            .unwrap();
        store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .unwrap();
        let c = store.connection.lock().unwrap();
        let rows: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM agent_access WHERE urn = ?1 AND user_id = 'u1'",
                [URN],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 2);
    }

    #[test]
    fn decide_approve_and_deny_only_from_pending() {
        let store = store();
        let a = store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .unwrap();
        let approved = store
            .decide_access_request(&a.request_id, Decision::Approve, "adm", Some("ok"))
            .unwrap();
        assert_eq!(approved.state, RequestState::Approved);
        assert_eq!(approved.decided_by.as_deref(), Some("adm"));
        assert_eq!(approved.reason.as_deref(), Some("ok"));
        assert!(approved.decided_at.is_some());
        assert!(matches!(
            store.decide_access_request(&a.request_id, Decision::Deny, "adm", None),
            Err(OrchestratorError::Conflict(_))
        ));
        let b = store
            .insert_access_request(URN, "u2", None, RequestState::Pending)
            .unwrap();
        let denied = store
            .decide_access_request(&b.request_id, Decision::Deny, "adm", Some("no"))
            .unwrap();
        assert_eq!(denied.state, RequestState::Denied);
        assert_eq!(store.live_access_row(URN, "u2").unwrap(), None);
        assert!(matches!(
            store.decide_access_request("acr_missing", Decision::Approve, "adm", None),
            Err(OrchestratorError::NotFound(_))
        ));
    }

    #[test]
    fn revoke_marks_approved_row_revoked() {
        let store = store();
        assert!(matches!(
            store.revoke_access(URN, "u1", "adm", None),
            Err(OrchestratorError::NotFound(_))
        ));
        let row = store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .unwrap();
        assert!(matches!(
            store.revoke_access(URN, "u1", "adm", None),
            Err(OrchestratorError::Conflict(_))
        ));
        store
            .decide_access_request(&row.request_id, Decision::Approve, "adm", None)
            .unwrap();
        let revoked = store.revoke_access(URN, "u1", "adm", Some("left")).unwrap();
        assert_eq!(revoked.state, RequestState::Revoked);
        assert_eq!(revoked.request_id, row.request_id);
        assert_eq!(revoked.reason.as_deref(), Some("left"));
        assert_eq!(store.live_access_row(URN, "u1").unwrap(), None);
        // Revoked is history: a new request is allowed.
        store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .unwrap();
    }

    // The rule: one test per outcome row and Usable reason of design §2.3.

    #[test]
    fn rule_no_policy_row_is_usable() {
        let store = store();
        assert_eq!(rule(&store, "u1", false, false), AgentAccess::Usable);
    }

    #[test]
    fn rule_org_everyone_is_usable() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Org, AccessMode::Everyone, "adm")
            .unwrap();
        assert_eq!(rule(&store, "u1", false, false), AgentAccess::Usable);
    }

    #[test]
    fn rule_org_admin_is_usable_on_every_gated_policy() {
        let store = store();
        for (v, m) in [
            (Visibility::Org, AccessMode::Approval),
            (Visibility::Org, AccessMode::Open),
            (Visibility::Restricted, AccessMode::Approval),
            (Visibility::Restricted, AccessMode::Open),
        ] {
            store.put_agent_access_policy(URN, v, m, "adm").unwrap();
            assert_eq!(rule(&store, "u1", true, false), AgentAccess::Usable);
        }
    }

    #[test]
    fn rule_agent_admin_is_usable() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Restricted, AccessMode::Approval, "adm")
            .unwrap();
        store.seed_first_admin(URN, "owner").unwrap();
        assert_eq!(rule(&store, "owner", false, false), AgentAccess::Usable);
        assert_eq!(rule(&store, "u1", false, false), AgentAccess::Hidden);
    }

    #[test]
    fn rule_own_draft_is_usable() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Restricted, AccessMode::Approval, "adm")
            .unwrap();
        assert_eq!(rule(&store, "u1", false, true), AgentAccess::Usable);
        assert_eq!(rule(&store, "u1", false, false), AgentAccess::Hidden);
    }

    #[test]
    fn rule_approved_row_is_usable_then_revoke_hides_again() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Org, AccessMode::Approval, "adm")
            .unwrap();
        let row = store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .unwrap();
        store
            .decide_access_request(&row.request_id, Decision::Approve, "adm", None)
            .unwrap();
        assert_eq!(rule(&store, "u1", false, false), AgentAccess::Usable);
        store.revoke_access(URN, "u1", "adm", None).unwrap();
        assert_eq!(
            rule(&store, "u1", false, false),
            AgentAccess::Visible { state: None }
        );
    }

    #[test]
    fn rule_org_visibility_without_access_is_visible_with_state() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Org, AccessMode::Approval, "adm")
            .unwrap();
        assert_eq!(
            rule(&store, "u1", false, false),
            AgentAccess::Visible { state: None }
        );
        store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .unwrap();
        assert_eq!(
            rule(&store, "u1", false, false),
            AgentAccess::Visible {
                state: Some(LiveState::Pending)
            }
        );
        // A denied row is history: state None again.
        let row = store.live_access_row(URN, "u1").unwrap().unwrap();
        store
            .decide_access_request(&row.request_id, Decision::Deny, "adm", None)
            .unwrap();
        assert_eq!(
            rule(&store, "u1", false, false),
            AgentAccess::Visible { state: None }
        );
    }

    #[test]
    fn rule_restricted_without_access_is_hidden_even_with_pending_row() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Restricted, AccessMode::Open, "adm")
            .unwrap();
        assert_eq!(rule(&store, "u1", false, false), AgentAccess::Hidden);
        store
            .insert_access_request(URN, "u1", None, RequestState::Pending)
            .unwrap();
        assert_eq!(rule(&store, "u1", false, false), AgentAccess::Hidden);
    }

    #[test]
    fn rule_restricted_everyone_reads_as_restricted_approval() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Restricted, AccessMode::Everyone, "adm")
            .unwrap();
        assert_eq!(rule(&store, "u1", false, false), AgentAccess::Hidden);
        assert_eq!(rule(&store, "u1", true, false), AgentAccess::Usable);
        store.seed_first_admin(URN, "owner").unwrap();
        assert_eq!(rule(&store, "owner", false, false), AgentAccess::Usable);
    }

    #[test]
    fn rule_empty_caller_resolves_to_local_user() {
        let store = store();
        store
            .put_agent_access_policy(URN, Visibility::Restricted, AccessMode::Approval, "adm")
            .unwrap();
        store
            .seed_first_admin(URN, crate::users::LOCAL_USER_ID)
            .unwrap();
        assert_eq!(rule(&store, "", false, false), AgentAccess::Usable);
    }

    #[test]
    fn decide_access_pure_table() {
        use AccessMode::*;
        use Visibility::*;
        assert_eq!(
            decide_access(None, false, false, false, None),
            AgentAccess::Usable
        );
        assert_eq!(
            decide_access(Some((Org, Everyone)), false, false, false, None),
            AgentAccess::Usable
        );
        assert_eq!(
            decide_access(
                Some((Org, Open)),
                false,
                false,
                false,
                Some(LiveState::Pending)
            ),
            AgentAccess::Visible {
                state: Some(LiveState::Pending)
            }
        );
        assert_eq!(
            decide_access(Some((Restricted, Approval)), false, false, false, None),
            AgentAccess::Hidden
        );
        assert_eq!(
            decide_access(
                Some((Restricted, Approval)),
                false,
                false,
                false,
                Some(LiveState::Approved)
            ),
            AgentAccess::Usable
        );
    }

    #[test]
    fn sql_predicate_matches_rule() {
        let store = store();
        let urns: Vec<String> = (0..7)
            .map(|i| format!("urn:vam:agent:local.vam.dev/a/p{i}"))
            .collect();
        // p0 no policy; p1 org/everyone; p2 org/approval (pending); p3 org/open (approved);
        // p4 restricted/approval (approved); p5 restricted/everyone; p6 org/approval, u1 admin.
        let policies = [
            (1, Visibility::Org, AccessMode::Everyone),
            (2, Visibility::Org, AccessMode::Approval),
            (3, Visibility::Org, AccessMode::Open),
            (4, Visibility::Restricted, AccessMode::Approval),
            (5, Visibility::Restricted, AccessMode::Everyone),
            (6, Visibility::Org, AccessMode::Approval),
        ];
        for (i, v, m) in policies {
            store
                .put_agent_access_policy(&urns[i], v, m, "adm")
                .unwrap();
        }
        store.seed_first_admin(&urns[6], "u1").unwrap();
        for (i, state) in [
            (3, RequestState::Approved),
            (2, RequestState::Pending),
            (4, RequestState::Approved),
        ] {
            store
                .insert_access_request(&urns[i], "u1", None, state)
                .unwrap();
        }
        let sql = format!(
            "SELECT COUNT(*) FROM (SELECT ?1 AS urn) t WHERE {}",
            usable_predicate_sql("t.urn", "?2")
        );
        for urn in &urns {
            for user in ["u1", "u2"] {
                let via_rule =
                    agent_access(&store, urn, user, false, false).unwrap() == AgentAccess::Usable;
                let c = store.connection.lock().unwrap();
                let via_sql: i64 = c.query_row(&sql, params![urn, user], |r| r.get(0)).unwrap();
                assert_eq!(via_sql == 1, via_rule, "{urn} as {user}");
            }
        }
    }
}
