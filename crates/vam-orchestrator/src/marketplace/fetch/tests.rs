use super::*;
use crate::marketplace::testutil::{git_bin, TestRepo};
use std::os::unix::fs::PermissionsExt;

fn fetcher(root: &Path) -> Option<Fetcher> {
    git_bin().map(|g| Fetcher::new(Some(g), root.to_path_buf(), Limits::default()))
}

fn remote(r: &TestRepo) -> Remote {
    Remote::File { url: r.url() }
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    p
}

const C1: &str = "1111111111111111111111111111111111111111";
const C2: &str = "2222222222222222222222222222222222222222";

// ---- the hardening of every call ------------------------------------------------------------

#[test]
fn every_call_carries_the_hardening_flags() {
    let https = Remote::Https {
        url: "https://git.example.com/a/b".into(),
        host: "git.example.com".into(),
    };
    let args = config_args(Some(&https), Some("http://127.0.0.1:9"));
    let pairs: Vec<&str> = args
        .chunks(2)
        .map(|c| {
            assert_eq!(c[0], "-c");
            c[1].as_str()
        })
        .collect();
    for want in [
        "core.hooksPath=/dev/null",
        "protocol.allow=never",
        "protocol.https.allow=always",
        "http.followRedirects=false",
        "http.proxy=http://127.0.0.1:9",
        "http.proxyAuthMethod=basic",
        "http.sslVerify=true",
        "fetch.fsckObjects=true",
        "transfer.fsckObjects=true",
        "fetch.recurseSubmodules=false",
        "credential.helper=",
    ] {
        assert!(pairs.contains(&want), "missing {want}: {pairs:?}");
    }
    assert!(
        !pairs.iter().any(|p| p.contains("protocol.file")),
        "an https call never allows the file transport"
    );
    // A local read (no remote) allows no transport at all.
    let local = config_args(None, None);
    assert!(local.iter().any(|a| a == "protocol.allow=never"));
    assert!(!local.iter().any(|a| a.starts_with("protocol.https.allow")));
    assert!(!local.iter().any(|a| a.starts_with("http.proxy=")));
    // The file transport exists only for the test seam's remote.
    let file = config_args(
        Some(&Remote::File {
            url: "file:///x".into(),
        }),
        None,
    );
    assert!(file.iter().any(|a| a == "protocol.file.allow=always"));
    assert!(!file.iter().any(|a| a.starts_with("protocol.https.allow")));
}

#[test]
fn the_child_environment_is_a_fixed_list() {
    let https = Remote::Https {
        url: "https://git.example.com/a/b".into(),
        host: "git.example.com".into(),
    };
    let env = child_env(
        Path::new("/usr/bin/git"),
        Path::new("/s/.home"),
        Path::new("/s/.tmp"),
        Some(&https),
    );
    let get = |k: &str| env.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    assert_eq!(get("GIT_CONFIG_NOSYSTEM"), Some("1"));
    assert_eq!(get("GIT_CONFIG_GLOBAL"), Some("/dev/null"));
    assert_eq!(get("GIT_TERMINAL_PROMPT"), Some("0"));
    assert_eq!(get("GIT_ALLOW_PROTOCOL"), Some("https"));
    assert_eq!(get("HOME"), Some("/s/.home"));
    let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
    for leak in [
        "HTTPS_PROXY",
        "https_proxy",
        "SSH_AUTH_SOCK",
        "GIT_SSH_COMMAND",
        "GIT_DIR",
        "GIT_EXEC_PATH",
    ] {
        assert!(!names.contains(&leak), "{leak} must not be set");
    }
}

#[test]
fn a_real_call_sees_only_the_fixed_environment_and_the_flags_in_front_of_the_subcommand() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("log");
    let fake = script(
        dir.path(),
        "fake-git",
        &format!(
            "env > {0}.env\nprintf '%s\\n' \"$@\" > {0}.args\nexit 0",
            log.display()
        ),
    );
    std::env::set_var("VAMD_TEST_SECRET_LEAK", "leaked");
    let f = Fetcher::new(Some(fake), dir.path().join("state"), Limits::default());
    f.ls_remote(&remote_file()).ok();
    let env = std::fs::read_to_string(dir.path().join("log.env")).unwrap();
    assert!(!env.contains("VAMD_TEST_SECRET_LEAK"), "{env}");
    assert!(!env.contains("leaked"), "{env}");
    assert!(env.contains("GIT_CONFIG_NOSYSTEM=1"), "{env}");
    assert!(env.contains("GIT_TERMINAL_PROMPT=0"), "{env}");
    assert!(env.contains("GIT_CONFIG_GLOBAL=/dev/null"), "{env}");
    let args = std::fs::read_to_string(dir.path().join("log.args")).unwrap();
    let lines: Vec<&str> = args.lines().collect();
    let sub = lines.iter().position(|l| *l == "ls-remote").unwrap();
    assert!(lines[..sub].contains(&"core.hooksPath=/dev/null"), "{args}");
    assert!(lines[..sub].contains(&"protocol.allow=never"), "{args}");
    // The URL comes after `--`, never as an option.
    assert_eq!(lines[sub + 1], "--");
    assert_eq!(lines[sub + 2], "file:///nowhere");
}

