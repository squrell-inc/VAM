//! The node's trusted marketplaces in SQLite.
//!
//! Tables (created with the rest of the schema, `CREATE IF NOT EXISTS`, no migration of existing
//! data):
//!
//! - `node_marketplaces`: one row per marketplace the node trusts: the canonical https URL, the
//!   publisher domain an admin confirmed, `trust` (`squrell` for the one preconfigured row,
//!   `unverified` for every added one), whether it is enabled, when it was last read and the error
//!   if that failed, the digest of the index it last read, and the listings that read produced (as
//!   JSON: each listing's tag, pinned commit, artifacts and capability summary).
//! - `node_marketplace_tags`: the tag -> commit pins (M5). A tag a listing names is pinned the first
//!   time it is read; a tag that later names another commit is refused, never followed.
//!
//! CN-4.5 adds its install records beside these (`node_marketplace_installs`, and
//! `catalog_origins` for each installed catalog row; see [`super::installs`]) and reads the pins
//! and listings from here.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

use super::{SQURELL_DOMAIN, SQURELL_ID, SQURELL_NAME, SQURELL_URL};

/// How far the node trusts a marketplace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Trust {
    /// The preconfigured Squrell marketplace; the only one that may write under the Squrell domain.
    Squrell,
    /// Any marketplace an admin added.
    Unverified,
}

impl Trust {
    pub fn as_str(self) -> &'static str {
        match self {
            Trust::Squrell => "squrell",
            Trust::Unverified => "unverified",
        }
    }
    pub(crate) fn parse(s: &str) -> Trust {
        if s == "squrell" {
            Trust::Squrell
        } else {
            Trust::Unverified
        }
    }
}

/// One artifact of a listing, as the last read found it at the listing's tag.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactState {
    pub kind: String,
    pub path: String,
    pub urn: String,
}

/// One listing as the last read of its marketplace left it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListingState {
    pub id: String,
    pub display_name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connector: Option<String>,
    #[serde(rename = "ref")]
    pub git_ref: String,
    pub requires_node: String,
    /// The commit the listing's tag is pinned to (set once the tag was read).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default)]
    pub artifacts: Vec<ArtifactState>,
    /// The capability summary (see `summary.rs`); absent when the listing could not be read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<serde_json::Value>,
    /// Why the listing cannot be installed as it stands (missing tag, moved tag, a package that
    /// fails its check, an artifact outside the marketplace's domain).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TagSeen {
    pub tag: String,
    pub commit: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MarketplaceRow {
    pub id: String,
    pub url: String,
    pub name: String,
    pub publisher_domain: String,
    pub trust: Trust,
    pub preconfigured: bool,
    pub enabled: bool,
    pub created_at: i64,
    pub created_by: String,
    pub last_checked_at: Option<i64>,
    pub last_error: Option<String>,
    pub index_digest: Option<String>,
    pub head_commit: Option<String>,
    pub listings: Vec<ListingState>,
    pub tags_seen: Vec<TagSeen>,
}

