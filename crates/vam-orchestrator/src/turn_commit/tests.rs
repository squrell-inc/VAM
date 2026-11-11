use super::*;

const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-turn-commit";
const SESSION: &str = "sess-tc";

fn store_with_run() -> Arc<SqliteWorkflowStore> {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    store
        .persist_run_spec(
            RUN,
            r#"{"inputs":{"prompt":"go"}}"#,
            Some(SESSION),
            "urn:vam:agent:test",
            None,
            "User::\"tester\"",
        )
        .expect("run");
    Arc::new(store)
}

fn store_with_session() -> Arc<SqliteWorkflowStore> {
    let store = store_with_run();
    {
        let connection = store.connection.lock().expect("lock");
        connection
            .execute(
                "INSERT INTO sessions (session_id, desired_state, observed_state, vm_id,
                    workspace_id, storage_profile_id, created_at, updated_at)
                 VALUES (?1, 'RUNNING', 'RUNNING', 'vm-tc', 'ws-a', 'profile', 0, 0)",
                [SESSION],
            )
            .expect("session");
    }
    store
}

fn digest(c: &str) -> String {
    c.repeat(32)
}

#[test]
fn state_strings_round_trip() {
    for state in [
        TurnCommitState::Pending,
        TurnCommitState::Complete,
        TurnCommitState::Failed,
    ] {
        assert_eq!(
            TurnCommitState::parse(state.as_str()).expect("parse"),
            state
        );
    }
    assert!(TurnCommitState::parse("done").is_err());
}

#[test]
fn noop_committer_refuses_and_has_no_status() {
    let noop = NoopTurnCommitter;
    let request = TurnCommitRequest {
        workspace_id: "ws".into(),
        session_id: "s".into(),
        run_id: RUN.into(),
        round: 1,
        messages_digest: digest("ab"),
        wrote_files: true,
    };
    assert!(matches!(
        noop.snapshot(&request),
        Err(TurnCommitError::Unavailable(_))
    ));
    assert!(noop.status(RUN, 1).expect("status").is_none());
    assert_eq!(RUN_ERROR_STORAGE_COMMIT_FAILED, "storage_commit_failed");
}

#[test]
fn a_commit_completes_when_the_manifest_and_the_boundary_are_both_recorded() {
    for manifest_first in [true, false] {
        let store = store_with_run();
        store
            .record_turn_commit_pending(RUN, SESSION, 1, 7, &digest("cd"), &digest("t1"))
            .expect("pending");
        store
            .record_turn_commit_pending(RUN, SESSION, 1, 7, &digest("cd"), &digest("t1"))
            .expect("repeat is a no-op");
        assert!(store
            .record_turn_commit_pending(RUN, SESSION, 1, 7, &digest("cd"), &digest("t2"))
            .is_err());
        assert!(store
            .record_turn_commit_pending(RUN, SESSION, 2, 7, "", &digest("t1"))
            .is_err());
        assert!(store
            .record_turn_commit_pending(RUN, SESSION, 2, 7, &digest("cd"), "")
            .is_err());

        if manifest_first {
            assert!(!store
                .set_turn_commit_manifest(RUN, 1, "manifest-1")
                .unwrap());
            assert_eq!(
                store.turn_commit(RUN, 1).unwrap(),
                Some((TurnCommitState::Pending, "manifest-1".into()))
            );
            let matched = store
                .match_turn_commit_tail(RUN, &digest("t1"), 9, &digest("9a"))
                .unwrap();
            assert_eq!(matched, Some((1, true)));
        } else {
            let matched = store
                .match_turn_commit_tail(RUN, &digest("t1"), 9, &digest("9a"))
                .unwrap();
            assert_eq!(matched, Some((1, false)));
            assert!(store
                .set_turn_commit_manifest(RUN, 1, "manifest-1")
                .unwrap());
        }
        let row = store.turn_commit_row(RUN, 1).unwrap().unwrap();
        assert_eq!(row.state, TurnCommitState::Complete);
        assert_eq!(row.manifest_hash, "manifest-1");
        assert_eq!(row.boundary_seq, Some(9));
        assert_eq!(row.boundary_digest, digest("9a"));
        assert_eq!(
            (row.log_seq, row.log_digest.as_str()),
            (7, digest("cd").as_str())
        );
        // The boundary is recorded once: a second request with the same tail
        // does not move it.
        assert_eq!(
            store
                .match_turn_commit_tail(RUN, &digest("t1"), 20, &digest("20"))
                .unwrap(),
            None
        );
        assert_eq!(
            store.last_complete_turn_manifest(RUN, SESSION).unwrap(),
            Some("manifest-1".to_string())
        );
        assert_eq!(
            store.last_complete_turn_manifest(RUN, "other").unwrap(),
            None
        );
    }
}

