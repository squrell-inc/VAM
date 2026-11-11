//! Jailer layout, preflight and file staging for [`crate::FirecrackerVmm`].
//!
//! Under the jailer Firecracker runs chrooted at `<base>/<exec>/<id>/root` as an
//! unprivileged uid, so every file it opens must be inside that directory. The
//! layout is flat and identical in every jail (`/kernel`, `/rootfs.ext4`,
//! `/data0.ext4`, `/v.sock`, ...). Files get in by hard link only (same
//! filesystem, nothing is copied): a **shared** inode (kernel, rootfs, read-only
//! drives) stays owned by whoever owns it and must not be writable by the VMM; a
//! **per-VM** inode (Drive9 image, log, metrics) is chowned to the jail uid.
//!
//! Nothing here talks to Firecracker. The host paths `vamd` already uses
//! (`/tmp/vam-vsock-<id>.socket`, `_<port>` listener sockets) stay where they are:
//! the VMM links listeners into the jail and points a symlink at the jail's socket.

use std::ffi::OsStr;
use std::fs;
use std::io;
use std::os::unix::fs::{chown, symlink, FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use thiserror::Error;

pub const DEFAULT_JAIL_UID: u32 = 65400;
pub const DEFAULT_JAIL_GID: u32 = 65400;
pub const DEFAULT_CHROOT_BASE: &str = "/srv/jailer";
pub const DEFAULT_CGROUP_VERSION: u8 = 2;
pub const DEFAULT_CGROUP_ROOT: &str = "/sys/fs/cgroup";
/// Longest unix socket path (`sun_path` is 108 bytes with the NUL).
pub const SUN_PATH_MAX: usize = 107;

/// What an operator does to run without the jailer; every refusal names it.
pub const OPT_OUT_HINT: &str =
    "start vamd with --no-jailer (or VAMD_USE_JAILER=0) to run the VMM unconfined";

static RUN_DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Where the per-VM vsock sockets live on the host. `/tmp` unless a jailed node set
/// another directory (the first call wins). The host listener sockets there are
/// hard-linked into each jail, so on a jailed node this directory must be on the
/// jail base dir's filesystem: `/tmp` is often a tmpfs, so `vamd` points it at
/// `<jailer base>/run`, a root-only directory beside the jails.
pub fn set_run_dir(dir: PathBuf) {
    let _ = RUN_DIR.set(dir);
}

pub fn run_dir() -> PathBuf {
    RUN_DIR
        .get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from("/tmp"))
}

/// `<run dir>/vam-vsock-<vm id>.socket`: the path every host component dials and
/// binds `_<port>` listeners beside.
pub fn vsock_uds_path(vm_id: &str) -> PathBuf {
    run_dir().join(format!("vam-vsock-{vm_id}.socket"))
}

/// Creates the run dir, readable by root only.
pub fn prepare_run_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

/// In-jail paths (what Firecracker sees). The same in every jail.
pub const IN_JAIL_API_SOCKET: &str = "/run/firecracker.socket";
pub const IN_JAIL_KERNEL: &str = "/kernel";
pub const IN_JAIL_ROOTFS: &str = "/rootfs.ext4";
pub const IN_JAIL_VSOCK: &str = "/v.sock";
pub const IN_JAIL_LOG: &str = "/fc.log";
pub const IN_JAIL_METRICS: &str = "/fc-metrics.json";

pub fn in_jail_data_drive(index: usize) -> String {
    format!("/data{index}.ext4")
}

/// Snapshot files inside a jail: where a bake VM's Firecracker writes them and where a
/// restore stages the golden ones. The same names in every jail, so the paths a
/// snapshot records (`/rootfs.ext4`, `/data0.ext4`, `/v.sock`) are valid in the next jail.
pub const IN_JAIL_MEM: &str = "/mem.snap";
pub const IN_JAIL_STATE: &str = "/vm.state";
/// The session's own Drive9 image after a restore (the drive is PATCHed to it).
pub const IN_JAIL_DATA_SESSION: &str = "/data0-session.ext4";

extern "C" {
    fn geteuid() -> u32;
    fn kill(pid: i32, sig: i32) -> i32;
}

/// The effective uid of this process.
/// Why a binary the jailer path would run as root is not trusted, if it is not: owned by
/// neither root nor `euid` (the user running vamd), or writable by group or others.
pub fn binary_trust_problem(owner: u32, mode: u32, euid: u32) -> Option<String> {
    if owner != 0 && owner != euid {
        return Some(format!("owned by uid {owner}, mode {:04o}", mode & 0o7777));
    }
    if mode & 0o022 != 0 {
        return Some(format!(
            "owner uid {owner}, mode {:04o} is writable by group or others",
            mode & 0o7777
        ));
    }
    None
}

pub fn current_euid() -> u32 {
    // SAFETY: geteuid has no preconditions and cannot fail.
    unsafe { geteuid() }
}

/// Jailer settings of a node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JailConfig {
    pub uid: u32,
    pub gid: u32,
    /// Where the jailer puts `<exec>/<id>/root`.
    pub base_dir: PathBuf,
    pub cgroup_version: u8,
    /// Where cgroups are mounted (injectable for tests).
    pub cgroup_root: PathBuf,
}

impl Default for JailConfig {
    fn default() -> Self {
        Self {
            uid: DEFAULT_JAIL_UID,
            gid: DEFAULT_JAIL_GID,
            base_dir: PathBuf::from(DEFAULT_CHROOT_BASE),
            cgroup_version: DEFAULT_CGROUP_VERSION,
            cgroup_root: PathBuf::from(DEFAULT_CGROUP_ROOT),
        }
    }
}

