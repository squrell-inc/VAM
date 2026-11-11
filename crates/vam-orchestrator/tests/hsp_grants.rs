//! E5 grant issuance (WO-1.5): what `invoke_hosted_tool` does when an approved grant tool is
//! called with a `run` argument. An in-process scripted broker stands in for the schedule
//! broker and keeps every `InvokeRequest` it is handed.

#![allow(clippy::result_large_err)]

use std::sync::{Arc, Mutex};
use std::time::Duration;

use prost_types::Struct;
use tempfile::tempdir;
use vam_inference::SessionBudget;
use vam_orchestrator::credentials::Transport;
use vam_orchestrator::host_tools::HostToolRegistry;
use vam_orchestrator::hsp::{HspClient, HspEndpoint};
use vam_orchestrator::hsp_echo::{spawn_scripted_invoke_unix, ScriptedInvokeBroker};
use vam_orchestrator::hsp_registry::HspRegistry;
use vam_orchestrator::hsp_store::seed_hsp_skills;
use vam_orchestrator::keys::KeyKind;
use vam_orchestrator::secrets::SecretsVault;
use vam_orchestrator::security::SessionSecurityContext;
use vam_orchestrator::session::{builtin_tool_catalog, merge_tool_catalog};
use vam_orchestrator::workflow::SqliteWorkflowStore;
use vam_security::PolicyMode;
use vam_spec::v1::{
    CapabilityManifest, ErrorStatus, ExecuteHostToolRequest, ExecuteHostToolResponse,
    ExecutionLease, InvokeRequest, InvokeResponse, RegisterRequest, ResourceId,
    RunRequestPermission, ToolExport, ToolIntentRequest, ToolResult,
};
use vam_spec::validation::host_tool_digest;

const RUN: &str = "urn:vam:run:local.vam.dev/acme/run-hsp-grants";
const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/grants-main";
const OTHER_AGENT: &str = "urn:vam:agent:local.vam.dev/acme/grants-other";
const OWNER: &str = "local";
const BROKER: &str = "hsp";

fn agent(urn: &str) -> vam_spec::v1::AgentSpec {
    vam_spec::v1::AgentSpec {
        resource_id: Some(ResourceId {
            urn: urn.to_string(),
        }),
        display_name: "grants".to_string(),
        description: "grants".to_string(),
        role: "assistant".to_string(),
        instruction_source: Some(
            vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".to_string()),
        ),
        ..Default::default()
    }
}

fn manifest(max_active: i32) -> CapabilityManifest {
    CapabilityManifest {
        broker_id: BROKER.to_string(),
        trust_tier: 2,
        tools: vec![ToolExport {
            tool_name: "echo.ping".to_string(),
            side_effect: 0,
            ..Default::default()
        }],
        run_request_permission: Some(RunRequestPermission {
            grant_tools: vec!["echo.ping".to_string()],
            loop_guarded_tools: vec!["echo.ping".to_string()],
            surface: "echo".to_string(),
            max_active_grants_per_user: max_active,
            ..Default::default()
        }),
        ..Default::default()
    }
}

fn json_to_value(value: &serde_json::Value) -> prost_types::Value {
    use prost_types::value::Kind;
    let kind = match value {
        serde_json::Value::Null => Kind::NullValue(0),
        serde_json::Value::Bool(b) => Kind::BoolValue(*b),
        serde_json::Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(s) => Kind::StringValue(s.clone()),
        serde_json::Value::Array(items) => Kind::ListValue(prost_types::ListValue {
            values: items.iter().map(json_to_value).collect(),
        }),
        serde_json::Value::Object(map) => Kind::StructValue(Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        }),
    };
    prost_types::Value { kind: Some(kind) }
}

fn json_struct(value: serde_json::Value) -> Struct {
    match json_to_value(&value).kind {
        Some(prost_types::value::Kind::StructValue(s)) => s,
        _ => Struct::default(),
    }
}

fn lease() -> ExecutionLease {
    ExecutionLease {
        run_id: Some(ResourceId {
            urn: RUN.to_string(),
        }),
        lifecycle_generation: 1,
        ..Default::default()
    }
}

/// What the scripted broker does with the next call.
#[derive(Clone)]
enum Mode {
    Ok,
    /// Answer with the token in the output and in the error text.
    EchoToken,
    TransportError,
    ToolError,
}

