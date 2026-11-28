//! PEP 751 `pylock.toml` (lock-version 1.0): packages locked by version with wheel hashes. Pure.

use super::limits::Limits;
use super::marker::{self, MarkerEnv};
use super::names::{cmp_release, normalize_project, parse_release};
use super::requirements::PyReq;
use super::target::Target;
use super::{refused, DepsError};
use std::cmp::Ordering;

const FILE: &str = "pylock.toml";

fn pkg_err(name: &str, version: &str, text: &str) -> DepsError {
    refused(format!("package '{name}' {version} {text}"))
}

pub(crate) fn parse(
    bytes: &[u8],
    target: &Target,
    env: &MarkerEnv,
    limits: &Limits,
) -> Result<Vec<PyReq>, DepsError> {
    if bytes.len() as u64 > limits.max_lockfile_bytes {
        return Err(refused(format!(
            "{FILE} is larger than {} MiB; drop unused dependencies",
            limits.max_lockfile_bytes / (1024 * 1024)
        )));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        refused(format!(
            "{FILE} is not UTF-8; regenerate it with uv export --format pylock.toml"
        ))
    })?;
    let doc: toml::Table = text.parse().map_err(|e: toml::de::Error| {
        refused(format!(
            "{FILE} is not valid TOML ({}); regenerate it with uv export --format pylock.toml",
            e.message()
        ))
    })?;

    match doc.get("lock-version").and_then(|v| v.as_str()) {
        Some("1.0") => {}
        other => {
            return Err(refused(format!(
                "{FILE} has lock-version {}; only 1.0 is accepted (regenerate with uv export --format pylock.toml)",
                other.unwrap_or("(missing)")
            )))
        }
    }
    if let Some(rp) = doc.get("requires-python") {
        let spec = rp.as_str().ok_or_else(|| {
            refused(format!(
                "{FILE} has a requires-python I cannot read; regenerate it"
            ))
        })?;
        if !specifier_allows(spec, &target.python_full_version()) {
            return Err(refused(format!(
                "{FILE} requires python '{spec}' but the node image has python {}; lock for this python version",
                target.python_full_version()
            )));
        }
    }
    let packages = match doc.get("packages") {
        None => return Ok(Vec::new()),
        Some(v) => v.as_array().ok_or_else(|| {
            refused(format!(
                "{FILE} has a malformed packages list; regenerate it"
            ))
        })?,
    };
    if packages.len() > limits.max_requirements {
        return Err(refused(format!(
            "{FILE} lists more than {} packages; trim the dependency list",
            limits.max_requirements
        )));
    }

    let mut out: Vec<PyReq> = Vec::new();
    for entry in packages {
        let table = entry.as_table().ok_or_else(|| {
            refused(format!(
                "{FILE} has a malformed packages entry; regenerate it"
            ))
        })?;
        let display = table
            .get("name")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                refused(format!(
                    "{FILE} has a package without a name; regenerate it"
                ))
            })?
            .to_string();
        let version = table.get("version").and_then(|v| v.as_str());

        for (key, kind) in [
            ("vcs", "a git"),
            ("directory", "a path"),
            ("archive", "a url"),
        ] {
            if table.contains_key(key) {
                return Err(refused(format!(
                    "{FILE}: '{display}' is {kind} requirement; only registry packages locked by version and hash are accepted"
                )));
            }
        }
        let Some(version) = version else {
            return Err(refused(format!(
                "{FILE}: '{display}' has no version; only registry packages locked by version and hash are accepted"
            )));
        };

        if let Some(m) = table.get("marker").and_then(|v| v.as_str()) {
            match marker::evaluate(m, env) {
                Ok(true) => {}
                Ok(false) => continue,
                Err(e) => {
                    return Err(refused(format!(
                        "{FILE}: '{display}=={version}' has a marker I cannot evaluate ('{}')",
                        e.0
                    )))
                }
            }
        }

        let wheels = table.get("wheels").and_then(|v| v.as_array());
        let Some(wheels) = wheels.filter(|w| !w.is_empty()) else {
            if table.contains_key("sdist") {
                return Err(pkg_err(
                    &display,
                    version,
                    &format!("is a source package (sdist); VAM never builds packages on the node. Lock a wheel for python 3.{} on linux x86_64", target.python_minor),
                ));
            }
            return Err(refused(format!(
                "{FILE}: '{display}=={version}' has no wheels with hashes; every requirement must be pinned and hashed (uv export --format pylock.toml)"
            )));
        };

        let mut hashes = Vec::new();
        for wheel in wheels {
            let hashes_table = wheel.get("hashes").and_then(|h| h.as_table());
            let Some(hashes_table) = hashes_table else {
                return Err(refused(format!(
                    "{FILE}: '{display}=={version}' has a wheel without hashes; every requirement must be pinned and hashed (uv export --format pylock.toml)"
                )));
            };
            match hashes_table.get("sha256").and_then(|h| h.as_str()) {
                Some(hex) if hex.len() == 64 && hex.bytes().all(|c| c.is_ascii_hexdigit()) => {
                    hashes.push(hex.to_ascii_lowercase())
                }
                Some(_) => {
                    return Err(refused(format!(
                        "{FILE}: '{display}=={version}' has a malformed sha256 hash; it must be 64 hex digits"
                    )))
                }
                None => {
                    let alg = hashes_table.keys().next().map(String::as_str).unwrap_or("no");
                    return Err(refused(format!(
                        "{FILE}: '{display}=={version}' uses a {alg} hash; only sha256 is accepted"
                    )));
                }
            }
        }
        hashes.sort();
        hashes.dedup();

        let name = normalize_project(&display);
        match out.iter_mut().find(|r| r.name == name) {
            Some(existing) if existing.version != version => {
                return Err(refused(format!(
                    "{FILE}: '{display}' is pinned twice ({} and {version}); keep one version",
                    existing.version
                )));
            }
            Some(existing) => {
                existing.hashes.extend(hashes);
                existing.hashes.sort();
                existing.hashes.dedup();
            }
            None => out.push(PyReq {
                name,
                display,
                version: version.to_string(),
                hashes,
            }),
        }
    }
    Ok(out)
}

