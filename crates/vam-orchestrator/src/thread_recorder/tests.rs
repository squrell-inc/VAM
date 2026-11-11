#![allow(unused_must_use)]
use super::*;
use tempfile::{tempdir, TempDir};
use vam_spec::v1::{ChatMessage, ToolCall, ToolSpec};
use vam_storage::thread_log::{verify_thread, Record, ThreadLogReader};

const THREAD: &str = "main";

struct Fixture {
    dir: TempDir,
    recorder: Arc<ThreadRecorder>,
}

impl Fixture {
    fn root(&self) -> std::path::PathBuf {
        self.dir.path().join("materialized")
    }

    fn tree(&self) -> TreeThreadStore {
        TreeThreadStore::new(self.root())
    }

    fn records(&self) -> Vec<Record> {
        self.tree().read(THREAD, 0, usize::MAX).unwrap().records
    }

    fn assembled(&self) -> Vec<WireMessage> {
        assemble_context(&self.records(), MAIN_LANE)
            .unwrap()
            .messages
            .iter()
            .map(LogMessage::to_wire)
            .collect()
    }

    fn scope_for(&self, thread: &str) -> RecorderScope {
        let layout = Drive9Layout::new(self.dir.path().join("drive9"));
        RecorderScope {
            workspace_id: "ws-1".into(),
            thread_ref: thread.into(),
            run_id: "urn:vam:run:test".into(),
            agent_urn: "urn:vam:agent:test".into(),
            prompts: PromptStore::new(&layout, "ws-1").unwrap(),
        }
    }

    fn recorder_for(&self, thread: &str) -> Arc<ThreadRecorder> {
        let scope = self.scope_for(thread);
        Arc::new(ThreadRecorder::with_resolver(
            "sess-test".into(),
            self.root(),
            Arc::new(move || Ok(scope.clone())),
        ))
    }
}

fn fixture() -> Fixture {
    let dir = tempdir().unwrap();
    // A session's materialized tree exists before anything is recorded.
    std::fs::create_dir_all(dir.path().join("materialized")).unwrap();
    let f = Fixture {
        dir,
        recorder: Arc::new(ThreadRecorder::with_resolver(
            "unused".into(),
            std::path::PathBuf::new(),
            Arc::new(|| Err("unset".into())),
        )),
    };
    let recorder = f.recorder_for(THREAD);
    Fixture { recorder, ..f }
}

fn msg(role: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: role.into(),
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: String::new(),
    }
}

fn call(id: &str, name: &str, args: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: name.into(),
        arguments: args.into(),
    }
}

fn assistant_calls(content: &str, calls: Vec<ToolCall>) -> ChatMessage {
    ChatMessage {
        role: "assistant".into(),
        content: content.into(),
        tool_calls: calls,
        tool_call_id: String::new(),
    }
}

fn tool_msg(id: &str, content: &str) -> ChatMessage {
    ChatMessage {
        role: "tool".into(),
        content: content.into(),
        tool_calls: vec![],
        tool_call_id: id.into(),
    }
}

fn tool_spec(name: &str, description: &str) -> ToolSpec {
    ToolSpec {
        name: name.into(),
        description: description.into(),
        ..Default::default()
    }
}

fn request(id: u64, system: &str, rest: Vec<ChatMessage>, tools: Vec<ToolSpec>) -> ExchangeRequest {
    let mut messages = vec![msg("system", system)];
    messages.extend(rest);
    ExchangeRequest {
        exchange_id: id,
        messages,
        tools,
        model: vam_inference::ModelCatalogEntry {
            provider: "mock".into(),
            model_name: "mock-model".into(),
            endpoint: String::new(),
            secret_ref: String::new(),
            price_prompt_micro: 0,
            price_completion_micro: 0,
            context_window: 0,
            supports_tools: true,
            supports_structured_output: false,
            ..Default::default()
        },
        context_hint: None,
    }
}

fn response(id: u64, text: &str, calls: Vec<ToolCall>) -> ExchangeResponse {
    ExchangeResponse {
        exchange_id: id,
        text: text.into(),
        tool_calls: calls,
        usage: Some(vam_spec::v1::TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 2,
            cached_tokens: 1,
            total_tokens: 12,
            cache_creation_tokens: 0,
            cached_tokens_unknown: false,
            cache_creation_long_tokens: 0,
        }),
        reasoning: None,
        reasoning_bytes: 0,
    }
}

fn wire_of(m: &ChatMessage) -> WireMessage {
    to_wire(m)
}

/// Runs a scripted 3-round task and returns the last request's non-system messages.
async fn three_rounds(rec: &ThreadRecorder) -> Vec<ChatMessage> {
    let tools = vec![tool_spec("read_file", "Read a file")];
    let u = msg("user", "Save the note \u{1F600}\nplease");
    let c1 = call(
        "c1",
        "write_file",
        "{\"path\":\"note.txt\",\"content\":\"kiwi-42\"}",
    );
    let a1 = assistant_calls("", vec![c1.clone()]);
    let t1 = tool_msg("c1", "ok");
    let c2 = call("c2", "read_file", "{\"path\":\"note.txt\"}");
    let a2 = assistant_calls("reading", vec![c2.clone()]);
    let t2 = tool_msg("c2", "kiwi-42");

    rec.on_request(request(1, "SYS", vec![u.clone()], tools.clone()))
        .await;
    rec.on_response(response(1, "", vec![c1])).await;
    rec.on_request(request(
        2,
        "SYS",
        vec![u.clone(), a1.clone(), t1.clone()],
        tools.clone(),
    ))
    .await;
    rec.on_response(response(2, "reading", vec![c2])).await;
    let last = vec![u, a1, t1, a2, t2];
    rec.on_request(request(3, "SYS", last.clone(), tools)).await;
    rec.on_response(response(3, "Saved.", vec![])).await;
    last
}

#[tokio::test]
async fn a_three_round_task_assembles_to_the_last_request_plus_the_answer() {
    let f = fixture();
    f.recorder
        .record_task_start("Save the note", "mock-model", "d1", false)
        .await;
    let last = three_rounds(&f.recorder).await;
    let mut want: Vec<WireMessage> = last.iter().map(wire_of).collect();
    want.push(wire_of(&assistant_calls("Saved.", vec![])));
    assert_eq!(f.assembled(), want);
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
    let records = f.records();
    assert_eq!(records[0].kind, kind::TASK_START);
    assert_eq!(records[0].body.as_ref().unwrap()["prompt"], "Save the note");
    // The first request carried the prompt digests (none were known at task start).
    assert_eq!(records[1].kind, kind::PROMPT_CHANGE);
    // A response keeps its usage and the round of its request.
    let answer = records.last().unwrap();
    assert_eq!(answer.round, 3);
    assert_eq!(answer.body.as_ref().unwrap()["usage"]["prompt"], 10);
    assert_eq!(f.recorder.stats(), RecorderStats::default());
    // Host-derived headers.
    assert!(records
        .iter()
        .all(|r| r.run == "urn:vam:run:test" && r.agent == "urn:vam:agent:test"));
}

fn all_bytes_under(dir: &std::path::Path) -> Vec<u8> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            out.extend(all_bytes_under(&path));
        } else {
            out.extend(std::fs::read(path).unwrap());
        }
    }
    out
}

#[tokio::test]
async fn the_system_prompt_and_tools_are_in_the_prompt_store_not_in_the_workspace() {
    let f = fixture();
    let secret_system = "SYSTEM-PROMPT-SECRET-7f3a";
    let secret_tool = "TOOL-DESCRIPTION-SECRET-91bc";
    let rec = &f.recorder;
    rec.on_request(request(
        1,
        secret_system,
        vec![msg("user", "hi")],
        vec![tool_spec("a", secret_tool)],
    ))
    .await;
    rec.on_response(response(1, "yo", vec![])).await;
    let tree_bytes = String::from_utf8_lossy(&all_bytes_under(&f.root())).into_owned();
    assert!(!tree_bytes.contains(secret_system));
    assert!(!tree_bytes.contains(secret_tool));
    let ctx = assemble_context(&f.records(), MAIN_LANE).unwrap();
    let sys_sha = ctx.system_sha256.expect("system digest");
    assert_eq!(sys_sha, sha256_hex(secret_system.as_bytes()));
    let scope = f.scope_for(THREAD);
    assert_eq!(
        scope
            .prompts
            .get(PromptKind::System, &sys_sha)
            .unwrap()
            .unwrap(),
        secret_system.as_bytes()
    );
    let tools_sha = ctx.tools_sha256.expect("tools digest");
    let blob = scope
        .prompts
        .get(PromptKind::Tools, &tools_sha)
        .unwrap()
        .unwrap();
    assert!(String::from_utf8(blob).unwrap().contains(secret_tool));
    // The store is outside the session tree.
    assert!(!f.root().join("prompts").exists());
}

#[tokio::test]
async fn a_rewritten_tool_output_is_a_rollback_and_a_reappend_never_a_loss() {
    let f = fixture();
    let rec = &f.recorder;
    let last = three_rounds(rec).await;
    // Round 4: an old tool output was rewritten by the guest.
    let mut rewritten = last.clone();
    rewritten.push(assistant_calls("Saved.", vec![]));
    rewritten[2] = tool_msg("c1", "[Output truncated]");
    rewritten.push(msg("user", "next"));
    rec.on_request(request(
        4,
        "SYS",
        rewritten.clone(),
        vec![tool_spec("read_file", "Read a file")],
    ))
    .await;
    assert_eq!(rec.stats().rollbacks, 1);
    let want: Vec<WireMessage> = rewritten.iter().map(wire_of).collect();
    assert_eq!(f.assembled(), want);
    let records = f.records();
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
    let rb = records
        .iter()
        .find(|r| r.kind == kind::ROLLBACK)
        .expect("rollback");
    let body = rb.body.as_ref().unwrap();
    assert_eq!(body["reason"], "prefix_mismatch");
    // The first-seen output is still in the log.
    assert!(records.iter().any(|r| r
        .message()
        .is_some_and(|m| m.to_wire().content == "ok" && m.role == "tool")));
    // The rollback sits before the re-appended tail and points at the last match.
    let to = body["to"].as_u64().unwrap();
    assert!(to < rb.seq);
    assert!(records
        .iter()
        .filter(|r| r.seq > rb.seq)
        .all(|r| r.kind == kind::MESSAGE));
}

#[tokio::test]
async fn text_parsed_tool_calls_are_recorded_as_the_guest_sends_them() {
    let f = fixture();
    let rec = &f.recorder;
    let u = msg("user", "go");
    rec.on_request(request(1, "SYS", vec![u.clone()], vec![]))
        .await;
    // The model answered with a tool call written as text; the guest parsed it.
    rec.on_response(response(
        1,
        "{\"tool\":\"run_it\",\"arguments\":{\"x\":1}}",
        vec![],
    ))
    .await;
    let parsed = assistant_calls("", vec![call("text_call_0", "run_it", "{\"x\":1}")]);
    rec.on_request(request(
        2,
        "SYS",
        vec![u.clone(), parsed.clone(), tool_msg("text_call_0", "done")],
        vec![],
    ))
    .await;
    assert_eq!(
        rec.stats().rollbacks,
        0,
        "the recorder normalized the reply"
    );
    assert_eq!(
        f.assembled(),
        vec![
            wire_of(&u),
            wire_of(&parsed),
            wire_of(&tool_msg("text_call_0", "done"))
        ]
    );
}

