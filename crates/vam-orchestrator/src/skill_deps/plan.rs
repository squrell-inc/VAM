//! VI-1.4: detect which lockfiles a skill ships, refuse the unsupported ones, parse the accepted
//! ones into typed package lists and compute the content-addressed cache key. Pure.

use super::files::{SkillFiles, REFUSED_LOCKFILES};
use super::limits::Limits;
use super::marker::MarkerEnv;
use super::npm::{self, NpmPkg};
use super::requirements::PyReq;
use super::target::Target;
use super::{pylock, refused, requirements, DepsError, Ecosystem, Planned, FETCHER_VERSION};
use sha2::{Digest, Sha256};

/// The packages a lockfile asks for.
#[derive(Debug, Clone)]
pub(crate) enum LockPackages {
    Python(Vec<PyReq>),
    Js(Vec<NpmPkg>),
}

/// One ecosystem's plan, with the packages the fetcher needs.
#[derive(Debug, Clone)]
pub(crate) struct LockPlan {
    pub planned: Planned,
    pub packages: LockPackages,
}

/// `plan` with the packages kept and caller-chosen limits.
pub(crate) fn plan_full(
    files: &SkillFiles,
    target: &Target,
    limits: &Limits,
) -> Result<Vec<LockPlan>, DepsError> {
    if let Some(name) = REFUSED_LOCKFILES
        .iter()
        .find(|n| files.refused.iter().any(|r| r == *n))
    {
        return Err(refuse_other_lockfile(name));
    }
    let mut out = Vec::new();

    // Python.
    let python = match (&files.requirements_txt, &files.pylock_toml) {
        (Some(_), Some(_)) => {
            return Err(refused(
                "requirements.txt and pylock.toml are both present; ship one Python lockfile",
            ))
        }
        (Some(bytes), None) => {
            let env = MarkerEnv::new(target);
            Some((bytes, requirements::parse(bytes, &env, limits)?))
        }
        (None, Some(bytes)) => {
            let env = MarkerEnv::new(target);
            Some((bytes, pylock::parse(bytes, target, &env, limits)?))
        }
        (None, None) => None,
    };
    if let Some((bytes, reqs)) = python {
        if !reqs.is_empty() {
            out.push(LockPlan {
                planned: Planned {
                    ecosystem: Ecosystem::Python,
                    key: compute_key(Ecosystem::Python, &target.abi_for(Ecosystem::Python), bytes),
                    package_count: reqs.len() as u32,
                },
                packages: LockPackages::Python(reqs),
            });
        }
    }

    // JS.
    if let Some(pj) = &files.package_json {
        let doc = npm::parse_package_json(pj, limits)?;
        if doc.get("workspaces").is_some() {
            return Err(refused(
                "package.json uses workspaces; workspaces are not supported (ship one package per skill)"));
        }
        let declares = !npm::production_dependency_names(&doc).is_empty();
        if declares && files.package_lock_json.is_none() {
            return Err(refused(
                "package.json lists dependencies but has no package-lock.json; run npm install --package-lock-only"));
        }
    }
    if let Some(lock) = &files.package_lock_json {
        if files.has_node_modules {
            return Err(refused(
                "package-lock.json and a node_modules directory are both present; ship either the lockfile or node_modules, not both; VAM installs from the lockfile"));
        }
        let pkgs = npm::parse(lock, files.package_json.as_deref(), limits)?;
        if !pkgs.is_empty() {
            out.push(LockPlan {
                planned: Planned {
                    ecosystem: Ecosystem::Js,
                    key: compute_key(Ecosystem::Js, &target.abi_for(Ecosystem::Js), lock),
                    package_count: pkgs.len() as u32,
                },
                packages: LockPackages::Js(pkgs),
            });
        }
    }

    let total: usize = out.iter().map(|p| p.planned.package_count as usize).sum();
    if total > limits.max_packages {
        return Err(refused(format!(
            "the lockfiles name {total} packages; at most {} are accepted per skill; drop dependencies the skill does not need",
            limits.max_packages
        )));
    }
    Ok(out)
}

fn refuse_other_lockfile(name: &str) -> DepsError {
    let hint = if name == "uv.lock" {
        "export it: `uv export --format pylock.toml -o pylock.toml`"
    } else {
        "`npm install --package-lock-only` writes a package-lock.json"
    };
    refused(format!("{name} is not accepted; {hint}"))
}

/// VI-1.4: which lockfiles the skill ships and the cache key of each. 0, 1 or 2 entries (one per
/// ecosystem). A skill with no dependencies gives an empty list, not an error.
pub fn plan(files: &SkillFiles, target: &Target) -> Result<Vec<Planned>, DepsError> {
    Ok(plan_full(files, target, &Limits::default())?
        .into_iter()
        .map(|p| p.planned)
        .collect())
}

