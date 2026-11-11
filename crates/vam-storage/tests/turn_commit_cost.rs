//! What a turn commit costs on this machine, for a workspace of a given size.
//!
//! ```text
//! TMPDIR=/path/on/the/volume/you/care/about \
//!   cargo test -p vam-storage --test turn_commit_cost -- --ignored --nocapture
//! ```
//!
//! It builds an ext4 image from a generated tree (about 384 MiB, override with
//! `VAM_COST_PROBE_MIB`), then times the two halves of a microVM turn commit:
//!
//! * the host copy taken while the guest waits (`snapshot_image`: reflink where the
//!   volume has it, otherwise a sparse copy), and
//! * the extraction a stop does later (the safe reader into a manifest).
//!
//! It prints the copy method, copy ms, extraction ms and the sizes, on a
//! `turn_commit_cost:` line. The temp directory decides the answer: a volume with
//! reflinks (XFS, btrfs) copies in milliseconds, one without (ext4) takes about
//! 0.3 s per 100 MB. It needs `mke2fs`; without it the probe says so and passes.

use std::io::Write;
use std::path::Path;
use std::process::Command;
use std::time::Instant;
use vam_storage::{
    snapshot_image, CacheManager, ChunkStore, Drive9Config, Drive9ImageIo, E2fsImageIo,
    ImageLimits, ManifestManager, SnapshotLimits,
};

const MIB: u64 = 1024 * 1024;

/// Fills `root` with about `total_mib` MiB in 3 MiB files across a few directories,
/// each file different and none of it zeros (so a copy cannot skip it as a hole).
fn generate_tree(root: &Path, total_mib: u64) -> u64 {
    let mut written = 0u64;
    let mut seed = 0x9e37_79b9_7f4a_7c15u64;
    let mut n = 0;
    while written < total_mib * MIB {
        let dir = root.join(format!("workspace/d{}", n % 8));
        std::fs::create_dir_all(&dir).expect("dir");
        let size = (3 * MIB).min(total_mib * MIB - written) as usize;
        let mut bytes = vec![0u8; size];
        for chunk in bytes.chunks_mut(8) {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            let word = seed.to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        let mut file = std::fs::File::create(dir.join(format!("f{n}.bin"))).expect("file");
        file.write_all(&bytes).expect("write");
        written += size as u64;
        n += 1;
    }
    written
}

#[test]
#[ignore = "a cost probe: run it by hand or in the firecracker lane"]
fn turn_commit_cost() {
    if Command::new("mke2fs").arg("-V").output().is_err() {
        eprintln!("turn_commit_cost: mke2fs is not installed; nothing measured");
        return;
    }
    let mib: u64 = std::env::var("VAM_COST_PROBE_MIB")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(384);
    let dir = tempfile::tempdir().expect("tempdir (TMPDIR decides the volume)");
    let tree = dir.path().join("tree");
    std::fs::create_dir_all(&tree).expect("tree");
    let started = Instant::now();
    let tree_bytes = generate_tree(&tree, mib);
    let generate_ms = started.elapsed().as_millis();

    let image = dir.path().join("drive9.ext4");
    let started = Instant::now();
    E2fsImageIo::new()
        .build_image(&tree, &image, tree_bytes * 2)
        .expect("build the image");
    let build_ms = started.elapsed().as_millis();
    let image_bytes = std::fs::metadata(&image).expect("image").len();

    let copy = dir.path().join("turns-1.ext4");
    let report = snapshot_image(&image, &copy, &SnapshotLimits::default()).expect("copy");

    let layout = Drive9Config::new(dir.path().join("drive9")).layout();
    let chunks = ChunkStore::new(layout.clone());
    let manifests = ManifestManager::new(layout.clone(), chunks.clone());
    let _cache = CacheManager::new(layout, chunks, 10 * MIB).expect("cache");
    let started = Instant::now();
    let (manifest, walk) = manifests
        .build_from_image(
            "probe",
            &E2fsImageIo::new() as &dyn Drive9ImageIo,
            &copy,
            None,
            &ImageLimits::from_env(),
        )
        .expect("extract");
    let extract_ms = started.elapsed().as_millis();
    assert_eq!(manifest.files.len() as u64, walk.files);
    assert!(walk.file_bytes >= tree_bytes);

    println!(
        "turn_commit_cost: method={} copy_ms={} copied_mib={} extract_ms={} extracted_mib={} \
         tree_mib={} image_mib={} build_ms={} generate_ms={} files={}",
        report.method.as_str(),
        report.elapsed_ms,
        report.copied_mib(),
        extract_ms,
        walk.file_bytes / MIB,
        tree_bytes / MIB,
        image_bytes / MIB,
        build_ms,
        generate_ms,
        walk.files
    );
}
