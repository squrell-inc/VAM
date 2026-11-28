//! Resolver tests on the W0 fixtures (`e2e/world/fixtures/package-mirror`), on mirrors built in
//! the tests, and on scripted failures. No test reaches the network.

use super::*;
use crate::skill_deps::testutil::{Reply, TestServer};
use crate::skill_deps::{plan, Mirrors};
use std::sync::atomic::{AtomicUsize, Ordering};

impl Resolver {
    /// Test seam: replace the transport (to count fetches or run a hook mid-fetch).
    fn with_transport(mut self, t: Arc<dyn Transport>) -> Resolver {
        self.transport_override = Some(t);
        self
    }
}

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/world/fixtures")
}

fn world() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../e2e/world")
}

fn target() -> Target {
    Target::new(13, 41)
}

fn file_mirrors(root: &Path) -> Mirrors {
    let u = |p: PathBuf| {
        url::Url::from_directory_path(p)
            .map(|u| u.to_string())
            .unwrap_or_default()
    };
    let spec = format!(
        "pypi={},npm={}",
        u(root.join("pypi/simple")),
        u(root.join("npm"))
    );
    Mirrors::parse(Some(&spec)).expect("mirrors")
}

fn w0_mirrors() -> Mirrors {
    file_mirrors(&fixtures().join("package-mirror"))
}

fn w0_files(dir: &str) -> SkillFiles {
    SkillFiles::from_dir(&fixtures().join(dir)).expect("fixture dir")
}

fn resolver(mirrors: Mirrors, drive9: &Path) -> Resolver {
    Resolver::new(mirrors, drive9, Limits::default())
}

fn tmp_count(drive9: &Path) -> usize {
    std::fs::read_dir(drive9.join("deps/.tmp"))
        .map(|d| d.count())
        .unwrap_or(0)
}

fn refusal<T: std::fmt::Debug>(r: Result<T, DepsError>) -> String {
    match r {
        Err(DepsError::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

// ---- a transport that counts and can run a hook ----

struct Counting {
    inner: Arc<dyn Transport>,
    gets: AtomicUsize,
    hook: Box<dyn Fn(&Locator) + Send + Sync>,
}

impl Counting {
    fn over(root: &Path, hook: impl Fn(&Locator) + Send + Sync + 'static) -> Arc<Counting> {
        let canon = std::fs::canonicalize(root).expect("canon");
        Arc::new(Counting {
            inner: Arc::new(FileTransport::new(canon)),
            gets: AtomicUsize::new(0),
            hook: Box::new(hook),
        })
    }

    fn count(&self) -> usize {
        self.gets.load(Ordering::SeqCst)
    }
}

impl Transport for Counting {
    fn get(
        &self,
        loc: &Locator,
        json: bool,
        cap: u64,
        deadline: Instant,
        out: &mut dyn Write,
    ) -> Result<Fetched, DepsError> {
        self.gets.fetch_add(1, Ordering::SeqCst);
        (self.hook)(loc);
        self.inner.get(loc, json, cap, deadline, out)
    }
}

// ---- a mirror built in the test ----

fn sha256_hex(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

fn zip_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, data) in files {
        let opts = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored);
        w.start_file(*name, opts).expect("start");
        w.write_all(data).expect("write");
    }
    w.finish().expect("finish").into_inner()
}

fn tgz_bytes(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, data) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(data.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, name, *data).expect("append");
    }
    let raw = b.into_inner().expect("tar");
    let mut enc = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    enc.write_all(&raw).expect("gz");
    enc.finish().expect("finish")
}

struct Mirror {
    dir: tempfile::TempDir,
}

impl Mirror {
    fn new() -> Mirror {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("pypi/simple")).expect("mkdir");
        std::fs::create_dir_all(dir.path().join("npm")).expect("mkdir");
        Mirror { dir }
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn mirrors(&self) -> Mirrors {
        file_mirrors(self.root())
    }

    /// A project with one file; `index_hash` is what the index claims (the lock uses it).
    fn python(
        &self,
        project: &str,
        filename: &str,
        bytes: &[u8],
        index_hash: &str,
        yanked: bool,
        url: &str,
    ) {
        let pdir = self.root().join("pypi/simple").join(project);
        std::fs::create_dir_all(&pdir).expect("mkdir");
        if !filename.is_empty() {
            std::fs::write(pdir.join(filename), bytes).expect("write");
        }
        let index = serde_json::json!({
            "meta": {"api-version": "1.0"},
            "name": project,
            "files": [{"filename": filename, "url": url, "hashes": {"sha256": index_hash}, "yanked": yanked}]
        });
        std::fs::write(
            pdir.join("index.json"),
            serde_json::to_vec(&index).expect("json"),
        )
        .expect("write");
    }