/// Why a node cannot jail. Each is a reason to refuse to start.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum JailRefusal {
    #[error("the jailer needs root (vamd runs as uid {euid})")]
    NotRoot { euid: u32 },
    #[error("the jailer uid and gid must not be 0 (got {uid}:{gid})")]
    RootIds { uid: u32, gid: u32 },
    #[error("unsupported cgroup version {0} (use 1 or 2)")]
    CgroupVersion(u8),
    #[error("{what} {path} is not an executable file")]
    BinaryNotExecutable { what: &'static str, path: PathBuf },
    #[error("{what} {path} is not trustworthy: {reason}; it must be owned by root or by the user running vamd (uid {euid}) and not writable by group or others")]
    BinaryUntrusted {
        what: &'static str,
        path: PathBuf,
        reason: String,
        euid: u32,
    },
    #[error("cgroup v{version} is configured but {path} is missing")]
    CgroupMissing { version: u8, path: PathBuf },
    #[error("jailer base dir {path} is not usable: {reason}")]
    BaseDir { path: PathBuf, reason: String },
    #[error("{what} {path} is on a different filesystem than the jailer base dir {base}; files enter the jail by hard link, which cannot cross filesystems")]
    DifferentFilesystem {
        what: String,
        path: PathBuf,
        base: PathBuf,
    },
    #[error("{what} {path} {reason}")]
    AssetPermissions {
        what: String,
        path: PathBuf,
        reason: String,
    },
}

/// A preflight refusal, worded for the operator.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("jailer preflight failed: {0}; start vamd with --no-jailer (or VAMD_USE_JAILER=0) to run the VMM unconfined")]
pub struct JailPreflightError(#[from] pub JailRefusal);

/// Staging and layout failures at VM start.
#[derive(Debug, Error)]
pub enum JailError {
    #[error("invalid jail id {0:?}: use 1 to 64 characters of A-Z, a-z, 0-9 and '-'")]
    BadId(String),
    #[error("path {path} is {len} bytes; a unix socket path is at most 107")]
    PathTooLong { path: PathBuf, len: usize },
    #[error("{0}")]
    Refused(#[from] JailRefusal),
    #[error("{ctx}: {source}")]
    Io {
        ctx: String,
        #[source]
        source: io::Error,
    },
}

fn io_err(ctx: impl Into<String>) -> impl FnOnce(io::Error) -> JailError {
    let ctx = ctx.into();
    move |source| JailError::Io { ctx, source }
}

/// What the preflight checks besides the config itself.
pub struct PreflightInputs<'a> {
    pub firecracker_bin: &'a Path,
    pub jailer_bin: &'a Path,
    /// Shared inodes the VMM may read and must not write (guest kernel, rootfs).
    pub shared_assets: &'a [(&'a str, &'a Path)],
    /// Paths whose files are hard-linked into jails (Drive9 root, the temp dir that
    /// holds the vsock listener sockets).
    pub same_fs: &'a [(&'a str, &'a Path)],
}

impl JailConfig {
    pub fn preflight(&self, inputs: &PreflightInputs<'_>) -> Result<(), JailPreflightError> {
        self.preflight_as(current_euid(), inputs)
    }

    /// `preflight` with the effective uid given (tests).
    pub fn preflight_as(
        &self,
        euid: u32,
        inputs: &PreflightInputs<'_>,
    ) -> Result<(), JailPreflightError> {
        self.refusal(euid, inputs).map_err(JailPreflightError)
    }

    fn refusal(&self, euid: u32, inputs: &PreflightInputs<'_>) -> Result<(), JailRefusal> {
        if self.uid == 0 || self.gid == 0 {
            return Err(JailRefusal::RootIds {
                uid: self.uid,
                gid: self.gid,
            });
        }
        if euid != 0 {
            return Err(JailRefusal::NotRoot { euid });
        }
        for (what, path) in [
            ("jailer binary", inputs.jailer_bin),
            ("firecracker binary", inputs.firecracker_bin),
        ] {
            let ok = fs::metadata(path)
                .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false);
            if !ok {
                return Err(JailRefusal::BinaryNotExecutable {
                    what,
                    path: path.to_path_buf(),
                });
            }
            // A root vamd runs these as root: whoever can write them owns the host. The owner
            // allowed besides root is the user really running vamd (not the simulated `euid`).
            let run_euid = current_euid();
            if let Ok(m) = fs::metadata(path) {
                if let Some(reason) =
                    binary_trust_problem(m.uid(), m.permissions().mode(), run_euid)
                {
                    return Err(JailRefusal::BinaryUntrusted {
                        what,
                        path: path.to_path_buf(),
                        reason,
                        euid: run_euid,
                    });
                }
            }
        }
        self.check_cgroups()?;
        self.check_base_dir()?;
        for (what, path) in inputs.shared_assets {
            check_shared_asset(what, path, self.uid)?;
        }
        let base_dev = fs::metadata(&self.base_dir)
            .map_err(|e| JailRefusal::BaseDir {
                path: self.base_dir.clone(),
                reason: e.to_string(),
            })?
            .dev();
        for (what, path) in inputs
            .shared_assets
            .iter()
            .chain(inputs.same_fs.iter())
            .copied()
        {
            if let Some(dev) = device_of_nearest_ancestor(path) {
                if dev != base_dev {
                    return Err(JailRefusal::DifferentFilesystem {
                        what: what.to_string(),
                        path: path.to_path_buf(),
                        base: self.base_dir.clone(),
                    });
                }
            }
        }
        Ok(())
    }

    fn check_cgroups(&self) -> Result<(), JailRefusal> {
        match self.cgroup_version {
            2 => {
                let p = self.cgroup_root.join("cgroup.controllers");
                if !p.is_file() {
                    return Err(JailRefusal::CgroupMissing {
                        version: 2,
                        path: p,
                    });
                }
            }
            1 => {
                let has_cpu = ["cpu", "cpu,cpuacct"]
                    .iter()
                    .any(|d| self.cgroup_root.join(d).is_dir());
                if !has_cpu {
                    return Err(JailRefusal::CgroupMissing {
                        version: 1,
                        path: self.cgroup_root.join("cpu"),
                    });
                }
            }
            v => return Err(JailRefusal::CgroupVersion(v)),
        }
        Ok(())
    }

    fn check_base_dir(&self) -> Result<(), JailRefusal> {
        let bad = |reason: String| JailRefusal::BaseDir {
            path: self.base_dir.clone(),
            reason,
        };
        fs::create_dir_all(&self.base_dir).map_err(|e| bad(e.to_string()))?;
        let probe = self
            .base_dir
            .join(format!(".vam-preflight-{}", std::process::id()));
        fs::write(&probe, b"").map_err(|e| bad(format!("not writable: {e}")))?;
        let _ = fs::remove_file(&probe);
        Ok(())
    }

    /// `<base>/<exec file name>/<id>`: the jailer's directory for one VM.
    pub fn jail_dir(&self, exec_name: &OsStr, id: &str) -> PathBuf {
        self.base_dir.join(exec_name).join(id)
    }
}