/// A comma-separated PEP 440 specifier set against a release (`>=3.9,<4`). Anything this cannot
/// read does not match, so the lockfile is refused rather than guessed at.
fn specifier_allows(spec: &str, version: &str) -> bool {
    let Some(v) = parse_release(version) else {
        return false;
    };
    let clauses: Vec<&str> = spec
        .split(',')
        .map(str::trim)
        .filter(|c| !c.is_empty())
        .collect();
    if clauses.is_empty() {
        return false;
    }
    clauses.iter().all(|clause| {
        let (op, rest) = split_op(clause);
        let rest = rest.trim();
        if let Some(prefix) = rest.strip_suffix(".*") {
            let Some(p) = parse_release(prefix) else {
                return false;
            };
            let hit = v.len() >= p.len() && v[..p.len()] == p[..];
            return match op {
                "==" => hit,
                "!=" => !hit,
                _ => false,
            };
        }
        let Some(r) = parse_release(rest) else {
            return false;
        };
        let ord = cmp_release(&v, &r);
        match op {
            "==" => ord == Ordering::Equal,
            "!=" => ord != Ordering::Equal,
            "<" => ord == Ordering::Less,
            "<=" => ord != Ordering::Greater,
            ">" => ord == Ordering::Greater,
            ">=" => ord != Ordering::Less,
            "~=" => {
                r.len() >= 2 && ord != Ordering::Less && {
                    let upper = &r[..r.len() - 1];
                    v.len() >= upper.len() && v[..upper.len()] == upper[..]
                }
            }
            _ => false,
        }
    })
}