#[tokio::test]
async fn the_run_range_starts_at_the_first_task_start_and_survives_a_resume() {
    let f = fixture();
    assert_eq!(f.recorder.run_first_seq(), None);
    f.recorder.record_task_start("go", "m", "d", false).await;
    assert_eq!(f.recorder.run_first_seq(), Some(1));
    three_rounds(&f.recorder).await;
    f.recorder.record_task_start("go", "m", "", true).await;
    assert_eq!(
        f.recorder.run_first_seq(),
        Some(1),
        "a resume continues the range"
    );
}

#[tokio::test]
async fn a_prompt_change_is_recorded_only_when_a_digest_changes() {
    let f = fixture();
    let rec = &f.recorder;
    let u = msg("user", "go");
    rec.on_request(request(1, "SYS-A", vec![u.clone()], vec![]))
        .await;
    rec.on_response(response(1, "a", vec![])).await;
    let a = assistant_calls("a", vec![]);
    rec.on_request(request(
        2,
        "SYS-A",
        vec![u.clone(), a.clone(), msg("user", "2")],
        vec![],
    ))
    .await;
    rec.on_response(response(2, "b", vec![])).await;
    // load_skill rewrote the system prompt.
    let b = assistant_calls("b", vec![]);
    rec.on_request(request(
        3,
        "SYS-B",
        vec![u, a, msg("user", "2"), b, msg("user", "3")],
        vec![],
    ))
    .await;
    let changes = f
        .records()
        .iter()
        .filter(|r| r.kind == kind::PROMPT_CHANGE)
        .count();
    assert_eq!(changes, 2, "the first request and the SYS-B change");
    assert_eq!(rec.stats().rollbacks, 0);
}

#[tokio::test]
async fn a_second_task_and_a_restarted_recorder_continue_the_log() {
    let f = fixture();
    f.recorder.record_task_start("first", "m", "d", false).await;
    let last = three_rounds(&f.recorder).await;
    let end = f
        .recorder
        .record_task_end("succeeded", "Saved.", "")
        .await
        .unwrap();
    assert_eq!(f.tree().head(THREAD).unwrap().unwrap(), end);
    let tail = f.records().last().unwrap().clone();
    assert_eq!(tail.kind, kind::TASK_END);
    let before = tail.body.as_ref().unwrap()["head_before"].clone();
    assert_eq!(before["seq"].as_u64().unwrap() + 1, end.seq);

    // vamd restarted: a new recorder over the same tree reloads from disk.
    let again = f.recorder_for(THREAD);
    again.record_task_start("second", "m", "d", false).await;
    let mut next = last.clone();
    next.push(assistant_calls("Saved.", vec![]));
    next.push(msg("user", "What does the note say?"));
    again
        .on_request(request(
            1,
            "SYS",
            next.clone(),
            vec![tool_spec("read_file", "Read a file")],
        ))
        .await;
    again.on_response(response(1, "kiwi-42", vec![])).await;
    assert_eq!(
        again.stats().rollbacks,
        0,
        "an extension of the reloaded context"
    );
    let mut want: Vec<WireMessage> = next.iter().map(wire_of).collect();
    want.push(wire_of(&assistant_calls("kiwi-42", vec![])));
    assert_eq!(f.assembled(), want);
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
    let kinds: Vec<String> = f.records().iter().map(|r| r.kind.clone()).collect();
    let end_pos = kinds.iter().position(|k| k == kind::TASK_END).unwrap();
    assert_eq!(kinds[end_pos + 1], kind::TASK_START);
    // The prompt did not change across tasks: no second prompt_change.
    assert_eq!(
        kinds.iter().filter(|k| *k == kind::PROMPT_CHANGE).count(),
        1
    );
}

#[tokio::test]
async fn an_unscoped_session_records_nothing() {
    let dir = tempdir().unwrap();
    let rec = ThreadRecorder::with_resolver(
        "s".into(),
        dir.path().join("materialized"),
        Arc::new(|| Err("no workspace".into())),
    );
    rec.record_task_start("p", "m", "d", false).await;
    rec.on_request(request(1, "SYS", vec![msg("user", "hi")], vec![]))
        .await;
    rec.on_response(response(1, "yo", vec![])).await;
    assert!(rec.record_task_end("succeeded", "yo", "").await.is_none());
    assert!(!dir.path().join("materialized").exists());
    assert_eq!(rec.stats().unscoped, 4);
    assert_eq!(rec.stats().failures, 0);
}

#[tokio::test]
async fn a_failing_store_never_fails_the_task_and_leaves_a_gap_once_it_recovers() {
    let f = fixture();
    let rec = &f.recorder;
    std::fs::create_dir_all(f.root()).unwrap();
    // `.vam` is a file: nothing can be written.
    std::fs::write(f.root().join(".vam"), b"in the way").unwrap();
    let u = msg("user", "go");
    rec.on_request(request(1, "SYS", vec![u.clone()], vec![]))
        .await;
    rec.on_response(response(1, "a", vec![])).await;
    assert!(rec.stats().failures >= 1);
    assert!(rec.cached_head().is_none());
    // The disk recovers; the next request re-derives what was missed.
    std::fs::remove_file(f.root().join(".vam")).unwrap();
    let a = assistant_calls("a", vec![]);
    rec.on_request(request(
        2,
        "SYS",
        vec![u.clone(), a.clone(), msg("user", "2")],
        vec![],
    ))
    .await;
    let records = f.records();
    assert_eq!(
        records[0].kind,
        kind::GAP,
        "readers see the log is incomplete"
    );
    assert!(
        records[0].body.as_ref().unwrap()["missed"]
            .as_u64()
            .unwrap()
            >= 1
    );
    assert_eq!(
        f.assembled(),
        vec![wire_of(&u), wire_of(&a), wire_of(&msg("user", "2"))]
    );
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
}

#[tokio::test]
async fn nothing_is_recorded_after_task_end() {
    let f = fixture();
    f.recorder.record_task_start("p", "m", "d", false).await;
    f.recorder.record_task_end("failed", "", "").await.unwrap();
    let count = f.records().len();
    f.recorder
        .on_request(request(1, "SYS", vec![msg("user", "late")], vec![]))
        .await;
    f.recorder.on_response(response(1, "late", vec![])).await;
    assert_eq!(f.records().len(), count);
    assert_eq!(f.recorder.stats().dropped_after_end, 2);
    // A resumed task unseals it.
    f.recorder.record_task_start("p", "m", "d", true).await;
    f.recorder
        .on_request(request(2, "SYS", vec![msg("user", "again")], vec![]))
        .await;
    assert!(f.records().len() > count + 1);
}