    fn npm(&self, name: &str, version: &str, bytes: &[u8]) {
        let base = name.rsplit('/').next().unwrap_or(name);
        let mut dir = self.root().join("npm");
        for part in name.split('/') {
            dir.push(part);
        }
        dir.push("-");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(format!("{base}-{version}.tgz")), bytes).expect("write");
    }
}

fn requirements(name: &str, version: &str, hash: &str) -> SkillFiles {
    let text = format!("{name}=={version} \\\n    --hash=sha256:{hash}\n");
    SkillFiles::from_entries(&[("requirements.txt", text.as_bytes())], false)
}

fn integrity_of(bytes: &[u8]) -> String {
    use base64::Engine as _;
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
    )
}

fn npm_files(entries: &[(&str, &str, &str)]) -> SkillFiles {
    // (lock key path, version, integrity)
    let mut packages = serde_json::Map::new();
    let mut deps = serde_json::Map::new();
    for (path, version, integrity) in entries {
        let name = path.rsplit("node_modules/").next().unwrap_or(path);
        if path.matches("node_modules/").count() == 1 {
            deps.insert(name.to_string(), serde_json::json!(version));
        }
        let base = name.rsplit('/').next().unwrap_or(name);
        packages.insert(
            path.to_string(),
            serde_json::json!({
                "version": version,
                "resolved": format!("https://registry.npmjs.org/{name}/-/{base}-{version}.tgz"),
                "integrity": integrity,
            }),
        );
    }
    packages.insert(String::new(), serde_json::json!({"dependencies": deps}));
    let lock = serde_json::to_vec(&serde_json::json!({"lockfileVersion": 3, "packages": packages}))
        .expect("json");
    SkillFiles::from_entries(&[("package-lock.json", lock.as_slice())], false)
}

fn good_wheel() -> Vec<u8> {
    zip_bytes(&[
        ("demo/__init__.py", b"VALUE = 7\n"),
        ("demo-1.0.dist-info/METADATA", b"Name: demo\n"),
    ])
}

fn good_tgz() -> Vec<u8> {
    tgz_bytes(&[
        ("package/package.json", b"{}"),
        ("package/index.js", b"module.exports = 1\n"),
    ])
}

// ---------------------------------------------------------------------------------------------
// W0 fixtures
// ---------------------------------------------------------------------------------------------

#[test]
fn w0_python_lockfile_resolves() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(w0_mirrors(), drive9.path());
    let files = w0_files("lockfile-python");
    let first = r.resolve(&files, &target()).expect("resolve");
    assert_eq!(first.len(), 1);
    let one = &first[0];
    assert_eq!(one.ecosystem, Ecosystem::Python);
    assert_eq!(one.source, Source::Mirror);
    assert_eq!(one.package_count, 1);
    assert_eq!(one.file_count, 4, "the wheel's four files");
    assert!(one.tree.join("vamfixture_hello/__init__.py").is_file());
    assert!(one.dir.join("deps.json").is_file());
    assert_eq!(one.dir, drive9.path().join("deps").join(&one.key));
    assert_eq!(one.key.len(), 64);
    let json: serde_json::Value =
        serde_json::from_slice(&std::fs::read(one.dir.join("deps.json")).expect("read"))
            .expect("json");
    assert_eq!(json["v"], 1);
    assert_eq!(json["ecosystem"], "python");
    assert_eq!(json["fetcher"], 1);
    assert_eq!(json["packages"][0]["name"], "vamfixture-hello");
    assert_eq!(
        json["packages"][0]["sha256"],
        "ba5a8f0ad76889d0864d568cc3896dd5555f4d97d45017e1e334a2383e16c74a"
    );
    let second = r.resolve(&files, &target()).expect("resolve again");
    assert_eq!(
        second[0].source,
        Source::Cache,
        "the second resolve is served from the cache"
    );
    assert_eq!(second[0].key, one.key);
    assert_eq!(tmp_count(drive9.path()), 0, "no staging left");
}

