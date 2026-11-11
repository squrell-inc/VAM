//! Spaces and threads: ids are the identity, names are labels (locked decision 10).
//!
//! A Space is stored in Drive9 under its id alone (`sp_…`, no owner or agent prefix); a thread
//! lives under `.vam/threads/th_…`. The owner and the bound agent are columns of the `spaces`
//! row. Labels are plain columns: never part of a key, a directory or a lookup, so renaming one
//! is a row update. Threads are keyed by `(space_id, thread_id)`: a `th_…` id means something
//! only inside its Space.
//!
//! Every read of a Space by id goes through [`SqliteWorkflowStore::get_space`], which takes the
//! owner and answers "not found" for a foreign id exactly as for a missing one.

use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension, Row};

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

pub const SPACE_PREFIX: &str = "sp_";
pub const THREAD_PREFIX: &str = "th_";
/// Label of a thread the caller did not name.
pub const UNTITLED: &str = "untitled";
/// Label of a Space the caller did not name.
pub const DEFAULT_SPACE_LABEL: &str = "assistant";
/// Longest label, in characters.
pub const MAX_LABEL_CHARS: usize = 200;

const BASE32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
/// 16 random bytes encode to 26 base32 characters (128 bits).
const ID_BYTES: usize = 16;

/// Mints `prefix` + 26 base32 characters from the OS CSPRNG (`OsRng` panics rather than
/// returning weak bytes, the right failure for an identity).
pub fn mint_id(prefix: &str) -> String {
    let mut bytes = [0u8; ID_BYTES];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    let mut id = String::with_capacity(prefix.len() + 26);
    id.push_str(prefix);
    let mut acc: u32 = 0;
    let mut bits = 0u32;
    for b in bytes {
        acc = (acc << 8) | u32::from(b);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            id.push(BASE32[((acc >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        id.push(BASE32[((acc << (5 - bits)) & 31) as usize] as char);
    }
    id
}

fn has_id_shape(value: &str, prefix: &str) -> bool {
    value.strip_prefix(prefix).is_some_and(|rest| {
        rest.len() == 26
            && rest
                .bytes()
                .all(|c| c.is_ascii_lowercase() || (b'2'..=b'7').contains(&c))
    })
}

/// `sp_` + 26 base32 characters.
pub fn is_space_id(value: &str) -> bool {
    has_id_shape(value, SPACE_PREFIX)
}

/// `th_` + 26 base32 characters.
pub fn is_thread_id(value: &str) -> bool {
    has_id_shape(value, THREAD_PREFIX)
}

/// A label as written: any characters, trimmed, never empty, bounded. Control characters are
/// refused (a label is shown to people and written into `thread.json`).
pub fn clean_label(raw: &str, fallback: &str) -> Result<String, OrchestratorError> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(fallback.to_string());
    }
    if trimmed.chars().count() > MAX_LABEL_CHARS {
        return Err(OrchestratorError::InvalidContract(format!(
            "a label is at most {MAX_LABEL_CHARS} characters"
        )));
    }
    if trimmed.chars().any(char::is_control) {
        return Err(OrchestratorError::InvalidContract(
            "a label must not contain control characters".into(),
        ));
    }
    Ok(trimmed.to_string())
}

/// Which default Space of an agent a run uses: today's draft / released split.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultKind {
    Released,
    Draft,
}

impl DefaultKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DefaultKind::Released => "released",
            DefaultKind::Draft => "draft",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpaceRow {
    pub space_id: String,
    pub owner_user_id: String,
    pub agent_urn: Option<String>,
    pub label: String,
    pub is_default: bool,
    pub default_key: Option<String>,
    pub default_thread_id: Option<String>,
    pub created_via: String,
    pub created_by_grant_id: Option<String>,
    pub created_at: i64,
    pub last_active_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub space_id: String,
    pub thread_id: String,
    pub label: String,
    pub state: String,
    pub created_via: String,
    pub created_by_grant_id: Option<String>,
    pub created_at: i64,
}

const SPACE_COLUMNS: &str = "space_id, owner_user_id, agent_urn, label, is_default, default_key, \
     default_thread_id, created_via, created_by_grant_id, created_at, last_active_at";
const THREAD_COLUMNS: &str =
    "space_id, thread_id, label, state, created_via, created_by_grant_id, created_at";

fn space_from_row(row: &Row<'_>) -> rusqlite::Result<SpaceRow> {
    Ok(SpaceRow {
        space_id: row.get(0)?,
        owner_user_id: row.get(1)?,
        agent_urn: row.get(2)?,
        label: row.get(3)?,
        is_default: row.get::<_, i64>(4)? != 0,
        default_key: row.get(5)?,
        default_thread_id: row.get(6)?,
        created_via: row.get(7)?,
        created_by_grant_id: row.get(8)?,
        created_at: row.get(9)?,
        last_active_at: row.get(10)?,
    })
}

fn thread_from_row(row: &Row<'_>) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        space_id: row.get(0)?,
        thread_id: row.get(1)?,
        label: row.get(2)?,
        state: row.get(3)?,
        created_via: row.get(4)?,
        created_by_grant_id: row.get(5)?,
        created_at: row.get(6)?,
    })
}

