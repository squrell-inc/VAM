//! Catalog ownership tier: `user`, `platform` (`examples.local` seeding) and `marketplace`
//! (installed from a trusted marketplace, CN-4.5), and the rules that keep installed rows
//! read-only (M2) and the domains of trusted marketplaces theirs (M3).
//!
//! **Who may write which domain, and from where**
//!
//! | Domain of the URN | Who writes it | From where |
//! |---|---|---|
//! | the org's registry domain, `local.vam.dev`, `*.local` | callers (`user`) | publish, put, release |
//! | `examples.local` | platform seeding (`platform`) | never a caller |
//! | the marketplace domain (`hub.squrell.cloud`, `VAMD_MARKETPLACE_DOMAIN`) | the catalog import (`marketplace`) | only the preconfigured Squrell marketplace |
//! | the `publisherDomain` of an added marketplace | the catalog import (`marketplace`) | only that marketplace |
//!
//! A caller's write on a URN under a marketplace domain, or on any installed URN, is refused
//! ([`reject_caller_installed_urn`]); that includes an org admin who has reassigned the owner.

use rusqlite::{Connection, OptionalExtension};

use crate::error::OrchestratorError;

/// Default tier for caller-published catalog artifacts.
pub const CATALOG_TIER_USER: &str = "user";
/// `examples.local` artifacts stamped as platform (not boot-copied from `examples/`).
pub const CATALOG_TIER_PLATFORM: &str = "platform";
/// Rows the catalog import wrote from a trusted marketplace. Nobody else writes this tier, and
/// nobody changes such a row: no draft, put, publish, release, delete or owner change (M2).
pub const CATALOG_TIER_MARKETPLACE: &str = "marketplace";

pub(crate) const CATALOG_TIER_CHECK_SQL: &str =
    "CHECK(catalog_tier IN ('user', 'platform', 'marketplace'))";

const CATALOG_TIER_TABLES: &[&str] = &[
    "agents",
    "playbooks",
    "skills",
    "policies",
    "skill_packages",
];

/// True when `urn` names a bundled `examples.local` catalog artifact.
pub fn is_platform_example_urn(urn: &str) -> bool {
    urn.contains(":examples.local/")
}

/// Refuses a caller publish under `examples.local`; that namespace is written only by platform seeding.
pub(crate) fn reject_caller_platform_urn(urn: &str, noun: &str) -> Result<(), OrchestratorError> {
    reject_caller_platform_urn_with(urn, noun, &crate::registry::registry_domains())
}

/// [`reject_caller_platform_urn`] against explicit domains. Also refuses the
/// configured marketplace domain, so nobody can forge a Squrell origin. With
/// no marketplace domain set, nothing extra is refused.
pub(crate) fn reject_caller_platform_urn_with(
    urn: &str,
    noun: &str,
    domains: &vam_spec::RegistryDomains,
) -> Result<(), OrchestratorError> {
    if is_platform_example_urn(urn) {
        return Err(OrchestratorError::InvalidContract(format!(
            "{noun} URN '{urn}' is in the examples.local namespace, which is reserved for platform artifacts"
        )));
    }
    if domains.is_marketplace_urn(urn) {
        return Err(OrchestratorError::InvalidContract(format!(
            "{noun} URN '{urn}' is under the marketplace domain '{}', which only the marketplace may publish to; use your registry domain '{}'",
            domains.marketplace_domain(),
            domains.registry_domain()
        )));
    }
    Ok(())
}

/// The tables that hold rows of one URN kind (`urn:vam:<kind>:…`). A skill lives in `skills`
/// (JSON puts) or `skill_packages` (uploaded packages).
fn tables_of_urn_kind(urn: &str) -> &'static [&'static str] {
    let kind = urn
        .strip_prefix("urn:vam:")
        .and_then(|rest| rest.split_once(':'))
        .map(|(kind, _)| kind);
    match kind {
        Some("agent") => &["agents"],
        Some("playbook") => &["playbooks"],
        Some("skill") => &["skills", "skill_packages"],
        Some("policy") => &["policies"],
        Some("connector") => &["connector_releases"],
        _ => &[],
    }
}

