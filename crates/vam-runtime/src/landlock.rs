//! In-guest Linux Landlock LSM Sandboxing for unprivileged tool processes.
//!
//! Provides filesystem subtree confinement with dynamic ABI probing and graceful degradation.

#![allow(dead_code)]

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
use std::ffi::CStr;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_LANDLOCK_CREATE_RULESET: i64 = 444;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_LANDLOCK_ADD_RULE: i64 = 445;
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const SYS_LANDLOCK_RESTRICT_SELF: i64 = 446;

const LANDLOCK_CREATE_RULESET_VERSION: u32 = 1 << 0;
const LANDLOCK_RULE_PATH_BENEATH: u32 = 1;

// Landlock FS access flags
pub const ACCESS_FS_EXECUTE: u64 = 1 << 0;
pub const ACCESS_FS_WRITE_FILE: u64 = 1 << 1;
pub const ACCESS_FS_READ_FILE: u64 = 1 << 2;
pub const ACCESS_FS_READ_DIR: u64 = 1 << 3;
pub const ACCESS_FS_REMOVE_DIR: u64 = 1 << 4;
pub const ACCESS_FS_REMOVE_FILE: u64 = 1 << 5;
pub const ACCESS_FS_MAKE_CHAR: u64 = 1 << 6;
pub const ACCESS_FS_MAKE_DIR: u64 = 1 << 7;
pub const ACCESS_FS_MAKE_REG: u64 = 1 << 8;
pub const ACCESS_FS_MAKE_SOCK: u64 = 1 << 9;
pub const ACCESS_FS_MAKE_FIFO: u64 = 1 << 10;
pub const ACCESS_FS_MAKE_BLOCK: u64 = 1 << 11;
pub const ACCESS_FS_MAKE_SYM: u64 = 1 << 12;
pub const ACCESS_FS_REFER: u64 = 1 << 13; // ABI 2 (Linux 5.19+)
pub const ACCESS_FS_TRUNCATE: u64 = 1 << 14; // ABI 3 (Linux 6.2+)
pub const ACCESS_FS_IOCTL_DEV: u64 = 1 << 15; // ABI 5 (Linux 6.10+)

pub const READ_ONLY_MASK: u64 = ACCESS_FS_EXECUTE | ACCESS_FS_READ_FILE | ACCESS_FS_READ_DIR;

#[repr(C)]
struct LandlockRulesetAttr {
    handled_access_fs: u64,
}

#[repr(C)]
struct LandlockPathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Compute the supported filesystem access mask for a given Landlock ABI version.
pub fn supported_fs_access_mask(abi: i32) -> u64 {
    if abi < 1 {
        return 0;
    }
    let mut mask = (1 << 13) - 1; // ABI 1: bits 0..12
    if abi >= 2 {
        mask |= ACCESS_FS_REFER;
    }
    if abi >= 3 {
        mask |= ACCESS_FS_TRUNCATE;
    }
    if abi >= 5 {
        mask |= ACCESS_FS_IOCTL_DEV;
    }
    mask
}

/// Probes the kernel's Landlock ABI version.
/// Returns positive ABI version integer (e.g. 1..7) or negative error code if unsupported.
pub fn probe_landlock_abi() -> i32 {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unsafe {
        let abi = libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            std::ptr::null::<LandlockRulesetAttr>(),
            0usize,
            LANDLOCK_CREATE_RULESET_VERSION,
        );
        abi as i32
    }
    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        -1
    }
}

/// Default (microVM) workspace root: the Drive9 data drive mount.
pub const DEFAULT_WORKSPACE_ROOT: &str = "/mnt/drive9/workspace";
/// Default (microVM) skills root: the read-only skills drive mount.
pub const DEFAULT_SKILLS_ROOT: &str = "/mnt/drive9/skills";

/// The paths a tool process may read or write, resolved to C strings before
/// `fork` so that the `pre_exec` closure never allocates and never reads env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LandlockPolicy {
    read_only: Vec<std::ffi::CString>,
    read_write: Vec<std::ffi::CString>,
}

