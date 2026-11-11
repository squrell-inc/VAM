//! Source-level guards for the process-guest boundary. They run in every build
//! (default and `process-guest`) and read the repository as text.

use std::path::{Path, PathBuf};

fn rs_files(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}

#[test]
fn rootfs_build_scripts_never_enable_process_guest() {
    let scripts = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts");
    let mut seen = 0;
    for entry in std::fs::read_dir(&scripts).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !(name.starts_with("build-rootfs") && name.ends_with(".sh")) {
            continue;
        }
        seen += 1;
        let text = std::fs::read_to_string(&path).unwrap();
        for forbidden in ["process-guest", "--features", "--all-features"] {
            assert!(
                !text.contains(forbidden),
                "{name} must not contain {forbidden:?}: the rootfs guest is the default build"
            );
        }
        assert!(
            text.contains("--bin vam-runtime"),
            "{name} builds vam-runtime"
        );
    }
    assert!(seen >= 2, "found the rootfs scripts");
}

#[test]
fn machine_wide_actions_live_only_in_the_pid1_init() {
    // Everything that acts on the whole machine is in guest_init/pid1.rs, which
    // the process-guest feature compiles out.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    let forbidden = [
        "libc::reboot",
        "libc::kill(-1",
        "libc::umount",
        "libc::mount(",
        "libc::clock_settime",
        "\"/dev/urandom\"",
        "\"/bin/busybox\"",
        "bring_up_loopback",
    ];
    for file in files {
        if file.ends_with("guest_init/pid1.rs") {
            continue;
        }
        let text = std::fs::read_to_string(&file).unwrap();
        for f in forbidden {
            assert!(
                !text.contains(f),
                "{} contains {f:?}; machine-wide actions belong in guest_init/pid1.rs",
                file.display()
            );
        }
    }
}

#[test]
fn no_guest_code_outside_tests_binds_a_wildcard_address() {
    // The microVM proxy used to fall back to a wildcard bind. Neither build binds
    // one now: the proxy listens on loopback or its unix socket only. Test modules
    // sit at the end of each file, so everything from `#[cfg(test)]` on is skipped.
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);
    assert!(!files.is_empty());
    for file in files {
        let text = std::fs::read_to_string(&file).unwrap();
        let production = text.split("#[cfg(test)]").next().unwrap();
        for forbidden in [
            "0.0.0.0:3128",
            "\"0.0.0.0",
            "\"[::]",
            "Ipv4Addr::UNSPECIFIED",
            "Ipv6Addr::UNSPECIFIED",
        ] {
            assert!(
                !production.contains(forbidden),
                "{} names a wildcard address ({forbidden:?}); the guest proxy listens on loopback or its unix socket only",
                file.display()
            );
        }
    }
}