#[tokio::test]
async fn a_path_shaped_thread_ref_cannot_escape() {
    for bad in ["../../evil", "a/b", "..", ".", "x\\y"] {
        let f = fixture();
        let rec = f.recorder_for(bad);
        rec.record_task_start("p", "m", "d", false).await;
        rec.on_request(request(1, "SYS", vec![msg("user", "hi")], vec![]))
            .await;
        rec.on_response(response(1, "yo", vec![])).await;
        assert!(rec.stats().failures >= 1, "{bad:?}");
        // Nothing was created anywhere but possibly the session root itself.
        for outside in ["evil", "a", "x", "log", "threads"] {
            assert!(!f.dir.path().join(outside).exists(), "{bad:?} {outside}");
            assert!(!f.root().join(outside).exists(), "{bad:?} {outside}");
        }
        assert!(!f.root().join(".vam/threads/log").exists(), "{bad:?}");
        assert!(!f.root().join(".vam/log").exists(), "{bad:?}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixteen_concurrent_exchanges_keep_one_ordered_chain() {
    let f = fixture();
    let rec = f.recorder.clone();
    let mut handles = Vec::new();
    for i in 0..16u64 {
        let rec = rec.clone();
        handles.push(tokio::spawn(async move {
            let user = msg("user", &format!("question {i}"));
            rec.on_request(request(i + 1, "SYS", vec![user], vec![]))
                .await;
            tokio::task::yield_now().await;
            rec.on_response(response(i + 1, &format!("answer {i}"), vec![]))
                .await;
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    let page = f.tree().read(THREAD, 0, usize::MAX).unwrap();
    assert_eq!(page.integrity, Integrity::Ok);
    assert!(page
        .records
        .iter()
        .enumerate()
        .all(|(i, r)| r.seq == i as u64 + 1));
    // Every question and every answer made it to disk, none torn.
    for i in 0..16 {
        for text in [format!("question {i}"), format!("answer {i}")] {
            assert!(
                page.records
                    .iter()
                    .any(|r| r.message().is_some_and(|m| m.to_wire().content == text)),
                "{text}"
            );
        }
    }
    // Each answer carries the round of its own request.
    for r in page.records.iter().filter(|r| r.kind == kind::MESSAGE) {
        let m = r.message().unwrap();
        if m.role == "assistant" {
            let n: u64 = m
                .to_wire()
                .content
                .trim_start_matches("answer ")
                .parse()
                .unwrap();
            let question = page
                .records
                .iter()
                .find(|q| {
                    q.message()
                        .is_some_and(|m| m.to_wire().content == format!("question {n}"))
                })
                .unwrap();
            assert_eq!(r.round, question.round, "answer {n}");
        }
    }
    assert_eq!(f.recorder.stats().failures, 0);
}

#[tokio::test]
async fn an_abort_forgets_the_pending_round() {
    let f = fixture();
    f.recorder
        .on_request(request(1, "SYS", vec![msg("user", "hi")], vec![]))
        .await;
    assert_eq!(f.recorder.pending.lock().unwrap().len(), 1);
    f.recorder.on_abort(1);
    assert!(f.recorder.pending.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_cached_head_follows_the_last_write() {
    let f = fixture();
    assert!(f.recorder.cached_head().is_none());
    f.recorder.record_task_start("p", "m", "d", false).await;
    let head = f.recorder.cached_head().unwrap();
    assert_eq!(f.tree().head(THREAD).unwrap().unwrap(), head);
}

#[test]
fn the_store_hands_out_and_forgets_recorders() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    assert!(store.thread_recorder("s").is_none());
    let recorder = Arc::new(ThreadRecorder::with_resolver(
        "s".into(),
        std::path::PathBuf::new(),
        Arc::new(|| Err("none".into())),
    ));
    store.register_thread_recorder("s", recorder.clone());
    assert!(Arc::ptr_eq(&store.thread_recorder("s").unwrap(), &recorder));
    let heads = RecorderHeads {
        store: store.clone(),
    };
    use crate::turn_commit::ThreadLogHeads;
    assert!(heads.log_head("s").is_none(), "nothing recorded yet");
    store.remove_thread_recorder("s");
    assert!(store.thread_recorder("s").is_none());
}

/// Persist before act, end to end: when the guest sees the stream end, the
/// assistant record (with its tool call) is already on disk.
#[tokio::test]
async fn the_response_is_on_disk_before_the_guest_sees_the_stream_end() {
    use prost::Message;
    use tokio::net::{UnixListener, UnixStream};
    use vam_inference::{InferenceBrokerServer, ModelCatalog, SessionBudget, SimpleSecretLookup};
    use vam_spec::v1::{InferenceRequest, InferenceResponseChunk, ModelPreference};
    use vam_vsock::{split_payload, Frame, FrameType, StreamChunk};

    let f = fixture();
    f.recorder
        .record_task_start("run-echo now", "mock-model", "d", false)
        .await;
    let sock_dir = tempdir().unwrap();
    let sock = sock_dir.path().join("b.sock");
    let listener = UnixListener::bind(&sock).unwrap();
    let server = Arc::new(
        InferenceBrokerServer::new(
            Arc::new(ModelCatalog::new()),
            Arc::new(SessionBudget::new(None)),
            Arc::new(SimpleSecretLookup::new()),
        )
        .with_exchange_observer(f.recorder.clone()),
    );
    tokio::spawn(async move {
        if let Ok((stream, _)) = listener.accept().await {
            server.handle_connection(stream).await;
        }
    });
    let client = UnixStream::connect(&sock).await.unwrap();
    let (mut reader, mut writer) = tokio::io::split(client);
    let request = InferenceRequest {
        model_preference: Some(ModelPreference {
            provider: "mock".into(),
            model_name: "mock-model".into(),
            temperature: 0.0,
            top_p: 1.0,
            max_completion_tokens: 0,
        }),
        messages: vec![msg("system", "SYS"), msg("user", "please run-echo now")],
        tools: vec![],
        context_hint: None,
    };
    let mut payload = Vec::new();
    request.encode(&mut payload).unwrap();
    for frame in split_payload(&payload, 5, FrameType::StreamInference) {
        frame.write_to(&mut writer).await.unwrap();
    }
    let mut tool_name = String::new();
    loop {
        let frame = Frame::read_from(&mut reader).await.unwrap();
        let chunk = StreamChunk::decode(&frame.payload).unwrap();
        if chunk.end_of_message {
            break;
        }
        let resp = InferenceResponseChunk::decode(chunk.payload.as_ref()).unwrap();
        if let Some(c) = resp.tool_calls.first() {
            tool_name = c.name.clone();
        }
    }
    assert_eq!(tool_name, "echo");
    // At the stream end the log already holds the user message and the assistant tool call.
    let records = f.records();
    let last = records
        .last()
        .unwrap()
        .message()
        .expect("assistant message");
    assert_eq!(last.role, "assistant");
    assert_eq!(last.to_wire().tool_calls[0].1, "echo");
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
}

/// Measurement, not a check: `cargo test -p vam-orchestrator --lib measure_round_cost -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "measurement"]
async fn measure_round_cost() {
    let f = fixture();
    let rec = &f.recorder;
    let mut history: Vec<ChatMessage> = vec![msg("user", "go")];
    let rounds = 200u64;
    let started = std::time::Instant::now();
    for i in 1..=rounds {
        rec.on_request(request(
            i,
            "SYS",
            history.clone(),
            vec![tool_spec("t", "d")],
        ))
        .await;
        rec.on_response(response(i, &format!("answer {i}"), vec![]))
            .await;
        history.push(assistant_calls(&format!("answer {i}"), vec![]));
        history.push(msg("user", &format!("next {i}")));
    }
    let total = started.elapsed();
    println!(
        "recorder: {rounds} rounds in {total:?}: {:?} per round (2 fsynced appends each)",
        total / rounds as u32
    );
}

use vam_spec::thread_context::{LoopState, PendingCall, SuspendTail, WireMessage as SpecWire};

fn tail(pending: &str, results: &[(&str, &str)]) -> SuspendTail {
    SuspendTail {
        schema_version: 3,
        run_id: "urn:vam:run:test".into(),
        lifecycle_generation: 1,
        kind: "suspend".into(),
        messages: results
            .iter()
            .map(|(id, text)| SpecWire {
                role: "tool".into(),
                content: (*text).into(),
                tool_calls: vec![],
                tool_call_id: (*id).into(),
            })
            .collect(),
        pending: PendingCall {
            tool_call_id: pending.into(),
            tool_name: "write_file".into(),
            args_json: "{}".into(),
        },
        state: LoopState::default(),
    }
}

/// A request and a response whose assistant message calls a, b and c.
async fn three_calls(rec: &ThreadRecorder) -> (ChatMessage, ChatMessage) {
    let u = msg("user", "go");
    rec.on_request(request(1, "SYS", vec![u.clone()], vec![]))
        .await;
    let calls = vec![
        call("a", "read_file", "{}"),
        call("b", "write_file", "{}"),
        call("c", "read_file", "{}"),
    ];
    rec.on_response(response(1, "", calls.clone())).await;
    (u, assistant_calls("", calls))
}

#[tokio::test]
async fn a_suspend_tail_is_validated_appended_as_guest_and_the_resume_extends_the_log() {
    let f = fixture();
    let rec = &f.recorder;
    rec.record_task_start("go", "m", "d", false).await;
    let (u, asst) = three_calls(rec).await;
    // `a` ran, `b` suspended, `c` never ran.
    let t = tail("b", &[("a", "contents")]);
    rec.record_suspend(&t, &"1".repeat(64)).await.unwrap();

    let records = f.records();
    let guest: Vec<&Record> = records.iter().filter(|r| r.src == "guest").collect();
    assert_eq!(guest.len(), 1);
    assert_eq!(guest[0].kind, kind::MESSAGE);
    let suspend = records.last().unwrap();
    assert_eq!(suspend.kind, kind::SUSPEND);
    let body = suspend.body.as_ref().unwrap();
    assert_eq!(body["pending"]["tool_call_id"], "b");
    assert_eq!(
        body["unanswered"],
        serde_json::json!(["c"]),
        "the host works it out"
    );

    // The resumed task's first request: tail, approved result, placeholder, nudge.
    let rolled = rec.stats().rollbacks;
    let resumed = vec![
        u.clone(),
        asst.clone(),
        tool_msg("a", "contents"),
        tool_msg("b", "written"),
        tool_msg("c", "placeholder"),
        msg("user", "Human approval was granted."),
    ];
    rec.on_request(request(2, "SYS", resumed.clone(), vec![]))
        .await;
    assert_eq!(rec.stats().rollbacks, rolled, "a pure extension of the log");
    assert_eq!(
        f.assembled(),
        resumed.iter().map(wire_of).collect::<Vec<_>>()
    );
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
}

#[tokio::test]
async fn a_suspend_tail_that_does_not_answer_the_last_assistant_message_is_refused() {
    let f = fixture();
    let rec = &f.recorder;
    three_calls(rec).await;
    for bad in [
        tail("b", &[("zzz", "x")]),           // not a call of the message
        tail("b", &[("a", "x"), ("a", "y")]), // twice
        tail("b", &[("b", "x")]),             // the pending call itself
        tail("nope", &[("a", "x")]),          // pending is not a call
    ] {
        assert!(rec.record_suspend(&bad, &"2".repeat(64)).await.is_err());
    }
    let mut not_tool = tail("b", &[("a", "x")]);
    not_tool.messages[0].role = "user".into();
    assert!(rec
        .record_suspend(&not_tool, &"3".repeat(64))
        .await
        .is_err());
    assert!(
        f.records().iter().all(|r| r.src != "guest"),
        "nothing was appended"
    );

    // After a user message the last message is not an assistant message.
    let f2 = fixture();
    f2.recorder
        .on_request(request(1, "SYS", vec![msg("user", "hi")], vec![]))
        .await;
    assert!(f2
        .recorder
        .record_suspend(&tail("b", &[]), &"4".repeat(64))
        .await
        .is_err());
}

#[tokio::test]
async fn a_retried_suspend_upload_is_recorded_once() {
    let f = fixture();
    let rec = &f.recorder;
    three_calls(rec).await;
    let t = tail("b", &[("a", "contents")]);
    let digest = "5".repeat(64);
    rec.record_suspend(&t, &digest).await.unwrap();
    let count = f.records().len();
    rec.record_suspend(&t, &digest).await.unwrap();
    assert_eq!(f.records().len(), count);
}

#[tokio::test]
async fn a_text_parsed_reply_keeps_the_raw_text_in_the_record_but_never_in_the_context() {
    let f = fixture();
    let rec = &f.recorder;
    let raw = "{\"tool\":\"run_it\",\"arguments\":{\"x\":1}}";
    rec.on_request(request(1, "SYS", vec![msg("user", "go")], vec![]))
        .await;
    rec.on_response(response(1, raw, vec![])).await;
    let records = f.records();
    let kept = records
        .iter()
        .filter(|r| r.kind == kind::MESSAGE)
        .any(|r| r.body.as_ref().is_some_and(|b| b["raw_text"] == raw));
    assert!(kept, "audit keeps what the model wrote");
    let ctx = assemble_context(&records, MAIN_LANE).unwrap();
    let sent = serde_json::to_string(&ctx.messages).unwrap();
    assert!(!sent.contains("raw_text") && !sent.contains(raw));
    assert_eq!(ctx.messages[1].to_wire().tool_calls[0].1, "run_it");
}

// ---------------------------------------------------------------------------
// Turn commit boundaries and the stop record (AR-2.6)

const RUN_ID: &str = "urn:vam:run:test";

fn commit_store() -> Arc<SqliteWorkflowStore> {
    let store = SqliteWorkflowStore::in_memory().unwrap();
    store
        .persist_run_spec(
            RUN_ID,
            r#"{"inputs":{"prompt":"go"}}"#,
            Some("sess-test"),
            "urn:vam:agent:test",
            None,
            "User::\"tester\"",
        )
        .unwrap();
    Arc::new(store)
}

fn recorder_with_commits(f: &Fixture, store: &Arc<SqliteWorkflowStore>) -> Arc<ThreadRecorder> {
    let scope = f.scope_for(THREAD);
    Arc::new(
        ThreadRecorder::with_resolver(
            "sess-test".into(),
            f.root(),
            Arc::new(move || Ok(scope.clone())),
        )
        .with_commit_store(store.clone()),
    )
}

fn spec_tail(messages: &[ChatMessage]) -> String {
    let wires: Vec<vam_spec::thread_context::WireMessage> =
        messages.iter().map(spec_wire).collect();
    vam_spec::thread_context::tail_digest(&wires)
}

/// Round 1 writes a file and takes a turn commit that completes when round 2's
/// request carries the results; round 2 only reads; round 3's call is cut off.
async fn stopped_after_read_round(
    rec: &ThreadRecorder,
    store: &SqliteWorkflowStore,
) -> (Vec<ChatMessage>, u64) {
    let tools = vec![tool_spec("read_file", "Read a file")];
    let u = msg("user", "Save the note, then run the slow command");
    let c1 = call(
        "c1",
        "write_file",
        "{\"path\":\"note.txt\",\"content\":\"kiwi-42\"}",
    );
    let a1 = assistant_calls("", vec![c1.clone()]);
    let t1 = tool_msg("c1", "ok");
    let c2 = call("c2", "read_file", "{\"path\":\"note.txt\"}");
    let a2 = assistant_calls("", vec![c2.clone()]);
    let t2 = tool_msg("c2", "kiwi-42");
    let c3 = call("c3", "run", "{\"command\":\"sleep 30\"}");
    let a3 = assistant_calls("", vec![c3.clone()]);

    rec.on_request(request(1, "SYS", vec![u.clone()], tools.clone()))
        .await;
    rec.on_response(response(1, "", vec![c1])).await;
    // CommitTurn for round 1, as the handler records it.
    let head = rec.cached_head().expect("head after the assistant message");
    store
        .record_turn_commit_pending(
            RUN_ID,
            "sess-test",
            1,
            head.seq,
            &head.digest,
            &spec_tail(std::slice::from_ref(&t1)),
        )
        .unwrap();
    store
        .set_turn_commit_manifest(RUN_ID, 1, "manifest-1")
        .unwrap();
    rec.on_request(request(
        2,
        "SYS",
        vec![u.clone(), a1.clone(), t1.clone()],
        tools.clone(),
    ))
    .await;
    rec.on_response(response(2, "", vec![c2])).await;
    rec.on_request(request(
        3,
        "SYS",
        vec![u.clone(), a1.clone(), t1.clone(), a2.clone(), t2.clone()],
        tools,
    ))
    .await;
    rec.on_response(response(3, "", vec![c3])).await;
    let boundary = store
        .last_complete_turn_commit(RUN_ID, "sess-test")
        .unwrap()
        .expect("round 1 completed")
        .boundary_seq
        .expect("boundary");
    (vec![u, a1, t1, a2, t2, a3], boundary)
}

#[tokio::test]
async fn a_request_with_the_announced_results_completes_the_turn_commit_at_that_boundary() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let (_, boundary) = stopped_after_read_round(&rec, &store).await;
    let row = store.turn_commit_row(RUN_ID, 1).unwrap().unwrap();
    assert_eq!(row.state, crate::turn_commit::TurnCommitState::Complete);
    // The boundary is the log record of the tool result, not the head.
    let records = f.records();
    let at = records.iter().find(|r| r.seq == boundary).unwrap();
    assert_eq!(at.kind, kind::MESSAGE);
    assert_eq!(at.message().unwrap().role, "tool");
    assert!(
        row.log_seq < boundary,
        "the commit was taken at the assistant message"
    );
}

#[tokio::test]
async fn a_request_with_other_results_completes_nothing() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let tools = vec![tool_spec("read_file", "Read")];
    let u = msg("user", "go");
    let c1 = call("c1", "write_file", "{}");
    let a1 = assistant_calls("", vec![c1.clone()]);
    rec.on_request(request(1, "SYS", vec![u.clone()], tools.clone()))
        .await;
    rec.on_response(response(1, "", vec![c1])).await;
    let head = rec.cached_head().unwrap();
    store
        .record_turn_commit_pending(
            RUN_ID,
            "sess-test",
            1,
            head.seq,
            &head.digest,
            &spec_tail(&[tool_msg("c1", "ok")]),
        )
        .unwrap();
    store.set_turn_commit_manifest(RUN_ID, 1, "m").unwrap();
    // The guest announced "ok" but sends something else.
    rec.on_request(request(
        2,
        "SYS",
        vec![u, a1, tool_msg("c1", "different")],
        tools,
    ))
    .await;
    let row = store.turn_commit_row(RUN_ID, 1).unwrap().unwrap();
    assert_eq!(row.state, crate::turn_commit::TurnCommitState::Pending);
    assert_eq!(row.boundary_seq, None);
}

#[tokio::test]
async fn a_stop_answers_the_cut_off_call_and_keeps_the_conversation_when_no_write_finished_after_the_commit(
) {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let (sent, _) = stopped_after_read_round(&rec, &store).await;
    let head = rec.record_stop().await.expect("task_end head");
    let records = f.records();
    assert!(
        records.iter().all(|r| r.kind != kind::ROLLBACK),
        "no rollback record"
    );
    let last = records.last().unwrap();
    assert_eq!(last.kind, kind::TASK_END);
    assert_eq!(last.body.as_ref().unwrap()["outcome"], "stopped");
    assert_eq!(head.seq, last.seq);
    // The context now ends with the host's answer to the cut-off call.
    let mut want: Vec<WireMessage> = sent.iter().map(wire_of).collect();
    want.push(wire_of(&tool_msg("c3", STOPPED_CALL_RESULT)));
    assert_eq!(f.assembled(), want);
    let host_msg = records
        .iter()
        .rev()
        .find(|r| r.kind == kind::MESSAGE)
        .unwrap();
    assert_eq!(host_msg.src, "host");
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
    // Sealed: a late request is dropped.
    rec.on_request(request(9, "SYS", vec![msg("user", "late")], vec![]))
        .await;
    assert_eq!(rec.stats().dropped_after_end, 1);
    // What completed stays complete.
    assert_eq!(
        store.turn_commit(RUN_ID, 1).unwrap().map(|(s, _)| s),
        Some(crate::turn_commit::TurnCommitState::Complete)
    );
}

#[tokio::test]
async fn a_stop_rolls_the_conversation_back_when_a_file_writing_result_is_after_the_commit() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let tools = vec![tool_spec("read_file", "Read")];
    let u = msg("user", "write two files");
    let c1 = call("c1", "write_file", "{\"path\":\"a\"}");
    let a1 = assistant_calls("", vec![c1.clone()]);
    let t1 = tool_msg("c1", "ok a");
    let c2 = call("c2", "write_file", "{\"path\":\"b\"}");
    let a2 = assistant_calls("", vec![c2.clone()]);
    let t2 = tool_msg("c2", "ok b");
    let c3 = call("c3", "run", "{}");

    rec.on_request(request(1, "SYS", vec![u.clone()], tools.clone()))
        .await;
    rec.on_response(response(1, "", vec![c1])).await;
    let head = rec.cached_head().unwrap();
    store
        .record_turn_commit_pending(
            RUN_ID,
            "sess-test",
            1,
            head.seq,
            &head.digest,
            &spec_tail(std::slice::from_ref(&t1)),
        )
        .unwrap();
    store.set_turn_commit_manifest(RUN_ID, 1, "m1").unwrap();
    rec.on_request(request(
        2,
        "SYS",
        vec![u.clone(), a1.clone(), t1.clone()],
        tools.clone(),
    ))
    .await;
    rec.on_response(response(2, "", vec![c2])).await;
    // Round 2's commit never happened (say the snapshot failed), yet its
    // results reach the log with the next request.
    rec.on_request(request(
        3,
        "SYS",
        vec![u.clone(), a1.clone(), t1.clone(), a2, t2],
        tools,
    ))
    .await;
    rec.on_response(response(3, "", vec![c3])).await;
    let boundary = store
        .last_complete_turn_commit(RUN_ID, "sess-test")
        .unwrap()
        .unwrap()
        .boundary_seq
        .unwrap();

    rec.record_stop().await.expect("head");
    let records = f.records();
    let rollback = records
        .iter()
        .find(|r| r.kind == kind::ROLLBACK)
        .expect("rollback");
    let body = rollback.body.as_ref().unwrap();
    assert_eq!(body["to"], boundary);
    assert_eq!(body["reason"], "stopped");
    // The conversation is exactly as of the commit: the second write and the
    // cut-off call are hidden together with the files they changed.
    assert_eq!(f.assembled(), vec![wire_of(&u), wire_of(&a1), wire_of(&t1)]);
    assert_eq!(rec.stats().rollbacks, 1);
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
}

#[tokio::test]
async fn a_stop_with_no_commit_rolls_back_the_whole_task_if_a_write_finished_and_not_otherwise() {
    // A write finished, nothing was committed: back to the task's start.
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let tools = vec![tool_spec("read_file", "Read")];
    let u = msg("user", "go");
    let c1 = call("c1", "write_file", "{}");
    let a1 = assistant_calls("", vec![c1.clone()]);
    rec.on_request(request(1, "SYS", vec![u.clone()], tools.clone()))
        .await;
    rec.on_response(response(1, "", vec![c1])).await;
    rec.on_request(request(
        2,
        "SYS",
        vec![u, a1, tool_msg("c1", "ok")],
        tools.clone(),
    ))
    .await;
    rec.on_response(response(2, "working", vec![])).await;
    rec.record_stop().await.expect("head");
    assert!(f.records().iter().any(|r| r.kind == kind::ROLLBACK));
    assert!(
        f.assembled().is_empty(),
        "the stopped task's messages are hidden"
    );

    // Only reads happened: nothing to roll back, the open call is answered.
    let f2 = fixture();
    let store2 = commit_store();
    let rec2 = recorder_with_commits(&f2, &store2);
    rec2.record_task_start("go", "mock-model", "d", false).await;
    let c = call("c1", "read_file", "{}");
    rec2.on_request(request(1, "SYS", vec![msg("user", "go")], tools))
        .await;
    rec2.on_response(response(1, "", vec![c])).await;
    rec2.record_stop().await.expect("head");
    assert!(f2.records().iter().all(|r| r.kind != kind::ROLLBACK));
    let assembled = f2.assembled();
    assert_eq!(assembled.last().unwrap().tool_call_id, "c1");
    assert_eq!(assembled.last().unwrap().content, STOPPED_CALL_RESULT);
}

#[tokio::test]
async fn the_users_stop_of_a_parked_call_answers_it_not_run_stopped_by_user() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let tools = vec![tool_spec("write_file", "Write")];
    let c = call("c1", "write_file", "{}");
    rec.on_request(request(1, "SYS", vec![msg("user", "go")], tools))
        .await;
    rec.on_response(response(1, "", vec![c])).await;
    rec.mark_user_stop();
    rec.record_stop().await.expect("head");
    let assembled = f.assembled();
    assert_eq!(assembled.last().unwrap().tool_call_id, "c1");
    assert_eq!(
        assembled.last().unwrap().content,
        "Not run: stopped by user"
    );
    let end = f
        .records()
        .into_iter()
        .rev()
        .find(|r| r.kind == kind::TASK_END)
        .expect("task_end");
    assert_eq!(end.body.as_ref().unwrap()["outcome"], "stopped");
}

// ---------------------------------------------------------------------------
// Size limits and the three-strike rule

fn quota_recorder(
    f: &Fixture,
    thread_bytes: u64,
    workspace_bytes: u64,
) -> (Arc<ThreadRecorder>, LogUsage) {
    let scope = f.scope_for(THREAD);
    let quota = LogQuota {
        thread_bytes,
        workspace_bytes,
        reserve_bytes: 64 * 1024,
    };
    let probe_root = f.root();
    let usage = LogUsage::new(move || TreeThreadStore::new(&probe_root).log_bytes());
    let recorder = Arc::new(
        ThreadRecorder::with_resolver(
            "sess-test".into(),
            f.root(),
            Arc::new(move || Ok(scope.clone())),
        )
        .with_quota(quota, usage.clone()),
    );
    (recorder, usage)
}

#[tokio::test]
async fn a_full_thread_log_refuses_the_request_leaves_a_gap_and_still_takes_the_end() {
    let f = fixture();
    let (rec, _) = quota_recorder(&f, 6000, 1 << 30);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let u = msg("user", "hello");
    assert!(rec
        .on_request(request(1, "SYS", vec![u.clone()], vec![]))
        .await
        .is_ok());
    rec.on_response(response(1, "hi", vec![])).await;

    let big = msg("user", &"x".repeat(8000));
    let refusal = rec
        .on_request(request(2, "SYS", vec![u.clone(), big.clone()], vec![]))
        .await
        .expect_err("over the thread limit");
    assert_eq!(refusal.reason, REFUSAL_QUOTA);
    assert!(refusal.message.contains("thread log is full"));
    assert!(
        !refusal.message.contains('/'),
        "no host path: {}",
        refusal.message
    );

    let gaps: Vec<_> = f
        .records()
        .into_iter()
        .filter(|r| r.kind == kind::GAP)
        .collect();
    assert_eq!(gaps.len(), 1);
    let body = gaps[0].body.as_ref().unwrap();
    assert_eq!(body["reason"], "quota");
    assert_eq!(body["scope"], "thread");
    assert_eq!(body["limit"], 6000);

    // Every later request is refused at once and writes nothing more.
    let before = f.records().len();
    let again = rec
        .on_request(request(3, "SYS", vec![u], vec![]))
        .await
        .expect_err("still full");
    assert_eq!(again.reason, REFUSAL_QUOTA);
    assert_eq!(f.records().len(), before);

    // The reserve lets the task end be recorded, and the chain stays good.
    rec.record_task_end("failed", "", "")
        .await
        .expect("task_end");
    assert_eq!(f.records().last().unwrap().kind, kind::TASK_END);
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
    // The big message never reached the log.
    assert!(f.assembled().iter().all(|m| m.content != big.content));
}

#[tokio::test]
async fn a_full_workspace_refuses_a_new_system_prompt_before_the_provider() {
    let f = fixture();
    let (rec, _) = quota_recorder(&f, 1 << 30, 3000);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let refusal = rec
        .on_request(request(
            1,
            &"S".repeat(5000),
            vec![msg("user", "hi")],
            vec![],
        ))
        .await
        .expect_err("the prompt does not fit");
    assert_eq!(refusal.reason, REFUSAL_QUOTA);
    assert!(refusal.message.contains("workspace log is full"));
    let gap = f
        .records()
        .into_iter()
        .find(|r| r.kind == kind::GAP)
        .unwrap();
    assert_eq!(gap.body.as_ref().unwrap()["scope"], "workspace");
}

#[tokio::test]
async fn three_failed_writes_in_a_row_refuse_the_request_and_a_good_write_resets_the_count() {
    let f = fixture();
    // A tree root that is a file: every append fails.
    let broken = f.dir.path().join("not-a-dir");
    std::fs::write(&broken, b"x").unwrap();
    let scope = f.scope_for(THREAD);
    let rec = ThreadRecorder::with_resolver(
        "sess-test".into(),
        broken,
        Arc::new(move || Ok(scope.clone())),
    );
    let u = msg("user", "hi");
    for round in 1..=2 {
        let r = rec
            .on_request(request(round, "SYS", vec![u.clone()], vec![]))
            .await;
        assert!(r.is_ok(), "failure {round} stays a gap: {r:?}");
    }
    let third = rec
        .on_request(request(3, "SYS", vec![u.clone()], vec![]))
        .await;
    let refusal = third.expect_err("the third failure in a row refuses");
    assert_eq!(refusal.reason, REFUSAL_UNAVAILABLE);
    // And it stays refused.
    assert!(rec
        .on_request(request(4, "SYS", vec![u], vec![]))
        .await
        .is_err());
    assert!(rec.stats().failures >= 3);

    // A recorder that writes fine never accumulates strikes.
    let f2 = fixture();
    for round in 1..=5 {
        let r = f2
            .recorder
            .on_request(request(
                round,
                "SYS",
                vec![msg("user", &format!("m{round}"))],
                vec![],
            ))
            .await;
        assert!(r.is_ok());
    }
}

// ---- compaction (AR-2.10) ----

fn summary_request(
    id: u64,
    prefix: &[ChatMessage],
    keep_index: usize,
    reason: &str,
) -> ExchangeRequest {
    let hint = vam_spec::v1::ContextHint {
        lane: vam_spec::v1::context_hint::Lane::Summary as i32,
        keep_from_index: keep_index as u32,
        keep_from_msg_sha256: cmp::message_digest(&prefix[keep_index]),
        reason: reason.into(),
    };
    let mut rest = prefix.to_vec();
    rest.push(cmp::instruction_message(Some("- [ ] a")));
    let mut req = request(id, "SYS", rest, vec![tool_spec("read_file", "Read a file")]);
    req.context_hint = Some(hint);
    req
}

fn main_request(id: u64, rest: &[ChatMessage]) -> ExchangeRequest {
    request(
        id,
        "SYS",
        rest.to_vec(),
        vec![tool_spec("read_file", "Read a file")],
    )
}

/// Records a plain conversation u1 a1 u2 a2 u3 and returns its messages.
async fn short_thread(rec: &ThreadRecorder) -> Vec<ChatMessage> {
    let (u1, a1) = (msg("user", "use bravo.txt"), msg("assistant", "ok"));
    let (u2, a2) = (msg("user", "and be brief"), msg("assistant", "sure"));
    let u3 = msg("user", "now write the report");
    rec.on_request(main_request(1, std::slice::from_ref(&u1)))
        .await
        .unwrap();
    rec.on_response(response(1, "ok", vec![])).await;
    rec.on_request(main_request(2, &[u1.clone(), a1.clone(), u2.clone()]))
        .await
        .unwrap();
    rec.on_response(response(2, "sure", vec![])).await;
    let all = vec![u1, a1, u2, a2, u3];
    rec.on_request(main_request(3, &all)).await.unwrap();
    all
}

async fn compact(rec: &ThreadRecorder, id: u64, prefix: &[ChatMessage], keep: usize, text: &str) {
    rec.on_request(summary_request(id, prefix, keep, "tokens"))
        .await
        .unwrap();
    rec.on_response(response(id, text, vec![])).await;
}

#[tokio::test]
async fn a_verified_summary_exchange_writes_a_compaction_off_the_main_lane() {
    let f = fixture();
    let all = short_thread(&f.recorder).await;
    compact(&f.recorder, 4, &all, 4, "SUMMARY text").await;

    let records = f.records();
    let compaction = records
        .iter()
        .find(|r| r.kind == kind::COMPACTION)
        .expect("a compaction record");
    assert_eq!(compaction.lane, MAIN_LANE);
    let body = compaction.body.as_ref().unwrap();
    assert_eq!(body["reason"], "tokens");
    // Numbers come from the host's record: the summary reply's reported prompt tokens.
    assert_eq!(body["tokens_before"], 10);
    // The kept start is the host's own seq of the user message, not a guest value.
    let u3_seq = records
        .iter()
        .filter(|r| r.kind == kind::MESSAGE && r.lane == MAIN_LANE)
        .find(|r| {
            r.message()
                .is_some_and(|m| m.to_wire().content == "now write the report")
        })
        .unwrap()
        .seq;
    assert_eq!(body["keep_from"], u3_seq);
    // The exchange itself is on the summary lane, with the main lane as parent.
    let summary_lane: Vec<&Record> = records
        .iter()
        .filter(|r| r.lane == cmp::SUMMARY_LANE)
        .collect();
    assert_eq!(summary_lane.len(), 2);
    assert!(summary_lane
        .iter()
        .all(|r| r.parent_lane.as_deref() == Some(MAIN_LANE)));
    // The context starts from the compaction: the summary, then the kept tail.
    let want_summary = cmp::summary_message("SUMMARY text", &all[..4]);
    assert_eq!(
        f.assembled(),
        vec![wire_of(&want_summary), wire_of(&all[4])]
    );
    assert!(want_summary.content.contains("use bravo.txt"));
    // Pre-compaction history is still in the log.
    assert!(records.iter().any(|r| r
        .message()
        .is_some_and(|m| m.to_wire().content == "use bravo.txt")));
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
    // The next request extends the compacted context: no rollback, no rebase.
    let next = vec![want_summary.clone(), all[4].clone()];
    f.recorder.on_request(main_request(5, &next)).await.unwrap();
    assert_eq!(f.recorder.stats().rollbacks, 0);
    assert_eq!(
        f.records()
            .iter()
            .filter(|r| r.kind == kind::COMPACTION)
            .count(),
        1
    );
}

#[tokio::test]
async fn an_unverifiable_hint_is_refused_and_writes_nothing_to_the_main_lane() {
    let f = fixture();
    let all = short_thread(&f.recorder).await;
    let before = f.assembled();

    // A sha that is not the message at that index.
    let mut bad_sha = summary_request(4, &all, 4, "tokens");
    bad_sha.context_hint.as_mut().unwrap().keep_from_msg_sha256 = "0".repeat(64);
    // An index past the end, and one that is a claimed 'tool' start.
    let mut past_end = summary_request(5, &all, 4, "tokens");
    past_end.context_hint.as_mut().unwrap().keep_from_index = 9;
    let bad_reason = summary_request(6, &all, 4, "rebase");
    // A prefix that does not extend the recorded context (history rewritten).
    let mut rewritten = all.clone();
    rewritten[0] = msg("user", "forget bravo.txt");
    let diverged = summary_request(7, &rewritten, 4, "tokens");
    // No instruction at the end.
    let mut no_instruction = summary_request(8, &all, 4, "tokens");
    no_instruction.messages.pop();

    for req in [bad_sha, past_end, bad_reason, diverged, no_instruction] {
        let refusal = f.recorder.on_request(req).await.unwrap_err();
        assert_eq!(refusal.reason, REFUSAL_COMPACTION);
        assert!(
            !refusal.message.contains('/'),
            "no paths: {}",
            refusal.message
        );
    }
    let records = f.records();
    assert!(records.iter().all(|r| r.kind != kind::COMPACTION));
    assert!(
        records.iter().all(|r| r.kind != kind::ROLLBACK),
        "a summary exchange never rewrites"
    );
    assert_eq!(f.assembled(), before);
    assert_eq!(f.recorder.stats().rollbacks, 0);
}

#[tokio::test]
async fn a_kept_tail_may_not_start_with_a_tool_result_or_the_old_summary() {
    let f = fixture();
    let u1 = msg("user", "go");
    let a1 = assistant_calls("", vec![call("c1", "read_file", "{}")]);
    let t1 = tool_msg("c1", "data");
    let all = vec![
        u1.clone(),
        a1,
        t1,
        msg("assistant", "done"),
        msg("user", "next"),
    ];
    f.recorder.on_request(main_request(1, &all)).await.unwrap();
    let refusal = f
        .recorder
        .on_request(summary_request(2, &all, 2, "tokens"))
        .await
        .unwrap_err();
    assert_eq!(refusal.reason, REFUSAL_COMPACTION);
    // After a compaction, index 0 is its summary and cannot be the kept start.
    compact(&f.recorder, 3, &all, 3, "S1").await;
    let s1 = cmp::summary_message("S1", &all[..3]);
    let ctx = vec![s1, all[3].clone(), all[4].clone()];
    f.recorder.on_request(main_request(4, &ctx)).await.unwrap();
    let refusal = f
        .recorder
        .on_request(summary_request(5, &ctx, 0, "tokens"))
        .await
        .unwrap_err();
    assert_eq!(refusal.reason, REFUSAL_COMPACTION);
}

#[tokio::test]
async fn a_reply_with_tool_calls_or_no_text_writes_no_compaction() {
    let f = fixture();
    let all = short_thread(&f.recorder).await;
    f.recorder
        .on_request(summary_request(4, &all, 4, "tokens"))
        .await
        .unwrap();
    f.recorder
        .on_response(response(4, "", vec![call("x", "read_file", "{}")]))
        .await;
    f.recorder
        .on_request(summary_request(5, &all, 4, "tokens"))
        .await
        .unwrap();
    f.recorder.on_response(response(5, "   ", vec![])).await;
    let records = f.records();
    assert!(records.iter().all(|r| r.kind != kind::COMPACTION));
    assert_eq!(
        records
            .iter()
            .filter(|r| r.lane == cmp::SUMMARY_LANE)
            .count(),
        4
    );
    assert_eq!(f.assembled().len(), 5, "the main context is unchanged");
}

#[tokio::test]
async fn a_divergence_inside_the_kept_head_rebases_and_never_unhides_older_messages() {
    let f = fixture();
    let all = short_thread(&f.recorder).await;
    compact(&f.recorder, 4, &all, 4, "S").await;
    let summary = cmp::summary_message("S", &all[..4]);

    // The kept tail changed (a rewritten message): a rebase, not a rollback below C.
    let rewritten = msg("user", "now write the report (edited)");
    f.recorder
        .on_request(main_request(5, &[summary.clone(), rewritten.clone()]))
        .await
        .unwrap();
    let records = f.records();
    let rebase = records
        .iter()
        .rfind(|r| r.kind == kind::COMPACTION)
        .unwrap();
    assert_eq!(rebase.body.as_ref().unwrap()["reason"], "rebase");
    assert_eq!(
        f.assembled(),
        vec![wire_of(&summary), wire_of(&rewritten)],
        "older messages stay hidden"
    );
    assert!(f.assembled().iter().all(|m| m.content != "use bravo.txt"));

    // The summary itself differs (k = 0): the same.
    let other = msg("user", "a different first message");
    f.recorder
        .on_request(main_request(6, &[other.clone(), rewritten.clone()]))
        .await
        .unwrap();
    assert_eq!(f.assembled(), vec![wire_of(&other), wire_of(&rewritten)]);
    assert!(f.assembled().iter().all(|m| m.content != "use bravo.txt"));
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
}

#[tokio::test]
async fn a_divergence_after_the_head_rolls_back_but_never_below_the_compaction() {
    let f = fixture();
    let all = short_thread(&f.recorder).await;
    compact(&f.recorder, 4, &all, 4, "S").await;
    let summary = cmp::summary_message("S", &all[..4]);
    let a3 = msg("assistant", "first draft");
    let u4 = msg("user", "shorter");
    f.recorder
        .on_request(main_request(5, &[summary.clone(), all[4].clone()]))
        .await
        .unwrap();
    f.recorder
        .on_response(response(5, "first draft", vec![]))
        .await;
    f.recorder
        .on_request(main_request(
            6,
            &[summary.clone(), all[4].clone(), a3.clone(), u4.clone()],
        ))
        .await
        .unwrap();
    // Now the assistant message is rewritten: a plain rollback above C.
    let a3b = msg("assistant", "rewritten draft");
    f.recorder
        .on_request(main_request(
            7,
            &[summary.clone(), all[4].clone(), a3b.clone()],
        ))
        .await
        .unwrap();
    let records = f.records();
    let c_seq = records
        .iter()
        .find(|r| r.kind == kind::COMPACTION)
        .unwrap()
        .seq;
    let rollback = records.iter().find(|r| r.kind == kind::ROLLBACK).unwrap();
    assert!(rollback.body.as_ref().unwrap()["to"].as_u64().unwrap() >= c_seq);
    assert_eq!(
        f.assembled(),
        vec![wire_of(&summary), wire_of(&all[4]), wire_of(&a3b)]
    );
    assert!(f.assembled().iter().all(|m| m.content != "use bravo.txt"));
}

#[tokio::test]
async fn a_second_compaction_replaces_the_first_and_keeps_the_user_messages() {
    let f = fixture();
    let all = short_thread(&f.recorder).await;
    compact(&f.recorder, 4, &all, 3, "S1").await;
    let s1 = cmp::summary_message("S1", &all[..3]);
    let ctx1 = vec![s1.clone(), all[3].clone(), all[4].clone()];
    f.recorder.on_request(main_request(5, &ctx1)).await.unwrap();
    f.recorder.on_response(response(5, "report", vec![])).await;
    let mut ctx2 = ctx1.clone();
    ctx2.push(msg("assistant", "report"));
    ctx2.push(msg("user", "thanks, one more thing"));
    f.recorder.on_request(main_request(6, &ctx2)).await.unwrap();
    compact(&f.recorder, 7, &ctx2, 4, "S2").await;
    let s2 = cmp::summary_message("S2", &ctx2[..4]);
    assert!(
        s2.content.contains("use bravo.txt"),
        "carried from the first summary"
    );
    assert!(s2.content.contains("and be brief") || s2.content.contains("be brief"));
    assert_eq!(f.assembled(), vec![wire_of(&s2), wire_of(&ctx2[4])]);
    assert_eq!(
        f.assembled()
            .iter()
            .filter(|m| m.content.starts_with(cmp::SUMMARY_HEADER))
            .count(),
        1
    );
}

#[tokio::test]
async fn a_request_over_the_context_guard_is_refused() {
    let f = fixture();
    let big = msg("user", &"x".repeat(MAX_CONTEXT_BYTES + 1));
    let refusal = f
        .recorder
        .on_request(main_request(1, &[big]))
        .await
        .unwrap_err();
    assert_eq!(refusal.reason, REFUSAL_CONTEXT_TOO_LARGE);
    assert!(f.records().iter().all(|r| r.kind != kind::MESSAGE));
}

#[tokio::test]
async fn the_hint_cannot_rewrite_history_even_with_a_valid_looking_exchange_after_a_stop() {
    // A summary request that arrives with unrecorded new messages extends the
    // main lane as an ordinary request would, and nothing else.
    let f = fixture();
    let all = short_thread(&f.recorder).await;
    let mut longer = all.clone();
    longer.push(msg("assistant", "extra"));
    longer.push(msg("user", "one more"));
    f.recorder
        .on_request(summary_request(4, &longer, 6, "bytes"))
        .await
        .unwrap();
    f.recorder.on_response(response(4, "S", vec![])).await;
    let records = f.records();
    assert!(records.iter().all(|r| r.kind != kind::ROLLBACK));
    let body = records
        .iter()
        .find(|r| r.kind == kind::COMPACTION)
        .unwrap()
        .body
        .clone()
        .unwrap();
    assert_eq!(body["reason"], "bytes");
    assert_eq!(f.assembled().len(), 2);
}

// --- todo recitation notes (the `note` lane) ---

fn recorder_with_policy(f: &Fixture, policy: NotePolicy) -> Arc<ThreadRecorder> {
    let scope = f.scope_for(THREAD);
    Arc::new(
        ThreadRecorder::with_resolver(
            "sess-test".into(),
            f.root(),
            Arc::new(move || Ok(scope.clone())),
        )
        .with_note_policy(policy),
    )
}

fn note_hint() -> Option<vam_spec::v1::ContextHint> {
    Some(vam_spec::v1::ContextHint {
        lane: vam_spec::v1::context_hint::Lane::Note as i32,
        ..Default::default()
    })
}

fn note_request(id: u64, rest: &[ChatMessage], plan: &str) -> ExchangeRequest {
    let mut with_note = rest.to_vec();
    with_note.push(cmp::recitation_note(plan));
    let mut req = main_request(id, &with_note);
    req.context_hint = note_hint();
    req
}

fn lane_records(f: &Fixture, lane: &str) -> Vec<Record> {
    f.records().into_iter().filter(|r| r.lane == lane).collect()
}

#[tokio::test]
async fn a_recitation_note_is_cut_from_the_main_lane_and_recorded_on_its_own_lane() {
    let f = fixture();
    let rec = recorder_with_policy(&f, Arc::new(|| true));
    let (u1, a1) = (msg("user", "use bravo.txt"), msg("assistant", "ok"));
    let (u2, a2) = (msg("user", "and be brief"), msg("assistant", "sure"));
    let u3 = msg("user", "now write the report");
    rec.on_request(note_request(1, std::slice::from_ref(&u1), "- [ ] a"))
        .await
        .unwrap();
    rec.on_response(response(1, "ok", vec![])).await;
    rec.on_request(note_request(
        2,
        &[u1.clone(), a1.clone(), u2.clone()],
        "- [ ] a",
    ))
    .await
    .unwrap();
    rec.on_response(response(2, "sure", vec![])).await;
    rec.on_request(note_request(
        3,
        &[u1.clone(), a1.clone(), u2.clone(), a2.clone(), u3.clone()],
        "- [x] a",
    ))
    .await
    .unwrap();
    rec.on_response(response(3, "done", vec![])).await;

    assert_eq!(rec.stats().rollbacks, 0);
    assert!(f.records().iter().all(|r| r.kind != kind::ROLLBACK));
    assert!(f.records().iter().all(|r| r.kind != kind::GAP));
    let want: Vec<WireMessage> = [&u1, &a1, &u2, &a2, &u3]
        .into_iter()
        .map(wire_of)
        .chain([wire_of(&msg("assistant", "done"))])
        .collect();
    assert_eq!(f.assembled(), want, "the main lane never holds a note");
    // Only a changed note is recorded: "- [ ] a" once, then "- [x] a".
    let notes = lane_records(&f, cmp::NOTE_LANE);
    assert_eq!(notes.len(), 2);
    for n in &notes {
        assert_eq!(n.kind, kind::MESSAGE);
        assert_eq!(n.parent_lane.as_deref(), Some(MAIN_LANE));
    }
    let text = |r: &Record| r.message().unwrap().to_wire().content;
    assert_eq!(text(&notes[0]), format!("{}- [ ] a", cmp::NOTE_LABEL));
    assert_eq!(text(&notes[1]), format!("{}- [x] a", cmp::NOTE_LABEL));
}

#[tokio::test]
async fn a_restarted_recorder_does_not_repeat_the_last_note() {
    let f = fixture();
    let policy: NotePolicy = Arc::new(|| true);
    let u1 = msg("user", "hi");
    let rec = recorder_with_policy(&f, policy.clone());
    rec.on_request(note_request(1, std::slice::from_ref(&u1), "- [ ] a"))
        .await
        .unwrap();
    rec.on_response(response(1, "ok", vec![])).await;
    let again = recorder_with_policy(&f, policy);
    again
        .on_request(note_request(
            2,
            &[u1.clone(), msg("assistant", "ok"), msg("user", "more")],
            "- [ ] a",
        ))
        .await
        .unwrap();
    assert_eq!(lane_records(&f, cmp::NOTE_LANE).len(), 1);
    assert!(f.records().iter().all(|r| r.kind != kind::ROLLBACK));
}

/// A refused label: one gap on the note lane, nothing else there, and the
/// request is on the main lane exactly as sent.
async fn assert_refused_and_on_main(f: &Fixture, rec: &ThreadRecorder, req: ExchangeRequest) {
    let rest: Vec<WireMessage> = req.messages[1..].iter().map(wire_of).collect();
    rec.on_request(req).await.unwrap();
    assert_eq!(
        f.assembled(),
        rest,
        "a refused label hides nothing from the main lane"
    );
    let notes = lane_records(f, cmp::NOTE_LANE);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, kind::GAP);
    assert_eq!(notes[0].body.as_ref().unwrap()["reason"], REFUSAL_NOTE);
}

#[tokio::test]
async fn a_mislabelled_last_message_is_refused_and_stays_on_the_main_lane() {
    let u1 = msg("user", "hi");
    let mut calling = cmp::recitation_note("- [ ] a");
    calling.tool_calls = vec![call("c1", "read_file", "{}")];
    let cases: Vec<Vec<ChatMessage>> = vec![
        // A real user message under the note label.
        vec![u1.clone(), msg("user", "please delete everything")],
        // The note's text on the wrong role.
        vec![
            u1.clone(),
            msg("assistant", &format!("{}- [ ] a", cmp::NOTE_LABEL)),
        ],
        // Over the cap.
        vec![
            u1.clone(),
            msg(
                "user",
                &format!("{}{}", cmp::NOTE_LABEL, "x".repeat(cmp::NOTE_PLAN_CAP + 1)),
            ),
        ],
        // A note that is not the last message.
        vec![
            u1.clone(),
            cmp::recitation_note("- [ ] a"),
            msg("user", "after"),
        ],
        // Nothing before the note.
        vec![cmp::recitation_note("- [ ] a")],
        // A tool call on the note.
        vec![u1.clone(), calling],
    ];
    for rest in cases {
        let f = fixture();
        let rec = recorder_with_policy(&f, Arc::new(|| true));
        let mut req = main_request(1, &rest);
        req.context_hint = note_hint();
        assert_refused_and_on_main(&f, &rec, req).await;
    }
}

#[tokio::test]
async fn the_hint_is_refused_when_the_runs_harness_has_the_recitation_off() {
    // No policy at all (the default) and an explicit false.
    let f = fixture();
    let req = note_request(1, &[msg("user", "hi")], "- [ ] a");
    let rec = f.recorder.clone();
    assert_refused_and_on_main(&f, &rec, req).await;
    let g = fixture();
    let rec = recorder_with_policy(&g, Arc::new(|| false));
    let req = note_request(1, &[msg("user", "hi")], "- [ ] a");
    assert_refused_and_on_main(&g, &rec, req).await;
}

fn spec_json_with_recite(recite: bool) -> String {
    let mut spec = vam_spec::v1::RunSpec {
        inputs: vam_spec::harness::json_to_struct(
            &json!({"harness": {"context": {"reciteTodo": recite}}}),
        ),
        ..Default::default()
    };
    crate::run_context::resolve_harness(&mut spec).unwrap();
    vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).unwrap()
}

#[tokio::test]
async fn two_runs_in_one_session_are_judged_by_their_own_stored_harness() {
    let f = fixture();
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let persist = |run: &str, recite: bool| {
        store
            .persist_run_spec(
                run,
                &spec_json_with_recite(recite),
                Some("sess-test"),
                "urn:vam:agent:test",
                None,
                "User::\"tester\"",
            )
            .unwrap();
    };
    let policy: NotePolicy = {
        let store = store.clone();
        Arc::new(move || session_recites_todo(&store, "sess-test"))
    };
    let rec = recorder_with_policy(&f, policy);
    let (u1, a1, u2) = (
        msg("user", "hi"),
        msg("assistant", "ok"),
        msg("user", "more"),
    );

    // The first run has the recitation on: its hint is accepted.
    persist("urn:vam:run:one", true);
    assert!(session_recites_todo(&store, "sess-test"));
    rec.on_request(note_request(1, std::slice::from_ref(&u1), "- [ ] a"))
        .await
        .unwrap();
    rec.on_response(response(1, "ok", vec![])).await;
    let notes = lane_records(&f, cmp::NOTE_LANE);
    assert_eq!(notes.len(), 1);
    assert_eq!(notes[0].kind, kind::MESSAGE);

    // A later run of the same session has it off: the same hint is refused.
    persist("urn:vam:run:two", false);
    assert!(!session_recites_todo(&store, "sess-test"));
    rec.on_request(note_request(
        2,
        &[u1.clone(), a1.clone(), u2.clone()],
        "- [ ] a",
    ))
    .await
    .unwrap();
    let notes = lane_records(&f, cmp::NOTE_LANE);
    assert_eq!(notes.len(), 2);
    assert_eq!(notes[1].kind, kind::GAP);
    assert_eq!(
        f.assembled().len(),
        4,
        "u1, the reply, u2 and the refused note (as an ordinary message) are on main"
    );
    assert!(!session_recites_todo(&store, "no-such-session"));
}

#[tokio::test]
async fn strip_keeps_the_reasoning_off_the_main_lane_and_other_modes_keep_none() {
    let f = fixture();
    f.recorder
        .record_task_start("Think", "mock-model", "d1", false)
        .await;
    // Round 1: a reply with reasoning under `strip`. Round 2: one with reasoning under
    // `drop` (the broker hands the recorder no text).
    let u1 = msg("user", "Think");
    f.recorder
        .on_request(request(1, "SYS", vec![u1.clone()], vec![]))
        .await
        .unwrap();
    let mut stripped = response(1, "hi", vec![]);
    stripped.reasoning = Some("secret-reasoning-marker-9".to_string());
    stripped.reasoning_bytes = 25;
    f.recorder.on_response(stripped).await;
    let a1 = msg("assistant", "hi");
    let u2 = msg("user", "again");
    f.recorder
        .on_request(request(2, "SYS", vec![u1, a1, u2], vec![]))
        .await
        .unwrap();
    let mut dropped = response(2, "yo", vec![]);
    dropped.reasoning_bytes = 40;
    f.recorder.on_response(dropped).await;

    let kept = lane_records(&f, cmp::REASONING_LANE);
    assert_eq!(kept.len(), 1, "only the strip reply keeps its text");
    assert_eq!(kept[0].parent_lane.as_deref(), Some(MAIN_LANE));
    let text = serde_json::to_string(kept[0].body.as_ref().unwrap()).unwrap();
    assert!(text.contains("secret-reasoning-marker-9") && text.contains("reasoning"));
    // The main lane (what the next request is built from) never carries it.
    let main = f.assembled();
    assert_eq!(main.len(), 4);
    assert!(main
        .iter()
        .all(|m| !m.content.contains("secret-reasoning-marker-9")));
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
    assert_eq!(f.recorder.stats(), RecorderStats::default());
}

// --- one resample of an invalid call (`adapter.resampleOnInvalid`) ---

fn recorder_with_resample_policy(f: &Fixture, policy: NotePolicy) -> Arc<ThreadRecorder> {
    let scope = f.scope_for(THREAD);
    Arc::new(
        ThreadRecorder::with_resolver(
            "sess-test".into(),
            f.root(),
            Arc::new(move || Ok(scope.clone())),
        )
        .with_resample_policy(policy),
    )
}

fn resample_request(id: u64, rest: &[ChatMessage]) -> ExchangeRequest {
    let mut req = main_request(id, rest);
    req.context_hint = Some(vam_spec::v1::ContextHint {
        lane: vam_spec::v1::context_hint::Lane::Main as i32,
        reason: "resample".into(),
        ..Default::default()
    });
    req
}

fn bad_call() -> Vec<ToolCall> {
    vec![ToolCall {
        id: "c1".into(),
        name: "read_file".into(),
        arguments: "{\"path\": ".into(),
    }]
}

#[tokio::test]
async fn an_accepted_resample_is_a_resample_rollback_and_never_a_prefix_mismatch() {
    let f = fixture();
    let rec = recorder_with_resample_policy(&f, Arc::new(|| true));
    let u1 = msg("user", "read it");
    rec.on_request(main_request(1, std::slice::from_ref(&u1)))
        .await
        .unwrap();
    rec.on_response(response(1, "", bad_call())).await;
    rec.on_request(resample_request(2, std::slice::from_ref(&u1)))
        .await
        .unwrap();
    rec.on_response(response(2, "fixed", vec![])).await;

    let rollbacks: Vec<Record> = f
        .records()
        .into_iter()
        .filter(|r| r.kind == kind::ROLLBACK)
        .collect();
    assert_eq!(rollbacks.len(), 1);
    let body = rollbacks[0].body.clone().unwrap();
    assert_eq!(body["reason"], "resample");
    assert_eq!(body["dropped"], 1);
    assert!(body["discarded_seq"].as_u64().is_some());
    assert!(f.records().iter().all(|r| r
        .body
        .as_ref()
        .is_none_or(|b| b["reason"] != "prefix_mismatch")));
    assert!(f.records().iter().all(|r| r.kind != kind::GAP));
    assert_eq!(rec.stats().rollbacks, 0);
    // The discarded reply stays in the log; the assembled context holds the resample.
    let want = vec![wire_of(&u1), wire_of(&msg("assistant", "fixed"))];
    assert_eq!(f.assembled(), want);
    assert!(f.records().iter().any(|r| r.kind == kind::MESSAGE
        && r.message()
            .is_some_and(|m| !m.to_wire().tool_calls.is_empty())));
}

#[tokio::test]
async fn a_resample_label_is_refused_when_the_switch_is_off() {
    let f = fixture();
    let rec = recorder_with_resample_policy(&f, Arc::new(|| false));
    let u1 = msg("user", "read it");
    rec.on_request(main_request(1, std::slice::from_ref(&u1)))
        .await
        .unwrap();
    rec.on_response(response(1, "", bad_call())).await;
    rec.on_request(resample_request(2, std::slice::from_ref(&u1)))
        .await
        .unwrap();
    let gaps = lane_records(&f, cmp::RESAMPLE_LANE);
    assert_eq!(gaps.len(), 1);
    assert_eq!(gaps[0].kind, kind::GAP);
    assert_eq!(gaps[0].body.clone().unwrap()["reason"], "resample_refused");
    // Recorded as if unlabelled: today's prefix_mismatch rollback.
    let body = f
        .records()
        .into_iter()
        .find(|r| r.kind == kind::ROLLBACK)
        .unwrap()
        .body
        .unwrap();
    assert_eq!(body["reason"], "prefix_mismatch");
    assert_eq!(rec.stats().rollbacks, 1);
}

#[tokio::test]
async fn a_resample_label_on_a_request_of_the_wrong_shape_is_refused() {
    let f = fixture();
    let rec = recorder_with_resample_policy(&f, Arc::new(|| true));
    let (u1, u2) = (msg("user", "read it"), msg("user", "something else"));
    rec.on_request(main_request(1, std::slice::from_ref(&u1)))
        .await
        .unwrap();
    rec.on_response(response(1, "ok", vec![])).await;
    // A request that rewrites the history is not "the same request again".
    rec.on_request(resample_request(2, &[u2])).await.unwrap();
    assert_eq!(lane_records(&f, cmp::RESAMPLE_LANE).len(), 1);
    let body = f
        .records()
        .into_iter()
        .find(|r| r.kind == kind::ROLLBACK)
        .unwrap()
        .body
        .unwrap();
    assert_eq!(body["reason"], "prefix_mismatch");
}

// ---------------------------------------------------------------------------
// The effect journal and a stop (AR-4.4)

/// A journaled outside call of the test run, `intended`, with an open execute path.
fn open_effect_call<'a>(
    store: &'a SqliteWorkflowStore,
    call_id: &str,
    tool: &str,
) -> crate::effect_path::EffectCall<'a> {
    let secret = crate::signing::NodeKeys::ephemeral().effect_key_secret();
    crate::effect_path::record_intent(&crate::effect_path::IntentFacts {
        store,
        secret: Some(secret.clone()),
        session_id: "sess-test",
        run_id: RUN_ID,
        tool,
        adapter_id: "hosted:echo:echo.create",
        side_effect: 3,
        call_id,
        args_digest: "abcdef0123456789",
        approval_id: None,
    })
    .unwrap();
    match crate::effect_path::open_execute(
        store,
        Some(&secret),
        RUN_ID,
        call_id,
        call_id,
        tool,
        "abcdef0123456789",
        1,
    ) {
        Ok(crate::effect_path::ExecuteOpen::Proceed(call)) => call,
        _ => panic!("an intended row proceeds"),
    }
}