/// What a successful read of a marketplace produced.
#[derive(Debug, Clone, PartialEq)]
pub struct RefreshData {
    pub name: String,
    pub index_digest: String,
    pub head_commit: String,
    pub listings: Vec<ListingState>,
    pub tags_seen: Vec<TagSeen>,
    /// Tags pinned by this read (tag, commit).
    pub new_pins: Vec<(String, String)>,
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS node_marketplaces (
            id TEXT PRIMARY KEY,
            url TEXT NOT NULL UNIQUE,
            name TEXT NOT NULL,
            publisher_domain TEXT NOT NULL,
            trust TEXT NOT NULL CHECK(trust IN ('squrell', 'unverified')),
            preconfigured INTEGER NOT NULL,
            enabled INTEGER NOT NULL,
            created_at INTEGER NOT NULL,
            created_by TEXT NOT NULL,
            last_checked_at INTEGER,
            last_error TEXT,
            index_digest TEXT,
            head_commit TEXT,
            listings_json TEXT NOT NULL DEFAULT '[]',
            tags_seen_json TEXT NOT NULL DEFAULT '[]'
        );
        CREATE TABLE IF NOT EXISTS node_marketplace_tags (
            marketplace_id TEXT NOT NULL,
            tag TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            first_seen_at INTEGER NOT NULL,
            PRIMARY KEY (marketplace_id, tag)
        );
        -- CN-4.5: what each marketplace listing installed, and where each installed catalog row
        -- came from (one row per installed release).
        CREATE TABLE IF NOT EXISTS node_marketplace_installs (
            marketplace_id TEXT NOT NULL,
            listing_id TEXT NOT NULL,
            git_ref TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            installed_at INTEGER NOT NULL,
            installed_by TEXT NOT NULL,
            artifacts_json TEXT NOT NULL,
            PRIMARY KEY (marketplace_id, listing_id)
        );
        CREATE TABLE IF NOT EXISTS catalog_origins (
            kind TEXT NOT NULL,
            urn TEXT NOT NULL,
            major INTEGER NOT NULL,
            minor INTEGER NOT NULL,
            patch INTEGER NOT NULL,
            marketplace_id TEXT NOT NULL,
            listing_id TEXT NOT NULL,
            git_ref TEXT NOT NULL,
            commit_sha TEXT NOT NULL,
            package_digest TEXT NOT NULL,
            installed_at INTEGER NOT NULL,
            PRIMARY KEY (kind, urn, major, minor, patch)
        );
        -- CN-4.11: the connector files marketplaces installed. Same release columns as the catalog
        -- tables (the import writes them with the same code); `spec_bytes` is the connector JSON.
        -- The newest released row of a URN is the connector's definition.
        CREATE TABLE IF NOT EXISTS connector_releases (
            urn TEXT NOT NULL,
            major INTEGER NOT NULL,
            minor INTEGER NOT NULL,
            patch INTEGER NOT NULL,
            prerelease TEXT NOT NULL DEFAULT '',
            channel TEXT NOT NULL DEFAULT 'released',
            revision INTEGER NOT NULL DEFAULT 1,
            digest TEXT NOT NULL,
            spec_bytes BLOB NOT NULL,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            owner_user_id TEXT NOT NULL,
            catalog_tier TEXT NOT NULL DEFAULT 'marketplace'
                CHECK(catalog_tier IN ('user', 'platform', 'marketplace')),
            release_note TEXT,
            released_by TEXT,
            PRIMARY KEY (urn, major, minor, patch, prerelease)
        );
        ",
    )?;
    // The preconfigured Squrell marketplace is there from the first boot. `INSERT OR IGNORE`
    // never touches an existing row, so an admin's disable survives a restart.
    connection.execute(
        "INSERT OR IGNORE INTO node_marketplaces
             (id, url, name, publisher_domain, trust, preconfigured, enabled, created_at, created_by)
         VALUES (?1, ?2, ?3, ?4, 'squrell', 1, 1, ?5, 'system')",
        params![
            SQURELL_ID,
            SQURELL_URL,
            SQURELL_NAME,
            SQURELL_DOMAIN,
            crate::connectors::now_secs()
        ],
    )?;
    Ok(())
}

const COLUMNS: &str = "id, url, name, publisher_domain, trust, preconfigured, enabled, created_at, \
     created_by, last_checked_at, last_error, index_digest, head_commit, listings_json, tags_seen_json";

fn row_to_marketplace(r: &rusqlite::Row<'_>) -> rusqlite::Result<MarketplaceRow> {
    let listings: String = r.get(13)?;
    let tags: String = r.get(14)?;
    Ok(MarketplaceRow {
        id: r.get(0)?,
        url: r.get(1)?,
        name: r.get(2)?,
        publisher_domain: r.get(3)?,
        trust: Trust::parse(&r.get::<_, String>(4)?),
        preconfigured: r.get::<_, i64>(5)? != 0,
        enabled: r.get::<_, i64>(6)? != 0,
        created_at: r.get(7)?,
        created_by: r.get(8)?,
        last_checked_at: r.get(9)?,
        last_error: r.get(10)?,
        index_digest: r.get(11)?,
        head_commit: r.get(12)?,
        listings: serde_json::from_str(&listings).unwrap_or_default(),
        tags_seen: serde_json::from_str(&tags).unwrap_or_default(),
    })
}

