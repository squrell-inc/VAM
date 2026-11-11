use super::*;
use crate::cache::CacheManager;
use crate::config::{Drive9Config, StorageProfile};
use crate::disk::MockImageIo;
use crate::workspace::{Drive9Backend, WorkspaceManager};
use serde_json::json;
use tempfile::tempdir;

fn wire(role: &str, content: &str) -> WireMessage {
    WireMessage {
        role: role.into(),
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: String::new(),
    }
}

fn msg_record(round: u64, w: &WireMessage) -> NewRecord {
    let message = LogMessage::from_wire(w);
    let mut rec = NewRecord::new(kind::MESSAGE, "broker", message_body(&message, None));
    rec.round = round;
    rec.run = "urn:vam:run:t".into();
    rec.agent = "urn:vam:agent:t".into();
    rec
}

fn rec(kind_name: &str, body: Value) -> NewRecord {
    NewRecord::new(kind_name, "host", body)
}

fn new_store() -> (tempfile::TempDir, TreeThreadStore) {
    let dir = tempdir().unwrap();
    let store = TreeThreadStore::new(dir.path());
    (dir, store)
}

fn log_path(dir: &Path, thread: &str, first_seq: u64) -> PathBuf {
    dir.join(".vam/threads")
        .join(thread)
        .join("log")
        .join(segment_file_name(first_seq))
}

#[test]
fn wire_messages_round_trip_through_typed_parts() {
    let cases = vec![
        wire("user", ""),
        wire("user", "line one\nline two \u{1F600} \"quoted\" \\ back"),
        wire("system", "S"),
        WireMessage {
            role: "assistant".into(),
            content: String::new(),
            tool_calls: vec![(
                "c1".into(),
                "read_file".into(),
                "{\"path\": \"a b\"}".into(),
            )],
            tool_call_id: String::new(),
        },
        WireMessage {
            role: "assistant".into(),
            content: "thinking".into(),
            tool_calls: vec![
                ("c1".into(), "a".into(), "{}".into()),
                ("c2".into(), "b".into(), " {\"x\":1} ".into()),
            ],
            tool_call_id: String::new(),
        },
        wire("assistant", ""),
        WireMessage {
            role: "tool".into(),
            content: String::new(),
            tool_calls: vec![],
            tool_call_id: "c1".into(),
        },
        WireMessage {
            role: "tool".into(),
            content: "kiwi-42\n".into(),
            tool_calls: vec![],
            tool_call_id: "c2".into(),
        },
    ];
    for case in cases {
        let message = LogMessage::from_wire(&case);
        let json = serde_json::to_string(&message).unwrap();
        let back: LogMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(back.to_wire(), case, "{json}");
        assert_eq!(back.msg_sha256(), wire_sha256(&case));
    }
}

#[test]
fn bytes_survive_the_file_exactly() {
    let (dir, store) = new_store();
    let texts = [
        "",
        "plain",
        "unicode \u{00E9}\u{4E2D}\u{1F600}",
        "a\nb\r\nc\t\"d\"\\",
    ];
    let batch: Vec<NewRecord> = texts
        .iter()
        .map(|t| msg_record(1, &wire("user", t)))
        .collect();
    store.append("main", batch).unwrap();
    let page = store.read("main", 0, usize::MAX).unwrap();
    assert_eq!(page.integrity, Integrity::Ok);
    let got: Vec<String> = page
        .records
        .iter()
        .map(|r| r.message().unwrap().to_wire().content)
        .collect();
    assert_eq!(got, texts);
    // Every line is canonical JSON: re-serializing the parsed line reproduces it.
    let raw = fs::read_to_string(log_path(dir.path(), "main", 1)).unwrap();
    for line in raw.lines() {
        let v: Value = serde_json::from_str(line).unwrap();
        assert_eq!(canonical_json(&v), line);
    }
}

#[test]
fn header_fields_ids_and_digests() {
    let (_dir, store) = new_store();
    let receipt = store
        .append(
            "main",
            vec![
                msg_record(3, &wire("user", "hi")),
                msg_record(3, &wire("assistant", "yo")),
            ],
        )
        .unwrap();
    assert_eq!(receipt.first_seq, 1);
    assert_eq!(receipt.head.seq, 2);
    let page = store.read("main", 0, 10).unwrap();
    let first = &page.records[0];
    assert_eq!(first.v, 1);
    assert_eq!(first.prev, genesis_digest());
    assert_eq!(first.lane, "main");
    assert_eq!(first.round, 3);
    assert_eq!(first.id.len(), 26);
    assert!(first.id.bytes().all(|b| CROCKFORD.contains(&b)));
    assert_ne!(first.id, page.records[1].id);
    assert!(
        first.ts.ends_with('Z') && first.ts.len() == 24,
        "{}",
        first.ts
    );
    assert_eq!(page.records[1].prev, first.digest);
    assert_eq!(store.head("main").unwrap().unwrap(), page.records[1].head());
    assert_eq!(
        first.body_sha256,
        sha256_hex(canonical_json(first.body.as_ref().unwrap()).as_bytes())
    );
}

