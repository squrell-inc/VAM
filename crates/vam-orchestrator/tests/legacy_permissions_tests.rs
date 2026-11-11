//! Legacy released agents (no `permissions` field) keep today's session policy.
//!
//! Characterization for AX-5.3a, before session assembly changes. Every agent
//! here is released without a `permissions` value. Agents with neither
//! `permissions` nor a legacy policy are inserted with
//! [`store_historical_release`]: draft publish (AX-5.6a) would fill
//! `bySideEffect` / `allow`. Agents that still go through [`release_agent`]
//! carry `capability_policy` or `hitl_policy`, which publish leaves unchanged.
//! AX-5.3c (pin enforcement) and AX-5.3b (session assembly) must keep the
//! non-ignored tests green.
//!
//! `resolve_session_policy` loads a capability policy by URN only: version and
//! digest on the ref are ignored, and the latest release wins. Composed Cedar
//! is `security::compose_policy` of that text plus the HITL and playbook overlays.
// `..Default::default()` kept so fixtures survive new fields; not changed in a lint wave.
#![allow(clippy::needless_update)]

use std::sync::Arc;

use prost_types::Struct;
use vam_inference::SessionBudget;
use vam_orchestrator::secrets::{broker_allowed_domains, SecretKind, SecretsVault};
use vam_orchestrator::security::compose_policy;
use vam_orchestrator::session::{builtin_tool_catalog, ToolCatalogEntry};
use vam_orchestrator::workflow::{PlaybookRunPin, SqliteWorkflowStore};
use vam_orchestrator::{
    derive_hitl_overlay, derive_playbook_overlay, resolve_session_policy, AttemptKey,
    PolicyResolutionKind, ResolvedSessionPolicy, SessionSecurityContext,
    DEFAULT_BUILTIN_FALLBACK_POLICY, LOCAL_USER_ID,
};
use vam_security::PolicyMode;
use vam_spec::v1::agent_spec::InstructionSource;
use vam_spec::v1::tool_intent_response::Disposition;
use vam_spec::v1::{
    AgentSpec, ArtifactRef, CapabilityPolicyRef, Digest, ExecutionLease, HitlPolicyRef,
    PlaybookSpec, PlaybookStep, ResourceId, ResourceRef, RunSpec, StepKind, StepMode, TaskOutcome,
    TaskOutcomeState, ToolIntentRequest, ToolIntentResponse, Version,
};

const JIRA_AGENT: &str = "urn:vam:agent:local.vam.dev/vam/jira-assistant";
const SQLITE_AGENT: &str = "urn:vam:agent:local.vam.dev/vam/sqlite-assistant";
const JIRA_POLICY: &str = "urn:vam:policy:local.vam.dev/vam/jira-egress-only";
const SQLITE_POLICY: &str = "urn:vam:policy:local.vam.dev/vam/sqlite-memory-only";
const JIRA_CEDAR: &str = include_str!("../../../examples/jira-assistant/policy.cedar");
const SQLITE_CEDAR: &str = include_str!("../../../examples/sqlite-assistant/policy.cedar");
const WORKSPACE_FILE: &str = "/mnt/drive9/workspace/notes.txt";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Effect {
    Allow,
    Deny,
    Suspend,
}

fn version(major: i32, minor: i32, patch: i32) -> Version {
    Version {
        major,
        minor,
        patch,
        ..Default::default()
    }
}

fn resource(urn: &str) -> ResourceId {
    ResourceId {
        urn: urn.to_string(),
        ..Default::default()
    }
}

fn floating_policy_ref(urn: &str) -> ResourceRef {
    ResourceRef {
        resource_id: Some(resource(urn)),
        ..Default::default()
    }
}

/// Released agent with no `permissions` value. A capability policy ref, when
/// present, is floating so release freezes version and digest.
fn legacy_agent(urn: &str, policy_urn: Option<&str>, monitored: &[&str]) -> AgentSpec {
    AgentSpec {
        resource_id: Some(resource(urn)),
        display_name: "Legacy agent".to_string(),
        description: "released before permissions".to_string(),
        role: "assistant".to_string(),
        instruction_source: Some(InstructionSource::SystemInstructions(
            "follow the legacy policy".to_string(),
        )),
        capability_policy: policy_urn.map(|policy| CapabilityPolicyRef {
            policy_ref: Some(floating_policy_ref(policy)),
            ..Default::default()
        }),
        hitl_policy: if monitored.is_empty() {
            None
        } else {
            Some(HitlPolicyRef {
                monitored_tools: monitored.iter().map(|tool| (*tool).to_string()).collect(),
                ..Default::default()
            })
        },
        ..Default::default()
    }
}

fn marker_policy(tool: &str) -> String {
    format!(
        r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"{tool}"
);
"#
    )
}

