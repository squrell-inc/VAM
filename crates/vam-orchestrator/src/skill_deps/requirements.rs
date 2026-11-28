//! `requirements.txt` in the one form VAM accepts: every requirement `name==version` with
//! `--hash=sha256:` (pip-compile `--generate-hashes`, `uv export`). Pure.

use super::limits::Limits;
use super::marker::{self, MarkerEnv};
use super::names::{normalize_project, valid_python_name};
use super::{refused, DepsError};

/// A pinned, hashed registry requirement that applies to the target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PyReq {
    /// PEP 503 normalised name.
    pub name: String,
    /// The name as the lockfile wrote it (for messages the author can search for).
    pub display: String,
    pub version: String,
    /// Lowercase hex sha256 digests (sorted, unique) of the files the author accepts.
    pub hashes: Vec<String>,
}

const FILE: &str = "requirements.txt";

fn err(line: usize, text: String) -> DepsError {
    refused(format!("{FILE} line {line}: {text}"))
}

const HASH_HINT: &str = "every requirement must be pinned and hashed (pip-compile --generate-hashes, or uv export --format requirements.txt)";

/// Joins `\` continuations, strips comments, and yields `(first line number, text)` for each
/// non-blank logical line.
fn logical_lines(text: &str) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut start = 0;
    for (i, raw) in text.lines().enumerate() {
        let n = i + 1;
        let stripped = strip_comment(raw);
        let (body, cont) = match stripped.trim_end().strip_suffix('\\') {
            Some(b) => (b, true),
            None => (stripped.as_str(), false),
        };
        if cur.is_empty() {
            start = n;
        }
        cur.push_str(body);
        cur.push(' ');
        if !cont {
            let line = cur.trim().to_string();
            if !line.is_empty() {
                out.push((start, line));
            }
            cur.clear();
        }
    }
    let tail = cur.trim().to_string();
    if !tail.is_empty() {
        out.push((start, tail));
    }
    out
}

/// A `#` starts a comment at the start of a line or after whitespace.
fn strip_comment(line: &str) -> String {
    let mut prev_ws = true;
    for (i, c) in line.char_indices() {
        if c == '#' && prev_ws {
            return line[..i].to_string();
        }
        prev_ws = c.is_whitespace();
    }
    line.to_string()
}

fn leading_name(s: &str) -> &str {
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(s.len());
    if end == 0 {
        s.trim()
    } else {
        &s[..end]
    }
}

/// The refusal kind of a requirement that is not a registry pin, if it is one.
fn non_registry_kind(spec: &str) -> Option<&'static str> {
    let s = spec.trim();
    if s.contains("git+") || s.starts_with("git:") || s.contains("git://") {
        return Some("git");
    }
    if s.contains("://")
        || s.contains(" @ ")
        || s.contains('@')
        || s.starts_with("file:")
        || s.contains("file:")
    {
        return Some("url");
    }
    if s.starts_with("./")
        || s.starts_with("../")
        || s.starts_with('/')
        || s.starts_with('~')
        || s.ends_with(".whl")
        || s.ends_with(".tar.gz")
        || s.ends_with(".zip")
    {
        return Some("path");
    }
    None
}

pub(crate) fn parse(
    bytes: &[u8],
    env: &MarkerEnv,
    limits: &Limits,
) -> Result<Vec<PyReq>, DepsError> {
    if bytes.len() as u64 > limits.max_lockfile_bytes {
        return Err(refused(format!(
            "{FILE} is larger than {} MiB; split the skill's dependencies or drop unused ones",
            limits.max_lockfile_bytes / (1024 * 1024)
        )));
    }
    if bytes.contains(&0) {
        return Err(refused(format!(
            "{FILE} contains a NUL byte; regenerate it with pip-compile or uv export"
        )));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| {
        refused(format!(
            "{FILE} is not UTF-8; regenerate it with pip-compile or uv export"
        ))
    })?;
    let lines = logical_lines(text);
    if lines.len() > limits.max_requirements {
        return Err(refused(format!(
            "{FILE} has more than {} lines; trim the dependency list",
            limits.max_requirements
        )));
    }
    let mut reqs: Vec<PyReq> = Vec::new();
    for (n, line) in lines {
        if let Some(req) = parse_line(n, &line, env)? {
            match reqs.iter_mut().find(|r| r.name == req.name) {
                Some(existing) if existing.version != req.version => {
                    return Err(err(
                        n,
                        format!(
                            "'{}' is pinned twice ({} and {}); keep one version",
                            req.display, existing.version, req.version
                        ),
                    ));
                }
                Some(existing) => {
                    existing.hashes.extend(req.hashes);
                    existing.hashes.sort();
                    existing.hashes.dedup();
                }
                None => reqs.push(req),
            }
        }
    }
    Ok(reqs)
}

