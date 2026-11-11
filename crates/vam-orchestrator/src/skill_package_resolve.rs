use std::fs;
use std::path::{Path, PathBuf};

use vam_spec::v1::{AgentSpec, ResourceRef, SkillChannel, SkillPackageRecord, Version};
use vam_spec::validation::parse_skill_urn;

use crate::catalog_resolve::{resolve_ref_for_owner, ArtifactKind, ResolveMode};
use crate::error::OrchestratorError;
use crate::manager::drive9_root;
use crate::skill_package_admit::{migrate_legacy_skill_blob, resolve_skill_root, unpack_tar_gz};
use crate::skill_packages::skill_catalog_root;
use crate::workflow::SqliteWorkflowStore;

/// Deprecated sentinel: SHA-256 of `""`.
///
/// DEPRECATED: kept for backward compatibility so already-shipped specs that pinned
/// this value still resolve as unpinned. New specs omit `digest`, or leave `hex_hash`
/// empty. Both of those, and this sentinel, mean "follow the catalog digest". Every
/// check goes through `catalog_resolve::is_unpinned_digest`, the resolver's single
/// test for it. Remove this constant once AX-3.4 stops the app sending it.
pub const EMPTY_SHA256_HEX: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Resolves `agent.skill_refs` into `{session_dir}/catalog-skills/<name>/` staging dirs.
///
/// Floating refs resolve through [`resolve_ref_for_owner`], with the same `owner` and
/// `mode` the run recorded its pins with (see [`run_resolve_context`]), so a draft run
/// and its boot agree on the pin.
///
/// Returns `Some(staging_root)` when at least one ref was unpacked from the local catalog;
/// returns `None` when every ref falls back to seed overlay or the agent has no refs.
pub fn resolve_agent_skill_refs(
    store: &SqliteWorkflowStore,
    agent: &AgentSpec,
    session_dir: &Path,
    seed_dir: Option<&Path>,
    owner: &str,
    mode: ResolveMode,
) -> Result<Option<PathBuf>, OrchestratorError> {
    if agent.skill_refs.is_empty() {
        return Ok(None);
    }

    let staging_root = session_dir.join("catalog-skills");
    let mut unpacked_any = false;

    for skill_ref in &agent.skill_refs {
        if resolve_one_skill_ref(store, skill_ref, &staging_root, seed_dir, owner, mode)? {
            unpacked_any = true;
        }
    }

    if unpacked_any {
        Ok(Some(staging_root))
    } else {
        Ok(None)
    }
}

fn resolve_one_skill_ref(
    store: &SqliteWorkflowStore,
    skill_ref: &ResourceRef,
    staging_root: &Path,
    seed_dir: Option<&Path>,
    owner: &str,
    mode: ResolveMode,
) -> Result<bool, OrchestratorError> {
    let urn = skill_ref
        .resource_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .filter(|u| !u.is_empty())
        .ok_or_else(|| {
            OrchestratorError::InvalidContract(
                "AgentSpec.skill_refs entry missing resource_id.urn".to_string(),
            )
        })?;

    let (registry, _, name) =
        parse_skill_urn(urn).map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
    // Resolving admits the marketplace domains too: rows a marketplace installed live there
    // (and only the catalog import can put anything there).
    let trusted = store.marketplace_publisher_domains()?;
    if !crate::registry::registry_domains().admits_installed_skill_registry(&registry, &trusted) {
        return Err(OrchestratorError::InvalidContract(format!(
            "skill ref '{urn}' uses non-local registry '{registry}'; remote pull is not supported"
        )));
    }

    let version = skill_ref.version.as_ref();
    let floating = version.is_none();
    if floating {
        let pinned = resolve_ref_for_owner(store, ArtifactKind::Skill, skill_ref, owner, mode)?;
        let pinned_version = pinned.reference.version.as_ref();
        let package = store.get_skill_package_for_query_visible(
            urn,
            pinned_version,
            pinned.channel,
            owner,
            false,
        );
        return match package {
            Ok(package) => {
                unpack_package_to_staging(&package, staging_root, &name)?;
                Ok(true)
            }
            // A pin with no package behind it (legacy `skills` row) has nothing to unpack.
            Err(OrchestratorError::NotFound(_)) => Err(OrchestratorError::InvalidContract(
                format!("skill '{urn}' has no published version"),
            )),
            Err(err) => Err(err),
        };
    }

    let version = version.ok_or_else(|| {
        OrchestratorError::InvalidContract(format!("skill ref '{urn}' missing version pin"))
    })?;

    let channel = channel_for_ref_version(version)?;

    match lookup_and_validate_package(store, skill_ref, urn, version, channel) {
        Ok(package) => {
            unpack_package_to_staging(&package, staging_root, &name)?;
            Ok(true)
        }
        Err(OrchestratorError::NotFound(_)) => {
            if seed_has_skill(seed_dir, &name) {
                eprintln!(
                    "[orchestrator] skill '{urn}' not in catalog; falling back to seed skills/{name}/"
                );
                Ok(false)
            } else if hsp_synthetic_skill_available(store, &name)? {
                eprintln!(
                    "[orchestrator] skill '{urn}' not in catalog; using HSP synthetic skill {name}"
                );
                Ok(false)
            } else {
                Err(OrchestratorError::InvalidContract(format!(
                    "skill '{urn}' not found in catalog and no seed overlay at skills/{name}/"
                )))
            }
        }
        Err(err) => Err(err),
    }
}