#[test]
fn iso_timestamp_is_correct() {
    assert_eq!(iso_timestamp(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(iso_timestamp(951_782_400_123), "2000-02-29T00:00:00.123Z");
    assert_eq!(iso_timestamp(1_792_404_000_000), "2026-10-19T10:00:00.000Z");
}

#[test]
fn paging_continues_after_a_seq() {
    let (_dir, store) = new_store();
    let batch: Vec<NewRecord> = (0..10)
        .map(|i| msg_record(i, &wire("user", &format!("m{i}"))))
        .collect();
    store.append("main", batch).unwrap();
    let a = store.read("main", 0, 4).unwrap();
    assert_eq!(a.records.len(), 4);
    assert_eq!(a.next_after, 4);
    let b = store.read("main", a.next_after, 100).unwrap();
    assert_eq!(b.records.first().unwrap().seq, 5);
    assert_eq!(b.records.len(), 6);
    assert_eq!(b.integrity, Integrity::Ok);
}

fn edit_line(path: &Path, line_index: usize, f: impl Fn(&mut Map<String, Value>)) {
    let raw = fs::read_to_string(path).unwrap();
    let mut lines: Vec<String> = raw.lines().map(str::to_string).collect();
    let mut v: Value = serde_json::from_str(&lines[line_index]).unwrap();
    f(v.as_object_mut().unwrap());
    lines[line_index] = canonical_json(&v);
    fs::write(path, lines.join("\n") + "\n").unwrap();
}

#[test]
fn an_edited_line_breaks_the_chain_at_its_seq() {
    for (what, expect) in [("body", 3u64), ("header", 4), ("swap", 3)] {
        let (dir, store) = new_store();
        let batch: Vec<NewRecord> = (0..5)
            .map(|i| msg_record(i, &wire("user", &format!("m{i}"))))
            .collect();
        store.append("main", batch).unwrap();
        let path = log_path(dir.path(), "main", 1);
        match what {
            "body" => {
                edit_line(&path, 2, |m| {
                    m.insert("body".into(), json!({"msg": {"role": "user", "parts": [{"type": "text", "text": "EDIT"}]}}));
                })
            }
            "header" => edit_line(&path, 2, |m| {
                m.insert("run".into(), json!("urn:vam:run:other"));
            }),
            _ => {
                let raw = fs::read_to_string(&path).unwrap();
                let mut lines: Vec<&str> = raw.lines().collect();
                lines.swap(2, 3);
                fs::write(&path, lines.join("\n") + "\n").unwrap();
            }
        }
        let fresh = TreeThreadStore::new(dir.path());
        let page = fresh.read("main", 0, usize::MAX).unwrap();
        match page.integrity {
            Integrity::BrokenAt { seq, .. } => assert_eq!(seq, expect, "{what}"),
            Integrity::Ok => panic!("{what}: chain verified"),
        }
        assert_eq!(page.records.len() as u64, expect - 1, "{what}");
        assert_eq!(
            verify_thread(&fresh, "main").unwrap().as_api_string(),
            format!("broken_at:{expect}")
        );
    }
}

#[test]
fn a_torn_last_line_is_skipped_then_repaired() {
    let (dir, store) = new_store();
    store
        .append(
            "main",
            vec![
                msg_record(1, &wire("user", "a")),
                msg_record(1, &wire("user", "b")),
            ],
        )
        .unwrap();
    let path = log_path(dir.path(), "main", 1);
    let before = fs::read(&path).unwrap();
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"v\":1,\"seq\":3,\"id\":\"torn").unwrap();
    drop(file);
    // A reader skips the torn tail.
    let reader = TreeThreadStore::new(dir.path());
    let page = reader.read("main", 0, usize::MAX).unwrap();
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.integrity, Integrity::Ok);
    assert_eq!(reader.head("main").unwrap().unwrap().seq, 2);
    // The writer truncates it before the next append.
    let writer = TreeThreadStore::new(dir.path());
    let receipt = writer
        .append("main", vec![msg_record(2, &wire("user", "c"))])
        .unwrap();
    assert_eq!(receipt.head.seq, 3);
    let after = fs::read(&path).unwrap();
    assert!(after.starts_with(&before));
    assert!(!String::from_utf8_lossy(&after).contains("torn"));
    assert_eq!(verify_thread(&writer, "main").unwrap(), Integrity::Ok);
}

#[test]
fn segments_rotate_and_an_oversize_record_gets_its_own() {
    let (dir, store) = new_store();
    let big = "x".repeat(1_500_000);
    for i in 0..5 {
        store
            .append("main", vec![msg_record(i, &wire("user", &big))])
            .unwrap();
    }
    let huge = "y".repeat(5 * 1024 * 1024);
    store
        .append("main", vec![msg_record(6, &wire("user", &huge))])
        .unwrap();
    store
        .append("main", vec![msg_record(7, &wire("user", "after"))])
        .unwrap();
    let segments = store.segments("main").unwrap();
    // 1.5 MiB records: 2 per segment (3 MB), the third would pass 4 MiB.
    assert_eq!(
        segments.iter().map(|s| s.first_seq).collect::<Vec<_>>(),
        vec![1, 3, 5, 6, 7]
    );
    assert!(segments.iter().all(|s| s.len > 0));
    for s in &segments[..2] {
        assert!(s.len <= SEGMENT_MAX_BYTES);
    }
    // The huge record sits alone; the next record opens a new segment.
    assert!(segments[3].len > SEGMENT_MAX_BYTES);
    let page = store.read("main", 0, usize::MAX).unwrap();
    assert_eq!(page.integrity, Integrity::Ok);
    assert_eq!(page.records.len(), 7);
    // Sealed segments are never rewritten.
    let sealed = fs::read(log_path(dir.path(), "main", 1)).unwrap();
    store
        .append("main", vec![msg_record(8, &wire("user", "more"))])
        .unwrap();
    assert_eq!(fs::read(log_path(dir.path(), "main", 1)).unwrap(), sealed);
    // Reading from the middle starts at the right segment and still verifies.
    let tail = store.read("main", 5, usize::MAX).unwrap();
    assert_eq!(tail.records.first().unwrap().seq, 6);
    assert_eq!(tail.integrity, Integrity::Ok);
}

