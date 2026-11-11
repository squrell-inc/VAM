//! The base image this node boots: what it holds, whether that is verified, and the install
//! and release check that refuses a skill whose `requiresRuntime` the base cannot meet
//! (workstream `machine-base`, MB-1.5 and MB-1.6).
//!
//! The base manifest is `<image>.base.json` beside the rootfs (`.ext4` replaced), or the file
//! named by `VAM_BASE_MANIFEST`. Its shape (written by `scripts/build-rootfs*.sh`):
//!
//! ```json
//! { "name": "vam/base:2", "version": "...", "digest": "sha256:<64 hex>",
//!   "runtimes": { "python": "3.13.5", "git": "2.47.3" } }
//! ```
//!
//! `digest` is the sha256 of the whole rootfs file. The golden `HashCache` hash is that same
//! value as bare lowercase hex, so the two are compared after dropping an optional `sha256:`
//! prefix and lowercasing ([`normalize_digest`]).
//!
//! On a VM node a missing, unreadable or mismatched manifest means no runtimes: the node
//! reports `verified: false` and every `requiresRuntime` is refused, while skills without one
//! are unaffected. A process node (E9 test nodes) reports the manifest it was given,
//! unverified, and its runtimes count as given.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use serde::Deserialize;
use vam_spec::runtime_requirement::parse_requirement;
use vam_spec::v1::{NodeBase, NodeRuntime, SkillChannel, SkillManifest};

use crate::error::OrchestratorError;

/// The variable that overrides the manifest path.
pub const BASE_MANIFEST_VAR: &str = "VAM_BASE_MANIFEST";

#[derive(Debug, Deserialize)]
struct BaseManifestFile {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    digest: String,
    #[serde(default)]
    runtimes: BTreeMap<String, String>,
}

/// What this node knows about its base. `runtimes` is what the node vouches for: empty on a
/// VM node whose manifest is missing or does not match the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoadedBase {
    pub name: String,
    pub version: String,
    pub digest: String,
    pub verified: bool,
    pub vm_node: bool,
    pub runtimes: BTreeMap<String, String>,
}

/// `sha256:ABC...` and `abc...` name the same digest.
pub fn normalize_digest(d: &str) -> String {
    d.trim()
        .strip_prefix("sha256:")
        .unwrap_or(d.trim())
        .to_ascii_lowercase()
}

/// `<rootfs with .ext4 replaced>.base.json`; `None` for an empty path (a process node).
pub fn sibling_manifest_path(rootfs: &Path) -> Option<PathBuf> {
    if rootfs.as_os_str().is_empty() {
        return None;
    }
    Some(rootfs.with_extension("base.json"))
}

fn unverified_empty(vm_node: bool) -> LoadedBase {
    LoadedBase {
        name: String::new(),
        version: String::new(),
        digest: String::new(),
        verified: false,
        vm_node,
        runtimes: BTreeMap::new(),
    }
}

/// Load the base manifest. `sha256_of_rootfs` is the golden `HashCache` hash (bare hex) and is
/// called only on a VM node whose manifest parsed. `None`: a process node with no manifest.
pub fn load(
    rootfs: &Path,
    manifest_override: Option<&Path>,
    vm_node: bool,
    sha256_of_rootfs: impl FnOnce(&Path) -> Result<String, OrchestratorError>,
) -> Option<LoadedBase> {
    let path = match manifest_override {
        Some(p) => p.to_path_buf(),
        None => sibling_manifest_path(rootfs)?,
    };
    let parsed = std::fs::read_to_string(&path)
        .map_err(|e| e.to_string())
        .and_then(|raw| {
            serde_json::from_str::<BaseManifestFile>(&raw).map_err(|e| format!("not valid: {e}"))
        });
    let file = match parsed {
        Ok(f) => f,
        Err(why) => {
            if vm_node || manifest_override.is_some() {
                tracing::warn!(
                    manifest = %path.display(),
                    "base manifest unusable ({why}); this node reports no runtimes. \
                     Build the image with scripts/build-rootfs.sh or set {BASE_MANIFEST_VAR}"
                );
            }
            return vm_node.then(|| unverified_empty(true));
        }
    };
    let mut base = LoadedBase {
        name: file.name,
        version: file.version,
        digest: file.digest,
        verified: false,
        vm_node,
        runtimes: file.runtimes,
    };
    if vm_node {
        let matches = sha256_of_rootfs(rootfs)
            .map(|have| have == normalize_digest(&base.digest))
            .unwrap_or(false);
        if matches {
            base.verified = true;
        } else {
            tracing::warn!(
                manifest = %path.display(),
                rootfs = %rootfs.display(),
                "base manifest digest does not match the rootfs this node boots; \
                 this node reports no runtimes"
            );
            base.runtimes.clear();
        }
    }
    Some(base)
}