#[test]
fn a_tail_nobody_announced_completes_nothing_and_the_newest_round_wins() {
    let store = store_with_run();
    for round in 1..=2u32 {
        store
            .record_turn_commit_pending(
                RUN,
                SESSION,
                round,
                round as u64,
                &digest("cd"),
                &digest("t"),
            )
            .unwrap();
    }
    assert_eq!(
        store
            .match_turn_commit_tail(RUN, &digest("zz"), 5, &digest("5a"))
            .unwrap(),
        None
    );
    // Both announced the same results (a retried round): the newest is matched.
    assert_eq!(
        store
            .match_turn_commit_tail(RUN, &digest("t"), 5, &digest("5a"))
            .unwrap(),
        Some((2, false))
    );
    assert!(store.set_turn_commit_manifest(RUN, 3, "m").is_err());
    assert!(store.set_turn_commit_manifest(RUN, 2, "").is_err());
}

#[test]
fn only_the_newest_complete_commit_of_the_session_is_the_rollback_target() {
    let store = store_with_run();
    for (round, session) in [(1u32, SESSION), (2, SESSION), (3, "other")] {
        let tail = digest(&format!("{round}"));
        store
            .record_turn_commit_pending(RUN, session, round, 1, &digest("cd"), &tail)
            .unwrap();
        store
            .set_turn_commit_manifest(RUN, round, &format!("m{round}"))
            .unwrap();
        store
            .match_turn_commit_tail(RUN, &tail, round as u64 * 10, &digest("aa"))
            .unwrap();
    }
    store
        .record_turn_commit_pending(RUN, SESSION, 4, 1, &digest("cd"), &digest("t4"))
        .unwrap();
    let last = store
        .last_complete_turn_commit(RUN, SESSION)
        .unwrap()
        .unwrap();
    assert_eq!((last.round, last.manifest_hash.as_str()), (2, "m2"));
    assert_eq!(last.boundary_seq, Some(20));
    // A stop marks what never completed as failed and keeps what did.
    assert_eq!(
        store
            .fail_pending_turn_commits(RUN, SESSION, "stopped")
            .unwrap(),
        1
    );
    assert_eq!(
        store.turn_commit(RUN, 4).unwrap().map(|(s, _)| s),
        Some(TurnCommitState::Failed)
    );
    assert_eq!(
        store.turn_commit(RUN, 2).unwrap().map(|(s, _)| s),
        Some(TurnCommitState::Complete)
    );
    assert_eq!(store.count_turn_commits(RUN).unwrap(), 4);
    assert!(
        store.fail_turn_commit(RUN, 2, "x").is_err(),
        "complete rows stay"
    );
}

#[test]
fn a_session_that_took_over_a_task_rolls_back_to_the_commit_an_earlier_session_took() {
    let store = store_with_run();
    for (round, session) in [(1u32, "earlier"), (2, "earlier")] {
        let tail = digest(&format!("{round}"));
        store
            .record_turn_commit_pending(RUN, session, round, 1, &digest("cd"), &tail)
            .unwrap();
        store
            .set_turn_commit_manifest(RUN, round, &format!("m{round}"))
            .unwrap();
        store
            .match_turn_commit_tail(RUN, &tail, round as u64 * 10, &digest("aa"))
            .unwrap();
    }
    // The new session has none of its own: the run's newest finished step is the target.
    assert!(store
        .last_complete_turn_commit(RUN, "new")
        .unwrap()
        .is_none());
    let found = store
        .last_complete_turn_commit_for_run(RUN, "new")
        .unwrap()
        .unwrap();
    assert_eq!((found.round, found.boundary_seq), (2, Some(20)));
    // Once it has its own, that one wins.
    store
        .record_turn_commit_pending(RUN, "new", 5, 1, &digest("cd"), &digest("t5"))
        .unwrap();
    store.set_turn_commit_manifest(RUN, 5, "m5").unwrap();
    store
        .match_turn_commit_tail(RUN, &digest("t5"), 50, &digest("aa"))
        .unwrap();
    assert_eq!(
        store
            .last_complete_turn_commit_for_run(RUN, "new")
            .unwrap()
            .unwrap()
            .round,
        5
    );
    // Rounds numbered past every row, whatever its state.
    assert_eq!(store.max_turn_commit_round(RUN).unwrap(), 5);
}

struct FakeCommitter {
    fail: Option<fn() -> TurnCommitError>,
}

impl FakeCommitter {
    fn ok() -> Arc<dyn TurnCommitter> {
        Arc::new(Self { fail: None })
    }
    fn failing(make: fn() -> TurnCommitError) -> Arc<dyn TurnCommitter> {
        Arc::new(Self { fail: Some(make) })
    }
}

