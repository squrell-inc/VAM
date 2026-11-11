use crate::config::MicroVmConfig;
use crate::jail::{self, Jail, JailConfig, JailError};
use crate::VamCoreError;
use std::fs;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use vam_firecracker::events::{
    CreateSnapshot, LoadSnapshot, PatchGuestDriveByID, PutGuestBootSource, PutGuestDriveByID,
    PutGuestVsock, PutLogger, PutMetrics,
};
use vam_firecracker::firecracker::FirecrackerOption;
use vam_firecracker::instance::Instance;
use vam_firecracker::jailer::JailerOption;
use vam_firecracker::models::boot_source::BootSource;
use vam_firecracker::models::drive::{CacheType, Drive};
use vam_firecracker::models::logger::{LogLevel, Logger};
use vam_firecracker::models::machine_configuration::MachineConfiguration;
use vam_firecracker::models::metrics::Metrics;
use vam_firecracker::models::vsock::Vsock;

/// The root drive is the base image shared by every session and hashed into the
/// golden snapshot key. It is always attached read-only; the guest init mounts
/// tmpfs for every path it writes.
pub(crate) fn root_drive(config: &MicroVmConfig) -> Drive {
    Drive {
        drive_id: "rootfs".to_string(),
        path_on_host: config.rootfs_path.clone(),
        is_root_device: true,
        is_read_only: true,
        partuuid: None,
        cache_type: Some(CacheType::Unsafe),
        rate_limiter: None,
        io_engine: None,
        socket: None,
    }
}