/// The jailer accepts ids of 1 to 64 characters from `[A-Za-z0-9-]`.
pub fn validate_id(id: &str) -> Result<(), JailError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    if ok {
        Ok(())
    } else {
        Err(JailError::BadId(id.to_string()))
    }
}

pub fn check_socket_path_len(path: &Path) -> Result<(), JailError> {
    let len = path.as_os_str().len();
    if len > SUN_PATH_MAX {
        return Err(JailError::PathTooLong {
            path: path.to_path_buf(),
            len,
        });
    }
    Ok(())
}

/// A shared inode: readable by the VMM, never writable by it.
pub fn check_shared_asset(what: &str, path: &Path, jail_uid: u32) -> Result<(), JailRefusal> {
    let bad = |reason: &str| JailRefusal::AssetPermissions {
        what: what.to_string(),
        path: path.to_path_buf(),
        reason: reason.to_string(),
    };
    let meta = fs::metadata(path).map_err(|e| bad(&format!("cannot be read: {e}")))?;
    let mode = meta.permissions().mode();
    if mode & 0o004 == 0 {
        return Err(bad("must be world-readable (the jailed VMM reads it)"));
    }
    if meta.uid() == jail_uid {
        return Err(bad(
            "must not be owned by the jail uid (the VMM could rewrite it)",
        ));
    }
    if mode & 0o022 != 0 {
        return Err(bad("must not be group- or world-writable"));
    }
    Ok(())
}

fn device_of_nearest_ancestor(path: &Path) -> Option<u64> {
    let mut p = Some(path);
    while let Some(cur) = p {
        if let Ok(m) = fs::metadata(cur) {
            return Some(m.dev());
        }
        p = cur.parent();
    }
    None
}

/// The process exists and is not a zombie (an exited child nobody has waited for).
pub fn process_alive(pid: u32) -> bool {
    // SAFETY: signal 0 only checks that the pid exists.
    if unsafe { kill(pid as i32, 0) } != 0 {
        return false;
    }
    match fs::read_to_string(format!("/proc/{pid}/stat")) {
        // `pid (comm) S ...`: the state follows the last ')'.
        Ok(stat) => stat
            .rsplit(')')
            .next()
            .and_then(|rest| rest.trim_start().chars().next())
            .map(|state| state != 'Z' && state != 'X')
            .unwrap_or(true),
        Err(_) => true,
    }
}

/// The jailer's pid file: the host pid of Firecracker.
pub fn read_pid_file(jail_root: &Path, exec_name: &OsStr) -> Option<u32> {
    let file = jail_root.join(format!("{}.pid", exec_name.to_string_lossy()));
    fs::read_to_string(file).ok()?.trim().parse().ok()
}

/// True when `pid` is a process whose root directory is this jail. Needs root;
/// anything unreadable is "no", so a reused pid is never signalled.
pub fn pid_is_in_jail(pid: u32, jail_root: &Path) -> bool {
    fs::read_link(format!("/proc/{pid}/root"))
        .map(|r| r == jail_root)
        .unwrap_or(false)
}

/// Replaces whatever is at `link` with a symlink to `target`. Fails closed if
/// something re-creates `link` between the remove and the symlink.
pub fn replace_symlink(link: &Path, target: &Path) -> io::Result<()> {
    match fs::remove_file(link) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    symlink(target, link)
}

#[cfg(any(target_arch = "aarch64", target_arch = "arm"))]
const O_NOFOLLOW: i32 = 0o100000;
#[cfg(not(any(target_arch = "aarch64", target_arch = "arm")))]
const O_NOFOLLOW: i32 = 0o400000;

/// Opens `path` without following a symlink at the last component and checks it is a
/// regular file with exactly one name. Everything a jailed VMM wrote is untrusted: it
/// may have planted a symlink or an extra hard link where root is about to look.
fn open_plain_file(path: &Path) -> Result<fs::File, JailError> {
    use std::os::unix::fs::OpenOptionsExt;
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(io_err(format!(
            "open {} (a symlink is refused)",
            path.display()
        )))?;
    let meta = file
        .metadata()
        .map_err(io_err(format!("stat {}", path.display())))?;
    if !meta.is_file() {
        return Err(JailError::Io {
            ctx: format!("{} is not a regular file", path.display()),
            source: io::Error::from(io::ErrorKind::InvalidInput),
        });
    }
    if meta.nlink() != 1 {
        return Err(JailError::Io {
            ctx: format!(
                "{} has {} names; the VMM may hold another",
                path.display(),
                meta.nlink()
            ),
            source: io::Error::from(io::ErrorKind::InvalidInput),
        });
    }
    Ok(file)
}