impl TurnCommitter for FakeCommitter {
    fn snapshot(&self, request: &TurnCommitRequest) -> Result<SnapshotHandle, TurnCommitError> {
        if let Some(make) = self.fail {
            return Err(make());
        }
        Ok(SnapshotHandle(format!("manifest-{}", request.round)))
    }
    fn extract(&self, handle: SnapshotHandle) -> ExtractFuture {
        Box::pin(async move { Ok(handle.0) })
    }
    fn status(&self, _: &str, _: u32) -> Result<Option<TurnCommitState>, TurnCommitError> {
        Ok(Some(TurnCommitState::Pending))
    }
}

fn lease_for(run_id: &str) -> vam_spec::v1::ExecutionLease {
    vam_spec::v1::ExecutionLease {
        run_id: Some(vam_spec::v1::ResourceId {
            urn: run_id.to_string(),
        }),
        lifecycle_generation: 1,
        ..Default::default()
    }
}

fn request(run_id: &str, tail: &str, round: u32) -> CommitTurnRequest {
    CommitTurnRequest {
        lease: Some(lease_for(run_id)),
        checkpoint_digest: Some(vam_spec::v1::Digest {
            algorithm: 1,
            hex_hash: tail.to_string(),
        }),
        round,
        wrote_files: true,
    }
}

struct FixedHead(Option<(u64, String)>);

impl ThreadLogHeads for FixedHead {
    fn log_head(&self, _session_id: &str) -> Option<(u64, String)> {
        self.0.clone()
    }
}

fn heads(head: Option<(u64, String)>) -> Arc<dyn ThreadLogHeads> {
    Arc::new(FixedHead(head))
}

async fn commit(
    store: &Arc<SqliteWorkflowStore>,
    committer: &Arc<dyn TurnCommitter>,
    heads: &Arc<dyn ThreadLogHeads>,
    session: &str,
    request: &CommitTurnRequest,
) -> CommitTurnResponse {
    handle_commit_turn(store, committer, heads, session, request).await
}

#[tokio::test]
async fn commit_turn_is_refused_with_code_12_under_the_noop_committer() {
    let store = store_with_session();
    let noop: Arc<dyn TurnCommitter> = Arc::new(NoopTurnCommitter);
    let reply = commit(
        &store,
        &noop,
        &heads(None),
        SESSION,
        &request(RUN, &digest("ab"), 1),
    )
    .await;
    assert!(!reply.accepted);
    assert_eq!(reply.error.as_ref().map(|e| e.code), Some(12));
    assert_eq!(reply.state, WireState::Unspecified as i32);
    assert!(store.turn_commit(RUN, 1).expect("read").is_none());
}

#[tokio::test]
async fn commit_turn_records_the_hosts_log_head_and_the_guests_digest_and_completes_later() {
    let store = store_with_session();
    let h = heads(Some((9, digest("9a"))));
    let reply = commit(
        &store,
        &FakeCommitter::ok(),
        &h,
        SESSION,
        &request(RUN, &digest("ab"), 1),
    )
    .await;
    assert!(reply.accepted, "{:?}", reply.error);
    assert_eq!(reply.state, WireState::Pending as i32);
    assert!(reply.manifest_hash.is_empty());
    assert_eq!(
        store.turn_commit_log_position(RUN, 1).expect("read"),
        Some((9, digest("9a")))
    );
    // The extraction task records the manifest; the commit is not complete
    // until the host sees the results.
    for _ in 0..50 {
        if store.turn_commit(RUN, 1).unwrap().map(|(_, m)| m) == Some("manifest-1".to_string()) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        store.turn_commit(RUN, 1).unwrap(),
        Some((TurnCommitState::Pending, "manifest-1".into()))
    );
    store
        .match_turn_commit_tail(RUN, &digest("ab"), 11, &digest("11"))
        .unwrap();
    assert_eq!(
        store.turn_commit(RUN, 1).unwrap().map(|(s, _)| s),
        Some(TurnCommitState::Complete)
    );
    // A retry of the same round answers with the state and records nothing new.
    let again = commit(
        &store,
        &FakeCommitter::ok(),
        &h,
        SESSION,
        &request(RUN, &digest("ab"), 1),
    )
    .await;
    assert!(again.accepted);
    assert_eq!(again.state, WireState::Complete as i32);
    assert_eq!(again.manifest_hash, "manifest-1");
    assert_eq!(store.count_turn_commits(RUN).unwrap(), 1);
}