fn channel_for_ref_version(version: &Version) -> Result<SkillChannel, OrchestratorError> {
    if version.prerelease == "draft" {
        Ok(SkillChannel::Draft)
    } else if version.prerelease.is_empty() {
        Ok(SkillChannel::Released)
    } else {
        Err(OrchestratorError::InvalidContract(format!(
            "skill ref version prerelease '{}' is not supported; use 'draft' or omit for released",
            version.prerelease
        )))
    }
}

fn lookup_and_validate_package(
    store: &SqliteWorkflowStore,
    skill_ref: &ResourceRef,
    urn: &str,
    version: &Version,
    channel: SkillChannel,
) -> Result<SkillPackageRecord, OrchestratorError> {
    let package = store.get_skill_package_for_query(urn, Some(version), channel)?;

    if channel == SkillChannel::Draft {
        return Ok(package);
    }

    let agent_digest = skill_ref
        .digest
        .as_ref()
        .map(|d| d.hex_hash.as_str())
        .unwrap_or(EMPTY_SHA256_HEX);

    if is_empty_digest(agent_digest) {
        eprintln!(
            "[orchestrator] skill ref '{urn}' digest is unpinned; following catalog digest {}",
            package.digest
        );
        return Ok(package);
    }

    if agent_digest != package.digest {
        return Err(OrchestratorError::InvalidContract(format!(
            "skill ref '{urn}' digest mismatch: agent pin {agent_digest}, catalog {}",
            package.digest
        )));
    }

    Ok(package)
}

fn is_empty_digest(hex: &str) -> bool {
    // Absent digests are mapped to [`EMPTY_SHA256_HEX`] before this check.
    crate::catalog_resolve::is_unpinned_digest(hex)
}

fn seed_has_skill(seed_dir: Option<&Path>, name: &str) -> bool {
    seed_dir
        .map(|seed| seed.join("skills").join(name))
        .map(|path| path.is_dir())
        .unwrap_or(false)
}

/// `hosted-<broker_id>` skills are generated by `seed_hsp_skills`, not catalog packages.
fn hsp_synthetic_skill_available(
    store: &SqliteWorkflowStore,
    skill_name: &str,
) -> Result<bool, OrchestratorError> {
    let Some(broker_id) = skill_name.strip_prefix("hosted-") else {
        return Ok(false);
    };
    if broker_id.is_empty() {
        return Ok(false);
    }
    Ok(!store.list_hsp_broker_tools(broker_id)?.is_empty())
}

