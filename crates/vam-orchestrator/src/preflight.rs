//! `vamd --preflight`: the node's start rules as JSON, for `vam node init --check`
//! (workstream `machine-base`, MB-1.8, open question 11).
//!
//! The jailer rules are `vam_core::JailConfig::preflight` and the variable rule is
//! `daemon::missing_vm_node_vars`; this module only reports them, so `vam-cli` needs no
//! `vam-core` dependency and the rules exist once. It reads and writes nothing it was not
//! asked to read, starts no daemon and never offers `--no-jailer` as a fix.
//!
//! `--root DIR` is a test seam. A tree cannot stand in for the host's cgroups, uids and
//! mounts, so under it the host-facing jailer rules are reported `skipped`; the rules that
//! read only the configuration still run.

use std::path::Path;

use serde::Serialize;

use crate::daemon::{missing_vm_node_vars, DaemonConfig};

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    /// `ok`, `missing`, `wrong` or `skipped`.
    pub status: &'static str,
    pub detail: String,
    pub fix: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct Report {
    pub checks: Vec<Check>,
}

fn ok(name: &'static str, detail: &str) -> Check {
    Check {
        name,
        status: "ok",
        detail: detail.to_string(),
        fix: String::new(),
    }
}

/// Run the rules. `get` reads the environment (a test passes a map); `root` is the seam.
pub fn run(
    config: &DaemonConfig,
    get: impl Fn(&str) -> Option<String>,
    root: Option<&Path>,
) -> Report {
    let mut checks = Vec::new();

    let missing = missing_vm_node_vars(&get, config.use_jailer);
    checks.push(if missing.is_empty() {
        ok("vamd-env", "the variables a microVM node needs are set")
    } else {
        Check {
            name: "vamd-env",
            status: "missing",
            detail: crate::daemon::vm_node_config_error(&missing),
            fix: format!(
                "set {} in /etc/vam/vamd.env (`vam node init` writes it)",
                missing.join(", ")
            ),
        }
    });

    checks.push(if config.jailer_uid == 0 || config.jailer_gid == 0 {
        Check {
            name: "jailer-ids",
            status: "wrong",
            detail: format!(
                "the jailer uid/gid are {}/{}; a jailed VMM must not run as root",
                config.jailer_uid, config.jailer_gid
            ),
            fix: format!(
                "unset VAMD_JAILER_UID and VAMD_JAILER_GID (default {}), or give them a non-zero value",
                vam_core::jail::DEFAULT_JAIL_UID
            ),
        }
    } else {
        ok("jailer-ids", "the jailer runs as an unprivileged uid/gid")
    });

    checks.push(match root {
        Some(_) => Check {
            name: "jailer-rules",
            status: "skipped",
            detail: "not evaluated under --root: a tree is not the host's cgroups and mounts"
                .to_string(),
            fix: String::new(),
        },
        None if !config.use_jailer => Check {
            // The node refuses to start in this state unless the operator chose it; the
            // check reports it and never suggests it.
            name: "jailer-rules",
            status: "wrong",
            detail: "the jailer is switched off on this node".to_string(),
            fix:
                "remove the jailer opt-out from the node configuration: the jailer is on by default"
                    .to_string(),
        },
        None => host_jailer_rules(config),
    });

    Report { checks }
}

fn host_jailer_rules(config: &DaemonConfig) -> Check {
    let jail_cfg = vam_core::JailConfig {
        uid: config.jailer_uid,
        gid: config.jailer_gid,
        cgroup_version: config.jailer_cgroup_version,
        ..Default::default()
    };
    let kernel = crate::golden::resolve_guest_kernel_path();
    let rootfs = crate::golden::resolve_rootfs_path();
    let drive9_root = std::env::var("VAM_DRIVE9_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("/var/lib/vam/drive9"));
    let run_dir = jail_cfg.base_dir.join("run");
    let inputs = vam_core::PreflightInputs {
        firecracker_bin: &config.firecracker_bin,
        jailer_bin: &config.jailer_bin,
        shared_assets: &[
            ("guest kernel", kernel.as_path()),
            ("guest rootfs", rootfs.as_path()),
        ],
        same_fs: &[
            ("Drive9 root", drive9_root.as_path()),
            ("vsock run dir", run_dir.as_path()),
        ],
    };
    match jail_cfg.preflight(&inputs) {
        Ok(()) => ok("jailer-rules", "the host can jail Firecracker"),
        Err(e) => Check {
            name: "jailer-rules",
            status: "wrong",
            detail: e.to_string(),
            fix: "resolve the item above; vamd will not start a microVM node it cannot jail"
                .to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| m.get(k).cloned()
    }

    fn status(r: &Report, name: &str) -> &'static str {
        r.checks
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.status)
            .unwrap_or("absent")
    }

    #[test]
    fn missing_variables_are_named_and_the_fix_is_not_no_jailer() {
        let mut cfg = DaemonConfig::parse();
        cfg.use_jailer = true;
        let r = run(&cfg, env(&[]), Some(Path::new("/nonexistent")));
        assert_eq!(status(&r, "vamd-env"), "missing");
        let fixes: String = r.checks.iter().map(|c| c.fix.clone()).collect();
        assert!(fixes.contains("VAMD_JAILER_BIN"), "{fixes}");
        assert!(!fixes.contains("--no-jailer"), "{fixes}");
        assert_eq!(status(&r, "jailer-rules"), "skipped");
    }

    #[test]
    fn variables_set_and_ids_unprivileged_is_all_ok_under_root() {
        let mut cfg = DaemonConfig::parse();
        cfg.use_jailer = true;
        cfg.jailer_uid = 65400;
        cfg.jailer_gid = 65400;
        let r = run(
            &cfg,
            env(&[
                ("VAMD_FIRECRACKER_BIN", "/f"),
                ("VAMD_JAILER_BIN", "/j"),
                ("VAM_ROOTFS_PATH", "/r"),
            ]),
            Some(Path::new("/t")),
        );
        assert_eq!(status(&r, "vamd-env"), "ok");
        assert_eq!(status(&r, "jailer-ids"), "ok");
    }

    #[test]
    fn root_ids_are_wrong() {
        let mut cfg = DaemonConfig::parse();
        cfg.jailer_uid = 0;
        let r = run(&cfg, env(&[]), Some(Path::new("/t")));
        assert_eq!(status(&r, "jailer-ids"), "wrong");
    }

    #[test]
    fn a_disabled_jailer_is_reported_without_offering_it() {
        let mut cfg = DaemonConfig::parse();
        cfg.use_jailer = false;
        let r = run(&cfg, env(&[]), None);
        let c = r.checks.iter().find(|c| c.name == "jailer-rules").cloned();
        let c = c.unwrap_or_else(|| ok("none", ""));
        assert_eq!(c.status, "wrong");
        assert!(!c.fix.contains("no-jailer") && !c.detail.contains("no-jailer"));
    }
}
