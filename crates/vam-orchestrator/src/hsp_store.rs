//! SQLite persistence for operator-approved HSP capability manifests.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, OptionalExtension};
use vam_spec::encoding::{json_to_proto, proto_to_json};
use vam_spec::v1::CapabilityManifest;
use vam_spec::validation::{host_tool_digest, Validate};

use crate::error::OrchestratorError;
use crate::hsp::HspEndpoint;
use crate::session::ToolCatalogEntry;
use crate::workflow::SqliteWorkflowStore;

const MANIFEST_PROTO: &str = "vam.v1.CapabilityManifest";

/// Stored ListTools snapshot row for a broker-exported tool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HspBrokerToolSnapshot {
    pub tool_name: String,
    pub adapter_id: String,
    pub side_effect: i32,
    pub description: String,
    pub input_schema_json: String,
}

/// Stored approval row deserialized from SQLite.
#[derive(Debug, Clone, PartialEq)]
pub struct HspBrokerApproval {
    pub manifest: CapabilityManifest,
    pub enabled: bool,
}

impl SqliteWorkflowStore {
    pub fn initialize_hsp_broker_approvals_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS hsp_broker_approvals (
                broker_id TEXT PRIMARY KEY,
                trust_tier INTEGER NOT NULL,
                manifest_json TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                updated_at INTEGER NOT NULL,
                endpoint_kind TEXT,
                endpoint_addr TEXT
            );

