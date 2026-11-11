use crate::VamCoreError;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriveAttachment {
    pub path: PathBuf,
    pub is_read_only: bool,
    /// Virtio block cache strategy. `None` defaults to Firecracker `Unsafe`.
    pub cache_type: Option<vam_firecracker::models::drive::CacheType>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MicroVmConfig {
    pub kernel_path: PathBuf,
    pub boot_args: String,
    pub rootfs_path: PathBuf,
    pub vcpu_count: u8,
    pub mem_size_mib: usize,
    pub track_dirty_pages: bool,
    pub guest_cid: u32,
    pub limit_iops: Option<u64>,
    pub data_drives: Vec<DriveAttachment>,
    pub log_path: Option<PathBuf>,
    pub metrics_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct MicroVmConfigBuilder {
    pub kernel_path: Option<PathBuf>,
    pub boot_args: Vec<String>,
    pub rootfs_path: Option<PathBuf>,
    pub vcpu_count: u8,
    pub mem_size_mib: usize,
    pub track_dirty_pages: bool,
    pub guest_cid: Option<u32>,
    pub data_drives: Vec<DriveAttachment>,
    pub log_path: Option<PathBuf>,
    pub metrics_path: Option<PathBuf>,
}

fn serial_console_enabled() -> bool {
    std::env::var("VAM_SERIAL_CONSOLE")
        .map(|v| v == "1")
        .unwrap_or(false)
}

impl Default for MicroVmConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl MicroVmConfigBuilder {
    pub fn new() -> Self {
        Self {
            kernel_path: None,
            boot_args: vec![
                "reboot=k".to_string(), // Reboot quickly on panic
                "panic=1".to_string(),  // Panic halt timer (1 sec)
                "pci=off".to_string(),  // Disable PCI scans
            ],
            rootfs_path: None,
            vcpu_count: 1,
            mem_size_mib: 512,
            track_dirty_pages: false,
            guest_cid: None,
            data_drives: Vec::new(),
            log_path: None,
            metrics_path: None,
        }
    }

    pub fn with_kernel(mut self, path: PathBuf) -> Self {
        self.kernel_path = Some(path);
        self
    }

    pub fn with_rootfs(mut self, path: PathBuf) -> Self {
        self.rootfs_path = Some(path);
        self
    }

    pub fn with_data_drive(mut self, path: PathBuf) -> Self {
        self.data_drives.push(DriveAttachment {
            path,
            is_read_only: false,
            cache_type: Some(vam_firecracker::models::drive::CacheType::WriteBack),
        });
        self
    }

    pub fn with_read_only_data_drive(mut self, path: PathBuf) -> Self {
        self.data_drives.push(DriveAttachment {
            path,
            is_read_only: true,
            cache_type: None,
        });
        self
    }

    pub fn with_data_drive_opts(mut self, path: PathBuf, is_read_only: bool) -> Self {
        self.data_drives.push(DriveAttachment {
            path,
            is_read_only,
            cache_type: if is_read_only {
                None
            } else {
                Some(vam_firecracker::models::drive::CacheType::WriteBack)
            },
        });
        self
    }

    pub fn with_limits(mut self, vcpus: u8, ram_mib: usize) -> Self {
        self.vcpu_count = vcpus;
        self.mem_size_mib = ram_mib;
        self
    }

    pub fn with_guest_cid(mut self, guest_cid: u32) -> Self {
        self.guest_cid = Some(guest_cid);
        self
    }

    pub fn with_init(mut self, init_path: &str) -> Self {
        self.boot_args.push(format!("init={}", init_path));
        self
    }

    pub fn with_boot_arg(mut self, arg: &str) -> Self {
        self.boot_args.push(arg.to_string());
        self
    }

    pub fn enable_dirty_page_tracking(mut self) -> Self {
        self.track_dirty_pages = true;
        self
    }

    pub fn with_log_path(mut self, path: PathBuf) -> Self {
        self.log_path = Some(path);
        self
    }

    pub fn with_metrics_path(mut self, path: PathBuf) -> Self {
        self.metrics_path = Some(path);
        self
    }

    pub fn build(self) -> Result<MicroVmConfig, VamCoreError> {
        let kernel = self
            .kernel_path
            .ok_or_else(|| VamCoreError::InvalidConfig("Kernel path is required".to_string()))?;
        let rootfs = self
            .rootfs_path
            .ok_or_else(|| VamCoreError::InvalidConfig("Rootfs path is required".to_string()))?;

        // 1. Path Sanitation
        if kernel.to_string_lossy().contains("..") {
            return Err(VamCoreError::InvalidConfig(format!(
                "Kernel path '{}' contains relative segments ('..')",
                kernel.display()
            )));
        }
        if rootfs.to_string_lossy().contains("..") {
            return Err(VamCoreError::InvalidConfig(format!(
                "Rootfs path '{}' contains relative segments ('..')",
                rootfs.display()
            )));
        }

        // 2. vCPU limits (1 or even number up to 32)
        if self.vcpu_count == 0 || self.vcpu_count > 32 {
            return Err(VamCoreError::InvalidConfig(format!(
                "vCPU count must be between 1 and 32, got {}",
                self.vcpu_count
            )));
        }
        if self.vcpu_count > 1 && !self.vcpu_count.is_multiple_of(2) {
            return Err(VamCoreError::InvalidConfig(format!(
                "vCPU count must be 1 or an even number, got {}",
                self.vcpu_count
            )));
        }

        // Check host thread limit
        if let Ok(cores) = std::thread::available_parallelism() {
            let host_threads = cores.get();
            if self.vcpu_count as usize > host_threads {
                return Err(VamCoreError::InvalidConfig(format!(
                    "vCPU count {} exceeds host actual threads ({})",
                    self.vcpu_count, host_threads
                )));
            }
        }

        // 3. Memory limits (at least 128 MiB)
        if self.mem_size_mib < 128 {
            return Err(VamCoreError::InvalidConfig(format!(
                "Memory must be at least 128 MiB, got {} MiB",
                self.mem_size_mib
            )));
        }

        // 4. Boot args: always-on flags + serial policy (VAM_SERIAL_CONSOLE)
        let mut boot_args = self.boot_args;
        for flag in [
            "i8042.noaux",
            "i8042.nomux",
            "i8042.nopnp",
            "i8042.dumbkbd",
            "random.trust_cpu=on",
            "tsc=reliable",
        ] {
            if !boot_args.iter().any(|arg| arg == flag) {
                boot_args.push(flag.to_string());
            }
        }

        let serial_console = serial_console_enabled();
        if serial_console {
            if !boot_args.iter().any(|arg| arg.starts_with("console=")) {
                boot_args.push("console=ttyS0".to_string());
            }
        } else {
            for flag in ["quiet", "loglevel=0", "8250.nr_uarts=0"] {
                if !boot_args.iter().any(|arg| arg == flag) {
                    boot_args.push(flag.to_string());
                }
            }
        }

        if serial_console && !boot_args.iter().any(|arg| arg.starts_with("console=")) {
            return Err(VamCoreError::InvalidConfig(
                "Console flag ('console=ttyS0') is required when VAM_SERIAL_CONSOLE=1".to_string(),
            ));
        }

        let guest_cid = self.guest_cid.unwrap_or(3);
        if guest_cid < 3 {
            return Err(VamCoreError::InvalidConfig(format!(
                "Guest CID must be at least 3, got {}",
                guest_cid
            )));
        }

        Ok(MicroVmConfig {
            kernel_path: kernel,
            boot_args: boot_args.join(" "),
            rootfs_path: rootfs,
            vcpu_count: self.vcpu_count,
            mem_size_mib: self.mem_size_mib,
            track_dirty_pages: self.track_dirty_pages,
            guest_cid,
            limit_iops: None,
            data_drives: self.data_drives,
            log_path: self.log_path,
            metrics_path: self.metrics_path,
        })
    }
}
