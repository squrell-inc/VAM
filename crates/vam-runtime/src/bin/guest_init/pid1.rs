//! PID-1 init for the microVM guest (the default build).
//!
//! Everything here acts on the whole machine: mounts, `ifconfig lo`,
//! `kill(-1)`, `reboot(2)`, `umount`, `clock_settime` and `/dev/urandom` writes.
//! It is the only init the rootfs guest has. The functions below were moved
//! verbatim from `vam-runtime.rs`; only visibility changed. The `process-guest`
//! feature compiles this file out and uses `process.rs` instead.

#[cfg(target_os = "linux")]
use std::io;
#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(target_os = "linux")]
use tokio::signal::unix::{signal, SignalKind};
#[cfg(target_os = "linux")]
use tokio::time::sleep;

/// Which init this binary was built with.
#[allow(dead_code)]
pub const MODE: &str = "pid1";

#[cfg(target_os = "linux")]
fn mount_one(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: *const libc::c_void,
) -> Result<(), String> {
    let src_bytes = format!("{}\0", source);
    let tgt_bytes = format!("{}\0", target);
    let type_bytes = format!("{}\0", fstype);
    unsafe {
        if libc::mount(
            src_bytes.as_ptr() as *const libc::c_char,
            tgt_bytes.as_ptr() as *const libc::c_char,
            type_bytes.as_ptr() as *const libc::c_char,
            flags,
            data,
        ) != 0
        {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EBUSY) {
                return Err(format!("Failed to mount {} on {}: {}", fstype, target, err));
            }
        }
    }
    Ok(())
}

/// True when the kernel command line carries `vam_golden_bake=1`, which the
/// orchestrator sets only on the golden snapshot bake VM.
fn cmdline_has_golden_bake(cmdline: &str) -> bool {
    cmdline
        .split_whitespace()
        .any(|part| part == "vam_golden_bake=1")
}

/// True when the kernel command line carries `vam_test_hold_proxy_port=1`. The
/// host adds it only on a test node (mock scripts loaded); PID 1 then occupies
/// `127.0.0.1:3128` before the egress proxy starts, so the proxy has to cope
/// with a busy loopback port. It only removes a loopback listener.
fn cmdline_has_hold_proxy_port(cmdline: &str) -> bool {
    cmdline
        .split_whitespace()
        .any(|part| part == "vam_test_hold_proxy_port=1")
}

/// The test listener, kept open for the life of PID 1 and never accepted from.
#[cfg(target_os = "linux")]
static PROXY_PORT_HOLDER: std::sync::OnceLock<std::net::TcpListener> = std::sync::OnceLock::new();

/// Occupies `127.0.0.1:3128` when the command line asks for it (test nodes).
#[cfg(target_os = "linux")]
fn hold_proxy_port_if_requested() {
    let wanted = std::fs::read_to_string("/proc/cmdline")
        .map(|c| cmdline_has_hold_proxy_port(&c))
        .unwrap_or(false);
    if !wanted {
        return;
    }
    match std::net::TcpListener::bind("127.0.0.1:3128") {
        Ok(listener) => {
            eprintln!("[guest-agent] vam_test_hold_proxy_port=1: holding 127.0.0.1:3128");
            let _ = PROXY_PORT_HOLDER.set(listener);
        }
        Err(e) => eprintln!("[guest-agent] could not hold 127.0.0.1:3128: {e}"),
    }
}

