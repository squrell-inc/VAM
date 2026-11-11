use super::*;
use crate::error::AttemptKey;
use crate::session::builtin_tool_catalog;
use crate::session_targets::assemble_session_targets;
use crate::users::LOCAL_USER_ID;
use serde_json::json;
use vam_inference::SessionBudget;
use vam_security::PolicyMode;
use vam_spec::v1::tool_intent_response::Disposition;
use vam_spec::v1::{
    agent_spec::InstructionSource, AgentPermissions, AgentSpec, ExecutionLease, PermissionDecision,
    PermissionRule, ResourceId, ResourceRef, RunSpec, ToolDefault, ToolIntentRequest,
    ToolIntentResponse, ToolPermissions, Version,
};

const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/outbox-agent";
const SESSION: &str = "sess-outbox";

/// Each fixture has its own run: the rate window is keyed by run id and the tests
/// run in one process.
fn fresh_run() -> String {
    use std::sync::atomic::{AtomicUsize, Ordering};
    static N: AtomicUsize = AtomicUsize::new(0);
    format!(
        "urn:vam:run:local.vam.dev/acme/outbox-run-{}",
        N.fetch_add(1, Ordering::SeqCst)
    )
}

fn rid(urn: &str) -> ResourceId {
    ResourceId {
        urn: urn.to_string(),
    }
}

fn lease_for(run: &str, lease_id: &str) -> ExecutionLease {
    ExecutionLease {
        run_id: Some(rid(run)),
        step_id: "step-1".to_string(),
        attempt_id: Some(rid("urn:vam:attempt:local.vam.dev/acme/outbox")),
        lease_id: Some(rid(lease_id)),
        lifecycle_generation: 1,
        fencing_token: vec![1],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: "idem-outbox".to_string(),
    }
}

fn lease(run: &str) -> ExecutionLease {
    lease_for(run, "urn:vam:lease:local.vam.dev/acme/outbox")
}

