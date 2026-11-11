//! The steps that need a world which counts: `kill-guest` (SIGKILL the task's guest, a crash and
//! never a stop) and `expect: { ledger }` (what the effects test broker did, read from the
//! broker's own ledger through its `ledger` subcommand).
//!
//! This module is a child of `engine`; the shared runner files only register the step and the
//! expectation.

use super::{Engine, Fail, Probe};
use crate::scenario::Class;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// `kill-guest: {}`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KillGuest {}

/// `ledger: { piece, tool, title?, count }`: the broker of the `piece` has exactly `count` ledger
/// lines for the tool (and the title, when given).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LedgerExpect {
    pub piece: String,
    pub tool: String,
    pub title: Option<String>,
    pub count: u64,
}

/// `(pid, starttime)` lines of the pid files under `root`, at most 5 levels deep.
fn live_guests(root: &Path, out: &mut Vec<u32>, depth: usize) {
    if depth > 5 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(root) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            live_guests(&p, out, depth + 1);
        } else if p.file_name().is_some_and(|n| n == "pid") {
            let raw = std::fs::read_to_string(&p).unwrap_or_default();
            let mut it = raw.split_whitespace();
            let (Some(pid), Some(start)) = (
                it.next().and_then(|v| v.parse::<u32>().ok()),
                it.next().and_then(|v| v.parse::<u64>().ok()),
            ) else {
                continue;
            };
            if started_at(pid) == Some(start) {
                out.push(pid);
            }
        }
    }
}

/// Live Firecracker processes under `proc_root` (comm `firecracker`). When some command line
/// names `session` (the jail id), only those; else all of them (one run is on the node at a
/// time, so the VM that is up is the run's).
pub fn vmm_pids(proc_root: &Path, session: &str) -> Vec<u32> {
    let mut all: Vec<(u32, bool)> = Vec::new();
    let Ok(rd) = std::fs::read_dir(proc_root) else {
        return vec![];
    };
    for e in rd.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else {
            continue;
        };
        let comm = std::fs::read_to_string(e.path().join("comm")).unwrap_or_default();
        if comm.trim() != "firecracker" {
            continue;
        }
        let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let named = !session.is_empty() && String::from_utf8_lossy(&cmd).contains(session);
        all.push((pid, named));
    }
    let mut out: Vec<u32> = if all.iter().any(|(_, n)| *n) {
        all.iter().filter(|(_, n)| *n).map(|(p, _)| *p).collect()
    } else {
        all.iter().map(|(p, _)| *p).collect()
    };
    out.sort_unstable();
    out
}

/// Field 22 of `/proc/<pid>/stat` for a live, non-zombie process.
fn started_at(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    let mut f = rest.split_whitespace();
    if f.next()? == "Z" {
        return None;
    }
    f.nth(18)?.parse().ok()
}

fn product(want: impl Into<String>, seen: impl Into<String>) -> Fail {
    Fail::new(Class::Product, want, seen)
}