#[cfg(target_os = "linux")]
pub fn mount_pseudo_filesystems() -> Result<(), String> {
    use std::fs;
    let _ = fs::create_dir_all("/proc");
    let _ = fs::create_dir_all("/sys");
    let _ = fs::create_dir_all("/dev");
    let _ = fs::create_dir_all("/tmp");
    let _ = fs::create_dir_all("/work");
    // Mount points must already exist in the (read-only) rootfs; these are best-effort.
    let _ = fs::create_dir_all("/home/guest");

    mount_one("proc", "/proc", "proc", 0, std::ptr::null())?;
    mount_one("sysfs", "/sys", "sysfs", 0, std::ptr::null())?;
    mount_one("devtmpfs", "/dev", "devtmpfs", 0, std::ptr::null())?;

    let _ = fs::create_dir_all("/dev/pts");
    let _ = mount_one(
        "devpts",
        "/dev/pts",
        "devpts",
        0,
        c"gid=5,mode=620".as_ptr() as *const libc::c_void,
    );

    mount_one(
        "tmpfs",
        "/tmp",
        "tmpfs",
        0,
        c"mode=1777".as_ptr() as *const libc::c_void,
    )?;
    mount_one("tmpfs", "/work", "tmpfs", 0, std::ptr::null())?;
    // The root drive is read-only; the guest home must be its own tmpfs.
    mount_one(
        "tmpfs",
        "/home/guest",
        "tmpfs",
        0,
        c"mode=755".as_ptr() as *const libc::c_void,
    )?;

    // The golden bake VM never mounts the data drives: its filesystem state would
    // live in the snapshot's page cache and could be written into the session
    // image that is patched in on restore. Restore mounts `/dev/vdb` itself.
    let golden_bake = std::fs::read_to_string("/proc/cmdline")
        .map(|c| cmdline_has_golden_bake(&c))
        .unwrap_or(false);
    let _ = fs::create_dir_all("/mnt/drive9");
    if golden_bake {
        eprintln!("[guest-agent] vam_golden_bake=1: skipping Drive9 data drive mounts");
    } else {
        if std::path::Path::new("/dev/vdb").exists() {
            mount_one("/dev/vdb", "/mnt/drive9", "ext4", 0, std::ptr::null())?;
            let _ = fs::create_dir_all("/mnt/drive9/workspace");
            let _ = fs::create_dir_all("/mnt/drive9/tools");
            let _ = fs::create_dir_all("/mnt/drive9/skills");
            let _ = fs::create_dir_all("/mnt/drive9/checkpoints");
        }

        if std::path::Path::new("/dev/vdc").exists() {
            let _ = fs::create_dir_all("/mnt/drive9/skills");
            mount_one(
                "/dev/vdc",
                "/mnt/drive9/skills",
                "ext4",
                libc::MS_RDONLY,
                std::ptr::null(),
            )?;
        }
    }

    // Create guest unprivileged home and workspace inside mounts
    unsafe {
        // chown -R 1000:100 (guest:guest) for writable locations only
        libc::chown(c"/work".as_ptr(), 1000, 100);
        libc::chown(c"/home/guest".as_ptr(), 1000, 100);
        if !golden_bake && std::path::Path::new("/mnt/drive9").exists() {
            libc::chown(c"/mnt/drive9".as_ptr(), 1000, 100);
            libc::chown(c"/mnt/drive9/workspace".as_ptr(), 1000, 100);
            libc::chown(c"/mnt/drive9/tools".as_ptr(), 1000, 100);
            // NOTE: /mnt/drive9/skills is intentionally NOT chowned to 1000:100 (kept root-owned / read-only)
            libc::chown(c"/mnt/drive9/checkpoints".as_ptr(), 1000, 100);
        }
    }

    // Flush the mount-time mkdir/chown to the data drive (harmless when nothing
    // was mounted, e.g. in the golden bake VM).
    #[cfg(target_os = "linux")]
    unsafe {
        libc::sync();
    }

    eprintln!("[guest-agent] Pseudo filesystems mounted successfully.");
    if let Err(e) = bring_up_loopback() {
        eprintln!("[guest-agent] Failed to bring up loopback: {}", e);
    }
    Ok(())
}