fn answered(text: &str) -> vam_spec::v1::ExecuteHostToolResponse {
    vam_spec::v1::ExecuteHostToolResponse {
        text_result: text.into(),
        ..Default::default()
    }
}

#[tokio::test]
async fn a_stop_waits_for_a_held_host_call_and_the_conversation_keeps_its_result() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let tools = vec![tool_spec("read_file", "Read")];
    let c = call("c1", "echo_create_ticket", "{\"title\":\"Q4\"}");
    rec.on_request(request(1, "SYS", vec![msg("user", "go")], tools))
        .await;
    rec.on_response(response(1, "", vec![c])).await;

    // The call left the host and the broker is holding it open.
    let effect_store = store.clone();
    let held = tokio::spawn(async move {
        let call = open_effect_call(&effect_store, "c1", "echo_create_ticket");
        call.mark_sent().unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        call.finish(&answered("T-1"));
    });
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    rec.mark_user_stop();
    rec.record_stop().await.expect("head");
    held.await.unwrap();

    // Not "Not run": the create happened and the conversation says so.
    let last = f.assembled().pop().unwrap();
    assert_eq!(last.tool_call_id, "c1");
    assert_eq!(last.content, "T-1");
}

#[tokio::test]
async fn a_sent_call_nobody_is_waiting_for_is_unknown_in_the_conversation() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    rec.on_request(request(1, "SYS", vec![msg("user", "go")], vec![]))
        .await;
    rec.on_response(response(
        1,
        "",
        vec![call("c1", "echo_create_ticket", "{\"title\":\"Q4\"}")],
    ))
    .await;
    // vamd restarted under a call it had sent: the row is `sent`, nothing is in flight.
    let secret = crate::signing::NodeKeys::ephemeral().effect_key_secret();
    crate::effect_path::record_intent(&crate::effect_path::IntentFacts {
        store: &store,
        secret: Some(secret),
        session_id: "sess-test",
        run_id: RUN_ID,
        tool: "echo_create_ticket",
        adapter_id: "hosted:echo:echo.create",
        side_effect: 3,
        call_id: "c1",
        args_digest: "abcdef0123456789",
        approval_id: None,
    })
    .unwrap();
    store.effect_mark_sent(RUN_ID, "c1", 1).unwrap();
    rec.record_interrupted("interrupted", "vamd_restarted")
        .await
        .expect("head");
    let last = f.assembled().pop().unwrap();
    assert!(
        last.content.starts_with("Outcome unknown"),
        "{}",
        last.content
    );
    assert_eq!(
        store.effect_get(RUN_ID, "c1").unwrap().unwrap().state,
        crate::effect_store::EffectState::Unknown
    );
}

