//! `.vam/connector.json` next to the manifest of a skill that needs a company-address connector
//! (CN-6.5).
//!
//! A skill tool that reaches `acme.ticketdesk.example` has to know `acme`, and nothing carries a
//! per-node value into a tool (the guest clears the tool's environment). The node writes the host
//! into the skill's own directory when the session's skills are staged, on the host, before the
//! skills drive is built, so the guest changes nothing:
//!
//! ```json
//! { "connector": "ticketdesk", "hosts": { "api": "acme.ticketdesk.example" } }
//! ```
//!
//! The file is a hint, not a grant. It holds the connector id and the filled binding hosts: no
//! token, no vault id, no secret. The swap attaches a token only inside the tool's declared shape
//! on the connector's filled hosts (`connection_egress`), whatever the file says or a skill does.
//!
//! Which skills: the ones the agent attaches (their directory names come from the agent's skill
//! URNs, never from the package's contents), whose manifest has `requiresConnector` naming a
//! connector that is enabled on this node with an address. For such a skill any `.vam` entry it
//! shipped is removed first (a symlink is unlinked, never followed) and the file is created new.
//! A skill with no `requiresConnector` is not touched.

use crate::connectors::{connector_id, ConnectorEntry, NodeConnector};
use crate::error::OrchestratorError;
use crate::workflow::SqliteWorkflowStore;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use vam_spec::v1::SkillManifest;

/// The directory and file, relative to a skill directory.
pub const HINT_DIR: &str = ".vam";
pub const HINT_FILE: &str = "connector.json";
/// A manifest larger than this is not read (a skill manifest is a few KiB).
const MAX_MANIFEST_BYTES: u64 = 1 << 20;

fn refuse(what: impl Into<String>) -> OrchestratorError {
    OrchestratorError::InvalidContract(what.into())
}

/// The connector's id and `binding -> first filled host`, when `urn` names a connector that is
/// enabled on this node with an address and has at least one binding host.
fn hint_for(
    urn: &str,
    entries: &[ConnectorEntry],
    nodes: &[NodeConnector],
) -> Option<(String, BTreeMap<String, String>)> {
    let entry = entries.iter().find(|e| e.def.urn == urn)?;
    // Only a connector with a company address: a built-in connector has none to tell.
    entry.template.tenant.as_ref()?;
    let id = connector_id(&entry.def);
    let node = nodes.iter().find(|n| n.connector_id == id)?;
    if !node.enabled || node.tenant_subdomain.is_none() {
        return None;
    }
    let hosts: BTreeMap<String, String> = entry
        .def
        .bindings
        .iter()
        .flatten()
        .filter_map(|b| {
            let h = b.hosts.iter().find(|h| !h.contains(['{', '}']))?;
            Some((b.name.clone(), h.clone()))
        })
        .collect();
    (!hosts.is_empty()).then_some((id, hosts))
}

/// A skill directory name that is one plain path component.
fn plain_name(name: &str) -> Result<&str, OrchestratorError> {
    let mut parts = Path::new(name).components();
    match (parts.next(), parts.next()) {
        (Some(Component::Normal(c)), None) if c == name && !name.contains('\\') => Ok(name),
        _ => Err(refuse(format!(
            "skill directory name {name:?} must be one plain path component"
        ))),
    }
}

/// `root/name`, a real directory (not a symlink) whose parent is `root` itself.
fn skill_dir(root: &Path, name: &str) -> Result<Option<PathBuf>, OrchestratorError> {
    let dir = root.join(plain_name(name)?);
    let meta = match std::fs::symlink_metadata(&dir) {
        Ok(m) => m,
        // Not staged here (a seed skill or a synthetic one): nothing to write.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(OrchestratorError::Io(e)),
    };
    if !meta.is_dir() {
        return Err(refuse(format!("skill entry {name:?} is not a directory")));
    }
    let real_root = std::fs::canonicalize(root).map_err(OrchestratorError::Io)?;
    let real_dir = std::fs::canonicalize(&dir).map_err(OrchestratorError::Io)?;
    if real_dir.parent() != Some(real_root.as_path()) {
        return Err(refuse(format!(
            "skill directory {name:?} leaves the catalog"
        )));
    }
    Ok(Some(dir))
}

/// The manifest's `requiresConnector`, if the skill has a regular `manifest.json` that parses.
fn requires_connector(dir: &Path) -> Option<String> {
    let path = dir.join("manifest.json");
    let meta = std::fs::symlink_metadata(&path).ok()?;
    if !meta.is_file() || meta.len() > MAX_MANIFEST_BYTES {
        return None;
    }
    let text = std::fs::read_to_string(&path).ok()?;
    let manifest =
        vam_spec::encoding::json_to_proto::<SkillManifest>("vam.v1.SkillManifest", &text).ok()?;
    Some(manifest.requires_connector).filter(|u| !u.is_empty())
}