fn json_of<T: Serialize>(v: &T) -> Result<String, OrchestratorError> {
    serde_json::to_string(v).map_err(|e| OrchestratorError::Core(e.to_string()))
}

impl SqliteWorkflowStore {
    fn mkt_conn(&self) -> Result<std::sync::MutexGuard<'_, Connection>, OrchestratorError> {
        self.connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)
    }

    /// Every marketplace, the preconfigured one first.
    pub fn marketplaces(&self) -> Result<Vec<MarketplaceRow>, OrchestratorError> {
        let c = self.mkt_conn()?;
        let mut stmt = c.prepare(&format!(
            "SELECT {COLUMNS} FROM node_marketplaces ORDER BY preconfigured DESC, created_at, id"
        ))?;
        let rows = stmt
            .query_map([], row_to_marketplace)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn marketplace(&self, id: &str) -> Result<Option<MarketplaceRow>, OrchestratorError> {
        let c = self.mkt_conn()?;
        Ok(c.query_row(
            &format!("SELECT {COLUMNS} FROM node_marketplaces WHERE id = ?1"),
            params![id],
            row_to_marketplace,
        )
        .optional()?)
    }

    /// The tag -> commit pins of one marketplace.
    pub fn marketplace_pins(
        &self,
        id: &str,
    ) -> Result<BTreeMap<String, String>, OrchestratorError> {
        let c = self.mkt_conn()?;
        let mut stmt = c.prepare(
            "SELECT tag, commit_sha FROM node_marketplace_tags WHERE marketplace_id = ?1",
        )?;
        let rows = stmt
            .query_map(params![id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<BTreeMap<_, _>, _>>()?;
        Ok(rows)
    }

    /// The commit a tag of a marketplace is pinned to, if it was ever read.
    pub fn marketplace_pin(
        &self,
        id: &str,
        tag: &str,
    ) -> Result<Option<String>, OrchestratorError> {
        let c = self.mkt_conn()?;
        Ok(c.query_row(
            "SELECT commit_sha FROM node_marketplace_tags WHERE marketplace_id = ?1 AND tag = ?2",
            params![id, tag],
            |r| r.get(0),
        )
        .optional()?)
    }

    /// Adds a marketplace an admin trusts (always `unverified`, never preconfigured), with the
    /// result of its first read. The URL is unique.
    pub fn insert_marketplace(
        &self,
        id: &str,
        url: &str,
        publisher_domain: &str,
        created_by: &str,
        now: i64,
        data: &RefreshData,
    ) -> Result<(), OrchestratorError> {
        let mut c = self.mkt_conn()?;
        let tx = c.transaction()?;
        let inserted = tx.execute(
            "INSERT INTO node_marketplaces
                 (id, url, name, publisher_domain, trust, preconfigured, enabled, created_at,
                  created_by, last_checked_at, last_error, index_digest, head_commit,
                  listings_json, tags_seen_json)
             VALUES (?1, ?2, ?3, ?4, 'unverified', 0, 1, ?5, ?6, ?5, NULL, ?7, ?8, ?9, ?10)",
            params![
                id,
                url,
                data.name,
                publisher_domain,
                now,
                created_by,
                data.index_digest,
                data.head_commit,
                json_of(&data.listings)?,
                json_of(&data.tags_seen)?
            ],
        );
        match inserted {
            Ok(_) => {}
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                return Err(OrchestratorError::Conflict(
                    "that marketplace is already trusted on this node".into(),
                ));
            }
            Err(e) => return Err(e.into()),
        }
        insert_pins(&tx, id, &data.new_pins, now)?;
        tx.commit()?;
        Ok(())
    }

    /// Records one read. A failed read passes `data: None` and the error; the listings of the last
    /// good read stay. A read that found a moved tag passes both: the listings (with the moved
    /// ones flagged) and the refusal.
    pub fn record_marketplace_read(
        &self,
        id: &str,
        now: i64,
        data: Option<&RefreshData>,
        error: Option<&str>,
    ) -> Result<(), OrchestratorError> {
        let mut c = self.mkt_conn()?;
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE node_marketplaces SET last_checked_at = ?2, last_error = ?3 WHERE id = ?1",
            params![id, now, error],
        )?;
        if let Some(d) = data {
            tx.execute(
                "UPDATE node_marketplaces SET name = ?2, index_digest = ?3, head_commit = ?4,
                     listings_json = ?5, tags_seen_json = ?6 WHERE id = ?1",
                params![
                    id,
                    d.name,
                    d.index_digest,
                    d.head_commit,
                    json_of(&d.listings)?,
                    json_of(&d.tags_seen)?
                ],
            )?;
            insert_pins(&tx, id, &d.new_pins, now)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn set_marketplace_enabled(
        &self,
        id: &str,
        enabled: bool,
    ) -> Result<bool, OrchestratorError> {
        let c = self.mkt_conn()?;
        Ok(c.execute(
            "UPDATE node_marketplaces SET enabled = ?2 WHERE id = ?1",
            params![id, enabled as i64],
        )? > 0)
    }

    /// Removes an added marketplace and its pins. The preconfigured one is never removed.
    pub fn delete_marketplace(&self, id: &str) -> Result<bool, OrchestratorError> {
        let mut c = self.mkt_conn()?;
        let tx = c.transaction()?;
        let n = tx.execute(
            "DELETE FROM node_marketplaces WHERE id = ?1 AND preconfigured = 0",
            params![id],
        )?;
        if n > 0 {
            tx.execute(
                "DELETE FROM node_marketplace_tags WHERE marketplace_id = ?1",
                params![id],
            )?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// Points the preconfigured row at this node's Squrell domain (`VAMD_MARKETPLACE_DOMAIN` can
    /// override the default for a staging node).
    pub fn sync_preconfigured_marketplace_domain(
        &self,
        domain: &str,
    ) -> Result<(), OrchestratorError> {
        let c = self.mkt_conn()?;
        c.execute(
            "UPDATE node_marketplaces SET publisher_domain = ?1
             WHERE preconfigured = 1 AND publisher_domain != ?1",
            params![domain],
        )?;
        Ok(())
    }

    /// True when a catalog row already lives under `domain` (`urn:vam:<kind>:<domain>/…`), in any
    /// kind. A marketplace may not claim a domain the org already publishes under.
    pub fn catalog_has_domain(&self, domain: &str) -> Result<bool, OrchestratorError> {
        // `domain` is a validated registry domain (letters, digits, '-', '.'), so it holds no LIKE
        // wildcard.
        if !super::is_domain_name(domain) {
            return Err(OrchestratorError::InvalidContract(format!(
                "`{domain}` is not a domain"
            )));
        }
        let c = self.mkt_conn()?;
        for table in [
            "agents",
            "playbooks",
            "skills",
            "policies",
            "skill_packages",
        ] {
            let found: Option<i64> = c
                .query_row(
                    &format!("SELECT 1 FROM {table} WHERE urn LIKE ?1 LIMIT 1"),
                    params![format!("urn:vam:%:{domain}/%")],
                    |r| r.get(0),
                )
                .optional()?;
            if found.is_some() {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn insert_pins(
    tx: &rusqlite::Transaction<'_>,
    id: &str,
    pins: &[(String, String)],
    now: i64,
) -> Result<(), OrchestratorError> {
    for (tag, commit) in pins {
        // A pin is written once and never replaced (M5).
        tx.execute(
            "INSERT OR IGNORE INTO node_marketplace_tags (marketplace_id, tag, commit_sha, first_seen_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![id, tag, commit, now],
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(name: &str, pins: &[(&str, &str)]) -> RefreshData {
        RefreshData {
            name: name.into(),
            index_digest: "d".into(),
            head_commit: "a".repeat(40),
            listings: vec![],
            tags_seen: vec![],
            new_pins: pins
                .iter()
                .map(|(t, c)| (t.to_string(), c.to_string()))
                .collect(),
        }
    }

    #[test]
    fn the_squrell_marketplace_is_there_from_the_first_boot_and_survives_a_disable() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let rows = store.marketplaces().unwrap();
        assert_eq!(rows.len(), 1);
        let s = &rows[0];
        assert_eq!(s.id, "squrell");
        assert_eq!(s.url, "https://github.com/squrell-inc/vam-marketplace");
        assert_eq!(s.trust, Trust::Squrell);
        assert!(s.preconfigured && s.enabled);
        assert_eq!(s.publisher_domain, "hub.squrell.cloud");
        assert!(store.set_marketplace_enabled("squrell", false).unwrap());
        // Seeding again (a restart) does not undo the disable.
        {
            let c = store.connection.lock().unwrap();
            initialize_schema(&c).unwrap();
        }
        assert!(!store.marketplace("squrell").unwrap().unwrap().enabled);
        // It can be disabled, never removed.
        assert!(!store.delete_marketplace("squrell").unwrap());
        assert!(store.marketplace("squrell").unwrap().is_some());
    }

    #[test]
    fn an_added_marketplace_is_unverified_and_unique_by_url() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .insert_marketplace(
                "m-1",
                "https://git.example.com/a/b",
                "a.example.com",
                "u1",
                10,
                &data("A", &[("t1", &"b".repeat(40))]),
            )
            .unwrap();
        let row = store.marketplace("m-1").unwrap().unwrap();
        assert_eq!(row.trust, Trust::Unverified);
        assert!(!row.preconfigured && row.enabled);
        assert_eq!(row.last_checked_at, Some(10));
        assert_eq!(row.name, "A");
        assert_eq!(store.marketplaces().unwrap()[0].id, "squrell");
        let dup = store.insert_marketplace(
            "m-2",
            "https://git.example.com/a/b",
            "c.example.com",
            "u1",
            11,
            &data("B", &[]),
        );
        assert!(
            matches!(dup, Err(OrchestratorError::Conflict(_))),
            "{dup:?}"
        );
        assert!(store.delete_marketplace("m-1").unwrap());
        assert!(store.marketplace_pins("m-1").unwrap().is_empty());
    }

    #[test]
    fn a_pin_is_written_once() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let (c1, c2) = ("1".repeat(40), "2".repeat(40));
        store
            .insert_marketplace(
                "m-1",
                "https://git.example.com/a/b",
                "a.example.com",
                "u",
                1,
                &data("A", &[("v1", &c1)]),
            )
            .unwrap();
        store
            .record_marketplace_read(
                "m-1",
                2,
                Some(&data("A", &[("v1", &c2), ("v2", &c2)])),
                None,
            )
            .unwrap();
        let pins = store.marketplace_pins("m-1").unwrap();
        assert_eq!(pins["v1"], c1, "the first commit stays pinned");
        assert_eq!(pins["v2"], c2);
        assert_eq!(store.marketplace_pin("m-1", "v1").unwrap(), Some(c1));
    }

    #[test]
    fn a_failed_read_keeps_the_last_good_listings() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let mut d = data("A", &[]);
        d.listings = vec![ListingState {
            id: "l".into(),
            display_name: "L".into(),
            description: "d".into(),
            connector: None,
            git_ref: "v1".into(),
            requires_node: ">=0.1.0".into(),
            commit: None,
            artifacts: vec![],
            summary: None,
            error: None,
        }];
        store
            .insert_marketplace(
                "m-1",
                "https://git.example.com/a/b",
                "a.example.com",
                "u",
                1,
                &d,
            )
            .unwrap();
        store
            .record_marketplace_read("m-1", 5, None, Some("index is broken"))
            .unwrap();
        let row = store.marketplace("m-1").unwrap().unwrap();
        assert_eq!(row.last_error.as_deref(), Some("index is broken"));
        assert_eq!(row.last_checked_at, Some(5));
        assert_eq!(row.listings.len(), 1);
        store
            .record_marketplace_read("m-1", 6, Some(&d), None)
            .unwrap();
        assert!(store
            .marketplace("m-1")
            .unwrap()
            .unwrap()
            .last_error
            .is_none());
    }

    #[test]
    fn a_domain_with_catalog_rows_is_detected() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        assert!(!store
            .catalog_has_domain("market.community.example")
            .unwrap());
        assert!(store.catalog_has_domain("bad domain").is_err());
    }

    #[test]
    fn the_preconfigured_domain_follows_the_node_setting() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .sync_preconfigured_marketplace_domain("hub.staging.example")
            .unwrap();
        assert_eq!(
            store
                .marketplace("squrell")
                .unwrap()
                .unwrap()
                .publisher_domain,
            "hub.staging.example"
        );
    }
}