#[tokio::test]
async fn an_interrupted_task_keeps_its_decided_round_open_and_writes_no_end() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    rec.on_request(request(1, "SYS", vec![msg("user", "go")], vec![]))
        .await;
    rec.on_response(response(
        1,
        "",
        vec![
            call("c1", "write_file", "{\"path\":\"a\"}"),
            call("c2", "echo_create_ticket", "{\"title\":\"Q4\"}"),
        ],
    ))
    .await;
    // The create was sent and vamd lost the answer.
    let secret = crate::signing::NodeKeys::ephemeral().effect_key_secret();
    crate::effect_path::record_intent(&crate::effect_path::IntentFacts {
        store: &store,
        secret: Some(secret),
        session_id: "sess-test",
        run_id: RUN_ID,
        tool: "echo_create_ticket",
        adapter_id: "hosted:echo:echo.create",
        side_effect: 3,
        call_id: "c2",
        args_digest: "abcdef0123456789",
        approval_id: None,
    })
    .unwrap();
    store.effect_mark_sent(RUN_ID, "c2", 1).unwrap();

    rec.record_interrupted_for_resume().await;

    // The conversation ends in the decided assistant message: nothing is answered "Not run",
    // and the task has not ended.
    let last = f.assembled().pop().unwrap();
    assert_eq!(last.role, "assistant");
    assert_eq!(last.tool_calls.len(), 2);
    let kinds: Vec<String> = f.records().iter().map(|r| r.kind.clone()).collect();
    assert!(
        !kinds.iter().any(|k| k == "task_end"),
        "no task_end: {kinds:?}"
    );
    assert!(!kinds.iter().any(|k| k == "rollback"), "{kinds:?}");
    // The lost answer is `unknown` in the journal, for the next guest to be told.
    assert_eq!(
        store.effect_get(RUN_ID, "c2").unwrap().unwrap().state,
        crate::effect_store::EffectState::Unknown
    );
    // The recorder of the next session continues the same log.
    let next = f.recorder_for(THREAD);
    next.record_task_start("go", "mock-model", "d", true).await;
    assert_eq!(f.assembled().pop().unwrap().role, "assistant");
}