#[test]
fn a_record_over_the_cap_becomes_a_gap() {
    let (_dir, store) = new_store();
    let enormous = "z".repeat(MAX_RECORD_BYTES + 10);
    store
        .append(
            "main",
            vec![
                msg_record(1, &wire("user", &enormous)),
                msg_record(1, &wire("user", "next")),
            ],
        )
        .unwrap();
    let page = store.read("main", 0, 10).unwrap();
    assert_eq!(page.integrity, Integrity::Ok);
    assert_eq!(page.records[0].kind, kind::GAP);
    let body = page.records[0].body.as_ref().unwrap();
    assert_eq!(body["reason"], "record_too_large");
    assert_eq!(body["original_kind"], kind::MESSAGE);
    assert_eq!(page.records[1].message().unwrap().to_wire().content, "next");
}

#[test]
fn an_overlong_line_on_disk_is_a_break_not_an_allocation() {
    let (dir, store) = new_store();
    store
        .append("main", vec![msg_record(1, &wire("user", "a"))])
        .unwrap();
    let path = log_path(dir.path(), "main", 1);
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(&vec![b'q'; MAX_RECORD_BYTES + 5]).unwrap();
    file.write_all(b"\n").unwrap();
    drop(file);
    let page = TreeThreadStore::new(dir.path())
        .read("main", 0, 10)
        .unwrap();
    assert_eq!(page.records.len(), 1);
    assert_eq!(
        page.integrity,
        Integrity::BrokenAt {
            seq: 2,
            reason: "record_too_large".into()
        }
    );
}

fn msgs(page: &ReadPage) -> Vec<String> {
    assemble_context(&page.records, MAIN_LANE)
        .unwrap()
        .messages
        .iter()
        .map(|m| m.to_wire().content)
        .collect()
}

#[test]
fn assembly_applies_compaction_rollback_and_stop_results() {
    let (_dir, store) = new_store();
    let mut batch = vec![rec(
        kind::TASK_START,
        json!({"prompt": "go", "system_sha256": "s1", "tools_sha256": "t1", "resumed": false}),
    )];
    for t in ["u1", "a1", "u2", "a2"] {
        let role = if t.starts_with('u') {
            "user"
        } else {
            "assistant"
        };
        batch.push(msg_record(1, &wire(role, t)));
    }
    store.append("main", batch).unwrap();
    // seq: 1 task_start, 2 u1, 3 a1, 4 u2, 5 a2
    let page = store.read("main", 0, 100).unwrap();
    assert_eq!(msgs(&page), ["u1", "a1", "u2", "a2"]);
    let ctx = assemble_context(&page.records, MAIN_LANE).unwrap();
    assert_eq!(ctx.system_sha256.as_deref(), Some("s1"));
    assert_eq!(ctx.head.as_ref().unwrap().seq, 5);

    // Stop: host results for unanswered calls are ordinary messages.
    let call = WireMessage {
        role: "assistant".into(),
        content: String::new(),
        tool_calls: vec![("c9".into(), "write_file".into(), "{}".into())],
        tool_call_id: String::new(),
    };
    let result = WireMessage {
        role: "tool".into(),
        content: "Not run: the task was stopped.".into(),
        tool_calls: vec![],
        tool_call_id: "c9".into(),
    };
    let mut stop_result = msg_record(2, &result);
    stop_result.src = "host".into();
    store
        .append("main", vec![msg_record(2, &call), stop_result])
        .unwrap(); // 6, 7
    let page = store.read("main", 0, 100).unwrap();
    assert_eq!(
        msgs(&page).last().unwrap(),
        "Not run: the task was stopped."
    );

    // Compaction at seq 8 keeps from seq 4.
    let summary = LogMessage::from_wire(&wire("user", "SUMMARY"));
    store
        .append(
            "main",
            vec![rec(
                kind::COMPACTION,
                json!({"summary": summary, "keep_from": 4, "reason": "tokens"}),
            )],
        )
        .unwrap();
    let page = store.read("main", 0, 100).unwrap();
    assert_eq!(
        msgs(&page),
        ["SUMMARY", "u2", "a2", "", "Not run: the task was stopped."]
    );

    // A rollback to 5 excludes 6, 7 and the compaction at 8 (records strictly between).
    store
        .append(
            "main",
            vec![rec(kind::ROLLBACK, json!({"to": 5, "reason": "stopped"}))],
        )
        .unwrap(); // 9
    store
        .append("main", vec![msg_record(3, &wire("user", "u3"))])
        .unwrap(); // 10
    let page = store.read("main", 0, 100).unwrap();
    assert_eq!(msgs(&page), ["u1", "a1", "u2", "a2", "u3"]);

    // A gap is counted, not a message.
    store
        .append(
            "main",
            vec![rec(
                kind::GAP,
                json!({"reason": "recorder_error", "missed": 1}),
            )],
        )
        .unwrap();
    let page = store.read("main", 0, 100).unwrap();
    let ctx = assemble_context(&page.records, MAIN_LANE).unwrap();
    assert_eq!(ctx.gaps, 1);
    assert_eq!(ctx.messages.len(), 5);
    // Another lane sees none of it.
    assert!(assemble_context(&page.records, "sub")
        .unwrap()
        .messages
        .is_empty());
}

