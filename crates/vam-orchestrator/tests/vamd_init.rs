//! Runs the real `vamd` for `--init` and `--preflight` (workstream `machine-base`, MB-1.7).
//!
//! Security floor: the first admin key is printed to stdout once and never reaches stderr
//! (tracing included, at TRACE), argv or any file under the state dir.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

fn vamd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vamd"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("RUST_LOG", "trace")
        .env("VAMD_DB_PATH", dir.join("state/vamd.db"))
        .env("VAMD_SPOOL_DIR", dir.join("spool"))
        .env("VAMD_OTLP_ENABLED", "0")
        .env("VAMD_LISTEN", "127.0.0.1:0")
        .env("VAMD_TELEMETRY_LISTEN", "127.0.0.1:0")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("run vamd")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn files_under(dir: &Path, out: &mut Vec<PathBuf>) {
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                files_under(&p, out);
            } else {
                out.push(p);
            }
        }
    }
}

fn secret_in(out: &Output) -> String {
    text(&out.stdout)
        .lines()
        .find_map(|l| l.strip_prefix("admin key: "))
        .unwrap_or_default()
        .to_string()
}

#[test]
fn init_shows_the_key_once_and_it_reaches_nothing_else() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();

    let first = run(&mut vamd(dir, &["--init"]));
    let (o1, e1) = (text(&first.stdout), text(&first.stderr));
    assert_eq!(first.status.code(), Some(0), "stderr: {e1}");
    assert_eq!(o1.matches("admin key: ").count(), 1, "{o1}");
    assert!(o1.contains("save it now; it is not shown again"), "{o1}");
    let secret = secret_in(&first);
    assert!(secret.starts_with("vam_sk_"), "no key in {o1}");

    let second = run(&mut vamd(dir, &["--init"]));
    let (o2, e2) = (text(&second.stdout), text(&second.stderr));
    assert_eq!(second.status.code(), Some(0), "stderr: {e2}");
    assert!(!o2.contains("admin key: "), "{o2}");
    assert!(
        o2.contains("this node already has an admin key; none shown"),
        "{o2}"
    );

    // The secret is on neither run's stderr, nor in the second's stdout, nor in a file.
    for (what, s) in [("stderr 1", &e1), ("stderr 2", &e2), ("stdout 2", &o2)] {
        assert!(!s.contains(&secret), "the secret is in {what}: {s}");
    }
    let mut files = Vec::new();
    files_under(dir, &mut files);
    assert!(files.iter().any(|f| f.ends_with("vamd.db")), "{files:?}");
    for f in files {
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "the secret is in {}",
            f.display()
        );
    }
}

#[test]
fn init_creates_the_state_dir_private() {
    use std::os::unix::fs::PermissionsExt;
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = run(&mut vamd(tmp.path(), &["--init"]));
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let mode = std::fs::metadata(tmp.path().join("state"))
        .expect("state dir")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}

#[test]
fn an_operator_key_is_stored_hashed_and_not_logged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let secret = "operator-chosen-secret-abcdef0123456789";
    let mut cmd = vamd(tmp.path(), &["--init"]);
    cmd.env("VAMD_BOOTSTRAP_API_KEY", secret);
    let out = run(&mut cmd);
    assert_eq!(out.status.code(), Some(0));
    assert!(!text(&out.stderr).contains(secret), "{}", text(&out.stderr));
    let mut files = Vec::new();
    files_under(tmp.path(), &mut files);
    for f in files {
        let bytes = std::fs::read(&f).unwrap_or_default();
        assert!(
            !bytes.windows(secret.len()).any(|w| w == secret.as_bytes()),
            "the secret is in {}",
            f.display()
        );
    }
}

#[test]
fn root_is_refused_on_every_path_but_preflight() {
    let tmp = tempfile::tempdir().expect("tempdir");
    for args in [
        vec!["--root", "/x"],
        vec!["--init", "--root", "/x"],
        vec!["--listen", "127.0.0.1:0", "--root", "/x"],
    ] {
        let out = run(&mut vamd(tmp.path(), &args));
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(
            text(&out.stderr).contains("--root is only valid with --preflight"),
            "{}",
            text(&out.stderr)
        );
    }
    // Nothing was created: refusal comes before any state is opened.
    assert!(!tmp.path().join("state").exists());
}

#[test]
fn preflight_prints_json_and_changes_nothing() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path().join("root");
    std::fs::create_dir_all(&root).expect("root");
    let out = run(&mut vamd(
        tmp.path(),
        &["--preflight", "--root", &root.to_string_lossy()],
    ));
    // The variables a microVM node needs are not set, so it fails; the JSON is the answer.
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json");
    let checks = v["checks"].as_array().expect("checks");
    let names: Vec<&str> = checks.iter().filter_map(|c| c["name"].as_str()).collect();
    assert!(names.contains(&"vamd-env"), "{names:?}");
    assert!(names.contains(&"jailer-rules"), "{names:?}");
    assert!(!text(&out.stdout).contains("no-jailer"));
    assert!(!tmp.path().join("state").exists(), "preflight wrote state");
}