/// Removes whatever `dir/.vam` is: a symlink or file is unlinked, a directory is deleted.
fn clear_hint_dir(dir: &Path) -> Result<(), OrchestratorError> {
    let hint = dir.join(HINT_DIR);
    match std::fs::symlink_metadata(&hint) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(&hint).map_err(OrchestratorError::Io),
        Ok(_) => std::fs::remove_file(&hint).map_err(OrchestratorError::Io),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(OrchestratorError::Io(e)),
    }
}

/// The seed tree's own copy of skill `name` (`seed/skills/<name>`): a real directory whose parent is
/// the seed's `skills` directory. `None` when the seed has no such skill.
fn seed_skill_dir(seed: Option<&Path>, name: &str) -> Result<Option<PathBuf>, OrchestratorError> {
    let Some(seed) = seed else {
        return Ok(None);
    };
    skill_dir(&seed.join("skills"), name)
}

/// Copies a seed skill into the session's staged tree: regular files and directories only (a
/// symlink in the seed is not followed or copied).
fn copy_seed_skill(from: &Path, to: &Path) -> Result<(), OrchestratorError> {
    std::fs::create_dir_all(to).map_err(OrchestratorError::Io)?;
    for entry in std::fs::read_dir(from).map_err(OrchestratorError::Io)? {
        let entry = entry.map_err(OrchestratorError::Io)?;
        let kind = entry.file_type().map_err(OrchestratorError::Io)?;
        let target = to.join(entry.file_name());
        if kind.is_dir() {
            copy_seed_skill(&entry.path(), &target)?;
        } else if kind.is_file() {
            std::fs::copy(entry.path(), &target).map_err(OrchestratorError::Io)?;
        }
    }
    Ok(())
}

/// Writes the file for one skill from the connectors and node rows given. Returns whether a
/// file was written.
///
/// A skill the catalog staged is written in place. A skill only the seed tree supplies is not staged;
/// when its manifest needs a connector this node can tell the host of, the seed's copy is copied
/// into the staged tree first (the catalog overlay wins per skill name on materialization, so the
/// copy replaces the seed's) and the file is written there. The shared seed tree is never written.
fn write_one(
    root: &Path,
    name: &str,
    entries: &[ConnectorEntry],
    nodes: &[NodeConnector],
    seed: Option<&Path>,
) -> Result<bool, OrchestratorError> {
    let dir = match skill_dir(root, name)? {
        Some(dir) => dir,
        None => {
            let Some(seed_dir) = seed_skill_dir(seed, name)? else {
                return Ok(false);
            };
            let Some(urn) = requires_connector(&seed_dir) else {
                return Ok(false);
            };
            if hint_for(&urn, entries, nodes).is_none() {
                return Ok(false);
            }
            let staged = root.join(plain_name(name)?);
            copy_seed_skill(&seed_dir, &staged)?;
            staged
        }
    };
    let Some(urn) = requires_connector(&dir) else {
        return Ok(false);
    };
    // A shipped or stale `.vam` goes first, whether or not there is anything to write now.
    clear_hint_dir(&dir)?;
    let Some((id, hosts)) = hint_for(&urn, entries, nodes) else {
        return Ok(false);
    };
    let body = serde_json::json!({ "connector": id, "hosts": hosts }).to_string();
    let hint_dir = dir.join(HINT_DIR);
    std::fs::create_dir(&hint_dir).map_err(OrchestratorError::Io)?;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(hint_dir.join(HINT_FILE))
        .map_err(OrchestratorError::Io)?;
    file.write_all(body.as_bytes())
        .map_err(OrchestratorError::Io)?;
    Ok(true)
}

/// Writes `.vam/connector.json` into the staged skills `catalog_root` for the agent's skills
/// `skill_urns` (the agent's `skillRefs`). Returns how many files were written. Call it after the
/// skills are staged and before they are materialized (the guest's skills drive is built from
/// the staged tree).
pub fn write_connector_files(
    store: &SqliteWorkflowStore,
    catalog_root: &Path,
    skill_urns: &[String],
    seed_dir: Option<&Path>,
) -> Result<usize, OrchestratorError> {
    let mut names: Vec<String> = Vec::new();
    for urn in skill_urns {
        let (_, _, name) = vam_spec::validation::parse_skill_urn(urn)
            .map_err(|e| refuse(format!("skill ref {urn:?}: {e}")))?;
        if !names.contains(&name) {
            names.push(name);
        }
    }
    let entries = store.connector_entries()?;
    let nodes = store.node_connectors()?;
    write_names(catalog_root, &names, &entries, &nodes, seed_dir)
}