/// Takes a file a (dead) jailed VMM wrote: opens it without following a symlink, checks
/// it is a regular file with one name, then gives it to `uid:gid` with mode 0644 through
/// the open descriptor. Returns the descriptor for the caller's next step.
pub fn reclaim_file(path: &Path, uid: u32, gid: u32) -> Result<fs::File, JailError> {
    use std::os::unix::fs::fchown;
    let file = open_plain_file(path)?;
    fchown(&file, Some(uid), Some(gid)).map_err(io_err(format!("chown {}", path.display())))?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(io_err(format!("chmod {}", path.display())))?;
    Ok(file)
}

/// Gives a per-VM file back to `uid:gid` (0644) once its VMM is gone: the bake's data
/// disk, which was chowned to the jail uid to be written.
pub fn reclaim_owned(path: &Path, uid: u32, gid: u32) -> Result<(), JailError> {
    use std::os::unix::fs::fchown;
    use std::os::unix::fs::OpenOptionsExt;
    // Not `open_plain_file`: the disk also has a name in the golden dir and one in the
    // (removed) jail, so its link count is not 1 until the jail is gone. Symlinks are
    // still refused.
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW)
        .open(path)
        .map_err(io_err(format!("open {}", path.display())))?;
    if !file
        .metadata()
        .map_err(io_err(format!("stat {}", path.display())))?
        .is_file()
    {
        return Err(JailError::Io {
            ctx: format!("{} is not a regular file", path.display()),
            source: io::Error::from(io::ErrorKind::InvalidInput),
        });
    }
    fchown(&file, Some(uid), Some(gid)).map_err(io_err(format!("chown {}", path.display())))?;
    file.set_permissions(fs::Permissions::from_mode(0o644))
        .map_err(io_err(format!("chmod {}", path.display())))
}

/// Publishes a file out of a dead jail to `dest`: [`reclaim_file`] (owner `uid:gid`,
/// mode 0644), then a hard link, or a copy when `dest` is on another filesystem. The
/// VMM that wrote it must already be dead. `dest` must not exist.
pub fn export_from_jail(src: &Path, dest: &Path, uid: u32, gid: u32) -> Result<(), JailError> {
    use std::os::unix::fs::fchown;
    let file = reclaim_file(src, uid, gid)?;
    let want = file
        .metadata()
        .map_err(io_err(format!("stat {}", src.display())))?;
    match fs::hard_link(src, dest) {
        Ok(()) => {
            // The name we linked must still be the file we checked.
            let got =
                fs::symlink_metadata(dest).map_err(io_err(format!("stat {}", dest.display())))?;
            if got.ino() != want.ino() || got.dev() != want.dev() {
                let _ = fs::remove_file(dest);
                return Err(JailError::Io {
                    ctx: format!("{} changed while it was exported", src.display()),
                    source: io::Error::from(io::ErrorKind::InvalidData),
                });
            }
            Ok(())
        }
        Err(e) if e.raw_os_error() == Some(18) => {
            use std::os::unix::fs::OpenOptionsExt;
            let mut out = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o644)
                .open(dest)
                .map_err(io_err(format!("create {}", dest.display())))?;
            let mut input = file;
            io::copy(&mut input, &mut out)
                .map_err(io_err(format!("copy to {}", dest.display())))?;
            fchown(&out, Some(uid), Some(gid))
                .map_err(io_err(format!("chown {}", dest.display())))?;
            Ok(())
        }
        Err(e) => Err(JailError::Io {
            ctx: format!("link {} out of the jail", src.display()),
            source: e,
        }),
    }
}

/// One VM's jail on the host.
#[derive(Debug, Clone)]
pub struct Jail {
    /// `<base>/<exec>/<id>/root`
    pub root: PathBuf,
    pub uid: u32,
    pub gid: u32,
}

impl Jail {
    pub fn new(root: PathBuf, uid: u32, gid: u32) -> Self {
        Self { root, uid, gid }
    }

    /// The host path of an in-jail path.
    pub fn host_path(&self, in_jail: &str) -> PathBuf {
        self.root.join(in_jail.trim_start_matches('/'))
    }

    fn link(&self, src: &Path, in_jail: &str) -> Result<PathBuf, JailError> {
        let dst = self.host_path(in_jail);
        match fs::hard_link(src, &dst) {
            Ok(()) => Ok(dst),
            Err(e) if e.raw_os_error() == Some(18) => {
                Err(JailError::Refused(JailRefusal::DifferentFilesystem {
                    what: "file".to_string(),
                    path: src.to_path_buf(),
                    base: self.root.clone(),
                }))
            }
            Err(e) => Err(JailError::Io {
                ctx: format!("link {} into the jail as {in_jail}", src.display()),
                source: e,
            }),
        }
    }

    /// Hard-links a shared inode in. It is checked first and never chowned.
    pub fn stage_shared(&self, src: &Path, in_jail: &str, what: &str) -> Result<(), JailError> {
        check_shared_asset(what, src, self.uid)?;
        self.link(src, in_jail).map(|_| ())
    }

    /// Hard-links a per-VM inode in, owned by the jail uid so the VMM can write it.
    pub fn stage_owned(&self, src: &Path, in_jail: &str) -> Result<(), JailError> {
        chown(src, Some(self.uid), Some(self.gid))
            .map_err(io_err(format!("chown {}", src.display())))?;
        self.link(src, in_jail).map(|_| ())
    }

    /// A sparse all-zero file of `len` bytes in the jail, owned by the jail uid. A
    /// restore needs one at `/data0.ext4` because the snapshot records that drive as
    /// read-write and Firecracker re-opens it at load; the real image is PATCHed in
    /// before the guest runs. It is the session's own, never a shared golden inode.
    pub fn create_placeholder_disk(&self, in_jail: &str, len: u64) -> Result<PathBuf, JailError> {
        let path = self.host_path(in_jail);
        let file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(io_err(format!("create {}", path.display())))?;
        file.set_len(len)
            .map_err(io_err(format!("size {}", path.display())))?;
        std::os::unix::fs::fchown(&file, Some(self.uid), Some(self.gid))
            .map_err(io_err(format!("chown {}", path.display())))?;
        Ok(path)
    }