/// Creates the tables. Safe to run on every start.
pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS spaces (
            space_id TEXT PRIMARY KEY NOT NULL,
            app_id TEXT,
            owner_user_id TEXT NOT NULL,
            agent_urn TEXT,
            label TEXT NOT NULL,
            is_default INTEGER NOT NULL DEFAULT 0,
            default_key TEXT CHECK (default_key IS NULL OR default_key IN ('released','draft')),
            default_thread_id TEXT,
            created_via TEXT NOT NULL CHECK (created_via IN ('run','grant','api')),
            created_by_grant_id TEXT,
            created_at INTEGER NOT NULL,
            last_active_at INTEGER NOT NULL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_spaces_default
            ON spaces(owner_user_id, IFNULL(agent_urn, ''), default_key) WHERE is_default = 1;
        CREATE INDEX IF NOT EXISTS idx_spaces_owner ON spaces(owner_user_id, created_at, space_id);
        CREATE TABLE IF NOT EXISTS threads (
            space_id TEXT NOT NULL REFERENCES spaces(space_id),
            thread_id TEXT NOT NULL,
            label TEXT NOT NULL,
            state TEXT NOT NULL DEFAULT 'live' CHECK (state IN ('reserved','live')),
            created_via TEXT NOT NULL CHECK (created_via IN ('run','grant','api')),
            created_by_grant_id TEXT,
            created_at INTEGER NOT NULL,
            PRIMARY KEY (space_id, thread_id)
        );
        CREATE TABLE IF NOT EXISTS space_create_keys (
            owner_user_id TEXT NOT NULL,
            idempotency_key TEXT NOT NULL,
            space_id TEXT NOT NULL REFERENCES spaces(space_id),
            PRIMARY KEY (owner_user_id, idempotency_key)
        );
        ",
    )?;
    Ok(())
}

fn now() -> i64 {
    crate::catalog::unix_timestamp()
}

fn not_found_space() -> OrchestratorError {
    OrchestratorError::NotFound("space not found".into())
}