fn remote_file() -> Remote {
    Remote::File {
        url: "file:///nowhere".into(),
    }
}

// ---- argument and path validation -------------------------------------------------------------

#[test]
fn ids_oids_and_tag_refs_are_validated() {
    assert!(valid_id("squrell") && valid_id("m-0a1b2c3d4e"));
    for bad in ["", "-x", "../x", "a/b", "A", "a b", &"a".repeat(41)] {
        assert!(!valid_id(bad), "{bad}");
    }
    assert!(valid_oid(C1));
    for bad in [
        "",
        "--upload-pack=x",
        &"g".repeat(40),
        &"A".repeat(40),
        &"1".repeat(39),
    ] {
        assert!(!valid_oid(bad), "{bad}");
    }
    // Tags `a` and `a/b` get flat, distinct refs (no file/directory clash).
    assert_ne!(tag_ref("a"), tag_ref("a/b"));
    assert!(
        tag_ref("a/b").starts_with("refs/vam/t/")
            && !tag_ref("a/b")["refs/vam/t/".len()..].contains('/')
    );
}

#[test]
fn a_tag_that_could_be_an_option_never_reaches_git() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("ran");
    let fake = script(
        dir.path(),
        "fake-git",
        &format!("touch {}", marker.display()),
    );
    let f = Fetcher::new(Some(fake), dir.path().join("state"), Limits::default());
    for tag in [
        "--upload-pack=touch /tmp/pwn",
        "-x",
        "a..b",
        "a b",
        "a;b",
        "$(id)",
        "x.lock",
        "../x",
    ] {
        let err = f
            .fetch("squrell", &remote_file(), &[tag.to_string()])
            .unwrap_err();
        assert!(matches!(err, FetchError::Invalid(_)), "{tag}: {err:?}");
    }
    assert!(!marker.exists(), "git must not have been started");
    assert!(f.tag_commit("squrell", "--exec").is_err());
    assert!(f.cache_dir("../x").is_err());
    assert!(f.wipe("../x").is_err());
}

#[test]
fn ls_remote_output_is_parsed_strictly() {
    let out = format!(
        "{C1}\tHEAD\n{C1}\trefs/tags/v1\n{C2}\trefs/tags/v2\n{C1}\trefs/tags/v2^{{}}\n{C2}\trefs/tags/--bad\n{C2}\trefs/heads/main\n"
    );
    let r = parse_ls_remote(&out, 100).unwrap();
    assert_eq!(r.head, C1);
    assert_eq!(
        r.tags.len(),
        2,
        "an unusable tag name is skipped: {:?}",
        r.tags
    );
    assert_eq!(r.tags["v1"], C1);
    assert_eq!(r.tags["v2"], C1, "an annotated tag is peeled to its commit");
    assert!(matches!(
        parse_ls_remote(&format!("{C1}\trefs/tags/v1\n"), 100),
        Err(FetchError::Invalid(_))
    ));
    assert!(parse_ls_remote("zz\tHEAD\n", 100).is_err());
    let many: String = std::iter::once(format!("{C1}\tHEAD\n"))
        .chain((0..20).map(|i| format!("{C1}\trefs/tags/t{i}\n")))
        .collect();
    assert!(matches!(
        parse_ls_remote(&many, 10),
        Err(FetchError::TooLarge(_))
    ));
}

#[test]
fn ls_tree_output_is_parsed() {
    let rec = format!("100644 blob {C1}      12\ta/b.json\0120000 blob {C2}       3\tlink\0160000 commit {C1}       -\tsub\0");
    let e = parse_ls_tree(rec.as_bytes()).unwrap();
    assert_eq!(e.len(), 3);
    assert_eq!(
        (e[0].mode.as_str(), e[0].size, e[0].path.as_str()),
        ("100644", 12, "a/b.json")
    );
    assert!(check_entry(&e[0]).is_ok());
    assert!(check_entry(&e[1])
        .unwrap_err()
        .to_string()
        .contains("symlink"));
    assert!(check_entry(&e[2])
        .unwrap_err()
        .to_string()
        .contains("submodule"));
    assert!(parse_ls_tree(b"garbage\0").is_err());
}

