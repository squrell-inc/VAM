//! `package-lock.json` (lockfileVersion 2 or 3) plus `package.json`: production registry
//! packages locked by integrity. The `resolved` URL is read only to check its shape; it is never
//! fetched. Pure.

use super::limits::Limits;
use super::names::{is_plain_semver, valid_npm_name};
use super::{refused, DepsError};
use base64::Engine as _;
use serde_json::{Map, Value};

const LOCK: &str = "package-lock.json";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HashAlg {
    Sha256,
    Sha384,
    Sha512,
}

impl HashAlg {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            HashAlg::Sha256 => "sha256",
            HashAlg::Sha384 => "sha384",
            HashAlg::Sha512 => "sha512",
        }
    }

    fn digest_len(self) -> usize {
        match self {
            HashAlg::Sha256 => 32,
            HashAlg::Sha384 => 48,
            HashAlg::Sha512 => 64,
        }
    }
}

/// An npm `integrity` value: the strongest accepted digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Integrity {
    pub alg: HashAlg,
    pub digest: Vec<u8>,
}

/// One production package to fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NpmPkg {
    pub name: String,
    pub version: String,
    /// The lock key: `node_modules/a` or `node_modules/a/node_modules/b`.
    pub path: String,
    pub integrity: Integrity,
}

fn pkg_refusal(name: &str, version: &str, text: &str) -> DepsError {
    refused(format!("package '{name}' {version} {text}"))
}

fn json_err(file: &str, e: &serde_json::Error) -> DepsError {
    refused(format!(
        "{file} is not valid JSON ({}); regenerate it with npm install --package-lock-only",
        e.classify_label()
    ))
}

trait ClassifyLabel {
    fn classify_label(&self) -> &'static str;
}

impl ClassifyLabel for serde_json::Error {
    fn classify_label(&self) -> &'static str {
        use serde_json::error::Category;
        match self.classify() {
            Category::Io => "unreadable",
            Category::Syntax => "syntax error",
            Category::Data => "unexpected shape",
            Category::Eof => "truncated or nested too deeply",
        }
    }
}

/// `node_modules/(@scope/)?name(/node_modules/(@scope/)?name)*`, every name valid by npm rules.
/// Returns the innermost package name.
pub(crate) fn parse_key(key: &str) -> Option<String> {
    let segs: Vec<&str> = key.split('/').collect();
    let mut i = 0;
    let mut last = None;
    while i < segs.len() {
        if segs[i] != "node_modules" {
            return None;
        }
        i += 1;
        let first = *segs.get(i)?;
        i += 1;
        let name = if first.starts_with('@') {
            let second = *segs.get(i)?;
            i += 1;
            format!("{first}/{second}")
        } else {
            first.to_string()
        };
        if !valid_npm_name(&name) {
            return None;
        }
        last = Some(name);
    }
    last
}

fn str_list(v: Option<&Value>) -> Vec<String> {
    v.and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

/// `os` / `cpu` constraint lists: `["linux"]`, `["!win32"]`.
fn excludes(list: &[String], want: &str) -> bool {
    if list.is_empty() {
        return false;
    }
    if list.iter().any(|i| i.strip_prefix('!') == Some(want)) {
        return true;
    }
    let positive: Vec<&String> = list.iter().filter(|i| !i.starts_with('!')).collect();
    !positive.is_empty() && !positive.iter().any(|i| i.as_str() == want)
}

fn is_true(entry: &Map<String, Value>, key: &str) -> bool {
    entry.get(key).and_then(|v| v.as_bool()) == Some(true)
}

fn not_production(entry: &Map<String, Value>) -> bool {
    if is_true(entry, "dev") || is_true(entry, "devOptional") {
        return true;
    }
    is_true(entry, "optional")
        && (excludes(&str_list(entry.get("os")), "linux")
            || excludes(&str_list(entry.get("cpu")), "x64"))
}

/// What kind of non-registry source a `resolved` value is.
fn non_registry_kind(resolved: &str) -> Option<&'static str> {
    let r = resolved.trim();
    if r.starts_with("git+")
        || r.starts_with("git:")
        || r.starts_with("git@")
        || r.starts_with("ssh:")
        || r.starts_with("github:")
        || r.starts_with("gitlab:")
        || r.starts_with("bitbucket:")
        || r.starts_with("gist:")
    {
        return Some("git");
    }
    if r.starts_with("file:") || r.starts_with("link:") || r.starts_with('/') || r.starts_with('.')
    {
        return Some("file");
    }
    let Ok(url) = url::Url::parse(r) else {
        return Some("url");
    };
    let registry_shape = matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some()
        && url.path().contains("/-/")
        && url.path().ends_with(".tgz");
    if registry_shape {
        None
    } else {
        Some("url")
    }
}

