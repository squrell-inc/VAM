//! The jailed `FirecrackerVmm`, against the real jailer and Firecracker binaries.
//!
//! The jailer needs root, so these are `#[ignore]` and run by hand or by the lane
//! through `sudo` on the built test binary (`.github/workflows/rust.yml`).
//!
//! - `jailed_vmm_is_confined_and_torn_down` needs no KVM: it spawns the jailed VMM,
//!   configures the hardware (the API accepts it, files staged in the jail), checks the
//!   confinement from `/proc` and tears it down.
//! - `jailed_restore_stages_the_golden_files_read_only` needs no KVM either: it stages a
//!   restore into a real jail and checks layout and ownership; Firecracker then refuses
//!   the (fake) state file for its content, which shows it found the in-jail path.
//! - `jailed_vsock_registration_and_ping` boots a real microVM (KVM) under the jailer.
#![cfg(target_os = "linux")]

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use vam_core::jail::{current_euid, JailConfig};
use vam_core::{CidAllocator, FirecrackerVmm, MicroVmConfigBuilder, Vmm};

fn lane_path(name: &str) -> PathBuf {
    match std::env::var(name) {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => panic!("{name} is not set; run via scripts/e2e/firecracker-assets.sh --env-file"),
    }
}

fn require_root() {
    assert_eq!(
        current_euid(),
        0,
        "the jailer needs root: run this test binary with sudo"
    );
}

/// What the jailer promises about Firecracker `pid`: chrooted at `jail_root`, not root,
/// its own pid and mount namespaces.
fn assert_confined(pid: u32, jail_root: &Path, uid: u32) {
    let proc = PathBuf::from(format!("/proc/{pid}"));
    // The jailer pivots into a new mount namespace, so `/proc/<pid>/root` reads `/` as a
    // link; what shows the jail is that the directory it opens IS the jail's root.
    let (theirs, jail) = (
        fs::metadata(proc.join("root")).unwrap(),
        fs::metadata(jail_root).unwrap(),
    );
    assert_eq!(
        (theirs.dev(), theirs.ino()),
        (jail.dev(), jail.ino()),
        "Firecracker's root is its jail"
    );
    let status = fs::read_to_string(proc.join("status")).unwrap();
    let ids = status
        .lines()
        .find(|l| l.starts_with("Uid:"))
        .unwrap()
        .to_string();
    let fields: Vec<u32> = ids
        .split_whitespace()
        .skip(1)
        .map(|v| v.parse().unwrap())
        .collect();
    assert!(
        fields.iter().all(|u| *u == uid && *u != 0),
        "real and effective uid are the jail uid: {ids}"
    );
    for ns in ["pid", "mnt"] {
        assert_ne!(
            fs::read_link(proc.join("ns").join(ns)).unwrap(),
            fs::read_link(format!("/proc/self/ns/{ns}")).unwrap(),
            "own {ns} namespace"
        );
    }
}

fn asset(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    fs::write(&p, vec![0u8; 1 << 20]).unwrap();
    fs::set_permissions(&p, fs::Permissions::from_mode(0o644)).unwrap();
    p
}