static BASE: OnceLock<LoadedBase> = OnceLock::new();

/// Remember the base for the life of the process. A second call is ignored.
pub fn install(base: LoadedBase) {
    let _ = BASE.set(base);
}

/// Load from the environment (`VAM_BASE_MANIFEST`, `VAM_ROOTFS_PATH`) using the golden hash
/// cache, and install the result. Reads the rootfs in full on a cold cache: call from a
/// blocking context.
pub fn install_from_env(vm_node: bool) {
    let rootfs = crate::golden::resolve_rootfs_path();
    let over = std::env::var(BASE_MANIFEST_VAR)
        .ok()
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    if let Some(base) = load(
        &rootfs,
        over.as_deref(),
        vm_node,
        crate::golden::file_sha256,
    ) {
        tracing::info!(
            base = %base.name,
            verified = base.verified,
            runtimes = base.runtimes.len(),
            "node base"
        );
        install(base);
    }
}

fn to_proto(base: &LoadedBase) -> NodeBase {
    NodeBase {
        name: base.name.clone(),
        version: base.version.clone(),
        digest: base.digest.clone(),
        verified: base.verified,
        runtimes: base
            .runtimes
            .iter()
            .map(|(name, version)| NodeRuntime {
                name: name.clone(),
                version: version.clone(),
            })
            .collect(),
    }
}

/// `GET /v1/node` `status.base`; `None` when the node has no base manifest.
pub fn report() -> Option<NodeBase> {
    BASE.get().map(to_proto)
}

fn short_digest(d: &str) -> String {
    normalize_digest(d).chars().take(12).collect()
}

fn base_label(base: &LoadedBase) -> String {
    format!("{} (sha256:{})", base.name, short_digest(&base.digest))
}

/// Refuse a skill whose `requiresRuntime` the node's base cannot meet. A skill without one is
/// never affected. Uses the installed base; see [`check_against`].
pub fn check_requires_runtime(manifest: &SkillManifest) -> Result<(), OrchestratorError> {
    check_against(BASE.get(), manifest)
}

/// The check against an explicit base (`None`: the node has no base manifest).
pub fn check_against(
    base: Option<&LoadedBase>,
    manifest: &SkillManifest,
) -> Result<(), OrchestratorError> {
    if manifest.requires_runtime.is_empty() {
        return check_interpreters(base, manifest);
    }
    let urn = manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .unwrap_or_default();
    let refuse = |msg: String| Err(OrchestratorError::InvalidContract(msg));
    let mut needs: Vec<(&String, &String)> = manifest.requires_runtime.iter().collect();
    needs.sort();
    for (runtime, raw) in needs {
        let shown = if raw.trim().is_empty() {
            "*"
        } else {
            raw.trim()
        };
        let lead = format!("skill {urn} needs {runtime} {shown}");
        let Some(base) = base else {
            return refuse(format!("{lead}; this node reports no base image"));
        };
        if base.vm_node && !base.verified {
            let name = if base.name.is_empty() {
                "its base".to_string()
            } else {
                format!("base {}", base_label(base))
            };
            return refuse(format!(
                "{lead}; this node's {name} is unverified, so it reports no runtimes"
            ));
        }
        let requirement = parse_requirement(raw)
            .map_err(|e| OrchestratorError::InvalidContract(format!("skill {urn}: {e}")))?;
        match base.runtimes.get(runtime) {
            None => {
                return refuse(format!(
                    "{lead}; this node's base {} has no {runtime}",
                    base_label(base)
                ))
            }
            Some(have) if !requirement.satisfied_by(have) => {
                return refuse(format!("{lead}; {} has {runtime} {have}", base.name))
            }
            Some(_) => {}
        }
    }
    check_interpreters(base, manifest)
}