#[test]
fn context_over_the_guard_is_refused() {
    let (_dir, store) = new_store();
    let chunk = "c".repeat(3 * 1024 * 1024);
    for _ in 0..6 {
        store
            .append("main", vec![msg_record(1, &wire("user", &chunk))])
            .unwrap();
    }
    let page = store.read("main", 0, usize::MAX).unwrap();
    assert!(matches!(
        assemble_context(&page.records, MAIN_LANE),
        Err(ThreadLogError::ContextTooLarge { .. })
    ));
}

#[test]
fn unknown_kinds_and_fields_are_ignored_and_v2_is_refused() {
    let (dir, store) = new_store();
    store
        .append(
            "main",
            vec![
                msg_record(1, &wire("user", "a")),
                rec("future_kind", json!({"x": 1})),
                msg_record(1, &wire("assistant", "b")),
            ],
        )
        .unwrap();
    // An unknown additive header field and an unknown body field.
    let path = log_path(dir.path(), "main", 1);
    // Rewriting a field would break the chain; verify that new fields are covered by the digest.
    let reader = TreeThreadStore::new(dir.path());
    let page = reader.read("main", 0, 10).unwrap();
    assert_eq!(msgs(&page), ["a", "b"]);
    // A v:2 line is refused with a clear error.
    let mut file = OpenOptions::new().append(true).open(&path).unwrap();
    file.write_all(b"{\"v\":2,\"seq\":4}\n").unwrap();
    drop(file);
    let err = TreeThreadStore::new(dir.path())
        .read("main", 0, 10)
        .unwrap_err();
    assert!(
        matches!(err, ThreadLogError::UnsupportedVersion { version: 2, .. }),
        "{err}"
    );
    assert!(err.to_string().contains("unsupported version 2"));
    // Unknown part types do not fail parsing.
    let m: LogMessage = serde_json::from_value(json!({"role": "assistant", "parts": [{"type": "hologram", "x": 1}, {"type": "text", "text": "t"}]})).unwrap();
    assert_eq!(m.to_wire().content, "t");
}

#[test]
fn extra_header_fields_are_covered_by_the_digest() {
    let (dir, store) = new_store();
    store
        .append(
            "main",
            vec![
                msg_record(1, &wire("user", "a")),
                msg_record(1, &wire("user", "b")),
            ],
        )
        .unwrap();
    let path = log_path(dir.path(), "main", 1);
    // Adding a header field to record 1 changes its digest, so record 2's prev no longer matches.
    edit_line(&path, 0, |m| {
        m.insert("future_field".into(), json!("x"));
    });
    let page = TreeThreadStore::new(dir.path())
        .read("main", 0, 10)
        .unwrap();
    assert_eq!(page.integrity.as_api_string(), "broken_at:2");
}

#[test]
fn a_redacted_body_keeps_the_chain_valid_only_with_a_redact_record() {
    let (dir, store) = new_store();
    store
        .append(
            "main",
            vec![
                msg_record(1, &wire("user", "secret")),
                msg_record(1, &wire("user", "keep")),
            ],
        )
        .unwrap();
    let first_id = store.read("main", 0, 1).unwrap().records[0].id.clone();
    let path = log_path(dir.path(), "main", 1);
    edit_line(&path, 0, |m| {
        m.remove("body");
    });
    // Body removed with no redact record: an integrity break.
    let page = TreeThreadStore::new(dir.path())
        .read("main", 0, 10)
        .unwrap();
    assert_eq!(page.integrity.as_api_string(), "broken_at:1");
    // With a redact record naming it, the chain verifies.
    let writer = TreeThreadStore::new(dir.path());
    writer
        .append(
            "main",
            vec![rec(
                kind::REDACT,
                json!({"target": first_id, "reason": "user request"}),
            )],
        )
        .unwrap();
    let page = writer.read("main", 0, 10).unwrap();
    assert_eq!(page.integrity, Integrity::Ok);
    assert!(page.records[0].body.is_none());
    assert_eq!(msgs(&page), ["keep"]);
}