    /// Removes a name from the jail.
    pub fn unlink(&self, in_jail: &str) -> Result<(), JailError> {
        let path = self.host_path(in_jail);
        fs::remove_file(&path).map_err(io_err(format!("unlink {}", path.display())))
    }

    /// A new empty file in the jail, owned by the jail uid (the VMM log, metrics).
    pub fn create_owned_file(&self, in_jail: &str) -> Result<PathBuf, JailError> {
        let path = self.host_path(in_jail);
        fs::write(&path, b"").map_err(io_err(format!("create {}", path.display())))?;
        chown(&path, Some(self.uid), Some(self.gid))
            .map_err(io_err(format!("chown {}", path.display())))?;
        Ok(path)
    }

    /// Links the host-side listener sockets `<host_uds>_<port>` into the jail as
    /// `<in_jail_uds>_<port>`, owned by the jail uid with mode 0600: the chrooted VMM
    /// and root can connect, nobody else (never 0666). Returns the ports linked.
    pub fn link_listener_sockets(
        &self,
        host_uds: &Path,
        in_jail_uds: &str,
    ) -> Result<Vec<String>, JailError> {
        let (Some(dir), Some(name)) = (host_uds.parent(), host_uds.file_name()) else {
            return Ok(Vec::new());
        };
        let prefix = format!("{}_", name.to_string_lossy());
        let mut linked = Vec::new();
        let entries = fs::read_dir(dir).map_err(io_err(format!("read {}", dir.display())))?;
        for entry in entries.flatten() {
            let fname = entry.file_name().to_string_lossy().into_owned();
            let Some(suffix) = fname.strip_prefix(&prefix) else {
                continue;
            };
            let is_socket = entry.file_type().map(|t| t.is_socket()).unwrap_or(false);
            if !is_socket {
                continue;
            }
            let src = entry.path();
            chown(&src, Some(self.uid), Some(self.gid))
                .map_err(io_err(format!("chown {}", src.display())))?;
            fs::set_permissions(&src, fs::Permissions::from_mode(0o600))
                .map_err(io_err(format!("chmod {}", src.display())))?;
            let in_jail = format!("{in_jail_uds}_{suffix}");
            let dst = self.host_path(&in_jail);
            let _ = fs::remove_file(&dst);
            self.link(&src, &in_jail)?;
            linked.push(suffix.to_string());
        }
        linked.sort();
        Ok(linked)
    }
}

/// Removes the jail directories under `<base>/<exec>` whose Firecracker is gone
/// (no pid file, or its process no longer exists), and the `/tmp` links that
/// pointed into them. Startup only: it must not run while a VM is being created.
/// Returns the ids removed.
pub fn sweep_stale_jails(base: &Path, exec_name: &OsStr, link_dirs: &[&Path]) -> Vec<String> {
    let exec_dir = base.join(exec_name);
    let Ok(entries) = fs::read_dir(&exec_dir) else {
        return Vec::new();
    };
    let mut removed = Vec::new();
    for entry in entries.flatten() {
        let Ok(ft) = entry.file_type() else { continue };
        if !ft.is_dir() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        let root = entry.path().join("root");
        let live = read_pid_file(&root, exec_name)
            .map(|pid| process_alive(pid) && pid_is_in_jail(pid, &root))
            .unwrap_or(false);
        if live {
            continue;
        }
        if fs::remove_dir_all(entry.path()).is_ok() {
            for dir in link_dirs {
                remove_host_links(dir, &id);
            }
            removed.push(id);
        }
    }
    removed.sort();
    removed
}