#[test]
fn w0_js_lockfile_resolves() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(w0_mirrors(), drive9.path());
    let files = w0_files("lockfile-js");
    let got = r.resolve(&files, &target()).expect("resolve");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].ecosystem, Ecosystem::Js);
    assert!(got[0].tree.ends_with("node_modules"));
    let index = got[0].tree.join("vamfixture-hello/index.js");
    assert!(index.is_file(), "{}", index.display());
    assert!(got[0].tree.join("vamfixture-hello/package.json").is_file());
    assert_eq!(
        r.resolve(&files, &target()).expect("again")[0].source,
        Source::Cache
    );
}

fn s3_contains() -> Vec<String> {
    let yaml = std::fs::read_to_string(
        world()
            .parent()
            .expect("e2e")
            .join("scenarios/skill-with-a-source-only-package-is-refused-when-published.yaml"),
    )
    .expect("scenario");
    let line = yaml
        .lines()
        .find(|l| l.contains("contains: ["))
        .expect("a contains list");
    let inner = line
        .split_once("contains: [")
        .expect("list")
        .1
        .split(']')
        .next()
        .expect("end");
    inner.split(',').map(|s| s.trim().to_string()).collect()
}

#[test]
fn w0_sdist_lockfile_refused() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(w0_mirrors(), drive9.path());
    let m = refusal(r.resolve(&w0_files("lockfile-sdist"), &target()));
    // The pair is read from S3's YAML, so scenario and code cannot drift.
    let want = s3_contains();
    assert_eq!(want, vec!["vamfixture-sdist-only", "source"]);
    for fragment in &want {
        assert!(
            m.contains(fragment.as_str()),
            "S3 expects '{fragment}' in: {m}"
        );
    }
    assert!(
        !drive9
            .path()
            .join("deps")
            .read_dir()
            .expect("ls")
            .flatten()
            .any(|e| e.file_name() != ".tmp"),
        "nothing cached"
    );
    assert_eq!(tmp_count(drive9.path()), 0);
}

#[test]
fn w0_same_lockfile_two_skills_share_key() {
    // S4's two skill pieces both map requirements.txt to the same fixture file.
    let piece = |name: &str| {
        let y = std::fs::read_to_string(world().join(format!("{name}.yaml"))).expect("piece");
        let line = y
            .lines()
            .find(|l| l.trim_start().starts_with("requirements.txt:"))
            .expect("requirements line");
        line.split_once(':').expect("kv").1.trim().to_string()
    };
    let a = piece("skill-with-python-lockfile");
    let b = piece("skill-with-the-same-python-lockfile");
    assert_eq!(a, b, "the pieces share one lockfile");
    let read = |p: &str| std::fs::read(fixtures().join(p)).expect("read");
    let fa = SkillFiles::from_entries(&[("requirements.txt", read(&a).as_slice())], false);
    let fb = SkillFiles::from_entries(&[("requirements.txt", read(&b).as_slice())], false);
    assert_eq!(
        plan(&fa, &target()).expect("a"),
        plan(&fb, &target()).expect("b")
    );
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(w0_mirrors(), drive9.path());
    assert_eq!(
        r.resolve(&fa, &target()).expect("a")[0].source,
        Source::Mirror
    );
    assert_eq!(
        r.resolve(&fb, &target()).expect("b")[0].source,
        Source::Cache
    );
}

#[test]
fn lock_url_is_never_fetched() {
    // Lockfile URLs point at a loopback server; the resolver must never connect to it.
    let server = TestServer::start(|_| Reply::ok(b"should never be asked".to_vec()));
    let port = server.port();
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(w0_mirrors(), drive9.path());
    // npm: `resolved` names the server (registry-shaped, so it passes the shape check).
    let js =
        std::fs::read_to_string(fixtures().join("lockfile-js/package-lock.json")).expect("lock");
    let js = js.replace(
        "https://registry.npmjs.org",
        &format!("http://127.0.0.1:{port}"),
    );
    let files = SkillFiles::from_entries(&[("package-lock.json", js.as_bytes())], false);
    assert!(js.contains("127.0.0.1"));
    assert_eq!(r.resolve(&files, &target()).expect("resolve").len(), 1);
    // pylock: the wheel's `url` names the server.
    let pylock = format!(
        "lock-version = \"1.0\"\n[[packages]]\nname = \"vamfixture-hello\"\nversion = \"1.0.0\"\n[[packages.wheels]]\nname = \"vamfixture_hello-1.0.0-py3-none-any.whl\"\nurl = \"http://127.0.0.1:{port}/x.whl\"\n[packages.wheels.hashes]\nsha256 = \"ba5a8f0ad76889d0864d568cc3896dd5555f4d97d45017e1e334a2383e16c74a\"\n"
    );
    let files = SkillFiles::from_entries(&[("pylock.toml", pylock.as_bytes())], false);
    assert_eq!(
        r.resolve(&files, &target()).expect("resolve")[0].ecosystem,
        Ecosystem::Python
    );
    assert_eq!(server.requests().len(), 0, "the lockfile URL was contacted");
}

