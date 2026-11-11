use std::fs::{self, File};
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use tar::Archive;
use vam_spec::v1::{ResourceId, SkillChannel, SkillManifest, SkillPackageRecord, Version};
use vam_spec::validation::parse_skill_urn;

use crate::error::OrchestratorError;
use crate::manager::drive9_root;
use crate::skill_packages::{skill_blob_filename, skill_blob_relpath, skill_catalog_root};
use crate::workflow::SqliteWorkflowStore;

/// Maximum uploaded skill package archive size (32 MiB).
pub const MAX_SKILL_PACKAGE_BYTES: usize = 32 * 1024 * 1024;

/// Unpack, validate, digest, store blob, and upsert a draft skill package row.
pub fn admit_and_publish_skill_package(
    store: &SqliteWorkflowStore,
    archive_bytes: &[u8],
) -> Result<SkillPackageRecord, OrchestratorError> {
    admit_and_publish_skill_package_owned(store, archive_bytes, crate::users::LOCAL_USER_ID)
}

/// Same as [`admit_and_publish_skill_package`], stamping `owner_user_id`.
pub fn admit_and_publish_skill_package_owned(
    store: &SqliteWorkflowStore,
    archive_bytes: &[u8],
    owner_user_id: &str,
) -> Result<SkillPackageRecord, OrchestratorError> {
    if archive_bytes.len() > MAX_SKILL_PACKAGE_BYTES {
        return Err(OrchestratorError::InvalidContract(format!(
            "skill package archive exceeds {} byte limit",
            MAX_SKILL_PACKAGE_BYTES
        )));
    }

    let temp_dir = tempfile::tempdir().map_err(OrchestratorError::Io)?;
    unpack_tar_gz(archive_bytes, temp_dir.path())?;
    let skill_root = resolve_skill_root(temp_dir.path())?;
    let manifest = load_and_validate_manifest(&skill_root)?;
    validate_skills_frontmatter(&skill_root, &manifest)?;
    crate::node_base::check_requires_runtime(&manifest)?;
    crate::skill_cards::check_package_cards(&skill_root, &manifest)
        .map_err(OrchestratorError::InvalidContract)?;

    let digest = vam_storage::skills_digest_from_dir(&skill_root);
    if digest == "none" {
        return Err(OrchestratorError::InvalidContract(
            "skill package produced an empty digest; SKILLS.md and manifest.json are required"
                .to_string(),
        ));
    }

    let owner = crate::owner::resolved_owner(owner_user_id);
    let blob_relpath = skill_blob_relpath(owner, &digest);
    write_content_addressed_blob(&blob_relpath, archive_bytes)?;

    let urn = manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .unwrap_or_default()
        .to_string();
    let version = manifest.version.clone().ok_or_else(|| {
        OrchestratorError::InvalidContract("manifest.json version is required".to_string())
    })?;

    let mut spec = manifest;
    spec.version = Some(Version {
        major: version.major,
        minor: version.minor,
        patch: version.patch,
        prerelease: "draft".to_string(),
        build: String::new(),
    });

    let package = SkillPackageRecord {
        resource_id: Some(ResourceId { urn }),
        version: Some(Version {
            major: version.major,
            minor: version.minor,
            patch: version.patch,
            prerelease: "draft".to_string(),
            build: String::new(),
        }),
        channel: SkillChannel::Draft as i32,
        digest,
        blob_relpath,
        size_bytes: archive_bytes.len() as i64,
        spec: Some(spec),
        metadata: None,
        ..Default::default()
    };

    store.put_skill_package_with_owner(&package, owner_user_id)
}

pub(crate) fn write_content_addressed_blob(
    relpath: &str,
    archive_bytes: &[u8],
) -> Result<(), OrchestratorError> {
    validate_blob_relpath(relpath)?;
    let catalog_root = skill_catalog_root(&drive9_root());
    let blob_path = catalog_root.join(relpath);
    if !blob_path.starts_with(&catalog_root) {
        return Err(OrchestratorError::InvalidContract(format!(
            "skill package blob path '{}' escapes catalog root",
            blob_path.display()
        )));
    }
    if blob_path.exists() {
        // Same owner + same digest: idempotent. Other owners use a different
        // relpath, so this cannot overwrite another user's blob.
        return Ok(());
    }
    if let Some(parent) = blob_path.parent() {
        fs::create_dir_all(parent).map_err(OrchestratorError::Io)?;
    }
    let mut file = File::create(&blob_path).map_err(OrchestratorError::Io)?;
    file.write_all(archive_bytes)
        .map_err(OrchestratorError::Io)?;
    Ok(())
}