#[tokio::test]
async fn commit_turn_without_a_log_head_records_nothing() {
    let store = store_with_session();
    let reply = commit(
        &store,
        &FakeCommitter::ok(),
        &heads(None),
        SESSION,
        &request(RUN, &digest("ab"), 1),
    )
    .await;
    assert_eq!(reply.error.as_ref().map(|e| e.code), Some(9));
    assert!(store.turn_commit(RUN, 1).expect("read").is_none());
}

#[tokio::test]
async fn commit_turn_rejects_bad_inputs_foreign_sessions_and_repeats_with_other_results() {
    let store = store_with_session();
    let h = heads(Some((1, digest("01"))));
    let ok = FakeCommitter::ok();
    let d = digest("ab");
    let round_zero = commit(&store, &ok, &h, SESSION, &request(RUN, &d, 0)).await;
    assert_eq!(round_zero.error.as_ref().map(|e| e.code), Some(3));
    let mut no_digest = request(RUN, &d, 3);
    no_digest.checkpoint_digest = None;
    let missing = commit(&store, &ok, &h, SESSION, &no_digest).await;
    assert_eq!(missing.error.as_ref().map(|e| e.code), Some(3));
    let other = commit(&store, &ok, &h, "sess-other", &request(RUN, &d, 4)).await;
    assert_eq!(other.error.as_ref().map(|e| e.code), Some(7));
    assert!(store.turn_commit(RUN, 4).expect("read").is_none());

    assert!(
        commit(&store, &ok, &h, SESSION, &request(RUN, &d, 5))
            .await
            .accepted
    );
    let clash = commit(&store, &ok, &h, SESSION, &request(RUN, &digest("cd"), 5)).await;
    assert_eq!(clash.error.as_ref().map(|e| e.code), Some(6));
}

#[tokio::test]
async fn commit_turn_is_refused_once_the_session_is_being_stopped() {
    let store = store_with_session();
    store.session_ends.track(SESSION);
    assert!(store.session_ends.begin_stop(SESSION));
    let reply = commit(
        &store,
        &FakeCommitter::ok(),
        &heads(Some((1, digest("01")))),
        SESSION,
        &request(RUN, &digest("ab"), 1),
    )
    .await;
    assert_eq!(reply.error.as_ref().map(|e| e.code), Some(9));
    assert!(store.turn_commit(RUN, 1).expect("read").is_none());
}

#[tokio::test]
async fn a_run_cannot_take_more_commits_than_the_cap() {
    let store = store_with_session();
    for round in 1..=MAX_TURN_COMMITS_PER_RUN {
        store
            .record_turn_commit_pending(RUN, SESSION, round, 1, &digest("cd"), &digest("ab"))
            .unwrap();
    }
    let reply = commit(
        &store,
        &FakeCommitter::ok(),
        &heads(Some((1, digest("01")))),
        SESSION,
        &request(RUN, &digest("ef"), MAX_TURN_COMMITS_PER_RUN + 1),
    )
    .await;
    assert_eq!(reply.error.as_ref().map(|e| e.code), Some(8));
}

#[tokio::test]
async fn a_storage_failure_is_a_typed_refusal_that_names_no_host_path_and_raises_the_event() {
    let store = store_with_session();
    let failing = FakeCommitter::failing(|| {
        TurnCommitError::Snapshot(
            "No space left on device at /var/lib/vam/drive9/sessions/x".into(),
        )
    });
    let reply = commit(
        &store,
        &failing,
        &heads(Some((1, digest("01")))),
        SESSION,
        &request(RUN, &digest("ab"), 1),
    )
    .await;
    let error = reply.error.expect("refused");
    assert_eq!(error.code, 13);
    assert!(error.message.starts_with(RUN_ERROR_STORAGE_COMMIT_FAILED));
    assert!(!error.message.contains('/'), "{}", error.message);
    assert!(store.turn_commit(RUN, 1).unwrap().is_none());
    let events = store.read_outbox_for_run(RUN, 0).unwrap();
    let raised = events
        .iter()
        .any(|e| e.event_type == crate::events::TYPE_WORKSPACE_COMMIT_FAILED);
    assert!(raised, "the commit_failed event was not emitted");
    assert!(
        events
            .iter()
            .filter(|e| e.event_type == crate::events::TYPE_WORKSPACE_COMMIT_FAILED)
            .all(|e| !e.payload.contains("/var/lib")),
        "the event names a host path"
    );
}

fn unscoped_recorder() -> Arc<crate::thread_recorder::ThreadRecorder> {
    Arc::new(crate::thread_recorder::ThreadRecorder::with_resolver(
        SESSION.into(),
        std::path::PathBuf::new(),
        Arc::new(|| Err("none".into())),
    ))
}