pub fn record_timing_step(
    sink: &mut Option<&mut Vec<(&'static str, u128)>>,
    label: &'static str,
    start: std::time::Instant,
) {
    if let Some(steps) = sink.as_mut() {
        steps.push((label, start.elapsed().as_micros()));
    }
}

#[cfg(unix)]
extern "C" {
    fn kill(pid: i32, sig: i32) -> i32;
}

/// How long teardown waits for a SIGKILLed VMM to be gone.
pub(crate) const SIGKILL_EXIT_WAIT: Duration = Duration::from_secs(5);

/// Waits until `pid` has exited (a zombie counts), polling; `false` if it is
/// still there at `timeout`.
pub(crate) fn wait_for_exit(pid: u32, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    loop {
        if !jail::process_alive(pid) {
            return true;
        }
        if start.elapsed() >= timeout {
            return false;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// The VMM abstraction trait enabling hot-swappable real/mock hypervisors.
pub trait Vmm: Send + Sync {
    /// Spawns the underlying VMM process (e.g. Firecracker inside Jailer).
    fn spawn(&mut self) -> Result<(), VamCoreError>;

    /// Configures the hardware endpoints prior to booting.
    fn configure_hardware(
        &mut self,
        config: &MicroVmConfig,
        timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<(), VamCoreError>;

    /// Starts the microVM guest kernel.
    fn start_instance(
        &mut self,
        timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<(), VamCoreError>;

    /// Suspends guest CPU execution.
    fn pause(&mut self) -> Result<(), VamCoreError>;

    /// Resumes guest CPU execution.
    fn resume(&mut self) -> Result<(), VamCoreError>;

    /// Creates a Firecracker VM memory and state snapshot.
    fn create_snapshot(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
    ) -> Result<(), VamCoreError>;

    /// Puts what `load_snapshot` and `patch_data_drive_path` will need where the VMM can
    /// reach it, after `spawn` and before `load_snapshot`. Only a jailed VMM has anything
    /// to do (the default does nothing): the golden memory and state files, the rootfs
    /// the snapshot records, a placeholder for the recorded data drive of
    /// `golden_disk_len` bytes, and the host's vsock listeners.
    fn stage_snapshot_restore(
        &mut self,
        _mem_file_path: &std::path::Path,
        _snapshot_path: &std::path::Path,
        _rootfs_path: &std::path::Path,
        _golden_disk_len: u64,
    ) -> Result<(), VamCoreError> {
        Ok(())
    }

    /// Restores a microVM from a previously captured snapshot.
    /// `vsock_uds_override` rebinds the restored vsock device to a new host UDS
    /// path at load time, so the path recorded in the snapshot is never bound.
    fn load_snapshot(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
        resume: bool,
        vsock_uds_override: Option<&std::path::Path>,
    ) -> Result<(), VamCoreError>;

    /// Updates a guest data drive path after snapshot load (VM must be paused).
    fn patch_data_drive_path(
        &mut self,
        drive_id: &str,
        path_on_host: &std::path::Path,
    ) -> Result<(), VamCoreError>;

    /// Rebinds the guest vsock UDS mux path after snapshot load (VM must be paused).
    fn patch_vsock_uds_path(&mut self, uds_path: &std::path::Path) -> Result<(), VamCoreError>;

    /// Signals or gracefully kills the underlying VMM process.
    fn shutdown(&mut self) -> Result<(), VamCoreError>;

    /// Returns the PID of the Firecracker process, if running.
    fn get_pid(&self) -> Option<u32>;

    /// Detaches the VMM process handle so it is not killed on drop.
    fn detach(&mut self);

    /// Turns a freshly built VMM into a handle on the already-running process
    /// `pid`, so teardown, pause and resume go through the configured VMM
    /// implementation instead of a hardcoded one. The default does nothing
    /// (the mock has no process).
    fn adopt(&mut self, _pid: u32) {}

    /// Removes the files a VMM that already exited left behind, without
    /// signalling anything. The default does nothing: Firecracker teardown
    /// for a dead VM has always been skipped.
    fn cleanup_after_exit(&mut self) {}
}

/// A mock implementation of `Vmm` for hermetic testing on non-KVM hosts.
pub struct MockVmm {
    history: Arc<Mutex<Vec<String>>>,
    pub pid: Option<u32>,
    pub configure_error: Option<String>,
}

impl Default for MockVmm {
    fn default() -> Self {
        Self::new()
    }
}

impl MockVmm {
    pub fn new() -> Self {
        Self {
            history: Arc::new(Mutex::new(Vec::new())),
            pid: Some(12345),
            configure_error: None,
        }
    }

    pub fn with_shared_history(history: Arc<Mutex<Vec<String>>>) -> Self {
        Self {
            history,
            pid: Some(12345),
            configure_error: None,
        }
    }

    fn record(&self, action: &str) {
        if let Ok(mut hist) = self.history.lock() {
            hist.push(action.to_string());
        }
    }

    pub fn get_history(&self) -> Vec<String> {
        if let Ok(hist) = self.history.lock() {
            hist.clone()
        } else {
            Vec::new()
        }
    }

    pub fn shared_history(&self) -> Arc<Mutex<Vec<String>>> {
        self.history.clone()
    }
}

impl Vmm for MockVmm {
    fn spawn(&mut self) -> Result<(), VamCoreError> {
        self.record("spawn");
        Ok(())
    }

    fn configure_hardware(
        &mut self,
        _config: &MicroVmConfig,
        _timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<(), VamCoreError> {
        self.record("configure_hardware");
        if let Some(ref err) = self.configure_error {
            return Err(VamCoreError::InvalidConfig(err.clone()));
        }
        Ok(())
    }

    fn start_instance(
        &mut self,
        _timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<(), VamCoreError> {
        self.record("start_instance");
        Ok(())
    }

    fn pause(&mut self) -> Result<(), VamCoreError> {
        self.record("pause");
        Ok(())
    }

    fn resume(&mut self) -> Result<(), VamCoreError> {
        self.record("resume");
        Ok(())
    }

    fn create_snapshot(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
    ) -> Result<(), VamCoreError> {
        self.record(&format!(
            "create_snapshot:{}:{}",
            mem_file_path.display(),
            snapshot_path.display()
        ));
        Ok(())
    }

    fn stage_snapshot_restore(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
        rootfs_path: &std::path::Path,
        golden_disk_len: u64,
    ) -> Result<(), VamCoreError> {
        self.record(&format!(
            "stage_snapshot_restore:{}:{}:{}:{}",
            mem_file_path.display(),
            snapshot_path.display(),
            rootfs_path.display(),
            golden_disk_len
        ));
        Ok(())
    }

    fn load_snapshot(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
        resume: bool,
        vsock_uds_override: Option<&std::path::Path>,
    ) -> Result<(), VamCoreError> {
        self.record(&format!(
            "load_snapshot:{}:{}:{}",
            mem_file_path.display(),
            snapshot_path.display(),
            resume
        ));
        if let Some(uds_path) = vsock_uds_override {
            self.record(&format!(
                "load_snapshot_vsock_override:{}",
                uds_path.display()
            ));
        }
        Ok(())
    }

    fn patch_data_drive_path(
        &mut self,
        drive_id: &str,
        path_on_host: &std::path::Path,
    ) -> Result<(), VamCoreError> {
        self.record(&format!(
            "patch_data_drive:{}:{}",
            drive_id,
            path_on_host.display()
        ));
        Ok(())
    }

    fn patch_vsock_uds_path(&mut self, uds_path: &std::path::Path) -> Result<(), VamCoreError> {
        self.record(&format!("patch_vsock:{}", uds_path.display()));
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), VamCoreError> {
        self.record("shutdown");
        Ok(())
    }

    fn get_pid(&self) -> Option<u32> {
        self.pid
    }

    fn detach(&mut self) {
        self.record("detach");
    }

    fn adopt(&mut self, pid: u32) {
        self.record(&format!("adopt:{pid}"));
    }

    fn cleanup_after_exit(&mut self) {
        self.record("cleanup_after_exit");
    }
}

/// A production-grade implementation of `Vmm` driving a real Firecracker/Jailer microVM.
pub struct FirecrackerVmm {
    id: String,
    firecracker_bin: PathBuf,
    jailer_bin: PathBuf,
    use_jailer: bool,
    jail_cfg: JailConfig,
    /// The live jail, once a jailed VMM has spawned.
    jail: Option<Jail>,
    /// Symlinks this VMM made on the host that point into the jail.
    host_links: Vec<PathBuf>,
    socket_path: PathBuf,
    vsock_uds_path: PathBuf,
    guest_cid: u32,
    instance: Option<Instance>,
    booted: bool,
    stdout_path: PathBuf,
    stderr_path: PathBuf,
    adopted_pid: Option<u32>,
    detached: bool,
}

impl FirecrackerVmm {
    pub fn new(
        id: String,
        firecracker_bin: PathBuf,
        jailer_bin: PathBuf,
        use_jailer: bool,
        guest_cid: u32,
    ) -> Self {
        let socket_path = PathBuf::from(format!("/tmp/vam-fc-{}.socket", id));
        let vsock_uds_path = crate::jail::vsock_uds_path(&id);
        let stdout_path = PathBuf::from(format!("/tmp/vam-fc-{}.stdout", id));
        let stderr_path = PathBuf::from(format!("/tmp/vam-fc-{}.stderr", id));

        Self {
            id,
            firecracker_bin,
            jailer_bin,
            use_jailer,
            jail_cfg: JailConfig::default(),
            jail: None,
            host_links: Vec::new(),
            socket_path,
            vsock_uds_path,
            guest_cid,
            instance: None,
            booted: false,
            stdout_path,
            stderr_path,
            adopted_pid: None,
            detached: false,
        }
    }

    pub fn new_adopted(
        id: String,
        firecracker_bin: PathBuf,
        jailer_bin: PathBuf,
        use_jailer: bool,
        guest_cid: u32,
        pid: u32,
    ) -> Self {
        let mut vmm = Self::new(id, firecracker_bin, jailer_bin, use_jailer, guest_cid);
        vmm.adopted_pid = Some(pid);
        vmm.booted = true; // Since we are adopting it, it's already booted!
        vmm
    }

    /// The teardown inputs of this handle, for comparing an adopted handle with
    /// one built by `new_adopted`.
    #[cfg(test)]
    pub(crate) fn teardown_plan(&self) -> (bool, Option<u32>, bool, PathBuf, PathBuf) {
        (
            self.use_jailer,
            self.adopted_pid,
            self.booted,
            self.socket_path.clone(),
            self.vsock_uds_path.clone(),
        )
    }

    pub fn with_uid_gid(mut self, uid: usize, gid: usize) -> Self {
        self.jail_cfg.uid = uid as u32;
        self.jail_cfg.gid = gid as u32;
        self
    }

    /// The jailer settings (uid, gid, base dir, cgroup version) used when `use_jailer`.
    pub fn with_jail(mut self, cfg: JailConfig) -> Self {
        self.jail_cfg = cfg;
        self
    }

    fn exec_name(&self) -> Result<std::ffi::OsString, VamCoreError> {
        self.firecracker_bin
            .file_name()
            .map(|n| n.to_os_string())
            .ok_or_else(|| VamCoreError::InvalidConfig("Bad Firecracker path".to_string()))
    }

    /// `<base>/<exec>/<id>` for this VM, jailed or not.
    fn jail_dir(&self) -> Option<PathBuf> {
        self.exec_name()
            .ok()
            .map(|n| self.jail_cfg.jail_dir(&n, &self.id))
    }

    /// Kills the Firecracker of this jail by its pid file when this handle has no pid
    /// of its own (a fresh handle after a failed restore, a crashed spawn). Only a pid
    /// whose root is this jail is signalled.
    fn pid_from_jail(&self) -> Option<u32> {
        let dir = self.jail_dir()?;
        let root = dir.join("root");
        let pid = jail::read_pid_file(&root, &self.exec_name().ok()?)?;
        jail::pid_is_in_jail(pid, &root).then_some(pid)
    }

    /// Removes the jail directory and the host links into it.
    fn remove_jail_artifacts(&mut self) {
        if let Some(dir) = self.jail_dir() {
            let _ = fs::remove_dir_all(dir);
        }
        jail::remove_host_links(&std::env::temp_dir(), &self.id);
        if let Some(run) = self.vsock_uds_path.parent() {
            jail::remove_host_links(run, &self.id);
        }
        for link in self.host_links.drain(..) {
            // Only symlinks: never a regular file that happens to share the name.
            if fs::symlink_metadata(&link)
                .map(|m| m.file_type().is_symlink())
                .unwrap_or(false)
            {
                let _ = fs::remove_file(link);
            }
        }
        self.jail = None;
    }

    /// Snapshot of a paused jailed VM. Firecracker writes `/mem.snap` and `/vm.state`
    /// inside the jail; then it is killed (nothing may hold those files while root
    /// publishes them) and each is given to root, mode 0644, and linked out to the
    /// golden paths. `vm_state.bin` records the in-jail names, so the snapshot restores
    /// in any jail with the same layout. The jail itself stays for `shutdown`.
    fn create_snapshot_jailed(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
    ) -> Result<(), VamCoreError> {
        let jail = spawned_jail(&self.jail)?.clone();
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| VamCoreError::InvalidConfig("VMM not running".to_string()))?;
        let params = vam_firecracker::models::snapshot_create_params::SnapshotCreateParams {
            mem_file_path: PathBuf::from(jail::IN_JAIL_MEM),
            snapshot_path: PathBuf::from(jail::IN_JAIL_STATE),
            snapshot_type: Some(
                vam_firecracker::models::snapshot_create_params::SnapshotType::Full,
            ),
            version: None,
        };
        instance
            .event(CreateSnapshot(&params))
            .map(|_| ())
            .map_err(VamCoreError::FirecrackerSdk)?;
        let pid = self.get_pid().or_else(|| self.pid_from_jail());
        if let Some(pid) = pid {
            kill_and_wait(pid);
        }
        let snap_err = |e: JailError| VamCoreError::SnapshotFailed(e.to_string());
        for (in_jail, dest) in [
            (jail::IN_JAIL_MEM, mem_file_path),
            (jail::IN_JAIL_STATE, snapshot_path),
        ] {
            let _ = fs::remove_file(dest);
            jail::export_from_jail(&jail.host_path(in_jail), dest, 0, 0).map_err(snap_err)?;
        }
        Ok(())
    }

    fn build_jailed_instance(&mut self) -> Result<Instance, VamCoreError> {
        let exec_name = self.exec_name()?;
        jail::validate_id(&self.id).map_err(jail_err)?;
        let dir = self.jail_cfg.jail_dir(&exec_name, &self.id);
        let root = dir.join("root");
        jail::check_socket_path_len(&root.join("run/firecracker.socket")).map_err(jail_err)?;
        // The longest host-listener name: `v.sock_55`.
        jail::check_socket_path_len(&root.join("v.sock_55")).map_err(jail_err)?;
        if dir.exists() {
            // A jail left by an earlier attempt with this id (a failed restore, a crash):
            // its VMM is obsolete.
            if let Some(pid) = self.pid_from_jail() {
                #[cfg(unix)]
                unsafe {
                    kill(pid as i32, 9)
                };
                #[cfg(not(unix))]
                let _ = pid;
            }
            let _ = fs::remove_dir_all(&dir);
        }
        fs::create_dir_all(&self.jail_cfg.base_dir).map_err(|e| VamCoreError::JailerSetup {
            details: format!("Failed to create {}: {e}", self.jail_cfg.base_dir.display()),
        })?;

        let mut jailer_opt = JailerOption::new(
            &self.jailer_bin,
            &self.firecracker_bin,
            &self.id,
            self.jail_cfg.gid as usize,
            self.jail_cfg.uid as usize,
        );
        jailer_opt.chroot_base_dir(Some(&self.jail_cfg.base_dir));
        jailer_opt.chroot_strategy(vam_firecracker::jailer::ChrootStrategy::Prestaged);
        jailer_opt.cgroup_version(Some(self.jail_cfg.cgroup_version as usize));
        // Its own PID namespace. Not daemonized: the jailer would send Firecracker's
        // stdout (the guest console) and stderr to /dev/null.
        jailer_opt.new_pid_ns(Some(true));

        let mut fc_opt = FirecrackerOption::new(&self.firecracker_bin);
        fc_opt.api_sock(jail::IN_JAIL_API_SOCKET);
        jailer_opt.firecracker_option(Some(&fc_opt));

        jailer_opt.stdout(&self.stdout_path);
        jailer_opt.stderr(&self.stderr_path);
        jailer_opt.stdin("/dev/null");

        let instance = jailer_opt.build().map_err(|e| VamCoreError::JailerSetup {
            details: e.to_string(),
        })?;
        self.jail = Some(Jail::new(root, self.jail_cfg.uid, self.jail_cfg.gid));
        Ok(instance)
    }
}

/// Points a host path at a file in the jail. Best effort: the jail file is the truth.
fn link_host_to_jail(links: &mut Vec<PathBuf>, host: &std::path::Path, jail: &Jail, in_jail: &str) {
    if jail::replace_symlink(host, &jail.host_path(in_jail)).is_ok() {
        links.push(host.to_path_buf());
    }
}

/// The jail of a jailed VMM that has spawned; an error when it has not.
fn spawned_jail(jail: &Option<Jail>) -> Result<&Jail, VamCoreError> {
    jail.as_ref()
        .ok_or_else(|| VamCoreError::InvalidConfig("VMM not spawned".to_string()))
}

/// SIGKILLs `pid` and waits (bounded) until it is gone or a zombie.
#[cfg(unix)]
fn kill_and_wait(pid: u32) {
    // SAFETY: plain signal to a pid this handle owns.
    unsafe { kill(pid as i32, 9) };
    let start = std::time::Instant::now();
    while jail::process_alive(pid) && start.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(not(unix))]
fn kill_and_wait(_pid: u32) {}

fn jail_err(e: JailError) -> VamCoreError {
    VamCoreError::JailerSetup {
        details: e.to_string(),
    }
}

/// `VAM_KEEP_VM_LOGS=1` keeps each Firecracker stdout (the guest serial console when
/// `VAM_SERIAL_CONSOLE=1`) and stderr after the VMM is dropped and when a later VMM
/// reuses the same id (the cold fallback after a failed restore). CI diagnostics only.
fn keep_vm_logs() -> bool {
    std::env::var("VAM_KEEP_VM_LOGS")
        .map(|v| v == "1")
        .unwrap_or(false)
}

/// Renames an existing log to `<name>.<unix millis>` so a new VMM with the same id does
/// not truncate it.
fn preserve_previous_log(path: &std::path::Path) {
    if !path.is_file() {
        return;
    }
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".{ms}"));
    let _ = fs::rename(path, std::path::PathBuf::from(name));
}

impl Vmm for FirecrackerVmm {
    fn spawn(&mut self) -> Result<(), VamCoreError> {
        if self.instance.is_some() {
            return Ok(()); // Already spawned
        }

        // 1. Setup Captured Stdout/Stderr Files
        if keep_vm_logs() {
            preserve_previous_log(&self.stdout_path);
            preserve_previous_log(&self.stderr_path);
        }
        let stdout_file =
            fs::File::create(&self.stdout_path).map_err(|e| VamCoreError::JailerSetup {
                details: format!("Failed to create stdout log file: {e}"),
            })?;
        let stderr_file =
            fs::File::create(&self.stderr_path).map_err(|e| VamCoreError::JailerSetup {
                details: format!("Failed to create stderr log file: {e}"),
            })?;

        // 2. Build Instance via SDK options
        let mut instance = if self.use_jailer {
            self.build_jailed_instance()?
        } else {
            let mut fc_opt = FirecrackerOption::new(&self.firecracker_bin);
            fc_opt.api_sock(&self.socket_path);
            fc_opt.id(&self.id);

            // Configure stdin/stderr/stdout on raw process Command
            let mut cmd = fc_opt.build_cmd();
            cmd.stdin(Stdio::null());
            cmd.stdout(Stdio::from(stdout_file));
            cmd.stderr(Stdio::from(stderr_file));

            let exec_file_name = self
                .firecracker_bin
                .file_name()
                .ok_or_else(|| VamCoreError::InvalidConfig("Bad Firecracker path".to_string()))?;

            Instance::new(
                self.socket_path.clone(),
                None,
                None,
                None,
                cmd,
                PathBuf::from(exec_file_name),
            )
        };

        // 3. Spawn VMM process
        instance.spawn_vmm().map_err(VamCoreError::FirecrackerSdk)?;
        self.instance = Some(instance);
        self.booted = false;

        if let Some(jail) = self.jail.clone() {
            // Firecracker binds the vsock socket inside the jail when the guest starts.
            // Everything on the host dials `/tmp/vam-vsock-<id>.socket`, so that path
            // becomes a symlink to it (dangling until then; dialers retry).
            jail::replace_symlink(&self.vsock_uds_path, &jail.host_path(jail::IN_JAIL_VSOCK))
                .map_err(|e| VamCoreError::JailerSetup {
                    details: format!("link {}: {e}", self.vsock_uds_path.display()),
                })?;
            self.host_links.push(self.vsock_uds_path.clone());
        }

        Ok(())
    }

    fn configure_hardware(
        &mut self,
        config: &MicroVmConfig,
        mut timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<(), VamCoreError> {
        let jail = self.jail.clone();
        let vsock_uds_path = self.vsock_uds_path.clone();
        let guest_cid = self.guest_cid;
        let instance = self.instance.as_mut().ok_or_else(|| {
            VamCoreError::InvalidConfig(
                "VMM must be spawned before configuring hardware".to_string(),
            )
        })?;

        // Boot Barrier Invariant
        if self.booted {
            return Err(VamCoreError::InvalidConfig(
                "Crossed Boot Barrier: Cannot configure hardware post-boot".to_string(),
            ));
        }
        let sdk = |e| VamCoreError::FirecrackerSdk(e);

        // 0. Put Logger if configured. Jailed: a file in the jail, linked from the host path.
        if let Some(ref log_path) = config.log_path {
            let t = std::time::Instant::now();
            let logger = |path: PathBuf| Logger {
                level: Some(LogLevel::Info),
                log_path: path,
                show_level: Some(true),
                show_log_origin: Some(true),
                module: None,
            };
            if let Some(jail) = &jail {
                jail.create_owned_file(jail::IN_JAIL_LOG)
                    .map_err(jail_err)?;
                instance
                    .event(PutLogger(&logger(PathBuf::from(jail::IN_JAIL_LOG))))
                    .map(|_| ())
                    .map_err(sdk)?;
                link_host_to_jail(&mut self.host_links, log_path, jail, jail::IN_JAIL_LOG);
            } else {
                instance
                    .put_logger(&logger(log_path.clone()))
                    .map_err(sdk)?;
            }
            record_timing_step(&mut timing, "fc_put_logger", t);
        }

        // 0.5 Put Metrics if configured
        if let Some(ref metrics_path) = config.metrics_path {
            let t = std::time::Instant::now();
            if let Some(jail) = &jail {
                jail.create_owned_file(jail::IN_JAIL_METRICS)
                    .map_err(jail_err)?;
                instance
                    .event(PutMetrics(&Metrics {
                        metrics_path: PathBuf::from(jail::IN_JAIL_METRICS),
                    }))
                    .map(|_| ())
                    .map_err(sdk)?;
                link_host_to_jail(
                    &mut self.host_links,
                    metrics_path,
                    jail,
                    jail::IN_JAIL_METRICS,
                );
            } else {
                instance
                    .put_metrics(&Metrics {
                        metrics_path: metrics_path.clone(),
                    })
                    .map_err(sdk)?;
            }
            record_timing_step(&mut timing, "fc_put_metrics", t);
        }

        // 1. Put Machine Configuration
        let t = std::time::Instant::now();
        instance
            .put_machine_configuration(&MachineConfiguration {
                cpu_template: None,
                smt: Some(false),
                mem_size_mib: config.mem_size_mib as isize,
                track_dirty_pages: Some(config.track_dirty_pages),
                vcpu_count: config.vcpu_count as isize,
                huge_pages: None,
            })
            .map_err(sdk)?;
        record_timing_step(&mut timing, "fc_put_machine_config", t);

        // 2. Put Boot Source
        let t = std::time::Instant::now();
        let mut boot_source = BootSource {
            boot_args: Some(config.boot_args.clone()),
            initrd_path: None,
            kernel_image_path: config.kernel_path.clone(),
        };
        if let Some(jail) = &jail {
            jail.stage_shared(&config.kernel_path, jail::IN_JAIL_KERNEL, "guest kernel")
                .map_err(jail_err)?;
            boot_source.kernel_image_path = PathBuf::from(jail::IN_JAIL_KERNEL);
            instance
                .event(PutGuestBootSource(&boot_source))
                .map(|_| ())
                .map_err(sdk)?;
        } else {
            instance.put_guest_boot_source(&boot_source).map_err(sdk)?;
        }
        record_timing_step(&mut timing, "fc_put_boot_source", t);

        // 3. Put Root Drive (shared base image: read-only so sessions cannot write to it)
        let t = std::time::Instant::now();
        let mut rootfs = root_drive(config);
        if let Some(jail) = &jail {
            jail.stage_shared(&config.rootfs_path, jail::IN_JAIL_ROOTFS, "rootfs")
                .map_err(jail_err)?;
            rootfs.path_on_host = PathBuf::from(jail::IN_JAIL_ROOTFS);
            instance
                .event(PutGuestDriveByID(&rootfs))
                .map(|_| ())
                .map_err(sdk)?;
        } else {
            instance.put_guest_drive_by_id(&rootfs).map_err(sdk)?;
        }
        record_timing_step(&mut timing, "fc_put_root_drive", t);

        // 3.5 Put Data Drives (Drive9 workspace: WriteBack; read-only skills: Unsafe)
        for (i, drive) in config.data_drives.iter().enumerate() {
            let cache_type = drive.cache_type.or(if drive.is_read_only {
                Some(CacheType::Unsafe)
            } else {
                Some(CacheType::WriteBack)
            });
            let t = std::time::Instant::now();
            let mut data = Drive {
                drive_id: format!("data{}", i),
                path_on_host: drive.path.clone(),
                is_root_device: false,
                is_read_only: drive.is_read_only,
                partuuid: None,
                cache_type,
                rate_limiter: None,
                io_engine: None,
                socket: None,
            };
            if let Some(jail) = &jail {
                let name = jail::in_jail_data_drive(i);
                if drive.is_read_only {
                    jail.stage_shared(&drive.path, &name, "read-only data drive")
                        .map_err(jail_err)?;
                } else {
                    // This VM's own image (Drive9 workspace): the VMM writes it.
                    jail.stage_owned(&drive.path, &name).map_err(jail_err)?;
                }
                data.path_on_host = PathBuf::from(name);
                instance
                    .event(PutGuestDriveByID(&data))
                    .map(|_| ())
                    .map_err(sdk)?;
            } else {
                instance.put_guest_drive_by_id(&data).map_err(sdk)?;
            }
            record_timing_step(
                &mut timing,
                if i == 0 {
                    "fc_put_data_drive"
                } else {
                    "fc_put_data_drive_extra"
                },
                t,
            );
        }

        // 4. Put Guest Vsock
        let t = std::time::Instant::now();
        let mut vsock = Vsock {
            guest_cid,
            uds_path: vsock_uds_path,
            vsock_id: None,
        };
        if jail.is_some() {
            vsock.uds_path = PathBuf::from(jail::IN_JAIL_VSOCK);
            instance
                .event(PutGuestVsock(&vsock))
                .map(|_| ())
                .map_err(sdk)?;
        } else {
            instance.put_guest_vsock(&vsock).map_err(sdk)?;
        }
        record_timing_step(&mut timing, "fc_put_vsock", t);

        Ok(())
    }

    fn start_instance(
        &mut self,
        mut timing: Option<&mut Vec<(&'static str, u128)>>,
    ) -> Result<(), VamCoreError> {
        let instance = self.instance.as_mut().ok_or_else(|| {
            VamCoreError::InvalidConfig("VMM must be spawned before starting".to_string())
        })?;

        if self.booted {
            return Err(VamCoreError::InvalidConfig(
                "MicroVM is already booted".to_string(),
            ));
        }

        // Jailed: the host's vsock listeners (bound before the guest boots) enter the jail
        // now, so the guest's first connect finds them. Owned by the jail uid, mode 0600.
        if let Some(jail) = &self.jail {
            jail.link_listener_sockets(&self.vsock_uds_path, jail::IN_JAIL_VSOCK)
                .map_err(jail_err)?;
        }

        let t = std::time::Instant::now();
        instance
            .send_instance_start()
            .map_err(VamCoreError::FirecrackerSdk)?;
        record_timing_step(&mut timing, "fc_instance_start", t);
        self.booted = true;

        Ok(())
    }

    fn pause(&mut self) -> Result<(), VamCoreError> {
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| VamCoreError::InvalidConfig("VMM not running".to_string()))?;
        instance.pause().map_err(VamCoreError::FirecrackerSdk)?;
        Ok(())
    }

    fn resume(&mut self) -> Result<(), VamCoreError> {
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| VamCoreError::InvalidConfig("VMM not running".to_string()))?;
        instance.resume().map_err(VamCoreError::FirecrackerSdk)?;
        Ok(())
    }

    fn create_snapshot(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
    ) -> Result<(), VamCoreError> {
        if self.use_jailer {
            return self.create_snapshot_jailed(mem_file_path, snapshot_path);
        }
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| VamCoreError::InvalidConfig("VMM not running".to_string()))?;
        let params = vam_firecracker::models::snapshot_create_params::SnapshotCreateParams {
            mem_file_path: mem_file_path.to_path_buf(),
            snapshot_path: snapshot_path.to_path_buf(),
            snapshot_type: Some(
                vam_firecracker::models::snapshot_create_params::SnapshotType::Full,
            ),
            version: None,
        };
        instance
            .create_snapshot(&params)
            .map_err(VamCoreError::FirecrackerSdk)?;
        Ok(())
    }

    fn stage_snapshot_restore(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
        rootfs_path: &std::path::Path,
        golden_disk_len: u64,
    ) -> Result<(), VamCoreError> {
        if !self.use_jailer {
            return Ok(());
        }
        let jail = spawned_jail(&self.jail)?;
        // Read-only to the VMM: root-owned shared inodes (checked), never chowned.
        jail.stage_shared(mem_file_path, jail::IN_JAIL_MEM, "golden snapshot memory")
            .map_err(jail_err)?;
        jail.stage_shared(snapshot_path, jail::IN_JAIL_STATE, "golden snapshot state")
            .map_err(jail_err)?;
        jail.stage_shared(rootfs_path, jail::IN_JAIL_ROOTFS, "rootfs")
            .map_err(jail_err)?;
        // The snapshot records `data0` read-write at `/data0.ext4` and Firecracker
        // re-opens it at load: this session's own empty stand-in, not the golden disk.
        jail.create_placeholder_disk(&jail::in_jail_data_drive(0), golden_disk_len)
            .map_err(jail_err)?;
        // The guest connects to the host's listeners as soon as it runs.
        jail.link_listener_sockets(&self.vsock_uds_path, jail::IN_JAIL_VSOCK)
            .map_err(jail_err)?;
        Ok(())
    }

    fn load_snapshot(
        &mut self,
        mem_file_path: &std::path::Path,
        snapshot_path: &std::path::Path,
        resume: bool,
        vsock_uds_override: Option<&std::path::Path>,
    ) -> Result<(), VamCoreError> {
        let jailed = self.use_jailer;
        if jailed {
            spawned_jail(&self.jail)?;
        }
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| VamCoreError::InvalidConfig("VMM not spawned".to_string()))?;
        // Jailed: the files were staged under fixed in-jail names, and the vsock device
        // binds `/v.sock` (the host path is the symlink `spawn` made to it).
        let params = vam_firecracker::models::snapshot_load_params::SnapshotLoadParams {
            mem_file_path: Some(if jailed {
                PathBuf::from(jail::IN_JAIL_MEM)
            } else {
                mem_file_path.to_path_buf()
            }),
            snapshot_path: if jailed {
                PathBuf::from(jail::IN_JAIL_STATE)
            } else {
                snapshot_path.to_path_buf()
            },
            resume_vm: Some(resume),
            enable_diff_snapshots: None,
            mem_backend: None,
            vsock_override: vsock_uds_override.map(|uds_path| {
                vam_firecracker::models::VsockOverride {
                    uds_path: if jailed {
                        PathBuf::from(jail::IN_JAIL_VSOCK)
                    } else {
                        uds_path.to_path_buf()
                    },
                }
            }),
        };

        if jailed {
            instance
                .event(LoadSnapshot(&params))
                .map(|_| ())
                .map_err(VamCoreError::FirecrackerSdk)?;
        } else {
            instance
                .load_snapshot(&params)
                .map_err(VamCoreError::FirecrackerSdk)?;
        }
        if jailed {
            // Memory is mapped and the state is read: the names are not needed again.
            if let Some(jail) = &self.jail {
                let _ = jail.unlink(jail::IN_JAIL_MEM);
                let _ = jail.unlink(jail::IN_JAIL_STATE);
            }
        } else if let Some(uds_path) = vsock_uds_override {
            self.vsock_uds_path = uds_path.to_path_buf();
        }
        self.booted = true;
        Ok(())
    }

    fn patch_data_drive_path(
        &mut self,
        drive_id: &str,
        path_on_host: &std::path::Path,
    ) -> Result<(), VamCoreError> {
        let jail = if self.use_jailer {
            Some(spawned_jail(&self.jail)?.clone())
        } else {
            None
        };
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| VamCoreError::InvalidConfig("VMM not running".to_string()))?;
        let mut partial = vam_firecracker::models::PartialDrive {
            drive_id: drive_id.to_string(),
            path_on_host: Some(path_on_host.to_path_buf()),
            rate_limiter: None,
        };
        let Some(jail) = jail else {
            instance
                .patch_guest_drive_by_id(&partial)
                .map_err(VamCoreError::FirecrackerSdk)?;
            return Ok(());
        };
        let Some(index) = drive_id
            .strip_prefix("data")
            .and_then(|n| n.parse::<usize>().ok())
        else {
            return Err(VamCoreError::InvalidConfig(format!(
                "not a data drive: {drive_id}"
            )));
        };
        // This session's own image (the VMM writes it), under its own name.
        jail.stage_owned(path_on_host, jail::IN_JAIL_DATA_SESSION)
            .map_err(jail_err)?;
        partial.path_on_host = Some(PathBuf::from(jail::IN_JAIL_DATA_SESSION));
        instance
            .event(PatchGuestDriveByID(&partial))
            .map(|_| ())
            .map_err(VamCoreError::FirecrackerSdk)?;
        // The stand-in the snapshot's recorded path needed is not wanted any more. The
        // guest is still paused (the caller resumes after this), so nothing ran with it.
        jail.unlink(&jail::in_jail_data_drive(index))
            .map_err(jail_err)
    }

    fn patch_vsock_uds_path(&mut self, uds_path: &std::path::Path) -> Result<(), VamCoreError> {
        if self.use_jailer {
            // The vsock path in a jail is fixed (`/v.sock`): restore passes it as the
            // load-time override instead.
            return Err(VamCoreError::SnapshotFailed(
                "the vsock path of a jailed VM is fixed; use the load-time override".to_string(),
            ));
        }
        let instance = self
            .instance
            .as_mut()
            .ok_or_else(|| VamCoreError::InvalidConfig("VMM not running".to_string()))?;
        let partial = vam_firecracker::models::PartialVsock {
            uds_path: Some(uds_path.to_path_buf()),
        };
        instance
            .patch_guest_vsock(&partial)
            .map_err(VamCoreError::FirecrackerSdk)?;
        self.vsock_uds_path = uds_path.to_path_buf();
        Ok(())
    }

    fn shutdown(&mut self) -> Result<(), VamCoreError> {
        // Idempotent GC Teardown Invariant
        let mut pid = self.get_pid();
        if pid.is_none() && self.use_jailer {
            // A handle with no process of its own (the cold fallback after a failed
            // restore, a spawn that died half way): the jail's pid file still names
            // the Firecracker that must not outlive its session.
            pid = self.pid_from_jail();
        }

        if let Some(pid_val) = pid {
            #[allow(unused_variables)]
            let _ = pid_val;
            #[cfg(unix)]
            {
                // 1. Send SIGTERM first to allow graceful guest/VMM stop
                unsafe { kill(pid_val as i32, 15) };

                // 2. Wait up to 2 seconds with try_wait
                let mut reaped = false;
                let start = std::time::Instant::now();
                while start.elapsed() < Duration::from_secs(2) {
                    if let Some(instance) = &mut self.instance {
                        // Check if the process exited
                        if instance.describe_instance().is_ok() {
                            // Still running
                        } else {
                            reaped = true;
                            break;
                        }
                    } else if !jail::process_alive(pid_val) {
                        // Adopted PID check: the process has exited (a zombie counts)
                        reaped = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }

                // 3. Escalate to SIGKILL if still running, then wait (bounded) for the
                // process to be gone: the Drive9 image is read right after teardown,
                // and a VMM that is still dying could still be writing it.
                if !reaped {
                    unsafe { kill(pid_val as i32, 9) };
                    if !wait_for_exit(pid_val, SIGKILL_EXIT_WAIT) {
                        eprintln!(
                            "[vmm] WARN pid {pid_val} still present {}s after SIGKILL; tearing down anyway",
                            SIGKILL_EXIT_WAIT.as_secs()
                        );
                    }
                }
            }
        }

        // 4. Force release / cleanup files and directories
        let _ = fs::remove_file(&self.socket_path);
        let _ = fs::remove_file(&self.vsock_uds_path);

        // Also clean up any port-specific vsock socket files (e.g. <uds_path>_52)
        if let Some(parent) = self.vsock_uds_path.parent() {
            if let Some(file_name) = self.vsock_uds_path.file_name() {
                let file_name_str = file_name.to_string_lossy();
                if let Ok(entries) = fs::read_dir(parent) {
                    for entry in entries.flatten() {
                        let name = entry.file_name().to_string_lossy().into_owned();
                        if name.starts_with(&format!("{}_", file_name_str)) {
                            let _ = fs::remove_file(entry.path());
                        }
                    }
                }
            }
        }

        if self.use_jailer {
            // The whole jail: its root, sockets, the staged links and the host links
            // that point into it. A hard link only drops a name; the files on the host
            // (kernel, rootfs, Drive9 image) stay.
            self.remove_jail_artifacts();
        }

        self.instance = None;
        self.booted = false;

        Ok(())
    }

    fn get_pid(&self) -> Option<u32> {
        self.adopted_pid.or_else(|| {
            self.instance
                .as_ref()
                .and_then(|inst| inst.firecracker_pid())
        })
    }

    fn detach(&mut self) {
        self.detached = true;
    }

    /// An adopted VM keeps the configured `use_jailer`: a jailed VM's teardown also
    /// removes its jail directory (by id, whichever process adopted it).
    fn adopt(&mut self, pid: u32) {
        self.adopted_pid = Some(pid);
        self.booted = true;
    }

    /// A jailed VM that already exited leaves its jail directory behind: remove it
    /// (no signal; the pid may be reused). An unjailed one has always been skipped.
    fn cleanup_after_exit(&mut self) {
        if self.use_jailer {
            self.remove_jail_artifacts();
        }
    }
}

// Clean up captured files on drop for safety if NOT detached
impl Drop for FirecrackerVmm {
    fn drop(&mut self) {
        if !self.detached {
            let _ = self.shutdown();
            if !keep_vm_logs() {
                let _ = fs::remove_file(&self.stdout_path);
                let _ = fs::remove_file(&self.stderr_path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserve_previous_log_renames_instead_of_truncating() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("vam-fc-x.stdout");
        preserve_previous_log(&p); // missing file: no-op
        fs::write(&p, "console").unwrap();
        preserve_previous_log(&p);
        assert!(!p.exists());
        let kept: Vec<_> = fs::read_dir(dir.path()).unwrap().flatten().collect();
        assert_eq!(kept.len(), 1);
        assert!(kept[0]
            .file_name()
            .to_string_lossy()
            .starts_with("vam-fc-x.stdout."));
        assert_eq!(fs::read_to_string(kept[0].path()).unwrap(), "console");
    }

    use crate::config::MicroVmConfigBuilder;

    #[cfg(unix)]
    fn spawn_sh(script: &str) -> std::process::Child {
        std::process::Command::new("sh")
            .args(["-c", script])
            .spawn()
            .expect("spawn sh")
    }

    #[cfg(unix)]
    #[test]
    fn wait_for_exit_returns_once_a_real_child_has_exited() {
        let mut child = spawn_sh("sleep 30");
        let pid = child.id();
        let killer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            let _ = child.kill();
            let _ = child.wait();
        });
        let start = std::time::Instant::now();
        assert!(wait_for_exit(pid, Duration::from_secs(10)));
        assert!(
            start.elapsed() >= Duration::from_millis(250),
            "{:?}",
            start.elapsed()
        );
        killer.join().unwrap();
        // Gone already: no wait at all.
        let start = std::time::Instant::now();
        assert!(wait_for_exit(pid, Duration::from_secs(10)));
        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[cfg(unix)]
    #[test]
    fn wait_for_exit_gives_up_at_the_timeout_on_a_process_that_stays() {
        let mut child = spawn_sh("sleep 30");
        let start = std::time::Instant::now();
        assert!(!wait_for_exit(child.id(), Duration::from_millis(200)));
        assert!(start.elapsed() >= Duration::from_millis(200));
        let _ = child.kill();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn shutdown_waits_for_a_real_child_to_be_gone_even_when_it_ignores_sigterm() {
        // The child ignores SIGTERM, so teardown escalates to SIGKILL and must not
        // return until the process has exited.
        let mut child = spawn_sh("trap '' TERM; while :; do sleep 1; done");
        let pid = child.id();
        // Let the shell install its trap.
        std::thread::sleep(Duration::from_millis(300));
        let mut vmm = FirecrackerVmm::new_adopted(
            "wait-test".to_string(),
            PathBuf::from("/nonexistent/firecracker"),
            PathBuf::from("/nonexistent/jailer"),
            false,
            3,
            pid,
        );
        let start = std::time::Instant::now();
        vmm.shutdown().expect("shutdown");
        assert!(
            !jail::process_alive(pid),
            "the process is still there after shutdown returned"
        );
        // It took the SIGTERM grace and no more than the kill wait on top.
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "{:?}",
            start.elapsed()
        );
        let _ = child.wait();
    }

    #[test]
    fn root_drive_is_read_only() {
        let config = MicroVmConfigBuilder::new()
            .with_kernel(PathBuf::from("/vmlinux"))
            .with_rootfs(PathBuf::from("/rootfs.ext4"))
            .build()
            .unwrap();
        let drive = root_drive(&config);
        assert!(drive.is_root_device);
        assert!(drive.is_read_only, "shared base image must not be writable");
        assert_eq!(drive.path_on_host, PathBuf::from("/rootfs.ext4"));
    }

    #[test]
    fn test_mock_vmm_transitions() {
        let mut mock = MockVmm::new();
        mock.spawn().unwrap();

        let config = MicroVmConfigBuilder::new()
            .with_kernel(PathBuf::from("/vmlinux"))
            .with_rootfs(PathBuf::from("/rootfs.ext4"))
            .build()
            .unwrap();

        mock.configure_hardware(&config, None).unwrap();
        mock.start_instance(None).unwrap();
        mock.pause().unwrap();
        mock.resume().unwrap();
        mock.shutdown().unwrap();

        let history = mock.get_history();
        assert_eq!(
            history,
            vec![
                "spawn",
                "configure_hardware",
                "start_instance",
                "pause",
                "resume",
                "shutdown"
            ]
        );
        assert_eq!(mock.get_pid(), Some(12345));
    }

    #[test]
    fn test_mock_vmm_snapshot_restore_patch() {
        let mut mock = MockVmm::new();
        mock.spawn().unwrap();
        mock.load_snapshot(
            std::path::Path::new("/tmp/mem.bin"),
            std::path::Path::new("/tmp/state.bin"),
            false,
            Some(std::path::Path::new("/tmp/vsock.socket")),
        )
        .unwrap();
        mock.patch_data_drive_path("data0", std::path::Path::new("/tmp/drive9.ext4"))
            .unwrap();
        mock.resume().unwrap();

        let history = mock.get_history();
        assert!(history.iter().any(|h| h.starts_with("load_snapshot:")));
        assert!(history
            .iter()
            .any(|h| h.starts_with("patch_data_drive:data0:")));
        assert!(history
            .iter()
            .any(|h| h.starts_with("load_snapshot_vsock_override:")));
        assert!(!history.iter().any(|h| h == "configure_hardware"));
        assert!(!history.iter().any(|h| h == "start_instance"));
    }
}

#[cfg(test)]
mod adopt_tests {
    use super::*;

    #[test]
    fn adopted_firecracker_keeps_the_configured_jailer_flag() {
        for use_jailer in [false, true] {
            let mut via_factory = FirecrackerVmm::new(
                "vm-a".into(),
                "/configured/firecracker".into(),
                "/configured/jailer".into(),
                use_jailer,
                7,
            );
            via_factory.adopt(4242);
            let plan = via_factory.teardown_plan();
            // A jailed VM's teardown removes its jail directory, adopted or not.
            assert_eq!(plan.0, use_jailer);
            assert_eq!((plan.1, plan.2), (Some(4242), true));
            assert_eq!(via_factory.get_pid(), Some(4242));
            via_factory.detach(); // Drop must not signal pid 4242
        }
        // The unjailed adopted handle is what `new_adopted(.., false, ..)` always was.
        let mut old = FirecrackerVmm::new_adopted(
            "vm-a".into(),
            "/usr/local/bin/firecracker".into(),
            "/usr/local/bin/jailer".into(),
            false,
            7,
            4242,
        );
        let mut unjailed = FirecrackerVmm::new(
            "vm-a".into(),
            "/configured/firecracker".into(),
            "/configured/jailer".into(),
            false,
            7,
        );
        unjailed.adopt(4242);
        assert_eq!(unjailed.teardown_plan(), old.teardown_plan());
        unjailed.detach();
        old.detach();
    }

    #[test]
    fn mock_adopt_only_records() {
        let mut m = MockVmm::new();
        m.adopt(99);
        assert_eq!(m.get_pid(), Some(12345), "the mock keeps its sentinel pid");
        assert_eq!(m.get_history(), vec!["adopt:99".to_string()]);
    }
}
