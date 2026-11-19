//! Runs the real `vamd` against a database whose usernames clash ignoring letter case
//! (workstream `user-identity`, UI-2.3, locked decision 16).
//!
//! `vamd` refuses to serve or `--init` such a database (exit 1, every clashing group listed with
//! usernames and user ids, and the fix command). The offline flags `--rename-user` and
//! `--reset-password` still work on it. After the rename the next start proceeds and creates
//! the case-insensitive unique index. A clash is never an `open` error, so `vamd` never falls
//! back to an empty in-memory node.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use vam_orchestrator::users::USERNAME_NOCASE_INDEX;
use vam_orchestrator::workflow::SqliteWorkflowStore;

const CLASH_HEADER: &str = "usernames must be unique ignoring letter case";

fn vamd(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_vamd"));
    cmd.env_clear()
        .env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("HOME", dir)
        .env("RUST_LOG", "info")
        .env("VAMD_DB_PATH", db_path(dir))
        .env("VAMD_SPOOL_DIR", dir.join("spool"))
        .env("VAMD_OTLP_ENABLED", "0")
        .env("VAMD_LISTEN", "127.0.0.1:0")
        .env("VAMD_TELEMETRY_LISTEN", "127.0.0.1:0")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn db_path(dir: &Path) -> PathBuf {
    dir.join("state/vamd.db")
}

fn run(cmd: &mut Command) -> Output {
    cmd.output().expect("run vamd")
}

fn text(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Runs a command that may keep serving: it is killed after `limit`. Returns what it printed
/// and whether it was still running at the end.
fn run_for(cmd: &mut Command, limit: Duration) -> (Option<i32>, String, String) {
    let mut child = cmd.spawn().expect("spawn vamd");
    let start = Instant::now();
    let code = loop {
        if let Some(status) = child.try_wait().expect("try_wait") {
            break status.code();
        }
        if start.elapsed() > limit {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let out = child.wait_with_output().expect("output");
    (code, text(&out.stdout), text(&out.stderr))
}

fn index_exists(dir: &Path) -> bool {
    let conn = rusqlite::Connection::open(db_path(dir)).expect("raw open");
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = ?1",
        [USERNAME_NOCASE_INDEX],
        |row| row.get::<_, i64>(0),
    )
    .expect("count")
        > 0
}

fn username_of(dir: &Path, user_id: &str) -> String {
    let conn = rusqlite::Connection::open(db_path(dir)).expect("raw open");
    conn.query_row(
        "SELECT username FROM users WHERE user_id = ?1",
        [user_id],
        |row| row.get::<_, String>(0),
    )
    .expect("user row")
}

/// A database an older node left behind: `alice`, `bob` (id `bob`) and `Bob` (id `bob-2`), and no
/// case-insensitive index (an old node never made one, so both `bob` rows were allowed).
fn seed_clashing_db(dir: &Path) {
    std::fs::create_dir_all(dir.join("state")).expect("state dir");
    {
        let store = SqliteWorkflowStore::open(db_path(dir)).expect("seed open");
        store.create_user("alice", "alice").expect("alice");
        store.create_user("bob", "bob").expect("bob");
    }
    let conn = rusqlite::Connection::open(db_path(dir)).expect("raw open");
    conn.execute_batch(&format!("DROP INDEX {USERNAME_NOCASE_INDEX};"))
        .expect("drop the index");
    conn.execute(
        "INSERT INTO users (user_id, username, tenant_id, created_at, role)
         VALUES ('bob-2', 'Bob', 'bob-2', 4102444800, 'member')",
        [],
    )
    .expect("the clashing row");
    assert!(!index_exists(dir));
}

#[test]
fn a_clashing_database_is_refused_naming_both_users_and_the_fix() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    seed_clashing_db(dir);

    // `--init` refuses: exit 1, no admin key shown.
    let init = run(&mut vamd(dir, &["--init"]));
    let (o, e) = (text(&init.stdout), text(&init.stderr));
    assert_eq!(init.status.code(), Some(1), "stdout: {o}\nstderr: {e}");
    assert!(!o.contains("admin key: "), "{o}");
    for needle in [
        CLASH_HEADER,
        "bob (user id bob)",
        "Bob (user id bob-2)",
        "vamd --rename-user <user_id> <new_username>",
        "start vamd again",
    ] {
        assert!(e.contains(needle), "{needle:?} missing from stderr:\n{e}");
    }
    assert!(
        !e.contains("alice"),
        "only the clashing groups are listed:\n{e}"
    );

    // Serving refuses the same way, before it opens a port, and does not fall back to an empty
    // in-memory node.
    let (code, so, se) = run_for(&mut vamd(dir, &[]), Duration::from_secs(60));
    assert_eq!(code, Some(1), "stdout: {so}\nstderr: {se}");
    for needle in [
        CLASH_HEADER,
        "bob (user id bob)",
        "Bob (user id bob-2)",
        "vamd --rename-user",
    ] {
        assert!(se.contains(needle), "{needle:?} missing from stderr:\n{se}");
    }
    assert!(
        !format!("{so}{se}").contains("in-memory"),
        "a clash must not start an in-memory node:\n{so}\n{se}"
    );

    // Nothing was changed by either refusal.
    assert!(!index_exists(dir));
    assert_eq!(username_of(dir, "bob"), "bob");
    assert_eq!(username_of(dir, "bob-2"), "Bob");
}

#[test]
fn rename_user_fixes_the_clash_and_the_next_start_creates_the_index_and_proceeds() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    seed_clashing_db(dir);

    let renamed = run(&mut vamd(
        dir,
        &["--rename-user", "bob-2", "Bobby+work@Example.Test"],
    ));
    let (o, e) = (text(&renamed.stdout), text(&renamed.stderr));
    assert_eq!(renamed.status.code(), Some(0), "stdout: {o}\nstderr: {e}");
    assert!(
        o.contains("bob-2") && o.contains("'Bob' -> 'Bobby+work@Example.Test'"),
        "old and new are printed: {o}"
    );
    // Only the login name changed; the id and the other user are as they were.
    assert_eq!(username_of(dir, "bob-2"), "Bobby+work@Example.Test");
    assert_eq!(username_of(dir, "bob"), "bob");

    // The next start no longer refuses: `--init` succeeds and the index now exists.
    let init = run(&mut vamd(dir, &["--init"]));
    let (o, e) = (text(&init.stdout), text(&init.stderr));
    assert_eq!(init.status.code(), Some(0), "stdout: {o}\nstderr: {e}");
    assert!(o.contains("admin key: "), "{o}");
    assert!(!e.contains(CLASH_HEADER), "{e}");
    assert!(index_exists(dir), "the next start creates the index");

    // So does a start that serves: whatever it does next, it is not the clash refusal.
    let (code, so, se) = run_for(&mut vamd(dir, &[]), Duration::from_secs(15));
    assert!(
        !se.contains(CLASH_HEADER) && !so.contains(CLASH_HEADER),
        "code {code:?}\nstdout: {so}\nstderr: {se}"
    );
}

#[test]
fn rename_user_refuses_bad_names_unknown_ids_and_a_name_that_would_still_clash() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    seed_clashing_db(dir);
    for (args, needle) in [
        (vec!["--rename-user", "bob-2", "bob"], "already taken"),
        (vec!["--rename-user", "bob-2", "BOB"], "already taken"),
        (vec!["--rename-user", "bob-2", "usr_chosen"], "usr_"),
        (vec!["--rename-user", "bob-2", "bootstrap"], "reserved"),
        (vec!["--rename-user", "bob-2", "a@b@c"], "at most one '@'"),
        (vec!["--rename-user", "bob-2", "has space"], "ASCII letters"),
        (vec!["--rename-user", "nobody", "whoever"], "not found"),
        // A login name is not an id: the flag takes the user id.
        (vec!["--rename-user", "Bob", "whoever"], "not found"),
    ] {
        let out = run(&mut vamd(dir, &args));
        let (o, e) = (text(&out.stdout), text(&out.stderr));
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?}\nstdout: {o}\nstderr: {e}"
        );
        assert!(
            e.contains(needle),
            "{args:?}: {needle:?} missing from stderr:\n{e}"
        );
        assert!(!o.contains("Renamed"), "{args:?}: {o}");
    }
    assert_eq!(
        username_of(dir, "bob-2"),
        "Bob",
        "no refused rename changed a row"
    );
    assert_eq!(username_of(dir, "bob"), "bob");

    // A missing value is an error, not a start.
    for args in [vec!["--rename-user"], vec!["--rename-user", "bob-2"]] {
        let out = run(&mut vamd(dir, &args));
        assert_eq!(
            out.status.code(),
            Some(2),
            "{args:?}: {}",
            text(&out.stderr)
        );
        assert!(
            text(&out.stderr).contains("--rename-user needs a user id and a new username"),
            "{}",
            text(&out.stderr)
        );
    }
}