#[test]
#[ignore = "needs root and the jailer and firecracker binaries (no KVM)"]
fn jailed_vmm_is_confined_and_torn_down() {
    require_root();
    let scratch = tempfile::Builder::new()
        .prefix("vj")
        .tempdir_in("/tmp")
        .unwrap();
    let base = scratch.path().join("jail");
    let id = format!("jailed-smoke-{}", std::process::id());
    let cfg = JailConfig {
        base_dir: base.clone(),
        ..JailConfig::default()
    };
    let (uid, gid) = (cfg.uid, cfg.gid);
    let kernel = asset(scratch.path(), "vmlinux");
    let rootfs = asset(scratch.path(), "rootfs.ext4");
    let drive9 = asset(scratch.path(), "drive9.ext4");

    let mut vmm = FirecrackerVmm::new(
        id.clone(),
        lane_path("VAM_FIRECRACKER_BIN"),
        lane_path("VAM_JAILER_BIN"),
        true,
        3,
    )
    .with_jail(cfg);
    let vsock_link = vam_core::jail::vsock_uds_path(&id);
    let metrics = PathBuf::from(format!("/tmp/vam-fc-metrics-{id}.json"));

    vmm.spawn().expect("jailed spawn");
    let pid = vmm.get_pid().expect("pid from the jailer's pid file");
    let exec = lane_path("VAM_FIRECRACKER_BIN");
    let exec_name = exec.file_name().unwrap();
    let jail_root = base.join(exec_name).join(&id).join("root");
    assert_confined(pid, &jail_root, uid);
    assert!(
        fs::symlink_metadata(&vsock_link)
            .unwrap()
            .file_type()
            .is_symlink(),
        "the host vsock path is a symlink into the jail"
    );

    let config = MicroVmConfigBuilder::new()
        .with_kernel(kernel.clone())
        .with_rootfs(rootfs.clone())
        .with_data_drive(drive9.clone())
        .with_limits(1, 128)
        .with_guest_cid(3)
        .with_metrics_path(metrics.clone())
        .build()
        .unwrap();
    vmm.configure_hardware(&config, None)
        .expect("Firecracker accepts the jailed hardware config");

    // Files entered by hard link, not copy. The shared ones keep their owner; the
    // per-VM drive is the jail uid's.
    for (host, name) in [
        (&kernel, "kernel"),
        (&rootfs, "rootfs.ext4"),
        (&drive9, "data0.ext4"),
    ] {
        assert_eq!(
            fs::metadata(host).unwrap().ino(),
            fs::metadata(jail_root.join(name)).unwrap().ino(),
            "{name} is a hard link"
        );
    }
    assert_eq!(fs::metadata(&kernel).unwrap().uid(), 0);
    assert_eq!(fs::metadata(&rootfs).unwrap().uid(), 0);
    assert_eq!(
        (
            fs::metadata(&drive9).unwrap().uid(),
            fs::metadata(&drive9).unwrap().gid()
        ),
        (uid, gid)
    );
    assert!(jail_root.join("fc-metrics.json").is_file());

    // Starting links the host's listener sockets into the jail (owned by the jail uid,
    // 0600). Whether Firecracker then boots the 1 MiB placeholder kernel does not matter.
    let listener =
        std::os::unix::net::UnixListener::bind(format!("{}_52", vsock_link.display())).unwrap();
    let _ = vmm.start_instance(None);
    let linked = fs::metadata(jail_root.join("v.sock_52")).expect("listener linked in");
    assert_eq!(
        linked.ino(),
        fs::metadata(format!("{}_52", vsock_link.display()))
            .unwrap()
            .ino()
    );
    assert_eq!(linked.permissions().mode() & 0o777, 0o600);
    assert_eq!((linked.uid(), linked.gid()), (uid, gid));
    drop(listener);

    vmm.shutdown().unwrap();
    assert!(!jail_root.exists(), "the jail directory is removed");
    assert!(
        fs::symlink_metadata(&vsock_link).is_err() && fs::symlink_metadata(&metrics).is_err(),
        "the host links are removed"
    );
    assert!(
        kernel.is_file() && rootfs.is_file() && drive9.is_file(),
        "the host files stay"
    );
    assert!(
        !Path::new(&format!("/proc/{pid}/root")).exists() || {
            // A killed process can linger as a zombie for a moment.
            !vam_core::jail::process_alive(pid)
        }
    );
}

#[test]
#[ignore = "needs root and the jailer and firecracker binaries (no KVM)"]
fn jailed_restore_stages_the_golden_files_read_only() {
    require_root();
    let scratch = tempfile::Builder::new()
        .prefix("vr")
        .tempdir_in("/tmp")
        .unwrap();
    let base = scratch.path().join("jail");
    let id = format!("jailed-restore-{}", std::process::id());
    let cfg = JailConfig {
        base_dir: base.clone(),
        ..JailConfig::default()
    };
    let (uid, gid) = (cfg.uid, cfg.gid);
    let mem = asset(scratch.path(), "vm_mem.bin");
    let state = asset(scratch.path(), "vm_state.bin");
    let rootfs = asset(scratch.path(), "rootfs.ext4");
    let golden_disk = asset(scratch.path(), "disk.ext4");
    let session = asset(scratch.path(), "session.ext4");
    let golden_len = 1u64 << 30;
    let fc = lane_path("VAM_FIRECRACKER_BIN");

    let mut vmm = FirecrackerVmm::new(id.clone(), fc.clone(), lane_path("VAM_JAILER_BIN"), true, 3)
        .with_jail(cfg);
    vmm.spawn().expect("jailed spawn");
    let jail_root = base.join(fc.file_name().unwrap()).join(&id).join("root");

    vmm.stage_snapshot_restore(&mem, &state, &rootfs, golden_len)
        .expect("stage");
    let ino = |p: &Path| fs::metadata(p).unwrap().ino();
    for (host, name) in [
        (&mem, "mem.snap"),
        (&state, "vm.state"),
        (&rootfs, "rootfs.ext4"),
    ] {
        let m = fs::metadata(jail_root.join(name)).unwrap();
        assert_eq!(
            ino(host),
            m.ino(),
            "{name} is a hard link of the golden file"
        );
        assert_eq!(
            m.uid(),
            0,
            "{name}: read-only to the VMM, never the jail uid's"
        );
        assert_eq!(
            m.permissions().mode() & 0o022,
            0,
            "{name} not writable by others"
        );
    }
    let stand_in = fs::metadata(jail_root.join("data0.ext4")).unwrap();
    assert_eq!(stand_in.len(), golden_len);
    assert_eq!((stand_in.uid(), stand_in.gid()), (uid, gid));
    assert_ne!(
        stand_in.ino(),
        ino(&golden_disk),
        "the golden disk never enters a session jail"
    );
    assert!(stand_in.blocks() < 64, "sparse");

    // The state file is fake: Firecracker must refuse it for its content (it found
    // `/vm.state` in the jail), and the load fails before any guest could run.
    let err = vmm
        .load_snapshot(
            &mem,
            &state,
            false,
            Some(&vam_core::jail::vsock_uds_path(&id)),
        )
        .expect_err("a fake snapshot cannot load");
    let msg = err.to_string();
    eprintln!("load_snapshot of a fake state file: {msg}");
    assert!(
        !msg.contains("No such file") && !msg.contains("os error 2"),
        "the in-jail path was found: {msg}"
    );

    // Patching in the session image: its own name, owned by the jail uid, and the
    // stand-in goes. (The API call itself may fail without a loaded VM; the staging
    // and the unlink happen around it only on success, so only check what is certain.)
    let _ = vmm.patch_data_drive_path("data0", &session);
    assert_eq!(
        fs::metadata(&session).unwrap().uid(),
        uid,
        "the session image is the VMM's to write"
    );
    assert!(jail_root.join("data0-session.ext4").exists());

    vmm.shutdown().unwrap();
    assert!(!jail_root.exists());
    assert!(
        mem.is_file() && state.is_file() && golden_disk.is_file(),
        "the golden files stay"
    );
}