/// True when a catalog row of `urn` is a marketplace install (tier `marketplace`), whatever its
/// owner or channel.
pub(crate) fn is_installed_urn(
    connection: &Connection,
    urn: &str,
) -> Result<bool, OrchestratorError> {
    for table in tables_of_urn_kind(urn) {
        let hit: Option<i64> = connection
            .query_row(
                &format!("SELECT 1 FROM {table} WHERE urn = ?1 AND catalog_tier = ?2 LIMIT 1"),
                rusqlite::params![urn, CATALOG_TIER_MARKETPLACE],
                |row| row.get(0),
            )
            .optional()
            .map_err(OrchestratorError::Database)?;
        if hit.is_some() {
            return Ok(true);
        }
    }
    Ok(false)
}

/// M2 and M3 for a caller's write (draft, put, publish, release) on `urn`, under the one lock
/// the write holds: refuses an installed URN (409), and any URN under the `publisherDomain` of a
/// trusted marketplace, the Squrell one included (400). The catalog import does not come through
/// here; it is the only writer of these rows.
pub(crate) fn reject_caller_installed_urn(
    connection: &Connection,
    urn: &str,
    noun: &str,
) -> Result<(), OrchestratorError> {
    if is_installed_urn(connection, urn)? {
        return Err(OrchestratorError::Conflict(format!(
            "{noun} '{urn}' was installed from a marketplace and is read-only: nobody edits, \
             releases or deletes it; the Marketplace page updates it"
        )));
    }
    let Some(domain) = vam_spec::marketplace::urn_domain(urn) else {
        return Ok(());
    };
    let owner: Option<String> = connection
        .query_row(
            "SELECT name FROM node_marketplaces WHERE publisher_domain = ?1 LIMIT 1",
            rusqlite::params![domain],
            |row| row.get(0),
        )
        .optional()
        .map_err(OrchestratorError::Database)?;
    if let Some(name) = owner {
        return Err(OrchestratorError::InvalidContract(format!(
            "{noun} URN '{urn}' is under the domain '{domain}' of the trusted marketplace '{name}', \
             which only that marketplace may publish to; use your registry domain '{}'",
            crate::registry::registry_domains().registry_domain()
        )));
    }
    Ok(())
}

/// Migration `catalog_tier_marketplace` (CN-4.5): the tier `CHECK` of a catalog table created
/// before the `marketplace` tier existed allows only `user | platform`, and SQLite cannot change a
/// `CHECK`. Each such table is rebuilt under the same name with the new `CHECK`, rows and indexes
/// kept in the one transaction; a table made after this change, or one whose tier column was
/// added by `ALTER` and so has no `CHECK`, is left alone. Idempotent.
pub(crate) fn allow_marketplace_tier(connection: &Connection) -> Result<(), OrchestratorError> {
    for table in CATALOG_TIER_TABLES {
        let sql: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .optional()
            .map_err(OrchestratorError::Database)?;
        let Some(sql) = sql else { continue };
        let Some(new_sql) = with_marketplace_tier(&sql) else {
            continue;
        };
        rebuild_table(connection, table, &new_sql)?;
    }
    Ok(())
}

/// `sql` with its tier `CHECK` widened, or `None` when it has none or already allows the tier.
fn with_marketplace_tier(sql: &str) -> Option<String> {
    let start = sql.find("CHECK(catalog_tier IN (")?;
    let end = start + sql[start..].find("))")? + 2;
    if sql[start..end].contains("'marketplace'") {
        return None;
    }
    Some(format!(
        "{}{}{}",
        &sql[..start],
        CATALOG_TIER_CHECK_SQL,
        &sql[end..]
    ))
}

