//! Hostile-image tests. Images come from the real `mke2fs`, built the way the
//! node builds them (`-O ^has_journal`, ext2-style block maps) and with
//! extents (`-t ext4`); hostile ones are made by patching the bytes of a valid
//! image. Tests skip when `mke2fs` is missing.

use super::*;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::process::Command;
use tempfile::{tempdir, TempDir};

fn have_mke2fs() -> bool {
    Command::new("mke2fs")
        .arg("-V")
        .output()
        .map(|o| o.status.success() || !o.stderr.is_empty())
        .unwrap_or(false)
}

macro_rules! need_mke2fs {
    () => {
        if !have_mke2fs() {
            eprintln!("skipping: mke2fs is not installed");
            return;
        }
    };
}

#[derive(Clone, Copy, Debug)]
enum Style {
    /// As the node builds images: no journal, block maps.
    BlockMapped,
    /// With extents.
    Extents,
}

const STYLES: [Style; 2] = [Style::BlockMapped, Style::Extents];

/// Builds an image of `size_mib` MiB from `src`.
fn build_image(src: &Path, out: &Path, style: Style, size_mib: u32) {
    let mut cmd = Command::new("mke2fs");
    cmd.args(["-q", "-F"]);
    if let Style::Extents = style {
        cmd.args(["-t", "ext4"]);
    }
    cmd.args(["-O", "^has_journal", "-m", "0", "-d"])
        .arg(src)
        .arg(out)
        .arg(format!("{size_mib}M"));
    let status = cmd.output().expect("run mke2fs");
    assert!(
        status.status.success(),
        "mke2fs failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
}

/// Replaces the one occurrence of `old` in the image with `new` (same length).
fn patch(image: &Path, old: &[u8], new: &[u8]) {
    assert_eq!(old.len(), new.len());
    let mut bytes = fs::read(image).expect("read image");
    let hits: Vec<usize> = bytes
        .windows(old.len())
        .enumerate()
        .filter(|(_, w)| *w == old)
        .map(|(i, _)| i)
        .collect();
    assert_eq!(hits.len(), 1, "expected one occurrence of {old:?}");
    bytes[hits[0]..hits[0] + old.len()].copy_from_slice(new);
    fs::write(image, bytes).expect("write image");
}

/// A directory entry's tail: name length, file type, name.
fn dirent_tail(name: &[u8], file_type: u8) -> Vec<u8> {
    let mut v = vec![name.len() as u8, file_type];
    v.extend_from_slice(name);
    v
}

fn limits() -> ImageLimits {
    ImageLimits {
        timeout: Duration::from_secs(60),
        ..ImageLimits::default()
    }
}

fn scan(image: &Path, limits: &ImageLimits) -> Result<(Vec<ImageEntry>, WalkReport), Drive9Error> {
    let mut v = ScanVisitor::default();
    let report = walk_ext4_image(image, limits, &mut v)?;
    Ok((v.entries, report))
}

fn extract(image: &Path, dest: &Path, limits: &ImageLimits) -> Result<WalkReport, Drive9Error> {
    fs::create_dir_all(dest).expect("dest");
    let mut w = TreeWriter::new(dest.to_path_buf(), None);
    walk_ext4_image(image, limits, &mut w)
}

fn paths_of(entries: &[ImageEntry]) -> Vec<String> {
    entries.iter().map(|e| e.path.clone()).collect()
}

/// Every path under `root`, relative, with its file type, links not followed.
fn host_tree(root: &Path) -> Vec<(String, bool, bool)> {
    fn go(root: &Path, dir: &Path, out: &mut Vec<(String, bool, bool)>) {
        for e in fs::read_dir(dir).expect("read_dir").flatten() {
            let md = fs::symlink_metadata(e.path()).expect("md");
            let rel = e
                .path()
                .strip_prefix(root)
                .expect("prefix")
                .to_string_lossy()
                .into_owned();
            out.push((rel, md.is_file(), md.is_dir()));
            if md.is_dir() {
                go(root, &e.path(), out);
            }
        }
    }
    let mut out = Vec::new();
    go(root, root, &mut out);
    out.sort();
    out
}

struct Fixture {
    _dir: TempDir,
    src: PathBuf,
    img: PathBuf,
    dir_path: PathBuf,
}

fn fixture() -> Fixture {
    let dir = tempdir().expect("tempdir");
    let src = dir.path().join("src");
    fs::create_dir_all(&src).expect("src");
    let img = dir.path().join("img");
    Fixture {
        dir_path: dir.path().to_path_buf(),
        _dir: dir,
        src,
        img,
    }
}

#[test]
fn files_dirs_and_modes_come_out_the_same_for_block_maps_and_extents() {
    need_mke2fs!();
    for style in STYLES {
        let f = fixture();
        fs::create_dir_all(f.src.join("workspace/deep/er")).unwrap();
        fs::write(f.src.join("workspace/a.txt"), b"alpha").unwrap();
        fs::write(f.src.join("workspace/deep/er/b.txt"), b"beta").unwrap();
        fs::write(f.src.join("workspace/empty"), b"").unwrap();
        fs::write(f.src.join("run.sh"), b"#!/bin/sh\n").unwrap();
        fs::set_permissions(f.src.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(f.src.join("suid"), b"x").unwrap();
        fs::set_permissions(f.src.join("suid"), fs::Permissions::from_mode(0o4755)).unwrap();
        fs::write(f.src.join("secret"), b"x").unwrap();
        fs::set_permissions(f.src.join("secret"), fs::Permissions::from_mode(0o600)).unwrap();
        build_image(&f.src, &f.img, style, 16);

        let out = f.dir_path.join("out");
        let report = extract(&f.img, &out, &limits()).unwrap();
        assert_eq!(report.files, 6, "{style:?}");
        assert_eq!(fs::read(out.join("workspace/a.txt")).unwrap(), b"alpha");
        assert_eq!(
            fs::read(out.join("workspace/deep/er/b.txt")).unwrap(),
            b"beta"
        );
        assert_eq!(fs::read(out.join("workspace/empty")).unwrap(), b"");
        let mode = |p: &str| fs::metadata(out.join(p)).unwrap().mode() & 0o7777;
        assert_eq!(mode("run.sh"), 0o755, "{style:?}");
        assert_eq!(mode("workspace/a.txt"), 0o644);
        // setuid and group/other bits never carry over; owner-only becomes 0644.
        assert_eq!(mode("suid"), 0o755);
        assert_eq!(mode("secret"), 0o644);
        // Ownership is the extracting user's, not the image's.
        assert_eq!(
            fs::metadata(out.join("workspace/a.txt")).unwrap().uid(),
            fs::metadata(&out).unwrap().uid()
        );
        assert!(!host_tree(&out).iter().any(|(p, _, _)| p == "lost+found"));
    }
}

#[test]
fn a_dirent_name_with_dotdot_and_a_separator_fails_the_walk_and_nothing_leaves_the_target() {
    need_mke2fs!();
    for style in STYLES {
        let f = fixture();
        fs::write(f.src.join("abcdefg"), b"pwned").unwrap();
        build_image(&f.src, &f.img, style, 8);
        patch(
            &f.img,
            &dirent_tail(b"abcdefg", 1),
            &dirent_tail(b"../evil", 1),
        );

        // The target is two levels below the scratch dir, as a session's staging is.
        let out = f.dir_path.join("out/inner");
        let err = extract(&f.img, &out, &limits()).unwrap_err();
        assert!(matches!(err, Drive9Error::ImageCorrupt(_)), "{err:?}");
        assert!(!f.dir_path.join("out/evil").exists(), "escaped the target");
        assert!(!f.dir_path.join("evil").exists());
        assert!(host_tree(&f.dir_path.join("out"))
            .iter()
            .all(|(p, _, _)| p == "inner"));
    }
}

#[test]
fn a_name_with_a_separator_inside_or_a_nul_never_becomes_a_host_path() {
    need_mke2fs!();
    for name in [&b"a/bcdef"[..], &b"a\0bcdef"[..], &b"/bcdefg"[..]] {
        let f = fixture();
        fs::write(f.src.join("abcdefg"), b"x").unwrap();
        build_image(&f.src, &f.img, Style::BlockMapped, 8);
        let mut new = vec![0u8; 7];
        new[..name.len().min(7)].copy_from_slice(&name[..name.len().min(7)]);
        patch(&f.img, &dirent_tail(b"abcdefg", 1), &dirent_tail(&new, 1));
        let out = f.dir_path.join("out");
        assert!(extract(&f.img, &out, &limits()).is_err(), "{name:?}");
        assert!(host_tree(&out).is_empty(), "{name:?}");
    }
}

#[test]
fn dot_names_refused_names_and_duplicates_are_skipped_not_extracted() {
    need_mke2fs!();
    let f = fixture();
    for n in ["aaaa", "bbbb", "cccc", "dddd", "eeee", "keep"] {
        fs::write(f.src.join(n), n.as_bytes()).unwrap();
    }
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    // `..` as the name of a regular file, a control character, a non-UTF-8 name,
    // and a second `aaaa` in the same directory.
    patch(&f.img, &dirent_tail(b"bbbb", 1), &dirent_tail(b"aaaa", 1));
    patch(&f.img, &dirent_tail(b"cccc", 1), &dirent_tail(b"c\ncc", 1));
    patch(
        &f.img,
        &dirent_tail(b"dddd", 1),
        &dirent_tail(&[0xff, 0xfe, b'd', b'd'], 1),
    );
    patch(&f.img, &dirent_tail(b"eeee", 1), &dirent_tail(b"eeee", 1));

    let (entries, report) = scan(&f.img, &limits()).unwrap();
    let paths = paths_of(&entries);
    assert!(paths.contains(&"aaaa".to_string()) && paths.contains(&"keep".to_string()));
    assert!(paths.contains(&"eeee".to_string()));
    assert_eq!(
        paths.iter().filter(|p| *p == "aaaa").count(),
        1,
        "{paths:?}"
    );
    assert_eq!(report.skipped_count, 3, "{:?}", report.skipped);
    assert!(report
        .skipped
        .iter()
        .any(|s| s.reason.contains("duplicate")));
    assert!(report.skipped.iter().any(|s| s.reason.contains("control")));
    assert!(report.skipped.iter().any(|s| s.reason.contains("UTF-8")));
    assert!(!paths.iter().any(|p| p.contains("..") || p.contains('\n')));
}

#[test]
fn names_that_differ_only_in_case_are_two_files_and_never_overwrite_each_other() {
    need_mke2fs!();
    let f = fixture();
    fs::write(f.src.join("Readme"), b"upper").unwrap();
    fs::write(f.src.join("README"), b"shout").unwrap();
    fs::write(f.src.join("readme"), b"lower").unwrap();
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    let (entries, report) = scan(&f.img, &limits()).unwrap();
    assert_eq!(paths_of(&entries), ["README", "Readme", "readme"]);
    assert_eq!(report.skipped_count, 0);
    let dest = f.dir_path.join("out");
    extract(&f.img, &dest, &limits()).unwrap();
    assert_eq!(fs::read(dest.join("Readme")).unwrap(), b"upper");
    assert_eq!(fs::read(dest.join("README")).unwrap(), b"shout");
    assert_eq!(fs::read(dest.join("readme")).unwrap(), b"lower");
}

#[test]
fn a_regular_file_entry_named_dot_dot_is_ignored() {
    need_mke2fs!();
    let f = fixture();
    fs::write(f.src.join("Zq"), b"x").unwrap();
    fs::write(f.src.join("ok"), b"ok").unwrap();
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    patch(&f.img, &dirent_tail(b"Zq", 1), &dirent_tail(b"..", 1));
    let out = f.dir_path.join("out");
    extract(&f.img, &out, &limits()).unwrap();
    let tree = host_tree(&out);
    assert_eq!(tree, vec![("ok".to_string(), true, false)]);
}

#[test]
fn symlinks_fifos_and_devices_are_reported_and_never_created() {
    need_mke2fs!();
    let f = fixture();
    let outside = f.dir_path.join("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(f.src.join("plain"), b"p").unwrap();
    std::os::unix::fs::symlink("/etc/passwd", f.src.join("abs-link")).unwrap();
    std::os::unix::fs::symlink(&outside, f.src.join("dir-link")).unwrap();
    std::os::unix::fs::symlink("../../up", f.src.join("rel-link")).unwrap();
    let fifo = std::ffi::CString::new(f.src.join("pipe").to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
    let dev = std::ffi::CString::new(f.src.join("chardev").to_str().unwrap()).unwrap();
    // SAFETY: a valid NUL-terminated path; fails without CAP_MKNOD, then skipped.
    let have_dev =
        unsafe { libc::mknod(dev.as_ptr(), libc::S_IFCHR | 0o600, libc::makedev(1, 3)) } == 0;
    build_image(&f.src, &f.img, Style::BlockMapped, 8);

    let out = f.dir_path.join("out");
    let report = extract(&f.img, &out, &limits()).unwrap();
    assert_eq!(report.dropped_count, if have_dev { 5 } else { 4 });
    let kinds: Vec<&str> = report.dropped.iter().map(|d| d.reason).collect();
    assert!(
        kinds.contains(&"symlink") && kinds.contains(&"fifo"),
        "{kinds:?}"
    );
    // Only the plain file exists on the host; nothing was linked or created.
    assert_eq!(host_tree(&out), vec![("plain".to_string(), true, false)]);
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
}

#[test]
fn a_name_that_would_cross_an_in_image_symlink_is_refused_and_the_link_target_stays_empty() {
    need_mke2fs!();
    let f = fixture();
    let target = f.dir_path.join("target");
    fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(&target, f.src.join("zlnk")).unwrap();
    fs::write(f.src.join("qqqqqqq"), b"x").unwrap();
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    patch(
        &f.img,
        &dirent_tail(b"qqqqqqq", 1),
        &dirent_tail(b"zlnk/xy", 1),
    );
    let out = f.dir_path.join("out");
    assert!(extract(&f.img, &out, &limits()).is_err());
    assert!(fs::read_dir(&target).unwrap().next().is_none());
}

#[test]
fn the_tree_writer_never_follows_a_symlink_or_overwrites_a_name() {
    let dir = tempdir().unwrap();
    let outside = dir.path().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let root = dir.path().join("root");
    fs::create_dir_all(&root).unwrap();
    std::os::unix::fs::symlink(&outside, root.join("lnk")).unwrap();
    fs::write(root.join("A"), b"original").unwrap();
    let mut w = TreeWriter::new(root.clone(), None);
    let entry = |path: &str| ImageEntry {
        path: path.to_string(),
        kind: ImageEntryKind::File,
        size: 3,
        mode: 0o644,
    };
    // A directory name that is a symlink on the host, and a name that exists.
    for path in ["lnk/x", "A", "../escape", "a/../b"] {
        let mut body: &[u8] = b"new";
        let r = w.visit(&entry(path), Some(&mut body));
        if path.contains("..") {
            assert!(r.is_err(), "{path}");
        } else {
            r.unwrap();
        }
    }
    assert_eq!(w.collisions, 2);
    assert_eq!(w.written_files, 0);
    assert_eq!(fs::read(root.join("A")).unwrap(), b"original");
    assert!(fs::read_dir(&outside).unwrap().next().is_none());
    assert!(!dir.path().join("escape").exists());
}

#[test]
fn hard_links_count_against_the_byte_cap_once_per_name() {
    need_mke2fs!();
    let f = fixture();
    fs::write(f.src.join("big"), vec![7u8; 300 * 1024]).unwrap();
    for i in 0..20 {
        fs::hard_link(f.src.join("big"), f.src.join(format!("link{i}"))).unwrap();
    }
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    let tight = ImageLimits {
        max_total_bytes: 1024 * 1024,
        ..limits()
    };
    let err = extract(&f.img, &f.dir_path.join("a"), &tight).unwrap_err();
    assert!(matches!(err, Drive9Error::ImageLimit(_)), "{err:?}");
    let roomy = ImageLimits {
        max_total_bytes: 64 * 1024 * 1024,
        ..limits()
    };
    let report = extract(&f.img, &f.dir_path.join("b"), &roomy).unwrap();
    assert_eq!(report.files, 21);
    assert_eq!(report.file_bytes, 21 * 300 * 1024);
}

#[test]
fn a_64_gib_sparse_file_is_refused_by_size_before_any_of_it_is_read() {
    need_mke2fs!();
    let f = fixture();
    let huge = fs::File::create(f.src.join("huge")).unwrap();
    huge.set_len(64 * 1024 * 1024 * 1024).unwrap();
    fs::write(f.src.join("small"), b"s").unwrap();
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    let out = f.dir_path.join("out");
    let started = Instant::now();
    let err = extract(&f.img, &out, &limits()).unwrap_err();
    assert!(matches!(err, Drive9Error::ImageLimit(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
    // Nothing of it was written (`huge` sorts before `small`; the walk stopped).
    assert!(!out.join("huge").exists());
    // Even with a raised per-file cap, the total cap holds.
    let raised = ImageLimits {
        max_file_bytes: 1 << 40,
        ..limits()
    };
    assert!(matches!(
        extract(&f.img, &f.dir_path.join("out2"), &raised).unwrap_err(),
        Drive9Error::ImageLimit(_)
    ));
}

#[test]
fn a_sparse_file_stays_sparse_and_holes_in_block_mapped_files_read_as_zeros() {
    need_mke2fs!();
    for style in STYLES {
        let f = fixture();
        // Data at the head and tail, a hole between (past the direct blocks, so
        // the indirect level has a zero pointer: the case upstream got wrong).
        let sparse = fs::File::create(f.src.join("sparse")).unwrap();
        sparse.set_len(8 * 1024 * 1024).unwrap();
        sparse.write_all_at(b"head", 0).unwrap();
        sparse.write_all_at(b"tail", 8 * 1024 * 1024 - 4).unwrap();
        drop(sparse);
        let all_hole = fs::File::create(f.src.join("hole")).unwrap();
        all_hole.set_len(2 * 1024 * 1024).unwrap();
        drop(all_hole);
        build_image(&f.src, &f.img, style, 16);

        let out = f.dir_path.join("out");
        extract(&f.img, &out, &limits()).unwrap();
        let got = fs::read(out.join("sparse")).unwrap();
        assert_eq!(got.len(), 8 * 1024 * 1024);
        assert_eq!(&got[..4], b"head");
        assert_eq!(&got[got.len() - 4..], b"tail");
        assert!(got[4..got.len() - 4].iter().all(|b| *b == 0));
        assert!(fs::read(out.join("hole")).unwrap().iter().all(|b| *b == 0));
        let allocated = fs::metadata(out.join("sparse")).unwrap().blocks() * 512;
        assert!(
            allocated < 1024 * 1024,
            "{style:?}: {allocated} bytes allocated"
        );
        assert_eq!(fs::metadata(out.join("hole")).unwrap().blocks(), 0);
    }
}

#[test]
fn deep_trees_loops_and_entry_floods_stop_at_the_caps() {
    need_mke2fs!();
    // Depth.
    let f = fixture();
    let mut deep = f.src.clone();
    for _ in 0..40 {
        deep.push("d");
    }
    fs::create_dir_all(&deep).unwrap();
    fs::write(deep.join("leaf"), b"l").unwrap();
    build_image(&f.src, &f.img, Style::BlockMapped, 16);
    let shallow = ImageLimits {
        max_depth: 10,
        ..limits()
    };
    assert!(matches!(
        scan(&f.img, &shallow).unwrap_err(),
        Drive9Error::ImageLimit(_)
    ));
    assert!(scan(&f.img, &limits()).is_ok());

    // Entries.
    let f = fixture();
    for i in 0..200 {
        fs::write(f.src.join(format!("f{i}")), b"x").unwrap();
    }
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    let few = ImageLimits {
        max_entries: 50,
        ..limits()
    };
    assert!(matches!(
        scan(&f.img, &few).unwrap_err(),
        Drive9Error::ImageLimit(_)
    ));
    // 200 files plus `lost+found`, which is counted before it is skipped.
    assert_eq!(scan(&f.img, &limits()).unwrap().1.entries, 201);

    // A directory that contains itself: the entry `loop` in `d` is patched to
    // point at `d`'s own inode.
    if Command::new("debugfs").arg("-V").output().is_err() {
        return;
    }
    let f = fixture();
    fs::create_dir_all(f.src.join("d")).unwrap();
    fs::write(f.src.join("d/loopxx"), b"x").unwrap();
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    let ls = Command::new("debugfs")
        .args(["-R", "ls -p /"])
        .arg(&f.img)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&ls.stdout).into_owned();
    let ino: u32 = text
        .lines()
        .find(|l| l.contains("/d/"))
        .and_then(|l| l.split('/').nth(1))
        .and_then(|n| n.parse().ok())
        .expect("inode of d");
    let mut bytes = fs::read(&f.img).unwrap();
    let tail = dirent_tail(b"loopxx", 1);
    let at = bytes
        .windows(tail.len())
        .position(|w| w == tail.as_slice())
        .expect("dirent");
    bytes[at - 6..at - 2].copy_from_slice(&ino.to_le_bytes());
    bytes[at + 1] = 2; // typed as a directory
    fs::write(&f.img, bytes).unwrap();
    let started = Instant::now();
    let capped = ImageLimits {
        max_depth: 20,
        ..limits()
    };
    let err = scan(&f.img, &capped).unwrap_err();
    assert!(matches!(err, Drive9Error::ImageLimit(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(5));
}

#[test]
fn per_file_cap_and_the_deadline_are_enforced() {
    need_mke2fs!();
    let f = fixture();
    fs::write(f.src.join("one"), vec![1u8; 300 * 1024]).unwrap();
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    let small = ImageLimits {
        max_file_bytes: 100 * 1024,
        ..limits()
    };
    assert!(matches!(
        scan(&f.img, &small).unwrap_err(),
        Drive9Error::ImageLimit(_)
    ));
    let expired = ImageLimits {
        timeout: Duration::ZERO,
        ..limits()
    };
    assert!(matches!(
        scan(&f.img, &expired).unwrap_err(),
        Drive9Error::ImageLimit(_)
    ));
}

#[test]
fn the_host_vam_dir_and_lost_found_are_never_read_but_nested_vam_dirs_are() {
    need_mke2fs!();
    let f = fixture();
    fs::create_dir_all(f.src.join(".vam/threads/t")).unwrap();
    fs::write(f.src.join(".vam/threads/t/log"), b"conversation").unwrap();
    fs::create_dir_all(f.src.join("workspace/.vam/agent")).unwrap();
    fs::write(f.src.join("workspace/.vam/agent/obs"), b"guest note").unwrap();
    fs::write(f.src.join("a"), b"a").unwrap();
    build_image(&f.src, &f.img, Style::BlockMapped, 8);
    let (entries, _) = scan(&f.img, &limits()).unwrap();
    let paths = paths_of(&entries);
    assert!(
        !paths.iter().any(|p| p == ".vam" || p.starts_with(".vam/")),
        "{paths:?}"
    );
    assert!(!paths.iter().any(|p| p.starts_with("lost+found")));
    assert!(paths.contains(&"workspace/.vam/agent/obs".to_string()));

    // A plain file named `.vam` at the top is skipped too.
    let g = fixture();
    fs::write(g.src.join(".vam"), b"not a dir").unwrap();
    fs::write(g.src.join("a"), b"a").unwrap();
    build_image(&g.src, &g.img, Style::BlockMapped, 8);
    assert_eq!(paths_of(&scan(&g.img, &limits()).unwrap().0), vec!["a"]);
}

/// A small xorshift generator: the fuzz cases are the same on every run.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
}

#[test]
fn torn_truncated_zero_and_flipped_images_fail_without_a_panic_or_a_hang() {
    need_mke2fs!();
    let f = fixture();
    fs::create_dir_all(f.src.join("workspace/sub")).unwrap();
    for i in 0..40 {
        fs::write(
            f.src.join(format!("workspace/sub/f{i}")),
            vec![i as u8; 5000],
        )
        .unwrap();
    }
    fs::write(f.src.join("workspace/big"), vec![9u8; 300 * 1024]).unwrap();
    for style in STYLES {
        build_image(&f.src, &f.img, style, 4);
        let good = fs::read(&f.img).unwrap();
        let tight = ImageLimits {
            max_entries: 10_000,
            timeout: Duration::from_secs(10),
            ..limits()
        };
        let try_image = |bytes: &[u8], what: &str| {
            let path = f.dir_path.join("fuzz.img");
            fs::write(&path, bytes).unwrap();
            let started = Instant::now();
            if let Err(e) = scan(&path, &tight) {
                assert!(
                    !e.to_string().contains("unexpectedly"),
                    "{what}: the reader panicked: {e}"
                );
            }
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "{what}: too slow"
            );
        };
        try_image(&[], "empty");
        try_image(&vec![0u8; 1 << 20], "zeros");
        try_image(&vec![0xffu8; 1 << 20], "0xff");
        try_image(&good[..good.len() / 2], "truncated");
        try_image(&good[..2048], "superblock only");
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        for round in 0..150 {
            let mut bytes = good.clone();
            let flips = 1 + rng.next() % 64;
            for _ in 0..flips {
                // Mostly the metadata at the front, sometimes anywhere.
                let span = if rng.next().is_multiple_of(4) {
                    bytes.len()
                } else {
                    512 * 1024
                };
                let at = (rng.next() % span as u64) as usize;
                bytes[at] = rng.next() as u8;
            }
            try_image(&bytes, &format!("{style:?} flip round {round}"));
        }
    }
}

#[test]
fn a_symlink_or_a_directory_is_not_a_readable_image() {
    let dir = tempdir().unwrap();
    let real = dir.path().join("real");
    fs::write(&real, b"x").unwrap();
    let link = dir.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    assert!(scan(&link, &limits()).is_err());
    assert!(scan(dir.path(), &limits()).is_err());
}

#[test]
fn valid_component_accepts_only_plain_names() {
    for ok in ["a", "file.txt", ".hidden", "with space", "ünï"] {
        assert!(valid_component(ok), "{ok}");
    }
    for bad in ["", ".", "..", "a/b", "a\0b", "a\nb", "a\tb", "\u{7f}"] {
        assert!(!valid_component(bad), "{bad:?}");
    }
    assert!(!valid_component(&"x".repeat(256)));
}