impl Engine {
    /// Kills the guest of the latest run with SIGKILL and waits until it is gone. The run is
    /// never stopped: vamd sees a crash.
    pub(super) fn kill_guest(&mut self, _k: &KillGuest) -> Result<(), Fail> {
        let want = "the task's guest is killed";
        let session = self.ctx.runs[self.last_run()?].session_id.clone();
        let mut roots: Vec<PathBuf> = vec![PathBuf::from("/tmp/vam-process-guest")];
        if let Some(s) = self.vamd.state_dir() {
            roots.push(s);
        }
        let mut pids = Vec::new();
        for r in &roots {
            live_guests(r, &mut pids, 0);
        }
        pids.sort_unstable();
        pids.dedup();
        if pids.is_empty() {
            // The microVM lane: the guest is the VMM. Kill the Firecracker of the run's session
            // (its jail id or command line names the session), else every live Firecracker.
            pids = vmm_pids(Path::new("/proc"), &session);
        }
        if pids.is_empty() {
            return Err(Fail::new(
                Class::Env,
                want,
                "no live process guest has a pid file (this step needs a process guest or a Firecracker VM)",
            ));
        }
        for pid in &pids {
            let _ = std::process::Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
        let start = Instant::now();
        while pids.iter().any(|p| started_at(*p).is_some()) {
            if start.elapsed() > Duration::from_secs(5) {
                return Err(Fail::new(Class::Env, want, "the guest is still alive"));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(())
    }

    /// Reads the broker's ledger through the broker binary: `(tool, title, count)` rows.
    fn read_ledger(&self, piece: &str) -> Result<Vec<(String, String, u64)>, Fail> {
        let want = format!("the ledger of `{piece}` is read");
        let dir = self
            .ctx
            .trig
            .brokers
            .iter()
            .find(|b| b.id == piece)
            .map(|b| b.dir.clone())
            .ok_or_else(|| {
                Fail::new(
                    Class::Scenario,
                    format!("a `{piece}` broker piece in `given`"),
                    "none",
                )
            })?;
        let bin = std::env::var("VAM_E2E_TEST_BROKER")
            .ok()
            .filter(|b| !b.is_empty())
            .ok_or_else(|| Fail::new(Class::Env, &want, "VAM_E2E_TEST_BROKER is not set"))?;
        let out = std::process::Command::new(&bin)
            .args(["ledger", "--state-dir"])
            .arg(&dir)
            .output()
            .map_err(|e| Fail::new(Class::Env, &want, format!("{bin}: {e}")))?;
        if !out.status.success() {
            return Err(Fail::new(
                Class::Env,
                want,
                String::from_utf8_lossy(&out.stderr).into_owned(),
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter_map(|l| serde_json::from_str::<serde_json::Value>(l).ok())
            .map(|v| {
                (
                    v["tool"].as_str().unwrap_or_default().to_string(),
                    v["title"].as_str().unwrap_or_default().to_string(),
                    v["count"].as_u64().unwrap_or(0),
                )
            })
            .collect())
    }

    /// Every ledger line of every broker piece of the scenario, `(tool, title, count)`; a
    /// broker without a ledger is skipped.
    pub(crate) fn world_ledger(&self) -> Vec<(String, String, u64)> {
        let ids: Vec<String> = self.ctx.trig.brokers.iter().map(|b| b.id.clone()).collect();
        ids.iter()
            .filter_map(|id| self.read_ledger(id).ok())
            .flatten()
            .collect()
    }

    /// `expect: { ledger }`: waits (within the step budget) for the count to be exactly `count`.
    pub(super) fn expect_ledger(&mut self, x: &LedgerExpect) -> Result<(), Fail> {
        let budget = self.cfg.step_budget.min(Duration::from_secs(20));
        let mut seen = 0u64;
        let r = self.wait_for(budget, None, |e| {
            let rows = e.read_ledger(&x.piece)?;
            seen = rows
                .iter()
                .filter(|(t, title, _)| {
                    *t == x.tool && x.title.as_deref().is_none_or(|w| w == title)
                })
                .map(|(_, _, c)| *c)
                .sum();
            Ok(if seen == x.count {
                Probe::Met
            } else {
                Probe::Pending
            })
        });
        match r {
            Ok(()) => Ok(()),
            Err(super::WaitEnd::Fail(f)) => Err(f),
            Err(_) => Err(product(
                format!(
                    "the broker's ledger holds {} `{}` line(s){}",
                    x.count,
                    x.tool,
                    x.title
                        .as_ref()
                        .map(|t| format!(" titled `{t}`"))
                        .unwrap_or_default()
                ),
                format!("{seen} line(s)"),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pid_file_of_a_live_process_is_found_and_a_stale_one_is_not() {
        let me = std::process::id();
        let start = started_at(me).expect("this process has a start time");
        let dir = std::env::temp_dir().join(format!("effects-pid-{me}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("vm-a")).unwrap();
        std::fs::create_dir_all(dir.join("vm-b")).unwrap();
        std::fs::write(dir.join("vm-a/pid"), format!("{me} {start}\n")).unwrap();
        // The same pid with another start time is a reused pid, not the guest.
        std::fs::write(dir.join("vm-b/pid"), format!("{me} {}\n", start + 1)).unwrap();
        let mut found = Vec::new();
        live_guests(&dir, &mut found, 0);
        assert_eq!(found, vec![me]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_vmm_of_the_session_is_found_else_every_firecracker() {
        let dir = std::env::temp_dir().join(format!("effects-vmm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        for (pid, comm, cmd) in [
            ("101", "firecracker", "firecracker\0--id\0sess-a\0"),
            ("102", "firecracker", "firecracker\0--id\0sess-b\0"),
            ("103", "bash", "bash\0sess-a\0"),
        ] {
            std::fs::create_dir_all(dir.join(pid)).unwrap();
            std::fs::write(dir.join(pid).join("comm"), format!("{comm}\n")).unwrap();
            std::fs::write(dir.join(pid).join("cmdline"), cmd).unwrap();
        }
        assert_eq!(vmm_pids(&dir, "sess-b"), vec![102]);
        assert_eq!(vmm_pids(&dir, "other"), vec![101, 102]);
        assert!(vmm_pids(&dir.join("none"), "x").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_step_and_the_expectation_parse() {
        let s: crate::scenario::Step = serde_yaml::from_str("kill-guest: {}").unwrap();
        assert!(matches!(s, crate::scenario::Step::KillGuest(_)));
        let s: crate::scenario::Step = serde_yaml::from_str(
            "expect: { ledger: { piece: effects-test-broker, tool: echo_create_ticket, count: 1 } }",
        )
        .unwrap();
        match s {
            crate::scenario::Step::Expect(e) => {
                assert_eq!(e.families(), 1);
                assert_eq!(e.ledger.unwrap().count, 1);
            }
            _ => panic!("not an expect"),
        }
    }
}