fn rebuild_table(
    connection: &Connection,
    table: &str,
    new_sql: &str,
) -> Result<(), OrchestratorError> {
    if !is_safe_ident(table) {
        return Err(OrchestratorError::InvalidContract(format!(
            "unsafe table name '{table}'"
        )));
    }
    let tmp = format!("{table}__tier_new");
    let create = [
        format!("CREATE TABLE \"{table}\""),
        format!("CREATE TABLE {table}"),
    ]
    .into_iter()
    .find_map(|head| {
        new_sql
            .strip_prefix(&head)
            .map(|rest| format!("CREATE TABLE {tmp}{rest}"))
    })
    .ok_or_else(|| {
        OrchestratorError::CorruptState(format!(
            "cannot rebuild {table}: its definition does not start with CREATE TABLE {table}"
        ))
    })?;
    let mut extras: Vec<String> = Vec::new();
    {
        let mut stmt = connection
            .prepare(
                "SELECT sql FROM sqlite_master
                 WHERE type IN ('index', 'trigger') AND tbl_name = ?1 AND sql IS NOT NULL",
            )
            .map_err(OrchestratorError::Database)?;
        let rows = stmt
            .query_map([table], |row| row.get::<_, String>(0))
            .map_err(OrchestratorError::Database)?;
        for row in rows {
            extras.push(row.map_err(OrchestratorError::Database)?);
        }
    }
    // Foreign keys cannot be switched inside a transaction.
    connection
        .execute("PRAGMA foreign_keys = OFF", [])
        .map_err(OrchestratorError::Database)?;
    let rebuilt = (|| -> Result<(), rusqlite::Error> {
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let steps = (|| -> Result<(), rusqlite::Error> {
            connection.execute_batch(&create)?;
            connection.execute_batch(&format!(
                "INSERT INTO {tmp} SELECT * FROM {table} ORDER BY rowid;
                 DROP TABLE {table};
                 ALTER TABLE {tmp} RENAME TO {table};"
            ))?;
            for extra in &extras {
                connection.execute_batch(extra)?;
            }
            Ok(())
        })();
        match steps {
            Ok(()) => connection.execute_batch("COMMIT"),
            Err(e) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    })();
    let restored = connection.execute("PRAGMA foreign_keys = ON", []);
    rebuilt.map_err(OrchestratorError::Database)?;
    restored.map_err(OrchestratorError::Database)?;
    Ok(())
}

impl crate::workflow::SqliteWorkflowStore {
    /// M2 and M3 for a caller's write on `urn` (see [`reject_caller_installed_urn`]), for the
    /// paths that decide before they hold the write lock.
    pub fn require_not_installed(&self, urn: &str, noun: &str) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        reject_caller_installed_urn(&connection, urn, noun)
    }

    /// Add `catalog_tier` on versioned catalog tables; widen its `CHECK` to allow `marketplace`
    /// (migration `catalog_tier_marketplace`); backfill `examples.local` URNs.
    pub fn initialize_catalog_tier(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        for table in CATALOG_TIER_TABLES {
            add_catalog_tier_column(&connection, table)?;
        }
        allow_marketplace_tier(&connection)?;
        for table in CATALOG_TIER_TABLES {
            connection
                .execute(
                    &format!(
                        "UPDATE {table} SET catalog_tier = ?1
                         WHERE urn LIKE '%:examples.local/%' AND catalog_tier != ?1"
                    ),
                    [CATALOG_TIER_PLATFORM],
                )
                .map_err(OrchestratorError::Database)?;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn catalog_tier_for_urn(
        &self,
        table: &str,
        urn: &str,
    ) -> Result<String, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        catalog_tier_for_urn(&connection, table, urn)?
            .ok_or_else(|| OrchestratorError::NotFound(format!("{table} '{urn}' not found")))
    }

    #[cfg(test)]
    pub(crate) fn count_examples_local_catalog_rows(&self) -> Result<usize, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut total = 0usize;
        for table in CATALOG_TIER_TABLES {
            let n: i64 = connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE urn LIKE '%:examples.local/%'"),
                    [],
                    |row| row.get(0),
                )
                .map_err(OrchestratorError::Database)?;
            total += n as usize;
        }
        Ok(total)
    }
}