#[test]
fn fork_keeps_digests_and_the_chain() {
    let (dir, store) = new_store();
    crate::vam_dir::ensure_thread_json(dir.path(), "main", "urn:vam:agent:a", "d1").unwrap();
    let big = "x".repeat(1_500_000);
    store
        .append(
            "main",
            vec![
                msg_record(1, &wire("user", &big)),
                msg_record(1, &wire("user", &big)),
            ],
        )
        .unwrap();
    store
        .append(
            "main",
            (0..3)
                .map(|i| msg_record(2, &wire("user", &format!("m{i}"))))
                .collect(),
        )
        .unwrap();
    store
        .append(
            "main",
            vec![
                msg_record(3, &wire("user", &big)),
                msg_record(3, &wire("user", &big)),
            ],
        )
        .unwrap();
    assert!(store.segments("main").unwrap().len() >= 2);
    let parent_page = store.read("main", 0, usize::MAX).unwrap();
    let at = &parent_page.records[3];
    let head = store.fork("main", at.seq, "side").unwrap();
    assert_eq!(head.seq, at.seq + 1);
    let child = store.read("side", 0, usize::MAX).unwrap();
    assert_eq!(child.integrity, Integrity::Ok);
    assert_eq!(child.records.len() as u64, at.seq + 1);
    for (p, c) in parent_page.records.iter().zip(&child.records) {
        if p.seq <= at.seq {
            assert_eq!(p.digest, c.digest);
        }
    }
    let fork = child.records.last().unwrap();
    assert_eq!(fork.kind, kind::FORK);
    assert_eq!(
        fork.body.as_ref().unwrap()["parent_digest"],
        at.digest.as_str()
    );
    // The child continues independently; the parent is untouched.
    store
        .append("side", vec![msg_record(4, &wire("user", "child only"))])
        .unwrap();
    assert_eq!(
        store.read("main", 0, usize::MAX).unwrap().records.len(),
        parent_page.records.len()
    );
    let meta = store.thread_meta("side").unwrap().unwrap();
    assert_eq!(meta.parent_thread.as_deref(), Some("main"));
    assert_eq!(meta.thread_id, "side");
    assert!(meta.log.is_some());
    // Bad forks.
    assert!(store.fork("main", 999, "nope").is_err());
    assert!(store.fork("main", 1, "main").is_err());
    assert!(store.fork("main", 1, "side").is_err());
}

#[test]
fn thread_ids_and_paths_cannot_escape() {
    let (dir, store) = new_store();
    for bad in [".", "..", "...", "a/b", "../x", "a\\b", "x y", "a\0b"] {
        assert!(
            matches!(
                store.append(bad, vec![rec(kind::GAP, json!({}))]),
                Err(ThreadLogError::InvalidThread(_))
            ),
            "{bad:?}"
        );
        assert!(store.read(bad, 0, 1).is_err(), "{bad:?}");
    }
    // The empty id is the default thread, never the parent directory.
    store.append("", vec![rec(kind::GAP, json!({}))]).unwrap();
    assert!(dir
        .path()
        .join(".vam/threads/main/log/00000001.jsonl")
        .exists());
    assert!(!dir.path().join(".vam/threads/log").exists());
}

#[cfg(unix)]
#[test]
fn a_symlinked_directory_or_segment_is_refused() {
    let (dir, store) = new_store();
    let outside = tempdir().unwrap();
    // A planted `log` symlink.
    fs::create_dir_all(dir.path().join(".vam/threads/main")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".vam/threads/main/log")).unwrap();
    assert!(matches!(
        store.append("main", vec![rec(kind::GAP, json!({}))]),
        Err(ThreadLogError::UnsafePath(_))
    ));
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    // A planted thread directory symlink.
    std::os::unix::fs::symlink(outside.path(), dir.path().join(".vam/threads/evil")).unwrap();
    assert!(matches!(
        store.append("evil", vec![rec(kind::GAP, json!({}))]),
        Err(ThreadLogError::UnsafePath(_))
    ));
    assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    // A planted segment symlink is not listed and not followed.
    let (dir2, store2) = new_store();
    let target = outside.path().join("secret.jsonl");
    fs::write(&target, b"{}\n").unwrap();
    fs::create_dir_all(dir2.path().join(".vam/threads/main/log")).unwrap();
    std::os::unix::fs::symlink(&target, log_path(dir2.path(), "main", 1)).unwrap();
    assert!(store2.segments("main").unwrap().is_empty());
}

#[test]
fn old_thread_json_without_log_still_reads() {
    let raw = r#"{"thread_id":"main","agent_urn":"u","agent_digest":"d","created_at":5}"#;
    let meta: ThreadMeta = serde_json::from_str(raw).unwrap();
    assert!(meta.log.is_none());
    let dir = tempdir().unwrap();
    let fresh = crate::vam_dir::ensure_thread_json(dir.path(), "t", "u", "d").unwrap();
    assert_eq!(fresh.log, Some(crate::vam_dir::ThreadLogInfo::v1()));
    assert_eq!(fresh.log.unwrap().format, LOG_FORMAT);
}

#[test]
fn list_reports_threads_heads_and_bytes() {
    let (dir, store) = new_store();
    crate::vam_dir::ensure_thread_json(dir.path(), "a", "u", "d").unwrap();
    store
        .append("a", vec![msg_record(1, &wire("user", "hi"))])
        .unwrap();
    store
        .append("b", vec![msg_record(1, &wire("user", "hi"))])
        .unwrap();
    let listing = store.list().unwrap();
    assert_eq!(
        listing
            .iter()
            .map(|t| t.thread_id.as_str())
            .collect::<Vec<_>>(),
        ["a", "b"]
    );
    assert!(listing[0].meta.is_some() && listing[1].meta.is_none());
    assert_eq!(listing[0].head.as_ref().unwrap().seq, 1);
    assert!(listing[0].log_bytes > 0);
}