fn release_policy(store: &SqliteWorkflowStore, urn: &str, cedar: &str, at: Version) {
    store
        .publish_policy_draft_with_owner(urn, cedar, LOCAL_USER_ID)
        .unwrap_or_else(|err| panic!("publish policy {urn}: {err}"));
    store
        .release_policy(urn, &at)
        .unwrap_or_else(|err| panic!("release policy {urn}: {err}"));
}

fn release_agent(store: &SqliteWorkflowStore, spec: &AgentSpec, at: Version) -> AgentSpec {
    let urn = spec.resource_id.as_ref().unwrap().urn.clone();
    store
        .publish_agent_draft_with_owner(spec, LOCAL_USER_ID)
        .unwrap_or_else(|err| panic!("publish agent {urn}: {err}"));
    store
        .release_agent(&urn, &at)
        .unwrap_or_else(|err| panic!("release agent {urn}: {err}"))
}

/// Insert a released row exactly as given. Draft publish fills permission
/// defaults (AX-5.6a); these fixtures are historical and stay without
/// `permissions`.
fn store_historical_release(store: &SqliteWorkflowStore, spec: &AgentSpec) -> AgentSpec {
    assert!(
        spec.permissions.is_none(),
        "historical fixtures are released without permissions"
    );
    let stored = store
        .put_agent_released_with_owner(spec, LOCAL_USER_ID)
        .unwrap_or_else(|err| panic!("store historical release: {err}"));
    assert!(stored.permissions.is_none());
    stored
}

fn bind_run(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    agent_urn: &str,
    agent_version: Option<&Version>,
    agent_digest: Option<&str>,
    resolved_refs: Vec<ResourceRef>,
) {
    let spec = RunSpec {
        run_id: Some(resource(run_id)),
        agent_ref: Some(ResourceRef {
            resource_id: Some(resource(agent_urn)),
            version: agent_version.cloned(),
            digest: agent_digest.map(|hex| Digest {
                algorithm: 1,
                hex_hash: hex.to_string(),
                ..Default::default()
            }),
            ..Default::default()
        }),
        resolved_refs,
        ..Default::default()
    };
    let spec_json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("run json");
    store
        .persist_run_spec_with_owner(
            run_id,
            &spec_json,
            Some(session_id),
            agent_urn,
            None,
            "User::\"local\"",
            LOCAL_USER_ID,
        )
        .expect("bind run");
}

fn seed_catalog(store: &SqliteWorkflowStore, session_id: &str, extra: &[ToolCatalogEntry]) {
    store
        .create_session(
            session_id,
            None,
            "vm-legacy",
            "ws-legacy",
            "storage-default",
        )
        .expect("session");
    let mut catalog = builtin_tool_catalog();
    catalog.extend_from_slice(extra);
    store
        .record_tool_catalog(session_id, &catalog)
        .expect("catalog");
}

fn security_ctx(session_id: &str, agent_urn: &str, policy: &str) -> SessionSecurityContext {
    SessionSecurityContext::new(
        session_id.to_string(),
        agent_urn.to_string(),
        policy,
        PolicyMode::Enforce,
        1,
        ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
        None,
        Arc::new(SessionBudget::new(None)),
    )
    .expect("security context")
}

fn composed_cedar(
    store: &SqliteWorkflowStore,
    session_id: &str,
    agent_urn: &str,
    base: &str,
    monitored: &[String],
) -> String {
    let hitl = if monitored.is_empty() {
        String::new()
    } else {
        derive_hitl_overlay(agent_urn, monitored)
    };
    let playbook = derive_playbook_overlay(store, session_id).expect("playbook overlay");
    let overlay = compose_policy(&hitl, &playbook);
    // The outbox overlay (AR-3.9) is always last.
    let overlay = compose_policy(&overlay, vam_orchestrator::security::OUTBOX_OVERLAY.trim());
    compose_policy(base, &overlay)
}

fn assert_composed_like_today(
    store: &SqliteWorkflowStore,
    session_id: &str,
    agent_urn: &str,
    base: &str,
    monitored: &[String],
    resolved: &ResolvedSessionPolicy,
) {
    assert_eq!(
        resolved.cedar,
        composed_cedar(store, session_id, agent_urn, base, monitored),
        "session cedar must be compose_policy(base, hitl + playbook + outbox overlays)"
    );
}

fn string_fields(pairs: &[(&str, &str)]) -> Struct {
    let mut args = Struct::default();
    for (key, value) in pairs {
        args.fields.insert(
            (*key).to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue((*value).to_string())),
            },
        );
    }
    args
}

