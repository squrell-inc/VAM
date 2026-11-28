//! Finding a package in a mirror: the PEP 691 JSON index (hash lookup, wheel choice), the npm
//! tarball path, and the contained mirror locators. The lockfile chooses what; the operator's
//! mirror says where. Nothing from the lockfile is used as an address.

use super::mirrors::MirrorSpec;
use super::names::{normalize_project, npm_base_name, versions_equal};
use super::requirements::PyReq;
use super::target::Target;
use super::transport::Locator;
use super::wheel_tags::{choose, WheelName};
use super::{refused, DepsError};
use percent_encoding::percent_decode_str;
use serde_json::Value;
use std::path::PathBuf;

/// One file of a project as the index lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct IndexFile {
    pub filename: String,
    pub url: String,
    pub sha256: Option<String>,
    pub yanked: bool,
}

pub(crate) fn parse_index(body: &[u8], project: &str) -> Result<Vec<IndexFile>, DepsError> {
    let first = body.iter().find(|b| !b.is_ascii_whitespace());
    if first != Some(&b'{') {
        return Err(refused(
            "the pypi mirror does not serve the PEP 691 JSON index",
        ));
    }
    let doc: Value = serde_json::from_slice(body).map_err(|_| {
        refused(format!(
            "the pypi mirror's index for '{project}' is not valid JSON"
        ))
    })?;
    let named = doc.get("name").and_then(|n| n.as_str()).unwrap_or("");
    if normalize_project(named) != project {
        return Err(refused(format!(
            "the pypi mirror's index for '{project}' is about '{named}'; the mirror and the lockfile disagree"
        )));
    }
    let files = doc.get("files").and_then(|f| f.as_array()).ok_or_else(|| {
        refused(format!(
            "the pypi mirror's index for '{project}' has no file list"
        ))
    })?;
    let mut out = Vec::new();
    for f in files {
        let (Some(filename), Some(url)) = (
            f.get("filename").and_then(|v| v.as_str()),
            f.get("url").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let sha256 = f
            .get("hashes")
            .and_then(|h| h.get("sha256"))
            .and_then(|h| h.as_str())
            .map(str::to_ascii_lowercase);
        // PEP 691: false, true, or a reason string (any string means yanked).
        let yanked = match f.get("yanked") {
            Some(Value::Bool(b)) => *b,
            Some(Value::String(_)) => true,
            _ => false,
        };
        out.push(IndexFile {
            filename: filename.to_string(),
            url: url.to_string(),
            sha256,
            yanked,
        });
    }
    Ok(out)
}

/// The wheel the lock's hashes pick for this target. The hash is the only selector: the
/// filename and version never choose a file.
pub(crate) fn select_wheel(
    req: &PyReq,
    files: &[IndexFile],
    target: &Target,
) -> Result<IndexFile, DepsError> {
    let label = format!("package '{}' {}", req.name, req.version);
    let matched: Vec<&IndexFile> = files
        .iter()
        .filter(|f| f.sha256.as_ref().is_some_and(|h| req.hashes.contains(h)))
        .collect();
    if matched.is_empty() {
        let first = req
            .hashes
            .first()
            .map(|h| &h[..12.min(h.len())])
            .unwrap_or("");
        return Err(refused(format!(
            "{label} is not in the package mirror (sha256 {first}...)"
        )));
    }
    let mut wheels: Vec<(String, WheelName)> = Vec::new();
    let mut had_sdist = false;
    for f in &matched {
        if f.filename.ends_with(".whl") {
            let parsed = WheelName::parse(&f.filename);
            let ok = parsed.as_ref().is_some_and(|w| {
                normalize_project(&w.name) == req.name && versions_equal(&w.version, &req.version)
            });
            match parsed {
                Some(w) if ok => wheels.push((f.filename.clone(), w)),
                _ => {
                    return Err(refused(format!(
                        "{label}: the package mirror lists '{}' under this hash, which is not this release; the mirror and the lockfile disagree",
                        f.filename
                    )))
                }
            }
        } else {
            had_sdist = true;
        }
    }
    if wheels.is_empty() {
        debug_assert!(had_sdist);
        return Err(refused(format!(
            "{label} is a source package (sdist); VAM never builds packages on the node. Lock a wheel for python 3.{} on linux x86_64",
            target.python_minor
        )));
    }
    let Some(best) = choose(&wheels, target) else {
        let found: Vec<String> = wheels.iter().map(|(_, w)| w.tags()).collect();
        return Err(refused(format!(
            "{label} has no wheel for python 3.{}, linux x86_64, glibc 2.{} among its locked hashes (found: {})",
            target.python_minor,
            target.glibc_minor,
            found.join(", ")
        )));
    };
    let chosen = matched
        .iter()
        .find(|f| f.filename == best.0)
        .map(|f| (*f).clone())
        .ok_or_else(|| refused(format!("{label}: internal wheel lookup failed")))?;
    if chosen.yanked {
        return Err(refused(format!("{label} was yanked on the index")));
    }
    Ok(chosen)
}

// ---------------------------------------------------------------------------------------------
// Locators
// ---------------------------------------------------------------------------------------------

/// `<index>/<normalised-name>/` or `<root>/<name>/index.json`.
pub(crate) fn python_index_locator(spec: &MirrorSpec, project: &str) -> Result<Locator, DepsError> {
    match spec {
        MirrorSpec::Https { base, .. } => {
            let mut u = base.clone();
            u.path_segments_mut()
                .map_err(|_| refused("the pypi mirror URL cannot be a base"))?
                .pop_if_empty()
                .push(project)
                .push("");
            Ok(Locator::Http(u))
        }
        MirrorSpec::File { root } => Ok(Locator::File(root.join(project).join("index.json"))),
    }
}

/// Where the chosen file lives. An https mirror follows an absolute or relative `url` from the
/// index (the allowlist is enforced by the transport on every hop); a file mirror takes only a
/// relative `url` with no `..` and no scheme, resolved under the project directory. In both, the
/// last path segment, percent-decoded, must equal the filename.
pub(crate) fn python_file_locator(
    spec: &MirrorSpec,
    index: &Locator,
    file: &IndexFile,
) -> Result<Locator, DepsError> {
    let bad = |why: &str| {
        refused(format!(
            "the package mirror's url for '{}' {why}",
            file.filename
        ))
    };
    let raw = file.url.split('#').next().unwrap_or("");
    match (spec, index) {
        (MirrorSpec::Https { .. }, Locator::Http(index_url)) => {
            let mut u = index_url.join(raw).map_err(|_| bad("is not a valid URL"))?;
            u.set_fragment(None);
            let last = u
                .path_segments()
                .and_then(|mut s| s.next_back())
                .map(|s| percent_decode_str(s).decode_utf8_lossy().into_owned())
                .unwrap_or_default();
            if last != file.filename {
                return Err(bad("does not end in the file name"));
            }
            Ok(Locator::Http(u))
        }
        (MirrorSpec::File { .. }, Locator::File(index_path)) => {
            if raw.is_empty()
                || raw.contains('\\')
                || raw.contains('\0')
                || raw.contains('?')
                || raw.starts_with('/')
            {
                return Err(bad("must be a relative path inside the mirror"));
            }
            let decoded = percent_decode_str(raw)
                .decode_utf8()
                .map_err(|_| bad("is not valid UTF-8"))?;
            if decoded.contains('\0') || decoded.contains('\\') {
                return Err(bad("must be a relative path inside the mirror"));
            }
            // A scheme (`https://`, `file:`) is a colon before the first slash.
            let first_seg = decoded.split('/').next().unwrap_or("");
            if first_seg.contains(':') {
                return Err(bad("must be a relative path inside the mirror"));
            }
            let mut path: PathBuf = index_path
                .parent()
                .map(PathBuf::from)
                .ok_or_else(|| bad("has no project directory"))?;
            let mut last = "";
            for comp in decoded.split('/') {
                if comp.is_empty() || comp == "." || comp == ".." {
                    return Err(bad("must be a relative path inside the mirror"));
                }
                path.push(comp);
                last = comp;
            }
            if last != file.filename {
                return Err(bad("does not end in the file name"));
            }
            Ok(Locator::File(path))
        }
        _ => Err(refused("internal: mirror and locator kinds differ")),
    }
}

/// `<registry>/<name>/-/<last segment of name>-<version>.tgz`, built from the lock entry's name
/// and version, never from `resolved`. Name and version were validated when the lock was read.
pub(crate) fn npm_tarball_locator(
    spec: &MirrorSpec,
    name: &str,
    version: &str,
) -> Result<Locator, DepsError> {
    let file = format!("{}-{version}.tgz", npm_base_name(name));
    match spec {
        MirrorSpec::Https { base, .. } => {
            let mut u = base.clone();
            {
                let mut segs = u
                    .path_segments_mut()
                    .map_err(|_| refused("the npm mirror URL cannot be a base"))?;
                segs.pop_if_empty();
                for part in name.split('/') {
                    segs.push(part);
                }
                segs.push("-").push(&file);
            }
            Ok(Locator::Http(u))
        }
        MirrorSpec::File { root } => {
            let mut p = root.clone();
            for part in name.split('/') {
                p.push(part);
            }
            p.push("-");
            p.push(&file);
            Ok(Locator::File(p))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use url::Url;

    const H_WHEEL: &str = "ba5a8f0ad76889d0864d568cc3896dd5555f4d97d45017e1e334a2383e16c74a";
    const H_SDIST: &str = "1bdaa7f5307d93a041459d1e8520af1c52ea3c21874ff5d3c4bf1a20005eab38";
    const H_OTHER: &str = "2222222222222222222222222222222222222222222222222222222222222222";

    fn t() -> Target {
        Target::new(13, 41)
    }

    fn req(name: &str, version: &str, hashes: &[&str]) -> PyReq {
        PyReq {
            name: normalize_project(name),
            display: name.to_string(),
            version: version.to_string(),
            hashes: hashes.iter().map(|h| h.to_string()).collect(),
        }
    }

    fn f(filename: &str, sha: Option<&str>, yanked: bool) -> IndexFile {
        IndexFile {
            filename: filename.to_string(),
            url: filename.to_string(),
            sha256: sha.map(str::to_string),
            yanked,
        }
    }

    fn refusal(r: Result<IndexFile, DepsError>) -> String {
        match r {
            Err(DepsError::Refused(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn index_must_be_pep691_json_for_the_project() {
        let m = match parse_index(b"<!DOCTYPE html><html></html>", "a") {
            Err(DepsError::Refused(m)) => m,
            other => panic!("{other:?}"),
        };
        assert!(
            m.contains("the pypi mirror does not serve the PEP 691 JSON index"),
            "{m}"
        );
        assert!(
            matches!(parse_index(b"{broken", "a"), Err(DepsError::Refused(m)) if m.contains("not valid JSON"))
        );
        assert!(
            matches!(parse_index(br#"{"name":"b","files":[]}"#, "a"), Err(DepsError::Refused(m)) if m.contains("about 'b'"))
        );
        assert!(
            matches!(parse_index(br#"{"name":"a"}"#, "a"), Err(DepsError::Refused(m)) if m.contains("no file list"))
        );
        let ok = parse_index(br#"{"name":"A_b","files":[{"filename":"x.whl","url":"x.whl","hashes":{"sha256":"AB"},"yanked":"bad release"},{"filename":"y"},{"filename":"z.whl","url":"z","yanked":false}]}"#, "a-b").expect("parse");
        assert_eq!(ok.len(), 2, "an entry without a url is ignored");
        assert!(ok[0].yanked, "a reason string means yanked");
        assert_eq!(
            ok[0].sha256.as_deref(),
            Some("ab"),
            "hashes compare lowercase"
        );
        assert!(!ok[1].yanked);
    }

    #[test]
    fn hash_is_the_only_selector() {
        let files = vec![
            f(
                "vamfixture_hello-1.0.0-py3-none-any.whl",
                Some(H_WHEEL),
                false,
            ),
            f(
                "vamfixture_hello-9.9.9-py3-none-any.whl",
                Some(H_OTHER),
                false,
            ),
        ];
        let r = req("vamfixture-hello", "1.0.0", &[H_WHEEL]);
        assert_eq!(
            select_wheel(&r, &files, &t()).expect("select").filename,
            "vamfixture_hello-1.0.0-py3-none-any.whl"
        );
        // The right filename under the wrong hash is not selected.
        let r = req("vamfixture-hello", "9.9.9", &[H_SDIST]);
        let m = refusal(select_wheel(&r, &files, &t()));
        assert!(m.contains("package 'vamfixture-hello' 9.9.9 is not in the package mirror (sha256 1bdaa7f5307d..."), "{m}");
    }

    #[test]
    fn sdist_only_is_the_s3_refusal() {
        let files = vec![f(
            "vamfixture-sdist-only-1.0.0.tar.gz",
            Some(H_SDIST),
            false,
        )];
        let r = req("vamfixture-sdist-only", "1.0.0", &[H_SDIST]);
        let m = refusal(select_wheel(&r, &files, &t()));
        assert!(
            m.contains("package 'vamfixture-sdist-only' 1.0.0 is a source package (sdist); VAM never builds packages on the node. Lock a wheel for python 3.13 on linux x86_64"),
            "{m}"
        );
    }

    #[test]
    fn wheel_for_another_release_is_a_mismatch() {
        let files = vec![f("other-1.0.0-py3-none-any.whl", Some(H_WHEEL), false)];
        let m = refusal(select_wheel(
            &req("vamfixture-hello", "1.0.0", &[H_WHEEL]),
            &files,
            &t(),
        ));
        assert!(m.contains("not this release"), "{m}");
        let files = vec![f(
            "vamfixture_hello-2.0.0-py3-none-any.whl",
            Some(H_WHEEL),
            false,
        )];
        assert!(refusal(select_wheel(
            &req("vamfixture-hello", "1.0.0", &[H_WHEEL]),
            &files,
            &t()
        ))
        .contains("not this release"));
        let files = vec![f("garbage.whl", Some(H_WHEEL), false)];
        assert!(refusal(select_wheel(
            &req("vamfixture-hello", "1.0.0", &[H_WHEEL]),
            &files,
            &t()
        ))
        .contains("not this release"));
    }

    #[test]
    fn incompatible_wheels_list_what_was_found() {
        let files = vec![
            f(
                "p-1.0-cp313-cp313-manylinux_2_42_x86_64.whl",
                Some(H_WHEEL),
                false,
            ),
            f(
                "p-1.0-cp313-cp313-musllinux_1_2_x86_64.whl",
                Some(H_OTHER),
                false,
            ),
        ];
        let m = refusal(select_wheel(
            &req("p", "1.0", &[H_WHEEL, H_OTHER]),
            &files,
            &t(),
        ));
        assert!(m.contains("package 'p' 1.0 has no wheel for python 3.13, linux x86_64, glibc 2.41 among its locked hashes (found: "), "{m}");
        assert!(
            m.contains("manylinux_2_42_x86_64") && m.contains("musllinux_1_2_x86_64"),
            "{m}"
        );
    }

    #[test]
    fn best_compatible_wheel_wins_and_yanked_is_refused() {
        let files = vec![
            f("p-1.0-py3-none-any.whl", Some(H_WHEEL), false),
            f(
                "p-1.0-cp313-cp313-manylinux_2_28_x86_64.whl",
                Some(H_OTHER),
                false,
            ),
        ];
        let r = req("p", "1.0", &[H_WHEEL, H_OTHER]);
        assert_eq!(
            select_wheel(&r, &files, &t()).expect("select").filename,
            "p-1.0-cp313-cp313-manylinux_2_28_x86_64.whl"
        );
        let files = vec![f("p-1.0-py3-none-any.whl", Some(H_WHEEL), true)];
        let m = refusal(select_wheel(&req("p", "1.0", &[H_WHEEL]), &files, &t()));
        assert!(m.contains("package 'p' 1.0 was yanked on the index"), "{m}");
    }

    #[test]
    fn sdist_beside_a_wheel_is_ignored_for_selection() {
        let files = vec![
            f("p-1.0.tar.gz", Some(H_SDIST), false),
            f("p-1.0-py3-none-any.whl", Some(H_WHEEL), false),
        ];
        let r = req("p", "1.0", &[H_SDIST, H_WHEEL]);
        assert_eq!(
            select_wheel(&r, &files, &t()).expect("select").filename,
            "p-1.0-py3-none-any.whl"
        );
    }

    // ---- locators ----

    fn https() -> MirrorSpec {
        MirrorSpec::Https {
            base: Url::parse("https://pypi.example/simple/").expect("url"),
            hosts: vec!["pypi.example".into(), "files.example".into()],
        }
    }

    #[test]
    fn https_locators_are_built_from_validated_parts() {
        let idx = python_index_locator(&https(), "vamfixture-hello").expect("loc");
        assert_eq!(
            idx,
            Locator::Http(
                Url::parse("https://pypi.example/simple/vamfixture-hello/").expect("url")
            )
        );
        let file = IndexFile {
            filename: "a-1-py3-none-any.whl".into(),
            url: "../../packages/ab/a-1-py3-none-any.whl#sha256=x".into(),
            sha256: None,
            yanked: false,
        };
        let loc = python_file_locator(&https(), &idx, &file).expect("loc");
        assert_eq!(
            loc,
            Locator::Http(
                Url::parse("https://pypi.example/packages/ab/a-1-py3-none-any.whl").expect("url")
            )
        );
        let abs = IndexFile {
            url: "https://files.example/p/a-1-py3-none-any.whl".into(),
            ..file.clone()
        };
        assert!(
            python_file_locator(&https(), &idx, &abs).is_ok(),
            "host policy is the transport's job"
        );
        let plus = IndexFile {
            filename: "a-1+x-py3-none-any.whl".into(),
            url: "a-1%2Bx-py3-none-any.whl".into(),
            ..file.clone()
        };
        assert!(
            python_file_locator(&https(), &idx, &plus).is_ok(),
            "percent-decoded last segment equals the filename"
        );
        let wrong = IndexFile {
            url: "https://files.example/p/evil.whl".into(),
            ..file
        };
        let m = match python_file_locator(&https(), &idx, &wrong) {
            Err(DepsError::Refused(m)) => m,
            other => panic!("{other:?}"),
        };
        assert!(m.contains("does not end in the file name"), "{m}");
    }

    #[test]
    fn npm_locators_follow_the_registry_layout() {
        let https = MirrorSpec::Https {
            base: Url::parse("https://registry.example/").expect("url"),
            hosts: vec!["registry.example".into()],
        };
        assert_eq!(
            npm_tarball_locator(&https, "left-pad", "1.3.0").expect("loc"),
            Locator::Http(
                Url::parse("https://registry.example/left-pad/-/left-pad-1.3.0.tgz").expect("url")
            )
        );
        match npm_tarball_locator(&https, "@scope/name", "2.0.0-rc.1").expect("loc") {
            Locator::Http(u) => assert_eq!(u.path(), "/@scope/name/-/name-2.0.0-rc.1.tgz"),
            other => panic!("{other:?}"),
        }
        let file = MirrorSpec::File {
            root: PathBuf::from("/m/npm"),
        };
        assert_eq!(
            npm_tarball_locator(&file, "@s/n", "1.0.0").expect("loc"),
            Locator::File(PathBuf::from("/m/npm/@s/n/-/n-1.0.0.tgz"))
        );
    }

    #[test]
    fn file_locators_accept_only_contained_relative_urls() {
        let spec = MirrorSpec::File {
            root: PathBuf::from("/m/pypi/simple"),
        };
        let idx = python_index_locator(&spec, "a").expect("loc");
        assert_eq!(
            idx,
            Locator::File(PathBuf::from("/m/pypi/simple/a/index.json"))
        );
        let mk = |url: &str, filename: &str| IndexFile {
            filename: filename.into(),
            url: url.into(),
            sha256: None,
            yanked: false,
        };
        let ok = python_file_locator(
            &spec,
            &idx,
            &mk("a-1-py3-none-any.whl", "a-1-py3-none-any.whl"),
        )
        .expect("loc");
        assert_eq!(
            ok,
            Locator::File(PathBuf::from("/m/pypi/simple/a/a-1-py3-none-any.whl"))
        );
        assert!(python_file_locator(&spec, &idx, &mk("sub/a.whl#sha256=ab", "a.whl")).is_ok());
        for (url, name) in [
            ("../b/a.whl", "a.whl"),
            ("..%2Fb%2Fa.whl", "a.whl"),
            ("%2e%2e/b/a.whl", "a.whl"),
            ("/etc/passwd", "passwd"),
            ("file:///etc/passwd", "passwd"),
            ("https://evil.test/a.whl", "a.whl"),
            ("a\\..\\b.whl", "b.whl"),
            ("a/./b.whl", "b.whl"),
            ("a//b.whl", "b.whl"),
            ("a.whl?x=1", "a.whl"),
            ("a.whl", "other.whl"),
            ("", "a.whl"),
            ("a%00.whl", "a.whl"),
        ] {
            assert!(
                matches!(
                    python_file_locator(&spec, &idx, &mk(url, name)),
                    Err(DepsError::Refused(_))
                ),
                "{url:?}"
            );
        }
    }
}