#[test]
fn exported_paths_cannot_leave_the_destination() {
    let d = Path::new("/x/dest");
    assert_eq!(
        safe_join(d, "a/b.txt").unwrap(),
        Path::new("/x/dest/a/b.txt")
    );
    for bad in [
        "../x",
        "a/../../x",
        "/etc/passwd",
        ".git/config",
        "a/.GIT/x",
        "a\\b",
        "",
    ] {
        assert!(safe_join(d, bad).is_err(), "{bad}");
    }
}

// ---- killing a call that goes wrong -----------------------------------------------------------

#[test]
fn a_hung_git_is_killed_at_the_timeout_with_its_children() {
    let dir = tempfile::tempdir().unwrap();
    let pidfile = dir.path().join("child.pid");
    let fake = script(
        dir.path(),
        "fake-git",
        &format!("sleep 30 &\necho $! > {}\nwait", pidfile.display()),
    );
    let limits = Limits {
        ls_remote: Duration::from_millis(400),
        ..Limits::default()
    };
    let f = Fetcher::new(Some(fake), dir.path().join("state"), limits);
    let started = Instant::now();
    let err = f.ls_remote(&remote_file()).unwrap_err();
    assert_eq!(err, FetchError::Timeout("ls-remote"));
    assert!(started.elapsed() < Duration::from_secs(10));
    // The background `sleep` was in the same process group and is gone.
    std::thread::sleep(Duration::from_millis(300));
    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    // SAFETY: signal 0 only checks that the process exists.
    let alive = unsafe { libc::kill(pid, 0) } == 0;
    // A killed child may linger as a zombie until its parent (init) reaps it; check its state.
    let zombie = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .map(|s| {
            s.split(')')
                .nth(1)
                .is_some_and(|r| r.trim_start().starts_with('Z'))
        })
        .unwrap_or(true);
    assert!(!alive || zombie, "the child process survived the kill");
}

#[test]
fn output_past_the_cap_kills_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let fake = script(
        dir.path(),
        "fake-git",
        "yes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    );
    let limits = Limits {
        max_ls_bytes: 4096,
        ..Limits::default()
    };
    let f = Fetcher::new(Some(fake), dir.path().join("state"), limits);
    let err = f.ls_remote(&remote_file()).unwrap_err();
    assert!(matches!(err, FetchError::TooLarge(_)), "{err:?}");
}

#[test]
fn a_fetch_that_grows_the_cache_past_the_cap_is_killed() {
    let dir = tempfile::tempdir().unwrap();
    let fake = script(
        dir.path(),
        "fake-git",
        r#"case " $* " in
  *" init "*) for last; do :; done; mkdir -p "$last"; : > "$last/HEAD"; exit 0;;
  *" fetch "*) while [ $# -gt 0 ]; do [ "$1" = --git-dir ] && d=$2; shift; done
     dd if=/dev/zero of="$d/big" bs=1M count=8 2>/dev/null; sleep 20;;
esac"#,
    );
    let limits = Limits {
        max_cache_bytes: 1024 * 1024,
        ..Limits::default()
    };
    let f = Fetcher::new(Some(fake), dir.path().join("state"), limits);
    let started = Instant::now();
    let err = f.fetch("squrell", &remote_file(), &[]).unwrap_err();
    assert!(matches!(err, FetchError::TooLarge(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(10));
}

#[test]
fn without_git_nothing_runs_and_the_error_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let f = Fetcher::new(None, dir.path().to_path_buf(), Limits::default());
    assert_eq!(
        f.ls_remote(&remote_file()).unwrap_err(),
        FetchError::GitMissing
    );
    let f = Fetcher::new(
        Some(PathBuf::from("/nonexistent/git")),
        dir.path().to_path_buf(),
        Limits::default(),
    );
    assert_eq!(
        f.ls_remote(&remote_file()).unwrap_err(),
        FetchError::GitMissing
    );
}

// ---- real git against a local repository -------------------------------------------------------

