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

fn skill_manifest(urn: &str) -> vam_spec::v1::SkillManifest {
    vam_spec::v1::SkillManifest {
        resource_id: Some(ResourceId {
            urn: urn.to_string(),
        }),
        version: Some(vam_spec::v1::Version {
            major: 1,
            ..Default::default()
        }),
        description: "Test".to_string(),
        ..Default::default()
    }
}

/// `urn:vam:skill:local.vam.dev/acme/<name>` released by `owner` at 1.0.0.
fn released_skill(store: &SqliteWorkflowStore, name: &str, owner: &str) {
    let urn = format!("urn:vam:skill:local.vam.dev/acme/{name}");
    store
        .publish_skill_draft_with_owner(&skill_manifest(&urn), owner)
        .unwrap();
    store
        .release_skill(
            &urn,
            &vam_spec::v1::Version {
                major: 1,
                ..Default::default()
            },
        )
        .unwrap();
}

fn draft_skill(store: &SqliteWorkflowStore, name: &str, owner: &str) {
    let urn = format!("urn:vam:skill:local.vam.dev/acme/{name}");
    store
        .publish_skill_draft_with_owner(&skill_manifest(&urn), owner)
        .unwrap();
}

fn skill_names(rows: &[(vam_spec::v1::SkillRecord, crate::catalog::SkillPageKey)]) -> Vec<String> {
    rows.iter()
        .map(|(r, _)| {
            let urn = &r.spec.as_ref().unwrap().resource_id.as_ref().unwrap().urn;
            let channel = if r.channel == vam_spec::v1::SkillChannel::Draft as i32 {
                "draft"
            } else {
                "released"
            };
            format!("{}:{channel}", urn.rsplit('/').next().unwrap())
        })
        .collect()
}

#[test]
fn skills_list_released_only_by_default_and_own_drafts_only_on_request() {
    let store = store();
    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    released_skill(&store, "shared", "alice");
    draft_skill(&store, "alice-wip", "alice");
    draft_skill(&store, "bob-wip", "bob");

    for who in ["alice", "bob"] {
        let rows = store.list_skill_records_page(who, false, 50, None).unwrap();
        assert_eq!(skill_names(&rows), ["shared:released"], "{who} default");
    }
    let alice = store
        .list_skill_records_page("alice", true, 50, None)
        .unwrap();
    // A release keeps the owner's draft row beside it, so alice also sees her draft of `shared`.
    assert_eq!(
        skill_names(&alice),
        ["alice-wip:draft", "shared:released", "shared:draft"]
    );
    let bob = store
        .list_skill_records_page("bob", true, 50, None)
        .unwrap();
    assert_eq!(skill_names(&bob), ["bob-wip:draft", "shared:released"]);
    assert!(alice.iter().all(|(r, _)| r.owner_user_id == "alice"));
}

#[test]
fn skills_page_newest_first_and_a_new_release_does_not_shift_the_next_page() {
    let store = store();
    for i in 0..5 {
        released_skill(&store, &format!("s{i}"), "local");
    }
    let first = store
        .list_skill_records_page("local", false, 3, None)
        .unwrap();
    assert_eq!(
        skill_names(&first),
        ["s4:released", "s3:released", "s2:released"]
    );
    released_skill(&store, "s5", "local");
    let second = store
        .list_skill_records_page("local", false, 3, Some(first.last().unwrap().1))
        .unwrap();
    assert_eq!(skill_names(&second), ["s1:released", "s0:released"]);
}

/// A package of `urn:vam:skill:local.vam.dev/acme/<name>` saved as `owner`'s draft at 1.0.0-draft,
/// and released at 1.0.0 when `release`.
fn skill_package(
    store: &SqliteWorkflowStore,
    name: &str,
    owner: &str,
    digest: char,
    release: bool,
) {
    let urn = format!("urn:vam:skill:local.vam.dev/acme/{name}");
    let draft = vam_spec::v1::Version {
        major: 1,
        prerelease: "draft".to_string(),
        ..Default::default()
    };
    let digest: String = std::iter::repeat_n(digest, 64).collect();
    let mut spec = skill_manifest(&urn);
    spec.version = Some(draft.clone());
    let package = vam_spec::v1::SkillPackageRecord {
        resource_id: Some(ResourceId { urn: urn.clone() }),
        version: Some(draft),
        channel: vam_spec::v1::SkillChannel::Draft as i32,
        blob_relpath: crate::skill_packages::skill_blob_relpath(owner, &digest),
        digest,
        size_bytes: 64,
        spec: Some(spec),
        ..Default::default()
    };
    store.put_skill_package_with_owner(&package, owner).unwrap();
    if release {
        store
            .release_skill_package(
                &urn,
                &vam_spec::v1::Version {
                    major: 1,
                    ..Default::default()
                },
            )
            .unwrap();
    }
}

#[test]
fn skills_list_packages_and_manifests_and_a_released_package_hides_its_manifest() {
    let store = store();
    store.create_user("alice", "alice").unwrap();
    store.create_user("bob", "bob").unwrap();
    skill_package(&store, "packaged", "alice", 'a', true);
    released_skill(&store, "manifest-only", "alice");
    // The same URN as a released package: a ref resolves to the package, so the manifest is hidden.
    released_skill(&store, "packaged", "alice");
    skill_package(&store, "bob-pkg-wip", "bob", 'b', false);

    let rows = store
        .list_skill_records_page("alice", false, 50, None)
        .unwrap();
    let mut seen: Vec<(String, String)> = rows
        .iter()
        .map(|(r, _)| {
            let urn = &r.spec.as_ref().unwrap().resource_id.as_ref().unwrap().urn;
            (
                urn.rsplit('/').next().unwrap().to_string(),
                r.source.clone(),
            )
        })
        .collect();
    seen.sort();
    assert_eq!(
        seen,
        [
            ("manifest-only".to_string(), "manifest".to_string()),
            ("packaged".to_string(), "package".to_string()),
        ]
    );
    let alice = store
        .list_skill_records_page("alice", true, 50, None)
        .unwrap();
    assert!(
        skill_names(&alice).iter().all(|n| !n.starts_with("bob-")),
        "{:?}",
        skill_names(&alice)
    );
    let bob = store
        .list_skill_records_page("bob", true, 50, None)
        .unwrap();
    assert!(skill_names(&bob).contains(&"bob-pkg-wip:draft".to_string()));
}