#[test]
fn both_ecosystems_resolve_together_and_the_skill_cap_spans_them() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(w0_mirrors(), drive9.path());
    let py = std::fs::read(fixtures().join("lockfile-python/requirements.txt")).expect("read");
    let lock = std::fs::read(fixtures().join("lockfile-js/package-lock.json")).expect("read");
    let files = SkillFiles::from_entries(
        &[
            ("requirements.txt", py.as_slice()),
            ("package-lock.json", lock.as_slice()),
        ],
        false,
    );
    let got = r.resolve(&files, &target()).expect("resolve");
    assert_eq!(
        got.iter().map(|g| g.ecosystem).collect::<Vec<_>>(),
        vec![Ecosystem::Python, Ecosystem::Js]
    );
    let total: u64 = got.iter().map(|g| g.bytes).sum();
    // Each tree alone fits a cap just above the larger one; together they do not.
    let cap = got.iter().map(|g| g.bytes).max().unwrap_or(0) + 1;
    assert!(total > cap);
    let drive9b = tempfile::tempdir().expect("tempdir");
    let tight = Resolver::new(
        w0_mirrors(),
        drive9b.path(),
        Limits {
            max_unpacked_skill_bytes: cap,
            ..Limits::default()
        },
    );
    let m = refusal(tight.resolve(&files, &target()));
    assert!(
        m.contains("skill packages are over the") && m.contains("limit (unpacked)"),
        "{m}"
    );
}

// ---------------------------------------------------------------------------------------------
// Security rows
// ---------------------------------------------------------------------------------------------

#[test]
fn hash_mismatch_is_refused_and_not_unpacked() {
    // The index lists the locked hash but the mirror serves other bytes (not even a zip: if the
    // unpacker ran, the error would be about the archive, not the hash).
    let m = Mirror::new();
    let locked = sha256_hex(b"what the author locked");
    m.python(
        "demo",
        "demo-1.0-py3-none-any.whl",
        b"tampered bytes",
        &locked,
        false,
        "demo-1.0-py3-none-any.whl",
    );
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(m.mirrors(), drive9.path());
    let text = refusal(r.resolve(&requirements("demo", "1.0", &locked), &target()));
    assert!(
        text.contains("package 'demo' 1.0: the downloaded file does not match its locked hash"),
        "{text}"
    );
    assert_eq!(tmp_count(drive9.path()), 0, "the temp file is gone");
    let key = plan(&requirements("demo", "1.0", &locked), &target()).expect("plan")[0]
        .key
        .clone();
    assert!(
        !drive9.path().join("deps").join(key).exists(),
        "nothing was published"
    );

    // npm: the integrity is checked on the bytes.
    let m = Mirror::new();
    m.npm("demo", "1.0.0", b"not the locked tarball");
    let integrity = integrity_of(b"the locked tarball");
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(m.mirrors(), drive9.path());
    let text = refusal(r.resolve(
        &npm_files(&[("node_modules/demo", "1.0.0", &integrity)]),
        &target(),
    ));
    assert!(
        text.contains("package 'demo' 1.0.0: the downloaded file does not match its locked hash"),
        "{text}"
    );
    assert_eq!(tmp_count(drive9.path()), 0);
}