pub(crate) fn catalog_tier_for_urn(
    connection: &Connection,
    table: &str,
    urn: &str,
) -> Result<Option<String>, OrchestratorError> {
    if !is_safe_ident(table) {
        return Err(OrchestratorError::InvalidContract(format!(
            "unsafe table name '{table}'"
        )));
    }
    connection
        .query_row(
            &format!(
                "SELECT catalog_tier FROM {table} WHERE urn = ?1
                 ORDER BY updated_at DESC LIMIT 1"
            ),
            [urn],
            |row| row.get(0),
        )
        .optional()
        .map_err(OrchestratorError::Database)
}

/// Tier for a new row: inherit an existing urn tier, else default `user` (or `platform` when seeding).
pub(crate) fn resolve_catalog_tier_for_insert(
    connection: &Connection,
    table: &str,
    urn: &str,
    seed_platform: bool,
) -> Result<String, OrchestratorError> {
    if seed_platform {
        return Ok(CATALOG_TIER_PLATFORM.to_string());
    }
    if let Some(tier) = catalog_tier_for_urn(connection, table, urn)? {
        return Ok(tier);
    }
    Ok(CATALOG_TIER_USER.to_string())
}

fn add_catalog_tier_column(connection: &Connection, table: &str) -> Result<(), OrchestratorError> {
    if !is_safe_ident(table) {
        return Err(OrchestratorError::InvalidContract(format!(
            "unsafe table name '{table}'"
        )));
    }
    let sql = format!(
        "ALTER TABLE {table} ADD COLUMN catalog_tier TEXT NOT NULL DEFAULT '{CATALOG_TIER_USER}'"
    );
    match connection.execute(&sql, []) {
        Ok(_) => Ok(()),
        Err(e) if is_duplicate_column(&e) => Ok(()),
        Err(e) => Err(OrchestratorError::Database(e)),
    }
}

fn is_duplicate_column(err: &rusqlite::Error) -> bool {
    err.to_string().to_lowercase().contains("duplicate column")
}