fn validate_blob_relpath(relpath: &str) -> Result<(), OrchestratorError> {
    let path = Path::new(relpath);
    if relpath.is_empty() || path.is_absolute() {
        return Err(OrchestratorError::InvalidContract(
            "skill package blob_relpath must be a relative owner-namespaced path".to_string(),
        ));
    }
    for component in path.components() {
        match component {
            Component::ParentDir | Component::Prefix(_) | Component::RootDir => {
                return Err(OrchestratorError::InvalidContract(format!(
                    "skill package blob_relpath must not contain '..' or an absolute prefix, got '{relpath}'"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Copy a legacy global `sha256-{digest}.tar.gz` blob into the owner namespace.
///
/// No-op when the namespaced file already exists. Does not delete the legacy
/// file (other rows may still be migrating). Never writes into another owner's
/// directory.
pub fn migrate_legacy_skill_blob(
    catalog_root: &Path,
    owner_user_id: &str,
    digest: &str,
) -> Result<PathBuf, OrchestratorError> {
    let relpath = skill_blob_relpath(owner_user_id, digest);
    validate_blob_relpath(&relpath)?;
    let dest = catalog_root.join(&relpath);
    if dest.is_file() {
        return Ok(dest);
    }
    let legacy = catalog_root.join(skill_blob_filename(digest));
    if legacy.is_file() {
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent).map_err(OrchestratorError::Io)?;
        }
        fs::copy(&legacy, &dest).map_err(OrchestratorError::Io)?;
    }
    Ok(dest)
}

pub fn unpack_tar_gz(archive_bytes: &[u8], dest: &Path) -> Result<(), OrchestratorError> {
    let decoder = GzDecoder::new(archive_bytes);
    let mut archive = Archive::new(decoder);
    for entry in archive
        .entries()
        .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?
    {
        let mut entry = entry.map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
        let entry_path = entry
            .path()
            .map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?
            .into_owned();
        validate_archive_path(&entry_path)?;
        entry
            .unpack_in(dest)
            .map_err(|e| OrchestratorError::InvalidContract(format!("tar unpack: {e}")))?;
    }
    Ok(())
}

fn validate_archive_path(path: &Path) -> Result<(), OrchestratorError> {
    let path_str = path.to_string_lossy();
    if path_str.contains('\0') {
        return Err(OrchestratorError::InvalidContract(
            "archive entry path contains NUL byte".to_string(),
        ));
    }
    if path.is_absolute() {
        return Err(OrchestratorError::InvalidContract(format!(
            "archive entry path must be relative, got '{}'",
            path.display()
        )));
    }
    for component in path.components() {
        match component {
            Component::ParentDir => {
                return Err(OrchestratorError::InvalidContract(format!(
                    "archive entry path must not contain '..', got '{}'",
                    path.display()
                )));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(OrchestratorError::InvalidContract(format!(
                    "archive entry path must be relative, got '{}'",
                    path.display()
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

pub fn resolve_skill_root(unpack_root: &Path) -> Result<PathBuf, OrchestratorError> {
    let mut entries = fs::read_dir(unpack_root)
        .map_err(OrchestratorError::Io)?
        .filter_map(|entry| entry.ok())
        .collect::<Vec<_>>();
    entries.sort_by_key(|entry| entry.file_name());

    if entries.len() == 1 {
        let only = entries[0].path();
        if only.is_dir() && vam_spec::package::skill_doc(&only).is_some() {
            return Ok(only);
        }
    }

    // `SKILLS.md` or the community `SKILL.md`; `skill_doc` documents the precedence.
    if vam_spec::package::skill_doc(unpack_root).is_some() {
        return Ok(unpack_root.to_path_buf());
    }

    Err(OrchestratorError::InvalidContract(
        "skill package must contain SKILLS.md (or SKILL.md) at archive root or inside a single top-level directory"
            .to_string(),
    ))
}

fn load_and_validate_manifest(skill_root: &Path) -> Result<SkillManifest, OrchestratorError> {
    // Manifest rules, tool and egress shapes, and the URN shape are the shared offline check.
    let manifest = vam_spec::package::load_skill_manifest(skill_root)
        .map_err(|e| OrchestratorError::InvalidContract(e.0))?;
    for warning in vam_spec::validation::skill_manifest_warnings(&manifest) {
        tracing::warn!("{warning}");
    }
    // Node policy: which registry domains this node admits.
    let urn = manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .unwrap_or_default();
    let (registry, _, _) =
        parse_skill_urn(urn).map_err(|e| OrchestratorError::InvalidContract(e.to_string()))?;
    if !crate::registry::registry_domains().admits_skill_registry(&registry) {
        return Err(OrchestratorError::InvalidContract(format!(
            "skill URN registry '{registry}' is not a local catalog registry"
        )));
    }
    Ok(manifest)
}

fn validate_skills_frontmatter(
    skill_root: &Path,
    manifest: &SkillManifest,
) -> Result<(), OrchestratorError> {
    vam_spec::package::check_skills_md(skill_root, manifest)
        .map_err(|e| OrchestratorError::InvalidContract(e.0))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_packages::DRIVE9_ENV_LOCK;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::{copy, Cursor};

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

    #[test]
    fn reject_parent_dir_in_tar_path() {
        let err = validate_archive_path(std::path::Path::new("../evil/SKILLS.md"))
            .expect_err("path jail");
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    }

    #[test]
    fn resolve_single_top_level_directory() {
        let temp = tempfile::tempdir().expect("tempdir");
        let skill_dir = temp.path().join("pkg");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(skill_dir.join("SKILLS.md"), "# x").expect("write");
        let root = resolve_skill_root(temp.path()).expect("resolve");
        assert_eq!(root, skill_dir);
    }

    #[test]
    fn resolve_finds_a_community_skill_md() {
        let temp = tempfile::tempdir().expect("tempdir");
        let skill_dir = temp.path().join("pkg");
        fs::create_dir_all(&skill_dir).expect("mkdir");
        fs::write(skill_dir.join("SKILL.md"), "# x").expect("write");
        assert_eq!(resolve_skill_root(temp.path()).expect("resolve"), skill_dir);
        let flat = tempfile::tempdir().expect("tempdir");
        fs::write(flat.path().join("SKILL.md"), "# x").expect("write");
        fs::write(flat.path().join("manifest.json"), "{}").expect("write");
        assert_eq!(
            resolve_skill_root(flat.path()).expect("resolve"),
            flat.path()
        );
    }

    // The owner rule (`require_matching_urn_owner`) refuses a second owner for a
    // URN the first owner already published. Both tests below keep their
    // blob-isolation intent by exercising that refusal, not by sharing a URN.

    #[test]
    fn owner_blob_paths_are_namespaced_per_owner_for_same_digest() {
        let _guard = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().expect("tempdir");
        std::env::set_var("VAM_DRIVE9_ROOT", root.path());

        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice user");
        store.create_user("bob", "bob").expect("bob user");
        let urn = "urn:vam:skill:vam.local/vam/test-skill";
        let archive = write_tar_gz(&[
            ("SKILLS.md", minimal_skills_md(urn).as_bytes()),
            ("manifest.json", minimal_manifest_json(urn).as_bytes()),
        ]);

        let alice =
            admit_and_publish_skill_package_owned(&store, &archive, "alice").expect("alice admit");
        // Bob cannot version alice's URN, so the catalog row is refused.
        let err = admit_and_publish_skill_package_owned(&store, &archive, "bob")
            .expect_err("bob must not version alice's URN");
        assert!(
            matches!(err, OrchestratorError::PermissionDenied(_)),
            "{err:?}"
        );

        // The per-owner relpaths for one digest must still differ, and alice's
        // blob must be where her admit put it.
        let catalog = skill_catalog_root(root.path());
        let alice_path = catalog.join(skill_blob_relpath("alice", &alice.digest));
        let bob_path = catalog.join(skill_blob_relpath("bob", &alice.digest));
        assert!(alice_path.is_file(), "alice blob {}", alice_path.display());
        assert_ne!(alice_path, bob_path);
        assert_eq!(
            alice_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned()),
            Some(skill_blob_filename(&alice.digest))
        );
    }

    #[test]
    fn a_package_with_skill_md_is_admitted() {
        let _guard = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().expect("tempdir");
        std::env::set_var("VAM_DRIVE9_ROOT", root.path());

        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice user");
        let urn = "urn:vam:skill:vam.local/vam/community-skill";
        let archive = write_tar_gz(&[
            ("SKILL.md", minimal_skills_md(urn).as_bytes()),
            ("manifest.json", minimal_manifest_json(urn).as_bytes()),
        ]);
        let record =
            admit_and_publish_skill_package_owned(&store, &archive, "alice").expect("admit");
        assert_ne!(record.digest, "none");
    }

    #[test]
    fn refused_admit_does_not_overwrite_other_owner_blob() {
        let _guard = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().expect("tempdir");
        std::env::set_var("VAM_DRIVE9_ROOT", root.path());

        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice user");
        store.create_user("bob", "bob").expect("bob user");
        let urn = "urn:vam:skill:vam.local/vam/test-skill";
        let archive = write_tar_gz(&[
            ("SKILLS.md", minimal_skills_md(urn).as_bytes()),
            ("manifest.json", minimal_manifest_json(urn).as_bytes()),
        ]);

        let bob = admit_and_publish_skill_package_owned(&store, &archive, "bob").expect("bob");
        let catalog = skill_catalog_root(root.path());
        let bob_path = catalog.join(&bob.blob_relpath);
        fs::write(&bob_path, b"bob-owned-marker").expect("marker");

        // Alice's admit of bob's URN is refused, and bob's blob must stay intact.
        let err = admit_and_publish_skill_package_owned(&store, &archive, "alice")
            .expect_err("alice must not version bob's URN");
        assert!(
            matches!(err, OrchestratorError::PermissionDenied(_)),
            "{err:?}"
        );
        assert_eq!(
            fs::read(&bob_path).expect("read bob"),
            b"bob-owned-marker",
            "alice's refused admit must not overwrite bob's blob"
        );
    }

    #[test]
    fn write_content_addressed_blob_keeps_owner_copies_distinct() {
        let _guard = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().expect("tempdir");
        std::env::set_var("VAM_DRIVE9_ROOT", root.path());

        let digest = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let alice_rel = skill_blob_relpath("alice", digest);
        let bob_rel = skill_blob_relpath("bob", digest);
        write_content_addressed_blob(&alice_rel, b"alice-bytes").expect("alice write");
        write_content_addressed_blob(&bob_rel, b"bob-bytes").expect("bob write");

        let catalog = skill_catalog_root(root.path());
        assert_eq!(
            fs::read(catalog.join(&alice_rel)).expect("read alice"),
            b"alice-bytes"
        );
        assert_eq!(
            fs::read(catalog.join(&bob_rel)).expect("read bob"),
            b"bob-bytes"
        );

        write_content_addressed_blob(&alice_rel, b"should-not-overwrite").expect("noop");
        assert_eq!(
            fs::read(catalog.join(&alice_rel)).expect("read alice after"),
            b"alice-bytes"
        );
        assert_eq!(
            fs::read(catalog.join(&bob_rel)).expect("read bob after"),
            b"bob-bytes"
        );
    }

    #[test]
    fn migrate_copies_legacy_blob_under_owner() {
        let root = tempfile::tempdir().expect("tempdir");
        let catalog = skill_catalog_root(root.path());
        let digest = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let legacy = catalog.join(skill_blob_filename(digest));
        fs::create_dir_all(&catalog).expect("mkdir");
        fs::write(&legacy, b"legacy-bytes").expect("write legacy");

        let dest = migrate_legacy_skill_blob(&catalog, "alice", digest).expect("migrate");
        assert_eq!(dest, catalog.join(skill_blob_relpath("alice", digest)));
        assert_eq!(fs::read(&dest).expect("read"), b"legacy-bytes");
        assert!(legacy.is_file(), "legacy file stays until CAS GC");
    }

    #[test]
    fn frontmatter_mismatch_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let urn = "urn:vam:skill:vam.local/vam/test-skill";
        fs::write(
            temp.path().join("SKILLS.md"),
            minimal_skills_md("urn:vam:skill:hub.vam.dev/acme/other"),
        )
        .expect("write skills");
        fs::write(
            temp.path().join("manifest.json"),
            minimal_manifest_json(urn),
        )
        .expect("write manifest");
        let manifest = load_and_validate_manifest(temp.path()).expect("manifest");
        let err = validate_skills_frontmatter(temp.path(), &manifest).expect_err("mismatch");
        assert!(matches!(err, OrchestratorError::InvalidContract(_)));
    }

    #[test]
    fn frontmatter_tools_disagree_with_manifest() {
        let temp = tempfile::tempdir().expect("tempdir");
        let urn = "urn:vam:skill:vam.local/vam/echo-skill";
        let manifest = r#"{
  "resourceId": {"urn": "urn:vam:skill:vam.local/vam/echo-skill"},
  "version": {"major": 1, "minor": 0, "patch": 0},
  "description": "Echo",
  "tools": [{
    "name": "echo_tool",
    "description": "Echo a query",
    "inputSchemaJson": "{\"type\":\"object\",\"properties\":{\"q\":{\"type\":\"string\"}},\"required\":[\"q\"]}",
    "localExec": {"command": "bun", "entrypoint": "./execute.js", "defaultArgs": ["search"]},
    "requiredSecrets": [{"name": "jira/base-url", "required": true}],
    "sideEffect": "SIDE_EFFECT_CLASS_READ_ONLY",
    "timeout": "30s"
  }]
}"#;
        let disagreeing = format!(
            "---\nid: \"{urn}\"\nversion: \"1.0.0\"\ntools:\n  - name: \"echo_tool\"\n    exec:\n      command: \"bun\"\n      entrypoint: \"/mnt/drive9/skills/echo-skill/execute.js\"\n---\n\n# Body\n"
        );
        fs::write(temp.path().join("manifest.json"), manifest).expect("manifest");
        fs::write(temp.path().join("SKILLS.md"), disagreeing).expect("skills");
        let loaded = load_and_validate_manifest(temp.path()).expect("manifest loads");
        let err = validate_skills_frontmatter(temp.path(), &loaded).expect_err("disagree");
        let OrchestratorError::InvalidContract(message) = err else {
            panic!("expected invalid contract");
        };
        assert!(
            message.contains("entrypoint"),
            "disagreement should name the entrypoint: {message}"
        );

        let agreeing = format!(
            "---\nid: \"{urn}\"\nversion: \"1.0.0\"\ntools:\n  - name: \"echo_tool\"\n    input_schema:\n      type: \"object\"\n      properties:\n        q:\n          type: \"string\"\n      required:\n        - \"q\"\n    exec:\n      command: \"bun\"\n      entrypoint: \"./execute.js\"\n      default_args: [\"search\"]\n    required_secrets:\n      - \"jira/base-url\"\n    side_effect: \"READ_ONLY\"\n    timeout: \"30s\"\n---\n\n# Use echo_tool for queries.\n"
        );
        fs::write(temp.path().join("SKILLS.md"), agreeing).expect("skills");
        validate_skills_frontmatter(temp.path(), &loaded).expect("matching frontmatter");
    }

    #[test]
    fn admit_preserves_index_md_in_unpacked_blob() {
        let _guard = DRIVE9_ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let root = tempfile::tempdir().expect("tempdir");
        std::env::set_var("VAM_DRIVE9_ROOT", root.path());

        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("alice", "alice").expect("alice user");
        let urn = "urn:vam:skill:vam.local/vam/oracle-sql";
        let index =
            b"# VAM skill documentation index\n\npo_headers_all\tdocs/tables/po_headers_all.md\n";
        let archive = write_tar_gz(&[
            ("SKILLS.md", minimal_skills_md(urn).as_bytes()),
            ("manifest.json", minimal_manifest_json(urn).as_bytes()),
            ("INDEX.md", index.as_slice()),
            (
                "docs/tables/po_headers_all.md",
                b"# po_headers_all\n\ncolumns\n",
            ),
        ]);

        let without_index = write_tar_gz(&[
            ("SKILLS.md", minimal_skills_md(urn).as_bytes()),
            ("manifest.json", minimal_manifest_json(urn).as_bytes()),
        ]);
        let digest_without = {
            let temp = tempfile::tempdir().expect("tempdir");
            unpack_tar_gz(&without_index, temp.path()).expect("unpack without");
            let skill_root = resolve_skill_root(temp.path()).expect("root");
            vam_storage::skills_digest_from_dir(&skill_root)
        };

        let admitted =
            admit_and_publish_skill_package_owned(&store, &archive, "alice").expect("admit");
        assert_ne!(
            admitted.digest, digest_without,
            "INDEX.md must participate in the package digest"
        );

        let blob_path = skill_catalog_root(root.path()).join(&admitted.blob_relpath);
        let blob = fs::read(&blob_path).expect("read blob");
        let unpack = tempfile::tempdir().expect("unpack dir");
        unpack_tar_gz(&blob, unpack.path()).expect("unpack blob");
        let skill_root = resolve_skill_root(unpack.path()).expect("skill root");
        let restored = fs::read(skill_root.join("INDEX.md")).expect("INDEX.md");
        assert_eq!(restored, index);
        assert!(skill_root.join("docs/tables/po_headers_all.md").is_file());
    }
}