/// The host-side links of VM `id` under `dir` (the vsock base socket and its
/// `_<port>` sockets, the metrics and log links).
pub fn remove_host_links(dir: &Path, id: &str) {
    let vsock = format!("vam-vsock-{id}.socket");
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let ours = name == vsock
                || name.starts_with(&format!("{vsock}_"))
                || name == format!("vam-fc-metrics-{id}.json")
                || name == format!("vam-fc-log-{id}.log");
            if ours {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;

    fn me() -> (u32, u32) {
        let m = fs::metadata("/proc/self").unwrap();
        (m.uid(), m.gid())
    }

    fn exe(dir: &Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        fs::write(&p, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn v2_cgroups(dir: &Path) -> PathBuf {
        let root = dir.join("cgroup");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("cgroup.controllers"), "cpu memory").unwrap();
        root
    }

    fn cfg(dir: &Path) -> JailConfig {
        JailConfig {
            base_dir: dir.join("jail-base"),
            cgroup_root: v2_cgroups(dir),
            ..JailConfig::default()
        }
    }

    fn inputs<'a>(fc: &'a Path, jl: &'a Path) -> PreflightInputs<'a> {
        PreflightInputs {
            firecracker_bin: fc,
            jailer_bin: jl,
            shared_assets: &[],
            same_fs: &[],
        }
    }

    #[test]
    fn defaults_are_a_dedicated_unprivileged_id_and_cgroup_v2() {
        let c = JailConfig::default();
        assert_eq!((c.uid, c.gid, c.cgroup_version), (65400, 65400, 2));
        assert_eq!(c.base_dir, PathBuf::from("/srv/jailer"));
    }

    #[test]
    fn refuses_a_node_that_is_not_root_and_names_the_opt_out() {
        let d = tempfile::tempdir().unwrap();
        let (fc, jl) = (exe(d.path(), "firecracker"), exe(d.path(), "jailer"));
        let e = cfg(d.path())
            .preflight_as(1000, &inputs(&fc, &jl))
            .unwrap_err();
        assert_eq!(e.0, JailRefusal::NotRoot { euid: 1000 });
        let msg = e.to_string();
        assert!(
            msg.contains("--no-jailer") && msg.contains("VAMD_USE_JAILER=0"),
            "{msg}"
        );
    }

    #[test]
    fn binary_trust_rules() {
        assert_eq!(binary_trust_problem(0, 0o755, 1000), None, "root-owned");
        assert_eq!(
            binary_trust_problem(1000, 0o755, 1000),
            None,
            "vamd-uid-owned"
        );
        assert_eq!(binary_trust_problem(0, 0o755, 0), None);
        let other = binary_trust_problem(1001, 0o755, 1000).unwrap();
        assert!(other.contains("1001") && other.contains("0755"), "{other}");
        for mode in [0o775, 0o757, 0o777, 0o722] {
            let r = binary_trust_problem(0, mode, 0).expect("writable");
            assert!(r.contains("writable"), "{r}");
        }
    }

    #[test]
    fn refuses_a_writable_binary_in_preflight() {
        let d = tempfile::tempdir().unwrap();
        let (fc, jl) = (exe(d.path(), "firecracker"), exe(d.path(), "jailer"));
        fs::set_permissions(&jl, fs::Permissions::from_mode(0o775)).unwrap();
        let e = cfg(d.path())
            .preflight_as(0, &inputs(&fc, &jl))
            .unwrap_err();
        assert!(
            matches!(
                e.0,
                JailRefusal::BinaryUntrusted {
                    what: "jailer binary",
                    ..
                }
            ),
            "{e}"
        );
        let msg = e.to_string();
        assert!(msg.contains("jailer") && msg.contains("0775"), "{msg}");
    }

    #[test]
    fn refuses_uid_or_gid_zero() {
        let d = tempfile::tempdir().unwrap();
        let (fc, jl) = (exe(d.path(), "firecracker"), exe(d.path(), "jailer"));
        for (uid, gid) in [(0, 65400), (65400, 0)] {
            let c = JailConfig {
                uid,
                gid,
                ..cfg(d.path())
            };
            let e = c.preflight_as(0, &inputs(&fc, &jl)).unwrap_err();
            assert_eq!(e.0, JailRefusal::RootIds { uid, gid });
        }
    }

    #[test]
    fn refuses_missing_or_non_executable_binaries() {
        let d = tempfile::tempdir().unwrap();
        let fc = exe(d.path(), "firecracker");
        let missing = d.path().join("nope");
        let e = cfg(d.path())
            .preflight_as(0, &inputs(&fc, &missing))
            .unwrap_err();
        assert!(matches!(
            e.0,
            JailRefusal::BinaryNotExecutable {
                what: "jailer binary",
                ..
            }
        ));
        let plain = d.path().join("plain");
        fs::write(&plain, b"x").unwrap();
        fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
        let e = cfg(d.path())
            .preflight_as(0, &inputs(&plain, &fc))
            .unwrap_err();
        assert!(matches!(
            e.0,
            JailRefusal::BinaryNotExecutable {
                what: "firecracker binary",
                ..
            }
        ));
    }

    #[test]
    fn refuses_a_missing_cgroup_mount_for_the_configured_version() {
        let d = tempfile::tempdir().unwrap();
        let (fc, jl) = (exe(d.path(), "firecracker"), exe(d.path(), "jailer"));
        let mut c = cfg(d.path());
        c.cgroup_root = d.path().join("no-cgroups");
        let e = c.preflight_as(0, &inputs(&fc, &jl)).unwrap_err();
        assert!(matches!(e.0, JailRefusal::CgroupMissing { version: 2, .. }));
        c.cgroup_version = 1;
        let e = c.preflight_as(0, &inputs(&fc, &jl)).unwrap_err();
        assert!(matches!(e.0, JailRefusal::CgroupMissing { version: 1, .. }));
        c.cgroup_version = 3;
        let e = c.preflight_as(0, &inputs(&fc, &jl)).unwrap_err();
        assert_eq!(e.0, JailRefusal::CgroupVersion(3));
    }

    #[test]
    fn refuses_an_unusable_base_dir() {
        let d = tempfile::tempdir().unwrap();
        let (fc, jl) = (exe(d.path(), "firecracker"), exe(d.path(), "jailer"));
        let mut c = cfg(d.path());
        let blocker = d.path().join("blocker");
        fs::write(&blocker, b"x").unwrap();
        c.base_dir = blocker.join("jailer");
        let e = c.preflight_as(0, &inputs(&fc, &jl)).unwrap_err();
        assert!(matches!(e.0, JailRefusal::BaseDir { .. }), "{e}");
    }

    #[test]
    fn refuses_shared_assets_the_vmm_could_not_read_or_could_rewrite() {
        let d = tempfile::tempdir().unwrap();
        let (fc, jl) = (exe(d.path(), "firecracker"), exe(d.path(), "jailer"));
        let kernel = d.path().join("vmlinux");
        fs::write(&kernel, b"k").unwrap();
        let set = |mode| fs::set_permissions(&kernel, fs::Permissions::from_mode(mode)).unwrap();
        let run = |c: &JailConfig| {
            c.preflight_as(
                0,
                &PreflightInputs {
                    firecracker_bin: &fc,
                    jailer_bin: &jl,
                    shared_assets: &[("guest kernel", kernel.as_path())],
                    same_fs: &[],
                },
            )
        };
        let c = cfg(d.path());
        set(0o644);
        assert!(run(&c).is_ok(), "{:?}", run(&c));
        set(0o640);
        assert!(run(&c).unwrap_err().to_string().contains("world-readable"));
        set(0o666);
        assert!(run(&c).unwrap_err().to_string().contains("writable"));
        set(0o644);
        let (uid, _) = me();
        let owned = JailConfig {
            uid,
            ..cfg(d.path())
        };
        // Owned by the jail uid: refuse (skip when that is root, which uid 0 already refuses).
        if uid != 0 {
            assert!(run(&owned).unwrap_err().to_string().contains("jail uid"));
        }
    }

    #[test]
    fn id_and_socket_path_checks() {
        assert!(validate_id("vm-sess-1a2b3c").is_ok());
        assert!(validate_id("golden-bake-vm-123").is_ok());
        for bad in ["", "vm_x", "vm.x", "a/b", &"x".repeat(65)] {
            assert!(
                matches!(validate_id(bad), Err(JailError::BadId(_))),
                "{bad}"
            );
        }
        assert!(check_socket_path_len(Path::new(
            "/srv/jailer/firecracker/vm-sess-1a2b3c4d5e6f7a8b9c0d/root/run/firecracker.socket"
        ))
        .is_ok());
        let long = format!(
            "/srv/jailer/firecracker/{}/root/run/firecracker.socket",
            "x".repeat(64)
        );
        assert!(matches!(
            check_socket_path_len(Path::new(&long)),
            Err(JailError::PathTooLong { .. })
        ));
    }

    #[test]
    fn staging_links_without_copying_and_chowns_only_per_vm_files() {
        let d = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let root = d.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let jail = Jail::new(root.clone(), uid + 1, gid);
        let kernel = d.path().join("vmlinux");
        fs::write(&kernel, b"k").unwrap();
        jail.stage_shared(&kernel, IN_JAIL_KERNEL, "guest kernel")
            .unwrap();
        assert_eq!(
            fs::metadata(&kernel).unwrap().nlink(),
            2,
            "a hard link, not a copy"
        );
        assert_eq!(
            fs::metadata(&kernel).unwrap().ino(),
            fs::metadata(jail.host_path("/kernel")).unwrap().ino()
        );
        // Shared: never chowned.
        assert_eq!(fs::metadata(&kernel).unwrap().uid(), uid);

        // Per-VM: chowned to the jail uid (only root may give a file away).
        let img = d.path().join("drive9.ext4");
        fs::write(&img, b"d").unwrap();
        let owned = Jail::new(root.clone(), uid, gid);
        owned.stage_owned(&img, &in_jail_data_drive(0)).unwrap();
        assert_eq!(
            fs::metadata(&img).unwrap().ino(),
            fs::metadata(root.join("data0.ext4")).unwrap().ino()
        );
        let log = owned.create_owned_file(IN_JAIL_LOG).unwrap();
        assert_eq!(log, root.join("fc.log"));
        assert_eq!(fs::metadata(&log).unwrap().len(), 0);
    }

    #[test]
    fn a_shared_asset_the_vmm_could_write_is_not_staged() {
        let d = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let root = d.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let jail = Jail::new(root.clone(), uid, gid);
        let rootfs = d.path().join("rootfs.ext4");
        fs::write(&rootfs, b"r").unwrap();
        fs::set_permissions(&rootfs, fs::Permissions::from_mode(0o644)).unwrap();
        // Owned by the jail uid: refused.
        let e = jail
            .stage_shared(&rootfs, IN_JAIL_ROOTFS, "rootfs")
            .unwrap_err();
        assert!(e.to_string().contains("jail uid"), "{e}");
        assert!(!root.join("rootfs.ext4").exists());
    }

    #[test]
    fn listener_sockets_are_linked_in_owned_and_private() {
        let d = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let root = d.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let host_uds = d.path().join("vam-vsock-vm-a.socket");
        let _l52 = UnixListener::bind(format!("{}_52", host_uds.display())).unwrap();
        let _l55 = UnixListener::bind(format!("{}_55", host_uds.display())).unwrap();
        // A regular file with the prefix is not a socket: skipped.
        fs::write(format!("{}_junk", host_uds.display()), b"x").unwrap();
        let jail = Jail::new(root.clone(), uid, gid);
        let linked = jail
            .link_listener_sockets(&host_uds, IN_JAIL_VSOCK)
            .unwrap();
        assert_eq!(linked, vec!["52".to_string(), "55".to_string()]);
        for port in ["52", "55"] {
            let host = fs::metadata(format!("{}_{port}", host_uds.display())).unwrap();
            let jailed = fs::metadata(root.join(format!("v.sock_{port}"))).unwrap();
            assert_eq!(host.ino(), jailed.ino());
            assert_eq!(jailed.permissions().mode() & 0o777, 0o600, "never 0666");
            assert_eq!(jailed.uid(), uid);
        }
        assert!(!root.join("v.sock_junk").exists());
        // Re-linking replaces the old link (the cold fallback rebinds its listeners).
        assert_eq!(
            jail.link_listener_sockets(&host_uds, IN_JAIL_VSOCK)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn run_dir_defaults_to_tmp_and_is_root_only_once_prepared() {
        // Nothing in the lib test binary sets it: the default is unchanged.
        assert_eq!(
            vsock_uds_path("vm-a"),
            PathBuf::from("/tmp/vam-vsock-vm-a.socket")
        );
        let d = tempfile::tempdir().unwrap();
        let run = d.path().join("run");
        prepare_run_dir(&run).unwrap();
        assert_eq!(
            fs::metadata(&run).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // Idempotent.
        prepare_run_dir(&run).unwrap();
    }

    #[test]
    fn replace_symlink_replaces_and_fails_closed_on_directories() {
        let d = tempfile::tempdir().unwrap();
        let link = d.path().join("l");
        replace_symlink(&link, Path::new("/a")).unwrap();
        replace_symlink(&link, Path::new("/b")).unwrap();
        assert_eq!(fs::read_link(&link).unwrap(), PathBuf::from("/b"));
        let dir = d.path().join("dir");
        fs::create_dir(&dir).unwrap();
        assert!(replace_symlink(&dir, Path::new("/c")).is_err());
    }

    #[test]
    fn sweep_removes_dead_jails_and_their_tmp_links_but_keeps_live_ones() {
        let d = tempfile::tempdir().unwrap();
        let base = d.path().join("jailer");
        let tmp = d.path().join("tmp");
        fs::create_dir_all(&tmp).unwrap();
        let exec = OsStr::new("firecracker");
        let mk = |id: &str, pid: Option<u32>| {
            let root = base.join("firecracker").join(id).join("root");
            fs::create_dir_all(&root).unwrap();
            if let Some(p) = pid {
                fs::write(root.join("firecracker.pid"), p.to_string()).unwrap();
            }
        };
        mk("vm-dead", Some(u32::MAX / 2));
        mk("vm-nopid", None);
        // Alive, but its /proc root is not this jail: swept (a reused pid is not ours).
        mk("vm-reused", Some(std::process::id()));
        symlink("/nowhere", tmp.join("vam-vsock-vm-dead.socket")).unwrap();
        fs::write(tmp.join("vam-vsock-vm-dead.socket_52"), b"").unwrap();
        fs::write(tmp.join("vam-vsock-vm-other.socket"), b"").unwrap();
        let removed = sweep_stale_jails(&base, exec, &[tmp.as_path()]);
        assert_eq!(removed, vec!["vm-dead", "vm-nopid", "vm-reused"]);
        assert!(!tmp.join("vam-vsock-vm-dead.socket").exists());
        assert!(fs::symlink_metadata(tmp.join("vam-vsock-vm-dead.socket")).is_err());
        assert!(!tmp.join("vam-vsock-vm-dead.socket_52").exists());
        assert!(tmp.join("vam-vsock-vm-other.socket").exists());
        assert!(fs::read_dir(base.join("firecracker"))
            .unwrap()
            .next()
            .is_none());
    }

    // ---- snapshot files ------------------------------------------------------

    #[test]
    fn export_gives_the_file_to_the_owner_and_links_it_out() {
        let d = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let jail_file = d.path().join("mem.snap");
        fs::write(&jail_file, b"memory").unwrap();
        fs::set_permissions(&jail_file, fs::Permissions::from_mode(0o666)).unwrap();
        let dest = d.path().join("vm_mem.bin");
        export_from_jail(&jail_file, &dest, uid, gid).unwrap();
        let m = fs::metadata(&dest).unwrap();
        assert_eq!(
            m.permissions().mode() & 0o777,
            0o644,
            "never writable by others"
        );
        assert_eq!(m.uid(), uid);
        assert_eq!(fs::read(&dest).unwrap(), b"memory");
        assert_eq!(
            m.ino(),
            fs::metadata(&jail_file).unwrap().ino(),
            "linked, not copied"
        );
    }

    #[test]
    fn export_refuses_a_symlink_a_second_name_and_a_non_file() {
        let d = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        // A symlink a compromised VMM planted where root will look.
        let secret = d.path().join("secret");
        fs::write(&secret, b"s").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0o600)).unwrap();
        let planted = d.path().join("mem.snap");
        symlink(&secret, &planted).unwrap();
        let dest = d.path().join("out");
        assert!(export_from_jail(&planted, &dest, uid, gid).is_err());
        assert!(!dest.exists());
        assert_eq!(
            fs::metadata(&secret).unwrap().permissions().mode() & 0o777,
            0o600,
            "the target of the symlink is not touched"
        );
        // A second hard link: the VMM might still hold a name for the inode.
        let f = d.path().join("vm.state");
        fs::write(&f, b"x").unwrap();
        fs::hard_link(&f, d.path().join("alias")).unwrap();
        let e = export_from_jail(&f, &d.path().join("out2"), uid, gid).unwrap_err();
        assert!(e.to_string().contains("names"), "{e}");
        // A directory.
        let dir = d.path().join("dir");
        fs::create_dir(&dir).unwrap();
        assert!(export_from_jail(&dir, &d.path().join("out3"), uid, gid).is_err());
    }

    #[test]
    fn reclaim_owned_refuses_a_symlink_and_fixes_mode() {
        let d = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let disk = d.path().join("disk.ext4");
        fs::write(&disk, b"d").unwrap();
        fs::set_permissions(&disk, fs::Permissions::from_mode(0o666)).unwrap();
        reclaim_owned(&disk, uid, gid).unwrap();
        assert_eq!(
            fs::metadata(&disk).unwrap().permissions().mode() & 0o777,
            0o644
        );
        let link = d.path().join("l");
        symlink(&disk, &link).unwrap();
        assert!(reclaim_owned(&link, uid, gid).is_err());
    }

    #[test]
    fn the_placeholder_disk_is_sparse_owned_and_never_the_golden_inode() {
        let d = tempfile::tempdir().unwrap();
        let (uid, gid) = me();
        let root = d.path().join("root");
        fs::create_dir_all(&root).unwrap();
        let golden = d.path().join("disk.ext4");
        fs::write(&golden, b"golden").unwrap();
        let jail = Jail::new(root.clone(), uid, gid);
        let ph = jail
            .create_placeholder_disk(&in_jail_data_drive(0), 1 << 30)
            .unwrap();
        let m = fs::metadata(&ph).unwrap();
        assert_eq!(m.len(), 1 << 30);
        assert!(m.blocks() < 64, "sparse: {} blocks", m.blocks());
        assert_eq!(m.uid(), uid);
        assert_ne!(m.ino(), fs::metadata(&golden).unwrap().ino());
        // A second one with the same name is refused, not truncated into.
        assert!(jail
            .create_placeholder_disk(&in_jail_data_drive(0), 1)
            .is_err());
        jail.unlink(&in_jail_data_drive(0)).unwrap();
        assert!(!ph.exists());
        assert_eq!(fs::read(&golden).unwrap(), b"golden");
    }
}