impl LandlockPolicy {
    /// Rules for a workspace root (read-write) and a skills root (read-only).
    /// Everything else is the fixed base set.
    pub fn for_roots(workspace: &std::path::Path, skills: &std::path::Path) -> Self {
        use std::os::unix::ffi::OsStrExt;
        let cstr = |p: &std::path::Path| std::ffi::CString::new(p.as_os_str().as_bytes()).ok();
        let mut read_only: Vec<std::ffi::CString> =
            ["/", "/usr", "/bin", "/lib", "/lib64", "/etc", "/proc"]
                .iter()
                .filter_map(|p| std::ffi::CString::new(*p).ok())
                .collect();
        read_only.extend(cstr(skills));
        let mut read_write: Vec<std::ffi::CString> = Vec::new();
        read_write.extend(std::ffi::CString::new("/work").ok());
        read_write.extend(cstr(workspace));
        read_write.extend(std::ffi::CString::new("/tmp").ok());
        read_write.extend(std::ffi::CString::new("/dev").ok());
        Self {
            read_only,
            read_write,
        }
    }

    /// The microVM rules: the fixed `/mnt/drive9` mounts.
    pub fn default_vm() -> Self {
        Self::for_roots(
            std::path::Path::new(DEFAULT_WORKSPACE_ROOT),
            std::path::Path::new(DEFAULT_SKILLS_ROOT),
        )
    }

    /// Rules for the roots this guest is configured with (`VAM_WORKSPACE_ROOT`,
    /// `VAM_SKILLS_MOUNT`; the microVM mounts when unset). Call before `fork`.
    pub fn from_configured_roots() -> Self {
        Self::for_roots(
            &crate::primitives::workspace_root(),
            &crate::primitives::skills_mount_root(),
        )
    }

    /// Read-only rule paths, in application order.
    pub fn read_only_paths(&self) -> Vec<String> {
        self.read_only
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect()
    }

    /// Read-write rule paths, in application order.
    pub fn read_write_paths(&self) -> Vec<String> {
        self.read_write
            .iter()
            .map(|c| c.to_string_lossy().into_owned())
            .collect()
    }
}