fn parse_line(n: usize, line: &str, env: &MarkerEnv) -> Result<Option<PyReq>, DepsError> {
    // Option lines.
    if line.starts_with('-') {
        let first = line.split_whitespace().next().unwrap_or(line);
        let rest: Vec<&str> = line.split_whitespace().skip(1).collect();
        if first == "--require-hash" || first == "--require-hashes" {
            return Ok(None);
        }
        if first == "--only-binary" && rest == [":all:"] || first == "--only-binary=:all:" {
            return Ok(None);
        }
        if first == "-e" || first == "--editable" {
            let name = rest.first().copied().unwrap_or("");
            return Err(err(
                n,
                format!("'{name}' is a editable requirement; only registry packages locked by version and hash are accepted"),
            ));
        }
        return Err(err(
            n,
            format!("option '{first}' is not accepted; package sources are the node's mirrors, not the lockfile"),
        ));
    }

    // Pull the --hash options out (they may follow the marker); refuse any other option.
    let mut hashes_raw: Vec<String> = Vec::new();
    let mut kept: Vec<&str> = Vec::new();
    for tok in line.split_whitespace() {
        if let Some(h) = tok.strip_prefix("--hash=") {
            hashes_raw.push(h.to_string());
        } else if tok == "--hash" {
            // `--hash sha256:...` with a space is not pip syntax for a requirements file.
            return Err(err(
                n,
                "option '--hash' must be written --hash=sha256:<digest>".to_string(),
            ));
        } else if tok.starts_with("--") {
            return Err(err(
                n,
                format!("option '{tok}' is not accepted; package sources are the node's mirrors, not the lockfile"),
            ));
        } else {
            kept.push(tok);
        }
    }
    let body = kept.join(" ");
    let (spec, marker_src) = match body.split_once(';') {
        Some((s, m)) => (s.trim().to_string(), Some(m.trim().to_string())),
        None => (body.trim().to_string(), None),
    };
    let name_guess = leading_name(&spec).to_string();

    if let Some(kind) = non_registry_kind(&spec) {
        let article = if kind == "url" {
            "a url"
        } else if kind == "git" {
            "a git"
        } else {
            "a path"
        };
        return Err(err(
            n,
            format!("'{name_guess}' is {article} requirement; only registry packages locked by version and hash are accepted"),
        ));
    }

    // name[extras] then ==version
    let name_end = spec
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')))
        .unwrap_or(spec.len());
    let display = spec[..name_end].to_string();
    if !valid_python_name(&display) {
        return Err(err(n, format!("'{name_guess}' is not a valid package name; only registry packages locked by version and hash are accepted")));
    }
    let mut rest = spec[name_end..].trim_start();
    if let Some(after) = rest.strip_prefix('[') {
        match after.split_once(']') {
            Some((_extras, tail)) => rest = tail.trim_start(),
            None => {
                return Err(err(
                    n,
                    format!("'{display}' has an unterminated extras list"),
                ))
            }
        }
    }
    let not_pinned = || err(n, format!("'{display}' is not pinned with ==; {HASH_HINT}"));
    let version = match rest.strip_prefix("==") {
        Some(v) if !v.starts_with('=') => v.trim(),
        _ => return Err(not_pinned()),
    };
    if version.is_empty()
        || version.contains('*')
        || version.contains(',')
        || !version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '!' | '_' | '-'))
    {
        return Err(not_pinned());
    }

    if hashes_raw.is_empty() {
        return Err(err(
            n,
            format!("'{display}=={version}' has no --hash=sha256:...; {HASH_HINT}"),
        ));
    }
    let mut hashes = Vec::new();
    for raw in &hashes_raw {
        let (alg, hex) = raw.split_once(':').unwrap_or((raw.as_str(), ""));
        if alg != "sha256" {
            return Err(err(
                n,
                format!("'{display}=={version}' uses a {alg} hash; only sha256 is accepted"),
            ));
        }
        if hex.len() != 64 || !hex.bytes().all(|c| c.is_ascii_hexdigit()) {
            return Err(err(
                n,
                format!(
                    "'{display}=={version}' has a malformed sha256 hash; it must be 64 hex digits"
                ),
            ));
        }
        hashes.push(hex.to_ascii_lowercase());
    }
    hashes.sort();
    hashes.dedup();

    if let Some(m) = marker_src {
        match marker::evaluate(&m, env) {
            Ok(true) => {}
            Ok(false) => return Ok(None),
            Err(e) => {
                return Err(err(
                    n,
                    format!(
                        "'{display}=={version}' has a marker I cannot evaluate ('{}')",
                        e.0
                    ),
                ))
            }
        }
    }

    Ok(Some(PyReq {
        name: normalize_project(&display),
        display,
        version: version.to_string(),
        hashes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_deps::target::Target;

    const H1: &str = "ba5a8f0ad76889d0864d568cc3896dd5555f4d97d45017e1e334a2383e16c74a";
    const H2: &str = "1bdaa7f5307d93a041459d1e8520af1c52ea3c21874ff5d3c4bf1a20005eab38";

    fn run(text: &str) -> Result<Vec<PyReq>, DepsError> {
        parse(
            text.as_bytes(),
            &MarkerEnv::new(&Target::new(13, 41)),
            &Limits::default(),
        )
    }

    fn refusal(text: &str) -> String {
        match run(text) {
            Err(DepsError::Refused(m)) => m,
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn valid_lock_with_continuation_comments_and_blank_lines() {
        let text = format!(
            "# comment\n\n--require-hashes\n--only-binary :all:\nFoo_Bar[extra]==1.2.3 \\\n    --hash=sha256:{H1} \\\n    --hash=sha256:{H2}  # trailing\nbaz==2.0 --hash=sha256:{H1}\n"
        );
        let r = run(&text).expect("parse");
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].name, "foo-bar");
        assert_eq!(r[0].display, "Foo_Bar");
        assert_eq!(r[0].version, "1.2.3");
        assert_eq!(r[0].hashes.len(), 2, "multi-hash line keeps both");
        assert_eq!(r[1].name, "baz");
    }

    #[test]
    fn hash_after_marker_is_accepted() {
        let r = run(&format!(
            "a==1 ; sys_platform == 'linux' \\\n --hash=sha256:{H1}\n"
        ))
        .expect("parse");
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn option_lines_are_refused() {
        for opt in [
            "-r other.txt",
            "-c c.txt",
            "--index-url https://x/simple",
            "-i https://x",
            "--extra-index-url https://x",
            "--find-links /tmp",
            "-f /tmp",
            "--trusted-host x",
            "--no-binary :all:",
            "--prefer-binary",
            "--pre",
            "--use-feature fast-deps",
            "--only-binary pkg",
            "-q",
        ] {
            let m = refusal(&format!("{opt}\n"));
            let first = opt.split_whitespace().next().unwrap_or(opt);
            assert!(
                m.contains(&format!(
                    "requirements.txt line 1: option '{first}' is not accepted"
                )) && m.contains("package sources are the node's mirrors"),
                "{opt}: {m}"
            );
        }
        let m = refusal(&format!("a==1 --hash=sha256:{H1} --index-url https://x\n"));
        assert!(m.contains("option '--index-url' is not accepted"), "{m}");
    }

    #[test]
    fn missing_hash_names_the_requirement_and_the_fix() {
        let m = refusal("\nvamfixture-hello==1.0.0\n");
        assert!(
            m.starts_with("skill dependencies: requirements.txt line 2: "),
            "{m}"
        );
        assert!(
            m.contains("'vamfixture-hello==1.0.0' has no --hash=sha256:..."),
            "{m}"
        );
        assert!(
            m.contains("pip-compile --generate-hashes, or uv export --format requirements.txt"),
            "{m}"
        );
    }

    #[test]
    fn weak_or_malformed_hash_is_refused() {
        let m = refusal("a==1 --hash=md5:d41d8cd98f00b204e9800998ecf8427e\n");
        assert!(
            m.contains("'a==1' uses a md5 hash; only sha256 is accepted"),
            "{m}"
        );
        let m = refusal("a==1 --hash=sha512:abcd\n");
        assert!(m.contains("uses a sha512 hash"), "{m}");
        let m = refusal("a==1 --hash=sha256:abcd\n");
        assert!(m.contains("malformed sha256 hash"), "{m}");
        let m = refusal(
            "a==1 --hash=sha256:zz5a8f0ad76889d0864d568cc3896dd5555f4d97d45017e1e334a2383e16c74a\n",
        );
        assert!(m.contains("malformed sha256 hash"), "{m}");
    }

    #[test]
    fn only_exact_pins_are_accepted() {
        for spec in [
            "a>=1", "a~=1.2", "a===1.0", "a!=1.0", "a==1.*", "a", "a==1,<2", "a<2", "a==",
            "a[x]>=1",
        ] {
            let m = refusal(&format!("{spec} --hash=sha256:{H1}\n"));
            assert!(m.contains("is not pinned with =="), "{spec}: {m}");
        }
    }

    #[test]
    fn url_git_path_and_editable_requirements_are_refused() {
        let cases = [
            ("a @ https://example.test/a.whl", "a", "url"),
            ("a @ git+https://example.test/a.git", "a", "git"),
            ("git+https://example.test/a.git#egg=a", "git+https", "git"),
            ("https://example.test/a-1.whl", "https", "url"),
            ("file:///tmp/a.whl", "file", "url"),
            ("./local/a", "", "path"),
            ("../a", "", "path"),
            ("/abs/a", "", "path"),
            ("~/a", "", "path"),
            ("a-1.0-py3-none-any.whl", "a-1.0-py3-none-any.whl", "path"),
            ("pkg.tar.gz", "pkg.tar.gz", "path"),
            ("pkg.zip", "pkg.zip", "path"),
        ];
        for (line, _name, kind) in cases {
            let m = refusal(&format!("{line} --hash=sha256:{H1}\n"));
            let article = match kind {
                "url" => "a url",
                "git" => "a git",
                _ => "a path",
            };
            assert!(
                m.contains(&format!("is {article} requirement; only registry packages locked by version and hash are accepted")),
                "{line}: {m}"
            );
        }
        let m = refusal("-e ./a\n");
        assert!(m.contains("is a editable requirement"), "{m}");
        let m = refusal("-e git+https://x/a.git#egg=a\n");
        assert!(m.contains("editable requirement"), "{m}");
    }

    #[test]
    fn unknown_marker_variable() {
        let m = refusal(&format!(
            "a==1 ; platform_release == '6.1' --hash=sha256:{H1}\n"
        ));
        assert!(
            m.contains("has a marker I cannot evaluate ('platform_release')"),
            "{m}"
        );
        let m = refusal(&format!(
            "a==1 ; (sys_platform == 'linux' --hash=sha256:{H1}\n"
        ));
        assert!(m.contains("marker I cannot evaluate"), "{m}");
    }

    #[test]
    fn false_marker_drops_the_line() {
        let text = format!(
            "a==1 ; sys_platform == 'win32' --hash=sha256:{H1}\nb==2 ; python_version >= '3.9' --hash=sha256:{H2}\nc==3 ; extra == 'dev' --hash=sha256:{H1}\n"
        );
        let r = run(&text).expect("parse");
        assert_eq!(
            r.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            vec!["b"]
        );
    }

    #[test]
    fn duplicate_pin_with_a_different_version_is_refused() {
        let m = refusal(&format!(
            "a==1 --hash=sha256:{H1}\nA==2 --hash=sha256:{H1}\n"
        ));
        assert!(m.contains("pinned twice"), "{m}");
        // Same version twice merges the hashes.
        let r = run(&format!(
            "a==1 --hash=sha256:{H1}\nA==1 --hash=sha256:{H2}\n"
        ))
        .expect("parse");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].hashes.len(), 2);
        // Dropped by marker: no conflict.
        assert!(run(&format!(
            "a==1 --hash=sha256:{H1}\na==2 ; sys_platform == 'win32' --hash=sha256:{H1}\n"
        ))
        .is_ok());
    }

    #[test]
    fn lockfile_size_caps() {
        let limits = Limits {
            max_requirements: 3,
            max_lockfile_bytes: 200,
            ..Limits::default()
        };
        let env = MarkerEnv::new(&Target::new(13, 41));
        let many = (0..4)
            .map(|i| format!("p{i}==1 --hash=sha256:{H1}\n"))
            .collect::<String>();
        let big_lines = parse(
            many.as_bytes(),
            &env,
            &Limits {
                max_requirements: 3,
                ..Limits::default()
            },
        );
        assert!(matches!(big_lines, Err(DepsError::Refused(m)) if m.contains("more than 3 lines")));
        let big = "#".repeat(300);
        assert!(
            matches!(parse(big.as_bytes(), &env, &limits), Err(DepsError::Refused(m)) if m.contains("larger than"))
        );
        assert!(
            matches!(parse(b"a==1\0", &env, &Limits::default()), Err(DepsError::Refused(m)) if m.contains("NUL"))
        );
        assert!(
            matches!(parse(&[0xff, 0xfe, b'a'], &env, &Limits::default()), Err(DepsError::Refused(m)) if m.contains("UTF-8"))
        );
        // The real default is 2000.
        let lots = (0..2001)
            .map(|i| format!("p{i}==1 --hash=sha256:{H1}\n"))
            .collect::<String>();
        assert!(
            matches!(parse(lots.as_bytes(), &env, &Limits { max_lockfile_bytes: u64::MAX, ..Limits::default() }), Err(DepsError::Refused(m)) if m.contains("more than 2000 lines"))
        );
    }
}