/// How the agent's permissions are written.
enum Perms {
    /// No `permissions`: the legacy policy path.
    Legacy,
    Rules(ToolDefault, Vec<(&'static str, PermissionDecision)>),
}

struct Fx {
    store: Arc<SqliteWorkflowStore>,
    ctx: SessionSecurityContext,
    run: String,
}

impl Fx {
    fn lease(&self) -> ExecutionLease {
        lease(&self.run)
    }
}

fn fixture(perms: Perms, jig: bool) -> Fx {
    let run = fresh_run();
    #[allow(non_snake_case)]
    let RUN = run.as_str();
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    store
        .create_session(SESSION, None, "vm", "ws", "storage-default")
        .expect("session");
    let (permissions, capability_policy) = match perms {
        Perms::Legacy => (
            None,
            Some(vam_spec::v1::CapabilityPolicyRef {
                security_profile_name: "restricted-egress".to_string(),
                ..Default::default()
            }),
        ),
        Perms::Rules(default, rules) => (
            Some(AgentPermissions {
                tools: Some(ToolPermissions {
                    default: default as i32,
                    rules: rules
                        .into_iter()
                        .map(|(target, decision)| PermissionRule {
                            target: target.to_string(),
                            decision: decision as i32,
                        })
                        .collect(),
                }),
                ..Default::default()
            }),
            None,
        ),
    };
    let agent = AgentSpec {
        resource_id: Some(rid(AGENT)),
        display_name: "Outbox agent".to_string(),
        description: "test".to_string(),
        role: "assistant".to_string(),
        instruction_source: Some(InstructionSource::SystemInstructions("x".to_string())),
        permissions,
        capability_policy,
        ..Default::default()
    };
    store
        .publish_agent_draft_with_owner(&agent, LOCAL_USER_ID)
        .expect("draft");
    let pinned = Version {
        major: 1,
        minor: 1,
        ..Default::default()
    };
    store.release_agent(AGENT, &pinned).expect("release");
    let overrides = if jig {
        json!({"human": {"jig": true}})
    } else {
        json!({})
    };
    let resolved = vam_spec::harness::Harness::resolve(&overrides).expect("harness");
    let record = vam_spec::harness::Harness::record(&resolved).expect("record");
    let mut inputs = prost_types::Struct::default();
    inputs.fields.insert(
        vam_spec::harness::INPUT_KEY.to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StructValue(
                vam_spec::harness::json_to_struct(&record).expect("record struct"),
            )),
        },
    );
    let spec = RunSpec {
        run_id: Some(rid(RUN)),
        agent_ref: Some(ResourceRef {
            resource_id: Some(rid(AGENT)),
            version: Some(pinned),
            ..Default::default()
        }),
        inputs: Some(inputs),
        ..Default::default()
    };
    let spec_json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("json");
    store
        .persist_run_spec_with_owner(
            RUN,
            &spec_json,
            Some(SESSION),
            AGENT,
            None,
            "User::\"local\"",
            LOCAL_USER_ID,
        )
        .expect("run");
    store.issue_lease(&lease(RUN)).expect("lease");
    let mut catalog = builtin_tool_catalog();
    catalog.extend(catalog_entries());
    store
        .record_tool_catalog(SESSION, &catalog)
        .expect("catalog");
    assemble_session_targets(&store, SESSION, &catalog, &[]).expect("assemble");
    let resolved =
        crate::security::resolve_session_policy(&store, SESSION, Some(AGENT)).expect("policy");
    let ctx = SessionSecurityContext::new(
        SESSION.to_string(),
        AGENT.to_string(),
        &resolved.cedar,
        PolicyMode::Enforce,
        1,
        ed25519_dalek::SigningKey::from_bytes(&[3u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .expect("ctx")
    .with_policy_mode(resolved.mode);
    Fx { store, ctx, run }
}

fn default_deny() -> Fx {
    fixture(
        Perms::Rules(
            ToolDefault::Deny,
            vec![("builtin:read_file", PermissionDecision::Allow)],
        ),
        true,
    )
}

fn intent(fx: &Fx, tool: &str, args: &prost_types::Struct) -> ToolIntentResponse {
    fx.ctx.handle_tool_intent_request(
        &fx.store,
        ToolIntentRequest {
            lease: Some(fx.lease()),
            tool_name: tool.to_string(),
            arguments: Some(args.clone()),
            arguments_digest: None,
            script_digest: Some(vam_spec::v1::Digest {
                algorithm: 1,
                hex_hash: vam_spec::outbox::tool_digest(tool),
            }),
            call_id: String::new(),
        },
    )
}

fn token_for(fx: &Fx, tool: &str, args: &prost_types::Struct) -> vam_spec::v1::SignedEnvelope {
    let resp = intent(fx, tool, args);
    assert_eq!(
        resp.disposition,
        Disposition::Authorized as i32,
        "{:?}",
        resp.error
    );
    resp.capability_token.expect("token")
}

fn message_request(
    fx: &Fx,
    text: &str,
    token: Option<vam_spec::v1::SignedEnvelope>,
) -> PostOutboxRequest {
    PostOutboxRequest {
        lease: Some(fx.lease()),
        kind: OutboxKind::Message as i32,
        text: text.to_string(),
        capability_token: token,
        ..Default::default()
    }
}

/// What the guest does: authorize the model's arguments, then send the data as JSON.
fn card_request(fx: &Fx, jig: &str, data: serde_json::Value) -> PostOutboxRequest {
    let guest_args =
        vam_spec::harness::json_to_struct(&json!({"jig": jig, "data": data})).expect("object");
    let token = token_for(fx, "show", &guest_args);
    let Some(prost_types::value::Kind::StructValue(d)) = guest_args.fields["data"].kind.clone()
    else {
        panic!("data struct")
    };
    PostOutboxRequest {
        lease: Some(fx.lease()),
        kind: OutboxKind::Card as i32,
        jig: jig.to_string(),
        data_json: vam_spec::harness::struct_to_json(&d).to_string(),
        capability_token: Some(token),
        ..Default::default()
    }
}

fn post(fx: &Fx, request: &PostOutboxRequest) -> PostOutboxResponse {
    handle_post_outbox(&fx.ctx, &fx.store, SESSION, request)
}

fn message_ok(fx: &Fx, text: &str) -> PostOutboxResponse {
    let token = token_for(fx, "post_message", &message_args(text));
    post(fx, &message_request(fx, text, Some(token)))
}

fn outbox_events(fx: &Fx) -> Vec<(String, serde_json::Value)> {
    fx.store
        .read_outbox_for_run(&fx.run, 0)
        .expect("events")
        .into_iter()
        .filter(|e| e.event_type == TYPE_OUTBOX_MESSAGE || e.event_type == TYPE_OUTBOX_CARD)
        .map(|e| {
            let event: serde_json::Value = serde_json::from_str(&e.payload).expect("json");
            assert_eq!(event["specVersion"], "1.0", "{event}");
            assert_eq!(event["type"], e.event_type, "{event}");
            assert_eq!(event["runId"]["urn"], fx.run.as_str(), "{event}");
            (e.event_type, event["data"].clone())
        })
        .collect()
}

fn code(resp: &PostOutboxResponse) -> i32 {
    resp.error.as_ref().map(|e| e.code).unwrap_or(0)
}

fn message(resp: &PostOutboxResponse) -> String {
    resp.error
        .as_ref()
        .map(|e| e.message.clone())
        .unwrap_or_default()
}

// ---- success ---------------------------------------------------------------

#[test]
fn a_message_is_recorded_as_an_event_with_its_text() {
    let fx = default_deny();
    let resp = message_ok(&fx, "Starting: 1 of 2");
    assert!(resp.error.is_none(), "{resp:?}");
    let events = outbox_events(&fx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, TYPE_OUTBOX_MESSAGE);
    assert_eq!(events[0].1, json!({"text": "Starting: 1 of 2"}));
}

#[test]
fn a_progress_card_is_recorded_with_its_checked_data_and_card_id() {
    let fx = default_deny();
    let data = json!({"label": "Checking", "done": 1, "total": 2, "card_id": "c1"});
    let resp = post(&fx, &card_request(&fx, "vam.progress", data.clone()));
    assert!(resp.error.is_none(), "{resp:?}");
    let events = outbox_events(&fx);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, TYPE_OUTBOX_CARD);
    assert_eq!(events[0].1["jig"], "vam.progress");
    assert_eq!(events[0].1["data"], data);
    assert_eq!(events[0].1["data"]["done"], json!(1));
    assert_eq!(events[0].1["card_id"], "c1");
}

#[test]
fn two_cards_with_one_card_id_are_two_events_naming_it() {
    let fx = default_deny();
    for done in [1, 2] {
        let data = json!({"label": "Work", "done": done, "total": 3, "card_id": "work"});
        assert!(post(&fx, &card_request(&fx, "vam.progress", data))
            .error
            .is_none());
    }
    let events = outbox_events(&fx);
    assert_eq!(events.len(), 2);
    assert!(events.iter().all(|(_, p)| p["card_id"] == "work"));
    assert_eq!(events[1].1["data"]["done"], json!(2));
}

// ---- refusals emit nothing -------------------------------------------------

#[test]
fn bad_card_data_is_a_typed_error_and_emits_nothing() {
    let fx = default_deny();
    let resp = post(
        &fx,
        &card_request(
            &fx,
            "vam.progress",
            json!({"label": "Checking", "done": "one", "total": 2}),
        ),
    );
    assert_eq!(code(&resp), INVALID_ARGUMENT);
    let text = message(&resp);
    assert!(
        text.starts_with("invalid data for vam.progress: /done"),
        "{text}"
    );
    assert!(outbox_events(&fx).is_empty());
    // An extra field and a missing one are named too.
    let resp = post(
        &fx,
        &card_request(
            &fx,
            "vam.progress",
            json!({"label": "x", "total": 2, "extra": 1}),
        ),
    );
    assert!(message(&resp).starts_with("invalid data for vam.progress"));
    assert!(outbox_events(&fx).is_empty());
}

#[test]
fn only_built_in_show_jigs_can_be_shown() {
    let fx = default_deny();
    for jig in ["vam.question", "vam.approve", "nope.nothing"] {
        let resp = post(&fx, &card_request(&fx, jig, json!({"label": "x"})));
        assert_eq!(code(&resp), INVALID_ARGUMENT, "{jig}");
        assert!(
            message(&resp).contains("vam.progress"),
            "{}",
            message(&resp)
        );
    }
    assert!(outbox_events(&fx).is_empty());
}

#[test]
fn card_data_must_be_one_json_object() {
    let fx = default_deny();
    let token = token_for(&fx, "show", &card_args("vam.progress", &json!({})).unwrap());
    for bad in ["[1,2]", "not json", "", "3"] {
        let resp = post(
            &fx,
            &PostOutboxRequest {
                lease: Some(fx.lease()),
                kind: OutboxKind::Card as i32,
                jig: "vam.progress".into(),
                data_json: bad.into(),
                capability_token: Some(token.clone()),
                ..Default::default()
            },
        );
        assert_eq!(code(&resp), INVALID_ARGUMENT, "{bad:?}");
    }
    assert!(outbox_events(&fx).is_empty());
}

#[test]
fn caps_on_text_and_card_data() {
    let fx = default_deny();
    let ok = "a".repeat(MAX_TEXT_BYTES);
    assert!(message_ok(&fx, &ok).error.is_none());
    let long = "a".repeat(MAX_TEXT_BYTES + 1);
    let resp = message_ok(&fx, &long);
    assert_eq!(code(&resp), INVALID_ARGUMENT);
    assert!(message(&resp).contains("4096"));
    assert_eq!(code(&message_ok(&fx, "   ")), INVALID_ARGUMENT);
    let big = json!({"label": "x".repeat(MAX_CARD_DATA_BYTES), "done": 1, "total": 2});
    let resp = post(&fx, &card_request(&fx, "vam.progress", big));
    assert_eq!(code(&resp), INVALID_ARGUMENT);
    assert!(message(&resp).contains("16384"), "{}", message(&resp));
    assert_eq!(outbox_events(&fx).len(), 1, "only the message at the cap");
}

// ---- authorization ---------------------------------------------------------

#[test]
fn a_guest_without_an_authorized_intent_is_refused() {
    let fx = default_deny();
    // No token at all.
    let resp = post(&fx, &message_request(&fx, "hi", None));
    assert_eq!(code(&resp), PERMISSION_DENIED);
    // A token minted for other arguments.
    let token = token_for(&fx, "post_message", &message_args("authorized text"));
    let resp = post(&fx, &message_request(&fx, "different text", Some(token)));
    assert_eq!(code(&resp), PERMISSION_DENIED);
    // A token minted for another tool.
    let other = card_args(
        "vam.progress",
        &json!({"label": "x", "done": 1, "total": 2}),
    )
    .unwrap();
    let token = token_for(&fx, "show", &other);
    let resp = post(&fx, &message_request(&fx, "hi", Some(token)));
    assert_eq!(code(&resp), PERMISSION_DENIED);
    // A malformed token.
    let junk = vam_spec::v1::SignedEnvelope {
        domain: "x".into(),
        claims: b"{}".to_vec(),
        ..Default::default()
    };
    assert_eq!(
        code(&post(&fx, &message_request(&fx, "hi", Some(junk)))),
        PERMISSION_DENIED
    );
    assert!(outbox_events(&fx).is_empty());
}

#[test]
fn a_token_works_once() {
    let fx = default_deny();
    let token = token_for(&fx, "post_message", &message_args("once"));
    assert!(
        post(&fx, &message_request(&fx, "once", Some(token.clone())))
            .error
            .is_none()
    );
    assert_eq!(
        code(&post(&fx, &message_request(&fx, "once", Some(token)))),
        PERMISSION_DENIED
    );
    assert_eq!(outbox_events(&fx).len(), 1);
}

#[test]
fn card_data_changed_after_the_intent_is_refused() {
    let fx = default_deny();
    let mut req = card_request(
        &fx,
        "vam.progress",
        json!({"label": "x", "done": 1, "total": 2}),
    );
    req.data_json = json!({"label": "x", "done": 2, "total": 2}).to_string();
    assert_eq!(code(&post(&fx, &req)), PERMISSION_DENIED);
    assert!(outbox_events(&fx).is_empty());
}

#[test]
fn a_lease_of_another_run_is_refused() {
    let fx = default_deny();
    let token = token_for(&fx, "post_message", &message_args("hi"));
    let mut req = message_request(&fx, "hi", Some(token));
    req.lease = Some(lease_for(
        "urn:vam:run:local.vam.dev/acme/other",
        "urn:vam:lease:local.vam.dev/acme/outbox",
    ));
    assert_eq!(code(&post(&fx, &req)), 7);
    let mut none = message_request(&fx, "hi", None);
    none.lease = None;
    assert_eq!(code(&post(&fx, &none)), 3);
    assert!(outbox_events(&fx).is_empty());
}

#[test]
fn show_needs_the_run_to_have_human_jig_on() {
    let off = fixture(Perms::Rules(ToolDefault::Deny, vec![]), false);
    let resp = post(
        &off,
        &card_request(
            &off,
            "vam.progress",
            json!({"label": "x", "done": 1, "total": 2}),
        ),
    );
    assert_eq!(code(&resp), PERMISSION_DENIED);
    assert!(message(&resp).contains("human.jig"), "{}", message(&resp));
    assert!(outbox_events(&off).is_empty());
    // post_message does not need it.
    assert!(message_ok(&off, "still fine").error.is_none());
}

// ---- rate ------------------------------------------------------------------

#[test]
fn the_sixty_first_post_in_a_minute_is_rate_limited_and_the_window_slides() {
    let run = "urn:vam:run:rate-test-a";
    let start = Instant::now();
    for i in 0..MAX_POSTS_PER_MINUTE {
        assert!(
            admit_at(run, start + Duration::from_millis(i as u64)),
            "{i}"
        );
    }
    assert!(!admit_at(run, start + Duration::from_secs(30)));
    // Another run is not affected.
    assert!(admit_at(
        "urn:vam:run:rate-test-b",
        start + Duration::from_secs(30)
    ));
    // After the minute, room again.
    assert!(admit_at(run, start + Duration::from_secs(61)));
}

#[test]
fn a_rate_limited_post_says_so_and_emits_nothing_more() {
    let fx = default_deny();
    let t0 = Instant::now();
    // Fill the run's window directly, then post once more.
    for _ in 0..MAX_POSTS_PER_MINUTE {
        assert!(admit_at(&fx.run, t0));
    }
    let token = token_for(&fx, "post_message", &message_args("one too many"));
    let resp = post_outbox_at(
        &fx.ctx,
        &fx.store,
        SESSION,
        &message_request(&fx, "one too many", Some(token)),
        t0,
    );
    assert_eq!(code(&resp), RESOURCE_EXHAUSTED);
    assert!(
        message(&resp).starts_with("rate limited"),
        "{}",
        message(&resp)
    );
    assert!(outbox_events(&fx).is_empty());
}

// ---- the overlay and policy.decided ---------------------------------------

fn decisions(fx: &Fx, tool: &str) -> Vec<String> {
    fx.store
        .read_outbox_for_run(&fx.run, 0)
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type.ends_with("policy.decided"))
        .filter_map(|e| serde_json::from_str::<serde_json::Value>(&e.payload).ok())
        .filter(|p| p["tool_name"] == tool)
        .map(|p| p["disposition"].as_str().unwrap_or_default().to_string())
        .collect()
}

#[test]
fn an_agent_with_default_deny_keeps_both_tools_and_every_call_is_decided() {
    let fx = default_deny();
    let advertised = fx.ctx.advertised_tool_names(&fx.store);
    assert!(
        advertised.contains(&"post_message".to_string()),
        "{advertised:?}"
    );
    assert!(advertised.contains(&"show".to_string()), "{advertised:?}");
    // Other tools stay denied by the default.
    assert!(!advertised.contains(&"run".to_string()));
    assert!(!advertised.contains(&"write_file".to_string()));
    assert!(message_ok(&fx, "hi").error.is_none());
    assert_eq!(decisions(&fx, "post_message"), ["ALLOW"]);
    let denied = intent(&fx, "run", &message_args("x"));
    assert_eq!(denied.disposition, Disposition::Denied as i32);
}

#[test]
fn an_explicit_deny_removes_the_tool_and_is_still_decided() {
    let fx = fixture(
        Perms::Rules(
            ToolDefault::Deny,
            vec![("builtin:post_message", PermissionDecision::Deny)],
        ),
        true,
    );
    let advertised = fx.ctx.advertised_tool_names(&fx.store);
    assert!(
        !advertised.contains(&"post_message".to_string()),
        "{advertised:?}"
    );
    assert!(advertised.contains(&"show".to_string()), "{advertised:?}");
    let resp = intent(&fx, "post_message", &message_args("hi"));
    assert_eq!(resp.disposition, Disposition::Denied as i32);
    assert_eq!(decisions(&fx, "post_message"), ["DENY"]);
    // Denied at the intent, so no token exists to post with.
    assert_eq!(
        code(&post(&fx, &message_request(&fx, "hi", None))),
        PERMISSION_DENIED
    );
    assert!(outbox_events(&fx).is_empty());
}

#[test]
fn an_explicit_deny_of_show_removes_show_only() {
    let fx = fixture(
        Perms::Rules(
            ToolDefault::Allow,
            vec![("builtin:show", PermissionDecision::Deny)],
        ),
        true,
    );
    let advertised = fx.ctx.advertised_tool_names(&fx.store);
    assert!(advertised.contains(&"post_message".to_string()));
    assert!(!advertised.contains(&"show".to_string()));
}

#[test]
fn a_wildcard_default_deny_rule_does_not_remove_the_tools() {
    // `tools:*` is the default, not a rule that names the tool.
    let fx = fixture(
        Perms::Rules(
            ToolDefault::Allow,
            vec![("tools:*", PermissionDecision::Deny)],
        ),
        true,
    );
    let advertised = fx.ctx.advertised_tool_names(&fx.store);
    assert!(
        advertised.contains(&"post_message".to_string()),
        "{advertised:?}"
    );
    assert!(advertised.contains(&"show".to_string()), "{advertised:?}");
}

#[test]
fn an_explicit_ask_rule_still_asks() {
    let fx = fixture(
        Perms::Rules(
            ToolDefault::Deny,
            vec![("builtin:post_message", PermissionDecision::Ask)],
        ),
        true,
    );
    let resp = intent(&fx, "post_message", &message_args("hi"));
    assert_eq!(
        resp.disposition,
        Disposition::Suspended as i32,
        "{:?}",
        resp.error
    );
    assert_eq!(decisions(&fx, "post_message"), ["SUSPEND"]);
}

#[test]
fn legacy_agents_get_the_overlay() {
    let fx = fixture(Perms::Legacy, true);
    let cedar = crate::security::resolve_session_policy(&fx.store, SESSION, Some(AGENT))
        .unwrap()
        .cedar;
    assert!(cedar.contains("Tool::\"post_message\"") && cedar.contains("Tool::\"show\""));
    assert!(message_ok(&fx, "hi").error.is_none());
    assert_eq!(decisions(&fx, "post_message"), ["ALLOW"]);
}

#[test]
fn the_overlay_is_in_the_permissions_policy_and_a_forbid_beats_it() {
    let fx = default_deny();
    let cedar = crate::security::resolve_session_policy(&fx.store, SESSION, Some(AGENT))
        .unwrap()
        .cedar;
    assert!(cedar.contains(crate::security::OUTBOX_OVERLAY.trim()));
    // The platform forbid on an org deny is not touched by the overlay.
    assert!(cedar.contains("resource.org_rule == \"deny\""));
}

#[test]
fn the_catalog_entries_are_builtins_with_the_shared_digest() {
    let entries = catalog_entries();
    let names: Vec<_> = entries.iter().map(|e| e.tool_name.as_str()).collect();
    assert_eq!(names, ["post_message", "show", "ask_user"]);
    for e in &entries {
        assert_eq!(e.command, "builtin");
        assert_eq!(e.digest, vam_spec::outbox::tool_digest(&e.tool_name));
    }
    // A skill tool of the same name fails closed in the merge.
    let clash = ToolCatalogEntry {
        tool_name: "show".into(),
        command: "bun".into(),
        entrypoint: "show".into(),
        digest: String::new(),
        side_effect: 1,
        skill_name: "demo".into(),
        approval_required: false,
    };
    let mut builtins = builtin_tool_catalog();
    builtins.extend(entries);
    assert!(crate::session::merge_tool_catalog(builtins, vec![clash], vec![]).is_err());
}

// ---- ask_user (AR-3.10) ----------------------------------------------------

fn ask_args(jig: &str, data: serde_json::Value) -> prost_types::Struct {
    vam_spec::outbox::card_args(jig, &data).expect("object data")
}

fn rid_of(resp: &ToolIntentResponse) -> String {
    resp.approval_request_id.clone().expect("approval id").urn
}

/// What the guest's suspend does after the intent: record the outcome and
/// publish the approval.
fn publish_question(fx: &Fx, request_id: &str) {
    use vam_spec::v1::{ArtifactRef, Digest, TaskOutcome, TaskOutcomeState};
    let key = AttemptKey {
        run_id: fx.run.clone(),
        step_id: "step-1".to_string(),
        attempt_id: "urn:vam:attempt:local.vam.dev/acme/outbox".to_string(),
    };
    let artifact = ArtifactRef {
        artifact_id: Some(rid("urn:vam:artifact:local.vam.dev/acme/ask-checkpoint")),
        digest: Some(Digest {
            algorithm: 1,
            hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                .to_string(),
        }),
        media_type: "application/octet-stream".to_string(),
        size_bytes: 1,
        uri: "memory://checkpoint".to_string(),
    };
    fx.store
        .record_outcome(&TaskOutcome {
            lease: Some(fx.lease()),
            state: TaskOutcomeState::Suspended as i32,
            output: None,
            output_artifacts: Vec::new(),
            continuation: Some(artifact.clone()),
            evidence: None,
            observed_usage: None,
            error: None,
        })
        .expect("outcome");
    fx.store
        .create_approval(&key, &rid(request_id), &artifact, 1)
        .expect("publish");
    fx.store
        .update_run_state(&fx.run, "PAUSED")
        .expect("paused");
}

fn decide(
    fx: &Fx,
    request_id: &str,
    decision: &str,
    reason: Option<&str>,
    amended: Option<&str>,
    response: Option<&str>,
    caller: &str,
) -> Result<crate::error::ApprovalRecord, crate::error::OrchestratorError> {
    fx.store.decide_approval(
        &rid(request_id),
        &rid("urn:vam:token:local.vam.dev/acme/ask"),
        decision,
        "User::\"local\"",
        caller,
        reason,
        amended,
        None,
        response,
    )
}

fn ask_fx(jig: bool) -> Fx {
    fixture(Perms::Legacy, jig)
}

#[test]
fn ask_user_with_bad_data_is_a_typed_error_and_creates_no_approval() {
    let fx = ask_fx(false);
    for data in [
        json!({}),
        json!({"question": ""}),
        json!({"question": "q", "choices": []}),
        json!({"question": "q", "extra": 1}),
    ] {
        let resp = intent(&fx, "ask_user", &ask_args("vam.question", data));
        assert_eq!(resp.disposition, Disposition::Denied as i32, "{resp:?}");
        let msg = resp.error.expect("error").message;
        assert!(msg.starts_with("invalid data for vam.question"), "{msg}");
        assert!(msg.contains("ask_user again"), "{msg}");
    }
    assert!(fx.store.pending_approval_kind(&fx.run).is_none());
    // Not an object, and too big.
    let big = json!({"question": "x".repeat(20_000)});
    let resp = intent(&fx, "ask_user", &ask_args("vam.question", big));
    assert_eq!(resp.disposition, Disposition::Denied as i32);
    assert!(fx.store.get_approval("none").is_err());
}

#[test]
fn ask_user_parks_a_question_approval_that_waits_for_input() {
    let fx = ask_fx(false);
    let data = json!({"question": "Which name should I use?", "choices": ["Ada", "Grace"]});
    let resp = intent(&fx, "ask_user", &ask_args("vam.question", data.clone()));
    assert_eq!(resp.disposition, Disposition::Suspended as i32, "{resp:?}");
    let id = rid_of(&resp);
    publish_question(&fx, &id);
    let stored = fx.store.get_approval(&id).unwrap();
    assert_eq!(stored.kind, crate::hitl::APPROVAL_KIND_QUESTION);
    assert_eq!(stored.tool_name, "ask_user");
    assert_eq!(stored.approval_message, "Which name should I use?");
    let requested: serde_json::Value = serde_json::from_str(&stored.requested_args_json).unwrap();
    assert_eq!(requested, json!({"jig": "vam.question", "data": data}));
    let schema: serde_json::Value = serde_json::from_str(&stored.amendable_schema_json).unwrap();
    assert_eq!(schema["required"], json!(["answer"]));
    assert!(!stored.always_offered);
    assert_eq!(
        fx.store.pending_approval_kind(&fx.run).as_deref(),
        Some("question")
    );
}

#[test]
fn other_ask_jigs_need_human_jig_but_question_never_does() {
    let off = ask_fx(false);
    let approve = json!({"tool": "run", "args_summary": "ls"});
    let resp = intent(&off, "ask_user", &ask_args("vam.approve", approve.clone()));
    assert_eq!(resp.disposition, Disposition::Denied as i32);
    assert!(resp.error.unwrap().message.contains("human.jig"));
    let ok = intent(
        &off,
        "ask_user",
        &ask_args("vam.question", json!({"question": "q"})),
    );
    assert_eq!(ok.disposition, Disposition::Suspended as i32);
    let on = ask_fx(true);
    let resp = intent(&on, "ask_user", &ask_args("vam.approve", approve));
    assert_eq!(resp.disposition, Disposition::Suspended as i32, "{resp:?}");
    // Not an Ask jig at all.
    let resp = intent(
        &on,
        "ask_user",
        &ask_args("vam.progress", json!({"label": "x", "done": 0, "total": 1})),
    );
    assert_eq!(resp.disposition, Disposition::Denied as i32);
}

fn parked_question(choices: bool) -> (Fx, String) {
    let fx = ask_fx(false);
    let data = if choices {
        json!({"question": "Pick", "choices": ["blue", "red"]})
    } else {
        json!({"question": "Name?"})
    };
    let resp = intent(&fx, "ask_user", &ask_args("vam.question", data));
    let id = rid_of(&resp);
    publish_question(&fx, &id);
    (fx, id)
}

#[test]
fn an_answer_off_the_choices_or_off_the_schema_is_refused_and_the_question_stays_pending() {
    let (fx, id) = parked_question(true);
    for bad in [
        r#"{"answer":"green"}"#,
        r#"{"answer":42}"#,
        r#"{}"#,
        r#"{"answer":"blue","extra":1}"#,
        r#"[1]"#,
        "not json",
    ] {
        let err =
            decide(&fx, &id, "SUBMITTED", None, None, Some(bad), LOCAL_USER_ID).expect_err(bad);
        assert!(
            matches!(err, crate::error::OrchestratorError::InvalidContract(_)),
            "{bad}: {err:?}"
        );
    }
    let stored = fx.store.get_approval(&id).unwrap();
    assert!(stored.decision.is_none(), "still pending");
    assert_eq!(fx.store.get_run_state(&fx.run).unwrap(), "PAUSED");
    // A valid one lands.
    let record = decide(
        &fx,
        &id,
        "SUBMITTED",
        None,
        None,
        Some(r#"{"answer":"red"}"#),
        LOCAL_USER_ID,
    )
    .expect("valid answer");
    assert_eq!(record.tool_name, "ask_user");
    let stored = fx.store.get_approval(&id).unwrap();
    assert_eq!(stored.decision.as_deref(), Some("SUBMITTED"));
    assert_eq!(
        stored.amended_args_json.as_deref(),
        Some(r#"{"answer":"red"}"#)
    );
}

#[test]
fn an_answer_over_16_kib_is_refused_before_it_is_checked_or_stored() {
    let (fx, id) = parked_question(false);
    let huge = format!(
        r#"{{"answer":"{}"}}"#,
        "x".repeat(vam_spec::outbox::MAX_ANSWER_BYTES)
    );
    assert!(huge.len() > vam_spec::outbox::MAX_ANSWER_BYTES);
    let err = decide(
        &fx,
        &id,
        "SUBMITTED",
        None,
        None,
        Some(&huge),
        LOCAL_USER_ID,
    )
    .unwrap_err();
    let text = err.to_string();
    assert!(text.contains("the limit is 16384"), "{text}");
    let stored = fx.store.get_approval(&id).unwrap();
    assert!(stored.decision.is_none());
    assert!(stored.amended_args_json.is_none(), "nothing was stored");
    assert!(fx.store.pending_approval_kind(&fx.run).is_some());
    // Just under the cap passes the size check (answer itself is still valid: <= 4000 chars is
    // the schema's own limit, so a 4000 character answer is accepted).
    let ok = format!(r#"{{"answer":"{}"}}"#, "y".repeat(4000));
    decide(&fx, &id, "SUBMITTED", None, None, Some(&ok), LOCAL_USER_ID).expect("4000 chars");
}

#[test]
fn only_the_runs_owner_answers() {
    let (fx, id) = parked_question(false);
    let err = decide(
        &fx,
        &id,
        "SUBMITTED",
        None,
        None,
        Some(r#"{"answer":"Ada"}"#),
        "someone-else",
    )
    .unwrap_err();
    assert!(
        !matches!(err, crate::error::OrchestratorError::InvalidContract(_)),
        "{err:?}"
    );
    assert!(fx.store.get_approval(&id).unwrap().decision.is_none());
}

#[test]
fn declining_answers_the_user_declined_to_answer() {
    let (fx, id) = parked_question(false);
    decide(&fx, &id, "REJECTED", None, None, None, LOCAL_USER_ID).expect("decline");
    let stored = fx.store.get_approval(&id).unwrap();
    assert_eq!(stored.decision.as_deref(), Some("REJECTED"));
    assert_eq!(
        stored.rejection_reason.as_deref(),
        Some("the user declined to answer")
    );
    assert!(!fx
        .store
        .has_hitl_grant(SESSION, "ask_user", &stored.args_digest));
}

#[test]
fn a_question_takes_no_grant_no_remember_no_amend_and_a_second_ask_asks_again() {
    let (fx, id) = parked_question(false);
    let digest = fx.store.get_approval(&id).unwrap().args_digest;
    // Approving or amending a question is refused; the question stays open.
    for decision in ["APPROVED", "AMENDED"] {
        let err = decide(
            &fx,
            &id,
            decision,
            None,
            Some(r#"{"jig":"vam.question","data":{"question":"other"}}"#),
            None,
            LOCAL_USER_ID,
        )
        .unwrap_err();
        assert!(
            matches!(err, crate::error::OrchestratorError::InvalidContract(_)),
            "{err:?}"
        );
    }
    assert!(fx.store.get_approval(&id).unwrap().decision.is_none());
    // Always allow / always deny (remember) are refused by the planner.
    let pending = fx.store.get_approval_for_owner(&id, LOCAL_USER_ID).unwrap();
    for rule in [
        vam_security::ScopeRule::Allow,
        vam_security::ScopeRule::Deny,
    ] {
        assert!(fx
            .store
            .plan_remember(&pending, LOCAL_USER_ID, rule)
            .is_err());
    }
    decide(
        &fx,
        &id,
        "SUBMITTED",
        None,
        None,
        Some(r#"{"answer":"Ada"}"#),
        LOCAL_USER_ID,
    )
    .expect("answer");
    // The answer stored no grant, in any session of the run.
    assert!(!fx.store.has_hitl_grant(SESSION, "ask_user", &digest));
    let grants: i64 = fx
        .store
        .connection
        .lock()
        .unwrap()
        .query_row("SELECT COUNT(*) FROM hitl_grants", [], |r| r.get(0))
        .unwrap();
    assert_eq!(grants, 0);
    // The same question, asked again in the same run (a new attempt after the
    // resume), asks again with a new approval.
    fx.store
        .connection
        .lock()
        .unwrap()
        .execute("UPDATE attempts SET state = 'RUNNING'", [])
        .unwrap();
    let again = intent(
        &fx,
        "ask_user",
        &ask_args("vam.question", json!({"question": "Name?"})),
    );
    assert_eq!(
        again.disposition,
        Disposition::Suspended as i32,
        "{again:?}"
    );
    assert_ne!(rid_of(&again), id);
}

#[test]
fn the_receipt_lists_the_answer_as_a_human_decision_of_kind_question() {
    let (fx, id) = parked_question(false);
    decide(
        &fx,
        &id,
        "SUBMITTED",
        None,
        None,
        Some(r#"{"answer":"Ada"}"#),
        LOCAL_USER_ID,
    )
    .expect("answer");
    let rows = fx.store.approval_provenance(&fx.run).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].kind, "question");
    assert_eq!(rows[0].decision.as_deref(), Some("SUBMITTED"));
    assert_eq!(rows[0].decided_by_user_id.as_deref(), Some(LOCAL_USER_ID));
    let asked = rows[0].requested_args_json.as_deref().unwrap();
    assert!(asked.contains("Name?"));
}

// ---- tool cards (CN-3.4) ----------------------------------------------------------------------

mod tool_cards {
    use super::*;
    use crate::skill_cards::tests_support::seeded;

    fn events(store: &SqliteWorkflowStore, run: &str, kind: &str) -> Vec<serde_json::Value> {
        store
            .read_outbox_for_run(run, 0)
            .unwrap()
            .into_iter()
            .filter(|e| e.event_type == kind)
            .map(|e| serde_json::from_str(&e.payload).unwrap())
            .collect()
    }

    const FIT: &str = r#"{"messages":[{"from":"a","subject":"Plan"}]}"#;

    #[test]
    fn a_fitting_result_emits_one_card_with_the_checked_ir() {
        let (session, _dir) = seeded("tc-fit", true);
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let run = fresh_run();
        attach_show_card(&store, &session, &run, "mail_list", "c1", FIT);
        let cards = events(&store, &run, TYPE_OUTBOX_CARD);
        assert_eq!(cards.len(), 1);
        let d = &cards[0]["data"];
        assert_eq!(d["jig"], "mail.mail_list");
        assert_eq!(d["tool"], "mail_list");
        assert_eq!(d["callId"], "c1");
        assert_eq!(d["source"], "tool");
        assert_eq!(d["data"]["messages"][0]["subject"], "Plan");
        assert_eq!(d["ir"]["jig"], "mail.mail_list");
        // The same call again adds nothing; another call adds a card.
        attach_show_card(&store, &session, &run, "mail_list", "c1", FIT);
        attach_show_card(&store, &session, &run, "mail_list", "c2", FIT);
        assert_eq!(events(&store, &run, TYPE_OUTBOX_CARD).len(), 2);
    }

    #[test]
    fn a_misfit_or_oversize_result_emits_card_skipped_only() {
        let (session, _dir) = seeded("tc-misfit", true);
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let run = fresh_run();
        attach_show_card(
            &store,
            &session,
            &run,
            "mail_list",
            "c1",
            r#"{"messages":[{"from":5}]}"#,
        );
        let big = format!(r#"{{"messages":[{{"subject":"{}"}}]}}"#, "x".repeat(20_000));
        attach_show_card(&store, &session, &run, "mail_list", "c2", &big);
        attach_show_card(&store, &session, &run, "mail_list", "c3", "not json");
        assert!(events(&store, &run, TYPE_OUTBOX_CARD).is_empty());
        let skipped = events(&store, &run, TYPE_CARD_SKIPPED);
        let reasons: Vec<_> = skipped
            .iter()
            .map(|e| e["data"]["reason"].as_str().unwrap())
            .collect();
        assert_eq!(
            reasons,
            [
                "result_does_not_fit",
                "result_too_large",
                "result_not_an_object"
            ]
        );
        assert_eq!(skipped[0]["data"]["tool"], "mail_list");
        assert!(skipped[0]["data"].get("data").is_none());
    }

    #[test]
    fn a_card_that_failed_at_seed_is_card_unavailable_and_a_tool_without_ui_is_silent() {
        let (session, _dir) = seeded("tc-bad", false);
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let run = fresh_run();
        attach_show_card(&store, &session, &run, "mail_list", "c1", FIT);
        attach_show_card(&store, &session, &run, "other_tool", "c2", FIT);
        let skipped = events(&store, &run, TYPE_CARD_SKIPPED);
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0]["data"]["reason"], "card_unavailable");
        assert!(events(&store, &run, TYPE_OUTBOX_CARD).is_empty());
    }

    #[test]
    fn changing_the_file_after_seed_does_not_change_the_card() {
        let (session, dir) = seeded("tc-fixed", true);
        std::fs::write(dir.path().join("mail/jigs/mail_list.json"), "{}").unwrap();
        let store = SqliteWorkflowStore::in_memory().unwrap();
        let run = fresh_run();
        attach_show_card(&store, &session, &run, "mail_list", "c1", FIT);
        let cards = events(&store, &run, TYPE_OUTBOX_CARD);
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0]["data"]["ir"]["root"], "root");
        assert_eq!(cards[0]["data"]["ir"]["nodes"]["root"]["type"], "List");
    }
}