#[tokio::test]
async fn a_rollback_never_hides_a_journaled_effect() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let tools = vec![tool_spec("read_file", "Read")];
    let u = msg("user", "write, then create a ticket");
    let c1 = call("c1", "write_file", "{\"path\":\"a\"}");
    let a1 = assistant_calls("", vec![c1.clone()]);
    let t1 = tool_msg("c1", "ok a");
    let c2 = call("c2", "write_file", "{\"path\":\"b\"}");
    let a2 = assistant_calls("", vec![c2.clone()]);
    let t2 = tool_msg("c2", "ok b");
    let c3 = call("c3", "echo_create_ticket", "{\"title\":\"Q4\"}");

    rec.on_request(request(1, "SYS", vec![u.clone()], tools.clone()))
        .await;
    rec.on_response(response(1, "", vec![c1])).await;
    let head = rec.cached_head().unwrap();
    store
        .record_turn_commit_pending(
            RUN_ID,
            "sess-test",
            1,
            head.seq,
            &head.digest,
            &spec_tail(std::slice::from_ref(&t1)),
        )
        .unwrap();
    store.set_turn_commit_manifest(RUN_ID, 1, "m1").unwrap();
    rec.on_request(request(
        2,
        "SYS",
        vec![u.clone(), a1.clone(), t1.clone()],
        tools.clone(),
    ))
    .await;
    rec.on_response(response(2, "", vec![c2])).await;
    rec.on_request(request(
        3,
        "SYS",
        vec![u.clone(), a1.clone(), t1.clone(), a2, t2],
        tools,
    ))
    .await;
    rec.on_response(response(3, "", vec![c3.clone()])).await;

    // The create finished before the stop.
    let effect_call = open_effect_call(&store, "c3", "echo_create_ticket");
    effect_call.mark_sent().unwrap();
    effect_call.finish(&answered("T-1"));
    drop(effect_call);

    rec.record_stop().await.expect("head");
    assert!(f.records().iter().any(|r| r.kind == kind::ROLLBACK));
    // The files of round 2 are gone with the conversation about them, but the create is not.
    assert_eq!(
        f.assembled(),
        vec![
            wire_of(&u),
            wire_of(&a1),
            wire_of(&t1),
            wire_of(&assistant_calls("", vec![c3])),
            wire_of(&tool_msg("c3", "T-1")),
        ]
    );
    assert_eq!(verify_thread(&f.tree(), THREAD).unwrap(), Integrity::Ok);
}