fn unpack_package_to_staging(
    package: &SkillPackageRecord,
    staging_root: &Path,
    name: &str,
) -> Result<(), OrchestratorError> {
    let catalog_root = skill_catalog_root(&drive9_root());
    let blob_path = catalog_root.join(&package.blob_relpath);
    let blob_path = if blob_path.is_file() {
        blob_path
    } else {
        let owner = package
            .blob_relpath
            .split_once('/')
            .map(|(owner, _)| owner)
            .unwrap_or(crate::users::LOCAL_USER_ID);
        migrate_legacy_skill_blob(&catalog_root, owner, &package.digest)?
    };
    if !blob_path.is_file() {
        return Err(OrchestratorError::InvalidContract(format!(
            "skill package blob missing at {}",
            blob_path.display()
        )));
    }

    let archive_bytes = fs::read(&blob_path).map_err(OrchestratorError::Io)?;
    let temp_dir = tempfile::tempdir().map_err(OrchestratorError::Io)?;
    unpack_tar_gz(&archive_bytes, temp_dir.path())?;
    let skill_root = resolve_skill_root(temp_dir.path())?;

    let dest = staging_root.join(name);
    if dest.exists() {
        fs::remove_dir_all(&dest).map_err(OrchestratorError::Io)?;
    }
    fs::create_dir_all(staging_root).map_err(OrchestratorError::Io)?;
    copy_dir_all(&skill_root, &dest)?;
    Ok(())
}