#[tokio::test]
async fn a_session_cannot_take_more_commits_than_the_model_requests_the_host_saw() {
    let store = store_with_session();
    store.register_thread_recorder(SESSION, unscoped_recorder());
    let reply = commit(
        &store,
        &FakeCommitter::ok(),
        &heads(Some((1, digest("01")))),
        SESSION,
        &request(RUN, &digest("ab"), 1),
    )
    .await;
    assert_eq!(reply.error.as_ref().map(|e| e.code), Some(3));
    assert!(store.turn_commit(RUN, 1).unwrap().is_none());
}

#[tokio::test]
async fn a_sealed_recorder_takes_no_more_commits() {
    let store = store_with_session();
    let recorder = unscoped_recorder();
    store.register_thread_recorder(SESSION, recorder.clone());
    recorder.record_stop().await;
    assert!(recorder.is_sealed());
    let reply = commit(
        &store,
        &FakeCommitter::ok(),
        &heads(Some((1, digest("01")))),
        SESSION,
        &request(RUN, &digest("ab"), 1),
    )
    .await;
    assert_eq!(reply.error.as_ref().map(|e| e.code), Some(9));
}

// ---------------------------------------------------------------------------
// The image committer, against real ext4 images (skipped without mke2fs).
// ---------------------------------------------------------------------------

pub(crate) mod image {
    use super::*;
    use vam_storage::{
        CacheManager, ChunkStore, Drive9Backend, Drive9Config, E2fsImageIo, ManifestManager,
        SnapshotLimits, StorageProfile, WorkspaceManager,
    };

    pub(crate) fn have_mke2fs() -> bool {
        ["mke2fs", "debugfs"].iter().all(|t| {
            std::process::Command::new(t)
                .arg("-V")
                .output()
                .map(|o| o.status.success() || !o.stderr.is_empty())
                .unwrap_or(false)
        })
    }

    macro_rules! need_mke2fs {
        () => {
            if !have_mke2fs() {
                eprintln!("skipping: mke2fs or debugfs is not installed");
                return;
            }
        };
    }

    pub(crate) struct Rig {
        pub dir: tempfile::TempDir,
        pub wm: Arc<WorkspaceManager>,
        pub store: Arc<SqliteWorkflowStore>,
    }

    pub(crate) fn rig() -> Rig {
        let dir = tempfile::tempdir().expect("dir");
        let layout = Drive9Config::new(dir.path()).layout();
        let chunks = ChunkStore::new(layout.clone());
        let manifests = ManifestManager::new(layout.clone(), chunks.clone());
        let cache = CacheManager::new(layout.clone(), chunks.clone(), 1 << 24).expect("cache");
        let wm = Arc::new(WorkspaceManager::new(
            layout,
            chunks,
            manifests,
            cache,
            Box::new(E2fsImageIo::new()),
        ));
        wm.materialize(
            SESSION,
            "ws-a",
            &StorageProfile::new("t".into(), 8 * 1024 * 1024),
            None,
            None,
            None,
        )
        .expect("materialize");
        Rig {
            dir,
            wm,
            store: store_with_session(),
        }
    }

    pub(crate) fn request_for(round: u32) -> TurnCommitRequest {
        TurnCommitRequest {
            workspace_id: "ws-a".into(),
            session_id: SESSION.into(),
            run_id: RUN.into(),
            round,
            messages_digest: digest("ab"),
            wrote_files: true,
        }
    }

    pub(crate) fn guest_write(rig: &Rig, guest_path: &str, body: &[u8]) {
        let image = rig.wm.layout().session_image_path(SESSION);
        let host = rig.dir.path().join("guest-src");
        std::fs::write(&host, body).expect("src");
        let mut cur = String::new();
        if let Some((dir, _)) = guest_path.rsplit_once('/') {
            for part in dir.split('/') {
                cur = format!("{cur}/{part}");
                let _ = std::process::Command::new("debugfs")
                    .args(["-w", "-R", &format!("mkdir {cur}")])
                    .arg(&image)
                    .output();
            }
        }
        let _ = std::process::Command::new("debugfs")
            .args(["-w", "-R", &format!("rm {guest_path}")])
            .arg(&image)
            .output();
        let out = std::process::Command::new("debugfs")
            .args([
                "-w",
                "-R",
                &format!("write {} {guest_path}", host.display()),
            ])
            .arg(&image)
            .output()
            .expect("debugfs");
        assert!(out.status.success(), "{:?}", out.stderr);
    }

    /// Takes the turn copy for `round` and records the commit as complete (the
    /// manifest half is the deferred token, the boundary is the recorder's).
    pub(crate) fn take_turn(rig: &Rig, committer: &ImageTurnCommitter, round: u32) {
        let handle = committer.snapshot(&request_for(round)).expect("snapshot");
        assert_eq!(handle.0, image_token(round));
        let tail = digest(&format!("{round}t"));
        rig.store
            .record_turn_commit_pending(RUN, SESSION, round, round as u64, &digest("cd"), &tail)
            .expect("pending");
        rig.store
            .set_turn_commit_manifest(RUN, round, &handle.0)
            .expect("token");
        rig.store
            .match_turn_commit_tail(RUN, &tail, round as u64 + 1, &digest("bb"))
            .expect("boundary");
    }

