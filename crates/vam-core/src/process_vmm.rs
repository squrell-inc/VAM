//! `ProcessVmm` runs the guest agent (`vam-runtime`, built with its
//! `process-guest` feature) as an ordinary host child process instead of a
//! Firecracker microVM. TEST HARNESS ONLY: there is NO isolation. The guest
//! shares the host kernel, filesystem, network and pid namespace.
//!
//! Compiled only with the non-default cargo feature `process-guest`; vamd
//! refuses to use it unless it is also a mock-model-only, loopback-only node
//! (see `vam-orchestrator/src/process_mode.rs`).
//!
//! Layout:
//! - vsock: the guest gets `VAM_GUEST_VSOCK_UDS=/tmp/vam-vsock-<id>.socket`, the
//!   base the orchestrator binds its `<base>_<port>` listeners on.
//! - Drive9: the guest works in the session's `materialized/` directory (the
//!   storage "directory backend"; there is no image file).
//! - State (`pid` file, `guest.log`) lives in `<state_root>/<id>/`.

use crate::config::MicroVmConfig;
use crate::VamCoreError;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
    fn geteuid() -> u32;
}

const SIGTERM: i32 = 15;
const SIGKILL: i32 = 9;
const SIGSTOP: i32 = 19;
const SIGCONT: i32 = 18;
const TERM_GRACE: Duration = Duration::from_secs(3);

/// Tool subprocesses drop to this uid:gid when the runtime runs as root, so the
/// writable Drive9 dirs belong to it (same as the VM guest init's chown).
const GUEST_UID: u32 = 1000;
const GUEST_GID: u32 = 100;

fn chown_tree(path: &Path, uid: u32, gid: u32) {
    let _ = std::os::unix::fs::lchown(path, Some(uid), Some(gid));
    if let Ok(entries) = fs::read_dir(path) {
        for entry in entries.flatten() {
            let p = entry.path();
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                chown_tree(&p, uid, gid);
            } else {
                let _ = std::os::unix::fs::lchown(&p, Some(uid), Some(gid));
            }
        }
    }
}

/// Field 22 (starttime) of `/proc/<pid>/stat`, plus whether the process is a zombie.
fn proc_stat(pid: u32) -> Option<(u64, bool)> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = &stat[stat.rfind(')')? + 1..];
    let fields: Vec<&str> = rest.split_whitespace().collect();
    let zombie = fields.first().map(|s| *s == "Z").unwrap_or(false);
    let start = fields.get(19)?.parse::<u64>().ok()?;
    Some((start, zombie))
}

fn alive(pid: u32, start: u64) -> bool {
    matches!(proc_stat(pid), Some((st, false)) if st == start)
}

/// Sends `sig` to the whole process group led by `pgid`. Refuses 0 and 1.
fn signal_group(pgid: u32, sig: i32) {
    if pgid <= 1 || pgid > i32::MAX as u32 {
        return;
    }
    unsafe { kill(-(pgid as i32), sig) };
}

fn unsupported(what: &str) -> VamCoreError {
    VamCoreError::InvalidConfig(format!("ProcessVmm does not support {what}"))
}

/// Host-side file names for a guest id. One place so spawn and sweep agree.
pub fn vsock_base(id: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/vam-vsock-{id}.socket"))
}

fn egress_sock(id: &str) -> PathBuf {
    PathBuf::from(format!("/tmp/vam-egress-{id}.sock"))
}

fn state_dir_of(state_root: &Path, id: &str) -> PathBuf {
    state_root.join(id)
}

/// The recorded `(pid, starttime)` of a guest, if the pid file is valid and the
/// process with that start time is alive.
fn live_recorded(state_root: &Path, id: &str) -> Option<(u32, u64)> {
    let raw = fs::read_to_string(state_dir_of(state_root, id).join("pid")).ok()?;
    let mut it = raw.split_whitespace();
    let pid = it.next()?.parse::<u32>().ok()?;
    let start = it.next()?.parse::<u64>().ok()?;
    alive(pid, start).then_some((pid, start))
}

