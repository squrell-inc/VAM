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
        CREATE INDEX IF NOT EXISTS agent_access_by_user ON agent_access(user_id, requested_at);
        CREATE TABLE IF NOT EXISTS agent_access_replays (
            owner_user_id   TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            fingerprint     TEXT NOT NULL,
            status          INTEGER NOT NULL,
            body            TEXT NOT NULL,
            created_at      INTEGER NOT NULL,
            PRIMARY KEY (owner_user_id, idempotency_key)
        );
        CREATE INDEX IF NOT EXISTS agent_access_replays_created
            ON agent_access_replays(created_at);
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

/// The error a refused outcome answers: Hidden is the route's own missing-URN error (built by
/// the caller from the same constructor a missing URN uses, so the text is equal by
/// construction), Visible is [`OrchestratorError::AgentAccessRequired`]. There is no
/// `AgentHidden` variant on purpose. `Usable` is a caller bug and answers like Hidden.
pub(crate) fn refusal(
    access: AgentAccess,
    urn: &str,
    hidden: impl FnOnce() -> OrchestratorError,
) -> OrchestratorError {
    match access {
        AgentAccess::Visible { .. } => OrchestratorError::AgentAccessRequired {
            urn: urn.to_string(),
        },
        AgentAccess::Hidden | AgentAccess::Usable => hidden(),
    }
}

/// Apply the rule at an admission site: `Ok` when `Usable`, else the [`refusal`].
pub(crate) fn require_usable(
    store: &SqliteWorkflowStore,
    urn: &str,
    caller: &str,
    org_admin: bool,
    row_is_own_draft: bool,
    hidden: impl FnOnce() -> OrchestratorError,
) -> Result<(), OrchestratorError> {
    match agent_access(store, urn, caller, org_admin, row_is_own_draft)? {
        AgentAccess::Usable => Ok(()),
        denied => Err(refusal(denied, urn, hidden)),
    }
}

/// The admin rule of design §2.3 for the policy, add and revoke routes: `users:admin` on the
/// key, or an `agent_admins` row. Deliberately not `pdp.authorize`, `catalog:write`,
/// `publish:grant` or `keys:admin`.
pub fn is_agent_or_org_admin(
    store: &SqliteWorkflowStore,
    urn: &str,
    user: &str,
    has_users_admin: bool,
) -> Result<bool, OrchestratorError> {
    if has_users_admin {
        return Ok(true);
    }
    store.is_agent_admin(urn, user)
}

/// [`is_agent_or_org_admin`] on a connection the caller already holds.
pub(crate) fn is_agent_or_org_admin_on(
    connection: &Connection,
    urn: &str,
    user: &str,
    has_users_admin: bool,
) -> Result<bool, OrchestratorError> {
    if has_users_admin {
        return Ok(true);
    }
    is_admin_on(connection, urn, resolved_owner(user))
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

/// A5: the default of a marketplace install. Writes `(org, approval)` for `urn` only when the
/// agent has no released row in `agents` yet, so it runs before the install's own write. An
/// update (or a pre-upgrade install getting its first update) finds its released row and writes
/// nothing, and an existing policy is never replaced. A draft is not an install: marketplace
/// import refuses a URN that has a local draft (`catalog_tier != marketplace`), and a stray draft
/// row must not suppress the default of a first install. Returns whether a policy row was added. Runs in the
/// install's transaction.
pub(crate) fn write_install_default_on(
    connection: &Connection,
    urn: &str,
    by: &str,
    at: i64,
) -> Result<bool, OrchestratorError> {
    let known: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM agents WHERE urn = ?1 AND channel = 'released')",
        params![urn],
        |r| r.get(0),
    )?;
    if known {
        return Ok(false);
    }
    let added = connection.execute(
        "INSERT OR IGNORE INTO agent_access_policy (urn, visibility, access_mode, updated_by, updated_at)
         VALUES (?1, 'org', 'approval', ?2, ?3)",
        params![urn, resolved_owner(by), at],
    )?;
    Ok(added > 0)
}

/// What [`SqliteWorkflowStore::request_agent_access`] did.
#[derive(Debug)]
pub enum AccessAttempt {
    Hidden,
    Usable,
    AlreadyPending,
    Inserted(AccessRow),
}

fn insert_access_request_on(
    c: &Connection,
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
        OrchestratorError::CorruptState(format!("access request '{id}' missing after write"))
    })
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

// ---------------------------------------------------------------------------------------------
// Phase 2 (wave 1): connection-level `_on` variants. The access routes run their whole request
// inside one `with_replay` lock (a `std::sync::Mutex` is not reentrant), so every read and write
// they make takes the connection and never the store.
// ---------------------------------------------------------------------------------------------

pub(crate) fn put_policy_on(
    c: &Connection,
    urn: &str,
    visibility: Visibility,
    mode: AccessMode,
    by: &str,
) -> Result<AgentAccessPolicy, OrchestratorError> {
    let by = resolved_owner(by);
    c.execute(
        "INSERT INTO agent_access_policy (urn, visibility, access_mode, updated_by, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT(urn) DO UPDATE SET visibility = excluded.visibility,
           access_mode = excluded.access_mode, updated_by = excluded.updated_by,
           updated_at = excluded.updated_at",
        params![
            urn,
            visibility.as_str(),
            mode.as_str(),
            by,
            unix_timestamp()
        ],
    )?;
    read_policy(c, urn)?.ok_or_else(|| {
        OrchestratorError::CorruptState(format!("policy of '{urn}' missing after write"))
    })
}

/// [`SqliteWorkflowStore::request_agent_access`] on a held connection.
pub(crate) fn request_access_on(
    c: &Connection,
    urn: &str,
    user: &str,
    org_admin: bool,
    row_is_own_draft: bool,
    message: Option<&str>,
) -> Result<AccessAttempt, OrchestratorError> {
    match agent_access_on(c, urn, user, org_admin, row_is_own_draft)? {
        AgentAccess::Hidden => return Ok(AccessAttempt::Hidden),
        AgentAccess::Usable => return Ok(AccessAttempt::Usable),
        AgentAccess::Visible {
            state: Some(LiveState::Pending),
        } => return Ok(AccessAttempt::AlreadyPending),
        AgentAccess::Visible { .. } => {}
    }
    // A `(restricted, everyone)` row cannot be written; the rule reads one as approval.
    let initial = match read_policy(c, urn)?.map(|p| p.mode) {
        Some(AccessMode::Open) => RequestState::Approved,
        _ => RequestState::Pending,
    };
    insert_access_request_on(c, urn, user, message, initial).map(AccessAttempt::Inserted)
}

/// [`SqliteWorkflowStore::decide_access_request`] on a held connection.
pub(crate) fn decide_on(
    c: &Connection,
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
}