#[tokio::test]
async fn the_stored_result_is_cleared_once_the_log_holds_the_tool_message() {
    let f = fixture();
    let store = commit_store();
    let rec = recorder_with_commits(&f, &store);
    rec.record_task_start("go", "mock-model", "d", false).await;
    let tools = vec![tool_spec("read_file", "Read")];
    let u = msg("user", "go");
    let c1 = call("c1", "echo_create_ticket", "{\"title\":\"Q4\"}");
    let a1 = assistant_calls("", vec![c1.clone()]);
    rec.on_request(request(1, "SYS", vec![u.clone()], tools.clone()))
        .await;
    rec.on_response(response(1, "", vec![c1])).await;
    let effect_call = open_effect_call(&store, "c1", "echo_create_ticket");
    effect_call.mark_sent().unwrap();
    effect_call.finish(&answered("T-1"));
    drop(effect_call);
    assert!(store
        .effect_get(RUN_ID, "c1")
        .unwrap()
        .unwrap()
        .result_bytes
        .is_some());
    rec.on_request(request(2, "SYS", vec![u, a1, tool_msg("c1", "T-1")], tools))
        .await;
    let row = store.effect_get(RUN_ID, "c1").unwrap().unwrap();
    assert!(
        row.result_bytes.is_none(),
        "bytes kept only until the log has them"
    );
    assert!(row.result_digest.is_some());
}