fn parse_integrity(raw: &str) -> Result<Integrity, &'static str> {
    let mut best: Option<Integrity> = None;
    let mut saw_sha1 = false;
    for token in raw.split_whitespace() {
        let Some((alg, b64)) = token.split_once('-') else {
            continue;
        };
        let alg = match alg {
            "sha512" => HashAlg::Sha512,
            "sha384" => HashAlg::Sha384,
            "sha256" => HashAlg::Sha256,
            "sha1" => {
                saw_sha1 = true;
                continue;
            }
            _ => continue,
        };
        // Options (`?foo`) after the digest are ignored by SRI; npm writes none.
        let b64 = b64.split('?').next().unwrap_or(b64);
        let Ok(digest) = base64::engine::general_purpose::STANDARD.decode(b64) else {
            continue;
        };
        if digest.len() != alg.digest_len() {
            continue;
        }
        let rank = |a: HashAlg| match a {
            HashAlg::Sha512 => 3,
            HashAlg::Sha384 => 2,
            HashAlg::Sha256 => 1,
        };
        if best.as_ref().is_none_or(|b| rank(alg) > rank(b.alg)) {
            best = Some(Integrity { alg, digest });
        }
    }
    match best {
        Some(i) => Ok(i),
        None if saw_sha1 => Err("integrity is sha1; sha512 required"),
        None => Err("has no usable integrity hash (sha512 required)"),
    }
}