#[test]
fn reset_password_still_works_on_a_clashing_database() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path();
    seed_clashing_db(dir);
    let reset = |who: &str| {
        let mut cmd = vamd(dir, &["--reset-password", who]);
        cmd.env("VAMD_RESET_PASSWORD", "fresh-offline-secret");
        run(&mut cmd)
    };

    // By the id of either clashing user, and by the exact-case name.
    for (who, id, name) in [
        ("bob-2", "bob-2", "Bob"),
        ("bob", "bob", "bob"),
        ("Bob", "bob-2", "Bob"),
    ] {
        let out = reset(who);
        let (o, e) = (text(&out.stdout), text(&out.stderr));
        assert_eq!(
            out.status.code(),
            Some(0),
            "{who}\nstdout: {o}\nstderr: {e}"
        );
        assert!(
            o.contains(&format!(
                "Password reset for user '{id}' (username '{name}')"
            )),
            "{who}: {o}"
        );
        assert!(!e.contains(CLASH_HEADER), "{who}: {e}");
    }
    // A name that matches both only by letter case is ambiguous: refused, not guessed.
    let out = reset("BOB");
    assert_eq!(out.status.code(), Some(1), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("more than one user"),
        "{}",
        text(&out.stderr)
    );

    // The password really was set, and the clash is still there to fix.
    {
        let store = SqliteWorkflowStore::open(db_path(dir)).expect("open");
        assert!(store
            .verify_user_password("bob-2", "fresh-offline-secret")
            .expect("verify"));
        assert_eq!(store.username_clashes().len(), 1);
    }

    // After the fix, a reset by the new name in any letter case follows the one rule.
    let renamed = run(&mut vamd(dir, &["--rename-user", "bob-2", "Bobby"]));
    assert_eq!(renamed.status.code(), Some(0), "{}", text(&renamed.stderr));
    let out = reset("BOBBY");
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        text(&out.stdout).contains("Password reset for user 'bob-2' (username 'Bobby')"),
        "{}",
        text(&out.stdout)
    );
}