fn split_op(clause: &str) -> (&str, &str) {
    for op in ["===", "~=", "==", "!=", "<=", ">=", "<", ">"] {
        if let Some(rest) = clause.strip_prefix(op) {
            return (op, rest);
        }
    }
    ("", clause)
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "ba5a8f0ad76889d0864d568cc3896dd5555f4d97d45017e1e334a2383e16c74a";
    const H2: &str = "1bdaa7f5307d93a041459d1e8520af1c52ea3c21874ff5d3c4bf1a20005eab38";

    fn run(text: &str) -> Result<Vec<PyReq>, DepsError> {
        let t = Target::new(13, 41);
        parse(text.as_bytes(), &t, &MarkerEnv::new(&t), &Limits::default())
    }

    fn refusal(text: &str) -> String {
        match run(text) {
            Err(DepsError::Refused(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    fn head() -> String {
        "lock-version = \"1.0\"\nrequires-python = \">=3.9\"\ncreated-by = \"uv\"\n".to_string()
    }

    #[test]
    fn valid_lock_resolves_names_versions_and_hashes() {
        let text = format!(
            "{}\n[[packages]]\nname = \"Vamfixture_Hello\"\nversion = \"1.0.0\"\n[[packages.wheels]]\nname = \"x.whl\"\nurl = \"https://example.test/never-fetched.whl\"\n[packages.wheels.hashes]\nsha256 = \"{H1}\"\n[[packages.wheels]]\nname = \"y.whl\"\n[packages.wheels.hashes]\nsha256 = \"{H2}\"\n[packages.sdist]\nname = \"s.tar.gz\"\n[packages.sdist.hashes]\nsha256 = \"{H2}\"\n",
            head()
        );
        let r = run(&text).expect("parse");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].name, "vamfixture-hello");
        assert_eq!(r[0].hashes.len(), 2);
    }

    #[test]
    fn sdist_only_package_is_a_source_refusal() {
        let text = format!(
            "{}\n[[packages]]\nname = \"vamfixture-sdist-only\"\nversion = \"1.0.0\"\n[packages.sdist]\nname = \"a.tar.gz\"\n[packages.sdist.hashes]\nsha256 = \"{H2}\"\n",
            head()
        );
        let m = refusal(&text);
        assert!(
            m.contains("vamfixture-sdist-only") && m.contains("source package (sdist)"),
            "{m}"
        );
    }

    #[test]
    fn vcs_directory_archive_and_missing_version_are_refused() {
        for (table, kind) in [
            ("vcs", "a git"),
            ("directory", "a path"),
            ("archive", "a url"),
        ] {
            let text = format!("{}\n[[packages]]\nname = \"a\"\nversion = \"1\"\n[packages.{table}]\npath = \"x\"\n", head());
            let m = refusal(&text);
            assert!(
                m.contains(&format!("is {kind} requirement")),
                "{table}: {m}"
            );
        }
        let m = refusal(&format!("{}\n[[packages]]\nname = \"a\"\n", head()));
        assert!(m.contains("'a' has no version"), "{m}");
    }

    #[test]
    fn wrong_lock_version_or_unmet_requires_python() {
        let m = refusal("lock-version = \"2.0\"\n");
        assert!(m.contains("lock-version 2.0"), "{m}");
        let m = refusal("created-by = \"x\"\n");
        assert!(m.contains("(missing)"), "{m}");
        let m = refusal("lock-version = \"1.0\"\nrequires-python = \">=3.14\"\n");
        assert!(m.contains("requires python '>=3.14'"), "{m}");
        let m = refusal("lock-version = \"1.0\"\nrequires-python = \"<3.13\"\n");
        assert!(m.contains("requires python"), "{m}");
        assert!(run("lock-version = \"1.0\"\nrequires-python = \">=3.9,<4,!=3.10.*\"\n").is_ok());
        assert!(run("lock-version = \"1.0\"\nrequires-python = \"~=3.9\"\n").is_ok());
        assert!(run("lock-version = \"1.0\"\nrequires-python = \"==3.13.*\"\n").is_ok());
        let m = refusal("lock-version = \"1.0\"\nrequires-python = \"banana\"\n");
        assert!(m.contains("requires python"), "{m}");
    }

    #[test]
    fn weak_hash_missing_hash_and_bad_toml() {
        let text = format!("{}\n[[packages]]\nname = \"a\"\nversion = \"1\"\n[[packages.wheels]]\nname = \"a.whl\"\n[packages.wheels.hashes]\nsha512 = \"ab\"\n", head());
        assert!(refusal(&text).contains("uses a sha512 hash; only sha256 is accepted"));
        let text = format!("{}\n[[packages]]\nname = \"a\"\nversion = \"1\"\n[[packages.wheels]]\nname = \"a.whl\"\n", head());
        assert!(refusal(&text).contains("wheel without hashes"));
        let text = format!("{}\n[[packages]]\nname = \"a\"\nversion = \"1\"\n", head());
        assert!(refusal(&text).contains("no wheels with hashes"));
        assert!(refusal("this is [not toml").contains("not valid TOML"));
    }

    #[test]
    fn marker_per_package_is_evaluated() {
        let pkg = |name: &str, marker: &str| {
            format!("\n[[packages]]\nname = \"{name}\"\nversion = \"1\"\nmarker = \"{marker}\"\n[[packages.wheels]]\nname = \"a.whl\"\n[packages.wheels.hashes]\nsha256 = \"{H1}\"\n")
        };
        let text = format!(
            "{}{}{}",
            head(),
            pkg("keep", "sys_platform == 'linux'"),
            pkg("drop", "sys_platform == 'win32'")
        );
        let r = run(&text).expect("parse");
        assert_eq!(
            r.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["keep"]
        );
        let m = refusal(&format!(
            "{}{}",
            head(),
            pkg("odd", "platform_release == '1'")
        ));
        assert!(
            m.contains("marker I cannot evaluate ('platform_release')"),
            "{m}"
        );
    }

    #[test]
    fn duplicate_names_with_different_versions_are_refused() {
        let pkg = |v: &str| {
            format!("\n[[packages]]\nname = \"a\"\nversion = \"{v}\"\n[[packages.wheels]]\nname = \"a.whl\"\n[packages.wheels.hashes]\nsha256 = \"{H1}\"\n")
        };
        assert!(refusal(&format!("{}{}{}", head(), pkg("1"), pkg("2"))).contains("pinned twice"));
    }
}