fn execute_args(tool: &str) -> Option<Struct> {
    match tool {
        "read_file" | "delete_file" | "glob" | "grep" => {
            Some(string_fields(&[("path", WORKSPACE_FILE)]))
        }
        "write_file" => Some(string_fields(&[
            ("path", WORKSPACE_FILE),
            ("content", "hello"),
        ])),
        "edit_file" => Some(string_fields(&[
            ("path", WORKSPACE_FILE),
            ("new_string", "hello"),
        ])),
        "todo_write" => Some(string_fields(&[("todos", "[]")])),
        "query_db" => Some(string_fields(&[("query", "SELECT id FROM facts")])),
        _ => None,
    }
}

fn issue_lease(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    n: u32,
) -> ExecutionLease {
    let lease = ExecutionLease {
        run_id: Some(resource(run_id)),
        step_id: format!("step-{n}"),
        attempt_id: Some(resource(&format!(
            "urn:vam:attempt:local.vam.dev/acme/{session_id}-{n}"
        ))),
        lease_id: Some(resource(&format!(
            "urn:vam:lease:local.vam.dev/acme/{session_id}-{n}"
        ))),
        lifecycle_generation: 1,
        fencing_token: vec![1, 2, 3],
        expires_at: Some(prost_types::Timestamp {
            seconds: 4_102_444_800,
            nanos: 0,
        }),
        idempotency_key: format!("idem-{session_id}-{n}"),
        ..Default::default()
    };
    store.issue_lease(&lease).expect("lease");
    lease
}

fn tool_request(
    lease: ExecutionLease,
    entry: &ToolCatalogEntry,
    arguments: Option<Struct>,
) -> ToolIntentRequest {
    let script_digest = if entry.digest.is_empty() {
        None
    } else {
        Some(Digest {
            algorithm: 1,
            hex_hash: entry.digest.clone(),
            ..Default::default()
        })
    };
    ToolIntentRequest {
        lease: Some(lease),
        tool_name: entry.tool_name.clone(),
        arguments,
        script_digest,
        ..Default::default()
    }
}

fn effect_of(response: &ToolIntentResponse) -> Effect {
    if response.disposition == Disposition::Authorized as i32 {
        assert!(
            response.capability_token.is_some(),
            "allow must mint a capability token: {response:?}"
        );
        Effect::Allow
    } else if response.disposition == Disposition::Suspended as i32 {
        assert!(
            response.approval_request_id.is_some(),
            "suspend must register an approval: {response:?}"
        );
        assert!(response.capability_token.is_none());
        Effect::Suspend
    } else if response.disposition == Disposition::Denied as i32 {
        assert!(response.capability_token.is_none());
        assert!(response.approval_request_id.is_none());
        Effect::Deny
    } else {
        panic!("unexpected disposition: {response:?}");
    }
}

fn catalog_with(extra: &[(&str, i32)]) -> Vec<ToolCatalogEntry> {
    let mut catalog = builtin_tool_catalog();
    for (name, side_effect) in extra {
        catalog.push(ToolCatalogEntry {
            tool_name: (*name).to_string(),
            command: "skill".to_string(),
            entrypoint: (*name).to_string(),
            digest: String::new(),
            side_effect: *side_effect,
            skill_name: String::new(),
            approval_required: false,
        });
    }
    catalog
}

fn assert_today_decisions(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    ctx: &SessionSecurityContext,
    catalog: &[ToolCatalogEntry],
    expect: impl Fn(&str) -> Effect,
) {
    let advertised = ctx.advertised_tool_names(store);
    let expected_advertised: Vec<String> = catalog
        .iter()
        .filter(|entry| expect(&entry.tool_name) != Effect::Deny)
        .map(|entry| entry.tool_name.clone())
        .collect();
    assert_eq!(
        advertised, expected_advertised,
        "advertised names are the Allow and Suspend tools, in catalog order"
    );

    for (n, entry) in catalog.iter().enumerate() {
        let response = ctx.handle_tool_intent_request(
            store,
            tool_request(
                issue_lease(store, session_id, run_id, n as u32),
                entry,
                execute_args(&entry.tool_name),
            ),
        );
        assert_eq!(
            effect_of(&response),
            expect(&entry.tool_name),
            "execute decision for {}",
            entry.tool_name
        );
    }
}

fn jira_effect(tool: &str) -> Effect {
    match tool {
        "read_file" | "glob" | "grep" | "load_skill" | "jira_search_issues"
        | "jira_add_comment" | "http_get" => Effect::Allow,
        "jira_create_issue" | "jira_transition_issue" | "http_write" => Effect::Suspend,
        _ => Effect::Deny,
    }
}