#[test]
fn index_not_json_not_found_and_wrong_project() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let h = sha256_hex(b"x");
    // HTML where JSON belongs.
    let m = Mirror::new();
    let pdir = m.root().join("pypi/simple/demo");
    std::fs::create_dir_all(&pdir).expect("mkdir");
    std::fs::write(
        pdir.join("index.json"),
        b"<!DOCTYPE html><html><body>Not Found</body></html>",
    )
    .expect("w");
    let r = resolver(m.mirrors(), drive9.path());
    let text = refusal(r.resolve(&requirements("demo", "1.0", &h), &target()));
    assert!(
        text.contains("the pypi mirror does not serve the PEP 691 JSON index"),
        "{text}"
    );
    // No index at all: not in the mirror.
    let m = Mirror::new();
    let r = resolver(m.mirrors(), drive9.path());
    let text = refusal(r.resolve(&requirements("demo", "1.0", &h), &target()));
    assert!(
        text.contains("package 'demo' 1.0 is not in the package mirror (sha256 2d711642b726...)"),
        "{text}"
    );
    // npm tarball missing.
    let text = refusal(r.resolve(
        &npm_files(&[("node_modules/demo", "1.0.0", &integrity_of(b"x"))]),
        &target(),
    ));
    assert!(
        text.contains("package 'demo' 1.0.0 is not in the package mirror"),
        "{text}"
    );
    // An index about another project.
    let m = Mirror::new();
    m.python(
        "other",
        "other-1.0-py3-none-any.whl",
        b"x",
        &h,
        false,
        "other-1.0-py3-none-any.whl",
    );
    std::fs::rename(
        m.root().join("pypi/simple/other"),
        m.root().join("pypi/simple/demo"),
    )
    .expect("rename");
    let r = resolver(m.mirrors(), drive9.path());
    assert!(
        refusal(r.resolve(&requirements("demo", "1.0", &h), &target()))
            .contains("is about 'other'")
    );
}

#[test]
fn yanked_wheel_is_refused_naming_the_package() {
    let m = Mirror::new();
    let wheel = good_wheel();
    let h = sha256_hex(&wheel);
    m.python(
        "demo",
        "demo-1.0-py3-none-any.whl",
        &wheel,
        &h,
        true,
        "demo-1.0-py3-none-any.whl",
    );
    let drive9 = tempfile::tempdir().expect("tempdir");
    let text = refusal(
        resolver(m.mirrors(), drive9.path()).resolve(&requirements("demo", "1.0", &h), &target()),
    );
    assert!(
        text.contains("package 'demo' 1.0 was yanked on the index"),
        "{text}"
    );
}

#[test]
fn file_mirror_escape_refused() {
    let wheel = good_wheel();
    let h = sha256_hex(&wheel);
    let drive9 = tempfile::tempdir().expect("tempdir");
    // A `..` in the index url never becomes a path.
    let outside = tempfile::tempdir().expect("tempdir");
    std::fs::write(outside.path().join("demo-1.0-py3-none-any.whl"), &wheel).expect("w");
    let m = Mirror::new();
    let evil = format!(
        "../../../../..{}/demo-1.0-py3-none-any.whl",
        outside.path().display()
    );
    m.python(
        "demo",
        "demo-1.0-py3-none-any.whl",
        &wheel,
        &h,
        false,
        &evil,
    );
    let text = refusal(
        resolver(m.mirrors(), drive9.path()).resolve(&requirements("demo", "1.0", &h), &target()),
    );
    assert!(
        text.contains("must be a relative path inside the mirror"),
        "{text}"
    );
    // A symlink inside the mirror that points out is refused at read time.
    let m = Mirror::new();
    m.python("demo", "", b"", &h, false, "demo-1.0-py3-none-any.whl");
    std::os::unix::fs::symlink(
        outside.path().join("demo-1.0-py3-none-any.whl"),
        m.root().join("pypi/simple/demo/demo-1.0-py3-none-any.whl"),
    )
    .expect("link");
    let index = serde_json::json!({"name": "demo", "files": [{"filename": "demo-1.0-py3-none-any.whl", "url": "demo-1.0-py3-none-any.whl", "hashes": {"sha256": h}}]});
    std::fs::write(
        m.root().join("pypi/simple/demo/index.json"),
        serde_json::to_vec(&index).expect("json"),
    )
    .expect("w");
    let text = refusal(
        resolver(m.mirrors(), drive9.path()).resolve(&requirements("demo", "1.0", &h), &target()),
    );
    assert!(text.contains("link or a non-file"), "{text}");
    // An npm tarball path that is a symlink out.
    let m = Mirror::new();
    let tgz = good_tgz();
    std::fs::create_dir_all(m.root().join("npm/demo/-")).expect("mkdir");
    std::fs::write(outside.path().join("t.tgz"), &tgz).expect("w");
    std::os::unix::fs::symlink(
        outside.path().join("t.tgz"),
        m.root().join("npm/demo/-/demo-1.0.0.tgz"),
    )
    .expect("link");
    let text = refusal(resolver(m.mirrors(), drive9.path()).resolve(
        &npm_files(&[("node_modules/demo", "1.0.0", &integrity_of(&tgz))]),
        &target(),
    ));
    assert!(text.contains("link or a non-file"), "{text}");
    assert_eq!(tmp_count(drive9.path()), 0);
}