/// Applies the Landlock LSM sandbox to the current process.
/// Grants read-only access to the policy's read-only paths and read-write
/// access to its read-write paths; everything else is denied.
/// Gracefully degrades if Landlock is unsupported on the kernel.
///
/// Safe to call from a `pre_exec` closure: it allocates nothing and reads no env.
pub fn apply_landlock_sandbox(policy: &LandlockPolicy) -> Result<bool, std::io::Error> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    unsafe {
        let abi = probe_landlock_abi();
        if abi < 1 {
            // Landlock unsupported on running kernel -> graceful degradation
            return Ok(false);
        }

        // Set PR_SET_NO_NEW_PRIVS if not already set
        if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let handled_mask = supported_fs_access_mask(abi);
        let attr = LandlockRulesetAttr {
            handled_access_fs: handled_mask,
        };

        let ruleset_fd = libc::syscall(
            SYS_LANDLOCK_CREATE_RULESET,
            &attr as *const _,
            std::mem::size_of::<LandlockRulesetAttr>(),
            0u32,
        );

        if ruleset_fd < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::ENOSYS)
                || err.raw_os_error() == Some(libc::EOPNOTSUPP)
            {
                return Ok(false);
            }
            return Err(err);
        }

        let ruleset_fd = ruleset_fd as i32;

        let add_path_rule = |c_path: &CStr, access_mask: u64| {
            let fd = libc::open(c_path.as_ptr(), libc::O_PATH | libc::O_CLOEXEC);
            if fd >= 0 {
                let path_attr = LandlockPathBeneathAttr {
                    allowed_access: access_mask & handled_mask,
                    parent_fd: fd,
                };
                libc::syscall(
                    SYS_LANDLOCK_ADD_RULE,
                    ruleset_fd,
                    LANDLOCK_RULE_PATH_BENEATH,
                    &path_attr as *const _,
                    0u32,
                );
                libc::close(fd);
            }
        };

        let read_write_mask = handled_mask;

        // 1. Read-only paths (including /proc for /proc/self/fd TOCTOU execution)
        for path in &policy.read_only {
            add_path_rule(path, READ_ONLY_MASK);
        }

        // 2. Read-write paths
        for path in &policy.read_write {
            add_path_rule(path, read_write_mask);
        }

        // 3. Restrict self
        let restrict_res = libc::syscall(SYS_LANDLOCK_RESTRICT_SELF, ruleset_fd, 0u32);
        libc::close(ruleset_fd);

        if restrict_res != 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(true)
    }

    #[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
    {
        let _ = policy;
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_supported_fs_access_mask_abi_levels() {
        assert_eq!(supported_fs_access_mask(0), 0);
        assert_eq!(supported_fs_access_mask(-1), 0);

        let abi1 = supported_fs_access_mask(1);
        assert_eq!(abi1, (1 << 13) - 1);
        assert_eq!(abi1 & ACCESS_FS_EXECUTE, ACCESS_FS_EXECUTE);
        assert_eq!(abi1 & ACCESS_FS_REFER, 0);

        let abi2 = supported_fs_access_mask(2);
        assert_eq!(abi2 & ACCESS_FS_REFER, ACCESS_FS_REFER);
        assert_eq!(abi2 & ACCESS_FS_TRUNCATE, 0);

        let abi3 = supported_fs_access_mask(3);
        assert_eq!(abi3 & ACCESS_FS_TRUNCATE, ACCESS_FS_TRUNCATE);
        assert_eq!(abi3 & ACCESS_FS_IOCTL_DEV, 0);

        let abi5 = supported_fs_access_mask(5);
        assert_eq!(abi5 & ACCESS_FS_IOCTL_DEV, ACCESS_FS_IOCTL_DEV);

        let abi7 = supported_fs_access_mask(7);
        assert_eq!(abi7 & ACCESS_FS_IOCTL_DEV, ACCESS_FS_IOCTL_DEV);
        assert_eq!(abi7 & ACCESS_FS_TRUNCATE, ACCESS_FS_TRUNCATE);
    }

    #[test]
    fn default_vm_policy_is_the_legacy_rule_list() {
        let p = LandlockPolicy::default_vm();
        assert_eq!(
            p.read_only_paths(),
            [
                "/",
                "/usr",
                "/bin",
                "/lib",
                "/lib64",
                "/etc",
                "/proc",
                "/mnt/drive9/skills"
            ]
        );
        assert_eq!(
            p.read_write_paths(),
            ["/work", "/mnt/drive9/workspace", "/tmp", "/dev"]
        );
    }

    #[test]
    fn policy_follows_the_configured_roots() {
        let ws = std::path::Path::new("/var/tmp/vam-ps/ws");
        let sk = std::path::Path::new("/var/tmp/vam-ps/skills");
        let p = LandlockPolicy::for_roots(ws, sk);
        let ro = p.read_only_paths();
        let rw = p.read_write_paths();
        assert!(ro.contains(&"/var/tmp/vam-ps/skills".to_string()));
        assert!(rw.contains(&"/var/tmp/vam-ps/ws".to_string()));
        assert!(!ro
            .iter()
            .chain(rw.iter())
            .any(|x| x.contains("/mnt/drive9")));
        // The base set is unchanged by the roots.
        assert!(ro.contains(&"/".to_string()) && rw.contains(&"/tmp".to_string()));
    }

    #[test]
    fn configured_roots_follow_vam_workspace_root() {
        let _g = crate::skills::skills_mount_test_guard();
        let prev_ws = std::env::var_os("VAM_WORKSPACE_ROOT");
        let prev_sk = std::env::var_os("VAM_SKILLS_MOUNT");
        std::env::set_var("VAM_WORKSPACE_ROOT", "/var/tmp/vam-ps-env/ws");
        std::env::set_var("VAM_SKILLS_MOUNT", "/var/tmp/vam-ps-env/skills");
        let p = LandlockPolicy::from_configured_roots();
        assert!(p
            .read_write_paths()
            .contains(&"/var/tmp/vam-ps-env/ws".to_string()));
        assert!(p
            .read_only_paths()
            .contains(&"/var/tmp/vam-ps-env/skills".to_string()));
        std::env::remove_var("VAM_WORKSPACE_ROOT");
        std::env::remove_var("VAM_SKILLS_MOUNT");
        assert_eq!(
            LandlockPolicy::from_configured_roots(),
            LandlockPolicy::default_vm()
        );
        if let Some(v) = prev_ws {
            std::env::set_var("VAM_WORKSPACE_ROOT", v);
        }
        if let Some(v) = prev_sk {
            std::env::set_var("VAM_SKILLS_MOUNT", v);
        }
    }
}