#[test]
fn tags_and_the_default_branch_are_read_and_fetched_shallow() {
    let state = tempfile::tempdir().unwrap();
    let Some(f) = fetcher(state.path()) else {
        return;
    };
    let mut r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    r.overlay("marketplace-squrell-update");
    r.tag("microsoft-365-v1.1.0");
    let refs = f.ls_remote(&remote(&r)).unwrap();
    assert_eq!(refs.head, r.head());
    assert_eq!(
        refs.tags["microsoft-365-v1.1.0"],
        r.rev("microsoft-365-v1.1.0")
    );
    assert_ne!(
        refs.tags["microsoft-365-v1.0.0"],
        refs.tags["microsoft-365-v1.1.0"]
    );
    f.fetch("squrell", &remote(&r), &["microsoft-365-v1.0.0".into()])
        .unwrap();
    assert_eq!(
        f.ref_commit("squrell", HEAD_REF).unwrap().as_deref(),
        Some(r.head().as_str())
    );
    assert_eq!(
        f.tag_commit("squrell", "microsoft-365-v1.0.0")
            .unwrap()
            .as_deref(),
        Some(r.rev("microsoft-365-v1.0.0").as_str())
    );
    // Only what was asked for was fetched.
    assert_eq!(
        f.tag_commit("squrell", "microsoft-365-v1.1.0").unwrap(),
        None
    );
    let shallow = f.cache_dir("squrell").unwrap().join("shallow");
    assert!(shallow.is_file(), "a depth-1 fetch leaves a shallow file");
    // A second fetch of the same refs works and the index reads back.
    f.fetch("squrell", &remote(&r), &["microsoft-365-v1.0.0".into()])
        .unwrap();
    let raw = f
        .read_file("squrell", &r.head(), "marketplace.json", 1 << 20)
        .unwrap();
    assert!(String::from_utf8(raw)
        .unwrap()
        .contains("\"publisherDomain\""));
}

