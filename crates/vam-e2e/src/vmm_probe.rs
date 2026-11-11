//! Reads the host's process table for the Firecracker process (`expect: { vmm: jailed }`).
//! Pure over a `/proc`-shaped directory so it is tested on fake trees.

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

/// What the host's process table says about the Firecracker processes.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No process named `firecracker`.
    NoProcess,
    /// Every Firecracker process is jailed.
    Jailed(usize),
    /// At least one is not; one line per broken property.
    Unjailed(Vec<String>),
    /// A property could not be read (not root, or `/proc` hidden).
    Unreadable(String),
}

/// The namespaces of the observer: `(pid, mnt)` link targets of `/proc/self/ns`.
pub fn own_ns() -> (String, String) {
    let read = |n: &str| {
        fs::read_link(format!("/proc/self/ns/{n}"))
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    };
    (read("pid"), read("mnt"))
}

/// True when the directory behind `root_link` (followed, so it needs root) is a jail
/// root `<under>/<exec>/<id>/root`. The jailer pivots into a new mount namespace, so for
/// a real jailed Firecracker `readlink /proc/<pid>/root` reads `/`; the directory it
/// opens is still the jail's.
fn root_is_a_jail_dir(root_link: &Path, under: &str) -> bool {
    let Ok(theirs) = fs::metadata(root_link) else {
        return false;
    };
    let key = (theirs.dev(), theirs.ino());
    let Ok(execs) = fs::read_dir(under) else {
        return false;
    };
    execs
        .flatten()
        .filter_map(|exec| fs::read_dir(exec.path()).ok())
        .flat_map(|ids| ids.flatten())
        .filter_map(|id| fs::metadata(id.path().join("root")).ok())
        .any(|m| (m.dev(), m.ino()) == key)
}

/// Judges every process under `proc_root` whose `comm` is `firecracker`: its root is under
/// `under`, its real and effective uid are not 0, and its PID and mount namespaces are not
/// the observer's.
pub fn judge(proc_root: &Path, under: &str, own: &(String, String)) -> Verdict {
    judge_seccomp(proc_root, under, own, false)
}

/// The `Seccomp:` value of `/proc/<pid>/status` text (`2` is filter mode).
fn seccomp_of(status: &str) -> Option<&str> {
    status
        .lines()
        .find_map(|l| l.strip_prefix("Seccomp:"))
        .map(str::trim)
}

/// As [`judge`]; with `want_seccomp` the process must also read `Seccomp: 2`. A failure lists
/// the value of every thread, so a lane run shows which threads carry a filter.
pub fn judge_seccomp(
    proc_root: &Path,
    under: &str,
    own: &(String, String),
    want_seccomp: bool,
) -> Verdict {
    let Ok(rd) = fs::read_dir(proc_root) else {
        return Verdict::Unreadable(format!("cannot read {}", proc_root.display()));
    };
    let mut pids: Vec<String> = rd
        .flatten()
        .filter_map(|e| e.file_name().to_str().map(String::from))
        .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
        .filter(|n| {
            fs::read_to_string(proc_root.join(n).join("comm"))
                .map(|c| c.trim() == "firecracker")
                .unwrap_or(false)
        })
        .collect();
    pids.sort();
    if pids.is_empty() {
        return Verdict::NoProcess;
    }
    let mut problems = Vec::new();
    for pid in &pids {
        let dir = proc_root.join(pid);
        match fs::read_link(dir.join("root")) {
            Ok(r) => {
                let r = r.display().to_string();
                let by_link = r.starts_with(&format!("{}/", under.trim_end_matches('/')));
                if !by_link && !root_is_a_jail_dir(&dir.join("root"), under) {
                    problems.push(format!("pid {pid}: root is `{r}`, not under {under}"));
                }
            }
            Err(e) => return Verdict::Unreadable(format!("pid {pid}: root: {e}")),
        }
        match fs::read_to_string(dir.join("status")) {
            Ok(st) => {
                let uids: Vec<&str> = st
                    .lines()
                    .find_map(|l| l.strip_prefix("Uid:"))
                    .map(|v| v.split_whitespace().collect())
                    .unwrap_or_default();
                if uids.len() < 2 || uids[0] == "0" || uids[1] == "0" {
                    problems.push(format!("pid {pid}: uid is {}", uids.join("/")));
                }
                if want_seccomp && seccomp_of(&st) != Some("2") {
                    let mut threads: Vec<String> = fs::read_dir(dir.join("task"))
                        .into_iter()
                        .flatten()
                        .flatten()
                        .filter_map(|t| {
                            let st = fs::read_to_string(t.path().join("status")).ok()?;
                            Some(format!(
                                "{}={}",
                                t.file_name().to_string_lossy(),
                                seccomp_of(&st)?
                            ))
                        })
                        .collect();
                    threads.sort();
                    problems.push(format!(
                        "pid {pid}: Seccomp is {}, not 2 (filter mode); threads: {}",
                        seccomp_of(&st).unwrap_or("absent"),
                        threads.join(" ")
                    ));
                }
            }
            Err(e) => return Verdict::Unreadable(format!("pid {pid}: status: {e}")),
        }
        for (name, mine) in [("pid", &own.0), ("mnt", &own.1)] {
            match fs::read_link(dir.join("ns").join(name)) {
                Ok(t) if t.display().to_string() == *mine => {
                    problems.push(format!("pid {pid}: shares the host {name} namespace"))
                }
                Ok(_) => {}
                Err(e) => return Verdict::Unreadable(format!("pid {pid}: ns/{name}: {e}")),
            }
        }
    }
    if problems.is_empty() {
        Verdict::Jailed(pids.len())
    } else {
        Verdict::Unjailed(problems)
    }
}
