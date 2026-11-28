//! `VAMD_PACKAGE_MIRRORS`: where the operator's packages come from. The lockfile never names
//! a source. Only `https` and `file://` mirrors exist; there is no flag that relaxes that.

use super::{DepsError, Ecosystem};
use std::path::PathBuf;
use url::Url;

const DEFAULT_PYPI: &str = "https://pypi.org/simple/";
const DEFAULT_NPM: &str = "https://registry.npmjs.org/";

/// One ecosystem's mirror.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MirrorSpec {
    /// An https registry. `hosts` is the allowlist for the index and every file and redirect.
    Https { base: Url, hosts: Vec<String> },
    /// A local directory tree (air-gapped nodes and tests); `root` is canonical.
    File { root: PathBuf },
}

/// The parsed mirror list, one entry per ecosystem.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mirrors {
    pub(crate) pypi: MirrorSpec,
    pub(crate) npm: MirrorSpec,
}

fn bad(reason: impl AsRef<str>) -> DepsError {
    DepsError::Refused(format!("VAMD_PACKAGE_MIRRORS: {}", reason.as_ref()))
}

fn default_https(url: &str, hosts: &[&str]) -> Result<MirrorSpec, DepsError> {
    let base =
        Url::parse(url).map_err(|e| bad(format!("built-in default '{url}' is invalid ({e})")))?;
    Ok(MirrorSpec::Https {
        base,
        hosts: hosts.iter().map(|h| h.to_string()).collect(),
    })
}

impl Mirrors {
    /// The built-in public mirrors: PyPI (files from `files.pythonhosted.org`) and the npm registry.
    pub(crate) fn public_defaults() -> Result<Mirrors, DepsError> {
        Ok(Mirrors {
            pypi: default_https(DEFAULT_PYPI, &["pypi.org", "files.pythonhosted.org"])?,
            npm: default_https(DEFAULT_NPM, &["registry.npmjs.org"])?,
        })
    }

    /// Comma-separated `pypi=<url>` and/or `npm=<url>`. `None` or empty gives the public defaults.
    /// A typo is an error and never a silent fall back to the public mirrors.
    pub fn parse(value: Option<&str>) -> Result<Mirrors, DepsError> {
        let mut out = Mirrors::public_defaults()?;
        let Some(value) = value.filter(|v| !v.trim().is_empty()) else {
            return Ok(out);
        };
        let (mut seen_pypi, mut seen_npm) = (false, false);
        for segment in value.split(',') {
            let segment = segment.trim();
            let Some((key, url)) = segment.split_once('=') else {
                return Err(bad(format!(
                    "'{segment}' is not pypi=<url> or npm=<url> (write a comma inside a path as %2C)"
                )));
            };
            let seen = match key {
                "pypi" => &mut seen_pypi,
                "npm" => &mut seen_npm,
                other => {
                    return Err(bad(format!(
                        "unknown mirror '{other}'; expected pypi or npm"
                    )))
                }
            };
            if *seen {
                return Err(bad(format!("'{key}' is given twice")));
            }
            *seen = true;
            let spec = parse_one(key, url.trim())?;
            if key == "pypi" {
                out.pypi = spec;
            } else {
                out.npm = spec;
            }
        }
        Ok(out)
    }

    pub(crate) fn for_ecosystem(&self, eco: Ecosystem) -> &MirrorSpec {
        match eco {
            Ecosystem::Python => &self.pypi,
            Ecosystem::Js => &self.npm,
        }
    }
}

