//! The connectors marketplaces installed (CN-4.11).
//!
//! A connector file in a listing is imported like the other artifacts (`import.rs`), into
//! `connector_releases` (the same release columns as the catalog tables) with a `catalog_origins`
//! row of kind `connector` for where it came from. The newest released row of a URN is the
//! definition the node uses; older releases stay.
//!
//! What is read back is checked again. A row that no longer parses, validates or passes the
//! limits of a marketplace connector (`vam_spec::connector_listing`) is not offered: the node fails
//! closed and a built-in connector always wins an id.

use rusqlite::{params, Connection};
use vam_spec::connector::{validate_connector, Connector};
use vam_spec::connector_listing::{check_marketplace_connector, connector_id_of_urn};

use super::installs::OriginRow;
use super::store::Trust;
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

/// A connector a marketplace installed: its definition and where it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct InstalledConnector {
    pub def: Connector,
    pub origin: OriginRow,
    /// The trust of the marketplace that installed it (`squrell` or `unverified`).
    pub trust: Trust,
    pub marketplace_name: String,
    pub publisher_domain: String,
}

const SELECT: &str = "SELECT r.urn, r.spec_bytes, o.marketplace_id, o.listing_id, o.git_ref,
        o.commit_sha, o.package_digest, m.trust, m.name, m.publisher_domain
     FROM connector_releases r
     JOIN catalog_origins o ON o.kind = 'connector' AND o.urn = r.urn
          AND o.major = r.major AND o.minor = r.minor AND o.patch = r.patch
     LEFT JOIN node_marketplaces m ON m.id = o.marketplace_id
     WHERE r.channel = 'released' AND r.prerelease = ''";
const ORDER: &str = " ORDER BY r.urn, r.major DESC, r.minor DESC, r.patch DESC";

type Row = (
    String,
    Vec<u8>,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

fn read(
    c: &Connection,
    sql: &str,
    args: &[&dyn rusqlite::ToSql],
) -> Result<Vec<Row>, OrchestratorError> {
    let mut stmt = c.prepare(sql)?;
    let rows = stmt
        .query_map(args, |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The newest release of each URN, as a definition the node may use; rows that do not pass are
/// left out.
fn definitions(rows: Vec<Row>) -> Vec<InstalledConnector> {
    let mut out: Vec<InstalledConnector> = Vec::new();
    let mut last_urn = String::new();
    for (urn, bytes, mid, listing, git_ref, commit, digest, trust, name, domain) in rows {
        // Newest first within a URN: only the first row of each URN is the definition.
        if urn == last_urn {
            continue;
        }
        last_urn = urn.clone();
        let def: Connector = match serde_json::from_slice(&bytes) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(urn, "installed connector does not parse, not offered: {e}");
                continue;
            }
        };
        if def.urn != urn
            || connector_id_of_urn(&def.urn).is_empty()
            || validate_connector(&def).is_err()
            || check_marketplace_connector(&def).is_err()
        {
            tracing::warn!(
                urn,
                "installed connector does not pass its checks, not offered"
            );
            continue;
        }
        out.push(InstalledConnector {
            def,
            origin: OriginRow {
                marketplace: mid,
                listing,
                git_ref,
                commit,
                package_digest: digest,
            },
            trust: trust.as_deref().map_or(Trust::Unverified, Trust::parse),
            marketplace_name: name.unwrap_or_default(),
            publisher_domain: domain.unwrap_or_default(),
        });
    }
    out
}

/// Every installed connector, by id.
pub fn installed_in(c: &Connection) -> Result<Vec<InstalledConnector>, OrchestratorError> {
    let rows = read(c, &format!("{SELECT}{ORDER}"), &[])?;
    let mut all = definitions(rows);
    all.sort_by(|a, b| connector_id_of_urn(&a.def.urn).cmp(connector_id_of_urn(&b.def.urn)));
    Ok(all)
}

/// The installed connector with this id, if there is one.
pub fn installed_by_id_in(
    c: &Connection,
    id: &str,
) -> Result<Option<InstalledConnector>, OrchestratorError> {
    let rows = read(
        c,
        &format!("{SELECT} AND substr(r.urn, -(length(?1) + 1)) = ('/' || ?1){ORDER}"),
        params![id],
    )?;
    Ok(definitions(rows).into_iter().next())
}

/// The ids of every connector installed (any release), for clash checks inside the import.
pub(crate) fn installed_urns_in(c: &Connection) -> Result<Vec<String>, OrchestratorError> {
    let mut stmt = c.prepare("SELECT DISTINCT urn FROM connector_releases")?;
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

impl SqliteWorkflowStore {
    fn connector_store_conn(
        &self,
    ) -> Result<std::sync::MutexGuard<'_, Connection>, OrchestratorError> {
        self.connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)
    }

    /// Every connector a marketplace installed.
    pub fn installed_connectors(&self) -> Result<Vec<InstalledConnector>, OrchestratorError> {
        installed_in(&*self.connector_store_conn()?)
    }
}