#[test]
fn an_artifact_directory_is_exported_from_the_exact_commit() {
    let state = tempfile::tempdir().unwrap();
    let Some(f) = fetcher(state.path()) else {
        return;
    };
    let mut r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    let v1 = r.rev("microsoft-365-v1.0.0");
    r.overlay("marketplace-squrell-update");
    r.tag("microsoft-365-v1.1.0");
    f.fetch(
        "squrell",
        &remote(&r),
        &["microsoft-365-v1.0.0".into(), "microsoft-365-v1.1.0".into()],
    )
    .unwrap();
    let dir = "listings/microsoft-365/playbooks/morning-triage";
    let (a, b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    f.export_dir("squrell", &v1, dir, a.path()).unwrap();
    f.export_dir("squrell", &r.rev("microsoft-365-v1.1.0"), dir, b.path())
        .unwrap();
    let (pa, pb) = (
        std::fs::read_to_string(a.path().join("playbook.json")).unwrap(),
        std::fs::read_to_string(b.path().join("playbook.json")).unwrap(),
    );
    assert_ne!(pa, pb, "each export is the text at its own commit");
    // A directory that is not there is refused, not exported empty.
    let c = tempfile::tempdir().unwrap();
    assert!(f
        .export_dir("squrell", &v1, "listings/nope", c.path())
        .is_err());
}

#[test]
fn a_symlink_a_submodule_or_an_oversize_file_is_refused_and_nothing_is_read_through_it() {
    let state = tempfile::tempdir().unwrap();
    let Some(f) = fetcher(state.path()) else {
        return;
    };
    let mut r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    // marketplace.json replaced by a symlink to a file outside the repository.
    let secret = state.path().join("outside.txt");
    std::fs::write(&secret, "TOP SECRET").unwrap();
    std::fs::remove_file(r.work_dir().join("marketplace.json")).unwrap();
    std::os::unix::fs::symlink(&secret, r.work_dir().join("marketplace.json")).unwrap();
    // A symlink inside an artifact directory, and a submodule.
    let pkg = r
        .work_dir()
        .join("listings/microsoft-365/playbooks/morning-triage");
    std::os::unix::fs::symlink("/etc/passwd", pkg.join("passwd")).unwrap();
    r.commit_all("odd objects");
    // A gitlink (submodule) entry inside an artifact directory; committed straight from the index,
    // since `add -A` would drop a path that is not on disk.
    let agent = "listings/microsoft-365/agents/executive-assistant";
    r.git(&[
        "update-index",
        "--add",
        "--cacheinfo",
        &format!("160000,{C1},{agent}/sub"),
    ]);
    r.git(&["commit", "-q", "-m", "submodule"]);
    r.publish();
    f.fetch("squrell", &remote(&r), &[]).unwrap();
    let head = r.head();
    let err = f
        .read_file("squrell", &head, "marketplace.json", 1 << 20)
        .unwrap_err();
    assert!(err.to_string().contains("symlink"), "{err}");
    let out = tempfile::tempdir().unwrap();
    let err = f
        .export_dir(
            "squrell",
            &head,
            "listings/microsoft-365/playbooks/morning-triage",
            out.path(),
        )
        .unwrap_err();
    assert!(err.to_string().contains("symlink"), "{err}");
    assert!(!out.path().join("passwd").exists());
    // A submodule entry in an exported directory is refused too.
    let out = tempfile::tempdir().unwrap();
    let err = f
        .export_dir("squrell", &head, agent, out.path())
        .unwrap_err();
    assert!(err.to_string().contains("submodule"), "{err}");
    // The cap on one file is checked before it is read.
    let big = "x".repeat(2 * 1024 * 1024);
    r.write(&[("big.json", &big)]);
    f.fetch("squrell", &remote(&r), &[]).unwrap();
    let err = f
        .read_file("squrell", &r.head(), "big.json", 1024)
        .unwrap_err();
    assert!(matches!(err, FetchError::TooLarge(_)), "{err:?}");
}

#[test]
fn nothing_in_the_source_repository_runs() {
    let state = tempfile::tempdir().unwrap();
    let Some(f) = fetcher(state.path()) else {
        return;
    };
    let r = TestRepo::new("marketplace-squrell", &["microsoft-365-v1.0.0"]);
    let marker = state.path().join("PWNED");
    let bare = r.dir.path().join("bare.git");
    // A hook the source repository would run if Git trusted it, and repository-local settings that
    // name commands (honoured only from protected configuration, never from the repository).
    let hook = bare.join("hooks/pre-receive");
    std::fs::create_dir_all(bare.join("hooks")).unwrap();
    std::fs::write(&hook, format!("#!/bin/sh\ntouch {}\n", marker.display())).unwrap();
    std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    let cfg = format!(
        "[uploadpack]\n\tpackObjectsHook = touch {m}\n[core]\n\tfsmonitor = touch {m}\n\thooksPath = {h}\n\tsshCommand = touch {m}\n",
        m = marker.display(),
        h = bare.join("hooks").display()
    );
    std::fs::OpenOptions::new()
        .append(true)
        .open(bare.join("config"))
        .and_then(|mut c| std::io::Write::write_all(&mut c, cfg.as_bytes()))
        .unwrap();
    f.ls_remote(&remote(&r)).unwrap();
    f.fetch("squrell", &remote(&r), &["microsoft-365-v1.0.0".into()])
        .unwrap();
    let out = tempfile::tempdir().unwrap();
    let c = f
        .tag_commit("squrell", "microsoft-365-v1.0.0")
        .unwrap()
        .unwrap();
    f.export_dir(
        "squrell",
        &c,
        "listings/microsoft-365/playbooks/morning-triage",
        out.path(),
    )
    .unwrap();
    assert!(!marker.exists(), "something in the source repository ran");
}

#[test]
fn the_guard_proxy_stops_git_at_a_loopback_https_host() {
    let state = tempfile::tempdir().unwrap();
    let Some(f) = fetcher(state.path()) else {
        return;
    };
    // Skips `check_url` and `precheck` on purpose: this is the connect-time layer on its own.
    let r = Remote::Https {
        url: "https://127.0.0.1/a/b".into(),
        host: "127.0.0.1".into(),
    };
    let started = Instant::now();
    let err = f.ls_remote(&r).unwrap_err();
    assert!(matches!(err, FetchError::Git(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(20));
    let text = err.to_string();
    // 403 (the target was refused), not 407: git sent the lease token on its first CONNECT.
    assert!(text.contains("403"), "{text}");
    assert!(!text.contains("407"), "{text}");
    assert!(
        !text.contains("vam:"),
        "no credentials in the error text: {text}"
    );
}

#[test]
fn precheck_refuses_a_host_that_resolves_to_an_internal_address() {
    let state = tempfile::tempdir().unwrap();
    let f = Fetcher::new(None, state.path().to_path_buf(), Limits::default());
    let r = Remote::Https {
        url: "https://127.0.0.1/a/b".into(),
        host: "127.0.0.1".into(),
    };
    assert!(matches!(f.precheck(&r), Err(FetchError::Refused(_))));
    let r = Remote::Https {
        url: "https://169.254.169.254/a/b".into(),
        host: "169.254.169.254".into(),
    };
    assert!(f.precheck(&r).is_err());
}