/// Firecracker guests start with `lo` down. Binding 127.0.0.1:3128 then fails and
/// tool processes get ECONNREFUSED when talking to the in-guest egress proxy.
#[cfg(target_os = "linux")]
fn bring_up_loopback() -> Result<(), String> {
    let attempts: &[(&str, &[&str])] = &[
        (
            "/bin/busybox",
            &["ifconfig", "lo", "127.0.0.1", "netmask", "255.0.0.0", "up"],
        ),
        ("/bin/busybox", &["ip", "link", "set", "lo", "up"]),
        ("/sbin/ifconfig", &["lo", "127.0.0.1", "up"]),
        ("/bin/ifconfig", &["lo", "127.0.0.1", "up"]),
    ];
    let mut last_err = "no ifconfig".to_string();
    for (bin, args) in attempts {
        match std::process::Command::new(bin).args(*args).output() {
            Ok(out) if out.status.success() => {
                eprintln!(
                    "[guest-agent] lo up via {bin} {:?}: {}",
                    args,
                    String::from_utf8_lossy(&out.stdout)
                );
                last_err.clear();
                break;
            }
            Ok(out) => {
                last_err = format!(
                    "{bin} {:?}: status={} stderr={}",
                    args,
                    out.status,
                    String::from_utf8_lossy(&out.stderr)
                );
            }
            Err(e) => last_err = format!("{bin}: {e}"),
        }
    }
    if !last_err.is_empty() {
        // ioctl fallback
        unsafe {
            let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
            if fd < 0 {
                return Err(format!(
                    "{last_err}; socket: {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut ifr: libc::ifreq = std::mem::zeroed();
            let name = b"lo";
            for (i, b) in name.iter().enumerate() {
                ifr.ifr_name[i] = *b as libc::c_char;
            }
            if libc::ioctl(fd, libc::SIOCGIFFLAGS as _, &mut ifr) != 0 {
                let err = std::io::Error::last_os_error().to_string();
                libc::close(fd);
                return Err(format!("{last_err}; SIOCGIFFLAGS: {err}"));
            }
            ifr.ifr_ifru.ifru_flags |=
                libc::IFF_UP as libc::c_short | libc::IFF_RUNNING as libc::c_short;
            if libc::ioctl(fd, libc::SIOCSIFFLAGS as _, &ifr) != 0 {
                let err = std::io::Error::last_os_error().to_string();
                libc::close(fd);
                return Err(format!("{last_err}; SIOCSIFFLAGS: {err}"));
            }
            libc::close(fd);
        }
    }
    eprintln!("[guest-agent] Loopback interface lo is UP.");
    Ok(())
}

/// Dynamic Waitpid child zombie process reaper loop
#[cfg(target_os = "linux")]
#[allow(dead_code)]
fn start_sigchld_reaper() {
    tokio::spawn(async {
        let mut sig = match signal(SignalKind::child()) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[guest-agent] Failed to register SIGCHLD signal: {}", e);
                return;
            }
        };

        loop {
            sig.recv().await;
            loop {
                let mut status = 0;
                let pid = unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) };
                if pid > 0 {
                    eprintln!("[guest-agent] Reclaimed zombie child process PID {}", pid);
                } else {
                    break;
                }
            }
        }
    });
    eprintln!("[guest-agent] SIGCHLD-driven zombie process reaper loop started.");
}

/// Graceful signal handling (SIGTERM / SIGINT)
#[cfg(target_os = "linux")]
pub fn register_graceful_signals() {
    tokio::spawn(async {
        let mut sigterm = signal(SignalKind::terminate()).unwrap();
        let mut sigint = signal(SignalKind::interrupt()).unwrap();

        tokio::select! {
            _ = sigterm.recv() => {
                eprintln!("[guest-agent] Intercepted SIGTERM.");
            }
            _ = sigint.recv() => {
                eprintln!("[guest-agent] Intercepted SIGINT.");
            }
        }

        // Broadcast SIGTERM to remaining child processes
        eprintln!("[guest-agent] Sending SIGTERM to all child processes...");
        unsafe {
            libc::kill(-1, libc::SIGTERM);
        }
        sleep(Duration::from_millis(500)).await;

        flush_drive9();
        halt("Graceful signal-initiated shutdown complete".to_string());
    });
    eprintln!("[guest-agent] Graceful signal listeners registered.");
}

#[cfg(target_os = "linux")]
pub fn get_local_cid() -> u32 {
    // Prefer the CID the host passed on the kernel command line. Inside some
    // Firecracker guests, IOCTL_VM_SOCKETS_GET_LOCAL_CID always reports 3.
    if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
        for part in cmdline.split_whitespace() {
            if let Some(value) = part.strip_prefix("cid=") {
                if let Ok(cid) = value.parse::<u32>() {
                    eprintln!("[guest-agent] Retrieved local CID from cmdline: {}", cid);
                    return cid;
                }
            }
        }
    }

    use std::os::unix::io::AsRawFd;
    if let Ok(file) = std::fs::File::open("/dev/vsock") {
        let mut cid: u32 = 0;
        let res = unsafe { libc::ioctl(file.as_raw_fd(), 0x0000070b, &mut cid) };
        if res >= 0 && cid >= 3 {
            eprintln!(
                "[guest-agent] Retrieved local CID from /dev/vsock ioctl: {}",
                cid
            );
            return cid;
        }
    }

    eprintln!("[guest-agent] Failed to retrieve CID; falling back to default 3");
    3
}