fn sqlite_effect(tool: &str) -> Effect {
    match tool {
        "read_file" | "write_file" | "edit_file" | "glob" | "run" | "todo_write" | "load_skill"
        | "init_db" | "insert_fact" | "insert_code_note" | "query_db" => Effect::Allow,
        _ => Effect::Deny,
    }
}

fn frozen_policy_ref(agent: &AgentSpec) -> ResourceRef {
    agent
        .capability_policy
        .as_ref()
        .and_then(|policy| policy.policy_ref.clone())
        .expect("frozen capability policy")
}

#[test]
fn released_jira_policy_matches_today_advertise_and_execute() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    release_policy(&store, JIRA_POLICY, JIRA_CEDAR, version(1, 0, 0));
    let released = release_agent(
        &store,
        &legacy_agent(JIRA_AGENT, Some(JIRA_POLICY), &[]),
        version(1, 0, 0),
    );
    let policy = store.get_policy(JIRA_POLICY).expect("policy");
    assert_eq!(policy.cedar, JIRA_CEDAR);
    let pin = frozen_policy_ref(&released);
    assert_eq!(pin.version, Some(version(1, 0, 0)));
    assert_eq!(
        pin.digest.as_ref().map(|digest| digest.hex_hash.as_str()),
        Some(policy.digest.as_str()),
        "release freezes the policy digest; resolution still loads by URN"
    );

    let session_id = "sess-jira-legacy";
    let run_id = "urn:vam:run:local.vam.dev/acme/jira-legacy";
    let extra = [
        ("jira_search_issues", 1),
        ("jira_add_comment", 1),
        ("http_get", 1),
        ("jira_create_issue", 3),
        ("jira_transition_issue", 3),
        ("http_write", 3),
    ];
    let catalog = catalog_with(&extra);
    seed_catalog(&store, session_id, &catalog[builtin_tool_catalog().len()..]);
    let agent_pin = store
        .catalog_pin_at_version(
            "agents",
            JIRA_AGENT,
            &version(1, 0, 0),
            LOCAL_USER_ID,
            false,
        )
        .expect("agent pin");
    bind_run(
        &store,
        session_id,
        run_id,
        JIRA_AGENT,
        Some(&agent_pin.version),
        Some(&agent_pin.digest),
        vec![pin],
    );

    let resolved = resolve_session_policy(&store, session_id, Some(JIRA_AGENT)).expect("resolve");
    assert_eq!(resolved.kind, PolicyResolutionKind::Catalog);
    assert!(resolved.warning.is_none());
    assert_composed_like_today(&store, session_id, JIRA_AGENT, JIRA_CEDAR, &[], &resolved);

    let ctx = security_ctx(session_id, JIRA_AGENT, &resolved.cedar);
    assert_today_decisions(&store, session_id, run_id, &ctx, &catalog, jira_effect);
}

#[test]
fn released_sqlite_policy_matches_today_advertise_and_execute() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    release_policy(&store, SQLITE_POLICY, SQLITE_CEDAR, version(1, 0, 0));
    release_agent(
        &store,
        &legacy_agent(SQLITE_AGENT, Some(SQLITE_POLICY), &[]),
        version(1, 0, 0),
    );
    let policy = store.get_policy(SQLITE_POLICY).expect("policy");
    assert_eq!(policy.cedar, SQLITE_CEDAR);

    let session_id = "sess-sqlite-legacy";
    let run_id = "urn:vam:run:local.vam.dev/acme/sqlite-legacy";
    let extra = [
        ("init_db", 2),
        ("insert_fact", 2),
        ("insert_code_note", 2),
        ("query_db", 1),
    ];
    let catalog = catalog_with(&extra);
    seed_catalog(&store, session_id, &catalog[builtin_tool_catalog().len()..]);
    let agent_pin = store
        .catalog_pin_at_version(
            "agents",
            SQLITE_AGENT,
            &version(1, 0, 0),
            LOCAL_USER_ID,
            false,
        )
        .expect("agent pin");
    bind_run(
        &store,
        session_id,
        run_id,
        SQLITE_AGENT,
        Some(&agent_pin.version),
        Some(&agent_pin.digest),
        vec![frozen_policy_ref(
            &store.get_agent(SQLITE_AGENT).expect("agent"),
        )],
    );

    let resolved = resolve_session_policy(&store, session_id, Some(SQLITE_AGENT)).expect("resolve");
    assert_eq!(resolved.kind, PolicyResolutionKind::Catalog);
    assert_composed_like_today(
        &store,
        session_id,
        SQLITE_AGENT,
        SQLITE_CEDAR,
        &[],
        &resolved,
    );

    let ctx = security_ctx(session_id, SQLITE_AGENT, &resolved.cedar);
    assert_today_decisions(&store, session_id, run_id, &ctx, &catalog, sqlite_effect);

    let query_db = catalog
        .iter()
        .find(|entry| entry.tool_name == "query_db")
        .expect("query_db");
    let dropped = ctx.handle_tool_intent_request(
        &store,
        tool_request(
            issue_lease(&store, session_id, run_id, 100),
            query_db,
            Some(string_fields(&[("query", "DROP TABLE facts")])),
        ),
    );
    assert_eq!(
        effect_of(&dropped),
        Effect::Deny,
        "DROP is denied at execute time even though query_db is advertised"
    );
}