/// The base runtime a `localExec` command needs, under the name the base manifest lists it
/// (`scripts/build-rootfs.sh`). These are the interpreters the guest resolves by bare name
/// (`vam-runtime/src/exec.rs`); an absolute path is matched by its file name. Anything else
/// (`/bin/sh`, a binary shipped in the skill) is not a known interpreter and is not checked.
fn interpreter_runtime(command: &str) -> Option<&'static str> {
    match command.rsplit('/').next().unwrap_or(command) {
        "python" | "python3" => Some("python"),
        "bun" => Some("bun"),
        "node" => Some("node"),
        _ => None,
    }
}

/// Refuse a skill with a `localExec` tool whose interpreter the node's base does not have, so
/// it fails at install or release and not at run time (launch audit S3). The standard base has
/// python and bun, the minimal base bun only, and no base has node.
///
/// Only a base the node vouches for is held to this: a verified VM base, or the manifest a
/// process node was given. With no base manifest, or an unverified one, the node does not know
/// what its image holds, so the skill is admitted as before; this keeps nodes booting an image
/// built before base manifests existed working. `requiresRuntime` is stricter (it refuses
/// there) because the author asked for it explicitly.
fn check_interpreters(
    base: Option<&LoadedBase>,
    manifest: &SkillManifest,
) -> Result<(), OrchestratorError> {
    use vam_spec::v1::tool_spec::ExecutionProvider;
    let Some(base) = base else {
        return Ok(());
    };
    if base.vm_node && !base.verified {
        return Ok(());
    }
    let urn = manifest
        .resource_id
        .as_ref()
        .map(|r| r.urn.as_str())
        .unwrap_or_default();
    for tool in &manifest.tools {
        let Some(ExecutionProvider::LocalExec(local)) = &tool.execution_provider else {
            continue;
        };
        let command = local.command.trim();
        let Some(runtime) = interpreter_runtime(command) else {
            continue;
        };
        if !base.runtimes.contains_key(runtime) {
            let has = if base.runtimes.is_empty() {
                "none".to_string()
            } else {
                base.runtimes.keys().cloned().collect::<Vec<_>>().join(", ")
            };
            return Err(OrchestratorError::InvalidContract(format!(
                "skill {urn} tool {} runs {command}; this node's base {} has no {runtime} \
                 (it has: {has}). Use an interpreter the base has, or run the node on a base \
                 that has {runtime}",
                tool.name,
                base_label(base)
            )));
        }
    }
    Ok(())
}

/// Release check for a skill package: the owner's draft, read from the store. A missing draft
/// passes here; the release reports it.
pub fn check_draft_package(
    store: &crate::SqliteWorkflowStore,
    urn: &str,
    owner_user_id: &str,
) -> Result<(), OrchestratorError> {
    check_draft_package_against(BASE.get(), store, urn, owner_user_id)
}