    fn turns(rig: &Rig) -> Vec<u32> {
        rig.wm.turn_copy_rounds(SESSION)
    }

    fn block_on<T>(fut: impl std::future::Future<Output = T>) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("rt")
            .block_on(fut)
    }

    #[test]
    fn the_copy_lands_under_turns_and_extraction_is_deferred() {
        need_mke2fs!();
        let rig = rig();
        guest_write(&rig, "workspace/note.txt", b"kiwi-42");
        let committer = ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone());
        let handle = committer.snapshot(&request_for(1)).expect("snapshot");
        assert_eq!(handle.0, "image:1");
        assert_eq!(parse_image_token(&handle.0), Some(1));
        assert_eq!(parse_image_token("deadbeef"), None);
        let copy = rig.wm.layout().session_turn_image_path(SESSION, 1);
        assert!(copy.is_file());
        // Extraction hands back the token and writes no manifest.
        let manifests = rig.dir.path().join("manifests");
        let count = |p: &std::path::Path| walkdir_count(p);
        let before = count(&manifests);
        let extracted = block_on(committer.extract(handle));
        assert_eq!(extracted.expect("extract"), "image:1");
        assert_eq!(before, count(&manifests), "no manifest before a stop");
        assert_eq!(committer.status(RUN, 1).unwrap(), None);
    }

    fn walkdir_count(path: &std::path::Path) -> usize {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        entries
            .flatten()
            .map(|e| 1 + walkdir_count(&e.path()))
            .sum()
    }

    #[test]
    fn the_pending_cap_prunes_older_failed_and_orphan_copies() {
        need_mke2fs!();
        let rig = rig();
        let committer = ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone());
        // Round 1 complete, round 2 failed, rounds 3 and 4 pending, round 9 an
        // orphan from an earlier run.
        take_turn(&rig, &committer, 1);
        for round in [2u32, 3, 4, 9] {
            rig.wm
                .snapshot_turn_image(SESSION, round, &SnapshotLimits::default())
                .expect("copy");
        }
        rig.store
            .record_turn_commit_pending(RUN, SESSION, 2, 2, &digest("cd"), &digest("2t"))
            .unwrap();
        rig.store.fail_turn_commit(RUN, 2, "x").unwrap();
        for round in [3u32, 4] {
            rig.store
                .record_turn_commit_pending(
                    RUN,
                    SESSION,
                    round,
                    round as u64,
                    &digest("cd"),
                    &digest(&format!("{round}t")),
                )
                .unwrap();
        }
        assert_eq!(turns(&rig), vec![1, 2, 3, 4, 9]);

        committer.snapshot(&request_for(5)).expect("snapshot 5");
        // The newest complete copy, the newest pending one and the new copy stay.
        assert_eq!(turns(&rig), vec![1, 4, 5]);
        assert_eq!(
            rig.store.turn_commit(RUN, 3).unwrap().map(|(s, _)| s),
            Some(TurnCommitState::Failed)
        );
        assert_eq!(
            rig.store.turn_commit(RUN, 4).unwrap().map(|(s, _)| s),
            Some(TurnCommitState::Pending)
        );
        // A later complete commit retires the older pending one.
        rig.store
            .record_turn_commit_pending(RUN, SESSION, 5, 5, &digest("cd"), &digest("5t"))
            .unwrap();
        rig.store
            .set_turn_commit_manifest(RUN, 5, "image:5")
            .unwrap();
        rig.store
            .match_turn_commit_tail(RUN, &digest("5t"), 9, &digest("bb"))
            .unwrap();
        committer.snapshot(&request_for(6)).expect("snapshot 6");
        assert_eq!(turns(&rig), vec![5, 6]);
        assert_eq!(
            rig.store.turn_commit(RUN, 4).unwrap().map(|(s, _)| s),
            Some(TurnCommitState::Failed)
        );
    }

    #[tokio::test]
    async fn a_copy_failure_is_a_code_13_refusal_naming_no_host_path_and_leaves_no_file() {
        need_mke2fs!();
        let rig = rig();
        let committer: Arc<dyn TurnCommitter> = Arc::new(
            ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone()).with_limits(
                SnapshotLimits {
                    try_reflink: false,
                    free_space_override: Some(0),
                    ..SnapshotLimits::default()
                },
            ),
        );
        let reply = handle_commit_turn(
            &rig.store,
            &committer,
            &heads(Some((1, digest("01")))),
            SESSION,
            &request(RUN, &digest("ab"), 1),
        )
        .await;
        let error = reply.error.expect("refused");
        assert_eq!(error.code, 13);
        assert!(error.message.starts_with(RUN_ERROR_STORAGE_COMMIT_FAILED));
        assert!(!error.message.contains('/'), "{}", error.message);
        assert!(turns(&rig).is_empty(), "a partial copy stayed");
        assert!(rig.store.turn_commit(RUN, 1).unwrap().is_none());
        let events = rig.store.read_outbox_for_run(RUN, 0).unwrap();
        assert!(events
            .iter()
            .any(|e| e.event_type == crate::events::TYPE_WORKSPACE_COMMIT_FAILED));
        let host = rig.dir.path().to_str().unwrap().to_string();
        assert!(events.iter().all(|e| !e.payload.contains(&host)));
    }

    #[tokio::test]
    async fn the_flow_through_handle_commit_turn_completes_with_the_token() {
        need_mke2fs!();
        let rig = rig();
        let committer: Arc<dyn TurnCommitter> =
            Arc::new(ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone()));
        let reply = handle_commit_turn(
            &rig.store,
            &committer,
            &heads(Some((1, digest("01")))),
            SESSION,
            &request(RUN, &digest("ab"), 1),
        )
        .await;
        assert!(reply.accepted, "{reply:?}");
        // The extraction task runs in the background; wait for the token.
        for _ in 0..100 {
            if rig.store.turn_commit(RUN, 1).unwrap().map(|(_, m)| m) == Some("image:1".into()) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            rig.store.turn_commit(RUN, 1).unwrap(),
            Some((TurnCommitState::Pending, "image:1".into()))
        );
        assert!(rig
            .store
            .match_turn_commit_tail(RUN, &digest("ab"), 5, &digest("5a"))
            .unwrap()
            .is_some_and(|(_, done)| done));
        assert_eq!(
            rig.store
                .last_complete_turn_manifest(RUN, SESSION)
                .unwrap()
                .as_deref(),
            Some("image:1")
        );
    }

    #[tokio::test]
    async fn a_stop_request_refuses_the_commit_and_takes_no_copy() {
        need_mke2fs!();
        let rig = rig();
        rig.store.session_ends.track(SESSION);
        assert!(rig.store.session_ends.begin_stop(SESSION));
        let committer: Arc<dyn TurnCommitter> =
            Arc::new(ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone()));
        let reply = handle_commit_turn(
            &rig.store,
            &committer,
            &heads(Some((1, digest("01")))),
            SESSION,
            &request(RUN, &digest("ab"), 1),
        )
        .await;
        assert_eq!(reply.error.as_ref().map(|e| e.code), Some(9));
        // The committer itself refuses too (a stop that lands during the call).
        assert!(matches!(
            committer.snapshot(&request_for(1)),
            Err(TurnCommitError::Stopping)
        ));
        assert!(turns(&rig).is_empty());
    }

    #[test]
    fn a_snapshot_waits_for_the_session_lock_a_reclaim_holds() {
        need_mke2fs!();
        let rig = rig();
        let committer = ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone());
        let held = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (wm, held2, released2) = (rig.wm.clone(), held.clone(), released.clone());
        let holder = std::thread::spawn(move || {
            wm.with_session_lock(SESSION, || {
                held2.store(true, std::sync::atomic::Ordering::SeqCst);
                std::thread::sleep(std::time::Duration::from_millis(300));
                released2.store(true, std::sync::atomic::Ordering::SeqCst);
            });
        });
        while !held.load(std::sync::atomic::Ordering::SeqCst) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        committer.snapshot(&request_for(1)).expect("snapshot");
        assert!(
            released.load(std::sync::atomic::Ordering::SeqCst),
            "the snapshot ran while the lock was held"
        );
        holder.join().unwrap();
    }

    #[test]
    fn the_committer_follows_the_backend() {
        need_mke2fs!();
        let rig = rig();
        let image = committer_for(&rig.wm, &rig.store);
        let handle = image.snapshot(&request_for(1)).expect("image snapshot");
        assert_eq!(handle.0, "image:1");

        let dir = tempfile::tempdir().unwrap();
        let wm = Arc::new(
            vam_storage::open_workspace_manager_with(dir.path(), Drive9Backend::Directory).unwrap(),
        );
        wm.materialize(
            SESSION,
            "ws-a",
            &StorageProfile::new("t".into(), 8 * 1024 * 1024),
            None,
            None,
            None,
        )
        .unwrap();
        let directory = committer_for(&wm, &rig.store);
        let handle = directory.snapshot(&request_for(1)).expect("tree snapshot");
        assert!(parse_image_token(&handle.0).is_none(), "{}", handle.0);
        assert!(wm.turn_copy_rounds(SESSION).is_empty());
    }

    fn resolve(rig: &Rig) -> Result<Option<String>, vam_storage::Drive9Error> {
        resolve_stop_base(&rig.store, &rig.wm, RUN, SESSION, "ws-a")
    }

    fn files_of(rig: &Rig, hash: &str) -> Vec<String> {
        let layout = rig.wm.layout().clone();
        ManifestManager::new(layout.clone(), ChunkStore::new(layout))
            .get_manifest("ws-a", hash)
            .unwrap()
            .files
            .into_iter()
            .map(|f| f.path)
            .collect()
    }

    #[test]
    fn a_stop_resolves_the_newest_token_and_stores_the_real_hash_back() {
        need_mke2fs!();
        let rig = rig();
        let committer = ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone());
        assert_eq!(resolve(&rig).unwrap(), None, "no commit: the run's start");
        guest_write(&rig, "workspace/note.txt", b"kiwi-42");
        take_turn(&rig, &committer, 1);
        guest_write(&rig, "workspace/partial.txt", b"half");
        let hash = resolve(&rig).unwrap().expect("a base");
        let files = files_of(&rig, &hash);
        assert!(
            files.contains(&"workspace/note.txt".to_string()),
            "{files:?}"
        );
        assert!(!files.contains(&"workspace/partial.txt".to_string()));
        // The row now holds the real hash, and a second stop reads nothing.
        assert_eq!(
            rig.store.last_complete_turn_manifest(RUN, SESSION).unwrap(),
            Some(hash.clone())
        );
        std::fs::remove_file(rig.wm.layout().session_turn_image_path(SESSION, 1)).unwrap();
        assert_eq!(resolve(&rig).unwrap(), Some(hash));
    }

    #[test]
    fn an_unreadable_newest_copy_falls_back_marks_the_row_failed_and_raises_the_event() {
        need_mke2fs!();
        let rig = rig();
        let committer = ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone());
        guest_write(&rig, "workspace/note.txt", b"one");
        take_turn(&rig, &committer, 1);
        // Round 2's copy is taken straight away (no prune): both copies exist.
        guest_write(&rig, "workspace/second.txt", b"two");
        rig.wm
            .snapshot_turn_image(SESSION, 2, &SnapshotLimits::default())
            .unwrap();
        let tail = digest("2t");
        rig.store
            .record_turn_commit_pending(RUN, SESSION, 2, 2, &digest("cd"), &tail)
            .unwrap();
        rig.store
            .set_turn_commit_manifest(RUN, 2, "image:2")
            .unwrap();
        rig.store
            .match_turn_commit_tail(RUN, &tail, 3, &digest("bb"))
            .unwrap();
        std::fs::write(
            rig.wm.layout().session_turn_image_path(SESSION, 2),
            vec![0xffu8; 1 << 20],
        )
        .unwrap();

        let hash = resolve(&rig).unwrap().expect("falls back to round 1");
        let files = files_of(&rig, &hash);
        assert!(
            files.contains(&"workspace/note.txt".to_string()),
            "{files:?}"
        );
        assert!(
            !files.contains(&"workspace/second.txt".to_string()),
            "{files:?}"
        );
        assert_eq!(
            rig.store.turn_commit(RUN, 2).unwrap().map(|(s, _)| s),
            Some(TurnCommitState::Failed)
        );
        assert_eq!(
            rig.store
                .last_complete_turn_commit(RUN, SESSION)
                .unwrap()
                .map(|r| r.round),
            Some(1)
        );
        let events = rig.store.read_outbox_for_run(RUN, 0).unwrap();
        assert!(events
            .iter()
            .any(|e| e.event_type == crate::events::TYPE_WORKSPACE_COMMIT_FAILED));
    }

    #[test]
    fn with_every_copy_unreadable_the_base_is_the_runs_start_and_an_io_error_is_returned() {
        need_mke2fs!();
        let rig = rig();
        let committer = ImageTurnCommitter::new(rig.wm.clone(), rig.store.clone());
        take_turn(&rig, &committer, 1);
        std::fs::write(
            rig.wm.layout().session_turn_image_path(SESSION, 1),
            vec![0xffu8; 1 << 20],
        )
        .unwrap();
        assert_eq!(resolve(&rig).unwrap(), None);
        // A missing copy is an I/O error, not corruption: the settle fails.
        take_turn(&rig, &committer, 2);
        std::fs::remove_file(rig.wm.layout().session_turn_image_path(SESSION, 2)).unwrap();
        assert!(resolve(&rig).is_err());
    }
}