pub fn get_instance_id() -> String {
    #[cfg(target_os = "linux")]
    {
        if let Ok(cmdline) = std::fs::read_to_string("/proc/cmdline") {
            for part in cmdline.split_whitespace() {
                if let Some(rest) = part.strip_prefix("instance_id=") {
                    return rest.to_string();
                }
            }
        }
    }
    "integration-test-vm".to_string()
}

/// Sync and unmount Drive9 so virtio WriteBack blocks reach the host image.
#[cfg(target_os = "linux")]
fn umount_path(path: &str) {
    let path_c = format!("{}\0", path);
    unsafe {
        if libc::umount(path_c.as_ptr() as *const libc::c_char) != 0 {
            let err = std::io::Error::last_os_error();
            let code = err.raw_os_error();
            // EINVAL/ENOENT: path not mounted (tests, partial boot).
            if code != Some(libc::EINVAL) && code != Some(libc::ENOENT) {
                eprintln!("[guest-agent] umount {} failed: {}", path, err);
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn umount_path(_path: &str) {}

/// `_IO(0x12, 97)` from `<linux/fs.h>`.
#[cfg(target_os = "linux")]
const BLKFLSBUF: u32 = 0x1261;

/// Issues a block-device ioctl (no argument) on `dev`.
#[cfg(target_os = "linux")]
fn blk_ioctl(dev: &str, request: u32) -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(dev)?;
    let rc = unsafe { libc::ioctl(file.as_raw_fd(), request as _, 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// After a golden restore `/dev/vdb` is the session image, patched in under a
/// guest that never mounted the bake disk (see `vam_golden_bake=1` in
/// `mount_pseudo_filesystems`). The umounts are no-ops in that case; they only
/// matter if a cold-booted session gets here. `BLKFLSBUF` then drops any cached
/// blocks of the previous backing file before the new mount.
#[cfg(target_os = "linux")]
pub fn remount_drive9_after_snapshot() {
    eprintln!("[guest-agent] Re-mounting Drive9 after snapshot restore...");
    umount_path("/mnt/drive9/skills");
    umount_path("/mnt/drive9");
    if std::path::Path::new("/dev/vdb").exists() {
        if let Err(e) = blk_ioctl("/dev/vdb", BLKFLSBUF) {
            eprintln!("[guest-agent] BLKFLSBUF /dev/vdb failed: {}", e);
        }
    }
    if std::path::Path::new("/dev/vdb").exists() {
        let _ = mount_one("/dev/vdb", "/mnt/drive9", "ext4", 0, std::ptr::null());
        let _ = std::fs::create_dir_all("/mnt/drive9/workspace");
        let _ = std::fs::create_dir_all("/mnt/drive9/tools");
        let _ = std::fs::create_dir_all("/mnt/drive9/skills");
        let _ = std::fs::create_dir_all("/mnt/drive9/checkpoints");
        unsafe {
            libc::chown(c"/mnt/drive9".as_ptr(), 1000, 100);
            libc::chown(c"/mnt/drive9/workspace".as_ptr(), 1000, 100);
            libc::chown(c"/mnt/drive9/tools".as_ptr(), 1000, 100);
            libc::chown(c"/mnt/drive9/checkpoints".as_ptr(), 1000, 100);
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn remount_drive9_after_snapshot() {}

#[cfg(target_os = "linux")]
pub fn write_entropy_to_urandom(entropy: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .open("/dev/urandom")
        .map_err(|e| format!("open /dev/urandom: {}", e))?;
    f.write_all(entropy)
        .map_err(|e| format!("write /dev/urandom: {}", e))
}

#[cfg(not(target_os = "linux"))]
pub fn write_entropy_to_urandom(_entropy: &[u8]) -> Result<(), String> {
    Ok(())
}

#[cfg(target_os = "linux")]
pub fn set_guest_clock_unix_nanos(nanos: i64) -> Result<(), String> {
    let secs = nanos / 1_000_000_000;
    let nsec = nanos % 1_000_000_000;
    let mut ts = libc::timespec {
        tv_sec: secs,
        tv_nsec: nsec,
    };
    unsafe {
        if libc::clock_settime(libc::CLOCK_REALTIME, &ts) != 0 {
            return Err(format!(
                "clock_settime failed: {}",
                std::io::Error::last_os_error()
            ));
        }
    }
    let _ = &mut ts;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn set_guest_clock_unix_nanos(_nanos: i64) -> Result<(), String> {
    Ok(())
}

/// Flush guest page cache and unmount `/mnt/drive9` before poweroff.
pub fn flush_drive9() {
    eprintln!("[guest-agent] Flushing Drive9 (sync + umount)...");
    #[cfg(target_os = "linux")]
    unsafe {
        libc::sync();
    }
    umount_path("/mnt/drive9/skills");
    umount_path("/mnt/drive9");
}

/// Structural NEVER-RETURN halt. Enforces RI-1 by construction.
pub fn halt(reason: String) -> ! {
    eprintln!("[guest-agent] SYSTEM HALTING. Reason: {}", reason);
    flush_drive9();
    #[cfg(target_os = "linux")]
    unsafe {
        libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
    }
    std::process::exit(0);
}

/// Mounts, signal handling: what the guest does first, as PID 1.
pub fn startup() {
    #[cfg(target_os = "linux")]
    {
        if let Err(e) = mount_pseudo_filesystems() {
            halt(format!("Filesystem mounting failed: {}", e));
        }
        // start_sigchld_reaper(); // Disabled to prevent stealing child exit codes from Tokio
        register_graceful_signals();
    }
}

/// True in the golden bake VM only (`vam_golden_bake=1` on the kernel command line).
pub fn golden_bake() -> bool {
    std::fs::read_to_string("/proc/cmdline")
        .map(|c| cmdline_has_golden_bake(&c))
        .unwrap_or(false)
}

pub fn instance_id() -> String {
    get_instance_id()
}

pub fn local_cid() -> u32 {
    #[cfg(target_os = "linux")]
    {
        get_local_cid()
    }
    #[cfg(not(target_os = "linux"))]
    {
        3
    }
}

/// The in-guest egress proxy always runs in the microVM.
pub fn egress_enabled() -> bool {
    true
}

/// Brings `lo` up, then serves the proxy on `127.0.0.1:3128` over `stream`.
pub async fn start_egress_proxy<S>(stream: S) -> Result<(), String>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static,
{
    #[cfg(target_os = "linux")]
    if let Err(e) = bring_up_loopback() {
        eprintln!("[guest-agent] loopback up before proxy failed: {}", e);
    }
    #[cfg(target_os = "linux")]
    hold_proxy_port_if_requested();
    vam_runtime::start_guest_proxy("127.0.0.1:3128", stream)
        .await
        .map(|bound| {
            eprintln!(
                "[guest-agent] egress proxy tcp={:?} unix={:?}",
                bound.tcp, bound.unix
            );
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn block_ioctl_number_matches_linux_fs_h() {
        assert_eq!(BLKFLSBUF, (0x12 << 8) | 97);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn block_ioctl_on_non_block_file_errors_without_panic() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("not-a-disk");
        std::fs::write(&file, b"x").unwrap();
        assert!(blk_ioctl(file.to_str().unwrap(), BLKFLSBUF).is_err());
        assert!(blk_ioctl("/nonexistent/vdb", BLKFLSBUF).is_err());
    }

    #[test]
    fn golden_bake_cmdline_flag_is_matched_exactly() {
        assert!(cmdline_has_golden_bake(
            "console=ttyS0 instance_id=vm cid=3 vam_golden_bake=1 clocksource=kvm-clock\n"
        ));
        assert!(!cmdline_has_golden_bake("instance_id=vm cid=3"));
        assert!(!cmdline_has_golden_bake("vam_golden_bake=0"));
        assert!(!cmdline_has_golden_bake(
            "xvam_golden_bake=1 vam_golden_bake=10"
        ));
        assert!(!cmdline_has_golden_bake(""));
    }

    #[test]
    fn hold_proxy_port_cmdline_flag_is_matched_exactly() {
        assert!(cmdline_has_hold_proxy_port(
            "instance_id=vm cid=3 vam_test_hold_proxy_port=1"
        ));
        assert!(!cmdline_has_hold_proxy_port("instance_id=vm cid=3"));
        assert!(!cmdline_has_hold_proxy_port("vam_test_hold_proxy_port=0"));
        assert!(!cmdline_has_hold_proxy_port(
            "xvam_test_hold_proxy_port=1 vam_test_hold_proxy_port=10"
        ));
        assert!(!cmdline_has_hold_proxy_port(""));
    }

    #[test]
    fn flush_drive9_tolerates_unmounted_paths() {
        // No /mnt/drive9 mount in unit tests; should not panic (EINVAL/ENOENT ignored).
        flush_drive9();
    }
}