/// sha256 over a domain-tagged, length-prefixed encoding. Mirror identity and caps are not
/// inputs: the contents are hash-verified, so the tree is the same from any mirror.
pub(crate) fn compute_key(eco: Ecosystem, abi: &str, lock_bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    let mut field = |bytes: &[u8]| {
        h.update((bytes.len() as u64).to_be_bytes());
        h.update(bytes);
    };
    field(b"vam-skill-deps/v1");
    field(&FETCHER_VERSION.to_be_bytes());
    field(eco.as_str().as_bytes());
    field(abi.as_bytes());
    field(lock_bytes);
    hex(&h.finalize())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "ba5a8f0ad76889d0864d568cc3896dd5555f4d97d45017e1e334a2383e16c74a";

    fn t() -> Target {
        Target::new(13, 41)
    }

    fn req() -> Vec<u8> {
        format!("a==1.0.0 --hash=sha256:{H1}\n").into_bytes()
    }

    fn npm_lock() -> Vec<u8> {
        let integrity = {
            use base64::Engine as _;
            format!(
                "sha512-{}",
                base64::engine::general_purpose::STANDARD.encode([3u8; 64])
            )
        };
        serde_json::to_vec(&serde_json::json!({
            "lockfileVersion": 3,
            "packages": {
                "": {"dependencies": {"a": "1.0.0"}},
                "node_modules/a": {"version": "1.0.0", "resolved": "https://registry.npmjs.org/a/-/a-1.0.0.tgz", "integrity": integrity}
            }
        }))
        .expect("json")
    }

    fn refusal(files: &SkillFiles) -> String {
        match plan(files, &t()) {
            Err(DepsError::Refused(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn no_lockfile_is_an_empty_plan() {
        assert!(plan(&SkillFiles::default(), &t()).expect("plan").is_empty());
        let f = SkillFiles::from_entries(&[("SKILL.md", b"x"), ("tool.py", b"print(1)")], false);
        assert!(plan(&f, &t()).expect("plan").is_empty());
    }

    #[test]
    fn empty_dependencies_is_not_a_lockfile() {
        let pj: &[u8] = br#"{"name":"s","version":"1.0.0","dependencies":{}}"#;
        let f = SkillFiles::from_entries(&[("package.json", pj)], false);
        assert!(plan(&f, &t()).expect("plan").is_empty());
        let pj2: &[u8] = br#"{"name":"s","devDependencies":{"x":"1"}}"#;
        let f = SkillFiles::from_entries(&[("package.json", pj2)], true);
        assert!(
            plan(&f, &t()).expect("plan").is_empty(),
            "dev deps and node_modules alone are not a lock"
        );
    }

    #[test]
    fn the_four_other_lockfiles_are_refused_with_a_hint() {
        let m = refusal(&SkillFiles::from_entries(&[("uv.lock", b"x")], false));
        assert!(
            m.contains("uv.lock is not accepted")
                && m.contains("uv export --format pylock.toml -o pylock.toml"),
            "{m}"
        );
        for name in ["bun.lock", "bun.lockb", "yarn.lock", "pnpm-lock.yaml"] {
            let m = refusal(&SkillFiles::from_entries(&[(name, b"x")], false));
            assert!(
                m.starts_with(&format!("skill dependencies: {name} is not accepted"))
                    && m.contains("npm install --package-lock-only"),
                "{name}: {m}"
            );
        }
        // Refused even beside an accepted lockfile.
        let r = req();
        let m = refusal(&SkillFiles::from_entries(
            &[("requirements.txt", r.as_slice()), ("yarn.lock", b"x")],
            false,
        ));
        assert!(m.contains("yarn.lock"), "{m}");
    }

    #[test]
    fn lock_plus_node_modules_is_refused() {
        let l = npm_lock();
        let m = refusal(&SkillFiles::from_entries(
            &[("package-lock.json", l.as_slice())],
            true,
        ));
        assert!(
            m.contains("ship either the lockfile or node_modules, not both"),
            "{m}"
        );
    }

    #[test]
    fn dependencies_without_a_lock_are_refused() {
        let pj: &[u8] = br#"{"dependencies":{"left-pad":"1.0.0"}}"#;
        let m = refusal(&SkillFiles::from_entries(&[("package.json", pj)], false));
        assert!(m.contains("lists dependencies but has no package-lock.json; run npm install --package-lock-only"), "{m}");
        let pj: &[u8] = br#"{"optionalDependencies":{"x":"1.0.0"}}"#;
        assert!(
            refusal(&SkillFiles::from_entries(&[("package.json", pj)], false))
                .contains("no package-lock.json")
        );
    }

    #[test]
    fn workspaces_are_refused() {
        let pj: &[u8] = br#"{"workspaces":["packages/*"]}"#;
        let m = refusal(&SkillFiles::from_entries(&[("package.json", pj)], false));
        assert!(m.contains("workspaces are not supported"), "{m}");
    }

    #[test]
    fn both_python_lockfiles_are_refused() {
        let r = req();
        let m = refusal(&SkillFiles::from_entries(
            &[
                ("requirements.txt", r.as_slice()),
                ("pylock.toml", b"lock-version = \"1.0\"\n"),
            ],
            false,
        ));
        assert!(m.contains("ship one Python lockfile"), "{m}");
    }

    #[test]
    fn both_ecosystems_give_two_entries_python_first() {
        let (r, l) = (req(), npm_lock());
        let f = SkillFiles::from_entries(
            &[
                ("package-lock.json", l.as_slice()),
                ("requirements.txt", r.as_slice()),
            ],
            false,
        );
        let p = plan(&f, &t()).expect("plan");
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].ecosystem, Ecosystem::Python);
        assert_eq!(p[1].ecosystem, Ecosystem::Js);
        assert_eq!((p[0].package_count, p[1].package_count), (1, 1));
        assert_ne!(p[0].key, p[1].key);
        assert_eq!(p[0].key.len(), 64);
    }

    #[test]
    fn a_lock_whose_markers_drop_everything_plans_nothing() {
        let r = format!("a==1 ; sys_platform == 'win32' --hash=sha256:{H1}\n").into_bytes();
        let f = SkillFiles::from_entries(&[("requirements.txt", r.as_slice())], false);
        assert!(plan(&f, &t()).expect("plan").is_empty());
    }

    #[test]
    fn package_cap_is_enforced() {
        let many = (0..3)
            .map(|i| format!("p{i}==1 --hash=sha256:{H1}\n"))
            .collect::<String>()
            .into_bytes();
        let f = SkillFiles::from_entries(&[("requirements.txt", many.as_slice())], false);
        let small = Limits {
            max_packages: 2,
            ..Limits::default()
        };
        assert!(
            matches!(plan_full(&f, &t(), &small), Err(DepsError::Refused(m)) if m.contains("at most 2 are accepted"))
        );
        assert!(plan_full(&f, &t(), &Limits::default()).is_ok());
    }

    #[test]
    fn key_changes_with_each_input() {
        let base = compute_key(Ecosystem::Python, "abi", b"lock");
        assert_eq!(
            base,
            compute_key(Ecosystem::Python, "abi", b"lock"),
            "deterministic"
        );
        assert_ne!(
            base,
            compute_key(Ecosystem::Python, "abi", b"lock "),
            "lockfile byte"
        );
        assert_ne!(
            base,
            compute_key(Ecosystem::Js, "abi", b"lock"),
            "ecosystem"
        );
        assert_ne!(base, compute_key(Ecosystem::Python, "abi2", b"lock"), "abi");
        assert_ne!(
            compute_key(Ecosystem::Python, "ab", b"c"),
            compute_key(Ecosystem::Python, "a", b"bc"),
            "length prefix keeps field boundaries"
        );
    }

    #[test]
    fn key_depends_on_python_minor_glibc_and_patch_not_on_limits() {
        let r = req();
        let f = SkillFiles::from_entries(&[("requirements.txt", r.as_slice())], false);
        let key = |target: &Target| plan(&f, target).expect("plan")[0].key.clone();
        let base = key(&Target::new(13, 41));
        assert_ne!(base, key(&Target::new(12, 41)), "python minor");
        assert_ne!(base, key(&Target::new(13, 39)), "glibc");
        let mut patched = Target::new(13, 41);
        patched.python_patch = 5;
        assert_ne!(
            base,
            key(&patched),
            "python patch (python_full_version markers)"
        );
        let tight = Limits {
            max_requirements: 5,
            max_download_bytes: 1,
            ..Limits::default()
        };
        let other = plan_full(&f, &Target::new(13, 41), &tight).expect("plan");
        assert_eq!(other[0].planned.key, base, "caps are not key inputs");
    }

    #[test]
    fn the_js_key_ignores_python_facts_and_package_json() {
        let l = npm_lock();
        let pj1: &[u8] = br#"{"dependencies":{"a":"1.0.0"}}"#;
        let pj2: &[u8] = br#"{"name":"renamed","dependencies":{"a":"1.0.0"}}"#;
        let k = |pj: &[u8], target: &Target| {
            let f = SkillFiles::from_entries(
                &[("package-lock.json", l.as_slice()), ("package.json", pj)],
                false,
            );
            plan(&f, target).expect("plan")[0].key.clone()
        };
        assert_eq!(k(pj1, &Target::new(13, 41)), k(pj2, &Target::new(13, 41)));
        assert_eq!(k(pj1, &Target::new(13, 41)), k(pj1, &Target::new(10, 17)));
    }
}