fn write_names(
    root: &Path,
    names: &[String],
    entries: &[ConnectorEntry],
    nodes: &[NodeConnector],
    seed: Option<&Path>,
) -> Result<usize, OrchestratorError> {
    let mut written = 0;
    for name in names {
        if write_one(root, name, entries, nodes, seed)? {
            written += 1;
        }
    }
    Ok(written)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connector_client::test_support::install_company_address_rows;

    const ZURN: &str = "urn:vam:connector:connectors.community.example/community/zendesk";

    fn manifest(requires: Option<&str>) -> String {
        let mut m = serde_json::json!({
            "resourceId": { "urn": "urn:vam:skill:connectors.community.example/community/zd-tickets" },
            "version": { "major": 1, "minor": 0, "patch": 0 },
            "description": "tickets",
            "tools": []
        });
        if let Some(r) = requires {
            m["requiresConnector"] = serde_json::json!(r);
        }
        m.to_string()
    }

    fn skill(root: &Path, name: &str, requires: Option<&str>) -> PathBuf {
        let d = root.join(name);
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("manifest.json"), manifest(requires)).unwrap();
        d
    }

    fn node(enabled: bool, address: Option<&str>) -> NodeConnector {
        NodeConnector {
            connector_id: "zendesk".into(),
            enabled,
            consent_tenant_id: None,
            consent_recorded_at: None,
            consent_recorded_by: None,
            tenant_subdomain: address.map(str::to_string),
            updated_at: 1,
            updated_by: "olga".into(),
        }
    }

    fn world(n: Option<NodeConnector>) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        install_company_address_rows(&store);
        if let Some(n) = n {
            store.put_node_connector(&n).unwrap();
        }
        store
    }

    fn urn(name: &str) -> String {
        format!("urn:vam:skill:connectors.community.example/community/{name}")
    }

    fn hint(dir: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(dir.join(".vam/connector.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn a_skill_that_requires_an_addressed_enabled_connector_gets_its_host_and_nothing_secret() {
        let root = tempfile::tempdir().unwrap();
        let d = skill(root.path(), "zd-tickets", Some(ZURN));
        let store = world(Some(node(true, Some("acme"))));
        let n = write_connector_files(&store, root.path(), &[urn("zd-tickets")], None).unwrap();
        assert_eq!(n, 1);
        let h = hint(&d);
        assert_eq!(
            h,
            serde_json::json!({ "connector": "zendesk", "hosts": { "api": "acme.zendesk.com" } })
        );
        let text = std::fs::read_to_string(d.join(".vam/connector.json")).unwrap();
        for forbidden in ["token", "vault", "secret", "connector/", "{subdomain}"] {
            assert!(!text.contains(forbidden), "{forbidden} in {text}");
        }
    }

    #[test]
    fn nothing_is_written_without_an_enabled_connector_with_an_address() {
        for n in [
            None,
            Some(node(false, Some("acme"))),
            Some(node(true, None)),
        ] {
            let root = tempfile::tempdir().unwrap();
            let d = skill(root.path(), "zd-tickets", Some(ZURN));
            let store = world(n);
            let written =
                write_connector_files(&store, root.path(), &[urn("zd-tickets")], None).unwrap();
            assert_eq!(written, 0);
            assert!(!d.join(".vam").exists());
        }
    }

    #[test]
    fn a_skill_without_a_connector_or_with_another_one_is_unchanged() {
        let root = tempfile::tempdir().unwrap();
        let plain = skill(root.path(), "plain", None);
        std::fs::create_dir_all(plain.join(".vam")).unwrap();
        std::fs::write(plain.join(".vam/connector.json"), "{\"own\":1}").unwrap();
        let other = skill(
            root.path(),
            "other",
            Some("urn:vam:connector:connectors.community.example/community/elsewhere"),
        );
        let m365 = skill(
            root.path(),
            "graph",
            Some("urn:vam:connector:hub.squrell.cloud/squrell/microsoft-365"),
        );
        let store = world(Some(node(true, Some("acme"))));
        let names = [urn("plain"), urn("other"), urn("graph")];
        assert_eq!(
            write_connector_files(&store, root.path(), &names, None).unwrap(),
            0
        );
        // A skill that does not require a connector keeps what it has.
        assert_eq!(
            std::fs::read_to_string(plain.join(".vam/connector.json")).unwrap(),
            "{\"own\":1}"
        );
        assert!(!other.join(".vam").exists() && !m365.join(".vam").exists());
    }

    #[test]
    fn a_shipped_or_stale_file_is_replaced_and_a_symlink_is_never_followed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let d = skill(root.path(), "zd-tickets", Some(ZURN));
        // The package shipped `.vam` as a symlink to a directory outside the staging tree.
        std::os::unix::fs::symlink(outside.path(), d.join(".vam")).unwrap();
        std::fs::write(outside.path().join("keep.txt"), "keep").unwrap();
        let store = world(Some(node(true, Some("acme"))));
        write_connector_files(&store, root.path(), &[urn("zd-tickets")], None).unwrap();
        assert!(outside.path().join("keep.txt").exists());
        assert!(!outside.path().join("connector.json").exists());
        assert!(std::fs::symlink_metadata(d.join(".vam"))
            .unwrap()
            .file_type()
            .is_dir());
        assert_eq!(hint(&d)["hosts"]["api"], "acme.zendesk.com");
        // A stale file from an earlier address is replaced; with no address it is removed.
        store
            .put_node_connector(&node(true, Some("globex")))
            .unwrap();
        write_connector_files(&store, root.path(), &[urn("zd-tickets")], None).unwrap();
        assert_eq!(hint(&d)["hosts"]["api"], "globex.zendesk.com");
        store
            .put_node_connector(&node(false, Some("globex")))
            .unwrap();
        write_connector_files(&store, root.path(), &[urn("zd-tickets")], None).unwrap();
        assert!(!d.join(".vam").exists());
    }

    #[test]
    fn a_name_that_is_not_one_plain_component_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let store = world(Some(node(true, Some("acme"))));
        let (entries, nodes) = (
            store.connector_entries().unwrap(),
            store.node_connectors().unwrap(),
        );
        for bad in ["..", ".", "", "../x", "a/b", "/etc", "/", "a\\b"] {
            let e = write_names(root.path(), &[bad.to_string()], &entries, &nodes, None);
            assert!(
                matches!(e, Err(OrchestratorError::InvalidContract(_))),
                "{bad:?}: {e:?}"
            );
        }
        // A URN whose last segment is not a slug never reaches the filesystem.
        let e = write_connector_files(
            &store,
            root.path(),
            &["urn:vam:skill:connectors.example/pub/..".to_string()],
            None,
        );
        assert!(e.is_err());
        let e = write_connector_files(&store, root.path(), &["not-a-urn".to_string()], None);
        assert!(e.is_err());
    }

    #[test]
    fn a_skill_directory_that_is_a_symlink_out_of_the_catalog_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("manifest.json"), manifest(Some(ZURN))).unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("zd-tickets")).unwrap();
        let store = world(Some(node(true, Some("acme"))));
        let e = write_connector_files(&store, root.path(), &[urn("zd-tickets")], None);
        assert!(
            matches!(e, Err(OrchestratorError::InvalidContract(_))),
            "{e:?}"
        );
        assert!(!outside.path().join(".vam").exists());
    }

    #[test]
    fn a_seed_skill_that_needs_the_connector_is_staged_with_its_hint_and_the_seed_is_left_alone() {
        let root = tempfile::tempdir().unwrap();
        let seed = tempfile::tempdir().unwrap();
        let seeded = skill(&seed.path().join("skills"), "zd-tickets", Some(ZURN));
        std::fs::write(seeded.join("execute.js"), "// tool").unwrap();
        // A symlink the seed skill holds is not followed or copied.
        std::os::unix::fs::symlink("/etc/passwd", seeded.join("passwd")).unwrap();
        let store = world(Some(node(true, Some("acme"))));
        let n = write_connector_files(&store, root.path(), &[urn("zd-tickets")], Some(seed.path()))
            .unwrap();
        assert_eq!(n, 1);
        let staged = root.path().join("zd-tickets");
        let body = std::fs::read_to_string(staged.join(HINT_DIR).join(HINT_FILE)).unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["connector"], "zendesk");
        assert_eq!(v["hosts"]["api"], "acme.zendesk.com");
        assert!(staged.join("manifest.json").is_file() && staged.join("execute.js").is_file());
        assert!(!staged.join("passwd").exists());
        assert!(
            !seeded.join(HINT_DIR).exists(),
            "the shared seed tree is never written"
        );
        // Not enabled with an address, or not needing a connector: nothing is staged.
        for (n, requires) in [
            (node(true, None), Some(ZURN)),
            (node(false, Some("acme")), Some(ZURN)),
            (node(true, Some("acme")), None),
        ] {
            let root = tempfile::tempdir().unwrap();
            let seed = tempfile::tempdir().unwrap();
            skill(&seed.path().join("skills"), "zd-tickets", requires);
            let store = world(Some(n));
            assert_eq!(
                write_connector_files(&store, root.path(), &[urn("zd-tickets")], Some(seed.path()))
                    .unwrap(),
                0
            );
            assert!(!root.path().join("zd-tickets").exists());
        }
    }

    #[test]
    fn a_skill_not_staged_here_is_skipped() {
        let root = tempfile::tempdir().unwrap();
        let store = world(Some(node(true, Some("acme"))));
        assert_eq!(
            write_connector_files(&store, root.path(), &[urn("seeded-elsewhere")], None).unwrap(),
            0
        );
    }
}