/// [`SqliteWorkflowStore::revoke_access`] on a held connection.
pub(crate) fn revoke_on(
    c: &Connection,
    urn: &str,
    user: &str,
    by: &str,
    reason: Option<&str>,
) -> Result<AccessRow, OrchestratorError> {
    let user = resolved_owner(user);
    let by = resolved_owner(by);
    let live = read_live_row(c, urn, user)?.ok_or_else(|| {
        OrchestratorError::NotFound(format!("user '{user}' has no access row for agent '{urn}'"))
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
}

/// The caller's own live row to `withdrawn` (a pending request is withdrawn, an approved grant
/// is left). There is no rule check: the route is keyed on the caller's own row, and a row the
/// caller already holds is theirs to end even after the agent turned restricted. Without a live
/// row the answer is the missing-URN text for a missing or Hidden agent, and `you have no
/// access request` for a Visible one. Returns the row as it was.
pub(crate) fn withdraw_on(
    c: &Connection,
    urn: &str,
    user: &str,
    org_admin: bool,
) -> Result<AccessRow, OrchestratorError> {
    let user = resolved_owner(user);
    match read_live_row(c, urn, user)? {
        Some(row) => {
            c.execute(
                "UPDATE agent_access SET state = 'withdrawn', decided_by = ?2, decided_at = ?3
                 WHERE request_id = ?1",
                params![row.request_id, user, unix_timestamp()],
            )?;
            Ok(row)
        }
        None => {
            standing_on(c, urn, user, org_admin)?;
            Err(OrchestratorError::NotFound(format!(
                "you have no access request for agent '{urn}'"
            )))
        }
    }
}

/// [`SqliteWorkflowStore::live_access_row`] on a held connection.
pub(crate) fn live_access_row_on(
    c: &Connection,
    urn: &str,
    user: &str,
) -> Result<Option<AccessRow>, OrchestratorError> {
    read_live_row(c, urn, resolved_owner(user))
}

/// [`SqliteWorkflowStore::get_access_request`] on a held connection.
pub(crate) fn get_access_request_on(
    c: &Connection,
    request_id: &str,
) -> Result<Option<AccessRow>, OrchestratorError> {
    read_request(c, request_id)
}

/// What the caller is to an agent, after the agent was read.
pub(crate) struct Standing {
    pub admin: bool,
    pub own_draft: bool,
}

/// Read the agent as the routes' own missing-URN answer and classify the caller. A missing URN
/// and a Hidden one both answer `agents '<urn>' not found in catalog` (one constructor).
pub(crate) fn standing_on(
    c: &Connection,
    urn: &str,
    caller: &str,
    org_admin: bool,
) -> Result<Standing, OrchestratorError> {
    let record = crate::catalog::agent_record_admitted_on(c, urn, caller, org_admin)?;
    let own_draft = record.channel == vam_spec::v1::SkillChannel::Draft as i32
        && resolved_owner(&record.owner_user_id) == resolved_owner(caller);
    if agent_access_on(c, urn, caller, org_admin, own_draft)? == AgentAccess::Hidden {
        return Err(crate::catalog::catalog_not_found("agents", urn));
    }
    let admin = is_agent_or_org_admin_on(c, urn, caller, org_admin)?;
    Ok(Standing { admin, own_draft })
}

/// The display name of an agent, from its latest released row, else its draft; `None` when the
/// agent has no row or no name.
pub(crate) fn agent_display_name_on(c: &Connection, urn: &str) -> Option<String> {
    crate::catalog::agent_record_admitted_on(c, urn, "", true)
        .ok()
        .and_then(|record| record.spec)
        .map(|spec| spec.display_name)
        .filter(|name| !name.is_empty())
}

/// An access row with what the JSON needs from other tables, read under the same lock.
#[derive(Debug, Clone)]
pub struct AccessView {
    pub row: AccessRow,
    pub agent_display_name: Option<String>,
    pub username: Option<String>,
}

/// `show_agent` is false when the rule gives Hidden for the reader: the agent's name is left out.
pub(crate) fn access_view_on(c: &Connection, row: AccessRow, show_agent: bool) -> AccessView {
    let agent_display_name = if show_agent {
        agent_display_name_on(c, &row.urn)
    } else {
        None
    };
    let username = crate::users::get_user_on(c, &row.user_id)
        .ok()
        .map(|user| user.username);
    AccessView {
        row,
        agent_display_name,
        username,
    }
}

/// SQL boolean expression: the agent whose URN is `urn_column` may be listed to the user bound
/// as `user_param`: it is not restricted, or it is Usable (an agent admin or an approved row).
/// An org admin lists everything and does not use this. Mirrors [`decide_access`]: Hidden is
/// exactly "restricted and not Usable".
pub fn listable_predicate_sql(urn_column: &str, user_param: &str) -> String {
    format!(
        "(NOT EXISTS (SELECT 1 FROM agent_access_policy lp WHERE lp.urn = {urn_column}
              AND lp.visibility = 'restricted')
          OR {usable})",
        usable = usable_predicate_sql(urn_column, user_param)
    )
}

/// SQL boolean expression: the user bound as `user_param` administers the agent in `urn_column`.
/// An org admin administers all (`1`). Shared by the request lists and the `/v1/me` count.
pub fn admin_scope_sql(urn_column: &str, user_param: &str, org_admin: bool) -> String {
    if org_admin {
        "1".to_string()
    } else {
        format!(
            "EXISTS (SELECT 1 FROM agent_admins sa WHERE sa.urn = {urn_column}
                AND sa.user_id = {user_param})"
        )
    }
}

/// One row of the browse list (design §6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowseAgent {
    pub urn: String,
    pub display_name: String,
    pub description: String,
    pub catalog_tier: String,
    pub latest_version: String,
    pub visibility: Visibility,
    pub mode: AccessMode,
    /// `none | pending | approved | denied | revoked`.
    pub state: &'static str,
    /// Set only when a row backs `state`.
    pub request_id: Option<String>,
    pub decided_at: Option<i64>,
    pub reason: Option<String>,
    pub can_admin: bool,
}

/// One page of browse: released agents sorted by URN with `urn > after`, those the caller may
/// list, at most `limit` rows. The visibility filter, `canAdmin` and the caller's row state are
/// all computed in SQL on every page.
fn browse_agents_on(
    c: &Connection,
    caller: &str,
    org_admin: bool,
    after_urn: &str,
    limit: usize,
) -> Result<Vec<BrowseAgent>, OrchestratorError> {
    use prost::Message;
    let caller = resolved_owner(caller);
    let usable = usable_predicate_sql("ag.urn", "?1");
    let listable = listable_predicate_sql("ag.urn", "?1");
    // Aliases `p`, `a` and `x` belong to the predicates' own subqueries; the outer query uses
    // `ag`, `pol` and `req` so `ag.urn = a.urn` cannot bind to the wrong table.
    let sql = format!(
        "SELECT ag.urn, ag.spec_bytes, ag.catalog_tier, ag.major, ag.minor, ag.patch, ag.prerelease,
                pol.visibility, pol.access_mode,
                (?2 OR EXISTS (SELECT 1 FROM agent_admins ad WHERE ad.urn = ag.urn AND ad.user_id = ?1)),
                (?2 OR {usable}),
                req.request_id, req.state, req.decided_at, req.reason
         FROM agents ag
         LEFT JOIN agent_access_policy pol ON pol.urn = ag.urn
         LEFT JOIN agent_access req ON req.rowid = (
             SELECT r2.rowid FROM agent_access r2 WHERE r2.urn = ag.urn AND r2.user_id = ?1
             ORDER BY r2.state IN ('pending','approved') DESC, r2.requested_at DESC, r2.rowid DESC
             LIMIT 1)
         WHERE ag.channel = 'released'
           AND NOT EXISTS (SELECT 1 FROM agents nb WHERE nb.urn = ag.urn AND nb.channel = 'released'
                 AND (nb.major, nb.minor, nb.patch, nb.rowid) > (ag.major, ag.minor, ag.patch, ag.rowid))
           AND ag.urn > ?3
           AND (?2 OR {listable})
         ORDER BY ag.urn
         LIMIT ?4"
    );
    let mut stmt = c.prepare(&sql)?;
    let rows = stmt.query_map(params![caller, org_admin, after_urn, limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, String>(2)?,
            (
                r.get::<_, i64>(3)?,
                r.get::<_, i64>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, String>(6)?,
            ),
            r.get::<_, Option<String>>(7)?,
            r.get::<_, Option<String>>(8)?,
            r.get::<_, bool>(9)?,
            r.get::<_, bool>(10)?,
            r.get::<_, Option<String>>(11)?,
            r.get::<_, Option<String>>(12)?,
            r.get::<_, Option<i64>>(13)?,
            r.get::<_, Option<String>>(14)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (
            urn,
            spec_bytes,
            catalog_tier,
            (major, minor, patch, prerelease),
            visibility,
            mode,
            can_admin,
            is_usable,
            request_id,
            row_state,
            decided_at,
            reason,
        ) = row?;
        let spec = vam_spec::v1::AgentSpec::decode(&*spec_bytes)
            .map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let visibility = visibility
            .as_deref()
            .map(Visibility::parse)
            .transpose()?
            .unwrap_or(Visibility::Org);
        let mode = match mode.as_deref().map(AccessMode::parse).transpose()? {
            None => AccessMode::Everyone,
            // `(restricted, everyone)` cannot be written; the rule reads one as approval.
            Some(AccessMode::Everyone) if visibility == Visibility::Restricted => {
                AccessMode::Approval
            }
            Some(mode) => mode,
        };
        let row_state = row_state.as_deref().map(RequestState::parse).transpose()?;
        let state = if is_usable {
            "approved"
        } else {
            match row_state {
                Some(RequestState::Pending) => "pending",
                Some(RequestState::Denied) => "denied",
                Some(RequestState::Revoked) => "revoked",
                // An approved row is usable, so `is_usable` took the branch above.
                Some(RequestState::Approved) => "approved",
                Some(RequestState::Withdrawn) | None => "none",
            }
        };
        // A row backs the state only when its own state is the one shown.
        let backed = matches!(
            (state, row_state),
            ("pending", Some(RequestState::Pending))
                | ("approved", Some(RequestState::Approved))
                | ("denied", Some(RequestState::Denied))
                | ("revoked", Some(RequestState::Revoked))
        );
        let latest_version = if prerelease.is_empty() {
            format!("{major}.{minor}.{patch}")
        } else {
            format!("{major}.{minor}.{patch}-{prerelease}")
        };
        out.push(BrowseAgent {
            urn,
            display_name: spec.display_name,
            description: spec.description,
            catalog_tier,
            latest_version,
            visibility,
            mode,
            state,
            request_id: request_id.filter(|_| backed),
            decided_at: decided_at.filter(|_| backed),
            reason: reason.filter(|r| backed && !r.is_empty()),
            can_admin,
        });
    }
    Ok(out)
}

/// Whose rows a request list reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessScope {
    /// The caller's own rows (`GET /v1/me/access`); the agent's name is left out where the
    /// rule gives Hidden for the caller now.
    Mine,
    /// Rows of the agents the caller administers (`GET /v1/access-requests`): every row for an
    /// org admin.
    Administered { org_admin: bool },
}