#[test]
fn monitored_tools_suspend_until_a_grant_then_execute() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    release_policy(&store, SQLITE_POLICY, SQLITE_CEDAR, version(1, 0, 0));
    let agent_urn = SQLITE_AGENT;
    release_agent(
        &store,
        &legacy_agent(agent_urn, Some(SQLITE_POLICY), &["insert_fact"]),
        version(1, 0, 0),
    );

    let session_id = "sess-legacy-hitl";
    let run_id = "urn:vam:run:local.vam.dev/acme/legacy-hitl";
    let insert_fact = ToolCatalogEntry {
        tool_name: "insert_fact".to_string(),
        command: "skill".to_string(),
        entrypoint: "insert_fact".to_string(),
        digest: String::new(),
        side_effect: 2,
        skill_name: String::new(),
        approval_required: false,
    };
    seed_catalog(&store, session_id, std::slice::from_ref(&insert_fact));
    bind_run(&store, session_id, run_id, agent_urn, None, None, vec![]);

    let monitored = vec!["insert_fact".to_string()];
    let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).expect("resolve");
    assert_eq!(resolved.monitored_tools, monitored);
    assert_composed_like_today(
        &store,
        session_id,
        agent_urn,
        SQLITE_CEDAR,
        &monitored,
        &resolved,
    );

    let ctx = security_ctx(session_id, agent_urn, &resolved.cedar);
    let advertised = ctx.advertised_tool_names(&store);
    assert!(advertised.iter().any(|name| name == "insert_fact"));

    let lease = issue_lease(&store, session_id, run_id, 1);
    let suspended =
        ctx.handle_tool_intent_request(&store, tool_request(lease.clone(), &insert_fact, None));
    assert_eq!(effect_of(&suspended), Effect::Suspend);

    let request_id = suspended.approval_request_id.clone().expect("approval");
    let key = AttemptKey {
        run_id: run_id.to_string(),
        step_id: lease.step_id.clone(),
        attempt_id: lease.attempt_id.as_ref().unwrap().urn.clone(),
    };
    let checkpoint = ArtifactRef {
        artifact_id: Some(resource(
            "urn:vam:artifact:local.vam.dev/acme/legacy-hitl-checkpoint",
        )),
        digest: Some(Digest {
            algorithm: 1,
            hex_hash: "abc123".to_string(),
            ..Default::default()
        }),
        media_type: "application/json".to_string(),
        size_bytes: 2,
        uri: "memory://checkpoint".to_string(),
        ..Default::default()
    };
    store
        .record_outcome(&TaskOutcome {
            lease: Some(lease.clone()),
            state: TaskOutcomeState::Suspended as i32,
            continuation: Some(checkpoint.clone()),
            ..Default::default()
        })
        .expect("record suspend");
    store
        .create_approval(&key, &request_id, &checkpoint, 1)
        .expect("publish approval");
    store
        .decide_approval(
            &request_id,
            &resource("urn:vam:token:local.vam.dev/acme/legacy-hitl-grant"),
            "APPROVED",
            "User::\"local\"",
            LOCAL_USER_ID,
            None,
            None,
            None,
            None,
        )
        .expect("grant");

    let authorized =
        ctx.handle_tool_intent_request(&store, tool_request(lease, &insert_fact, None));
    assert_eq!(
        effect_of(&authorized),
        Effect::Allow,
        "a grant lets the monitored tool execute"
    );
}