#[test]
fn skill_cap_refused() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = Resolver::new(
        w0_mirrors(),
        drive9.path(),
        Limits {
            max_unpacked_skill_bytes: 200,
            ..Limits::default()
        },
    );
    let text = refusal(r.resolve(&w0_files("lockfile-python"), &target()));
    assert!(text.contains("package 'vamfixture-hello' 1.0.0: skill packages are over the 200 bytes limit (unpacked)"), "{text}");
    assert_eq!(tmp_count(drive9.path()), 0);
    // A package count cap, at plan time.
    let r = Resolver::new(
        w0_mirrors(),
        drive9.path(),
        Limits {
            max_packages: 0,
            ..Limits::default()
        },
    );
    assert!(refusal(r.resolve(&w0_files("lockfile-python"), &target()))
        .contains("at most 0 are accepted"));
}

#[test]
fn download_cap_refused() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = Resolver::new(
        w0_mirrors(),
        drive9.path(),
        Limits {
            max_download_bytes: 100,
            ..Limits::default()
        },
    );
    let text = refusal(r.resolve(&w0_files("lockfile-python"), &target()));
    assert!(
        text.contains("package 'vamfixture-hello' 1.0.0:") && text.contains("more than"),
        "{text}"
    );
    assert_eq!(
        tmp_count(drive9.path()),
        0,
        "the partial download is deleted"
    );
}

#[test]
fn resolve_semaphore() {
    let gate = Gate::new(2);
    let far = Instant::now() + Duration::from_secs(5);
    let a = gate.acquire(far).expect("first");
    let _b = gate.acquire(far).expect("second");
    let started = Instant::now();
    let third = gate.acquire(Instant::now() + Duration::from_millis(150));
    assert!(matches!(third, Err(DepsError::Unavailable(m)) if m.contains("busy")));
    assert!(started.elapsed() >= Duration::from_millis(100));
    drop(a);
    assert!(gate.acquire(far).is_ok(), "a released permit is reusable");
}

#[test]
fn resolve_deadline_is_unavailable() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = Resolver::new(
        w0_mirrors(),
        drive9.path(),
        Limits {
            resolve_deadline: Duration::ZERO,
            ..Limits::default()
        },
    );
    assert!(matches!(
        r.resolve(&w0_files("lockfile-python"), &target()),
        Err(DepsError::Unavailable(_))
    ));
    assert_eq!(tmp_count(drive9.path()), 0);
}

// ---------------------------------------------------------------------------------------------
// Cache behaviour
// ---------------------------------------------------------------------------------------------

fn deps_dirs(drive9: &Path) -> Vec<String> {
    std::fs::read_dir(drive9.join("deps"))
        .expect("ls")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| n != ".tmp")
        .collect()
}

#[test]
fn two_threads_same_key_one_tree_one_fetch() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let mirror_root = fixtures().join("package-mirror");
    let counting = Counting::over(&mirror_root, |_| {
        std::thread::sleep(Duration::from_millis(50));
    });
    let r = Arc::new(resolver(w0_mirrors(), drive9.path()).with_transport(counting.clone()));
    let files = Arc::new(w0_files("lockfile-python"));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let (r, files) = (r.clone(), files.clone());
            std::thread::spawn(move || r.resolve(&files, &target()))
        })
        .collect();
    let results: Vec<Resolved> = handles
        .into_iter()
        .map(|h| h.join().expect("thread").expect("resolve").remove(0))
        .collect();
    assert_eq!(results[0].key, results[1].key);
    assert_eq!(deps_dirs(drive9.path()).len(), 1, "one tree");
    assert_eq!(
        counting.count(),
        2,
        "one index fetch and one wheel fetch in total"
    );
    let mut sources: Vec<&str> = results.iter().map(|r| r.source.as_str()).collect();
    sources.sort();
    assert_eq!(sources, vec!["cache", "mirror"]);
    assert_eq!(tmp_count(drive9.path()), 0);
    assert!(
        r.in_flight.lock().map(|m| m.is_empty()).unwrap_or(false),
        "the key lock is released"
    );
}