impl SqliteWorkflowStore {
    fn spaces_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, OrchestratorError> {
        self.connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)
    }

    /// The one access function: a Space by id **for this owner**. A foreign id answers the same
    /// "not found" as a missing one, so an id leaks nothing about who holds it.
    pub fn get_space(&self, owner: &str, space_id: &str) -> Result<SpaceRow, OrchestratorError> {
        let conn = self.spaces_conn()?;
        get_space_in(&conn, owner, space_id)
    }

    /// A thread by id inside the Space the caller names, owner checked first.
    pub fn get_thread(
        &self,
        owner: &str,
        space_id: &str,
        thread_id: &str,
    ) -> Result<ThreadRow, OrchestratorError> {
        let conn = self.spaces_conn()?;
        get_thread_in(&conn, owner, space_id, thread_id)
    }

    /// The owner's default Space for an agent and a draft / released side, created on first use.
    /// `label` names it only when it is created.
    pub fn default_space(
        &self,
        owner: &str,
        agent_urn: Option<&str>,
        kind: DefaultKind,
        label: &str,
    ) -> Result<SpaceRow, OrchestratorError> {
        let agent = agent_urn.filter(|u| !u.is_empty());
        let conn = self.spaces_conn()?;
        let t = now();
        // The partial unique index makes a racing create a no-op; the select then finds the
        // winner's row.
        conn.execute(
            "INSERT OR IGNORE INTO spaces (space_id, owner_user_id, agent_urn, label, is_default, \
             default_key, created_via, created_at, last_active_at) \
             VALUES (?1, ?2, ?3, ?4, 1, ?5, 'run', ?6, ?6)",
            params![mint_id(SPACE_PREFIX), owner, agent, label, kind.as_str(), t],
        )?;
        let row = conn
            .query_row(
                &format!(
                    "SELECT {SPACE_COLUMNS} FROM spaces WHERE owner_user_id = ?1 \
                     AND IFNULL(agent_urn, '') = ?2 AND is_default = 1 AND default_key = ?3"
                ),
                params![owner, agent.unwrap_or(""), kind.as_str()],
                space_from_row,
            )
            .optional()?;
        row.ok_or_else(not_found_space)
    }

    /// A new (non-default) Space for the owner, labelled as given and bound to `agent_urn` when
    /// it is known. Used by the API that creates Spaces and by grants (`created_by_grant_id`).
    pub fn create_space(
        &self,
        owner: &str,
        agent_urn: Option<&str>,
        label: &str,
        created_via: &str,
        created_by_grant_id: Option<&str>,
    ) -> Result<SpaceRow, OrchestratorError> {
        let space_id = mint_id(SPACE_PREFIX);
        let agent = agent_urn.filter(|u| !u.is_empty());
        let conn = self.spaces_conn()?;
        let t = now();
        conn.execute(
            "INSERT INTO spaces (space_id, owner_user_id, agent_urn, label, is_default, \
             created_via, created_by_grant_id, created_at, last_active_at) \
             VALUES (?1, ?2, ?3, ?4, 0, ?5, ?6, ?7, ?7)",
            params![
                space_id,
                owner,
                agent,
                label,
                created_via,
                created_by_grant_id,
                t
            ],
        )?;
        get_space_in(&conn, owner, &space_id)
    }

    /// Creates a Space once per (owner, idempotency key): a retry with the same key returns the
    /// Space the first call made. The connection lock makes the check and the insert one step.
    pub fn create_space_once(
        &self,
        owner: &str,
        idempotency_key: &str,
        agent_urn: &str,
        label: &str,
    ) -> Result<SpaceRow, OrchestratorError> {
        let conn = self.spaces_conn()?;
        let existing: Option<String> = conn
            .query_row(
                "SELECT space_id FROM space_create_keys WHERE owner_user_id = ?1 \
                 AND idempotency_key = ?2",
                params![owner, idempotency_key],
                |r| r.get(0),
            )
            .optional()?;
        if let Some(space_id) = existing {
            return get_space_in(&conn, owner, &space_id);
        }
        let space_id = mint_id(SPACE_PREFIX);
        let t = now();
        conn.execute(
            "INSERT INTO spaces (space_id, owner_user_id, agent_urn, label, is_default, \
             created_via, created_at, last_active_at) VALUES (?1, ?2, ?3, ?4, 0, 'api', ?5, ?5)",
            params![space_id, owner, agent_urn, label, t],
        )?;
        conn.execute(
            "INSERT INTO space_create_keys (owner_user_id, idempotency_key, space_id) \
             VALUES (?1, ?2, ?3)",
            params![owner, idempotency_key, space_id],
        )?;
        get_space_in(&conn, owner, &space_id)
    }

    /// Renames a Space in place: only the label changes. Same call twice, same result.
    pub fn rename_space(
        &self,
        owner: &str,
        space_id: &str,
        label: &str,
    ) -> Result<SpaceRow, OrchestratorError> {
        let conn = self.spaces_conn()?;
        get_space_in(&conn, owner, space_id)?;
        conn.execute(
            "UPDATE spaces SET label = ?3 WHERE space_id = ?1 AND owner_user_id = ?2",
            params![space_id, owner, label],
        )?;
        get_space_in(&conn, owner, space_id)
    }

    /// Renames a thread in place (owner and Space checked first): only the label changes.
    pub fn rename_thread(
        &self,
        owner: &str,
        space_id: &str,
        thread_id: &str,
        label: &str,
    ) -> Result<ThreadRow, OrchestratorError> {
        let conn = self.spaces_conn()?;
        get_thread_in(&conn, owner, space_id, thread_id)?;
        conn.execute(
            "UPDATE threads SET label = ?3 WHERE space_id = ?1 AND thread_id = ?2",
            params![space_id, thread_id, label],
        )?;
        get_thread_in(&conn, owner, space_id, thread_id)
    }

    /// Binds the agent of a Space that has none yet (the first run). A bound Space is unchanged.
    pub fn bind_space_agent(
        &self,
        owner: &str,
        space_id: &str,
        agent_urn: &str,
    ) -> Result<(), OrchestratorError> {
        let conn = self.spaces_conn()?;
        conn.execute(
            "UPDATE spaces SET agent_urn = ?3 WHERE space_id = ?1 AND owner_user_id = ?2 \
             AND (agent_urn IS NULL OR agent_urn = '') AND is_default = 0",
            params![space_id, owner, agent_urn],
        )?;
        Ok(())
    }

    /// A new thread in the Space (owner checked), labelled as written or [`UNTITLED`].
    pub fn create_thread(
        &self,
        owner: &str,
        space_id: &str,
        label: &str,
        created_via: &str,
    ) -> Result<ThreadRow, OrchestratorError> {
        let conn = self.spaces_conn()?;
        get_space_in(&conn, owner, space_id)?;
        let thread_id = mint_id(THREAD_PREFIX);
        let t = now();
        conn.execute(
            "INSERT INTO threads (space_id, thread_id, label, state, created_via, created_at) \
             VALUES (?1, ?2, ?3, 'live', ?4, ?5)",
            params![space_id, thread_id, label, created_via, t],
        )?;
        conn.execute(
            "UPDATE spaces SET last_active_at = ?2, \
             default_thread_id = COALESCE(default_thread_id, ?3) WHERE space_id = ?1",
            params![space_id, t, thread_id],
        )?;
        get_thread_in(&conn, owner, space_id, &thread_id)
    }

    /// Marks the Space as used now.
    pub fn touch_space(&self, owner: &str, space_id: &str) -> Result<(), OrchestratorError> {
        let conn = self.spaces_conn()?;
        conn.execute(
            "UPDATE spaces SET last_active_at = ?3 WHERE space_id = ?1 AND owner_user_id = ?2",
            params![space_id, owner, now()],
        )?;
        Ok(())
    }

    /// The owner's Spaces, oldest first, after `after` (a `(created_at, space_id)` cursor).
    pub fn list_spaces(
        &self,
        owner: &str,
        agent_urn: Option<&str>,
        after: Option<(i64, String)>,
        limit: usize,
    ) -> Result<Vec<SpaceRow>, OrchestratorError> {
        let conn = self.spaces_conn()?;
        let (after_t, after_id) = after.unwrap_or((i64::MIN, String::new()));
        let mut stmt = conn.prepare(&format!(
            "SELECT {SPACE_COLUMNS} FROM spaces WHERE owner_user_id = ?1 \
             AND (?2 IS NULL OR agent_urn = ?2) AND (created_at, space_id) > (?3, ?4) \
             ORDER BY created_at, space_id LIMIT ?5"
        ))?;
        let rows = stmt
            .query_map(
                params![owner, agent_urn, after_t, after_id, limit as i64],
                space_from_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// The threads of a Space (owner checked), oldest first.
    pub fn list_thread_rows(
        &self,
        owner: &str,
        space_id: &str,
    ) -> Result<Vec<ThreadRow>, OrchestratorError> {
        let conn = self.spaces_conn()?;
        get_space_in(&conn, owner, space_id)?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {THREAD_COLUMNS} FROM threads WHERE space_id = ?1 ORDER BY created_at, thread_id"
        ))?;
        let rows = stmt
            .query_map(params![space_id], thread_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

fn get_space_in(
    conn: &Connection,
    owner: &str,
    space_id: &str,
) -> Result<SpaceRow, OrchestratorError> {
    conn.query_row(
        &format!("SELECT {SPACE_COLUMNS} FROM spaces WHERE space_id = ?1 AND owner_user_id = ?2"),
        params![space_id, owner],
        space_from_row,
    )
    .optional()?
    .ok_or_else(not_found_space)
}

fn get_thread_in(
    conn: &Connection,
    owner: &str,
    space_id: &str,
    thread_id: &str,
) -> Result<ThreadRow, OrchestratorError> {
    get_space_in(conn, owner, space_id)?;
    conn.query_row(
        &format!("SELECT {THREAD_COLUMNS} FROM threads WHERE space_id = ?1 AND thread_id = ?2"),
        params![space_id, thread_id],
        thread_from_row,
    )
    .optional()?
    .ok_or_else(|| OrchestratorError::NotFound("thread not found".into()))
}

/// The Space a route names: an `sp_…` id the owner holds. A name is a label, not a key, so it
/// reaches nothing; a foreign id is "not found" like a missing one.
pub fn space_for_route(
    store: &SqliteWorkflowStore,
    owner: &str,
    raw: &str,
) -> Result<String, OrchestratorError> {
    let id = raw.trim();
    if !is_space_id(id) {
        return Err(not_found_space());
    }
    Ok(store.get_space(owner, id)?.space_id)
}

/// Where a run goes: its Space and thread, resolved by ids with the defaults.
#[derive(Debug, Clone)]
pub struct Place {
    pub space: SpaceRow,
    pub thread: ThreadRow,
}

/// What a request names.
pub struct PlaceRequest<'a> {
    pub owner: &'a str,
    pub space_id: &'a str,
    pub thread_id: &'a str,
    /// The thread label the caller gave (`threadRef`); only a label, never a lookup key.
    pub thread_label: &'a str,
    /// The Space label the caller gave (`workspaceRefUri`); names a default Space on creation.
    pub space_label: &'a str,
    pub agent_urn: Option<&'a str>,
    pub kind: DefaultKind,
}

/// Resolves the place of a run: a Space id (owner checked) or the agent's default Space; a
/// thread id (inside that Space only) or a new thread labelled as given.
pub fn resolve_place(
    store: &SqliteWorkflowStore,
    req: &PlaceRequest<'_>,
) -> Result<Place, OrchestratorError> {
    let space = if req.space_id.trim().is_empty() {
        store.default_space(
            req.owner,
            req.agent_urn,
            req.kind,
            &clean_label(req.space_label, DEFAULT_SPACE_LABEL)?,
        )?
    } else {
        store.get_space(req.owner, req.space_id.trim())?
    };
    let thread = if req.thread_id.trim().is_empty() {
        store.create_thread(
            req.owner,
            &space.space_id,
            &clean_label(req.thread_label, UNTITLED)?,
            "run",
        )?
    } else {
        store.get_thread(req.owner, &space.space_id, req.thread_id.trim())?
    };
    Ok(Place { space, thread })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> SqliteWorkflowStore {
        SqliteWorkflowStore::in_memory().expect("store")
    }

    fn req<'a>(owner: &'a str, space_id: &'a str, thread_id: &'a str) -> PlaceRequest<'a> {
        PlaceRequest {
            owner,
            space_id,
            thread_id,
            thread_label: "",
            space_label: "",
            agent_urn: Some("urn:agent:a"),
            kind: DefaultKind::Released,
        }
    }

    #[test]
    fn ids_have_the_shape_and_differ() {
        let a = mint_id(SPACE_PREFIX);
        let b = mint_id(SPACE_PREFIX);
        assert!(is_space_id(&a) && is_space_id(&b));
        assert_ne!(a, b);
        assert!(is_thread_id(&mint_id(THREAD_PREFIX)));
        assert!(!is_space_id(&mint_id(THREAD_PREFIX)));
        assert!(!is_space_id("sp_short"));
    }

    #[test]
    fn a_foreign_space_id_is_not_found_exactly_like_a_missing_one() {
        let s = store();
        let space = s
            .default_space(
                "alice",
                Some("urn:agent:a"),
                DefaultKind::Released,
                "assistant",
            )
            .unwrap();
        let foreign = s.get_space("bob", &space.space_id).unwrap_err().to_string();
        let missing = s
            .get_space("bob", &mint_id(SPACE_PREFIX))
            .unwrap_err()
            .to_string();
        assert!(matches!(
            s.get_space("bob", &space.space_id),
            Err(OrchestratorError::NotFound(_))
        ));
        assert_eq!(foreign, missing);
        assert!(s.get_space("alice", &space.space_id).is_ok());
        // Threads and the place resolver are behind the same door.
        let thread = s
            .create_thread("alice", &space.space_id, "t", "run")
            .unwrap();
        assert!(s
            .get_thread("bob", &space.space_id, &thread.thread_id)
            .is_err());
        assert!(s.create_thread("bob", &space.space_id, "t", "run").is_err());
        assert!(s.list_thread_rows("bob", &space.space_id).is_err());
        assert!(resolve_place(&s, &req("bob", &space.space_id, "")).is_err());
        assert!(s.list_spaces("bob", None, None, 10).unwrap().is_empty());
    }

    #[test]
    fn default_space_is_one_per_owner_agent_and_side() {
        let s = store();
        let r1 = s
            .default_space("alice", Some("urn:agent:a"), DefaultKind::Released, "x y")
            .unwrap();
        let r2 = s
            .default_space("alice", Some("urn:agent:a"), DefaultKind::Released, "other")
            .unwrap();
        let d = s
            .default_space("alice", Some("urn:agent:a"), DefaultKind::Draft, "x y")
            .unwrap();
        let b = s
            .default_space("bob", Some("urn:agent:a"), DefaultKind::Released, "x y")
            .unwrap();
        let other_agent = s
            .default_space("alice", Some("urn:agent:b"), DefaultKind::Released, "x y")
            .unwrap();
        assert_eq!(r1.space_id, r2.space_id);
        assert_eq!(r1.label, "x y");
        let ids = [
            &r1.space_id,
            &d.space_id,
            &b.space_id,
            &other_agent.space_id,
        ];
        for (i, a) in ids.iter().enumerate() {
            for z in &ids[i + 1..] {
                assert_ne!(a, z);
            }
        }
    }

    #[test]
    fn threads_are_labels_not_names_and_resolve_only_in_their_space() {
        let s = store();
        let sp1 = s
            .default_space("alice", Some("urn:agent:a"), DefaultKind::Released, "a")
            .unwrap();
        let sp2 = s
            .default_space("alice", Some("urn:agent:a"), DefaultKind::Draft, "a")
            .unwrap();
        let mut r = req("alice", "", "");
        r.thread_label = "  daily notes  ";
        let one = resolve_place(&s, &r).unwrap();
        let two = resolve_place(&s, &r).unwrap();
        assert_eq!(one.thread.label, "daily notes");
        assert_eq!(two.thread.label, "daily notes");
        assert_ne!(one.thread.thread_id, two.thread.thread_id);
        assert_eq!(one.space.space_id, sp1.space_id);
        r.thread_label = "";
        assert_eq!(resolve_place(&s, &r).unwrap().thread.label, UNTITLED);
        // By id, in its Space.
        let again = resolve_place(&s, &req("alice", &sp1.space_id, &one.thread.thread_id)).unwrap();
        assert_eq!(again.thread.thread_id, one.thread.thread_id);
        // The same thread id means nothing in another Space.
        assert!(resolve_place(&s, &req("alice", &sp2.space_id, &one.thread.thread_id)).is_err());
        // A thread id with no Space named uses the default Space and is checked there.
        assert!(resolve_place(&s, &req("alice", "", &one.thread.thread_id)).is_ok());
    }

    #[test]
    fn a_label_is_a_plain_row_update() {
        let s = store();
        let sp = s
            .default_space("alice", None, DefaultKind::Released, "before")
            .unwrap();
        let t = s
            .create_thread("alice", &sp.space_id, "one", "run")
            .unwrap();
        {
            let conn = s.connection.lock().unwrap();
            conn.execute(
                "UPDATE threads SET label = 'two words' WHERE space_id = ?1 AND thread_id = ?2",
                params![sp.space_id, t.thread_id],
            )
            .unwrap();
        }
        let t2 = s.get_thread("alice", &sp.space_id, &t.thread_id).unwrap();
        assert_eq!(t2.label, "two words");
        assert_eq!(t2.thread_id, t.thread_id);
    }

    #[test]
    fn labels_keep_spaces_and_refuse_control_characters() {
        assert_eq!(clean_label("my notes", UNTITLED).unwrap(), "my notes");
        assert_eq!(clean_label("   ", UNTITLED).unwrap(), UNTITLED);
        assert!(clean_label("a\nb", UNTITLED).is_err());
        assert!(clean_label(&"x".repeat(MAX_LABEL_CHARS + 1), UNTITLED).is_err());
    }

    #[test]
    fn concurrent_default_creation_yields_one_space() {
        let s = std::sync::Arc::new(store());
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let s = s.clone();
                std::thread::spawn(move || {
                    s.default_space("alice", Some("urn:agent:a"), DefaultKind::Draft, "d")
                        .unwrap()
                        .space_id
                })
            })
            .collect();
        let ids: std::collections::BTreeSet<_> =
            handles.into_iter().map(|h| h.join().unwrap()).collect();
        assert_eq!(ids.len(), 1);
    }

    #[test]
    fn listing_is_owner_scoped_and_filters_by_agent() {
        let s = store();
        s.default_space("alice", Some("urn:agent:a"), DefaultKind::Released, "a")
            .unwrap();
        s.default_space("alice", Some("urn:agent:b"), DefaultKind::Released, "b")
            .unwrap();
        s.default_space("bob", Some("urn:agent:a"), DefaultKind::Released, "a")
            .unwrap();
        assert_eq!(s.list_spaces("alice", None, None, 10).unwrap().len(), 2);
        assert_eq!(
            s.list_spaces("alice", Some("urn:agent:b"), None, 10)
                .unwrap()
                .len(),
            1
        );
        assert_eq!(s.list_spaces("bob", None, None, 10).unwrap().len(), 1);
    }
}
