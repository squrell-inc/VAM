//! Runs the real `vamd` binary against the process-guest gate.
//!
//! Built without the `process-guest` feature, vamd must reject every
//! `VAMD_PROCESS_*` variable. Built with it, each refusal must stop vamd with
//! exit code 1 and the refusal message, before it opens any state.

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

#[cfg(not(feature = "process-guest"))]
mod default_build {
    use super::*;

    #[test]
    fn default_vamd_rejects_the_process_variable() {
        let d = tempfile::tempdir().unwrap();
        let (code, err) = run_to_exit(vamd(d.path(), &[("VAMD_PROCESS_GUEST_BIN", "/bin/true")]));
        assert_eq!(code, Some(1), "{err}");
        assert!(err.contains("not compiled into this vamd"), "{err}");
    }

    #[test]
    fn default_vamd_rejects_any_process_prefixed_variable() {
        let d = tempfile::tempdir().unwrap();
        let (code, err) = run_to_exit(vamd(d.path(), &[("VAMD_PROCESS_ANYTHING", "1")]));
        assert_eq!(code, Some(1), "{err}");
    }
}

#[cfg(feature = "process-guest")]
mod process_build {
    use super::*;
    use std::process::Child;

    /// A complete, accepted environment (the guest binary only has to be a file here).
    fn good(dir: &std::path::Path) -> Vec<(String, String)> {
        let scripts = dir.join("scripts");
        std::fs::create_dir_all(&scripts).unwrap();
        vec![
            ("VAMD_PROCESS_GUEST_BIN".into(), "/bin/sh".into()),
            (
                "VAMD_E2E_MOCK_SCRIPTS".into(),
                scripts.display().to_string(),
            ),
            ("VAM_SNAPSHOT_RESTORE".into(), "0".into()),
            (
                "VAM_DRIVE9_ROOT".into(),
                dir.join("drive9").display().to_string(),
            ),
            ("VAMD_BOOTSTRAP_API_KEY".into(), "vam_sk_test_key".into()),
            ("VAMD_SECRETS_MASTER_KEY".into(), "ab".repeat(32)),
        ]
    }

    fn with(dir: &std::path::Path, edit: impl Fn(&mut Vec<(String, String)>)) -> Command {
        let mut env = good(dir);
        edit(&mut env);
        let pairs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
        vamd(dir, &pairs)
    }

    fn set(env: &mut Vec<(String, String)>, k: &str, v: &str) {
        env.retain(|(key, _)| key != k);
        env.push((k.into(), v.into()));
    }

    fn unset(env: &mut Vec<(String, String)>, k: &str) {
        env.retain(|(key, _)| key != k);
    }

    fn refused(edit: impl Fn(&mut Vec<(String, String)>), needle: &str) {
        let d = tempfile::tempdir().unwrap();
        let (code, err) = run_to_exit(with(d.path(), edit));
        assert_eq!(code, Some(1), "{err}");
        assert!(err.contains("process-guest mode refused"), "{err}");
        assert!(err.contains(needle), "wanted {needle:?} in {err}");
    }

    #[test]
    fn refuses_without_mock_scripts() {
        refused(
            |e| unset(e, "VAMD_E2E_MOCK_SCRIPTS"),
            "VAMD_E2E_MOCK_SCRIPTS",
        );
    }

    #[test]
    fn refuses_a_non_loopback_http_listener() {
        refused(|e| set(e, "VAMD_LISTEN", "0.0.0.0:0"), "not a loopback");
    }

    #[test]
    fn refuses_a_non_loopback_telemetry_listener() {
        refused(
            |e| set(e, "VAMD_TELEMETRY_LISTEN", "0.0.0.0:0"),
            "not a loopback",
        );
    }

    #[test]
    fn refuses_secret_import() {
        refused(
            |e| set(e, "VAMD_IMPORT_ENV_SECRETS", "1"),
            "VAMD_IMPORT_ENV_SECRETS",
        );
        refused(
            |e| set(e, "VAMD_IMPORT_ENV_SECRETS", "0"),
            "VAMD_IMPORT_ENV_SECRETS",
        );
    }

    #[test]
    fn refuses_snapshots_on() {
        refused(|e| unset(e, "VAM_SNAPSHOT_RESTORE"), "VAM_SNAPSHOT_RESTORE");
    }

    #[test]
    fn refuses_a_bad_guest_binary() {
        refused(
            |e| set(e, "VAMD_PROCESS_GUEST_BIN", "relative/vam-runtime"),
            "absolute",
        );
        refused(
            |e| set(e, "VAMD_PROCESS_GUEST_BIN", "/nonexistent/x"),
            "not a file",
        );
    }

    #[test]
    fn refuses_an_unknown_process_variable() {
        refused(|e| set(e, "VAMD_PROCESS_TYPO", "1"), "VAMD_PROCESS_TYPO");
    }

    fn wait_for_node(port: u16) -> serde_json::Value {
        let client = reqwest::blocking::Client::new();
        let t = Instant::now();
        loop {
            if let Ok(r) = client
                .get(format!("http://127.0.0.1:{port}/v1/node"))
                .header("Authorization", "Bearer vam_sk_test_key")
                .send()
            {
                if r.status().is_success() {
                    return r.json().unwrap();
                }
            }
            assert!(
                t.elapsed() < Duration::from_secs(30),
                "vamd never served /v1/node"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    fn stop(mut child: Child) -> String {
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .status();
        let t = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if t.elapsed() > Duration::from_secs(10) {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let mut out = String::new();
        let mut err = String::new();
        if let Some(mut s) = child.stdout.take() {
            let _ = s.read_to_string(&mut out);
        }
        if let Some(mut s) = child.stderr.take() {
            let _ = s.read_to_string(&mut err);
        }
        format!("{out}\n{err}")
    }

    #[test]
    fn a_good_node_warns_and_reports_guest_mode_process() {
        let d = tempfile::tempdir().unwrap();
        let port = free_port();
        let tport = free_port();
        let child = with(d.path(), |e| {
            set(e, "VAMD_LISTEN", &format!("127.0.0.1:{port}"));
            set(e, "VAMD_TELEMETRY_LISTEN", &format!("127.0.0.1:{tport}"));
            // This test reads the node over plain HTTP; TLS is the default (runner scenarios cover it).
            set(e, "VAMD_INSECURE_DEV", "1");
        })
        .spawn()
        .unwrap();
        let node = wait_for_node(port);
        let logs = stop(child);
        assert_eq!(node["status"]["guestMode"], "process", "{node}");
        // proto3 JSON leaves a false bool out: TLS off reads as absent, never `true`.
        assert_ne!(node["status"]["isolation"]["apiTls"], true, "{node}");
        assert!(logs.contains("INSECURE DEV MODE"), "{logs}");
        assert!(
            logs.contains("PROCESS GUEST MODE"),
            "banner missing in {logs}"
        );
        assert!(logs.contains("NO isolation"), "{logs}");
    }
}