#[test]
fn lost_rename_race_returns_the_winner() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let files = w0_files("lockfile-python");
    let key = plan(&files, &target()).expect("plan")[0].key.clone();
    let dir = drive9.path().join("deps").join(&key);
    // While we are fetching, "another process" lands a finished tree at deps/<key>.
    let winner_dir = dir.clone();
    let key2 = key.clone();
    let counting = Counting::over(&fixtures().join("package-mirror"), move |loc| {
        if matches!(loc, Locator::File(p) if p.extension().is_some_and(|e| e == "whl"))
            && !winner_dir.exists()
        {
            std::fs::create_dir_all(winner_dir.join("python")).expect("mkdir");
            std::fs::write(winner_dir.join("python/winner.py"), b"w").expect("w");
            let json = serde_json::json!({"v": 1, "ecosystem": "python", "key": key2, "target": "t", "fetcher": 1, "files": 1, "bytes": 999, "packages": []});
            std::fs::write(
                winner_dir.join("deps.json"),
                serde_json::to_vec(&json).expect("json"),
            )
            .expect("w");
        }
    });
    let r = resolver(w0_mirrors(), drive9.path()).with_transport(counting);
    let got = r.resolve(&files, &target()).expect("resolve");
    assert_eq!(got[0].bytes, 999, "the winner's tree is what is returned");
    assert_eq!(
        got[0].source,
        Source::Mirror,
        "the caller that fetched reports mirror"
    );
    assert!(dir.join("python/winner.py").is_file());
    assert!(
        !dir.join("python/vamfixture_hello").exists(),
        "the loser's tree was dropped"
    );
    assert_eq!(tmp_count(drive9.path()), 0, "the loser's staging is gone");
}

#[test]
fn corrupt_cache_entry_is_refetched() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let counting = Counting::over(&fixtures().join("package-mirror"), |_| {});
    let r = resolver(w0_mirrors(), drive9.path()).with_transport(counting.clone());
    let files = w0_files("lockfile-python");
    let first = r.resolve(&files, &target()).expect("resolve").remove(0);
    assert_eq!(counting.count(), 2);
    // A half-written or planted entry.
    std::fs::write(first.dir.join("deps.json"), b"{\"v\": 1").expect("corrupt");
    let hits = r.lookup(&files, &target()).expect("lookup");
    assert!(hits[0].1.is_none(), "lookup treats it as a miss");
    assert_eq!(counting.count(), 2, "lookup never touches the mirror");
    let again = r.resolve(&files, &target()).expect("resolve").remove(0);
    assert_eq!(again.source, Source::Mirror, "refetched");
    assert_eq!(counting.count(), 4);
    assert!(again.tree.join("vamfixture_hello/__init__.py").is_file());
    assert_eq!(
        r.resolve(&files, &target()).expect("resolve")[0].source,
        Source::Cache
    );
    assert!(
        tmp_count(drive9.path()) >= 1,
        "the corrupt entry sits in .tmp/trash-* until a sweep"
    );
    assert!(r.sweep_stale_tmp(Duration::ZERO) >= 1);
    assert_eq!(tmp_count(drive9.path()), 0);
}

#[test]
fn cap_shrink_invalidates_a_hit() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let files = w0_files("lockfile-python");
    let big = resolver(w0_mirrors(), drive9.path());
    big.resolve(&files, &target()).expect("resolve");
    assert!(big.lookup(&files, &target()).expect("lookup")[0]
        .1
        .is_some());
    // A node restarted with a smaller cap refuses the tree built under the larger one.
    let small = Resolver::new(
        w0_mirrors(),
        drive9.path(),
        Limits {
            max_unpacked_skill_bytes: 100,
            ..Limits::default()
        },
    );
    assert!(small.lookup(&files, &target()).expect("lookup")[0]
        .1
        .is_none());
}

#[test]
fn lookup_is_cache_only() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let counting = Counting::over(&fixtures().join("package-mirror"), |_| {});
    let r = resolver(w0_mirrors(), drive9.path()).with_transport(counting.clone());
    let files = w0_files("lockfile-python");
    let miss = r.lookup(&files, &target()).expect("lookup");
    assert_eq!(miss.len(), 1);
    assert!(miss[0].1.is_none());
    assert_eq!(counting.count(), 0);
    r.resolve(&files, &target()).expect("resolve");
    let hit = r.lookup(&files, &target()).expect("lookup");
    assert_eq!(hit[0].1.as_ref().map(|r| r.source), Some(Source::Cache));
    assert_eq!(
        hit[0].0.key,
        hit[0].1.as_ref().map(|r| r.key.clone()).unwrap_or_default()
    );
    assert!(r
        .lookup(&SkillFiles::default(), &target())
        .expect("lookup")
        .is_empty());
}