#[tokio::test]
async fn a_second_suspend_on_the_same_call_adds_a_record_and_no_duplicate_results() {
    let f = fixture();
    let rec = &f.recorder;
    rec.record_task_start("go", "m", "d", false).await;
    three_calls(rec).await;
    rec.record_suspend(&tail("b", &[("a", "contents")]), &"1".repeat(64))
        .await
        .unwrap();
    // The resumed task re-asked for `b` and parks again; even a tail that repeats `a` is
    // not logged twice.
    rec.record_suspend(&tail("b", &[("a", "contents")]), &"2".repeat(64))
        .await
        .unwrap();
    rec.record_suspend(&tail("b", &[]), &"3".repeat(64))
        .await
        .unwrap();
    let records = f.records();
    assert_eq!(records.iter().filter(|r| r.src == "guest").count(), 1);
    let suspends: Vec<&Record> = records.iter().filter(|r| r.kind == kind::SUSPEND).collect();
    assert_eq!(suspends.len(), 3);
    let body = suspends[2].body.as_ref().unwrap();
    assert_eq!(body["pending"]["tool_call_id"], "b");
    assert_eq!(body["unanswered"], serde_json::json!(["c"]));
}

#[test]
fn new_tool_results_name_the_tool_from_the_call_and_skip_seen_messages() {
    let rest = vec![
        msg("user", "go"),
        assistant_calls(
            "",
            vec![call("c1", "mail_list", "{}"), call("c2", "other", "{}")],
        ),
        tool_msg("c1", "{\"a\":1}"),
        tool_msg("c2", "x"),
    ];
    let all = new_tool_results(&rest, 0);
    assert_eq!(
        all[0],
        ("mail_list".into(), "c1".into(), "{\"a\":1}".into())
    );
    assert_eq!(all.len(), 2);
    let later = new_tool_results(&rest, 3);
    assert_eq!(
        later,
        vec![("other".to_string(), "c2".to_string(), "x".to_string())]
    );
}