/// Removes everything a guest leaves behind, exact names only: the state dir
/// (pid file, log), the base vsock socket, every `<base>_<port>` socket and the
/// egress socket. Safe to call when the guest already exited, and twice.
pub fn sweep_leftovers(state_root: &Path, id: &str) {
    let _ = fs::remove_dir_all(state_dir_of(state_root, id));
    let base = vsock_base(id);
    let _ = fs::remove_file(&base);
    let _ = fs::remove_file(egress_sock(id));
    if let (Some(dir), Some(name)) = (base.parent(), base.file_name()) {
        let prefix = format!("{}_", name.to_string_lossy());
        if let Ok(entries) = fs::read_dir(dir) {
            for e in entries.flatten() {
                let n = e.file_name().to_string_lossy().into_owned();
                if n.strip_prefix(&prefix)
                    .map(|port| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
                    .unwrap_or(false)
                {
                    let _ = fs::remove_file(e.path());
                }
            }
        }
    }
}

/// Kills the live guest recorded for `id` (if any) and sweeps its files. Used
/// by vamd at startup for every session row it knows.
pub fn kill_and_sweep(state_root: &Path, id: &str) {
    if let Some((pid, start)) = live_recorded(state_root, id) {
        terminate(pid, start, None);
    }
    sweep_leftovers(state_root, id);
}

/// SIGTERM the group, wait up to the grace period, then SIGKILL the group.
fn terminate(pid: u32, start: u64, mut child: Option<&mut Child>) {
    if !alive(pid, start) {
        return;
    }
    signal_group(pid, SIGTERM);
    let t = Instant::now();
    while alive(pid, start) && t.elapsed() < TERM_GRACE {
        if let Some(c) = child.as_deref_mut() {
            let _ = c.try_wait();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if alive(pid, start) {
        signal_group(pid, SIGKILL);
        let t = Instant::now();
        while alive(pid, start) && t.elapsed() < Duration::from_secs(2) {
            if let Some(c) = child.as_deref_mut() {
                let _ = c.try_wait();
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

pub struct ProcessVmm {
    id: String,
    guest_bin: PathBuf,
    guest_cid: u32,
    state_root: PathBuf,
    drive9_dir: Option<PathBuf>,
    child: Option<Child>,
    /// `(pid, starttime)` of the guest this handle controls.
    proc: Option<(u32, u64)>,
}

impl ProcessVmm {
    pub fn new(id: String, guest_bin: PathBuf, guest_cid: u32, state_root: PathBuf) -> Self {
        Self {
            id,
            guest_bin,
            guest_cid,
            state_root,
            drive9_dir: None,
            child: None,
            proc: None,
        }
    }

    pub fn state_dir(&self) -> PathBuf {
        state_dir_of(&self.state_root, &self.id)
    }

    pub fn vsock_uds_path(&self) -> PathBuf {
        vsock_base(&self.id)
    }

    pub fn guest_log_path(&self) -> PathBuf {
        self.state_dir().join("guest.log")
    }

    fn signal(&self, sig: i32) {
        if let Some((pid, start)) = self.proc {
            if alive(pid, start) {
                signal_group(pid, sig);
            }
        }
    }
}

impl crate::Vmm for ProcessVmm {
    fn spawn(&mut self) -> Result<(), VamCoreError> {
        if !self.guest_bin.is_absolute() || !self.guest_bin.is_file() {
            return Err(VamCoreError::InvalidConfig(format!(
                "ProcessVmm guest binary must be an absolute path to a file: {}",
                self.guest_bin.display()
            )));
        }
        fs::create_dir_all(self.state_dir())?;
        Ok(())
    }

    fn configure_hardware(
        &mut self,
        config: &MicroVmConfig,
        _timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<(), VamCoreError> {
        let image = config
            .data_drives
            .first()
            .map(|d| d.path.clone())
            .ok_or_else(|| VamCoreError::InvalidConfig("ProcessVmm needs a data drive".into()))?;
        let session_dir = image.parent().ok_or_else(|| {
            VamCoreError::InvalidConfig(format!("data drive has no parent: {}", image.display()))
        })?;
        let dir = session_dir.join("materialized");
        if !dir.is_dir() {
            return Err(VamCoreError::InvalidConfig(format!(
                "ProcessVmm: no materialized/ next to {}",
                image.display()
            )));
        }
        // The directory backend never writes an image. One here means the
        // ext4 backend ran: the guest would work on a directory commit ignores.
        if image.exists() {
            return Err(VamCoreError::InvalidConfig(format!(
                "ProcessVmm needs the Drive9 directory backend, but {} exists",
                image.display()
            )));
        }
        self.guest_cid = config.guest_cid;
        self.drive9_dir = Some(dir);
        Ok(())
    }

    fn start_instance(
        &mut self,
        _timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<(), VamCoreError> {
        let dir = self
            .drive9_dir
            .clone()
            .ok_or_else(|| VamCoreError::InvalidConfig("configure_hardware first".into()))?;
        for sub in ["workspace", "skills", "tools", "checkpoints"] {
            fs::create_dir_all(dir.join(sub))?;
        }
        if unsafe { geteuid() } == 0 {
            let _ = std::os::unix::fs::lchown(&dir, Some(GUEST_UID), Some(GUEST_GID));
            for sub in ["workspace", "tools", "checkpoints"] {
                chown_tree(&dir.join(sub), GUEST_UID, GUEST_GID);
            }
        }
        fs::create_dir_all(self.state_dir())?;
        let log = fs::File::create(self.guest_log_path())?;
        let log_err = log.try_clone()?;
        let path = std::env::var("PATH")
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| "/usr/local/bin:/usr/bin:/bin".to_string());

        let mut cmd = Command::new(&self.guest_bin);
        cmd.env_clear()
            .env("PATH", path)
            .env("HOME", self.state_dir())
            .env("TMPDIR", "/tmp")
            .env("RUST_BACKTRACE", "1")
            .env("VAM_GUEST_VSOCK_UDS", self.vsock_uds_path())
            .env("VAM_INSTANCE_ID", &self.id)
            .env("VAM_GUEST_CID", self.guest_cid.to_string())
            .env("VAM_WORKSPACE_ROOT", dir.join("workspace"))
            .env("VAM_SKILLS_MOUNT", dir.join("skills"))
            .env("VAM_EGRESS_PROXY_SOCK", egress_sock(&self.id))
            .env("VAM_EGRESS_PROXY_ADDR", "127.0.0.1:0")
            .current_dir(dir.join("workspace"))
            .stdin(Stdio::null())
            .stdout(Stdio::from(log))
            .stderr(Stdio::from(log_err));
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let child = cmd.spawn()?;
        let pid = child.id();
        let start = proc_stat(pid).map(|(s, _)| s).unwrap_or(0);
        if let Err(e) = fs::write(self.state_dir().join("pid"), format!("{pid} {start}")) {
            let mut child = child;
            let _ = child.kill();
            let _ = child.wait();
            return Err(e.into());
        }
        self.proc = Some((pid, start));
        self.child = Some(child);
        Ok(())
    }

    fn pause(&mut self) -> Result<(), VamCoreError> {
        self.signal(SIGSTOP);
        Ok(())
    }

    fn resume(&mut self) -> Result<(), VamCoreError> {
        self.signal(SIGCONT);
        Ok(())
    }

    fn create_snapshot(&mut self, _: &Path, _: &Path) -> Result<(), VamCoreError> {
        Err(unsupported("snapshots"))
    }

    fn load_snapshot(
        &mut self,
        _: &Path,
        _: &Path,
        _: bool,
        _: Option<&Path>,
    ) -> Result<(), VamCoreError> {
        Err(unsupported("snapshots"))
    }

    fn patch_data_drive_path(&mut self, _: &str, _: &Path) -> Result<(), VamCoreError> {
        Err(unsupported("drive patching"))
    }

    fn patch_vsock_uds_path(&mut self, _: &Path) -> Result<(), VamCoreError> {
        Err(unsupported("vsock patching"))
    }

    fn shutdown(&mut self) -> Result<(), VamCoreError> {
        let target = self
            .proc
            .filter(|(p, s)| alive(*p, *s))
            .or_else(|| live_recorded(&self.state_root, &self.id));
        if let Some((pid, start)) = target {
            terminate(pid, start, self.child.as_mut());
        }
        if let Some(mut c) = self.child.take() {
            let _ = c.wait();
        }
        self.proc = None;
        sweep_leftovers(&self.state_root, &self.id);
        Ok(())
    }

    fn get_pid(&self) -> Option<u32> {
        self.proc.map(|(p, _)| p)
    }

    fn detach(&mut self) {
        // Keep the guest running; reap it on a thread so it never lingers as a zombie.
        if let Some(mut child) = self.child.take() {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }

    fn cleanup_after_exit(&mut self) {
        sweep_leftovers(&self.state_root, &self.id);
    }

    /// Controls the recorded guest only if `pid` is the one in the pid file
    /// and the same process (start time) is still alive; otherwise it controls
    /// nothing, so a reused pid is never signalled.
    fn adopt(&mut self, pid: u32) {
        self.proc = live_recorded(&self.state_root, &self.id).filter(|(p, _)| *p == pid);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{DriveAttachment, MicroVmConfig};
    use crate::Vmm;
    use std::os::unix::fs::PermissionsExt;

    struct Fixture {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        session: PathBuf,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("state");
        let session = tmp.path().join("session");
        fs::create_dir_all(session.join("materialized")).unwrap();
        Fixture {
            root,
            session,
            _tmp: tmp,
        }
    }

    fn script(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("guest.sh");
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn cfg(session: &Path) -> MicroVmConfig {
        MicroVmConfig {
            kernel_path: "/k".into(),
            boot_args: String::new(),
            rootfs_path: "/r".into(),
            vcpu_count: 1,
            mem_size_mib: 64,
            track_dirty_pages: false,
            guest_cid: 41,
            limit_iops: None,
            data_drives: vec![DriveAttachment {
                path: session.join("drive9.ext4"),
                is_read_only: false,
                cache_type: None,
            }],
            log_path: None,
            metrics_path: None,
        }
    }

    fn started(f: &Fixture, id: &str, body: &str) -> ProcessVmm {
        let bin = script(f._tmp.path(), body);
        let mut v = ProcessVmm::new(id.into(), bin, 41, f.root.clone());
        v.spawn().unwrap();
        v.configure_hardware(&cfg(&f.session), None).unwrap();
        v.start_instance(None).unwrap();
        v
    }

    fn wait_for(path: &Path) {
        for _ in 0..200 {
            if path.exists() {
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        panic!("{} never appeared", path.display());
    }

    #[test]
    fn env_is_exactly_the_allow_list() {
        std::env::set_var("VAM_TEST_SECRET_LEAK", "leak");
        let f = fixture();
        let out = f._tmp.path().join("env.txt");
        let mut v = started(
            &f,
            "t-env",
            &format!("env | sort > {0}.tmp; mv {0}.tmp {0}", out.display()),
        );
        wait_for(&out);
        let env = fs::read_to_string(&out).unwrap();
        assert!(!env.contains("VAM_TEST_SECRET_LEAK"), "{env}");
        let keys: Vec<&str> = env
            .lines()
            .filter_map(|l| l.split('=').next())
            .filter(|k| *k != "PWD" && *k != "SHLVL" && *k != "_" && *k != "OLDPWD")
            .collect();
        let mut want = vec![
            "HOME",
            "PATH",
            "RUST_BACKTRACE",
            "TMPDIR",
            "VAM_EGRESS_PROXY_ADDR",
            "VAM_EGRESS_PROXY_SOCK",
            "VAM_GUEST_CID",
            "VAM_GUEST_VSOCK_UDS",
            "VAM_INSTANCE_ID",
            "VAM_SKILLS_MOUNT",
            "VAM_WORKSPACE_ROOT",
        ];
        let mut got = keys.clone();
        got.sort();
        want.sort();
        assert_eq!(got, want, "{env}");
        assert!(env.contains("VAM_GUEST_VSOCK_UDS=/tmp/vam-vsock-t-env.socket"));
        assert!(env.contains("VAM_INSTANCE_ID=t-env"));
        assert!(env.contains("VAM_GUEST_CID=41"));
        assert!(env.contains("/materialized/workspace"));
        assert!(env.contains("/materialized/skills"));
        v.shutdown().unwrap();
    }

    #[test]
    fn shutdown_kills_the_group_and_sweeps() {
        let f = fixture();
        let marker = f._tmp.path().join("child.pid");
        let mut v = started(
            &f,
            "t-grp",
            &format!("sleep 300 & echo $! > {}; wait", marker.display()),
        );
        wait_for(&marker);
        let child: u32 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
        fs::write(vsock_base("t-grp"), b"").unwrap();
        fs::write(format!("{}_52", vsock_base("t-grp").display()), b"").unwrap();
        let guest = v.get_pid().unwrap();
        v.shutdown().unwrap();
        assert!(proc_stat(guest).is_none());
        // the sleep child was in the guest's group: gone (or an unreaped zombie of init)
        assert!(!matches!(proc_stat(child), Some((_, false))));
        assert!(!vsock_base("t-grp").exists());
        assert!(!Path::new(&format!("{}_52", vsock_base("t-grp").display())).exists());
        assert!(!v.state_dir().exists());
        v.shutdown().unwrap(); // idempotent
    }

    #[test]
    fn sigkill_after_the_grace_period() {
        let f = fixture();
        let ready = f._tmp.path().join("ready");
        let mut v = started(
            &f,
            "t-kill",
            &format!(
                "trap '' TERM; touch {}; while :; do sleep 1; done",
                ready.display()
            ),
        );
        wait_for(&ready);
        let guest = v.get_pid().unwrap();
        let t = Instant::now();
        v.shutdown().unwrap();
        assert!(t.elapsed() >= Duration::from_secs(2), "TERM was ignored");
        assert!(proc_stat(guest).is_none());
    }

    #[test]
    fn sweep_after_the_guest_already_exited() {
        let f = fixture();
        let mut v = started(&f, "t-gone", "exit 0");
        let guest = v.get_pid().unwrap();
        for _ in 0..200 {
            if !matches!(proc_stat(guest), Some((_, false))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        fs::write(vsock_base("t-gone"), b"").unwrap(); // a crashed guest leaves its base
        fs::write(format!("{}_53", vsock_base("t-gone").display()), b"").unwrap();
        v.shutdown().unwrap();
        assert!(!vsock_base("t-gone").exists());
        assert!(!Path::new(&format!("{}_53", vsock_base("t-gone").display())).exists());
        assert!(!v.state_dir().exists());
    }

    #[test]
    fn sweep_only_touches_exact_names() {
        let f = fixture();
        fs::write(vsock_base("t-ex"), b"").unwrap();
        let other = vsock_base("t-ex2");
        fs::write(&other, b"").unwrap();
        let junk = PathBuf::from("/tmp/vam-vsock-t-ex.socket_x");
        fs::write(&junk, b"").unwrap();
        sweep_leftovers(&f.root, "t-ex");
        assert!(!vsock_base("t-ex").exists());
        assert!(other.exists() && junk.exists());
        let _ = fs::remove_file(other);
        let _ = fs::remove_file(junk);
    }

    #[test]
    fn adopt_with_a_wrong_pid_signals_nothing() {
        let f = fixture();
        let ready = f._tmp.path().join("ready");
        let mut v = started(&f, "t-ad", &format!("touch {}; sleep 300", ready.display()));
        wait_for(&ready);
        let real = v.get_pid().unwrap();
        v.detach();
        let mut fresh = ProcessVmm::new("t-ad".into(), v.guest_bin.clone(), 41, f.root.clone());
        fresh.adopt(real + 7);
        assert_eq!(fresh.get_pid(), None);
        fresh.pause().unwrap();
        fresh.resume().unwrap();
        assert!(
            matches!(proc_stat(real), Some((_, false))),
            "guest untouched"
        );
        fresh.adopt(real);
        assert_eq!(fresh.get_pid(), Some(real));
        fresh.shutdown().unwrap();
        for _ in 0..100 {
            if !matches!(proc_stat(real), Some((_, false))) {
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(!matches!(proc_stat(real), Some((_, false))));
    }

    #[test]
    fn pause_and_resume_stop_and_continue() {
        let f = fixture();
        let ready = f._tmp.path().join("ready");
        let mut v = started(&f, "t-pr", &format!("touch {}; sleep 300", ready.display()));
        wait_for(&ready);
        let guest = v.get_pid().unwrap();
        v.pause().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        let state = fs::read_to_string(format!("/proc/{guest}/stat")).unwrap();
        assert!(state
            .rsplit(')')
            .next()
            .unwrap()
            .trim_start()
            .starts_with('T'));
        v.resume().unwrap();
        v.shutdown().unwrap();
    }

    #[test]
    fn signal_group_refuses_pgid_0_and_1() {
        // Would signal everything if it did not refuse; SIGCONT is harmless anyway.
        signal_group(0, SIGCONT);
        signal_group(1, SIGCONT);
    }

    #[test]
    fn unsupported_calls_error() {
        let f = fixture();
        let mut v = ProcessVmm::new("t-un".into(), "/bin/true".into(), 3, f.root.clone());
        assert!(v.create_snapshot(Path::new("a"), Path::new("b")).is_err());
        assert!(v
            .load_snapshot(Path::new("a"), Path::new("b"), true, None)
            .is_err());
        assert!(v.patch_data_drive_path("d", Path::new("p")).is_err());
        assert!(v.patch_vsock_uds_path(Path::new("p")).is_err());
    }

    #[test]
    fn refuses_an_image_next_to_the_directory() {
        let f = fixture();
        fs::write(f.session.join("drive9.ext4"), b"x").unwrap();
        let mut v = ProcessVmm::new("t-img".into(), "/bin/true".into(), 3, f.root.clone());
        assert!(v.configure_hardware(&cfg(&f.session), None).is_err());
    }

    #[test]
    fn refuses_a_relative_guest_binary() {
        let f = fixture();
        let mut v = ProcessVmm::new("t-rel".into(), "vam-runtime".into(), 3, f.root.clone());
        assert!(v.spawn().is_err());
    }

    #[test]
    fn workspace_is_chowned_when_root() {
        if unsafe { geteuid() } != 0 {
            return;
        }
        use std::os::unix::fs::MetadataExt;
        let f = fixture();
        let mut v = started(&f, "t-ch", "exit 0");
        let md = fs::metadata(f.session.join("materialized/workspace")).unwrap();
        assert_eq!((md.uid(), md.gid()), (GUEST_UID, GUEST_GID));
        let sk = fs::metadata(f.session.join("materialized/skills")).unwrap();
        assert_eq!(sk.uid(), 0, "skills stay root-owned");
        v.shutdown().unwrap();
    }
}