/// One page of access rows, newest first by `(requested_at, request_id)`, rows after `after`
/// (`(requested_at, request_id)` of the last row of the previous page), at most `limit`.
fn list_access_on(
    c: &Connection,
    caller: &str,
    org_admin: bool,
    scope: AccessScope,
    state: Option<RequestState>,
    after: Option<&(i64, String)>,
    limit: usize,
) -> Result<Vec<AccessView>, OrchestratorError> {
    let caller = resolved_owner(caller);
    let who = match scope {
        AccessScope::Mine => "ar.user_id = ?1".to_string(),
        AccessScope::Administered { org_admin } => admin_scope_sql("ar.urn", "?1", org_admin),
    };
    let listable = listable_predicate_sql("ar.urn", "?1");
    let columns = ROW_COLUMNS
        .split(", ")
        .map(|col| format!("ar.{col}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT {columns}, (?6 OR {listable}) FROM agent_access ar
         WHERE {who}
           AND (?2 = '' OR ar.state = ?2)
           AND (?3 IS NULL OR ar.requested_at < ?3 OR (ar.requested_at = ?3 AND ar.request_id < ?4))
         ORDER BY ar.requested_at DESC, ar.request_id DESC
         LIMIT ?5"
    );
    let mut stmt = c.prepare(&sql)?;
    let raw = stmt
        .query_map(
            params![
                caller,
                state.map(RequestState::as_str).unwrap_or(""),
                after.map(|a| a.0),
                after.map(|a| a.1.as_str()).unwrap_or(""),
                limit as i64,
                org_admin,
            ],
            |r| Ok((map_row(r)?, r.get::<_, bool>(9)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut out = Vec::with_capacity(raw.len());
    for (parts, visible) in raw {
        let row = finish_row(parts)?;
        // Administrators see every agent they administer; a reader of their own rows does not
        // learn the name of an agent the rule hides from them now.
        let show_agent = matches!(scope, AccessScope::Administered { .. }) || visible;
        out.push(access_view_on(c, row, show_agent));
    }
    Ok(out)
}

/// An agent admin as listed by `GET …/admins`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdminEntry {
    pub user_id: String,
    pub username: Option<String>,
    pub added_by: String,
    pub added_at: i64,
    pub deactivated: bool,
}

/// The admins of `urn` by `added_at` (then insertion order).
pub(crate) fn list_admins_on(
    c: &Connection,
    urn: &str,
) -> Result<Vec<AdminEntry>, OrchestratorError> {
    let mut stmt = c.prepare(
        "SELECT ad.user_id, u.username, ad.added_by, ad.added_at, u.deactivated_at IS NOT NULL
         FROM agent_admins ad LEFT JOIN users u ON u.user_id = ad.user_id
         WHERE ad.urn = ?1 ORDER BY ad.added_at, ad.rowid",
    )?;
    let rows = stmt
        .query_map(params![urn], |r| {
            Ok(AdminEntry {
                user_id: r.get(0)?,
                username: r.get(1)?,
                added_by: r.get(2)?,
                added_at: r.get(3)?,
                deactivated: r.get(4)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Make `ids` the admins of `urn`: rows not in the set go, new ids come in as added by `by`,
/// kept rows stay as they are. Only a NEW id must be an existing, active user (the same text for
/// an unknown and a deactivated one), so a list read back from `GET` can always be put again.
/// The caller validates the list's shape; every check here runs before the first write.
pub(crate) fn replace_admins_on(
    c: &Connection,
    urn: &str,
    ids: &[String],
    by: &str,
) -> Result<(), OrchestratorError> {
    let current: Vec<String> = list_admins_on(c, urn)?
        .into_iter()
        .map(|a| a.user_id)
        .collect();
    let new: Vec<&String> = ids.iter().filter(|id| !current.contains(id)).collect();
    for id in &new {
        let active: Option<bool> = c
            .query_row(
                "SELECT deactivated_at IS NULL FROM users WHERE user_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        if active != Some(true) {
            return Err(OrchestratorError::InvalidContract(format!(
                "user '{id}' cannot be an agent admin"
            )));
        }
    }
    for gone in current.iter().filter(|id| !ids.contains(id)) {
        c.execute(
            "DELETE FROM agent_admins WHERE urn = ?1 AND user_id = ?2",
            params![urn, gone],
        )?;
    }
    let by = resolved_owner(by);
    let now = unix_timestamp();
    for id in new {
        c.execute(
            "INSERT INTO agent_admins (urn, user_id, added_by, added_at) VALUES (?1, ?2, ?3, ?4)",
            params![urn, id, by, now],
        )?;
    }
    Ok(())
}

/// Replay rows older than this are pruned before every lookup.
pub(crate) const REPLAY_TTL_SECS: i64 = 86_400;

/// The fingerprint of a request: sha256 over length-prefixed fields, so `("ab","c")` and
/// `("a","bc")` differ. Field order counts.
pub(crate) fn replay_fingerprint(parts: &[&str]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Run one access write under an `Idempotency-Key`, in one lock and one transaction:
///
/// 1. prune rows older than 24 h (before the lookup, so an expired key is a fresh request);
/// 2. `precheck(&tx)`: the Hidden and admin rules, so a replay never answers a caller who has
///    since lost the right to ask (what it returns goes to `run`);
/// 3. look the key up under `owner`: the same fingerprint replays the stored status and body,
///    another fingerprint is `Conflict`;
/// 4. otherwise `run(&tx, prepared)` does the state checks, the write and the rendering; on
///    success the answer is stored and the transaction commits, on any error nothing is kept.
///
/// `owner` is `resolved_owner(owner_user_id)`, so no answer crosses users. The caller parses
/// the body first (a 400 is never stored) and builds `fingerprint` with [`replay_fingerprint`].
pub(crate) fn with_replay_at<P>(
    store: &SqliteWorkflowStore,
    now: i64,
    owner: &str,
    key: &str,
    fingerprint: &str,
    precheck: impl FnOnce(&Connection) -> Result<P, OrchestratorError>,
    run: impl FnOnce(&Connection, P) -> Result<(u16, String), OrchestratorError>,
) -> Result<(u16, String), OrchestratorError> {
    store.with_access_connection(|c| {
        let tx = c.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM agent_access_replays WHERE created_at < ?1",
            params![now - REPLAY_TTL_SECS],
        )?;
        let prepared = precheck(&tx)?;
        let stored: Option<(String, i64, String)> = tx
            .query_row(
                "SELECT fingerprint, status, body FROM agent_access_replays
                 WHERE owner_user_id = ?1 AND idempotency_key = ?2",
                params![owner, key],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        if let Some((stored_fingerprint, status, body)) = stored {
            if stored_fingerprint == fingerprint {
                return Ok((status as u16, body));
            }
            return Err(OrchestratorError::Conflict(
                "Idempotency-Key was already used for a different request".to_string(),
            ));
        }
        let (status, body) = run(&tx, prepared)?;
        tx.execute(
            "INSERT INTO agent_access_replays
               (owner_user_id, idempotency_key, fingerprint, status, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![owner, key, fingerprint, i64::from(status), body, now],
        )?;
        tx.commit()?;
        Ok((status, body))
    })
}

/// [`with_replay_at`] at the current time.
pub(crate) fn with_replay<P>(
    store: &SqliteWorkflowStore,
    owner: &str,
    key: &str,
    fingerprint: &str,
    precheck: impl FnOnce(&Connection) -> Result<P, OrchestratorError>,
    run: impl FnOnce(&Connection, P) -> Result<(u16, String), OrchestratorError>,
) -> Result<(u16, String), OrchestratorError> {
    with_replay_at(
        store,
        unix_timestamp(),
        owner,
        key,
        fingerprint,
        precheck,
        run,
    )
}

impl SqliteWorkflowStore {
    pub(crate) fn with_access_connection<T>(
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
        self.with_access_connection(|c| put_policy_on(c, urn, visibility, mode, by))
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
        self.with_access_connection(|c| insert_access_request_on(c, urn, user, message, initial))
    }

    /// The Add / Request of a caller, decided in one lock: the rule, the policy read and the
    /// insert see the same state, so a concurrent switch to restricted cannot leave an approved
    /// row behind. `open` mode inserts `Approved` (decided by the user), anything else `Pending`.
    pub fn request_agent_access(
        &self,
        urn: &str,
        user: &str,
        org_admin: bool,
        row_is_own_draft: bool,
        message: Option<&str>,
    ) -> Result<AccessAttempt, OrchestratorError> {
        self.with_access_connection(|c| {
            request_access_on(c, urn, user, org_admin, row_is_own_draft, message)
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
        self.with_access_connection(|c| decide_on(c, request_id, decision, by, reason))
    }

    /// One page of the browse list, see [`BrowseAgent`]. `after_urn` is the last URN of the
    /// previous page; it need not name an agent the caller can see (the keyset is a plain
    /// comparison, so a hidden or missing URN answers like any other string).
    pub fn browse_agents(
        &self,
        caller: &str,
        org_admin: bool,
        after_urn: Option<&str>,
        limit: usize,
    ) -> Result<Vec<BrowseAgent>, OrchestratorError> {
        self.with_access_connection(|c| {
            browse_agents_on(c, caller, org_admin, after_urn.unwrap_or(""), limit)
        })
    }

    /// One page of access rows of `scope`, see [`list_access_on`].
    pub fn list_access(
        &self,
        caller: &str,
        org_admin: bool,
        scope: AccessScope,
        state: Option<RequestState>,
        after: Option<&(i64, String)>,
        limit: usize,
    ) -> Result<Vec<AccessView>, OrchestratorError> {
        self.with_access_connection(|c| {
            list_access_on(c, caller, org_admin, scope, state, after, limit)
        })
    }

    /// `GET /v1/me`: the URNs the user administers by an explicit `agent_admins` row (never by
    /// `users:admin`) that still have a released row, sorted, and the number of `pending` requests
    /// the user can decide (every one for an org admin).
    pub fn access_me_summary(
        &self,
        user: &str,
        org_admin: bool,
    ) -> Result<(Vec<String>, i64), OrchestratorError> {
        let user = resolved_owner(user);
        self.with_access_connection(|c| {
            let mut stmt = c.prepare(
                "SELECT a.urn FROM agent_admins a
                 WHERE a.user_id = ?1
                   AND EXISTS (SELECT 1 FROM agents ag WHERE ag.urn = a.urn
                               AND ag.channel = 'released')
                 ORDER BY a.urn",
            )?;
            let urns = stmt
                .query_map([user], |r| r.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            let scope = admin_scope_sql("ar.urn", "?1", org_admin);
            let pending: i64 = c.query_row(
                &format!(
                    "SELECT COUNT(*) FROM agent_access ar
                     WHERE ar.state = 'pending' AND ?1 IS NOT NULL AND {scope}"
                ),
                [user],
                |r| r.get(0),
            )?;
            Ok((urns, pending))
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
        self.with_access_connection(|c| revoke_on(c, urn, user, by, reason))
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

/// Phase 1 wave 2: the rule at the admission reads (design/agent-access.md §3, breakdown 1.1).
#[cfg(test)]
mod enforcement_tests {
    use super::*;
    use vam_spec::v1::{AgentSpec, ResourceId, Version};

    const URN: &str = "urn:vam:agent:local.vam.dev/acme/gated";
    const MISSING: &str = "urn:vam:agent:local.vam.dev/acme/no-such-agent";
    const DRAFT_ONLY: &str = "urn:vam:agent:local.vam.dev/acme/draft-only";

    fn spec(urn: &str) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Gated".to_string(),
            description: "enforcement test".to_string(),
            role: "assistant".to_string(),
            instruction_source: Some(
                vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".to_string()),
            ),
            ..Default::default()
        }
    }

    fn v1() -> Version {
        Version {
            major: 1,
            ..Default::default()
        }
    }

    fn draft() -> Version {
        Version {
            major: 1,
            prerelease: "draft".to_string(),
            ..Default::default()
        }
    }

    /// `URN` released by `owner` (an agent admin by A6); `DRAFT_ONLY` is `u1`'s unreleased draft.
    fn store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_catalog_schema().expect("catalog schema");
        for user in ["u1", "u2", "u3", "owner", "adm"] {
            store.create_user(user, user).expect("user");
        }
        store
            .publish_agent_draft_with_owner(&spec(URN), "owner")
            .expect("draft");
        store.release_agent(URN, &v1()).expect("release");
        store
            .publish_agent_draft_with_owner(&spec(DRAFT_ONLY), "u1")
            .expect("draft only");
        assert!(store.is_agent_admin(URN, "owner").unwrap(), "A6 seeding");
        store
    }

    fn restrict(store: &SqliteWorkflowStore, visibility: Visibility, mode: AccessMode) {
        store
            .put_agent_access_policy(URN, visibility, mode, "adm")
            .unwrap();
        store
            .put_agent_access_policy(DRAFT_ONLY, visibility, mode, "adm")
            .unwrap();
    }

    /// The text with the URN replaced, so a hidden agent's answer and a missing URN's compare.
    fn text(err: OrchestratorError, urn: &str) -> String {
        err.to_string().replace(urn, "<urn>")
    }

    #[test]
    fn install_default_ignores_a_draft_but_not_a_released_row() {
        let s = store();
        let c = s.connection.lock().unwrap();
        // `DRAFT_ONLY` has only a draft row: a first install still gets `(org, approval)`.
        assert!(write_install_default_on(&c, DRAFT_ONLY, "adm", 1).unwrap());
        let p = read_policy(&c, DRAFT_ONLY).unwrap().unwrap();
        assert_eq!(
            (p.visibility, p.mode),
            (Visibility::Org, AccessMode::Approval)
        );
        // `URN` has a released row: an update or pre-upgrade install gets nothing.
        assert!(!write_install_default_on(&c, URN, "adm", 1).unwrap());
        assert!(read_policy(&c, URN).unwrap().is_none());
    }

    #[test]
    fn request_agent_access_decides_under_the_current_policy() {
        let s = store();
        let put = |v, m| s.put_agent_access_policy(URN, v, m, "adm").unwrap();
        let rows = |s: &SqliteWorkflowStore| {
            let c = s.connection.lock().unwrap();
            c.query_row("SELECT COUNT(*) FROM agent_access", [], |r| {
                r.get::<_, i64>(0)
            })
            .unwrap()
        };
        let ask = |user: &str| {
            s.request_agent_access(URN, user, false, false, None)
                .unwrap()
        };
        // A switch to restricted (after any earlier read) leaves no row: Hidden, nothing written.
        put(Visibility::Restricted, AccessMode::Open);
        assert!(matches!(ask("u2"), AccessAttempt::Hidden));
        assert_eq!(rows(&s), 0);
        // (org, everyone) is already usable.
        put(Visibility::Org, AccessMode::Everyone);
        assert!(matches!(ask("u2"), AccessAttempt::Usable));
        // (org, approval) is pending; a second ask is refused as pending.
        put(Visibility::Org, AccessMode::Approval);
        assert!(
            matches!(ask("u2"), AccessAttempt::Inserted(r) if r.state == RequestState::Pending)
        );
        assert!(matches!(ask("u2"), AccessAttempt::AlreadyPending));
        // (org, open) is approved at once.
        put(Visibility::Org, AccessMode::Open);
        assert!(
            matches!(ask("u3"), AccessAttempt::Inserted(r) if r.state == RequestState::Approved)
        );
        assert_eq!(rows(&s), 2);
    }

    #[test]
    fn hidden_answers_are_the_missing_urn_text_byte_for_byte() {
        let s = store();
        restrict(&s, Visibility::Restricted, AccessMode::Approval);
        let caller = "u2";
        let get = |urn: &str| {
            text(
                s.get_agent_record_visible(urn, caller, false).unwrap_err(),
                urn,
            )
        };
        assert_eq!(get(URN), get(MISSING));
        assert_eq!(get(URN), "not found: agents '<urn>' not found in catalog");
        let spec_get = |urn: &str| text(s.get_agent_visible(urn, caller, false).unwrap_err(), urn);
        assert_eq!(spec_get(URN), spec_get(MISSING));
        let at = |urn: &str, v: &Version| {
            text(
                s.get_agent_at_version_visible(urn, v, caller, false)
                    .unwrap_err(),
                urn,
            )
        };
        assert_eq!(at(URN, &v1()), at(MISSING, &v1()));
        assert!(at(URN, &v1()).contains("1.0.0 not found in catalog"));
        assert_eq!(at(URN, &draft()), at(MISSING, &draft()));
        let versions = |urn: &str| {
            text(
                s.list_catalog_versions("agents", urn, caller, false)
                    .unwrap_err(),
                urn,
            )
        };
        assert_eq!(versions(URN), versions(MISSING));
        assert!(versions(URN).contains("has no versions visible to this caller"));
        // The list leaves it out.
        let listed = s.list_agent_records_visible(caller, false).unwrap();
        assert!(listed
            .iter()
            .all(|r| r.spec.as_ref().unwrap().resource_id.as_ref().unwrap().urn != URN));
    }

    #[test]
    fn visible_answers_agent_access_required_and_is_not_listed() {
        let s = store();
        restrict(&s, Visibility::Org, AccessMode::Approval);
        let caller = "u2";
        let required = |e: OrchestratorError| {
            assert!(
                matches!(&e, OrchestratorError::AgentAccessRequired { urn } if urn == URN),
                "{e:?}"
            );
            assert_eq!(
                e.to_string(),
                format!("agent_access_required: request access to '{URN}'")
            );
        };
        required(s.get_agent_record_visible(URN, caller, false).unwrap_err());
        required(s.get_agent_visible(URN, caller, false).unwrap_err());
        required(
            s.get_agent_at_version_visible(URN, &v1(), caller, false)
                .unwrap_err(),
        );
        required(
            s.list_catalog_versions("agents", URN, caller, false)
                .unwrap_err(),
        );
        let listed = s.list_agent_records_visible(caller, false).unwrap();
        assert!(listed
            .iter()
            .all(|r| r.spec.as_ref().unwrap().resource_id.as_ref().unwrap().urn != URN));
        // A pending request does not change that.
        s.insert_access_request(URN, caller, None, RequestState::Pending)
            .unwrap();
        required(s.get_agent_visible(URN, caller, false).unwrap_err());
    }

    #[test]
    fn usable_callers_read_and_list_the_agent() {
        let s = store();
        restrict(&s, Visibility::Restricted, AccessMode::Approval);
        s.insert_access_request(URN, "u3", None, RequestState::Approved)
            .unwrap();
        let listed_by = |user: &str, admin: bool| -> Vec<String> {
            s.list_agent_records_visible(user, admin)
                .unwrap()
                .into_iter()
                .map(|r| r.spec.unwrap().resource_id.unwrap().urn)
                .collect()
        };
        // The agent admin (first releaser), an approved user and an org admin: usable.
        for (user, org_admin) in [("owner", false), ("u3", false), ("adm", true)] {
            s.get_agent_visible(URN, user, org_admin).expect(user);
            s.get_agent_at_version_visible(URN, &v1(), user, org_admin)
                .expect(user);
            s.list_catalog_versions("agents", URN, user, org_admin)
                .expect(user);
            assert!(
                listed_by(user, org_admin).iter().any(|u| u == URN),
                "{user}"
            );
        }
        assert!(!listed_by("u2", false).iter().any(|u| u == URN));
        // Opening the agent to everyone makes it usable for all again.
        s.put_agent_access_policy(URN, Visibility::Org, AccessMode::Everyone, "adm")
            .unwrap();
        s.get_agent_visible(URN, "u2", false).expect("everyone");
        assert!(listed_by("u2", false).iter().any(|u| u == URN));
    }

    #[test]
    fn an_owners_own_draft_stays_usable_and_a_foreign_draft_stays_not_found() {
        let s = store();
        restrict(&s, Visibility::Restricted, AccessMode::Approval);
        // u1 owns the unreleased draft.
        s.get_agent_visible(DRAFT_ONLY, "u1", false)
            .expect("own draft");
        s.get_agent_at_version_visible(DRAFT_ONLY, &draft(), "u1", false)
            .expect("own draft pin");
        s.list_catalog_versions("agents", DRAFT_ONLY, "u1", false)
            .expect("own draft versions");
        assert!(s
            .list_agent_records_visible("u1", false)
            .unwrap()
            .iter()
            .any(|r| r.spec.as_ref().unwrap().resource_id.as_ref().unwrap().urn == DRAFT_ONLY));
        // Another user gets the draft text, the same as for a draft that does not exist.
        let foreign = s
            .get_agent_at_version_visible(DRAFT_ONLY, &draft(), "u2", false)
            .unwrap_err();
        assert_eq!(
            text(foreign, DRAFT_ONLY),
            "not found: no draft of agent '<urn>' for this user"
        );
        assert!(matches!(
            s.get_agent_visible(DRAFT_ONLY, "u2", false).unwrap_err(),
            OrchestratorError::NotFound(_)
        ));
    }

    #[test]
    fn an_admitted_session_still_loads_its_agent_after_a_revoke() {
        let s = store();
        restrict(&s, Visibility::Org, AccessMode::Approval);
        let row = s
            .insert_access_request(URN, "u2", None, RequestState::Pending)
            .unwrap();
        s.decide_access_request(&row.request_id, Decision::Approve, "owner", None)
            .unwrap();
        s.get_agent_visible(URN, "u2", false).expect("approved");
        s.revoke_access(URN, "u2", "owner", Some("left the team"))
            .unwrap();
        // A new admission is refused ...
        assert!(matches!(
            s.get_agent_visible(URN, "u2", false).unwrap_err(),
            OrchestratorError::AgentAccessRequired { .. }
        ));
        // ... the admitted twins (boot, resume, per-tool-call policy read) are not.
        s.get_agent_admitted(URN, "u2", false).expect("admitted");
        s.get_agent_record_admitted(URN, "u2", false)
            .expect("record admitted");
        s.get_agent_at_version_admitted(URN, &v1(), "u2", false)
            .expect("at version admitted");
        let run = vam_spec::v1::RunSpec::default();
        crate::catalog_resolve::load_admitted_agent(&s, &run, URN, "u2", false)
            .expect("load admitted");
        assert!(crate::catalog_resolve::load_agent_for_run(&s, &run, URN, "u2", false).is_err());
    }

    #[test]
    fn existing_agents_with_no_policy_row_behave_as_before() {
        let s = store();
        s.get_agent_visible(URN, "u2", false)
            .expect("no policy row");
        s.get_agent_at_version_visible(URN, &v1(), "u2", false)
            .expect("at version");
        s.list_catalog_versions("agents", URN, "u2", false)
            .expect("versions");
        assert!(s
            .list_agent_records_visible("u2", false)
            .unwrap()
            .iter()
            .any(|r| r.spec.as_ref().unwrap().resource_id.as_ref().unwrap().urn == URN));
    }

    #[test]
    fn the_403_body_is_permission_denied_with_the_marker_message_and_not_retryable() {
        let api: crate::api::error::ApiError = OrchestratorError::AgentAccessRequired {
            urn: URN.to_string(),
        }
        .into();
        assert_eq!(api.status, axum::http::StatusCode::FORBIDDEN);
        assert_eq!(api.code, vam_spec::v1::VamErrorCode::PermissionDenied);
        assert_eq!(
            api.message,
            format!("agent_access_required: request access to '{URN}'")
        );
        assert!(!api.retryable);
    }
}

/// Phase 2 wave 1: browse, lists, admins, withdraw and replay at the store (design §6 amendments).
#[cfg(test)]
mod phase2_tests {
    use super::*;
    use vam_spec::v1::{AgentSpec, ResourceId, Version};

    fn urn(n: &str) -> String {
        format!("urn:vam:agent:local.vam.dev/acme/{n}")
    }

    fn spec(urn: &str) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: format!("Name of {urn}"),
            description: "phase 2".to_string(),
            role: "assistant".to_string(),
            instruction_source: Some(
                vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".to_string()),
            ),
            ..Default::default()
        }
    }

    /// Released agents `names` (released by `owner`, so `owner` administers each), users u1..u3,
    /// `adm` and `gone` (deactivated).
    fn store_with(names: &[&str]) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_catalog_schema().expect("catalog schema");
        for user in ["u1", "u2", "u3", "owner", "adm", "gone"] {
            store.create_user(user, user).expect("user");
        }
        for name in names {
            let urn = urn(name);
            store
                .publish_agent_draft_with_owner(&spec(&urn), "owner")
                .expect("draft");
            store
                .release_agent(
                    &urn,
                    &Version {
                        major: 1,
                        ..Default::default()
                    },
                )
                .expect("release");
        }
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE users SET deactivated_at = 1 WHERE user_id = 'gone'",
                [],
            )
            .unwrap();
        store
    }

    fn policy(store: &SqliteWorkflowStore, name: &str, v: Visibility, m: AccessMode) {
        store
            .put_agent_access_policy(&urn(name), v, m, "adm")
            .unwrap();
    }

    fn browse(store: &SqliteWorkflowStore, user: &str, org_admin: bool) -> Vec<BrowseAgent> {
        store.browse_agents(user, org_admin, None, 100).unwrap()
    }

    fn names(rows: &[BrowseAgent]) -> Vec<String> {
        rows.iter()
            .map(|r| r.urn.rsplit('/').next().unwrap().to_string())
            .collect()
    }

    #[test]
    fn listable_predicate_matches_the_rule_for_every_policy_and_caller() {
        let store = store_with(&["l0", "l1", "l2", "l3", "l4", "l5", "l6"]);
        use AccessMode::*;
        use Visibility::*;
        // l0 no policy; l1 org/approval; l2 restricted/approval + u1 pending;
        // l3 restricted/approval + u1 approved; l4 restricted/open + u1 admin; l5 restricted/
        // approval, no relation; l6 restricted/approval + u1 revoked.
        policy(&store, "l1", Org, Approval);
        policy(&store, "l2", Restricted, Approval);
        policy(&store, "l3", Restricted, Approval);
        policy(&store, "l4", Restricted, Open);
        policy(&store, "l5", Restricted, Approval);
        policy(&store, "l6", Restricted, Approval);
        store
            .insert_access_request(&urn("l2"), "u1", None, RequestState::Pending)
            .unwrap();
        store
            .insert_access_request(&urn("l3"), "u1", None, RequestState::Approved)
            .unwrap();
        store.seed_first_admin(&urn("l4"), "u1").unwrap();
        store
            .insert_access_request(&urn("l6"), "u1", None, RequestState::Approved)
            .unwrap();
        store
            .revoke_access(&urn("l6"), "u1", "adm", Some("no"))
            .unwrap();

        let sql = format!(
            "SELECT COUNT(*) FROM (SELECT ?1 AS urn) t WHERE {}",
            listable_predicate_sql("t.urn", "?2")
        );
        for name in ["l0", "l1", "l2", "l3", "l4", "l5", "l6"] {
            for user in ["u1", "u2"] {
                let u = urn(name);
                let by_rule =
                    agent_access(&store, &u, user, false, false).unwrap() != AgentAccess::Hidden;
                let c = store.connection.lock().unwrap();
                let by_sql: i64 = c.query_row(&sql, params![u, user], |r| r.get(0)).unwrap();
                assert_eq!(by_sql == 1, by_rule, "{name} as {user}");
            }
        }
        // The browse list is that predicate plus the org-admin branch.
        assert_eq!(
            names(&browse(&store, "u1", false)),
            ["l0", "l1", "l3", "l4"]
        );
        assert_eq!(names(&browse(&store, "u2", false)), ["l0", "l1"]);
        assert_eq!(
            names(&browse(&store, "u2", true)),
            ["l0", "l1", "l2", "l3", "l4", "l5", "l6"],
            "an org admin lists every agent"
        );
        assert!(browse(&store, "u2", true).iter().all(|r| r.can_admin));
        assert!(browse(&store, "u1", false)
            .iter()
            .all(|r| r.can_admin == (r.urn == urn("l4"))));
    }

    #[test]
    fn browse_state_follows_the_latest_row_and_a_tie_prefers_the_live_row() {
        let store = store_with(&["a"]);
        policy(&store, "a", Visibility::Org, AccessMode::Approval);
        let a = urn("a");
        let state = |user: &str| {
            let row = browse(&store, user, false).remove(0);
            (row.state, row.request_id, row.decided_at, row.reason)
        };
        assert_eq!(state("u1"), ("none", None, None, None));
        let pending = store
            .insert_access_request(&a, "u1", Some("hi"), RequestState::Pending)
            .unwrap();
        assert_eq!(state("u1").0, "pending");
        assert_eq!(
            state("u1").1.as_deref(),
            Some(pending.request_id.as_str()),
            "a row backs pending"
        );
        let denied = store
            .decide_access_request(&pending.request_id, Decision::Deny, "adm", Some("not now"))
            .unwrap();
        let s = state("u1");
        assert_eq!(s.0, "denied");
        assert_eq!(s.1.as_deref(), Some(denied.request_id.as_str()));
        assert_eq!(s.3.as_deref(), Some("not now"));
        // Withdraw then request again inside the same second: the live row wins over the
        // withdrawn one however the timestamps tie.
        let again = store
            .insert_access_request(&a, "u1", None, RequestState::Pending)
            .unwrap();
        store
            .with_access_connection(|c| withdraw_on(c, &a, "u1", false))
            .unwrap();
        assert_eq!(state("u1").0, "none", "withdrawn reads as none");
        assert_eq!(state("u1").1, None, "no requestId for none");
        let third = store
            .insert_access_request(&a, "u1", None, RequestState::Pending)
            .unwrap();
        assert_eq!(state("u1").1.as_deref(), Some(third.request_id.as_str()));
        assert_ne!(again.request_id, third.request_id);
        // Approved row: usable, backed by the row; revoked: shown with its reason.
        store
            .decide_access_request(&third.request_id, Decision::Approve, "adm", None)
            .unwrap();
        let s = state("u1");
        assert_eq!(s.0, "approved");
        assert_eq!(s.1.as_deref(), Some(third.request_id.as_str()));
        store.revoke_access(&a, "u1", "adm", Some("left")).unwrap();
        let s = state("u1");
        assert_eq!(s.0, "revoked");
        assert_eq!(s.3.as_deref(), Some("left"));
        // Usable for another reason (everyone): approved, but no row backs it.
        policy(&store, "a", Visibility::Org, AccessMode::Everyone);
        assert_eq!(state("u1"), ("approved", None, None, None));
        // Another user is unaffected.
        assert_eq!(state("u2"), ("approved", None, None, None));
    }

    #[test]
    fn browse_pages_by_urn_and_a_hidden_or_missing_urn_cursor_answers_the_same() {
        // a < hidden < b by URN.
        let store = store_with(&["a", "m-hidden", "z"]);
        policy(
            &store,
            "m-hidden",
            Visibility::Restricted,
            AccessMode::Approval,
        );
        let all = names(&browse(&store, "u1", false));
        assert_eq!(all, ["a", "z"]);
        let page = |after: Option<&str>, limit| {
            names(&store.browse_agents("u1", false, after, limit).unwrap())
        };
        assert_eq!(page(None, 1), ["a"]);
        assert_eq!(page(Some(&urn("a")), 1), ["z"]);
        let from_hidden = page(Some(&urn("m-hidden")), 100);
        let from_missing = page(Some(&urn("m-no-such-agent")), 100);
        assert_eq!(from_hidden, ["z"]);
        assert_eq!(
            from_hidden, from_missing,
            "a hidden URN token reads as a missing one"
        );
        assert_eq!(
            from_hidden,
            page(Some(&urn("a")), 100),
            "same rows as from the previous visible URN"
        );
    }

    #[test]
    fn browse_lists_the_latest_released_version_once() {
        let store = store_with(&["a"]);
        let a = urn("a");
        let mut changed = spec(&a);
        changed.description = "changed".to_string();
        store
            .publish_agent_draft_with_owner(&changed, "owner")
            .unwrap();
        store
            .release_agent(
                &a,
                &Version {
                    major: 1,
                    minor: 2,
                    patch: 3,
                    ..Default::default()
                },
            )
            .unwrap();
        let rows = browse(&store, "u1", false);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].latest_version, "1.2.3");
        assert_eq!(rows[0].display_name, format!("Name of {a}"));
        assert_eq!(rows[0].visibility, Visibility::Org);
        assert_eq!(rows[0].mode, AccessMode::Everyone);
    }

    #[test]
    fn withdraw_ends_the_callers_own_live_row_even_after_the_agent_turned_restricted() {
        let store = store_with(&["a"]);
        policy(&store, "a", Visibility::Org, AccessMode::Approval);
        let a = urn("a");
        let row = store
            .insert_access_request(&a, "u1", None, RequestState::Pending)
            .unwrap();
        // Now restricted: u1 can no longer see the agent, but the pending row is still theirs.
        policy(&store, "a", Visibility::Restricted, AccessMode::Approval);
        let ended = store
            .with_access_connection(|c| withdraw_on(c, &a, "u1", false))
            .unwrap();
        assert_eq!(ended.request_id, row.request_id);
        assert_eq!(
            store
                .get_access_request(&row.request_id)
                .unwrap()
                .unwrap()
                .state,
            RequestState::Withdrawn
        );
        assert_eq!(
            store
                .get_access_request(&row.request_id)
                .unwrap()
                .unwrap()
                .decided_by
                .as_deref(),
            Some("u1")
        );
        // No row now: Hidden answers the missing-URN text, a missing URN the same, Visible the
        // route's own text.
        let hidden = store
            .with_access_connection(|c| withdraw_on(c, &a, "u1", false))
            .unwrap_err();
        let missing = store
            .with_access_connection(|c| withdraw_on(c, &urn("no-such"), "u1", false))
            .unwrap_err();
        assert_eq!(
            hidden.to_string().replace(&a, "<urn>"),
            missing.to_string().replace(&urn("no-such"), "<urn>")
        );
        assert!(matches!(hidden, OrchestratorError::NotFound(_)));
        policy(&store, "a", Visibility::Org, AccessMode::Approval);
        let visible = store
            .with_access_connection(|c| withdraw_on(c, &a, "u1", false))
            .unwrap_err();
        assert!(
            visible
                .to_string()
                .contains(&format!("you have no access request for agent '{a}'")),
            "{visible}"
        );
    }

    #[test]
    fn put_admins_checks_only_new_ids_and_keeps_deactivated_ones() {
        let store = store_with(&["a"]);
        let a = urn("a");
        let set = |ids: &[&str]| {
            let ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
            store.with_access_connection(|c| replace_admins_on(c, &a, &ids, "adm"))
        };
        let admins = || {
            store
                .with_access_connection(|c| list_admins_on(c, &a))
                .unwrap()
        };
        // `owner` is seeded by the release (A6).
        assert_eq!(admins().len(), 1);
        // Unknown and deactivated read the same.
        let unknown = set(&["owner", "nobody"]).unwrap_err().to_string();
        let gone = set(&["owner", "gone"]).unwrap_err().to_string();
        assert_eq!(
            unknown.replace("nobody", "<id>"),
            gone.replace("gone", "<id>")
        );
        assert!(unknown.contains("cannot be an agent admin"), "{unknown}");
        assert_eq!(admins().len(), 1, "a refused list changes nothing");
        // Replace: u1 in, owner out, added_by is the caller.
        set(&["u1", "u2"]).unwrap();
        let rows = admins();
        assert_eq!(
            rows.iter()
                .map(|r| r.user_id.as_str())
                .collect::<Vec<_>>()
                .len(),
            2
        );
        assert!(rows.iter().all(|r| r.added_by == "adm"));
        // A kept admin who was deactivated since: the round trip still works.
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE users SET deactivated_at = 1 WHERE user_id = 'u2'",
                [],
            )
            .unwrap();
        assert!(admins().iter().any(|r| r.user_id == "u2" && r.deactivated));
        set(&["u1", "u2"]).expect("kept deactivated id is fine");
        set(&["u2"]).expect("kept deactivated id alone");
        // Empty list: no explicit admins, and the rule still lets an org admin through.
        set(&[]).unwrap();
        assert!(admins().is_empty());
        assert!(is_agent_or_org_admin(&store, &a, "u3", true).unwrap());
        assert!(!is_agent_or_org_admin(&store, &a, "u3", false).unwrap());
    }

    #[test]
    fn lists_filter_by_scope_state_and_hide_the_name_of_a_hidden_agent() {
        let store = store_with(&["a", "b"]);
        policy(&store, "a", Visibility::Org, AccessMode::Approval);
        policy(&store, "b", Visibility::Org, AccessMode::Approval);
        let (a, b) = (urn("a"), urn("b"));
        store
            .insert_access_request(&a, "u1", None, RequestState::Pending)
            .unwrap();
        store
            .insert_access_request(&b, "u1", None, RequestState::Pending)
            .unwrap();
        store
            .insert_access_request(&b, "u2", None, RequestState::Pending)
            .unwrap();
        let mine = store
            .list_access("u1", false, AccessScope::Mine, None, None, 10)
            .unwrap();
        assert_eq!(mine.len(), 2);
        assert!(mine.iter().all(|v| v.agent_display_name.is_some()));
        // `b` turns restricted: u1's own row stays listed, the name goes.
        policy(&store, "b", Visibility::Restricted, AccessMode::Approval);
        let mine = store
            .list_access("u1", false, AccessScope::Mine, None, None, 10)
            .unwrap();
        assert_eq!(mine.len(), 2);
        for v in &mine {
            assert_eq!(
                v.agent_display_name.is_some(),
                v.row.urn == a,
                "{}",
                v.row.urn
            );
        }
        // Administered scope: owner administers a and b (released both), u1 administers nothing.
        let queue = |user: &str, org: bool| {
            store
                .list_access(
                    user,
                    org,
                    AccessScope::Administered { org_admin: org },
                    Some(RequestState::Pending),
                    None,
                    10,
                )
                .unwrap()
        };
        assert_eq!(queue("owner", false).len(), 3);
        assert!(queue("owner", false)
            .iter()
            .all(|v| v.agent_display_name.is_some()));
        assert!(queue("u1", false).is_empty());
        assert_eq!(queue("adm", true).len(), 3);
        // Paging: newest first with a stable keyset, no row twice.
        let first = store
            .list_access(
                "owner",
                false,
                AccessScope::Administered { org_admin: false },
                None,
                None,
                2,
            )
            .unwrap();
        let last = first.last().unwrap();
        let cursor = (last.row.requested_at, last.row.request_id.clone());
        let rest = store
            .list_access(
                "owner",
                false,
                AccessScope::Administered { org_admin: false },
                None,
                Some(&cursor),
                2,
            )
            .unwrap();
        let mut ids: Vec<_> = first
            .iter()
            .chain(&rest)
            .map(|v| v.row.request_id.clone())
            .collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), 3);
    }

    fn replay_store() -> SqliteWorkflowStore {
        store_with(&[])
    }

    fn replay(
        store: &SqliteWorkflowStore,
        now: i64,
        owner: &str,
        key: &str,
        fp: &str,
        runs: &std::cell::Cell<u32>,
    ) -> Result<(u16, String), OrchestratorError> {
        with_replay_at(
            store,
            now,
            owner,
            key,
            fp,
            |_| Ok(()),
            |_, ()| {
                runs.set(runs.get() + 1);
                Ok((202, format!("run {}", runs.get())))
            },
        )
    }

    #[test]
    fn replay_returns_the_first_answer_and_runs_once() {
        let store = replay_store();
        let runs = std::cell::Cell::new(0);
        let fp = replay_fingerprint(&["POST", "/x", "a"]);
        let first = replay(&store, 1_000, "u1", "k", &fp, &runs).unwrap();
        let second = replay(&store, 1_001, "u1", "k", &fp, &runs).unwrap();
        assert_eq!(first, (202, "run 1".to_string()));
        assert_eq!(second, first);
        assert_eq!(runs.get(), 1);
        // Another request under the key is a 409.
        let other = replay_fingerprint(&["POST", "/x", "b"]);
        let err = replay(&store, 1_002, "u1", "k", &other, &runs).unwrap_err();
        assert!(matches!(err, OrchestratorError::Conflict(m) if m.contains("different request")));
        assert_eq!(runs.get(), 1);
        // The same key for another user takes the normal path.
        let b = replay(&store, 1_003, "u2", "k", &fp, &runs).unwrap();
        assert_eq!(b, (202, "run 2".to_string()));
    }

    #[test]
    fn a_failed_call_stores_nothing_and_its_writes_roll_back() {
        let store = replay_store();
        let fp = replay_fingerprint(&["POST", "/x", "a"]);
        let err = with_replay_at(
            &store,
            10,
            "u1",
            "k",
            &fp,
            |_| Ok(()),
            |c, ()| {
                c.execute(
                    "INSERT INTO agent_access_policy VALUES ('urn:x','org','open','u1',1)",
                    [],
                )?;
                Err::<(u16, String), _>(OrchestratorError::Conflict("no".into()))
            },
        )
        .unwrap_err();
        assert!(matches!(err, OrchestratorError::Conflict(_)));
        let c = store.connection.lock().unwrap();
        for table in ["agent_access_replays", "agent_access_policy"] {
            let n: i64 = c
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0, "{table}");
        }
    }

    #[test]
    fn a_failing_precheck_beats_a_stored_replay() {
        let store = replay_store();
        let runs = std::cell::Cell::new(0);
        let fp = replay_fingerprint(&["POST", "/x", "a"]);
        replay(&store, 10, "u1", "k", &fp, &runs).unwrap();
        let err = with_replay_at(
            &store,
            11,
            "u1",
            "k",
            &fp,
            |_| Err::<(), _>(OrchestratorError::PermissionDenied("lost admin".into())),
            |_, ()| Ok((200, String::new())),
        )
        .unwrap_err();
        assert!(matches!(err, OrchestratorError::PermissionDenied(_)));
    }

    #[test]
    fn replay_rows_expire_after_24_hours_with_an_injected_clock() {
        let store = replay_store();
        let runs = std::cell::Cell::new(0);
        let fp = replay_fingerprint(&["POST", "/x", "a"]);
        let first = replay(&store, 1_000, "u1", "k", &fp, &runs).unwrap();
        // Exactly 24 h later the row still replays.
        let at_ttl = replay(&store, 1_000 + REPLAY_TTL_SECS, "u1", "k", &fp, &runs).unwrap();
        assert_eq!(at_ttl, first);
        assert_eq!(runs.get(), 1);
        // One second more: pruned before the lookup, so the key is a fresh request (no primary
        // key clash, no 500), and a different body under it is no longer a 409.
        let other = replay_fingerprint(&["POST", "/x", "b"]);
        let fresh = replay(
            &store,
            1_000 + REPLAY_TTL_SECS + 1,
            "u1",
            "k",
            &other,
            &runs,
        )
        .unwrap();
        assert_eq!(fresh, (202, "run 2".to_string()));
        assert_eq!(runs.get(), 2);
    }

    #[test]
    fn fingerprints_are_length_prefixed_and_ordered() {
        assert_ne!(
            replay_fingerprint(&["ab", "c"]),
            replay_fingerprint(&["a", "bc"])
        );
        assert_ne!(
            replay_fingerprint(&["PUT", "u", "x", "y"]),
            replay_fingerprint(&["PUT", "u", "y", "x"]),
            "userIds order counts"
        );
        assert_eq!(replay_fingerprint(&["a"]), replay_fingerprint(&["a"]));
    }
}

#[cfg(test)]
mod notification_tests {
    use super::*;
    use crate::notifications::{self, Acted, Kind, ListFilter};
    use crate::users::UserRole;
    use vam_spec::v1::{AgentSpec, ResourceId, Version};

    const URN: &str = "urn:vam:agent:local.vam.dev/acme/notified";

    /// `URN` released by `owner` (its admin by A6); users u2, u3, `oa` (org_admin), `gone`.
    fn store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_catalog_schema().expect("catalog schema");
        for user in ["u2", "u3", "owner", "gone"] {
            store.create_user(user, user).expect("user");
        }
        store
            .create_user_with_role("oa", "oa", UserRole::OrgAdmin)
            .expect("org admin");
        let spec = AgentSpec {
            resource_id: Some(ResourceId { urn: URN.into() }),
            display_name: "Notified".into(),
            role: "assistant".into(),
            instruction_source: Some(
                vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".into()),
            ),
            ..Default::default()
        };
        store
            .publish_agent_draft_with_owner(&spec, "owner")
            .expect("draft");
        store
            .release_agent(
                URN,
                &Version {
                    major: 1,
                    ..Default::default()
                },
            )
            .expect("release");
        store
    }

    fn open(store: &SqliteWorkflowStore, user: &str, kind: &str) -> Vec<notifications::Row> {
        let filter = ListFilter {
            open_only: true,
            kind: Some(kind.to_string()),
            ..Default::default()
        };
        store
            .list_notifications(user, &filter, None, 50)
            .expect("list")
    }

    fn raise(store: &SqliteWorkflowStore, message: &str) -> AccessRow {
        store
            .with_access_connection(|c| {
                let AccessAttempt::Inserted(row) =
                    request_access_on(c, URN, "u2", false, false, Some(message))?
                else {
                    panic!("not inserted")
                };
                crate::notifications::record_access_requested(c, &row.request_id, URN, "u2")?;
                Ok(row)
            })
            .expect("raise")
    }

    fn approval_agent(store: &SqliteWorkflowStore) {
        store
            .put_agent_access_policy(URN, Visibility::Org, AccessMode::Approval, "oa")
            .unwrap();
    }

    #[test]
    fn a_request_reaches_the_agents_admins_only_with_a_fixed_text() {
        let s = store();
        approval_agent(&s);
        let row = raise(&s, "secret-words-unit");
        let admin = open(&s, "owner", "access.requested");
        assert_eq!(admin.len(), 1);
        let n = &admin[0];
        assert_eq!(n.title, "u2 asks to use Notified");
        assert_eq!(n.severity, "action");
        assert_eq!(n.agent_urn, URN);
        let json = n.to_json().to_string();
        assert!(!json.contains("secret-words-unit"), "{json}");
        assert!(json.contains(&row.request_id));
        assert!(json.contains("POST /v1/access-requests/"));
        // The agent has an admin, so org admins, members and the requester hear nothing.
        for user in ["oa", "u2", "u3", "gone", crate::users::OPERATOR_USER_ID] {
            assert!(open(&s, user, "access.requested").is_empty(), "{user}");
        }
    }

    #[test]
    fn with_no_active_admin_the_org_admins_and_the_operator_are_told() {
        let s = store();
        approval_agent(&s);
        s.deactivate_user("owner").unwrap();
        raise(&s, "hi");
        assert_eq!(open(&s, "oa", "access.requested").len(), 1);
        assert_eq!(
            open(&s, crate::users::OPERATOR_USER_ID, "access.requested").len(),
            1
        );
        for user in ["owner", "u2", "u3", "gone"] {
            assert!(open(&s, user, "access.requested").is_empty(), "{user}");
        }
    }

    #[test]
    fn a_decide_closes_acted_a_withdraw_closes_withdrawn_and_each_notifies_correctly() {
        let s = store();
        approval_agent(&s);
        let first = raise(&s, "a");
        s.with_access_connection(|c| {
            let decided = decide_on(c, &first.request_id, Decision::Deny, "owner", Some("no"))?;
            notifications::resolve_access_requested(
                c,
                &decided.request_id,
                "acted",
                Some(Acted {
                    surface: "api",
                    by_user_id: "owner",
                }),
            )?;
            notifications::record_access_decided(c, &decided, "denied")?;
            Ok(())
        })
        .unwrap();
        assert!(open(&s, "owner", "access.requested").is_empty());
        let all = s
            .list_notifications(
                "owner",
                &ListFilter {
                    kind: Some("access.requested".into()),
                    ..Default::default()
                },
                None,
                10,
            )
            .unwrap();
        assert_eq!(all[0].resolved_reason.as_deref(), Some("acted"));
        assert_eq!(all[0].acted_by_user_id.as_deref(), Some("owner"));
        let told = open(&s, "u2", "access.decided");
        assert_eq!(told.len(), 1);
        assert_eq!(told[0].title, "Your access to Notified was denied");
        assert_eq!(told[0].severity, "info");

        // A new request, withdrawn by the member: closed `withdrawn`, with no actor.
        let second = raise(&s, "b");
        assert_eq!(open(&s, "owner", "access.requested").len(), 1);
        s.with_access_connection(|c| {
            let row = withdraw_on(c, URN, "u2", false)?;
            assert_eq!(row.request_id, second.request_id);
            notifications::resolve_access_requested(c, &row.request_id, "withdrawn", None)?;
            Ok(())
        })
        .unwrap();
        let all = s
            .list_notifications(
                "owner",
                &ListFilter {
                    kind: Some("access.requested".into()),
                    ..Default::default()
                },
                None,
                10,
            )
            .unwrap();
        let w = all
            .iter()
            .find(|r| r.resolved_reason.as_deref() == Some("withdrawn"))
            .expect("withdrawn row");
        assert!(w.acted_at.is_none());
    }

    #[test]
    fn a_replayed_request_notifies_once() {
        let s = store();
        approval_agent(&s);
        let run = || {
            with_replay_at(
                &s,
                1_000,
                "u2",
                "k",
                "fp",
                |_| Ok(()),
                |c, ()| {
                    let AccessAttempt::Inserted(row) =
                        request_access_on(c, URN, "u2", false, false, None)?
                    else {
                        panic!("not inserted")
                    };
                    notifications::record_access_requested(c, &row.request_id, URN, "u2")?;
                    Ok((202, row.request_id))
                },
            )
            .unwrap()
        };
        let first = run();
        let second = run();
        assert_eq!(first, second);
        let rows = open(&s, "owner", "access.requested");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].count, 1);
    }

    #[test]
    fn me_summary_counts_explicit_admin_rows_and_pending_requests() {
        let s = store();
        approval_agent(&s);
        assert_eq!(
            s.access_me_summary("owner", false).unwrap(),
            (vec![URN.to_string()], 0)
        );
        raise(&s, "x");
        assert_eq!(s.access_me_summary("owner", false).unwrap().1, 1);
        // An org admin decides every request but administers no agent by an explicit row.
        assert_eq!(s.access_me_summary("oa", true).unwrap(), (vec![], 1));
        assert_eq!(s.access_me_summary("u3", false).unwrap(), (vec![], 0));
        // Kinds are closed: the new two parse.
        assert_eq!(Kind::parse("access.decided"), Some(Kind::AccessDecided));
    }
}