pub(crate) fn parse(
    lock: &[u8],
    package_json: Option<&[u8]>,
    limits: &Limits,
) -> Result<Vec<NpmPkg>, DepsError> {
    if lock.len() as u64 > limits.max_lockfile_bytes {
        return Err(refused(format!(
            "{LOCK} is larger than {} MiB; drop unused dependencies",
            limits.max_lockfile_bytes / (1024 * 1024)
        )));
    }
    let doc: Value = serde_json::from_slice(lock).map_err(|e| json_err(LOCK, &e))?;
    let version = doc.get("lockfileVersion").and_then(|v| v.as_u64());
    if !matches!(version, Some(2) | Some(3)) {
        return Err(refused(format!(
            "{LOCK} has lockfileVersion {}; regenerate it with npm 7 or newer (npm install --package-lock-only)",
            version.map_or("(missing)".to_string(), |v| v.to_string())
        )));
    }
    let packages = doc
        .get("packages")
        .and_then(|p| p.as_object())
        .ok_or_else(|| {
            refused(format!(
                "{LOCK} has no packages map; regenerate it with npm install --package-lock-only"
            ))
        })?;

    if let Some(pj) = package_json {
        check_package_json(pj, packages.get(""), limits)?;
    }

    let mut out = Vec::new();
    for (key, entry) in packages {
        if key.is_empty() {
            continue;
        }
        let Some(entry) = entry.as_object() else {
            return Err(refused(format!(
                "{LOCK} has an invalid package path '{key}'"
            )));
        };
        if not_production(entry) {
            continue;
        }
        let Some(name) = parse_key(key) else {
            return Err(refused(format!(
                "{LOCK} has an invalid package path '{key}'"
            )));
        };
        let version = entry.get("version").and_then(|v| v.as_str()).unwrap_or("");
        let shown_version = if version.is_empty() {
            "(no version)"
        } else {
            version
        };

        let not_registry = |kind: &str| {
            pkg_refusal(
                &name,
                shown_version,
                &format!("is a {kind} dependency; only registry packages locked by integrity are accepted"),
            )
        };
        if is_true(entry, "link") {
            return Err(not_registry("link"));
        }
        if is_true(entry, "inBundle") || is_true(entry, "bundled") {
            return Err(not_registry("bundled"));
        }
        if entry
            .get("name")
            .and_then(|n| n.as_str())
            .is_some_and(|n| n != name)
        {
            return Err(not_registry("aliased"));
        }
        match entry.get("resolved").and_then(|r| r.as_str()) {
            Some(r) if r.starts_with("npm:") => return Err(not_registry("aliased")),
            Some(r) => {
                if let Some(kind) = non_registry_kind(r) {
                    return Err(not_registry(kind));
                }
            }
            None => return Err(not_registry("non-registry")),
        }
        if is_true(entry, "hasInstallScript") {
            return Err(pkg_refusal(
                &name,
                shown_version,
                "runs an install script (hasInstallScript); install scripts never run on the node",
            ));
        }
        if !is_plain_semver(version) {
            return Err(pkg_refusal(
                &name,
                shown_version,
                "has a version that is not plain semver; run npm install --package-lock-only",
            ));
        }
        let integrity = match entry.get("integrity").and_then(|i| i.as_str()) {
            None => {
                return Err(pkg_refusal(
                    &name,
                    version,
                    "has no integrity hash (sha512 required)",
                ))
            }
            Some(raw) => parse_integrity(raw).map_err(|why| pkg_refusal(&name, version, why))?,
        };
        out.push(NpmPkg {
            name,
            version: version.to_string(),
            path: key.clone(),
            integrity,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

fn check_package_json(pj: &[u8], root: Option<&Value>, limits: &Limits) -> Result<(), DepsError> {
    let doc = parse_package_json(pj, limits)?;
    let declared = production_dependency_names(&doc);
    if declared.is_empty() {
        return Ok(());
    }
    let locked: Vec<String> = root
        .and_then(|r| r.as_object())
        .map(|r| {
            ["dependencies", "optionalDependencies"]
                .iter()
                .filter_map(|k| r.get(*k).and_then(|d| d.as_object()))
                .flat_map(|d| d.keys().cloned())
                .collect()
        })
        .unwrap_or_default();
    for name in declared {
        if !locked.contains(&name) {
            return Err(refused(format!(
                "package.json and {LOCK} disagree about '{name}'; run npm install --package-lock-only"
            )));
        }
    }
    Ok(())
}

pub(crate) fn parse_package_json(pj: &[u8], limits: &Limits) -> Result<Value, DepsError> {
    if pj.len() as u64 > limits.max_lockfile_bytes {
        return Err(refused(format!(
            "package.json is larger than {} MiB",
            limits.max_lockfile_bytes / (1024 * 1024)
        )));
    }
    serde_json::from_slice(pj).map_err(|e| json_err("package.json", &e))
}

/// Names under `dependencies` and `optionalDependencies`.
pub(crate) fn production_dependency_names(doc: &Value) -> Vec<String> {
    ["dependencies", "optionalDependencies"]
        .iter()
        .filter_map(|k| doc.get(*k).and_then(|d| d.as_object()))
        .flat_map(|d| d.keys().cloned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// sha512 of nothing, base64: any valid length digest works for the grammar tests.
    fn good_integrity() -> String {
        format!(
            "sha512-{}",
            base64::engine::general_purpose::STANDARD.encode([7u8; 64])
        )
    }

    fn lock(version: u32, packages: Value) -> Vec<u8> {
        serde_json::to_vec(&json!({"name": "x", "lockfileVersion": version, "packages": packages}))
            .expect("json")
    }

    fn entry(extra: Value) -> Value {
        let mut base = json!({
            "version": "1.0.0",
            "resolved": "https://registry.npmjs.org/a/-/a-1.0.0.tgz",
            "integrity": good_integrity(),
        });
        if let (Some(b), Some(e)) = (base.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                b.insert(k.clone(), v.clone());
            }
        }
        base
    }

    fn run(packages: Value) -> Result<Vec<NpmPkg>, DepsError> {
        parse(&lock(3, packages), None, &Limits::default())
    }

    fn refusal(packages: Value) -> String {
        match run(packages) {
            Err(DepsError::Refused(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn v3_and_v2_are_accepted() {
        let pk = json!({"": {"dependencies": {"a": "1.0.0"}}, "node_modules/a": entry(json!({}))});
        for v in [2, 3] {
            let r = parse(&lock(v, pk.clone()), None, &Limits::default()).expect("parse");
            assert_eq!(r.len(), 1);
            assert_eq!(r[0].name, "a");
            assert_eq!(r[0].path, "node_modules/a");
            assert_eq!(r[0].integrity.alg, HashAlg::Sha512);
        }
    }

    #[test]
    fn v1_and_missing_version_are_refused() {
        for doc in [
            json!({"lockfileVersion": 1, "dependencies": {}}),
            json!({"packages": {}}),
        ] {
            let m = match parse(
                &serde_json::to_vec(&doc).expect("json"),
                None,
                &Limits::default(),
            ) {
                Err(DepsError::Refused(m)) => m,
                other => panic!("{other:?}"),
            };
            assert!(m.contains("regenerate it with npm 7 or newer"), "{m}");
        }
        assert!(matches!(
            parse(&serde_json::to_vec(&json!({"lockfileVersion": 3})).expect("json"), None, &Limits::default()),
            Err(DepsError::Refused(m)) if m.contains("no packages map")
        ));
        assert!(
            matches!(parse(b"{not json", None, &Limits::default()), Err(DepsError::Refused(m)) if m.contains("not valid JSON"))
        );
    }

    #[test]
    fn dev_and_foreign_optional_are_skipped_peers_kept() {
        let pk = json!({
            "node_modules/prod": entry(json!({"resolved": "https://registry.npmjs.org/prod/-/prod-1.0.0.tgz"})),
            "node_modules/dev": entry(json!({"dev": true, "hasInstallScript": true, "resolved": "git+ssh://x/y.git"})),
            "node_modules/devopt": entry(json!({"devOptional": true, "integrity": "sha1-AAAA"})),
            "node_modules/mac": entry(json!({"optional": true, "os": ["darwin"], "hasInstallScript": true})),
            "node_modules/arm": entry(json!({"optional": true, "cpu": ["arm64"]})),
            "node_modules/notwin": entry(json!({"optional": true, "os": ["!linux"]})),
            "node_modules/linuxopt": entry(json!({"optional": true, "os": ["linux"], "cpu": ["x64", "arm64"], "resolved": "https://registry.npmjs.org/linuxopt/-/linuxopt-1.0.0.tgz"})),
            "node_modules/peer": entry(json!({"peer": true, "resolved": "https://registry.npmjs.org/peer/-/peer-1.0.0.tgz"})),
        });
        let names: Vec<String> = run(pk)
            .expect("parse")
            .into_iter()
            .map(|p| p.name)
            .collect();
        assert_eq!(names, vec!["linuxopt", "peer", "prod"]);
    }

    #[test]
    fn install_script_names_the_package() {
        let m = refusal(json!({"node_modules/a": entry(json!({"hasInstallScript": true}))}));
        assert!(
            m.contains("package 'a' 1.0.0 runs an install script (hasInstallScript); install scripts never run on the node"),
            "{m}"
        );
    }

    #[test]
    fn integrity_missing_weak_or_wrong_length() {
        let mut e = entry(json!({}));
        if let Some(o) = e.as_object_mut() {
            o.remove("integrity");
        }
        let m = refusal(json!({"node_modules/a": e}));
        assert!(
            m.contains("package 'a' 1.0.0 has no integrity hash (sha512 required)"),
            "{m}"
        );
        let m = refusal(
            json!({"node_modules/a": entry(json!({"integrity": "sha1-AAAAAAAAAAAAAAAAAAAAAAAAAAA="}))}),
        );
        assert!(m.contains("integrity is sha1; sha512 required"), "{m}");
        let m = refusal(json!({"node_modules/a": entry(json!({"integrity": "sha512-AAAA"}))}));
        assert!(m.contains("no usable integrity hash"), "{m}");
        let m = refusal(json!({"node_modules/a": entry(json!({"integrity": "md5-AAAA"}))}));
        assert!(m.contains("no usable integrity hash"), "{m}");
    }

    #[test]
    fn strongest_integrity_wins() {
        let s256 = format!(
            "sha256-{}",
            base64::engine::general_purpose::STANDARD.encode([1u8; 32])
        );
        let s384 = format!(
            "sha384-{}",
            base64::engine::general_purpose::STANDARD.encode([2u8; 48])
        );
        let both = format!("{s256} {s384}");
        let r = run(json!({"node_modules/a": entry(json!({"integrity": both}))})).expect("parse");
        assert_eq!(r[0].integrity.alg, HashAlg::Sha384);
        let r = run(json!({"node_modules/a": entry(json!({"integrity": s256}))})).expect("parse");
        assert_eq!(r[0].integrity.alg, HashAlg::Sha256);
    }

    #[test]
    fn non_registry_sources_are_refused() {
        let cases = [
            (json!({"link": true}), "link"),
            (json!({"inBundle": true}), "bundled"),
            (json!({"bundled": true}), "bundled"),
            (
                json!({"resolved": "git+ssh://git@github.com/x/y.git#abc"}),
                "git",
            ),
            (json!({"resolved": "github:x/y"}), "git"),
            (json!({"resolved": "git+https://github.com/x/y.git"}), "git"),
            (json!({"resolved": "file:../local"}), "file"),
            (json!({"resolved": "link:../local"}), "file"),
            (json!({"resolved": "../local"}), "file"),
            (
                json!({"resolved": "ftp://example.test/a/-/a-1.0.0.tgz"}),
                "url",
            ),
            (
                json!({"resolved": "https://codeload.github.com/x/y/tar.gz/abc"}),
                "url",
            ),
            (json!({"resolved": "https://example.test/a.tgz"}), "url"),
            (json!({"name": "other"}), "aliased"),
            (json!({"resolved": "npm:other@1.0.0"}), "aliased"),
        ];
        for (extra, kind) in cases {
            let m = refusal(json!({"node_modules/a": entry(extra.clone())}));
            assert!(
                m.contains(&format!("package 'a' 1.0.0 is a {kind} dependency; only registry packages locked by integrity are accepted")),
                "{extra}: {m}"
            );
        }
        let mut e = entry(json!({}));
        if let Some(o) = e.as_object_mut() {
            o.remove("resolved");
        }
        assert!(
            refusal(json!({"node_modules/a": e})).contains("dependency; only registry packages")
        );
    }

    #[test]
    fn version_must_be_plain_semver() {
        for v in ["^1.0.0", "latest", "1.0", "file:x"] {
            let m = refusal(json!({"node_modules/a": entry(json!({"version": v}))}));
            assert!(m.contains("not plain semver"), "{v}: {m}");
        }
    }

    #[test]
    fn npm_key_traversal_table() {
        for good in [
            "node_modules/a",
            "node_modules/@scope/a",
            "node_modules/a/node_modules/b",
            "node_modules/@s/a/node_modules/@t/b",
        ] {
            assert!(parse_key(good).is_some(), "{good}");
        }
        for bad in [
            "node_modules/../../x",
            "node_modules/a/../b",
            "node_modules/%2e%2e/x",
            "node_modules/a%2fb",
            "/node_modules/a",
            "../node_modules/a",
            "node_modules//a",
            "node_modules/a/",
            "node_modules/@scope",
            "node_modules/@scope/",
            "node_modules/@/a",
            "node_modules/A",
            "node_modules/.bin",
            "node_modules/_x",
            "node_modules/a/node_modules",
            "node_modules/a/b",
            "packages/a",
            "node_modules/a\0b",
            "node_modules\\a",
            "C:/node_modules/a",
            "",
            "node_modules",
            "node_modules/a/node_modules/../x",
        ] {
            assert!(parse_key(bad).is_none(), "{bad:?}");
        }
        let m = refusal(json!({"node_modules/../../x": entry(json!({}))}));
        assert!(
            m.contains("package-lock.json has an invalid package path 'node_modules/../../x'"),
            "{m}"
        );
        let m = refusal(json!({"node_modules/%2e%2e/x": entry(json!({}))}));
        assert!(m.contains("invalid package path"), "{m}");
    }

    #[test]
    fn nested_and_scoped_packages_keep_their_lock_path() {
        let pk = json!({
            "node_modules/a": entry(json!({"resolved": "https://registry.npmjs.org/a/-/a-1.0.0.tgz"})),
            "node_modules/a/node_modules/@s/b": entry(json!({"version": "2.0.0", "resolved": "https://registry.npmjs.org/@s/b/-/b-2.0.0.tgz"})),
        });
        let r = run(pk).expect("parse");
        assert_eq!(r[1].name, "@s/b");
        assert_eq!(r[1].path, "node_modules/a/node_modules/@s/b");
    }

    #[test]
    fn package_json_and_lock_must_agree() {
        let pk = json!({"": {"dependencies": {"a": "1.0.0"}}, "node_modules/a": entry(json!({}))});
        let l = lock(3, pk);
        let ok = br#"{"name":"s","dependencies":{"a":"1.0.0"},"devDependencies":{"zzz":"1"}}"#;
        assert!(parse(&l, Some(ok), &Limits::default()).is_ok());
        let bad = br#"{"name":"s","dependencies":{"a":"1.0.0","b":"1.0.0"}}"#;
        let m = match parse(&l, Some(bad), &Limits::default()) {
            Err(DepsError::Refused(m)) => m,
            other => panic!("{other:?}"),
        };
        assert!(m.contains("package.json and package-lock.json disagree about 'b'; run npm install --package-lock-only"), "{m}");
        let no_root = lock(3, json!({"node_modules/a": entry(json!({}))}));
        assert!(
            matches!(parse(&no_root, Some(ok), &Limits::default()), Err(DepsError::Refused(m)) if m.contains("disagree about 'a'"))
        );
    }

    #[test]
    fn size_cap() {
        let l = lock(3, json!({"node_modules/a": entry(json!({}))}));
        let small = Limits {
            max_lockfile_bytes: 10,
            ..Limits::default()
        };
        assert!(
            matches!(parse(&l, None, &small), Err(DepsError::Refused(m)) if m.contains("larger than"))
        );
        // serde_json's recursion limit stops a deeply nested document.
        let deep = format!("{}{}", "[".repeat(2000), "]".repeat(2000));
        assert!(matches!(
            parse(deep.as_bytes(), None, &Limits::default()),
            Err(DepsError::Refused(_))
        ));
    }
}
