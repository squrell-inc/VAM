//! Keyset paging in the store (IT-2.5): the order, the cursor, and what happens between pages.

use rusqlite::params;
use vam_spec::v1::ResourceId;

use crate::workflow::SqliteWorkflowStore;

fn store() -> SqliteWorkflowStore {
    SqliteWorkflowStore::in_memory().expect("store")
}

fn pending_approval(store: &SqliteWorkflowStore, id: &str, created_at: i64) {
    let connection = store.connection.lock().unwrap();
    connection
        .execute(
            "INSERT INTO runs (run_id, state, revision, owner_user_id, created_at)
             VALUES (?1, 'RUNNING', 0, 'local', 1)",
            [format!("urn:run:{id}")],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO approvals (request_id, run_id, step_id, attempt_id, checkpoint_digest,
                continuation_generation, created_at, published)
             VALUES (?1, ?2, 's', 'a', 'd', 1, ?3, 1)",
            params![id, format!("urn:run:{id}"), created_at],
        )
        .unwrap();
}

#[test]
fn approvals_page_oldest_first_across_ties_and_decisions() {
    let store = store();
    // Two share a timestamp: the request id breaks the tie, so no page repeats or skips one.
    for (id, at) in [("a1", 10), ("a2", 10), ("a3", 11), ("a4", 12), ("a5", 13)] {
        pending_approval(&store, id, at);
    }
    let first = store
        .list_published_approvals_page_for_owner("local", 3, None)
        .unwrap();
    let ids: Vec<_> = first.iter().map(|a| a.request_id.as_str()).collect();
    assert_eq!(ids, ["a1", "a2", "a3"]);
    let last = first.last().unwrap();
    // The first row is decided and a newer one arrives between the pages.
    store
        .connection
        .lock()
        .unwrap()
        .execute(
            "UPDATE approvals SET decision = 'approve' WHERE request_id = 'a1'",
            [],
        )
        .unwrap();
    pending_approval(&store, "a6", 14);
    let second = store
        .list_published_approvals_page_for_owner(
            "local",
            3,
            Some((last.created_at, last.request_id.clone())),
        )
        .unwrap();
    let ids: Vec<_> = second.iter().map(|a| a.request_id.as_str()).collect();
    assert_eq!(ids, ["a4", "a5", "a6"]);
    let end = second.last().unwrap();
    let empty = store
        .list_published_approvals_page_for_owner(
            "local",
            3,
            Some((end.created_at, end.request_id.clone())),
        )
        .unwrap();
    assert!(empty.is_empty(), "an empty last page");
}

#[test]
fn approvals_page_is_the_owners_only() {
    let store = store();
    pending_approval(&store, "mine", 1);
    let theirs = store
        .list_published_approvals_page_for_owner("someone-else", 10, None)
        .unwrap();
    assert!(theirs.is_empty());
}

#[test]
fn sessions_page_newest_first_and_a_new_session_does_not_shift_the_next_page() {
    let store = store();
    for i in 0..7 {
        store
            .create_session(&format!("s-{i:02}"), None, &format!("vm-{i:02}"), "ws", "p")
            .unwrap();
    }
    let first = store
        .load_sessions_page_for_owner("local", 3, None)
        .unwrap();
    let ids: Vec<_> = first.iter().map(|s| s.session_id.as_str()).collect();
    // Same-second sessions order by id, descending.
    assert_eq!(ids, ["s-06", "s-05", "s-04"]);
    store
        .create_session("s-99", None, "vm-99", "ws", "p")
        .unwrap();
    let last = first.last().unwrap();
    let second = store
        .load_sessions_page_for_owner("local", 3, Some((last.created_at, last.session_id.clone())))
        .unwrap();
    let ids: Vec<_> = second.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(ids, ["s-03", "s-02", "s-01"]);
    let last = second.last().unwrap();
    let third = store
        .load_sessions_page_for_owner("local", 3, Some((last.created_at, last.session_id.clone())))
        .unwrap();
    let ids: Vec<_> = third.iter().map(|s| s.session_id.as_str()).collect();
    assert_eq!(ids, ["s-00"]);
}

#[test]
fn runs_page_newest_first_by_rowid_cursor() {
    let store = store();
    let run = |i: u32| ResourceId {
        urn: format!("urn:vam:run:local.vam.dev/acme/r{i}"),
    };
    for i in 0..5 {
        store.create_run(&run(i)).unwrap();
    }
    let name = |r: &(String, String, i64)| r.0.rsplit('/').next().unwrap().to_string();
    let first = store.list_runs_page_for_owner("local", 3, None).unwrap();
    assert_eq!(
        first.iter().map(name).collect::<Vec<_>>(),
        ["r4", "r3", "r2"]
    );
    store.create_run(&run(5)).unwrap();
    let second = store
        .list_runs_page_for_owner("local", 3, Some(first.last().unwrap().2))
        .unwrap();
    assert_eq!(second.iter().map(name).collect::<Vec<_>>(), ["r1", "r0"]);
}