struct Fx {
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<SecretsVault>,
    ctx: SessionSecurityContext,
    registry: HostToolRegistry,
    skills_dir: std::path::PathBuf,
    seen: Arc<Mutex<Vec<InvokeRequest>>>,
    mode: Arc<Mutex<Mode>>,
    db: std::path::PathBuf,
    calling_space: String,
    calling_thread: String,
    _dir: tempfile::TempDir,
}

async fn fixture(max_active: i32) -> Fx {
    let dir = tempdir().unwrap();
    let db = dir.path().join("vam.db");
    let store = Arc::new(SqliteWorkflowStore::open(&db).unwrap());
    store.initialize_catalog_schema().unwrap();
    let approved = manifest(max_active);
    store.upsert_hsp_broker_approval(&approved).unwrap();
    let vault = SecretsVault::for_tests(store.clone());
    let seen: Arc<Mutex<Vec<InvokeRequest>>> = Arc::default();
    let mode = Arc::new(Mutex::new(Mode::Ok));
    let (seen_in, mode_in) = (seen.clone(), mode.clone());
    let sock = dir.path().join("scripted.sock");
    let (_path, _handle) = spawn_scripted_invoke_unix(
        sock.clone(),
        ScriptedInvokeBroker::new(move |req| {
            seen_in.lock().unwrap().push(req.clone());
            let token = req
                .run_grant
                .as_ref()
                .map(|g| g.token.clone())
                .unwrap_or_default();
            match mode_in.lock().unwrap().clone() {
                Mode::Ok => Ok(InvokeResponse {
                    result: Some(ToolResult {
                        output: Some(json_struct(serde_json::json!({ "ok": true }))),
                        error: None,
                    }),
                    ..Default::default()
                }),
                Mode::EchoToken => Ok(InvokeResponse {
                    result: Some(ToolResult {
                        output: Some(json_struct(serde_json::json!({ "echo": token }))),
                        error: None,
                    }),
                    ..Default::default()
                }),
                Mode::TransportError => Err(tonic::Status::internal(format!("boom {token}"))),
                Mode::ToolError => Ok(InvokeResponse {
                    result: Some(ToolResult {
                        output: None,
                        error: Some(ErrorStatus {
                            message: format!("refused {token}"),
                            ..Default::default()
                        }),
                    }),
                    ..Default::default()
                }),
            }
        }),
    )
    .unwrap();
    let mut client =
        HspClient::connect_with_timeout(HspEndpoint::Unix(sock), Duration::from_secs(2))
            .await
            .unwrap();
    HspRegistry::new(store.as_ref())
        .register_admitted(
            &mut client,
            RegisterRequest {
                broker_id: BROKER.to_string(),
                broker_version: "t".to_string(),
                manifest: Some(approved),
            },
        )
        .await
        .unwrap();

    let _ = store.create_user(OWNER, OWNER);
    let calling_space = store
        .create_space(OWNER, Some(AGENT), "main", "run", None)
        .unwrap()
        .space_id;
    let calling_thread = store
        .create_thread(OWNER, &calling_space, "chat", "run")
        .unwrap()
        .thread_id;
    let session_id = "sess-hsp-grants";
    store
        .create_session_with_owner(
            session_id,
            None,
            "vm-test",
            &calling_space,
            "storage-default",
            OWNER,
        )
        .unwrap();
    for urn in [AGENT, OTHER_AGENT] {
        store
            .publish_agent_draft_with_owner(&agent(urn), OWNER)
            .unwrap();
        store
            .release_agent(
                urn,
                &vam_spec::v1::Version {
                    major: 1,
                    ..Default::default()
                },
            )
            .unwrap();
    }
    store
        .create_run(&ResourceId {
            urn: RUN.to_string(),
        })
        .unwrap();
    let skills_dir = dir.path().join("skills");
    seed_hsp_skills(&skills_dir, store.as_ref(), true).unwrap();
    let skills: Vec<_> = vam_orchestrator::session::parse_skills_catalog(&skills_dir)
        .into_iter()
        .filter(|entry| !entry.entrypoint.starts_with("hosted:"))
        .collect();
    let hosted = store.list_hsp_catalog_entries(true).unwrap();
    let catalog = merge_tool_catalog(builtin_tool_catalog(), skills, hosted).unwrap();
    store.record_tool_catalog(session_id, &catalog).unwrap();
    let ctx = SessionSecurityContext::new(
        session_id.to_string(),
        AGENT.to_string(),
        r#"permit (principal, action == Action::"ExecuteTool", resource == Tool::"echo.ping");"#,
        PolicyMode::Enforce,
        1,
        ed25519_dalek::SigningKey::from_bytes(&[42u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .unwrap()
    .with_effect_key_secret(vam_orchestrator::signing::NodeKeys::ephemeral().effect_key_secret());
    let fx = Fx {
        store,
        vault,
        ctx,
        registry: HostToolRegistry::with_test_adapters(),
        skills_dir,
        seen,
        mode,
        db,
        calling_space,
        calling_thread,
        _dir: dir,
    };
    fx.set_run(&format!(
        r#"{{"spaceId":"{}","threadId":"{}"}}"#,
        fx.calling_space, fx.calling_thread
    ));
    fx
}

impl Fx {
    /// Sets the calling run's stored spec (its Space and thread ids).
    fn set_run(&self, spec_json: &str) {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        conn.execute(
            "UPDATE runs SET spec_json = ?1 WHERE run_id = ?2",
            rusqlite::params![spec_json, RUN],
        )
        .unwrap();
    }

    /// Marks the calling run as started by a broker.
    fn set_source(&self, source: &str) {
        let conn = rusqlite::Connection::open(&self.db).unwrap();
        conn.execute(
            "UPDATE runs SET source = ?1 WHERE run_id = ?2",
            rusqlite::params![source, RUN],
        )
        .unwrap();
    }

    fn grants(&self) -> Vec<vam_orchestrator::keys::ApiKeyRecord> {
        self.store
            .list_keys_of_kind(OWNER, false, KeyKind::Grants)
            .unwrap()
    }

    fn live_grants(&self) -> usize {
        self.grants()
            .iter()
            .filter(|g| g.revoked_at.is_none())
            .count()
    }

    fn spaces(&self) -> usize {
        self.store
            .list_spaces(OWNER, None, None, 100)
            .unwrap()
            .len()
    }

    fn threads(&self, space: &str) -> usize {
        self.store.list_thread_rows(OWNER, space).unwrap().len()
    }

    fn calls(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    async fn call(&self, args: serde_json::Value) -> ExecuteHostToolResponse {
        let args = json_struct(args);
        let intent = self.ctx.handle_tool_intent_request(
            self.store.as_ref(),
            ToolIntentRequest {
                lease: Some(lease()),
                tool_name: "echo.ping".to_string(),
                arguments: Some(args.clone()),
                arguments_digest: None,
                script_digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: host_tool_digest("hosted:hsp:echo.ping"),
                }),
                call_id: String::new(),
            },
        );
        assert_eq!(
            intent.disposition,
            vam_spec::v1::tool_intent_response::Disposition::Authorized as i32,
            "{intent:?}"
        );
        self.ctx
            .handle_execute_host_tool(
                self.store.as_ref(),
                Some(self.vault.as_ref()),
                &self.registry,
                &self.skills_dir,
                ExecuteHostToolRequest {
                    lease: Some(lease()),
                    tool_name: "echo.ping".to_string(),
                    arguments: Some(args),
                    capability_token: intent.capability_token,
                    call_id: String::new(),
                },
            )
            .await
    }

    /// A call whose `run` is `run`.
    async fn arm(&self, run: serde_json::Value) -> ExecuteHostToolResponse {
        self.call(serde_json::json!({ "run": run })).await
    }
}

/// A failed call answers a `tool_error: ...` text (the guest sees it as the tool's result).
fn failed(r: &ExecuteHostToolResponse) -> bool {
    r.error.is_some() || r.text_result.starts_with("tool_error: ")
}

fn error_of(r: &ExecuteHostToolResponse) -> String {
    assert!(failed(r), "expected an error, got {r:?}");
    match r.error.as_ref() {
        Some(e) => e.message.clone(),
        None => r.text_result.trim_start_matches("tool_error: ").to_string(),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn all_outbox(fx: &Fx) -> String {
    fx.store
        .read_outbox_for_run(RUN, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.payload)
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn an_approved_call_with_run_issues_a_bound_grant() {
    let fx = fixture(0).await;
    let r = fx
        .arm(serde_json::json!({
            "message": "write the report",
            "workspace": { "mode": "SAME" },
            "thread": { "mode": "NEW", "name": "daily report" },
            "onBusy": "SKIP"
        }))
        .await;
    assert!(!failed(&r), "{r:?}");
    assert_eq!(fx.calls(), 1);
    let sent = fx.seen.lock().unwrap()[0].clone();
    let grant = sent.run_grant.expect("the broker is handed the grant");
    assert!(grant.token.starts_with("vam_rg_"), "{}", grant.token);
    let target = grant.target.clone().unwrap();
    assert_eq!(target.agent_urn, AGENT);
    assert_eq!(target.space_id, fx.calling_space);
    assert_eq!(target.space_name, "main");
    assert!(target.thread_id.starts_with("th_"));
    assert_ne!(target.thread_id, fx.calling_thread);
    assert_eq!(target.thread_name, "daily report");
    assert_eq!(target.on_busy, vam_spec::v1::OnBusy::Skip as i32);

    // The row: holder, target, constraints (keys the grant list reads), expiry, provenance.
    let rows = fx.grants();
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    assert_eq!(row.key_id, grant.grant_id);
    assert_eq!(row.holder(), "broker:hsp");
    assert_eq!(row.target_kind.as_deref(), Some("space"));
    assert_eq!(row.target_id.as_deref(), Some(fx.calling_space.as_str()));
    assert_eq!(row.issued_via.as_deref(), Some("invoke"));
    assert_eq!(row.source_run_id.as_deref(), Some(RUN));
    assert_eq!(row.tool_name.as_deref(), Some("echo.ping"));
    assert_eq!(row.scopes, vec!["runs:start".to_string()]);
    let ttl = row.expires_at.unwrap() - now();
    assert!((89 * 86_400..=90 * 86_400).contains(&ttl), "{ttl}");
    let c: serde_json::Value =
        serde_json::from_str(row.constraints_json.as_ref().unwrap()).unwrap();
    assert_eq!(c["thread_id"], target.thread_id);
    assert_eq!(c["message"], "write the report");
    assert_eq!(c["space_id"], fx.calling_space);
    assert_eq!(c["agent_urn"], AGENT);
    assert_eq!(c["on_busy"], "SKIP");
    assert!(target.request_digest.starts_with("sha256:"));
    assert_eq!(target.request_digest.len(), "sha256:".len() + 64);

    // The token checks on the broker's gateway and nowhere else.
    let secret = grant.token.split_once('.').unwrap().1;
    assert!(fx
        .store
        .check_credential(secret, &Transport::Broker(BROKER.to_string()))
        .is_ok());
    assert!(fx.store.check_credential(secret, &Transport::Http).is_err());

    // The new thread exists in the Space and is stamped with the grant.
    let thread = fx
        .store
        .get_thread(OWNER, &fx.calling_space, &target.thread_id)
        .unwrap();
    assert_eq!(thread.created_via, "grant");
    assert_eq!(
        thread.created_by_grant_id.as_deref(),
        Some(grant.grant_id.as_str())
    );
}

#[tokio::test]
async fn no_grant_without_run_or_with_a_null_run() {
    let fx = fixture(0).await;
    let r = fx.call(serde_json::json!({ "x": 1 })).await;
    assert!(!failed(&r), "{r:?}");
    let r = fx.call(serde_json::json!({ "run": null })).await;
    assert!(!failed(&r), "{r:?}");
    assert_eq!(fx.calls(), 2);
    assert!(fx
        .seen
        .lock()
        .unwrap()
        .iter()
        .all(|req| req.run_grant.is_none()));
    assert!(fx.grants().is_empty());
}

#[tokio::test]
async fn a_denied_call_issues_nothing() {
    let fx = fixture(0).await;
    let args = json_struct(serde_json::json!({ "run": { "message": "m" } }));
    // No capability token from the permission decision: the host refuses before the broker.
    let r = fx
        .ctx
        .handle_execute_host_tool(
            fx.store.as_ref(),
            Some(fx.vault.as_ref()),
            &fx.registry,
            &fx.skills_dir,
            ExecuteHostToolRequest {
                lease: Some(lease()),
                tool_name: "echo.ping".to_string(),
                arguments: Some(args),
                capability_token: None,
                call_id: String::new(),
            },
        )
        .await;
    assert!(failed(&r), "{r:?}");
    assert_eq!(fx.calls(), 0);
    assert!(fx.grants().is_empty());
}

#[tokio::test]
async fn the_loop_guard_reads_runs_source_and_issues_nothing() {
    let fx = fixture(0).await;
    fx.set_source("broker:hsp");
    let spaces = fx.spaces();
    let r = fx
        .arm(serde_json::json!({ "message": "again", "thread": { "mode": "NEW" } }))
        .await;
    let msg = error_of(&r);
    assert!(
        msg.contains("this tool cannot be used in a run a trigger started"),
        "{msg}"
    );
    // Guarded even without a `run` argument.
    let r = fx.call(serde_json::json!({ "x": 1 })).await;
    assert!(error_of(&r).contains("run a trigger started"));
    assert_eq!(fx.calls(), 0);
    assert!(fx.grants().is_empty());
    assert_eq!(fx.spaces(), spaces);
    // A person's run (NULL source) is unaffected.
    let conn = rusqlite::Connection::open(&fx.db).unwrap();
    conn.execute("UPDATE runs SET source = NULL WHERE run_id = ?1", [RUN])
        .unwrap();
    let r = fx.arm(serde_json::json!({ "message": "ok" })).await;
    assert!(!failed(&r), "{r:?}");
    assert_eq!(fx.live_grants(), 1);
}

#[tokio::test]
async fn a_foreign_or_missing_space_or_thread_id_is_refused_alike() {
    let fx = fixture(0).await;
    let _ = fx.store.create_user("bob", "bob");
    let bobs = fx
        .store
        .create_space("bob", Some(AGENT), "bobs", "run", None)
        .unwrap();
    let bobs_thread = fx
        .store
        .create_thread("bob", &bobs.space_id, "t", "run")
        .unwrap();
    let missing = "sp_aaaaaaaaaaaaaaaaaaaaaaaaaa";
    let spaces = fx.spaces();
    let threads = fx.threads(&fx.calling_space);

    let foreign = error_of(
        &fx.arm(serde_json::json!({ "message": "m", "workspace": { "mode": "ID", "id": bobs.space_id } }))
            .await,
    );
    let gone = error_of(
        &fx.arm(
            serde_json::json!({ "message": "m", "workspace": { "mode": "ID", "id": missing } }),
        )
        .await,
    );
    assert!(foreign.starts_with("space_not_found:"), "{foreign}");
    assert_eq!(foreign, gone);
    assert!(!foreign.contains(&bobs.space_id));

    // A foreign thread id inside my own Space, and bob's thread id by name.
    for id in [
        bobs_thread.thread_id.as_str(),
        "th_aaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let e = error_of(
            &fx.arm(serde_json::json!({ "message": "m", "workspace": { "mode": "SAME" }, "thread": { "mode": "ID", "id": id } }))
                .await,
        );
        assert!(e.starts_with("thread_not_found:"), "{e}");
    }
    assert_eq!(fx.calls(), 0);
    assert!(fx.grants().is_empty());
    assert_eq!(fx.spaces(), spaces);
    assert_eq!(fx.threads(&fx.calling_space), threads);

    // The owner's own thread id works.
    let r = fx
        .arm(
            serde_json::json!({ "message": "m", "workspace": { "mode": "SAME" },
            "thread": { "mode": "ID", "id": fx.calling_thread } }),
        )
        .await;
    assert!(!failed(&r), "{r:?}");
    let g = fx.seen.lock().unwrap()[0].run_grant.clone().unwrap();
    assert_eq!(g.target.unwrap().thread_id, fx.calling_thread);
}

#[tokio::test]
async fn another_agent_needs_a_new_space_or_one_bound_to_it() {
    let fx = fixture(0).await;
    let spaces = fx.spaces();
    // Not an agent the owner may run.
    let e = error_of(
        &fx.arm(serde_json::json!({ "agentUrn": "urn:vam:agent:local.vam.dev/acme/nope", "message": "m", "workspace": { "mode": "NEW" } }))
            .await,
    );
    assert!(e.starts_with("agent_not_found:"), "{e}");
    // SAME Space belongs to the calling agent.
    let e = error_of(
        &fx.arm(serde_json::json!({ "agentUrn": OTHER_AGENT, "message": "m", "workspace": { "mode": "SAME" } }))
            .await,
    );
    assert!(e.starts_with("space_agent_mismatch:"), "{e}");
    // An ID Space bound to a different agent.
    let e = error_of(
        &fx.arm(serde_json::json!({ "agentUrn": OTHER_AGENT, "message": "m",
            "workspace": { "mode": "ID", "id": fx.calling_space } }))
            .await,
    );
    assert!(e.starts_with("space_agent_mismatch:"), "{e}");
    assert_eq!((fx.calls(), fx.grants().len(), fx.spaces()), (0, 0, spaces));

    // A NEW Space is bound to the other agent and labelled as given.
    let r = fx
        .arm(serde_json::json!({ "agentUrn": OTHER_AGENT, "message": "m",
            "workspace": { "mode": "NEW", "name": "Other agent's space" } }))
        .await;
    assert!(!failed(&r), "{r:?}");
    let t = fx.seen.lock().unwrap()[0]
        .run_grant
        .clone()
        .unwrap()
        .target
        .unwrap();
    assert_eq!(t.agent_urn, OTHER_AGENT);
    assert_eq!(t.space_name, "Other agent's space");
    let row = fx.store.get_space(OWNER, &t.space_id).unwrap();
    assert_eq!(row.agent_urn.as_deref(), Some(OTHER_AGENT));
    assert_eq!(row.created_via, "grant");
    assert_eq!(
        row.created_by_grant_id.as_deref(),
        Some(fx.grants()[0].key_id.as_str())
    );
    assert_eq!(fx.spaces(), spaces + 1);

    // Now an ID Space bound to the other agent works.
    let r = fx
        .arm(serde_json::json!({ "agentUrn": OTHER_AGENT, "message": "m",
            "workspace": { "mode": "ID", "id": t.space_id } }))
        .await;
    assert!(!failed(&r), "{r:?}");
    assert_eq!(fx.live_grants(), 2);
}

#[tokio::test]
async fn expiry_defaults_to_90_days_is_capped_at_365_and_never_in_the_past() {
    let fx = fixture(0).await;
    let far = now() + 1000 * 86_400;
    let r = fx
        .arm(serde_json::json!({ "message": "m", "expiresAt": chrono_rfc3339(far) }))
        .await;
    assert!(!failed(&r), "{r:?}");
    // Rows list by creation second, then id: look them up by their expiry, not by position.
    let ttl = fx.grants()[0].expires_at.unwrap() - now();
    assert!(
        (364 * 86_400..=365 * 86_400).contains(&ttl),
        "capped at 365 d, got {ttl}"
    );
    let soon = now() + 3600;
    let r = fx
        .arm(serde_json::json!({ "message": "m", "expiresAt": chrono_rfc3339(soon) }))
        .await;
    assert!(!failed(&r), "{r:?}");
    assert!(fx.grants().iter().any(|g| g.expires_at == Some(soon)));
    let before = fx.grants().len();
    let e = error_of(
        &fx.arm(serde_json::json!({ "message": "m", "expiresAt": chrono_rfc3339(now() - 10) }))
            .await,
    );
    assert!(e.starts_with("expiry_in_past:"), "{e}");
    assert_eq!(fx.grants().len(), before);
}

/// RFC 3339 UTC for a unix second.
fn chrono_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Civil from days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[tokio::test]
async fn the_live_grant_cap_refuses_and_leaves_no_rows() {
    let fx = fixture(2).await;
    for _ in 0..2 {
        let r = fx.arm(serde_json::json!({ "message": "m" })).await;
        assert!(!failed(&r), "{r:?}");
    }
    let spaces = fx.spaces();
    let e = error_of(
        &fx.arm(serde_json::json!({ "message": "m", "workspace": { "mode": "NEW" } }))
            .await,
    );
    assert!(e.starts_with("grant_limit:"), "{e}");
    assert_eq!((fx.calls(), fx.live_grants(), fx.spaces()), (2, 2, spaces));
    // Revoking one frees a slot.
    let id = fx.grants()[0].key_id.clone();
    fx.store.revoke_api_key_for_owner(&id, OWNER).unwrap();
    let r = fx.arm(serde_json::json!({ "message": "m" })).await;
    assert!(!failed(&r), "{r:?}");
}

#[tokio::test]
async fn a_broker_failure_revokes_the_grant_and_removes_the_rows_it_made() {
    for mode in [Mode::TransportError, Mode::ToolError] {
        let fx = fixture(0).await;
        *fx.mode.lock().unwrap() = mode;
        let spaces = fx.spaces();
        let threads = fx.threads(&fx.calling_space);
        let r = fx
            .arm(serde_json::json!({ "message": "m",
                "workspace": { "mode": "NEW", "name": "fresh" },
                "thread": { "mode": "NEW", "name": "t" } }))
            .await;
        assert!(failed(&r), "{r:?}");
        assert_eq!(fx.calls(), 1);
        let grants = fx.grants();
        assert_eq!(grants.len(), 1);
        assert!(grants[0].revoked_at.is_some(), "the grant is revoked");
        let token = fx.seen.lock().unwrap()[0].run_grant.clone().unwrap().token;
        let secret = token.split_once('.').unwrap().1;
        assert!(fx
            .store
            .check_credential(secret, &Transport::Broker(BROKER.to_string()))
            .is_err());
        assert_eq!(fx.spaces(), spaces, "the new Space is gone");
        assert_eq!(fx.threads(&fx.calling_space), threads);
        // The broker's own words, with the token in them, come back redacted.
        let text = error_of(&r);
        assert!(!text.contains(secret) && !text.contains(&token), "{text}");

        // A thread NEW in the calling Space is removed too.
        let r = fx
            .arm(serde_json::json!({ "message": "m", "thread": { "mode": "NEW" } }))
            .await;
        assert!(failed(&r));
        assert_eq!(fx.threads(&fx.calling_space), threads);
        assert_eq!(fx.live_grants(), 0);
    }
}

#[tokio::test]
async fn the_token_never_reaches_the_result_or_the_outbox() {
    let fx = fixture(0).await;
    *fx.mode.lock().unwrap() = Mode::EchoToken;
    let r = fx.arm(serde_json::json!({ "message": "m" })).await;
    assert!(!failed(&r), "{r:?}");
    let token = fx.seen.lock().unwrap()[0].run_grant.clone().unwrap().token;
    let secret = token.split_once('.').unwrap().1.to_string();
    assert!(!r.text_result.contains(&token) && !r.text_result.contains(&secret));
    let outbox = all_outbox(&fx);
    assert!(!outbox.contains(&token) && !outbox.contains(&secret));
    // The grant stays: only the broker holds the token.
    assert_eq!(fx.live_grants(), 1);
    assert!(
        fx.grants()[0]
            .constraints_json
            .as_ref()
            .is_some_and(|c| !c.contains(&secret)),
        "the secret is not stored"
    );
}

#[tokio::test]
async fn bad_requests_are_typed_errors_and_leave_nothing() {
    let fx = fixture(0).await;
    let spaces = fx.spaces();
    let big = "x".repeat(32 * 1024 + 1);
    let cases = [
        (serde_json::json!({ "message": big }), "message_too_large:"),
        (serde_json::json!({ "message": "  " }), "run_invalid:"),
        (
            serde_json::json!({ "message": "m", "surprise": 1 }),
            "run_invalid:",
        ),
        (
            serde_json::json!({ "message": "m", "inputs": { "a": 1 } }),
            "run_invalid:",
        ),
        (
            serde_json::json!({ "message": "m", "workspace": { "mode": "NEW" }, "thread": { "mode": "SAME" } }),
            "thread_needs_space:",
        ),
        (
            serde_json::json!({ "message": "m", "workspace": { "mode": "NEW" },
                "thread": { "mode": "ID", "id": "th_aaaaaaaaaaaaaaaaaaaaaaaaaa" } }),
            "thread_needs_space:",
        ),
        (
            serde_json::json!({ "message": "m", "workspace": { "mode": "ID", "id": "not-an-id" } }),
            "space_not_found:",
        ),
        (
            serde_json::json!({ "message": "m",
                "playbookRef": { "resourceId": { "urn": "urn:vam:playbook:local.vam.dev/acme/none" } } }),
            "playbook_invalid:",
        ),
        (serde_json::json!("just text"), "run_invalid:"),
    ];
    for (run, code) in cases {
        let e = error_of(&fx.arm(run.clone()).await);
        assert!(e.starts_with(code), "{run} -> {e}");
    }
    assert_eq!((fx.calls(), fx.grants().len(), fx.spaces()), (0, 0, spaces));
    // Full enum names and a generated label are accepted.
    let r = fx
        .arm(serde_json::json!({ "message": "m",
            "workspace": { "mode": "PLACE_MODE_NEW" }, "onBusy": "ON_BUSY_QUEUE" }))
        .await;
    assert!(!failed(&r), "{r:?}");
    let t = fx.seen.lock().unwrap()[0]
        .run_grant
        .clone()
        .unwrap()
        .target
        .unwrap();
    assert!(t.space_name.starts_with("schedule-") && t.space_name.len() == "schedule-".len() + 6);
    assert_eq!(t.thread_name, "untitled");
}

const PLAYBOOK: &str = "urn:vam:playbook:local.vam.dev/acme/grants-strict";

/// Publishes and releases a playbook whose `input_schema` requires a string `invoice_url`.
fn seed_playbook(fx: &Fx) {
    use vam_spec::v1::{PlaybookSpec, PlaybookStep, StepMode};
    let schema = json_struct(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["invoice_url"],
        "properties": { "invoice_url": { "type": "string" } }
    }));
    let playbook = PlaybookSpec {
        activation: Some(vam_spec::v1::PlaybookActivation::asking(
            "Use for invoices.",
        )),
        resource_id: Some(ResourceId {
            urn: PLAYBOOK.to_string(),
        }),
        display_name: "Strict".to_string(),
        description: "Playbook with a strict input schema".to_string(),
        input_schema: Some(schema),
        steps: vec![PlaybookStep {
            id: "s1".to_string(),
            description: "step".to_string(),
            instruction: "Do the step.".to_string(),
            mode: StepMode::Agentic as i32,
            ..Default::default()
        }],
        ..Default::default()
    };
    fx.store
        .publish_playbook_draft_with_owner(&playbook, OWNER)
        .unwrap();
    fx.store
        .release_playbook(
            PLAYBOOK,
            &vam_spec::v1::Version {
                major: 1,
                ..Default::default()
            },
        )
        .unwrap();
}

#[tokio::test]
async fn a_playbook_grant_pins_the_playbook_and_binds_the_inputs() {
    let fx = fixture(0).await;
    seed_playbook(&fx);
    let r = fx
        .arm(serde_json::json!({
            "playbookRef": { "resourceId": { "urn": PLAYBOOK } },
            "inputs": { "invoice_url": "INV-1001" },
            "thread": { "mode": "NEW", "name": "invoices" }
        }))
        .await;
    assert!(!failed(&r), "{r:?}");
    let grant = fx.seen.lock().unwrap()[0].run_grant.clone().unwrap();
    let target = grant.target.unwrap();
    let pinned = target
        .playbook_ref
        .expect("GrantTarget carries the playbook");
    assert_eq!(pinned.resource_id.unwrap().urn, PLAYBOOK);
    assert_eq!(pinned.version.unwrap().major, 1);
    assert!(pinned.digest.is_some_and(|d| !d.hex_hash.is_empty()));

    let rows = fx.grants();
    assert_eq!(rows.len(), 1);
    let c: serde_json::Value =
        serde_json::from_str(rows[0].constraints_json.as_ref().unwrap()).unwrap();
    assert_eq!(c["playbook_ref"]["urn"], PLAYBOOK);
    assert_eq!(c["playbook_ref"]["version"], "1.0.0");
    assert_eq!(c["playbook_ref"]["digest"].as_str().map(str::len), Some(64));
    assert_eq!(
        c["inputs"],
        serde_json::json!({ "invoice_url": "INV-1001" })
    );
    assert_eq!(c["message"], "");
    assert_eq!(c["thread_id"], target.thread_id);
}

#[tokio::test]
async fn playbook_inputs_that_fail_the_schema_are_refused_and_leave_nothing() {
    let fx = fixture(0).await;
    seed_playbook(&fx);
    let spaces = fx.spaces();
    let threads = fx.threads(&fx.calling_space);
    for inputs in [
        serde_json::json!({}),
        serde_json::json!({ "invoice_url": 7 }),
        serde_json::json!({ "invoice_url": "u", "extra": 1 }),
    ] {
        let e = error_of(
            &fx.arm(serde_json::json!({
                "playbookRef": { "resourceId": { "urn": PLAYBOOK } },
                "inputs": inputs,
                "workspace": { "mode": "NEW" },
                "thread": { "mode": "NEW" }
            }))
            .await,
        );
        assert!(e.starts_with("playbook_invalid:"), "{inputs} -> {e}");
    }
    assert_eq!((fx.calls(), fx.grants().len(), fx.spaces()), (0, 0, spaces));
    assert_eq!(fx.threads(&fx.calling_space), threads);
}
