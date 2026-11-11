//! Runs the real `vamd` binary against the TLS gate: a bad TLS setup stops vamd
//! (exit 1, a message naming the problem) before any state is opened. It never
//! falls back to plain HTTP.

use std::io::Read;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn vamd(dir: &std::path::Path, envs: &[(&str, &str)]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vamd"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("VAMD_DB_PATH", dir.join("state/vamd.db"))
        .env("VAMD_SPOOL_DIR", dir.join("spool"))
        .env("VAMD_OTLP_ENABLED", "0")
        .env("VAMD_LISTEN", "127.0.0.1:0")
        .env("VAMD_TELEMETRY_LISTEN", "127.0.0.1:0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd
}

/// Runs vamd to its exit (it must exit by itself, quickly) and returns (code, stderr).
fn run_to_exit(mut cmd: Command) -> (Option<i32>, String) {
    let mut child = cmd.spawn().expect("spawn vamd");
    let t = Instant::now();
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            let mut err = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut err)
                .unwrap();
            return (status.code(), err);
        }
        if t.elapsed() > Duration::from_secs(20) {
            let _ = child.kill();
            panic!("vamd did not exit; it started instead of refusing");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn refused(envs: &[(&str, &str)], args: &[&str], expect: &str) {
    let d = tempfile::tempdir().unwrap();
    let mut cmd = vamd(d.path(), envs);
    cmd.args(args);
    let (code, err) = run_to_exit(cmd);
    assert_eq!(code, Some(1), "{err}");
    assert!(err.contains(expect), "wanted '{expect}' in: {err}");
    assert!(
        !d.path().join("state/vamd.db").exists(),
        "vamd opened its state before refusing"
    );
}

#[test]
fn a_certificate_without_a_key_stops_vamd() {
    refused(
        &[("VAMD_TLS_CERT", "/nonexistent/cert.pem")],
        &[],
        "must be set together",
    );
}

#[test]
fn a_key_without_a_certificate_stops_vamd() {
    refused(
        &[("VAMD_TLS_KEY", "/nonexistent/key.pem")],
        &[],
        "must be set together",
    );
}

#[test]
fn an_unreadable_pair_stops_vamd() {
    refused(
        &[
            ("VAMD_TLS_CERT", "/nonexistent/cert.pem"),
            ("VAMD_TLS_KEY", "/nonexistent/key.pem"),
        ],
        &[],
        "cannot read",
    );
}

#[test]
fn insecure_dev_with_a_pair_stops_vamd() {
    refused(
        &[
            ("VAMD_TLS_CERT", "/nonexistent/cert.pem"),
            ("VAMD_TLS_KEY", "/nonexistent/key.pem"),
        ],
        &["--insecure-dev"],
        "unset one side",
    );
}

#[test]
fn a_bad_extra_name_stops_vamd() {
    refused(&[("VAMD_TLS_SANS", "not a name")], &[], "VAMD_TLS_SANS");
}