            CREATE TABLE IF NOT EXISTS hsp_broker_tools (
                broker_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                adapter_id TEXT NOT NULL,
                side_effect INTEGER NOT NULL,
                description TEXT NOT NULL DEFAULT '',
                input_schema_json TEXT NOT NULL DEFAULT '{}',
                PRIMARY KEY (broker_id, tool_name)
            );
            ",
        )?;
        // Additive migrations for DBs created before Phase 4 endpoint columns.
        let _ = connection.execute(
            "ALTER TABLE hsp_broker_approvals ADD COLUMN endpoint_kind TEXT",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE hsp_broker_approvals ADD COLUMN endpoint_addr TEXT",
            [],
        );
        // The trigger gateway (E5): the socket path while it is open, and the one uid allowed
        // to connect (NULL = no check; 0 = root is enforced like any other value).
        let _ = connection.execute(
            "ALTER TABLE hsp_broker_approvals ADD COLUMN gateway_socket TEXT",
            [],
        );
        let _ = connection.execute(
            "ALTER TABLE hsp_broker_approvals ADD COLUMN peer_uid INTEGER",
            [],
        );
        Ok(())
    }

    /// Validate and upsert an operator-approved capability manifest.
    pub fn upsert_hsp_broker_approval(
        &self,
        manifest: &CapabilityManifest,
    ) -> Result<(), OrchestratorError> {
        manifest
            .validate()
            .map_err(|error| OrchestratorError::InvalidContract(error.to_string()))?;
        let manifest_json =
            proto_to_json(MANIFEST_PROTO, manifest).map_err(OrchestratorError::InvalidContract)?;
        let updated_at = unix_now_secs()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "
            INSERT INTO hsp_broker_approvals (broker_id, trust_tier, manifest_json, enabled, updated_at)
            VALUES (?1, ?2, ?3, 1, ?4)
            ON CONFLICT(broker_id) DO UPDATE SET
                trust_tier = excluded.trust_tier,
                manifest_json = excluded.manifest_json,
                enabled = 1,
                updated_at = excluded.updated_at
            ",
            params![
                manifest.broker_id,
                manifest.trust_tier,
                manifest_json,
                updated_at
            ],
        )?;
        Ok(())
    }

    /// Load an approval row by broker id.
    pub fn get_hsp_broker_approval(
        &self,
        broker_id: &str,
    ) -> Result<Option<HspBrokerApproval>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = connection
            .query_row(
                "
                SELECT manifest_json, enabled
                FROM hsp_broker_approvals
                WHERE broker_id = ?1
                ",
                [broker_id],
                |row| {
                    let manifest_json: String = row.get(0)?;
                    let enabled: i64 = row.get(1)?;
                    Ok((manifest_json, enabled))
                },
            )
            .optional()?;
        let Some((manifest_json, enabled)) = row else {
            return Ok(None);
        };
        let manifest = json_to_proto(MANIFEST_PROTO, &manifest_json)
            .map_err(OrchestratorError::InvalidContract)?;
        Ok(Some(HspBrokerApproval {
            manifest,
            enabled: enabled != 0,
        }))
    }

    /// Disable an approval without deleting the manifest snapshot.
    /// Clears the stored broker endpoint so invoke cannot dial a disabled broker.
    pub fn disable_hsp_broker_approval(&self, broker_id: &str) -> Result<(), OrchestratorError> {
        let updated_at = unix_now_secs()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "
            UPDATE hsp_broker_approvals
            SET enabled = 0,
                endpoint_kind = NULL,
                endpoint_addr = NULL,
                gateway_socket = NULL,
                updated_at = ?2
            WHERE broker_id = ?1
            ",
            params![broker_id, updated_at],
        )?;
        connection.execute(
            "DELETE FROM hsp_broker_tools WHERE broker_id = ?1",
            [broker_id],
        )?;
        Ok(())
    }

    /// Records the gateway socket path (None when closed) and the allowed peer uid (None = no
    /// check) of an approval.
    pub fn set_hsp_broker_gateway(
        &self,
        broker_id: &str,
        socket: Option<&Path>,
        peer_uid: Option<i64>,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE hsp_broker_approvals SET gateway_socket = ?2, peer_uid = ?3 WHERE broker_id = ?1",
            params![
                broker_id,
                socket.map(|p| p.to_string_lossy().into_owned()),
                peer_uid
            ],
        )?;
        Ok(())
    }

    /// Only the socket path column (the uid stays).
    pub fn set_hsp_broker_gateway_socket(
        &self,
        broker_id: &str,
        socket: Option<&Path>,
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "UPDATE hsp_broker_approvals SET gateway_socket = ?2 WHERE broker_id = ?1",
            params![broker_id, socket.map(|p| p.to_string_lossy().into_owned())],
        )?;
        Ok(())
    }

    /// The gateway socket path (while open) and the allowed peer uid of an approval.
    pub fn get_hsp_broker_gateway(
        &self,
        broker_id: &str,
    ) -> Result<(Option<String>, Option<i64>), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = connection
            .query_row(
                "SELECT gateway_socket, peer_uid FROM hsp_broker_approvals WHERE broker_id = ?1",
                [broker_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        Ok(row.unwrap_or((None, None)))
    }

    /// Persist the broker dial target after a successful admitted Register.
    pub fn set_hsp_broker_endpoint(
        &self,
        broker_id: &str,
        endpoint: &HspEndpoint,
    ) -> Result<(), OrchestratorError> {
        let (kind, addr) = match endpoint {
            HspEndpoint::Unix(path) => ("unix", path.to_string_lossy().into_owned()),
            HspEndpoint::Tcp(socket) => ("tcp", socket.to_string()),
        };
        let updated_at = unix_now_secs()?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let rows = connection.execute(
            "
            UPDATE hsp_broker_approvals
            SET endpoint_kind = ?2, endpoint_addr = ?3, updated_at = ?4
            WHERE broker_id = ?1
            ",
            params![broker_id, kind, addr, updated_at],
        )?;
        if rows == 0 {
            return Err(OrchestratorError::Hsp(format!(
                "hsp denied: no approval row for endpoint ({broker_id})"
            )));
        }
        Ok(())
    }

    /// Load the dial target for an approved broker, if one was recorded at Register.
    pub fn get_hsp_broker_endpoint(
        &self,
        broker_id: &str,
    ) -> Result<Option<HspEndpoint>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row = connection
            .query_row(
                "
                SELECT endpoint_kind, endpoint_addr
                FROM hsp_broker_approvals
                WHERE broker_id = ?1
                ",
                [broker_id],
                |row| {
                    let kind: Option<String> = row.get(0)?;
                    let addr: Option<String> = row.get(1)?;
                    Ok((kind, addr))
                },
            )
            .optional()?;
        let Some((Some(kind), Some(addr))) = row else {
            return Ok(None);
        };
        match kind.as_str() {
            "unix" => Ok(Some(HspEndpoint::Unix(PathBuf::from(addr)))),
            "tcp" => {
                let socket: SocketAddr = addr.parse().map_err(|err| {
                    OrchestratorError::CorruptState(format!(
                        "invalid hsp tcp endpoint '{addr}': {err}"
                    ))
                })?;
                Ok(Some(HspEndpoint::Tcp(socket)))
            }
            other => Err(OrchestratorError::CorruptState(format!(
                "unknown hsp endpoint_kind '{other}'"
            ))),
        }
    }

    /// Broker ids with an enabled operator approval.
    pub fn list_enabled_hsp_broker_ids(&self) -> Result<Vec<String>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "
            SELECT broker_id
            FROM hsp_broker_approvals
            WHERE enabled = 1
            ORDER BY broker_id
            ",
        )?;
        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::from)
    }

    /// Replace the ListTools snapshot for a broker after a successful catalog refresh.
    pub fn replace_hsp_broker_tools(
        &self,
        broker_id: &str,
        entries: &[HspBrokerToolSnapshot],
    ) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "DELETE FROM hsp_broker_tools WHERE broker_id = ?1",
            [broker_id],
        )?;
        for entry in entries {
            connection.execute(
                "
                INSERT INTO hsp_broker_tools (
                    broker_id, tool_name, adapter_id, side_effect, description, input_schema_json
                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                ",
                params![
                    broker_id,
                    entry.tool_name,
                    entry.adapter_id,
                    entry.side_effect,
                    entry.description,
                    entry.input_schema_json,
                ],
            )?;
        }
        Ok(())
    }

    /// Load persisted ListTools rows for a broker.
    pub fn list_hsp_broker_tools(
        &self,
        broker_id: &str,
    ) -> Result<Vec<HspBrokerToolSnapshot>, OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let mut stmt = connection.prepare(
            "
            SELECT tool_name, adapter_id, side_effect, description, input_schema_json
            FROM hsp_broker_tools
            WHERE broker_id = ?1
            ORDER BY tool_name
            ",
        )?;
        let rows = stmt.query_map([broker_id], |row| {
            Ok(HspBrokerToolSnapshot {
                tool_name: row.get(0)?,
                adapter_id: row.get(1)?,
                side_effect: row.get(2)?,
                description: row.get(3)?,
                input_schema_json: row.get(4)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(OrchestratorError::from)
    }

    /// The approved manifest of `broker_id` carries `vam.capability = web`.
    /// The label is part of the operator's approval, so a broker cannot label
    /// itself.
    pub fn hsp_broker_is_web(&self, broker_id: &str) -> bool {
        matches!(
            self.get_hsp_broker_approval(broker_id),
            Ok(Some(approval)) if crate::hsp_registry::manifest_is_web(&approval.manifest)
        )
    }

    /// An enabled web broker with at least one registered tool exists.
    pub fn web_broker_available(&self) -> bool {
        self.list_enabled_hsp_broker_ids()
            .unwrap_or_default()
            .into_iter()
            .filter(|id| self.hsp_broker_is_web(id))
            .any(|id| {
                self.list_hsp_broker_tools(&id)
                    .map(|tools| !tools.is_empty())
                    .unwrap_or(false)
            })
    }

    /// Build the hosted overlay for session catalog seed from enabled broker
    /// snapshots. A web broker's tools are included only when the session's
    /// agent has web on (`web_enabled`, AR-1.7).
    pub fn list_hsp_catalog_entries(
        &self,
        web_enabled: bool,
    ) -> Result<Vec<ToolCatalogEntry>, OrchestratorError> {
        let rows: Vec<(String, ToolCatalogEntry)> = {
            let connection = self
                .connection
                .lock()
                .map_err(|_| OrchestratorError::StorageUnavailable)?;
            let mut stmt = connection.prepare(
                "
                SELECT t.broker_id, t.tool_name, t.adapter_id, t.side_effect
                FROM hsp_broker_tools t
                INNER JOIN hsp_broker_approvals a
                    ON a.broker_id = t.broker_id AND a.enabled = 1
                ORDER BY t.broker_id, t.tool_name
                ",
            )?;
            let mapped = stmt.query_map([], |row| {
                let broker_id: String = row.get(0)?;
                let tool_name: String = row.get(1)?;
                let adapter_id: String = row.get(2)?;
                let side_effect: i32 = row.get(3)?;
                Ok((
                    broker_id,
                    ToolCatalogEntry {
                        tool_name,
                        command: "host".to_string(),
                        entrypoint: adapter_id.clone(),
                        digest: host_tool_digest(&adapter_id),
                        side_effect,
                        skill_name: String::new(),
                        approval_required: false,
                    },
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        Ok(rows
            .into_iter()
            .filter(|(broker_id, _)| web_enabled || !self.hsp_broker_is_web(broker_id))
            .map(|(_, entry)| entry)
            .collect())
    }
}

/// Materialize synthetic Drive9 skills for enabled HSP brokers under `skills_dir`.
///
/// Writes `hosted-<broker_id>/manifest.json` + `SKILLS.md` so guest `scan_catalog`
/// can list the skill. Host catalog merge still uses `list_hsp_catalog_entries`.
///
/// The manifests are `"preloaded": true`: the guest registers the tools at
/// boot and the model sees them on turn 1 with no `load_skill` (AR-1.7). A web
/// broker is seeded only when `web_enabled`; otherwise a leftover
/// `hosted-<web broker>` directory is removed.
pub fn seed_hsp_skills(
    skills_dir: &Path,
    store: &SqliteWorkflowStore,
    web_enabled: bool,
) -> Result<(), OrchestratorError> {
    std::fs::create_dir_all(skills_dir).map_err(|err| {
        OrchestratorError::Core(format!("failed to create skills dir for HSP seed: {err}"))
    })?;

    for broker_id in store.list_enabled_hsp_broker_ids()? {
        if !web_enabled && store.hsp_broker_is_web(&broker_id) {
            let stale = skills_dir.join(format!("hosted-{broker_id}"));
            if stale.is_dir() {
                std::fs::remove_dir_all(&stale).map_err(|err| {
                    OrchestratorError::Core(format!(
                        "failed to remove web skill dir '{}': {err}",
                        stale.display()
                    ))
                })?;
            }
            continue;
        }
        let tools = store.list_hsp_broker_tools(&broker_id)?;
        if tools.is_empty() {
            continue;
        }

        let skill_dir = skills_dir.join(format!("hosted-{broker_id}"));
        std::fs::create_dir_all(&skill_dir).map_err(|err| {
            OrchestratorError::Core(format!(
                "failed to create HSP skill dir '{}': {err}",
                skill_dir.display()
            ))
        })?;

        let tools_json: Vec<serde_json::Value> = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool.tool_name,
                    "description": tool.description,
                    "input_schema_json": tool.input_schema_json,
                    "side_effect": tool.side_effect,
                    "host_exec": {
                        "adapter_id": tool.adapter_id,
                    },
                })
            })
            .collect();
        // The guest registers these tools at boot; no `load_skill` call.
        let manifest = serde_json::json!({ "preloaded": true, "tools": tools_json });
        let manifest_path = skill_dir.join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).map_err(|err| {
                OrchestratorError::Core(format!("HSP skill manifest serialize failed: {err}"))
            })?,
        )
        .map_err(|err| {
            OrchestratorError::Core(format!(
                "failed to write '{}': {err}",
                manifest_path.display()
            ))
        })?;

        let tool_list = tools
            .iter()
            .map(|tool| {
                let desc = if tool.description.is_empty() {
                    String::new()
                } else {
                    format!(" — {}", tool.description)
                };
                format!("- `{}`{}", tool.tool_name, desc)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let skills_md = format!(
            "---\nname: \"hosted-{broker_id}\"\ndescription: \"HSP broker tools for {broker_id}\"\n---\n\n# hosted-{broker_id}\n\nHosted tools from broker `{broker_id}`. These tools are already available; call them by name.\n\n{tool_list}\n"
        );
        let skills_md_path = skill_dir.join("SKILLS.md");
        std::fs::write(&skills_md_path, skills_md).map_err(|err| {
            OrchestratorError::Core(format!(
                "failed to write '{}': {err}",
                skills_md_path.display()
            ))
        })?;
    }

    Ok(())
}

fn unix_now_secs() -> Result<i64, OrchestratorError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .map_err(|_| OrchestratorError::CorruptState("system clock before unix epoch".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::{HookParticipation, ManifestSecretBinding, ToolExport};

    fn sample_manifest(broker_id: &str) -> CapabilityManifest {
        CapabilityManifest {
            broker_id: broker_id.to_string(),
            trust_tier: 2,
            secret_bindings: vec![ManifestSecretBinding {
                logical_name: "api-token".to_string(),
                vault_key: "vault/files/api-token".to_string(),
                required: true,
            }],
            egress_domains: vec!["files.example.com".to_string()],
            hooks: vec![HookParticipation {
                phase: 1,
                required: true,
            }],
            tools: vec![ToolExport {
                tool_name: "files.get".to_string(),
                side_effect: 1,
                required_secrets: vec![],
                destination_policy: Default::default(),
                effect: None,
            }],
            depends_on: vec![],
            labels: Default::default(),
            identity_requirement: None,
            run_request_permission: None,
            notification_channel: None,
        }
    }

    #[test]
    fn upsert_and_load_round_trip() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let manifest = sample_manifest("vam.files");
        store.upsert_hsp_broker_approval(&manifest).unwrap();

        let loaded = store
            .get_hsp_broker_approval("vam.files")
            .unwrap()
            .expect("approval row");
        assert!(loaded.enabled);
        assert_eq!(loaded.manifest.broker_id, "vam.files");
        assert_eq!(loaded.manifest.tools.len(), 1);
    }

    #[test]
    fn disable_marks_row_disabled() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&sample_manifest("vam.files"))
            .unwrap();
        store.disable_hsp_broker_approval("vam.files").unwrap();

        let loaded = store
            .get_hsp_broker_approval("vam.files")
            .unwrap()
            .expect("approval row");
        assert!(!loaded.enabled);
    }

    #[test]
    fn replace_and_list_broker_tools_round_trip() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&sample_manifest("echo"))
            .unwrap();
        let entries = vec![HspBrokerToolSnapshot {
            tool_name: "echo.ping".to_string(),
            adapter_id: "hosted:echo:echo.ping".to_string(),
            side_effect: 0,
            description: "Ping".to_string(),
            input_schema_json: "{}".to_string(),
        }];
        store.replace_hsp_broker_tools("echo", &entries).unwrap();

        let loaded = store.list_hsp_broker_tools("echo").unwrap();
        assert_eq!(loaded, entries);

        let catalog = store.list_hsp_catalog_entries(true).unwrap();
        assert_eq!(catalog.len(), 1);
        assert_eq!(catalog[0].tool_name, "echo.ping");
        assert_eq!(catalog[0].command, "host");
        assert_eq!(catalog[0].entrypoint, "hosted:echo:echo.ping");
    }

    #[test]
    fn disabled_approval_omits_catalog_entries() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&sample_manifest("echo"))
            .unwrap();
        store
            .replace_hsp_broker_tools(
                "echo",
                &[HspBrokerToolSnapshot {
                    tool_name: "echo.ping".to_string(),
                    adapter_id: "hosted:echo:echo.ping".to_string(),
                    side_effect: 0,
                    description: String::new(),
                    input_schema_json: "{}".to_string(),
                }],
            )
            .unwrap();
        store
            .set_hsp_broker_endpoint(
                "echo",
                &crate::hsp::HspEndpoint::Unix(std::path::PathBuf::from("/tmp/echo.sock")),
            )
            .unwrap();
        store.disable_hsp_broker_approval("echo").unwrap();

        assert!(store.list_hsp_broker_tools("echo").unwrap().is_empty());
        assert!(store.list_hsp_catalog_entries(true).unwrap().is_empty());
        assert!(store.get_hsp_broker_endpoint("echo").unwrap().is_none());
    }

    #[test]
    fn endpoint_round_trip_unix_and_tcp() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&sample_manifest("echo"))
            .unwrap();

        let unix = crate::hsp::HspEndpoint::Unix(std::path::PathBuf::from("/tmp/hsp-echo.sock"));
        store.set_hsp_broker_endpoint("echo", &unix).unwrap();
        assert_eq!(
            store.get_hsp_broker_endpoint("echo").unwrap().as_ref(),
            Some(&unix)
        );

        let tcp = crate::hsp::HspEndpoint::Tcp("127.0.0.1:9555".parse().unwrap());
        store.set_hsp_broker_endpoint("echo", &tcp).unwrap();
        assert_eq!(
            store.get_hsp_broker_endpoint("echo").unwrap().as_ref(),
            Some(&tcp)
        );
    }

    #[test]
    fn seed_hsp_skills_again_into_a_sealed_skills_dir() {
        // The directory backend seals `materialized/skills` read-only; the host seeds again after
        // materialize. Root ignores the mode bits, so this only failed for a non-root vamd (CI).
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&sample_manifest("echo"))
            .unwrap();
        store
            .replace_hsp_broker_tools(
                "echo",
                &[HspBrokerToolSnapshot {
                    tool_name: "echo.ping".to_string(),
                    adapter_id: "hosted:echo:echo.ping".to_string(),
                    side_effect: 0,
                    description: "Ping".to_string(),
                    input_schema_json: r#"{"type":"object"}"#.to_string(),
                }],
            )
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");
        seed_hsp_skills(&skills, &store, true).unwrap();
        // Seal like materialize does: files 0444, dirs 0555.
        use std::os::unix::fs::PermissionsExt;
        let skill = skills.join("hosted-echo");
        let set = |p: &std::path::Path, m| {
            std::fs::set_permissions(p, std::fs::Permissions::from_mode(m)).unwrap()
        };
        let mode_of = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode();
        set(&skill.join("manifest.json"), 0o444);
        set(&skill.join("SKILLS.md"), 0o444);
        set(&skill, 0o555);
        set(&skills, 0o555);

        vam_storage::with_skills_writable(&skills, || seed_hsp_skills(&skills, &store, true))
            .unwrap();
        // Sealed again afterwards.
        assert_eq!(
            mode_of(&skills) & 0o222,
            0,
            "skills dir must be sealed again"
        );
        assert_eq!(mode_of(&skill.join("manifest.json")) & 0o222, 0);
        // Let the tempdir clean up.
        set(&skill, 0o755);
        set(&skills, 0o755);
    }

    #[test]
    fn seed_hsp_skills_writes_manifest_and_skills_md() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        store
            .upsert_hsp_broker_approval(&sample_manifest("echo"))
            .unwrap();
        store
            .replace_hsp_broker_tools(
                "echo",
                &[HspBrokerToolSnapshot {
                    tool_name: "echo.ping".to_string(),
                    adapter_id: "hosted:echo:echo.ping".to_string(),
                    side_effect: 0,
                    description: "Ping".to_string(),
                    input_schema_json: r#"{"type":"object"}"#.to_string(),
                }],
            )
            .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let skills_dir = dir.path().join("skills");
        seed_hsp_skills(&skills_dir, &store, true).unwrap();

        let skill_dir = skills_dir.join("hosted-echo");
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(skill_dir.join("manifest.json")).unwrap(),
        )
        .unwrap();
        let tool = &manifest["tools"][0];
        assert_eq!(tool["name"], "echo.ping");
        assert_eq!(tool["host_exec"]["adapter_id"], "hosted:echo:echo.ping");
        assert_eq!(tool["input_schema_json"], r#"{"type":"object"}"#);

        let skills_md = std::fs::read_to_string(skill_dir.join("SKILLS.md")).unwrap();
        assert!(skills_md.contains("name: \"hosted-echo\""));
        assert!(skills_md.contains("description:"));
        assert!(skills_md.contains("`echo.ping`"));
        assert!(skills_md.contains("# hosted-echo"));
    }

    fn register_with_tool(store: &SqliteWorkflowStore, broker_id: &str, web: bool) {
        let mut manifest = sample_manifest(broker_id);
        if web {
            manifest
                .labels
                .insert("vam.capability".to_string(), "web".to_string());
        }
        store.upsert_hsp_broker_approval(&manifest).unwrap();
        store
            .replace_hsp_broker_tools(
                broker_id,
                &[HspBrokerToolSnapshot {
                    tool_name: format!("{}_tool", broker_id.replace('.', "_")),
                    adapter_id: format!("hosted:{broker_id}:t"),
                    side_effect: 1,
                    description: "d".to_string(),
                    input_schema_json: "{}".to_string(),
                }],
            )
            .unwrap();
    }

    #[test]
    fn web_brokers_are_hidden_from_the_catalog_unless_the_agent_has_web() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        register_with_tool(&store, "echo", false);
        register_with_tool(&store, "vam.web", true);
        assert!(store.hsp_broker_is_web("vam.web"));
        assert!(!store.hsp_broker_is_web("echo"));
        assert!(!store.hsp_broker_is_web("missing"));

        let off: Vec<String> = store
            .list_hsp_catalog_entries(false)
            .unwrap()
            .into_iter()
            .map(|e| e.tool_name)
            .collect();
        assert_eq!(off, vec!["echo_tool".to_string()]);
        let on = store.list_hsp_catalog_entries(true).unwrap();
        assert_eq!(on.len(), 2);
    }

    #[test]
    fn web_broker_available_needs_an_enabled_labelled_broker_with_tools() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        assert!(!store.web_broker_available());
        register_with_tool(&store, "echo", false);
        assert!(!store.web_broker_available());
        register_with_tool(&store, "vam.web", true);
        assert!(store.web_broker_available());
        store.disable_hsp_broker_approval("vam.web").unwrap();
        assert!(!store.web_broker_available());
    }

    #[test]
    fn seeded_hosted_skills_are_preloaded_and_web_skills_follow_the_switch() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        register_with_tool(&store, "echo", false);
        register_with_tool(&store, "vam.web", true);
        let dir = tempfile::tempdir().unwrap();
        let skills = dir.path().join("skills");

        seed_hsp_skills(&skills, &store, false).unwrap();
        assert!(skills.join("hosted-echo").is_dir());
        assert!(!skills.join("hosted-vam.web").exists());
        let echo: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(skills.join("hosted-echo/manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(echo["preloaded"], true);
        let md = std::fs::read_to_string(skills.join("hosted-echo/SKILLS.md")).unwrap();
        assert!(md.contains("already available"), "{md}");

        seed_hsp_skills(&skills, &store, true).unwrap();
        assert!(skills.join("hosted-vam.web/manifest.json").is_file());
        // Turning the switch off removes a leftover web skill.
        seed_hsp_skills(&skills, &store, false).unwrap();
        assert!(!skills.join("hosted-vam.web").exists());
        assert!(skills.join("hosted-echo").is_dir());
    }
}