#[test]
fn cached_tree_is_read_only() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let got = resolver(w0_mirrors(), drive9.path())
        .resolve(&w0_files("lockfile-python"), &target())
        .expect("resolve")
        .remove(0);
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: PathBuf| std::fs::metadata(p).expect("meta").permissions().mode() & 0o7777;
    assert_eq!(mode(got.tree.join("vamfixture_hello/__init__.py")), 0o444);
    assert_eq!(mode(got.tree.join("vamfixture_hello")), 0o755);
    // Hard-linking a cached file works (VI-1.7 links the tree into each staged skill).
    let link = drive9.path().join("linked.py");
    std::fs::hard_link(got.tree.join("vamfixture_hello/__init__.py"), &link).expect("hard link");
    assert_eq!(mode(link), 0o444);
}

#[test]
fn nothing_is_left_in_tmp_after_failures_and_successes() {
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = resolver(w0_mirrors(), drive9.path());
    let _ = r.resolve(&w0_files("lockfile-sdist"), &target());
    assert_eq!(tmp_count(drive9.path()), 0);
    r.resolve(&w0_files("lockfile-js"), &target())
        .expect("resolve");
    assert_eq!(tmp_count(drive9.path()), 0);
}

#[test]
fn nested_npm_packages_land_at_their_lock_paths() {
    let m = Mirror::new();
    let outer = tgz_bytes(&[("package/package.json", b"{}"), ("package/outer.js", b"1")]);
    let inner = tgz_bytes(&[("package/package.json", b"{}"), ("package/inner.js", b"2")]);
    m.npm("outer", "1.0.0", &outer);
    m.npm("@scope/inner", "2.0.0", &inner);
    let files = npm_files(&[
        ("node_modules/outer", "1.0.0", &integrity_of(&outer)),
        (
            "node_modules/outer/node_modules/@scope/inner",
            "2.0.0",
            &integrity_of(&inner),
        ),
    ]);
    let drive9 = tempfile::tempdir().expect("tempdir");
    let got = resolver(m.mirrors(), drive9.path())
        .resolve(&files, &target())
        .expect("resolve")
        .remove(0);
    assert_eq!(got.package_count, 2);
    assert!(got.tree.join("outer/outer.js").is_file());
    assert!(got
        .tree
        .join("outer/node_modules/@scope/inner/inner.js")
        .is_file());
}

#[test]
fn a_hostile_but_hash_matching_wheel_is_refused_by_the_unpacker() {
    // The hash matches (the lock's author chose it), the archive is hostile.
    let evil = zip_bytes(&[
        ("../../escape.py", b"x"),
        ("demo-1.0.dist-info/METADATA", b"x"),
    ]);
    let h = sha256_hex(&evil);
    let m = Mirror::new();
    m.python(
        "demo",
        "demo-1.0-py3-none-any.whl",
        &evil,
        &h,
        false,
        "demo-1.0-py3-none-any.whl",
    );
    let drive9 = tempfile::tempdir().expect("tempdir");
    let text = refusal(
        resolver(m.mirrors(), drive9.path()).resolve(&requirements("demo", "1.0", &h), &target()),
    );
    assert!(
        text.contains("package 'demo' 1.0: archive entry") && text.contains("'..'"),
        "{text}"
    );
    assert!(deps_dirs(drive9.path()).is_empty());
    assert_eq!(tmp_count(drive9.path()), 0);
}

#[test]
fn public_api_works_inside_spawn_blocking() {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let drive9 = tempfile::tempdir().expect("tempdir");
        let root = drive9.path().to_path_buf();
        let got = tokio::task::spawn_blocking(move || {
            let r = Resolver::new(w0_mirrors(), &root, Limits::default());
            let files = SkillFiles::from_dir(&fixtures().join("lockfile-python"))?;
            r.resolve(&files, &Target::from_runtimes(&Default::default()))
        })
        .await
        .expect("join")
        .expect("resolve");
        assert_eq!(got.len(), 1);
        assert!(got[0].tree.join("vamfixture_hello/__init__.py").is_file());
    });
}

#[test]
fn mirror_over_https_is_never_reached_by_file_tests() {
    // The default mirrors are the public ones; this suite never builds them into a resolver
    // that fetches (a plan with no packages does not even create a transport).
    let drive9 = tempfile::tempdir().expect("tempdir");
    let r = Resolver::new(
        Mirrors::parse(None).expect("defaults"),
        drive9.path(),
        Limits::default(),
    );
    assert!(r
        .resolve(&SkillFiles::default(), &target())
        .expect("empty")
        .is_empty());
}