fn check_draft_package_against(
    base: Option<&LoadedBase>,
    store: &crate::SqliteWorkflowStore,
    urn: &str,
    owner_user_id: &str,
) -> Result<(), OrchestratorError> {
    match store.get_skill_package_for_query_visible(
        urn,
        None,
        SkillChannel::Draft,
        owner_user_id,
        false,
    ) {
        Ok(record) => match record.spec.as_ref() {
            Some(manifest) => check_against(base, manifest),
            None => Ok(()),
        },
        Err(OrchestratorError::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Release check for a catalog skill (`PUT /v1/skills`): the owner's draft, read from the store.
pub fn check_draft_skill(
    store: &crate::SqliteWorkflowStore,
    urn: &str,
    owner_user_id: &str,
) -> Result<(), OrchestratorError> {
    check_draft_skill_against(BASE.get(), store, urn, owner_user_id)
}

fn check_draft_skill_against(
    base: Option<&LoadedBase>,
    store: &crate::SqliteWorkflowStore,
    urn: &str,
    owner_user_id: &str,
) -> Result<(), OrchestratorError> {
    match store.get_skill_visible(urn, owner_user_id) {
        Ok(manifest) => check_against(base, &manifest),
        Err(OrchestratorError::NotFound(_)) => Ok(()),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod node_base_tests {
    use super::*;
    use vam_spec::v1::ResourceId;

    const HEX: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn write_manifest(dir: &Path, name: &str, digest: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(
            &p,
            format!(
                r#"{{"name":"vam/base:2","version":"2.0.0","digest":"{digest}",
                    "runtimes":{{"python":"3.13.5","git":"2.47.3"}}}}"#
            ),
        )
        .expect("write");
        p
    }

    fn no_hash(_: &Path) -> Result<String, OrchestratorError> {
        Err(OrchestratorError::Core("not called".into()))
    }

    #[test]
    fn manifest_beside_the_image() {
        let dir = tempfile::tempdir().expect("dir");
        let rootfs = dir.path().join("rootfs.ext4");
        write_manifest(dir.path(), "rootfs.base.json", &format!("sha256:{HEX}"));
        let base = load(&rootfs, None, true, |_| Ok(HEX.to_string())).expect("base");
        assert!(base.verified && base.vm_node);
        assert_eq!(base.name, "vam/base:2");
        assert_eq!(base.runtimes["python"], "3.13.5");
        assert_eq!(
            sibling_manifest_path(&rootfs).expect("p"),
            dir.path().join("rootfs.base.json")
        );
    }

    #[test]
    fn override_variable_path_wins() {
        let dir = tempfile::tempdir().expect("dir");
        let rootfs = dir.path().join("rootfs.ext4");
        write_manifest(dir.path(), "rootfs.base.json", "sha256:other");
        let over = write_manifest(dir.path(), "elsewhere.json", HEX);
        let base = load(&rootfs, Some(&over), true, |_| Ok(HEX.to_string())).expect("base");
        assert!(base.verified);
    }

    #[test]
    fn digest_forms_compare_equal() {
        assert_eq!(
            normalize_digest(&format!("sha256:{}", HEX.to_uppercase())),
            HEX
        );
        assert_eq!(normalize_digest(HEX), HEX);
        let dir = tempfile::tempdir().expect("dir");
        let rootfs = dir.path().join("r.ext4");
        let over = write_manifest(
            dir.path(),
            "m.json",
            &format!("sha256:{}", HEX.to_uppercase()),
        );
        let base = load(&rootfs, Some(&over), true, |_| Ok(HEX.to_string())).expect("base");
        assert!(base.verified);
    }

    #[test]
    fn the_hash_cache_form_is_a_bare_sha256_of_the_whole_file() {
        // golden's HashCache hashes the whole file and formats it with `{:x}`.
        use sha2::{Digest, Sha256};
        let dir = tempfile::tempdir().expect("dir");
        let img = dir.path().join("rootfs.ext4");
        std::fs::write(&img, b"not really ext4").expect("write");
        let expect = format!("{:x}", Sha256::digest(b"not really ext4"));
        let got = crate::golden::file_sha256(&img).expect("hash");
        assert_eq!(got, expect);
        assert_eq!(got.len(), 64);
        write_manifest(dir.path(), "rootfs.base.json", &format!("sha256:{expect}"));
        let base = load(&img, None, true, crate::golden::file_sha256).expect("base");
        assert!(base.verified);
    }

    #[test]
    fn digest_mismatch_means_no_runtimes() {
        let dir = tempfile::tempdir().expect("dir");
        let rootfs = dir.path().join("rootfs.ext4");
        write_manifest(dir.path(), "rootfs.base.json", &format!("sha256:{HEX}"));
        let base = load(&rootfs, None, true, |_| Ok("f".repeat(64))).expect("base");
        assert!(!base.verified);
        assert!(base.runtimes.is_empty());
        assert_eq!(base.name, "vam/base:2");
    }

    #[test]
    fn missing_or_broken_file_on_a_vm_node_means_no_runtimes() {
        let dir = tempfile::tempdir().expect("dir");
        let rootfs = dir.path().join("rootfs.ext4");
        let base = load(&rootfs, None, true, no_hash).expect("vm node always reports");
        assert!(!base.verified && base.runtimes.is_empty() && base.name.is_empty());
        std::fs::write(dir.path().join("rootfs.base.json"), "{ nope").expect("write");
        let base = load(&rootfs, None, true, no_hash).expect("base");
        assert!(!base.verified && base.runtimes.is_empty());
    }

    #[test]
    fn process_node_reports_the_manifest_it_was_given() {
        let dir = tempfile::tempdir().expect("dir");
        let over = write_manifest(dir.path(), "m.json", "sha256:fixture-not-a-real-digest");
        let base = load(Path::new(""), Some(&over), false, no_hash).expect("base");
        assert!(!base.verified && !base.vm_node);
        assert_eq!(base.runtimes["python"], "3.13.5");
        assert!(load(Path::new(""), None, false, no_hash).is_none());
        let proto = to_proto(&base);
        assert_eq!(proto.name, "vam/base:2");
        assert_eq!(proto.runtimes.len(), 2);
    }

    fn skill(reqs: &[(&str, &str)]) -> SkillManifest {
        SkillManifest {
            resource_id: Some(ResourceId {
                urn: "urn:vam:skill:hub.vam.dev/acme/s".into(),
            }),
            requires_runtime: reqs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ..Default::default()
        }
    }

    fn base(vm_node: bool, verified: bool) -> LoadedBase {
        LoadedBase {
            name: "vam/base:2".into(),
            version: "2.0.0".into(),
            digest: format!("sha256:{HEX}"),
            verified,
            vm_node,
            runtimes: [("python", "3.13.5"), ("git", "2.47.3")]
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    #[test]
    fn requires_runtime_missing_runtime_is_named() {
        let err = check_against(Some(&base(true, true)), &skill(&[("java", ">=21")]))
            .expect_err("refused")
            .to_string();
        assert!(
            err.contains("urn:vam:skill:hub.vam.dev/acme/s needs java >=21"),
            "{err}"
        );
        assert!(
            err.contains("vam/base:2 (sha256:0123456789ab) has no java"),
            "{err}"
        );
    }

    #[test]
    fn requires_runtime_version_too_old() {
        let err = check_against(Some(&base(true, true)), &skill(&[("python", ">=3.14")]))
            .expect_err("refused")
            .to_string();
        assert!(
            err.contains("needs python >=3.14; vam/base:2 has python 3.13.5"),
            "{err}"
        );
    }

    #[test]
    fn requires_runtime_satisfied_and_no_field() {
        let b = base(true, true);
        check_against(Some(&b), &skill(&[("python", ">=3.11"), ("git", "*")])).expect("ok");
        check_against(Some(&b), &skill(&[("python", "=3.13")])).expect("ok");
        check_against(Some(&b), &skill(&[])).expect("no field is never affected");
        check_against(None, &skill(&[])).expect("no base, no field");
        let bare = unverified_empty(true);
        check_against(Some(&bare), &skill(&[])).expect("unverified base, no field");
    }

    #[test]
    fn requires_runtime_unverified_vm_base_refuses_any_requirement() {
        let mut b = base(true, false);
        b.runtimes.clear();
        let err = check_against(Some(&b), &skill(&[("git", "*")]))
            .expect_err("refused")
            .to_string();
        assert!(err.contains("unverified"), "{err}");
        let err = check_against(Some(&unverified_empty(true)), &skill(&[("git", "*")]))
            .expect_err("refused")
            .to_string();
        assert!(err.contains("unverified"), "{err}");
    }

    #[test]
    fn requires_runtime_process_node_counts_the_manifest_as_given() {
        check_against(Some(&base(false, false)), &skill(&[("python", ">=3.11")])).expect("ok");
        assert!(check_against(Some(&base(false, false)), &skill(&[("java", ">=21")])).is_err());
    }

    fn skill_running(command: &str) -> SkillManifest {
        let mut s = skill(&[]);
        s.tools = vec![vam_spec::v1::ToolSpec {
            name: "probe".into(),
            execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::LocalExec(
                vam_spec::v1::LocalExecSpec {
                    command: command.into(),
                    entrypoint: "./probe.py".into(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        }];
        s
    }

    #[test]
    fn local_exec_interpreter_the_base_lacks_is_refused() {
        // The test base has python and git, no bun or node.
        let b = base(true, true);
        for command in ["bun", "node", "/usr/local/bin/bun"] {
            let err = check_against(Some(&b), &skill_running(command))
                .expect_err(command)
                .to_string();
            assert!(err.contains(&format!("tool probe runs {command}")), "{err}");
            assert!(err.contains("(it has: git, python)"), "{err}");
        }
        check_against(Some(&b), &skill_running("python3")).expect("python3 is on the base");
        check_against(Some(&b), &skill_running("/bin/sh")).expect("not a known interpreter");
        // A process node holds skills to the manifest it was given.
        assert!(check_against(Some(&base(false, false)), &skill_running("bun")).is_err());
    }

    #[test]
    fn local_exec_minimal_base_has_bun_only() {
        let mut minimal = base(true, true);
        minimal.name = "vam/base:1".into();
        minimal.runtimes = [("bun".to_string(), "1.4.4".to_string())].into();
        check_against(Some(&minimal), &skill_running("bun")).expect("bun");
        let err = check_against(Some(&minimal), &skill_running("python3"))
            .expect_err("no python on the minimal base")
            .to_string();
        assert!(err.contains("has no python (it has: bun)"), "{err}");
    }

    #[test]
    fn local_exec_is_not_checked_without_a_base_the_node_vouches_for() {
        check_against(None, &skill_running("bun")).expect("no manifest: as before");
        check_against(Some(&unverified_empty(true)), &skill_running("python3"))
            .expect("unverified VM base: as before");
    }

    #[test]
    fn requires_runtime_no_base_at_all_refuses() {
        let err = check_against(None, &skill(&[("python", "*")]))
            .expect_err("refused")
            .to_string();
        assert!(err.contains("no base image"), "{err}");
    }

    #[test]
    fn requires_runtime_a_draft_admitted_before_the_check_is_refused_at_release() {
        let store = crate::SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_catalog_schema().expect("schema");
        let mut draft = skill(&[("java", ">=21")]);
        draft.version = Some(vam_spec::v1::Version {
            major: 1,
            ..Default::default()
        });
        draft.description = "needs java".into();
        let owner = crate::users::LOCAL_USER_ID;
        store
            .publish_skill_draft_with_owner(&draft, owner)
            .expect("draft stored without the check");
        let urn = "urn:vam:skill:hub.vam.dev/acme/s";
        let err = check_draft_skill_against(Some(&base(true, true)), &store, urn, owner)
            .expect_err("refused at release")
            .to_string();
        assert!(err.contains("has no java"), "{err}");
        // A node whose base has java passes, and a missing draft is left to the release.
        let mut with_java = base(true, true);
        with_java.runtimes.insert("java".into(), "21.0.2".into());
        check_draft_skill_against(Some(&with_java), &store, urn, owner).expect("ok");
        check_draft_skill_against(
            Some(&base(true, true)),
            &store,
            "urn:vam:skill:hub.vam.dev/acme/none",
            owner,
        )
        .expect("no draft is the release's error");
        check_draft_package_against(Some(&base(true, true)), &store, urn, owner)
            .expect("no package draft");
    }
}