#[tokio::test]
#[ignore = "needs root and KVM: firecracker lane"]
async fn jailed_vsock_registration_and_ping() {
    require_root();
    let id = format!("jailed-vm-{}", std::process::id());
    let firecracker_bin = lane_path("VAM_FIRECRACKER_BIN");
    let jailer_bin = lane_path("VAM_JAILER_BIN");
    let kernel_path = lane_path("VAM_KERNEL_PATH");
    let rootfs_path = lane_path("VAM_ROOTFS_PATH");
    let cfg = JailConfig::default();
    let uid = cfg.uid;
    let exec_name = firecracker_bin.file_name().unwrap().to_owned();
    let jail_root = cfg.base_dir.join(&exec_name).join(&id).join("root");

    // The host's listener sockets are hard-linked into the jail, so they live beside the
    // jails (`/tmp` is a tmpfs on hosted runners), as `vamd` does.
    let run_dir = cfg.base_dir.join("run");
    vam_core::jail::prepare_run_dir(&run_dir).unwrap();
    vam_core::jail::set_run_dir(run_dir);
    let vsock_uds_path = vam_core::jail::vsock_uds_path(&id);
    let cid_allocator = CidAllocator::default();
    let guest_cid = cid_allocator.allocate().unwrap();

    // The host binds its listener BEFORE the VM boots, as the session manager does.
    let host_listener = vam_vsock::HostListener::bind(&vsock_uds_path, 52)
        .await
        .unwrap();

    let mut vmm = FirecrackerVmm::new(id.clone(), firecracker_bin, jailer_bin, true, guest_cid);
    let config = MicroVmConfigBuilder::new()
        .with_kernel(kernel_path)
        .with_rootfs(rootfs_path)
        .with_limits(2, 512)
        .with_guest_cid(guest_cid)
        .with_boot_arg(&format!("instance_id={id}"))
        .build()
        .unwrap();

    let start_time = std::time::Instant::now();
    vmm.spawn().unwrap();
    vmm.configure_hardware(&config, None).unwrap();
    vmm.start_instance(None).unwrap();
    assert_confined(vmm.get_pid().unwrap(), &jail_root, uid);

    let challenge_registry = vam_vsock::ChallengeRegistry::new();
    let verifier = vam_vsock::DevChallengeOnlyVerifier;
    let reg_future = async {
        let stream = host_listener.accept().await.unwrap();
        vam_vsock::handle_registration_handshake(
            stream,
            &id,
            guest_cid,
            &challenge_registry,
            &verifier,
        )
        .await
    };
    let reg_res = tokio::time::timeout(std::time::Duration::from_secs(15), reg_future).await;
    assert!(reg_res.is_ok(), "Registration timed out under the jailer");
    reg_res.unwrap().unwrap();
    println!(
        "JAILED_COLD_BOOT_TO_REGISTERED_LATENCY: {:?}",
        start_time.elapsed()
    );

    // The host dials the guest through the symlink into the jail.
    let mut guest_conn = None;
    for _ in 0..30 {
        if let Ok(stream) = vam_vsock::host_dial(&vsock_uds_path, 52).await {
            guest_conn = Some(stream);
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let guest_conn = guest_conn.expect("dial the guest through the jail");
    let rtt = vam_vsock::host_ping(guest_conn, 1, &id, std::time::Duration::from_secs(1))
        .await
        .unwrap();
    println!("JAILED_GUEST_RPC_PING_RTT: {rtt:?}");
    assert!(rtt > std::time::Duration::from_secs(0));

    vmm.shutdown().unwrap();
    assert!(!jail_root.exists(), "the jail directory is removed");
    cid_allocator.release(guest_cid);
}