#[test]
fn legacy_agent_without_capability_policy_uses_builtins_and_does_not_ask() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/legacy-builtins";
    store_historical_release(&store, &legacy_agent(agent_urn, None, &[]));

    let session_id = "sess-legacy-builtins";
    let run_id = "urn:vam:run:local.vam.dev/acme/legacy-builtins";
    let mcp = ToolCatalogEntry {
        tool_name: "mcp_create_issue".to_string(),
        command: "mcp".to_string(),
        entrypoint: "create_issue".to_string(),
        digest: String::new(),
        side_effect: vam_spec::v1::SideEffectClass::MutableExternal as i32,
        skill_name: String::new(),
        approval_required: false,
    };
    seed_catalog(&store, session_id, std::slice::from_ref(&mcp));
    bind_run(&store, session_id, run_id, agent_urn, None, None, vec![]);

    let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).expect("resolve");
    assert_eq!(resolved.kind, PolicyResolutionKind::NoPolicyDeclared);
    assert!(resolved.warning.is_none());
    assert!(resolved.policy_urn.is_none());
    assert_composed_like_today(
        &store,
        session_id,
        agent_urn,
        DEFAULT_BUILTIN_FALLBACK_POLICY,
        &[],
        &resolved,
    );

    let ctx = security_ctx(session_id, agent_urn, &resolved.cedar);
    let advertised = ctx.advertised_tool_names(&store);
    assert_eq!(
        advertised,
        vam_security::BUILTIN_TOOL_NAMES
            .iter()
            .map(|name| (*name).to_string())
            .collect::<Vec<_>>()
    );
    assert!(
        !advertised.iter().any(|name| name == "mcp_create_issue"),
        "a mutable external tool is not advertised for a legacy agent"
    );

    let response = ctx.handle_tool_intent_request(
        &store,
        tool_request(issue_lease(&store, session_id, run_id, 1), &mcp, None),
    );
    assert_eq!(
        effect_of(&response),
        Effect::Deny,
        "no bySideEffect default asks before a mutable external tool"
    );
}

#[test]
fn declared_but_missing_policy_falls_back_with_warning() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/legacy-missing-policy";
    let missing = "urn:vam:policy:local.vam.dev/acme/gone";
    // A release refuses a missing policy. The row below is a released agent whose
    // policy disappeared afterwards (or was stored before the policy existed).
    store
        .put_agent_released_with_owner(&legacy_agent(agent_urn, Some(missing), &[]), LOCAL_USER_ID)
        .expect("released agent");

    let session_id = "sess-legacy-missing";
    let run_id = "urn:vam:run:local.vam.dev/acme/legacy-missing";
    store
        .create_session(
            session_id,
            None,
            "vm-legacy",
            "ws-legacy",
            "storage-default",
        )
        .expect("session");
    bind_run(&store, session_id, run_id, agent_urn, None, None, vec![]);

    let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).expect("resolve");
    assert_eq!(resolved.kind, PolicyResolutionKind::DeclaredButMissing);
    assert_eq!(resolved.policy_urn.as_deref(), Some(missing));
    assert_eq!(
        resolved.warning.as_deref(),
        Some(format!(
            "declared capability policy '{missing}' is missing from the catalog; using builtins-only"
        ).as_str())
    );
    assert_composed_like_today(
        &store,
        session_id,
        agent_urn,
        DEFAULT_BUILTIN_FALLBACK_POLICY,
        &[],
        &resolved,
    );
}

#[test]
fn pre_phase3_policy_ref_without_digest_loads_latest_release() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/legacy-undigested";
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/legacy-undigested";
    release_policy(
        &store,
        policy_urn,
        &marker_policy("legacy_policy_a"),
        version(1, 0, 0),
    );
    release_policy(
        &store,
        policy_urn,
        &marker_policy("legacy_policy_b"),
        version(1, 1, 0),
    );

    let mut agent = legacy_agent(agent_urn, None, &[]);
    agent.capability_policy = Some(CapabilityPolicyRef {
        policy_ref: Some(ResourceRef {
            resource_id: Some(resource(policy_urn)),
            version: Some(version(1, 0, 0)),
            digest: None,
            ..Default::default()
        }),
        ..Default::default()
    });
    let stored = store
        .put_agent_released_with_owner(&agent, LOCAL_USER_ID)
        .expect("pre-phase-3 agent");
    let pref = frozen_policy_ref(&stored);
    assert_eq!(pref.version, Some(version(1, 0, 0)));
    assert!(
        pref.digest.is_none(),
        "pre-Phase-3 ref carries a version and no digest"
    );

    let session_id = "sess-legacy-undigested";
    let run_id = "urn:vam:run:local.vam.dev/acme/legacy-undigested";
    store
        .create_session(
            session_id,
            None,
            "vm-legacy",
            "ws-legacy",
            "storage-default",
        )
        .expect("session");
    bind_run(&store, session_id, run_id, agent_urn, None, None, vec![]);

    let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).expect("resolve");
    assert_eq!(resolved.kind, PolicyResolutionKind::Catalog);
    let latest = store.get_policy(policy_urn).expect("latest");
    assert!(latest.cedar.contains("legacy_policy_b"));
    assert_composed_like_today(&store, session_id, agent_urn, &latest.cedar, &[], &resolved);
    assert!(
        !resolved.cedar.contains("legacy_policy_a"),
        "a ref with no digest loads the latest release, not the named 1.0.0"
    );
}