fn parse_one(key: &str, raw: &str) -> Result<MirrorSpec, DepsError> {
    let url = Url::parse(raw).map_err(|e| bad(format!("{key}: '{raw}' is not a URL ({e})")))?;
    if url.query().is_some() || url.fragment().is_some() {
        return Err(bad(format!(
            "{key}: '{raw}' must not have a query or fragment"
        )));
    }
    match url.scheme() {
        "https" => {
            if !url.username().is_empty() || url.password().is_some() {
                return Err(bad(format!("{key}: '{raw}' must not carry credentials")));
            }
            let Some(host) = url.host_str().filter(|h| !h.is_empty()) else {
                return Err(bad(format!("{key}: '{raw}' has no host")));
            };
            let host = host.to_ascii_lowercase();
            let mut base = url;
            if !base.path().ends_with('/') {
                let with_slash = format!("{}/", base.path());
                base.set_path(&with_slash);
            }
            Ok(MirrorSpec::Https {
                base,
                hosts: vec![host],
            })
        }
        "http" => Err(bad(format!(
            "{key}: '{raw}' is http://; only https:// and file:// mirrors are accepted"
        ))),
        "file" => {
            if url.host_str().is_some_and(|h| !h.is_empty()) {
                return Err(bad(format!(
                    "{key}: '{raw}' is a file:// URL with a host; use file:///absolute/path"
                )));
            }
            if url.path() == "/" {
                return Err(bad(format!(
                    "{key}: '{raw}' is the filesystem root; name a mirror directory"
                )));
            }
            let path = url
                .to_file_path()
                .map_err(|_| bad(format!("{key}: '{raw}' is not an absolute file:// path")))?;
            let root = std::fs::canonicalize(&path)
                .map_err(|e| bad(format!("{key}: {} is not readable ({e})", path.display())))?;
            if !root.is_dir() {
                return Err(bad(format!("{key}: {} is not a directory", root.display())));
            }
            if root == std::path::Path::new("/") {
                return Err(bad(format!(
                    "{key}: '{raw}' resolves to the filesystem root"
                )));
            }
            Ok(MirrorSpec::File { root })
        }
        other => Err(bad(format!(
            "{key}: '{raw}' uses scheme {other}; only https:// and file:// mirrors are accepted"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reason(v: &str) -> String {
        match Mirrors::parse(Some(v)) {
            Err(DepsError::Refused(m)) => m,
            other => panic!("{v}: expected a refusal, got {other:?}"),
        }
    }

    /// As the W0 runner builds it: `from_directory_path` percent-encodes most characters, but not a
    /// comma, which is the list separator, so the comma is written `%2C` by hand.
    fn file_url(p: &std::path::Path) -> String {
        Url::from_directory_path(p)
            .expect("abs")
            .to_string()
            .replace(',', "%2C")
    }

    #[test]
    fn defaults_when_unset_or_empty() {
        for v in [None, Some(""), Some("   ")] {
            let m = Mirrors::parse(v).expect("defaults");
            assert_eq!(
                m.pypi,
                MirrorSpec::Https {
                    base: Url::parse("https://pypi.org/simple/").expect("url"),
                    hosts: vec!["pypi.org".into(), "files.pythonhosted.org".into()],
                }
            );
            assert_eq!(
                m.npm,
                MirrorSpec::Https {
                    base: Url::parse("https://registry.npmjs.org/").expect("url"),
                    hosts: vec!["registry.npmjs.org".into()],
                }
            );
        }
    }

    #[test]
    fn valid_file_pair_matches_the_w0_runner_format() {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("pypi/simple")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("npm")).expect("mkdir");
        let root = std::fs::canonicalize(dir.path()).expect("canon");
        let v = format!(
            "pypi=file://{p}/pypi/simple,npm=file://{p}/npm",
            p = root.display()
        );
        let m = Mirrors::parse(Some(&v)).expect("parse");
        assert_eq!(
            m.pypi,
            MirrorSpec::File {
                root: root.join("pypi/simple")
            }
        );
        assert_eq!(
            m.npm,
            MirrorSpec::File {
                root: root.join("npm")
            }
        );
    }

    #[test]
    fn one_entry_leaves_the_other_default() {
        let m = Mirrors::parse(Some("npm=https://npm.corp.example/registry")).expect("parse");
        assert_eq!(
            m.npm,
            MirrorSpec::Https {
                base: Url::parse("https://npm.corp.example/registry/").expect("url"),
                hosts: vec!["npm.corp.example".into()],
            },
            "custom https mirror: own host only, trailing slash added"
        );
        assert_eq!(m.pypi, Mirrors::public_defaults().expect("defaults").pypi);
    }

    #[test]
    fn custom_pypi_mirror_does_not_inherit_pythonhosted() {
        let m = Mirrors::parse(Some("pypi=https://pypi.corp.example/simple")).expect("parse");
        match m.pypi {
            MirrorSpec::Https { hosts, .. } => {
                assert_eq!(hosts, vec!["pypi.corp.example".to_string()])
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn mirrors_parse_table() {
        let dir = tempfile::tempdir().expect("tempdir");
        let ok_dir = file_url(dir.path());
        assert!(reason("pypi=http://pypi.example/simple")
            .contains("is http://; only https:// and file:// mirrors"));
        assert!(reason("pypi=ftp://x/y").contains("scheme ftp"));
        assert!(reason("pypi=https://a/x,pypi=https://b/x").contains("'pypi' is given twice"));
        assert!(reason("pip=https://a/x").contains("unknown mirror 'pip'"));
        assert!(reason("https://a/x").contains("is not pypi=<url> or npm=<url>"));
        assert!(
            reason("pypi=https://a/x,").contains("is not pypi=<url> or npm=<url>"),
            "trailing comma"
        );
        assert!(reason("pypi=file://somehost/tmp").contains("with a host"));
        assert!(reason("pypi=file:///").contains("filesystem root"));
        assert!(reason("pypi=file:///definitely/not/here/vam").contains("not readable"));
        assert!(reason("pypi=https://a/x?token=1").contains("query or fragment"));
        assert!(reason("pypi=https://a/x#frag").contains("query or fragment"));
        assert!(reason("pypi=https://user:pw@a/x").contains("credentials"));
        assert!(reason("pypi=relative/path").contains("is not a URL"));
        assert!(reason(&format!("pypi={ok_dir}?q=1")).contains("query or fragment"));
        let file = dir.path().join("afile");
        std::fs::write(&file, b"x").expect("write");
        assert!(reason(&format!("pypi=file://{}", file.display())).contains("not a directory"));
        assert!(reason("npm=").contains("is not a URL"));
        for r in [reason("pypi=http://x/y"), reason("nope")] {
            assert!(r.starts_with("VAMD_PACKAGE_MIRRORS: "), "{r}");
        }
    }

    #[test]
    fn commas_inside_a_path_are_written_percent_2c() {
        let dir = tempfile::tempdir().expect("tempdir");
        let weird = dir.path().join("a,b");
        std::fs::create_dir(&weird).expect("mkdir");
        let encoded = file_url(&weird);
        assert!(encoded.contains("%2C"), "{encoded}");
        let m = Mirrors::parse(Some(&format!("pypi={encoded}"))).expect("parse");
        assert_eq!(
            m.pypi,
            MirrorSpec::File {
                root: std::fs::canonicalize(&weird).expect("canon")
            }
        );
        // The raw comma splits the value, so it is refused rather than misread.
        let raw = format!("pypi=file://{}", weird.display());
        assert!(Mirrors::parse(Some(&raw)).is_err());
    }

    #[test]
    fn paths_with_percent_hash_and_question_mark_survive_when_encoded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let weird = dir.path().join("100%#?x y");
        std::fs::create_dir(&weird).expect("mkdir");
        let m = Mirrors::parse(Some(&format!("npm={}", file_url(&weird)))).expect("parse");
        assert_eq!(
            m.npm,
            MirrorSpec::File {
                root: std::fs::canonicalize(&weird).expect("canon")
            }
        );
    }
}