fn copy_dir_all(src: &Path, dst: &Path) -> Result<(), OrchestratorError> {
    fs::create_dir_all(dst).map_err(OrchestratorError::Io)?;
    for entry in fs::read_dir(src).map_err(OrchestratorError::Io)? {
        let entry = entry.map_err(OrchestratorError::Io)?;
        let file_type = entry.file_type().map_err(OrchestratorError::Io)?;
        let target = dst.join(entry.file_name());
        if file_type.is_dir() {
            copy_dir_all(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), target).map_err(OrchestratorError::Io)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_packages::DRIVE9_ENV_LOCK;
    use crate::users::LOCAL_USER_ID;
    use std::io::{copy, Cursor};

    use flate2::write::GzEncoder;
    use flate2::Compression;
    use vam_spec::v1::digest::Algorithm;
    use vam_spec::v1::{
        CapabilityManifest, Digest, ResourceId, SkillManifest, SkillPackageRecord, ToolExport,
    };

    use crate::hsp_store::HspBrokerToolSnapshot;
    use crate::skill_packages::skill_blob_relpath;

    const ASSISTANT_URN: &str = "urn:vam:skill:vam.local/vam/assistant-tools";
    const REMOTE_URN: &str = "urn:vam:skill:hub.vam.dev/acme/assistant-tools";
    const DIGEST_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const DIGEST_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn write_tar_gz(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut tar_buf = Vec::new();
        {
            let mut builder = tar::Builder::new(&mut tar_buf);
            for (path, bytes) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(bytes.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                builder
                    .append_data(&mut header, *path, &mut Cursor::new(*bytes))
                    .expect("append tar entry");
            }
            builder.finish().expect("finish tar");
        }
        let mut gz = Vec::new();
        {
            let mut encoder = GzEncoder::new(&mut gz, Compression::default());
            copy(&mut Cursor::new(&tar_buf), &mut encoder).expect("gzip tar");
            encoder.finish().expect("finish gzip");
        }
        gz
    }

    fn minimal_manifest_json(urn: &str) -> String {
        format!(
            r#"{{
  "resourceId": {{"urn": "{urn}"}},
  "version": {{"major": 1, "minor": 0, "patch": 0}},
  "description": "Test skill",
  "tools": []
}}"#
        )
    }

    fn minimal_skills_md(urn: &str) -> String {
        format!(
            "---\nid: \"{urn}\"\nname: Test\nversion: \"1.0.0\"\ndescription: Test\n---\n\n# Body\n"
        )
    }

    fn fixture_archive(content: &str) -> Vec<u8> {
        write_tar_gz(&[
            ("SKILLS.md", minimal_skills_md(ASSISTANT_URN).as_bytes()),
            (
                "manifest.json",
                minimal_manifest_json(ASSISTANT_URN).as_bytes(),
            ),
            ("execute.js", content.as_bytes()),
        ])
    }

    fn package_record(digest: &str, channel: SkillChannel, prerelease: &str) -> SkillPackageRecord {
        SkillPackageRecord {
            resource_id: Some(ResourceId {
                urn: ASSISTANT_URN.to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: prerelease.to_string(),
                build: String::new(),
            }),
            channel: channel as i32,
            digest: digest.to_string(),
            blob_relpath: skill_blob_relpath(crate::users::LOCAL_USER_ID, digest),
            size_bytes: 64,
            spec: Some(SkillManifest {
                resource_id: Some(ResourceId {
                    urn: ASSISTANT_URN.to_string(),
                }),
                version: Some(Version {
                    major: 1,
                    minor: 0,
                    patch: 0,
                    prerelease: prerelease.to_string(),
                    build: String::new(),
                }),
                description: "Test".to_string(),
                system_extension: String::new(),
                tools: vec![],
                runtime_compatibility: None,
                metadata: None,
                locators: vec![],
                capabilities: vec![],
                policy: String::new(),
                playbooks: vec![],
                ..Default::default()
            }),
            metadata: None,
            ..Default::default()
        }
    }

    fn skill_ref(digest_hex: &str, prerelease: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: ASSISTANT_URN.to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: prerelease.to_string(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: Algorithm::Sha256 as i32,
                hex_hash: digest_hex.to_string(),
            }),
            locators: vec![],
        }
    }

    fn agent_with_refs(refs: Vec<ResourceRef>) -> AgentSpec {
        AgentSpec {
            skill_refs: refs,
            ..Default::default()
        }
    }

    fn store() -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("in-memory store");
        store.initialize_catalog_schema().expect("catalog schema");
        store
    }

    struct Drive9Fixture {
        _guard: std::sync::MutexGuard<'static, ()>,
        root: tempfile::TempDir,
    }

    impl Drive9Fixture {
        fn new() -> Self {
            let guard = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let root = tempfile::tempdir().expect("tempdir");
            std::env::set_var("VAM_DRIVE9_ROOT", root.path());
            Self {
                _guard: guard,
                root,
            }
        }

        fn write_blob(&self, digest: &str, archive: &[u8]) {
            let path = skill_catalog_root(self.root.path())
                .join(skill_blob_relpath(crate::users::LOCAL_USER_ID, digest));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).expect("mkdir blob parent");
            }
            fs::write(path, archive).expect("write blob");
        }
    }

    fn real_digest_for(content: &str) -> String {
        let temp = tempfile::tempdir().expect("tempdir");
        let skill_dir = temp.path().join("skill");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(
            skill_dir.join("SKILLS.md"),
            minimal_skills_md(ASSISTANT_URN),
        )
        .expect("skills");
        fs::write(
            skill_dir.join("manifest.json"),
            minimal_manifest_json(ASSISTANT_URN),
        )
        .expect("manifest");
        fs::write(skill_dir.join("execute.js"), content).expect("execute");
        vam_storage::skills_digest_from_dir(&skill_dir)
    }

    #[test]
    fn draft_follows_replaced_catalog_digest() {
        let _drive9 = Drive9Fixture::new();
        let store = store();
        let digest_a = real_digest_for("v1");
        let digest_b = real_digest_for("v2");
        store
            .put_skill_package(&package_record(&digest_a, SkillChannel::Draft, "draft"))
            .expect("first draft");
        store
            .put_skill_package(&package_record(&digest_b, SkillChannel::Draft, "draft"))
            .expect("replace draft");

        _drive9.write_blob(&digest_b, &fixture_archive("v2"));

        let session = tempfile::tempdir().expect("session");
        let agent = agent_with_refs(vec![skill_ref(&digest_a, "draft")]);
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("resolve")
        .expect("staging");

        let execute = staging.join("assistant-tools/execute.js");
        assert!(execute.is_file());
        assert_eq!(fs::read_to_string(execute).expect("read"), "v2");
    }

    #[test]
    fn local_vam_dev_registry_skill_ref_resolves() {
        const ORG_URN: &str = "urn:vam:skill:local.vam.dev/vam/assistant-tools";
        let _drive9 = Drive9Fixture::new();
        let store = store();
        let digest = real_digest_for("org-registry");
        let mut record = package_record(&digest, SkillChannel::Draft, "draft");
        record.resource_id = Some(ResourceId {
            urn: ORG_URN.to_string(),
        });
        if let Some(spec) = record.spec.as_mut() {
            spec.resource_id = Some(ResourceId {
                urn: ORG_URN.to_string(),
            });
        }
        store.put_skill_package(&record).expect("draft");
        _drive9.write_blob(&digest, &fixture_archive("org-registry"));

        let mut skill = skill_ref(&digest, "draft");
        skill.resource_id = Some(ResourceId {
            urn: ORG_URN.to_string(),
        });
        let agent = agent_with_refs(vec![skill]);
        let session = tempfile::tempdir().expect("session");
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("local.vam.dev is a local registry")
        .expect("staging");

        let execute = staging.join("assistant-tools/execute.js");
        assert_eq!(fs::read_to_string(execute).expect("read"), "org-registry");
    }

    #[test]
    fn released_digest_mismatch_fails() {
        let store = store();
        store
            .put_skill_package(&package_record(DIGEST_A, SkillChannel::Released, ""))
            .expect("released");

        let agent = agent_with_refs(vec![skill_ref(DIGEST_B, "")]);
        let session = tempfile::tempdir().expect("session");
        let err = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect_err("mismatch");
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    }

    #[test]
    fn empty_digest_follows_released_catalog() {
        let _drive9 = Drive9Fixture::new();
        let store = store();
        store
            .put_skill_package(&package_record(DIGEST_A, SkillChannel::Released, ""))
            .expect("released");
        _drive9.write_blob(DIGEST_A, &fixture_archive("released-body"));

        let agent = agent_with_refs(vec![skill_ref(EMPTY_SHA256_HEX, "")]);
        let session = tempfile::tempdir().expect("session");
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("resolve")
        .expect("staging");
        assert!(staging.join("assistant-tools/execute.js").is_file());
    }

    #[test]
    fn empty_hex_follows_released_catalog() {
        let _drive9 = Drive9Fixture::new();
        let store = store();
        store
            .put_skill_package(&package_record(DIGEST_A, SkillChannel::Released, ""))
            .expect("released");
        _drive9.write_blob(DIGEST_A, &fixture_archive("released-body"));

        let agent = agent_with_refs(vec![skill_ref("", "")]);
        let session = tempfile::tempdir().expect("session");
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("empty hex is unpinned")
        .expect("staging");
        assert!(staging.join("assistant-tools/execute.js").is_file());
    }

    #[test]
    fn missing_catalog_falls_back_to_seed() {
        let store = store();
        let seed = tempfile::tempdir().expect("seed");
        let skill_seed = seed.path().join("skills/assistant-tools");
        fs::create_dir_all(&skill_seed).expect("mkdir seed skill");
        fs::write(skill_seed.join("execute.js"), b"seed").expect("write");

        let agent = agent_with_refs(vec![skill_ref(EMPTY_SHA256_HEX, "draft")]);
        let session = tempfile::tempdir().expect("session");
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            Some(seed.path()),
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("resolve");
        assert!(staging.is_none());
    }

    #[test]
    fn missing_catalog_and_seed_fails() {
        let store = store();
        let agent = agent_with_refs(vec![skill_ref(EMPTY_SHA256_HEX, "draft")]);
        let session = tempfile::tempdir().expect("session");
        let err = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect_err("missing");
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    }

    #[test]
    fn missing_catalog_falls_back_to_hsp_synthetic_skill() {
        const HSP_URN: &str = "urn:vam:skill:examples.local/vam/hosted-echo-ts";
        let store = store();
        store
            .upsert_hsp_broker_approval(&CapabilityManifest {
                broker_id: "echo-ts".to_string(),
                trust_tier: 2,
                secret_bindings: vec![],
                egress_domains: vec![],
                hooks: vec![],
                tools: vec![ToolExport {
                    tool_name: "echo.ping".to_string(),
                    side_effect: 0,
                    required_secrets: vec![],
                    destination_policy: Default::default(),
                    effect: None,
                }],
                depends_on: vec![],
                labels: Default::default(),
                identity_requirement: None,
                run_request_permission: None,
                notification_channel: None,
            })
            .expect("approve echo-ts");
        store
            .replace_hsp_broker_tools(
                "echo-ts",
                &[HspBrokerToolSnapshot {
                    tool_name: "echo.ping".to_string(),
                    adapter_id: "hosted:echo-ts:echo.ping".to_string(),
                    side_effect: 0,
                    description: "Ping".to_string(),
                    input_schema_json: "{}".to_string(),
                }],
            )
            .expect("snapshot tools");

        let agent = agent_with_refs(vec![ResourceRef {
            resource_id: Some(ResourceId {
                urn: HSP_URN.to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: String::new(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: Algorithm::Sha256 as i32,
                hex_hash: EMPTY_SHA256_HEX.to_string(),
            }),
            locators: vec![],
        }]);
        let session = tempfile::tempdir().expect("session");
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("resolve");
        assert!(staging.is_none());
    }

    #[test]
    fn non_local_registry_fails_closed() {
        let store = store();
        let agent = agent_with_refs(vec![ResourceRef {
            resource_id: Some(ResourceId {
                urn: REMOTE_URN.to_string(),
            }),
            version: Some(Version {
                major: 1,
                minor: 0,
                patch: 0,
                prerelease: "draft".to_string(),
                build: String::new(),
            }),
            digest: Some(Digest {
                algorithm: Algorithm::Sha256 as i32,
                hex_hash: EMPTY_SHA256_HEX.to_string(),
            }),
            locators: vec![],
        }]);
        let session = tempfile::tempdir().expect("session");
        let err = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect_err("remote");
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    }

    #[test]
    fn configured_org_registry_passes_the_registry_check() {
        let org = "urn:vam:skill:vam.squrell.dev/acme/assistant-tools";
        let mut agent = agent_with_refs(vec![skill_ref(EMPTY_SHA256_HEX, "draft")]);
        if let Some(id) = agent.skill_refs[0].resource_id.as_mut() {
            id.urn = org.to_string();
        }
        let session = tempfile::tempdir().expect("session");
        let resolve = || {
            resolve_agent_skill_refs(
                &store(),
                &agent,
                session.path(),
                None,
                LOCAL_USER_ID,
                ResolveMode::Release,
            )
            .expect_err("no such package in the empty catalog")
            .to_string()
        };
        crate::registry::install_registry_domains(
            vam_spec::RegistryDomains::new("vam.squrell.dev", None).expect("domains"),
        );
        let configured = resolve();
        crate::registry::install_registry_domains(vam_spec::RegistryDomains::default());
        let unconfigured = resolve();
        assert!(
            !configured.contains("non-local registry"),
            "org registry must pass the registry check: {configured}"
        );
        assert!(
            unconfigured.contains("non-local registry"),
            "same ref is refused on an unconfigured node: {unconfigured}"
        );
    }

    #[test]
    fn unpack_lands_at_skills_name_segment() {
        let _drive9 = Drive9Fixture::new();
        let store = store();
        let digest = real_digest_for("path-test");
        store
            .put_skill_package(&package_record(&digest, SkillChannel::Draft, "draft"))
            .expect("draft");
        _drive9.write_blob(&digest, &fixture_archive("path-test"));

        let agent = agent_with_refs(vec![skill_ref(EMPTY_SHA256_HEX, "draft")]);
        let session = tempfile::tempdir().expect("session");
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("resolve")
        .expect("staging");
        assert_eq!(staging, session.path().join("catalog-skills"));
        assert!(staging.join("assistant-tools/SKILLS.md").is_file());
        assert!(staging.join("assistant-tools/manifest.json").is_file());
    }

    #[test]
    fn draft_ref_at_any_numbers_loads_the_skill_draft() {
        let _drive9 = Drive9Fixture::new();
        let store = store();
        let digest = real_digest_for("v1");
        store
            .put_skill_package(&package_at(&digest, SkillChannel::Draft, 0, 1, 1, "draft"))
            .expect("draft 0.1.1");
        _drive9.write_blob(&digest, &fixture_archive("v1"));

        for (major, minor, patch) in [(0, 1, 1), (1, 0, 0), (9, 9, 9)] {
            let mut skill = skill_ref(&digest, "draft");
            if let Some(version) = skill.version.as_mut() {
                version.major = major;
                version.minor = minor;
                version.patch = patch;
            }
            let session = tempfile::tempdir().expect("session");
            let agent = agent_with_refs(vec![skill]);
            let staging = resolve_agent_skill_refs(
                &store,
                &agent,
                session.path(),
                None,
                LOCAL_USER_ID,
                ResolveMode::Release,
            )
            .unwrap_or_else(|e| panic!("{major}.{minor}.{patch}-draft: {e}"))
            .expect("staging");
            assert!(staging.join("assistant-tools/execute.js").is_file());
        }
    }

    #[test]
    fn resolve_migrates_legacy_blob_into_owner_namespace() {
        let _drive9 = Drive9Fixture::new();
        let store = store();
        let digest = real_digest_for("legacy-blob");
        store
            .put_skill_package(&package_record(&digest, SkillChannel::Draft, "draft"))
            .expect("draft");

        let catalog = skill_catalog_root(_drive9.root.path());
        let legacy = catalog.join(crate::skill_packages::skill_blob_filename(&digest));
        fs::create_dir_all(&catalog).expect("mkdir");
        fs::write(&legacy, fixture_archive("legacy-blob")).expect("legacy blob");

        let agent = agent_with_refs(vec![skill_ref(EMPTY_SHA256_HEX, "draft")]);
        let session = tempfile::tempdir().expect("session");
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("resolve")
        .expect("staging");
        assert!(staging.join("assistant-tools/execute.js").is_file());
        assert!(catalog
            .join(skill_blob_relpath(crate::users::LOCAL_USER_ID, &digest))
            .is_file());
    }

    fn unversioned_skill_ref() -> ResourceRef {
        ResourceRef {
            resource_id: Some(ResourceId {
                urn: ASSISTANT_URN.to_string(),
            }),
            version: None,
            digest: None,
            locators: vec![],
        }
    }

    fn package_at(
        digest: &str,
        channel: SkillChannel,
        major: i32,
        minor: i32,
        patch: i32,
        prerelease: &str,
    ) -> SkillPackageRecord {
        let mut record = package_record(digest, channel, prerelease);
        let version = Version {
            major,
            minor,
            patch,
            prerelease: prerelease.to_string(),
            build: String::new(),
        };
        record.version = Some(version.clone());
        if let Some(spec) = record.spec.as_mut() {
            spec.version = Some(version);
        }
        record
    }

    #[test]
    fn unversioned_skill_ref_resolves_latest_released_not_draft() {
        let _drive9 = Drive9Fixture::new();
        let store = store();
        let digest_old = real_digest_for("old");
        let digest_new = real_digest_for("new");
        store
            .put_skill_package(&package_at(
                &digest_old,
                SkillChannel::Released,
                1,
                0,
                0,
                "",
            ))
            .expect("1.0.0");
        store
            .put_skill_package(&package_at(
                &digest_new,
                SkillChannel::Released,
                1,
                1,
                0,
                "",
            ))
            .expect("1.1.0");
        store
            .put_skill_package(&package_at(DIGEST_B, SkillChannel::Draft, 2, 0, 0, "draft"))
            .expect("draft");
        _drive9.write_blob(&digest_new, &fixture_archive("new"));

        let agent = agent_with_refs(vec![unversioned_skill_ref()]);
        let session = tempfile::tempdir().expect("session");
        let staging = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect("resolve")
        .expect("staging");
        assert_eq!(
            fs::read_to_string(staging.join("assistant-tools/execute.js")).expect("read"),
            "new"
        );
    }

    #[test]
    fn unversioned_skill_ref_fails_without_published_version() {
        let store = store();
        store
            .put_skill_package(&package_record(DIGEST_A, SkillChannel::Draft, "draft"))
            .expect("draft");
        let agent = agent_with_refs(vec![unversioned_skill_ref()]);
        let session = tempfile::tempdir().expect("session");
        let err = resolve_agent_skill_refs(
            &store,
            &agent,
            session.path(),
            None,
            LOCAL_USER_ID,
            ResolveMode::Release,
        )
        .expect_err("closed");
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
        assert!(err.to_string().contains("no published version"));
    }
}