#[test]
fn legacy_egress_allowlist_is_empty_or_the_union_of_secret_destinations() {
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/legacy-egress";
    store_historical_release(&store, &legacy_agent(agent_urn, None, &[]));

    let vault = SecretsVault::for_tests(Arc::clone(&store));
    let empty = vault.session_egress_bindings().expect("no secrets");
    assert!(
        empty.is_empty(),
        "a legacy agent with no secrets contributes no destinations"
    );
    assert!(
        broker_allowed_domains(&empty).is_empty(),
        "the broker allowlist is empty when the agent has no secrets"
    );

    vault
        .put_secret(
            "alpha/token",
            "alpha-secret",
            SecretKind::Egress,
            &["alpha.example".to_string(), "shared.example".to_string()],
            None,
        )
        .expect("alpha");
    vault
        .put_secret(
            "beta/token",
            "beta-secret",
            SecretKind::Egress,
            &["beta.example".to_string(), "shared.example".to_string()],
            None,
        )
        .expect("beta");
    let bindings = vault
        .session_egress_bindings_for_caller(LOCAL_USER_ID)
        .expect("two secrets");
    assert_eq!(
        broker_allowed_domains(&bindings),
        vec![
            "alpha.example".to_string(),
            "shared.example".to_string(),
            "beta.example".to_string(),
        ],
        "two secrets union their allowed_destinations, first-seen order"
    );
}

#[test]
fn legacy_agent_with_default_playbook_appends_playbook_overlay() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    let playbook_urn = "urn:vam:playbook:local.vam.dev/acme/legacy-default";
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/legacy-playbook";
    let playbook = PlaybookSpec {
        activation: Some(vam_spec::v1::PlaybookActivation::asking(
            "Use when the request matches this test playbook.",
        )),
        resource_id: Some(resource(playbook_urn)),
        display_name: "Legacy default".to_string(),
        description: "default playbook on a legacy agent".to_string(),
        steps: vec![PlaybookStep {
            id: "work".to_string(),
            instruction: "do the work".to_string(),
            kind: StepKind::Task as i32,
            mode: StepMode::Agentic as i32,
            ..Default::default()
        }],
        ..Default::default()
    };
    store
        .publish_playbook_draft_with_owner(&playbook, LOCAL_USER_ID)
        .expect("playbook draft");
    store
        .release_playbook(playbook_urn, &version(1, 0, 0))
        .expect("playbook release");

    let pin = store
        .latest_released_catalog_pin("playbooks", playbook_urn)
        .expect("playbook pin");
    let mut agent = legacy_agent(agent_urn, None, &[]);
    agent.default_playbook_ref = Some(ResourceRef {
        resource_id: Some(resource(playbook_urn)),
        version: Some(pin.version),
        digest: Some(Digest {
            algorithm: 1,
            hex_hash: pin.digest,
        }),
        ..Default::default()
    });
    let released = store_historical_release(&store, &agent);
    let playbook_pin = released
        .default_playbook_ref
        .expect("frozen default playbook");
    assert!(playbook_pin.digest.is_some());
    assert_eq!(playbook_pin.version, Some(version(1, 0, 0)));

    let session_id = "sess-legacy-playbook";
    let run_id = "urn:vam:run:local.vam.dev/acme/legacy-playbook";
    store
        .create_session(
            session_id,
            None,
            "vm-legacy",
            "ws-legacy",
            "storage-default",
        )
        .expect("session");
    bind_run(&store, session_id, run_id, agent_urn, None, None, vec![]);
    let pinned_version = playbook_pin.version.as_ref().unwrap();
    store
        .init_playbook_run(
            run_id,
            &PlaybookRunPin {
                urn: playbook_urn.to_string(),
                version: format!(
                    "{}.{}.{}",
                    pinned_version.major, pinned_version.minor, pinned_version.patch
                ),
                digest: playbook_pin.digest.unwrap().hex_hash,
            },
            &store.get_playbook(playbook_urn).expect("playbook"),
        )
        .expect("playbook run");

    let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).expect("resolve");
    assert_eq!(resolved.kind, PolicyResolutionKind::NoPolicyDeclared);
    assert_composed_like_today(
        &store,
        session_id,
        agent_urn,
        DEFAULT_BUILTIN_FALLBACK_POLICY,
        &[],
        &resolved,
    );
    assert!(resolved.cedar.contains("Tool::\"step_complete\""));
    assert!(resolved.cedar.contains("Tool::\"run_step\""));
    assert!(resolved.cedar.contains("Tool::\"check_playbook_finish\""));
}