#[test]
fn concurrent_appends_keep_one_ordered_chain() {
    let (_dir, store) = new_store();
    let store = Arc::new(store);
    let mut handles = Vec::new();
    for t in 0..8 {
        let store = store.clone();
        handles.push(std::thread::spawn(move || {
            for i in 0..10 {
                let batch = vec![
                    msg_record(i, &wire("user", &format!("t{t}-{i}-a"))),
                    msg_record(i, &wire("assistant", &format!("t{t}-{i}-b"))),
                ];
                store.append("main", batch).unwrap();
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }
    let page = store.read("main", 0, usize::MAX).unwrap();
    assert_eq!(page.integrity, Integrity::Ok);
    assert_eq!(page.records.len(), 160);
    assert!(page
        .records
        .iter()
        .enumerate()
        .all(|(i, r)| r.seq == i as u64 + 1));
    // A batch is never interleaved with another.
    for pair in page.records.chunks(2) {
        let a = pair[0].message().unwrap().to_wire().content;
        let b = pair[1].message().unwrap().to_wire().content;
        assert_eq!(a.trim_end_matches("-a"), b.trim_end_matches("-b"));
    }
}

fn manager(dir: &Path, backend: Drive9Backend) -> WorkspaceManager {
    let config = Drive9Config::new(dir);
    let layout = config.layout();
    let chunk_store = ChunkStore::new(layout.clone());
    let manifest_manager = ManifestManager::new(layout.clone(), chunk_store.clone());
    let cache_manager =
        CacheManager::new(layout.clone(), chunk_store.clone(), 10 * 1024 * 1024).unwrap();
    WorkspaceManager::new(
        layout,
        chunk_store,
        manifest_manager,
        cache_manager,
        Box::new(MockImageIo),
    )
    .with_backend(backend)
}

#[test]
fn records_read_back_from_head_after_a_commit() {
    for backend in [Drive9Backend::Directory, Drive9Backend::Ext4Image] {
        let root = tempdir().unwrap();
        let wm = manager(root.path(), backend);
        let profile = StorageProfile::small_for_test();
        let mat = wm
            .materialize("sess-log", "ws-log", &profile, None, None, None)
            .unwrap();
        let store = TreeThreadStore::new(&mat);
        crate::vam_dir::ensure_thread_json(&mat, "main", "u", "d").unwrap();
        let big = "x".repeat(1_500_000);
        let batch: Vec<NewRecord> = (0..6)
            .map(|i| {
                msg_record(
                    i,
                    &wire(
                        "user",
                        &if i % 2 == 0 {
                            big.clone()
                        } else {
                            format!("m{i}")
                        },
                    ),
                )
            })
            .collect();
        store.append("main", batch).unwrap();
        let live = store.read("main", 0, usize::MAX).unwrap();
        wm.commit("sess-log", "ws-log").unwrap();
        let mm = ManifestManager::new(wm.layout().clone(), wm.chunk_store().clone());
        let reader = HeadThreadReader::new(&mm, wm.chunk_store(), "ws-log");
        let page = reader.read("main", 0, usize::MAX).unwrap();
        assert_eq!(page.integrity, Integrity::Ok, "{backend:?}");
        assert_eq!(page.records, live.records, "{backend:?}");
        assert_eq!(reader.head("main").unwrap(), store.head("main").unwrap());
        let listing = reader.list().unwrap();
        assert_eq!(listing.len(), 1);
        assert!(listing[0].meta.is_some());
        assert!(reader.read("other", 0, 10).unwrap().records.is_empty());
        assert_eq!(
            reader.segments("main").unwrap().len(),
            store.segments("main").unwrap().len()
        );
    }
}

#[test]
fn a_legacy_timeline_is_read_and_a_missing_one_is_empty() {
    let (dir, store) = new_store();
    assert!(store.timeline("old").unwrap().is_empty());
    let thread_dir = dir.path().join(".vam/threads/old");
    fs::create_dir_all(&thread_dir).unwrap();
    let line = |seq: u64| {
        json!({
            "seq": seq, "prev_hash": "", "run_id": "r", "prompt": "p", "answer": "a",
            "model": "m", "manifest_hash": "h", "timestamp": 1
        })
        .to_string()
    };
    fs::write(
        thread_dir.join("timeline.jsonl"),
        format!("{}\n\n{}\n", line(1), line(2)),
    )
    .unwrap();
    let turns = store.timeline("old").unwrap();
    assert_eq!(turns.len(), 2);
    assert_eq!(turns[1].seq, 2);
    assert!(store.timeline("../x").is_err());
}

#[test]
fn an_empty_workspace_has_no_threads() {
    let root = tempdir().unwrap();
    let wm = manager(root.path(), Drive9Backend::Directory);
    let mm = ManifestManager::new(wm.layout().clone(), wm.chunk_store().clone());
    let reader = HeadThreadReader::new(&mm, wm.chunk_store(), "nothing");
    assert!(reader.list().unwrap().is_empty());
    assert!(reader.head("main").unwrap().is_none());
}

#[test]
fn a_reloaded_store_continues_the_same_chain() {
    let (dir, store) = new_store();
    store
        .append("main", vec![msg_record(1, &wire("user", "a"))])
        .unwrap();
    let again = TreeThreadStore::new(dir.path());
    let receipt = again
        .append("main", vec![msg_record(2, &wire("user", "b"))])
        .unwrap();
    assert_eq!(receipt.first_seq, 2);
    assert_eq!(verify_thread(&again, "main").unwrap(), Integrity::Ok);
    // Another writer appending behind this store's back is noticed.
    again
        .append("main", vec![msg_record(3, &wire("user", "c"))])
        .unwrap();
    store
        .append("main", vec![msg_record(4, &wire("user", "d"))])
        .unwrap();
    let page = again.read("main", 0, usize::MAX).unwrap();
    assert_eq!(page.integrity, Integrity::Ok);
    assert_eq!(page.records.len(), 4);
}

#[test]
fn assembly_lists_runs_of_surviving_messages_and_ignores_context_source() {
    let (_dir, store) = new_store();
    let mut a = msg_record(1, &wire("user", "from run a"));
    a.run = "run-a".into();
    let mut b = msg_record(1, &wire("user", "from run b"));
    b.run = "run-b".into();
    store
        .append(
            "main",
            vec![
                a,
                rec(
                    kind::CONTEXT_SOURCE,
                    json!({"source": "memory", "kind": "user_memory", "digest": "d", "size": 3}),
                ),
                b,
            ],
        )
        .unwrap();
    let (records, integrity) = read_all_records(&store, "main").unwrap();
    assert_eq!(integrity, Integrity::Ok);
    let ctx = assemble_context(&records, MAIN_LANE).unwrap();
    assert_eq!(ctx.messages.len(), 2);
    assert_eq!(ctx.runs, ["run-a", "run-b"]);
    // Rolling back run b's message leaves only run a.
    store
        .append(
            "main",
            vec![rec(
                kind::ROLLBACK,
                json!({"to": 1, "reason": "prefix_mismatch", "dropped": 1}),
            )],
        )
        .unwrap();
    let (records, _) = read_all_records(&store, "main").unwrap();
    let ctx = assemble_context(&records, MAIN_LANE).unwrap();
    assert_eq!(ctx.runs, ["run-a"]);
}

// ---------------------------------------------------------------------------
// Size limits

fn quota_store(
    dir: &Path,
    thread_bytes: u64,
    workspace_bytes: u64,
    reserve_bytes: u64,
) -> (TreeThreadStore, crate::log_quota::LogUsage) {
    let quota = crate::log_quota::LogQuota {
        thread_bytes,
        workspace_bytes,
        reserve_bytes,
    };
    let probe = TreeThreadStore::new(dir);
    let usage = crate::log_quota::LogUsage::new(move || probe.log_bytes());
    (
        TreeThreadStore::new(dir).with_quota(quota, usage.clone()),
        usage,
    )
}

fn big_message(round: u64, bytes: usize) -> NewRecord {
    msg_record(round, &wire("user", &"x".repeat(bytes)))
}

fn on_disk(dir: &Path, thread: &str) -> u64 {
    let log = dir.join(".vam/threads").join(thread).join("log");
    std::fs::read_dir(log)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.metadata().map(|m| m.len()).unwrap_or(0))
                .sum()
        })
        .unwrap_or(0)
}

#[test]
fn a_thread_log_stops_taking_conversation_at_its_limit_and_loses_nothing() {
    let dir = tempdir().unwrap();
    let (store, _) = quota_store(dir.path(), 4096, 1 << 30, 2048);
    store.append("t", vec![big_message(1, 1500)]).unwrap();
    store.append("t", vec![big_message(2, 1500)]).unwrap();
    let before = on_disk(dir.path(), "t");
    let err = store.append("t", vec![big_message(3, 1500)]).unwrap_err();
    match err {
        ThreadLogError::QuotaExceeded { scope, limit, used } => {
            assert_eq!(scope, crate::log_quota::QuotaScope::Thread);
            assert_eq!(limit, 4096);
            assert_eq!(used, before);
        }
        other => panic!("expected a quota error, got {other:?}"),
    }
    assert_eq!(on_disk(dir.path(), "t"), before, "nothing was written");
    // The chain is still good and the next seq is unchanged.
    let page = store.read("t", 0, usize::MAX).unwrap();
    assert_eq!(page.records.len(), 2);
    assert_eq!(page.integrity, Integrity::Ok);
}

#[test]
fn control_records_use_the_reserve_and_conversation_never_does() {
    let dir = tempdir().unwrap();
    let (store, _) = quota_store(dir.path(), 3000, 1 << 30, 3000);
    store.append("t", vec![big_message(1, 2000)]).unwrap();
    // Over the limit for conversation...
    assert!(matches!(
        store.append("t", vec![big_message(2, 2000)]),
        Err(ThreadLogError::QuotaExceeded { .. })
    ));
    // ...but a gap and a task_end still fit in the reserve.
    store
        .append(
            "t",
            vec![rec(kind::GAP, json!({"reason": "quota", "missed": 1}))],
        )
        .unwrap();
    store
        .append("t", vec![rec(kind::TASK_END, json!({"outcome": "failed"}))])
        .unwrap();
    // A host-written message (a stop's result) is control too.
    let mut host = big_message(3, 100);
    host.src = "host".into();
    store.append("t", vec![host]).unwrap();
    // The reserve itself is bounded.
    let big_control = rec(kind::GAP, json!({"detail": "y".repeat(4000)}));
    assert!(matches!(
        store.append("t", vec![big_control]),
        Err(ThreadLogError::QuotaExceeded { .. })
    ));
    // A mixed batch is held to the limit for its content.
    assert!(matches!(
        store.append("t", vec![rec(kind::GAP, json!({})), big_message(4, 10),]),
        Err(ThreadLogError::QuotaExceeded { .. })
    ));
}

#[test]
fn the_workspace_limit_counts_every_thread_and_the_shared_usage() {
    let dir = tempdir().unwrap();
    let (store, usage) = quota_store(dir.path(), 1 << 30, 7000, 1000);
    store.append("a", vec![big_message(1, 1800)]).unwrap();
    store.append("b", vec![big_message(1, 1800)]).unwrap();
    // Something else (the prompt store) also uses the workspace.
    usage.try_add(1000, u64::MAX).unwrap();
    let err = store.append("c", vec![big_message(1, 1800)]).unwrap_err();
    match err {
        ThreadLogError::QuotaExceeded { scope, limit, .. } => {
            assert_eq!(scope, crate::log_quota::QuotaScope::Workspace);
            assert_eq!(limit, 7000);
        }
        other => panic!("expected a quota error, got {other:?}"),
    }
    assert_eq!(on_disk(dir.path(), "c"), 0);
    usage.sub(1000);
    store.append("c", vec![big_message(1, 1800)]).unwrap();
}

#[test]
fn usage_is_counted_from_disk_when_a_store_is_reopened() {
    let dir = tempdir().unwrap();
    {
        let (store, _) = quota_store(dir.path(), 1 << 30, 5000, 0);
        store.append("a", vec![big_message(1, 2500)]).unwrap();
    }
    let (store, usage) = quota_store(dir.path(), 1 << 30, 5000, 0);
    assert!(usage.used() >= 2500);
    assert!(matches!(
        store.append("b", vec![big_message(1, 2800)]),
        Err(ThreadLogError::QuotaExceeded { .. })
    ));
}

#[test]
fn a_store_without_a_quota_is_unbounded() {
    let (dir, store) = new_store();
    store.append("t", vec![big_message(1, 300_000)]).unwrap();
    assert!(on_disk(dir.path(), "t") > 300_000);
}

#[test]
fn a_second_compaction_replaces_the_first_summary_and_reports_its_seq() {
    let (_dir, store) = new_store();
    let mut batch = vec![rec(
        kind::TASK_START,
        json!({"prompt": "go", "system_sha256": "s1", "tools_sha256": "t1", "resumed": false}),
    )];
    for t in ["u1", "a1", "u2", "a2"] {
        let role = if t.starts_with('u') {
            "user"
        } else {
            "assistant"
        };
        batch.push(msg_record(1, &wire(role, t)));
    }
    store.append("main", batch).unwrap(); // 1 task_start, 2 u1, 3 a1, 4 u2, 5 a2
    let s1 = LogMessage::from_wire(&wire("user", "SUM1"));
    store
        .append(
            "main",
            vec![rec(
                kind::COMPACTION,
                json!({"summary": s1, "keep_from": 4, "reason": "tokens"}),
            )],
        )
        .unwrap(); // 6
    store
        .append("main", vec![msg_record(2, &wire("user", "u3"))])
        .unwrap(); // 7
    let page = store.read("main", 0, 100).unwrap();
    let ctx = assemble_context(&page.records, MAIN_LANE).unwrap();
    assert_eq!(ctx.compaction_seq, Some(6));
    // The second compaction keeps from a2 (seq 5), below the first compaction.
    let s2 = LogMessage::from_wire(&wire("user", "SUM2"));
    store
        .append(
            "main",
            vec![rec(
                kind::COMPACTION,
                json!({"summary": s2, "keep_from": 5, "reason": "tokens"}),
            )],
        )
        .unwrap(); // 8
    let page = store.read("main", 0, 100).unwrap();
    assert_eq!(msgs(&page), ["SUM2", "a2", "u3"]);
    let ctx = assemble_context(&page.records, MAIN_LANE).unwrap();
    assert_eq!(ctx.compaction_seq, Some(8));
    assert_eq!(ctx.seqs, vec![8, 5, 7]);
}

#[test]
fn a_rebase_compaction_hides_older_messages_and_keeps_later_ones() {
    let (_dir, store) = new_store();
    store
        .append(
            "main",
            vec![
                msg_record(1, &wire("user", "old")),
                msg_record(1, &wire("assistant", "older")),
            ],
        )
        .unwrap(); // 1, 2
                   // keep_from equal to its own seq keeps nothing.
    let s = LogMessage::from_wire(&wire("user", "SUM"));
    store
        .append(
            "main",
            vec![
                rec(
                    kind::COMPACTION,
                    json!({"summary": s, "keep_from": 3, "reason": "rebase"}),
                ),
                msg_record(2, &wire("user", "new")),
            ],
        )
        .unwrap(); // 3, 4
    let page = store.read("main", 0, 100).unwrap();
    assert_eq!(msgs(&page), ["SUM", "new"]);
}