fn is_safe_ident(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::SqliteWorkflowStore;

    #[test]
    fn marketplace_domain_is_refused_only_when_set() {
        let set = vam_spec::RegistryDomains::new("vam.squrell.dev", Some("hub.example.test"))
            .expect("domains");
        let unset = vam_spec::RegistryDomains::new("vam.squrell.dev", None).expect("domains");
        let urn = "urn:vam:agent:hub.example.test/acme/demo";
        let err = reject_caller_platform_urn_with(urn, "agent", &set).expect_err("refused");
        assert!(err.to_string().contains("marketplace domain"));
        assert!(reject_caller_platform_urn_with(urn, "agent", &unset).is_ok());
        assert!(reject_caller_platform_urn_with(
            "urn:vam:agent:vam.squrell.dev/acme/demo",
            "agent",
            &set
        )
        .is_ok());
    }

    #[test]
    fn reject_caller_platform_urn_only_refuses_examples_local() {
        assert!(
            reject_caller_platform_urn("urn:vam:skill:examples.local/vam/demo", "skill").is_err()
        );
        assert!(
            reject_caller_platform_urn("urn:vam:skill:local.vam.dev/acme/demo", "skill").is_ok()
        );
    }

    #[test]
    fn platform_example_urn_matches_examples_local_registry() {
        assert!(is_platform_example_urn(
            "urn:vam:policy:examples.local/vam/jira-egress-only"
        ));
        assert!(!is_platform_example_urn(
            "urn:vam:agent:local.vam.dev/acme/demo"
        ));
    }

    #[test]
    fn fresh_store_has_builtin_users_and_no_examples_catalog() {
        let store = SqliteWorkflowStore::in_memory().expect("db");
        assert_eq!(store.user_count().expect("users"), 2);
        let operator = store
            .get_user(crate::users::OPERATOR_USER_ID)
            .expect("operator");
        let local = store.get_user(crate::users::LOCAL_USER_ID).expect("local");
        assert_eq!(operator.username, crate::users::OPERATOR_USERNAME);
        assert_eq!(local.username, crate::users::LOCAL_USERNAME);
        assert_eq!(store.count_examples_local_catalog_rows().expect("count"), 0);
        assert!(store.list_agents().expect("agents").is_empty());
        assert!(store.list_policies().expect("policies").is_empty());
        assert!(store.list_skill_packages().expect("packages").is_empty());
    }

    #[test]
    fn put_platform_policy_stamps_platform_tier() {
        let store = SqliteWorkflowStore::in_memory().expect("db");
        const URN: &str = "urn:vam:policy:examples.local/vam/jira-egress-only";
        const CEDAR: &str = "permit(principal, action, resource);";
        store.put_platform_policy(URN, CEDAR).expect("put");
        assert_eq!(
            store.catalog_tier_for_urn("policies", URN).expect("tier"),
            CATALOG_TIER_PLATFORM
        );
    }

    /// A catalog table as the previous release created it: the tier `CHECK` allows only
    /// `user | platform`.
    const OLD_AGENTS_DDL: &str = "
        CREATE TABLE agents (
            urn TEXT NOT NULL,
            major INTEGER NOT NULL,
            minor INTEGER NOT NULL,
            patch INTEGER NOT NULL,
            prerelease TEXT NOT NULL DEFAULT '',
            channel TEXT NOT NULL CHECK(channel IN ('draft', 'released')),
            revision INTEGER NOT NULL,
            digest TEXT NOT NULL,
            spec_bytes BLOB NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            owner_user_id TEXT REFERENCES users(user_id),
            catalog_tier TEXT NOT NULL DEFAULT 'user' CHECK(catalog_tier IN ('user', 'platform')),
            PRIMARY KEY (urn, major, minor, patch, prerelease)
        );";

    fn insert_agent(c: &Connection, urn: &str, tier: &str) -> rusqlite::Result<usize> {
        c.execute(
            "INSERT INTO agents (urn, major, minor, patch, prerelease, channel, revision, digest,
                 spec_bytes, created_at, updated_at, owner_user_id, catalog_tier)
             VALUES (?1, 1, 0, 0, '', 'released', 1, 'd', x'00', 1, 1, 'local', ?2)",
            rusqlite::params![urn, tier],
        )
    }

    #[test]
    fn migration_widens_an_old_tier_check_and_keeps_rows_and_indexes() {
        let store = SqliteWorkflowStore::in_memory().expect("db");
        let c = store.connection.lock().unwrap();
        // Put `agents` back the way the previous release made it, with rows and extra indexes.
        c.execute_batch("DROP TABLE agents;").unwrap();
        c.execute_batch(OLD_AGENTS_DDL).unwrap();
        c.execute_batch(
            "ALTER TABLE agents ADD COLUMN release_note TEXT;
             ALTER TABLE agents ADD COLUMN released_by TEXT;
             CREATE INDEX idx_agents_owner_user_id ON agents(owner_user_id);
             CREATE UNIQUE INDEX idx_agents_one_draft ON agents(urn) WHERE channel = 'draft';",
        )
        .unwrap();
        insert_agent(&c, "urn:vam:agent:local.vam.dev/a/user", "user").unwrap();
        insert_agent(&c, "urn:vam:agent:examples.local/vam/plat", "platform").unwrap();
        c.execute(
            "UPDATE agents SET release_note = 'note', released_by = 'local' WHERE catalog_tier = 'user'",
            [],
        )
        .unwrap();
        assert!(
            insert_agent(&c, "urn:vam:agent:hub.squrell.cloud/s/x", "marketplace").is_err(),
            "the old CHECK refuses the new tier"
        );

        allow_marketplace_tier(&c).unwrap();

        // The new tier is accepted; the others are still the only ones that are.
        insert_agent(&c, "urn:vam:agent:hub.squrell.cloud/s/x", "marketplace").unwrap();
        assert!(insert_agent(&c, "urn:vam:agent:local.vam.dev/a/bad", "bogus").is_err());
        // Existing rows are as they were, tiers and release metadata included.
        let rows: Vec<(String, String, Option<String>)> = c
            .prepare("SELECT urn, catalog_tier, release_note FROM agents ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(
            rows[0],
            (
                "urn:vam:agent:local.vam.dev/a/user".into(),
                "user".into(),
                Some("note".into())
            )
        );
        assert_eq!(rows[1].1, "platform");
        // Both indexes came back, and the one-draft rule still holds.
        let idx: Vec<String> = c
            .prepare("SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'agents' AND sql IS NOT NULL ORDER BY name")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert_eq!(idx, ["idx_agents_one_draft", "idx_agents_owner_user_id"]);
        // Running it again changes nothing (idempotent), and a table made today needs no rebuild.
        allow_marketplace_tier(&c).unwrap();
        assert_eq!(
            c.query_row("SELECT COUNT(*) FROM agents", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        let fresh: String = c
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'playbooks'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(fresh.contains("'marketplace'"), "{fresh}");
        assert!(with_marketplace_tier(&fresh).is_none());
    }

    #[test]
    fn migration_leaves_a_table_whose_tier_was_added_by_alter_alone() {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE agents (urn TEXT PRIMARY KEY, catalog_tier TEXT NOT NULL DEFAULT 'user');",
        )
        .unwrap();
        allow_marketplace_tier(&c).unwrap();
        c.execute(
            "INSERT INTO agents (urn, catalog_tier) VALUES ('u', 'marketplace')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn caller_writes_on_installed_and_marketplace_urns_are_refused() {
        let store = SqliteWorkflowStore::in_memory().expect("db");
        let c = store.connection.lock().unwrap();
        let installed = "urn:vam:agent:hub.squrell.cloud/squrell/ea";
        insert_agent(&c, installed, "marketplace").unwrap();
        insert_agent(&c, "urn:vam:agent:local.vam.dev/acme/mine", "user").unwrap();
        // An installed URN: 409 whoever asks, and the same URN as another kind is its own thing.
        assert!(matches!(
            reject_caller_installed_urn(&c, installed, "agent"),
            Err(OrchestratorError::Conflict(_))
        ));
        assert!(
            reject_caller_installed_urn(&c, "urn:vam:agent:local.vam.dev/acme/mine", "agent")
                .is_ok()
        );
        assert!(reject_caller_installed_urn(
            &c,
            "urn:vam:playbook:local.vam.dev/acme/mine",
            "playbook"
        )
        .is_ok());
        // The Squrell domain is the marketplace's, installed or not.
        assert!(matches!(
            reject_caller_installed_urn(&c, "urn:vam:skill:hub.squrell.cloud/acme/new", "skill"),
            Err(OrchestratorError::InvalidContract(_))
        ));
        // An added marketplace's domain is reserved from the day it is added.
        c.execute(
            "INSERT INTO node_marketplaces (id, url, name, publisher_domain, trust, preconfigured,
                 enabled, created_at, created_by)
             VALUES ('m-1', 'https://git.example.com/a/b', 'Community', 'market.example', 'unverified', 0, 1, 1, 'u')",
            [],
        )
        .unwrap();
        let e = reject_caller_installed_urn(&c, "urn:vam:playbook:market.example/x/y", "playbook")
            .unwrap_err();
        assert!(e.to_string().contains("Community"), "{e}");
        // Strings that are not artifact URNs are not this guard's business.
        assert!(reject_caller_installed_urn(&c, "not-a-urn", "agent").is_ok());
    }

    #[test]
    fn user_published_policy_is_user_tier() {
        let store = SqliteWorkflowStore::in_memory().expect("db");
        const URN: &str = "urn:vam:policy:local.vam.dev/acme/user-policy";
        const CEDAR: &str = "permit(principal, action, resource);";
        store
            .put_policy_with_owner(URN, CEDAR, crate::users::LOCAL_USER_ID)
            .expect("put");
        assert_eq!(
            store.catalog_tier_for_urn("policies", URN).expect("tier"),
            CATALOG_TIER_USER
        );
    }
}