/// Release a policy, freeze it into the agent, then rewrite that row's Cedar.
/// Today's resolver returns the tampered text. AX-5.3c must fail closed (DX-6A).
#[test]
fn tampered_pinned_policy_fails_closed() {
    let dir = tempfile::tempdir().expect("db dir");
    let path = dir.path().join("legacy.db");
    let store = SqliteWorkflowStore::open(&path).expect("store");
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/legacy-tamper";
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/legacy-tamper";
    let original = marker_policy("legacy_untampered");
    release_policy(&store, policy_urn, &original, version(1, 0, 0));
    let released = release_agent(
        &store,
        &legacy_agent(agent_urn, Some(policy_urn), &[]),
        version(1, 0, 0),
    );
    let pin = frozen_policy_ref(&released);
    assert!(pin.digest.is_some());
    let original_digest = store.get_policy(policy_urn).expect("policy").digest;

    let tampered = marker_policy("legacy_tampered");
    let conn = rusqlite::Connection::open(&path).expect("second connection");
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .expect("busy timeout");
    let updated = conn
        .execute(
            "UPDATE policies SET cedar = ?1
             WHERE urn = ?2 AND major = 1 AND minor = 0 AND patch = 0
               AND prerelease = '' AND channel = 'released'",
            rusqlite::params![tampered, policy_urn],
        )
        .expect("tamper cedar");
    assert_eq!(updated, 1, "the frozen 1.0.0 policy row must change");

    let loaded = store.get_policy(policy_urn).expect("reloaded");
    assert!(loaded.cedar.contains("legacy_tampered"));
    assert_eq!(
        loaded.digest, original_digest,
        "the digest column stays at the pin; the Cedar bytes changed"
    );

    let session_id = "sess-legacy-tamper";
    let run_id = "urn:vam:run:local.vam.dev/acme/legacy-tamper";
    store
        .create_session(
            session_id,
            None,
            "vm-legacy",
            "ws-legacy",
            "storage-default",
        )
        .expect("session");
    bind_run(
        &store,
        session_id,
        run_id,
        agent_urn,
        Some(&version(1, 0, 0)),
        None,
        vec![pin],
    );

    let result = resolve_session_policy(&store, session_id, Some(agent_urn));
    assert!(
        result.is_err(),
        "tampered pinned policy must fail closed, got {result:?}"
    );
}

/// Agent 1.0.0 freezes policy A; agent 1.1.0 freezes policy B. The session's
/// `RunSpec.agent_ref` is 1.0.0.
///
/// Today's `resolve_session_policy` loads the latest release (agent 1.1.0 /
/// policy B) and ignores that pin. AX-5.3c must use policy A.
#[test]
fn session_pinned_to_agent_1_0_0_uses_policy_a() {
    let store = SqliteWorkflowStore::in_memory().expect("store");
    let policy_urn = "urn:vam:policy:local.vam.dev/acme/legacy-pinned";
    let agent_urn = "urn:vam:agent:local.vam.dev/acme/legacy-pinned";
    release_policy(
        &store,
        policy_urn,
        &marker_policy("legacy_policy_a"),
        version(1, 0, 0),
    );
    release_agent(
        &store,
        &legacy_agent(agent_urn, Some(policy_urn), &[]),
        version(1, 0, 0),
    );
    release_policy(
        &store,
        policy_urn,
        &marker_policy("legacy_policy_b"),
        version(1, 1, 0),
    );
    release_agent(
        &store,
        &legacy_agent(agent_urn, Some(policy_urn), &[]),
        version(1, 1, 0),
    );

    let v100 = version(1, 0, 0);
    let agent_v100 = store
        .get_agent_at_version(agent_urn, &v100)
        .expect("agent 1.0.0");
    let policy_a = frozen_policy_ref(&agent_v100);
    assert!(
        store
            .get_policy(policy_urn)
            .expect("latest")
            .cedar
            .contains("legacy_policy_b"),
        "latest release is policy B"
    );
    let agent_pin = store
        .catalog_pin_at_version("agents", agent_urn, &v100, LOCAL_USER_ID, false)
        .expect("1.0.0 pin");

    let session_id = "sess-legacy-pinned";
    let run_id = "urn:vam:run:local.vam.dev/acme/legacy-pinned";
    store
        .create_session(
            session_id,
            None,
            "vm-legacy",
            "ws-legacy",
            "storage-default",
        )
        .expect("session");
    bind_run(
        &store,
        session_id,
        run_id,
        agent_urn,
        Some(&agent_pin.version),
        Some(&agent_pin.digest),
        vec![policy_a],
    );

    let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).expect("resolve");
    assert!(
        resolved.cedar.contains("Tool::\"legacy_policy_a\""),
        "session pinned to agent 1.0.0 must use policy A; today's code loads the latest release. cedar:\n{}",
        resolved.cedar
    );
    assert!(
        !resolved.cedar.contains("legacy_policy_b"),
        "policy B is the latest release and must not be used for a 1.0.0 pin"
    );
}
