//! Compatibility path for released agents that still carry `capability_policy`
//! and `hitl_policy`.
//!
//! Resolves the session's capability policy the way it always did (a catalog
//! row by URN, else builtins-only, plus the HITL and playbook overlays), with
//! two changes (AX-5.3c, DX-6A):
//!
//! * the agent is the one the run pinned, not the latest release;
//! * a policy ref that carries a version and digest is loaded at exactly that
//!   version, and the digest is recomputed from the stored Cedar. A missing
//!   version or a digest mismatch fails closed.
//!
//! A ref with no digest (pre-Phase-3) still loads the latest release, with a
//! warning recorded on the run.

use crate::catalog::policy_cedar_digest;
use crate::catalog_resolve::{is_unpinned_digest, run_resolve_context};
use crate::error::OrchestratorError;
use crate::security::{
    capability_policy_urn, compose_policy, derive_hitl_overlay, derive_playbook_overlay,
    monitored_tools_from_agent, session_agent_urn, PolicyResolutionKind, ResolvedSessionPolicy,
    DEFAULT_BUILTIN_FALLBACK_POLICY, EVENT_POLICY_RESOLUTION,
};
use crate::workflow::SqliteWorkflowStore;
use vam_spec::v1::{AgentSpec, ResourceRef, RunSpec, Version};

/// The agent a session runs, at the version its run pinned.
#[derive(Debug, Clone)]
pub(crate) struct SessionAgent {
    /// `None` when the session names no agent or the agent is not in the catalog
    /// and the run recorded no pin.
    pub agent: Option<AgentSpec>,
    /// The run's `agent_ref` is a `-draft` pin.
    pub draft_run: bool,
    /// Whose drafts this session may load: the agent row's owner on a draft run,
    /// else the session owner. Released rows are visible to everyone.
    pub resolve_owner: String,
    /// The run's spec, when it has one.
    pub run_spec: Option<RunSpec>,
}

/// Load the session's run spec. A stored spec that does not parse is corrupt
/// state, never "no pin".
fn load_run_spec(
    store: &SqliteWorkflowStore,
    session_id: &str,
) -> Result<Option<RunSpec>, OrchestratorError> {
    let Some(run_id) = store.find_run_id_for_session(session_id)? else {
        return Ok(None);
    };
    let (spec_json, _, _) = store.get_run_spec_context(&run_id)?;
    let Some(spec_json) = spec_json.filter(|json| !json.trim().is_empty()) else {
        return Ok(None);
    };
    vam_spec::encoding::json_to_proto::<RunSpec>("vam.v1.RunSpec", &spec_json)
        .map(Some)
        .map_err(|e| {
            OrchestratorError::CorruptState(format!("run '{run_id}' spec does not parse: {e}"))
        })
}

/// The version the run pinned for `agent_urn`, if its `agent_ref` names that agent.
fn pinned_agent_version<'a>(run_spec: &'a RunSpec, agent_urn: &str) -> Option<&'a Version> {
    let agent_ref = run_spec.agent_ref.as_ref()?;
    let names_agent = agent_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .filter(|urn| !urn.is_empty())
        .is_none_or(|urn| urn == agent_urn);
    if names_agent {
        agent_ref.version.as_ref()
    } else {
        None
    }
}

/// Load the agent a session runs at the version its run pinned: the owner's
/// draft for a draft run, the exact release otherwise. A pin that cannot be
/// loaded is an error, never a fall back to the latest release. A run with no
/// recorded pin keeps the old lookup: latest release, else the session owner's
/// draft, else the catalog's latest release for any owner.
pub(crate) fn load_session_agent(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
) -> Result<SessionAgent, OrchestratorError> {
    let agent_urn = session_agent_urn(store, session_id, principal_urn)?;
    let session_owner = store
        .session_owner_user_id(session_id)
        .unwrap_or_else(|_| crate::users::LOCAL_USER_ID.to_string());
    let run_spec = load_run_spec(store, session_id)?;

    let Some(urn) = agent_urn.as_deref() else {
        return Ok(SessionAgent {
            agent: None,
            draft_run: false,
            resolve_owner: session_owner,
            run_spec,
        });
    };

    let pinned = run_spec
        .as_ref()
        .and_then(|spec| pinned_agent_version(spec, urn).map(|version| (spec, version)));
    if let Some((spec, version)) = pinned {
        let draft_run = version.prerelease == "draft";
        let resolve_owner = if draft_run {
            run_resolve_context(store, spec, &session_owner).0
        } else {
            session_owner
        };
        let agent = store.get_agent_at_version_visible(urn, version, &resolve_owner, false)?;
        return Ok(SessionAgent {
            agent: Some(agent),
            draft_run,
            resolve_owner,
            run_spec,
        });
    }

    let agent = match store.get_agent_visible(urn, &session_owner, false) {
        Ok(spec) => Some(spec),
        Err(OrchestratorError::NotFound(_)) => match store.get_agent(urn) {
            Ok(spec) => Some(spec),
            Err(OrchestratorError::NotFound(_)) => None,
            Err(e) => return Err(e),
        },
        Err(e) => return Err(e),
    };
    Ok(SessionAgent {
        agent,
        draft_run: false,
        resolve_owner: session_owner,
        run_spec,
    })
}

fn version_label(version: &Version) -> String {
    if version.prerelease.is_empty() {
        format!("{}.{}.{}", version.major, version.minor, version.patch)
    } else {
        format!(
            "{}.{}.{}-{}",
            version.major, version.minor, version.patch, version.prerelease
        )
    }
}

/// A ref that names an exact version and carries a real digest.
struct PolicyPin {
    version: Version,
    digest: String,
}

fn pin_of(policy_ref: &ResourceRef) -> Option<PolicyPin> {
    let version = policy_ref.version.clone()?;
    let digest = policy_ref
        .digest
        .as_ref()
        .map(|d| d.hex_hash.clone())
        .filter(|hex| !is_unpinned_digest(hex))?;
    Some(PolicyPin { version, digest })
}

/// The pin for `policy_urn`: the one the run recorded in `resolved_refs`, else the
/// agent's own frozen ref. `None` when neither carries a version and a digest.
fn policy_pin(
    agent: &AgentSpec,
    run_spec: Option<&RunSpec>,
    policy_urn: &str,
) -> Option<PolicyPin> {
    let names_policy =
        |r: &&ResourceRef| r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(policy_urn);
    let recorded = run_spec.and_then(|spec| {
        spec.resolved_refs
            .iter()
            .filter(names_policy)
            .find_map(pin_of)
    });
    recorded.or_else(|| {
        agent
            .capability_policy
            .as_ref()
            .and_then(|cp| cp.policy_ref.as_ref())
            .filter(names_policy)
            .and_then(pin_of)
    })
}

/// Why a pinned policy could not be loaded.
enum PinFailure {
    MissingVersion,
    DigestMismatch { actual: String },
}

fn pin_failure_message(policy_urn: &str, pin: &PolicyPin, failure: &PinFailure) -> String {
    let version = version_label(&pin.version);
    match failure {
        PinFailure::MissingVersion => format!(
            "pinned capability policy '{policy_urn}' version {version} is missing from the catalog \
             (expected digest {}); refusing to start the session",
            pin.digest
        ),
        PinFailure::DigestMismatch { actual } => format!(
            "pinned capability policy '{policy_urn}' version {version} failed digest verification: \
             expected digest {}, stored Cedar hashes to {actual}; refusing to start the session",
            pin.digest
        ),
    }
}

/// Record a fail-closed pin violation on the session's run outbox, next to the
/// other policy-resolution events, then return the error that stops the session.
fn fail_closed(
    store: &SqliteWorkflowStore,
    session_id: &str,
    agent_urn: Option<&str>,
    policy_urn: &str,
    pin: &PolicyPin,
    failure: &PinFailure,
) -> OrchestratorError {
    let message = pin_failure_message(policy_urn, pin, failure);
    eprintln!(
        "[orchestrator] ERROR session {session_id} agent={agent_urn:?} policy_urn={policy_urn}: {message}"
    );
    let actual = match failure {
        PinFailure::MissingVersion => None,
        PinFailure::DigestMismatch { actual } => Some(actual.as_str()),
    };
    let payload = serde_json::json!({
        "message": message,
        "kind": "PinViolation",
        "policy_urn": policy_urn,
        "policy_version": version_label(&pin.version),
        "expected_digest": pin.digest,
        "actual_digest": actual,
        "agent_urn": agent_urn,
    })
    .to_string();
    match store.find_run_id_for_session(session_id) {
        Ok(Some(run_id)) => {
            if let Err(e) = store.append_outbox_event(&run_id, EVENT_POLICY_RESOLUTION, &payload) {
                eprintln!(
                    "[orchestrator] failed to record policy pin violation on run {run_id}: {e}"
                );
            }
        }
        Ok(None) => {}
        Err(e) => eprintln!("[orchestrator] failed to find run for session {session_id}: {e}"),
    }
    OrchestratorError::CorruptState(format!("policy pin violation: {message}"))
}

/// Load the pinned policy row and recompute its digest from the stored Cedar.
/// The `digest` column is never trusted: a rewritten Cedar column fails here.
fn load_pinned_policy(
    store: &SqliteWorkflowStore,
    session: &SessionAgent,
    session_id: &str,
    policy_urn: &str,
    pin: &PolicyPin,
) -> Result<String, OrchestratorError> {
    let agent_urn = session
        .agent
        .as_ref()
        .and_then(|a| a.resource_id.as_ref())
        .map(|id| id.urn.as_str());
    let record =
        match store.get_policy_at_version_visible(policy_urn, &pin.version, &session.resolve_owner)
        {
            Ok(record) => record,
            Err(OrchestratorError::NotFound(_)) => {
                return Err(fail_closed(
                    store,
                    session_id,
                    agent_urn,
                    policy_urn,
                    pin,
                    &PinFailure::MissingVersion,
                ))
            }
            Err(e) => return Err(e),
        };
    let actual = policy_cedar_digest(&record.cedar);
    if !actual.eq_ignore_ascii_case(&pin.digest) {
        return Err(fail_closed(
            store,
            session_id,
            agent_urn,
            policy_urn,
            pin,
            &PinFailure::DigestMismatch { actual },
        ));
    }
    Ok(record.cedar)
}

/// Resolve Cedar source for a session whose agent declares `capability_policy`
/// (or none): catalog row, else builtins-only, plus HITL and playbook overlays.
/// In-flight sessions do not hot-reload; policy is fixed at broker spawn time.
pub(crate) fn resolve_legacy_session_policy(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
) -> Result<ResolvedSessionPolicy, OrchestratorError> {
    let agent_urn = session_agent_urn(store, session_id, principal_urn)?;
    let session = load_session_agent(store, session_id, principal_urn)?;
    let agent = session.agent.as_ref();

    let monitored = agent.map(monitored_tools_from_agent).unwrap_or_default();
    let skill_ref_count = agent.map(|a| a.skill_refs.len()).unwrap_or(0);
    let declared_urn = agent.and_then(capability_policy_urn);

    let (base, kind, warning) = match (declared_urn.as_deref(), agent) {
        (Some(urn), Some(agent)) => {
            match policy_pin(agent, session.run_spec.as_ref(), urn) {
                Some(pin) => (
                    load_pinned_policy(store, &session, session_id, urn, &pin)?,
                    PolicyResolutionKind::Catalog,
                    None,
                ),
                None => {
                    // A draft run resolves a floating ref when the run is created.
                    let floats_by_design = session.draft_run
                        && agent
                            .capability_policy
                            .as_ref()
                            .and_then(|cp| cp.policy_ref.as_ref())
                            .is_some_and(|r| r.version.is_none());
                    resolve_unpinned_policy(store, &session, urn, floats_by_design)?
                }
            }
        }
        _ => (
            DEFAULT_BUILTIN_FALLBACK_POLICY.to_string(),
            PolicyResolutionKind::NoPolicyDeclared,
            None,
        ),
    };

    let hitl_overlay = match agent_urn.as_deref() {
        Some(urn) if !monitored.is_empty() => derive_hitl_overlay(urn, &monitored),
        _ => String::new(),
    };
    let playbook_overlay = derive_playbook_overlay(store, session_id)?;
    let overlay = compose_policy(&hitl_overlay, &playbook_overlay);
    let overlay = compose_policy(&overlay, crate::security::OUTBOX_OVERLAY.trim());
    let cedar = compose_policy(&base, &overlay);

    Ok(ResolvedSessionPolicy {
        cedar,
        mode: crate::security::SessionPolicyMode::Legacy,
        kind,
        policy_urn: declared_urn,
        monitored_tools: monitored,
        skill_ref_count,
        warning,
    })
}

/// A ref with no version or no digest (pre-Phase-3) follows the catalog: the
/// latest release, else the owner's draft. A policy the catalog lacks falls back
/// to builtins-only with a warning, as before.
fn resolve_unpinned_policy(
    store: &SqliteWorkflowStore,
    session: &SessionAgent,
    urn: &str,
    floats_by_design: bool,
) -> Result<(String, PolicyResolutionKind, Option<String>), OrchestratorError> {
    let owner = &session.resolve_owner;
    let record = match store.get_policy_visible(urn, owner) {
        Ok(record) => Some(record),
        Err(OrchestratorError::NotFound(_)) => match store.get_policy(urn) {
            Ok(record) => Some(record),
            Err(OrchestratorError::NotFound(_)) => None,
            Err(e) => return Err(e),
        },
        Err(e) => return Err(e),
    };
    Ok(match record {
        Some(record) => {
            let warning = (!floats_by_design).then(|| {
                format!(
                    "capability policy '{urn}' is not pinned by digest; loaded the latest release"
                )
            });
            (record.cedar, PolicyResolutionKind::Catalog, warning)
        }
        None => {
            let warning = format!(
                "declared capability policy '{urn}' is missing from the catalog; using builtins-only"
            );
            (
                DEFAULT_BUILTIN_FALLBACK_POLICY.to_string(),
                PolicyResolutionKind::DeclaredButMissing,
                Some(warning),
            )
        }
    })
}

/// `EgressRequest` policy for a legacy session: a permit for exactly the
/// destinations the broker allowlist held before AX-5.3d, the union of the
/// session's secrets' `allowed_destinations` (`secrets::broker_allowed_domains`).
/// No secrets compiles to a policy with no permit, which denies every host.
///
/// The patterns mean what `vam_network::match_domain` meant: an exact name, a
/// `*.suffix` wildcard that covers subdomains but not the apex, or a bare `*`.
/// A pattern `match_domain` could only ever match as a literal (a `*` anywhere
/// else, an empty wildcard suffix, a character Cedar cannot carry) matches no
/// real host, so it is dropped here and a warning names it.
pub(crate) fn compile_legacy_egress_policy(
    destinations: &[String],
) -> Result<String, vam_security::SecurityError> {
    let mut allow_all = false;
    let mut patterns: Vec<String> = Vec::new();
    for destination in destinations {
        let pattern = destination.to_ascii_lowercase();
        if pattern == "*" {
            allow_all = true;
        } else if !legacy_pattern_is_expressible(&pattern) {
            eprintln!(
                "[orchestrator] WARN legacy egress destination {destination:?} cannot match a host; ignored"
            );
        } else if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    if allow_all {
        return vam_security::platform_policy::compile_egress_rules("allow", &[]);
    }
    let rules: Vec<vam_security::platform_policy::EgressRule<'_>> = patterns
        .iter()
        .map(|pattern| vam_security::platform_policy::EgressRule {
            pattern,
            decision: "allow",
            except: Vec::new(),
        })
        .collect();
    vam_security::platform_policy::compile_egress_rules("deny", &rules)
}

fn legacy_pattern_is_expressible(pattern: &str) -> bool {
    let body = pattern.strip_prefix("*.").unwrap_or(pattern);
    !body.is_empty()
        && !body.contains('*')
        && !body
            .chars()
            .any(|c| c.is_control() || matches!(c, '"' | '\\'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::users::LOCAL_USER_ID;
    use vam_spec::v1::{CapabilityPolicyRef, Digest, ResourceId};

    const POLICY: &str = "urn:vam:policy:local.vam.dev/acme/pin-policy";
    const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/pin-agent";
    const SESSION: &str = "sess-pin";
    const RUN: &str = "urn:vam:run:local.vam.dev/acme/pin-run";

    fn ver(major: i32, minor: i32, patch: i32) -> Version {
        Version {
            major,
            minor,
            patch,
            ..Default::default()
        }
    }

    fn draft_ver() -> Version {
        Version {
            major: 1,
            prerelease: "draft".to_string(),
            ..Default::default()
        }
    }

    fn rid(urn: &str) -> ResourceId {
        ResourceId {
            urn: urn.to_string(),
        }
    }

    fn marker(tool: &str) -> String {
        format!(
            "permit (principal, action == Action::\"ExecuteTool\", resource == Tool::\"{tool}\");"
        )
    }

    fn agent(label: &str, policy_ref: Option<ResourceRef>) -> AgentSpec {
        AgentSpec {
            resource_id: Some(rid(AGENT)),
            display_name: label.to_string(),
            description: "pin test".to_string(),
            role: "assistant".to_string(),
            instruction_source: Some(
                vam_spec::v1::agent_spec::InstructionSource::SystemInstructions("x".to_string()),
            ),
            capability_policy: policy_ref.map(|r| CapabilityPolicyRef {
                policy_ref: Some(r),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn floating() -> ResourceRef {
        ResourceRef {
            resource_id: Some(rid(POLICY)),
            ..Default::default()
        }
    }

    fn pinned(version: Version, digest: &str) -> ResourceRef {
        ResourceRef {
            resource_id: Some(rid(POLICY)),
            version: Some(version),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: digest.to_string(),
            }),
            locators: vec![],
        }
    }

    fn release_policy(store: &SqliteWorkflowStore, cedar: &str, at: Version) {
        store
            .publish_policy_draft_with_owner(POLICY, cedar, LOCAL_USER_ID)
            .expect("policy draft");
        store.release_policy(POLICY, &at).expect("policy release");
    }

    fn release_agent(store: &SqliteWorkflowStore, spec: &AgentSpec, at: Version) {
        store
            .publish_agent_draft_with_owner(spec, LOCAL_USER_ID)
            .expect("agent draft");
        store.release_agent(AGENT, &at).expect("agent release");
    }

    /// A session with a run that pins `agent_version` and records `resolved_refs`.
    fn bind_run(
        store: &SqliteWorkflowStore,
        agent_version: Option<Version>,
        resolved_refs: Vec<ResourceRef>,
    ) {
        store
            .create_session(SESSION, None, "vm", "ws", "storage-default")
            .expect("session");
        let spec = RunSpec {
            run_id: Some(rid(RUN)),
            agent_ref: Some(ResourceRef {
                resource_id: Some(rid(AGENT)),
                version: agent_version,
                ..Default::default()
            }),
            resolved_refs,
            ..Default::default()
        };
        let json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec).expect("json");
        store
            .persist_run_spec_with_owner(
                RUN,
                &json,
                Some(SESSION),
                AGENT,
                None,
                "User::\"local\"",
                LOCAL_USER_ID,
            )
            .expect("run");
    }

    fn policy_events(store: &SqliteWorkflowStore) -> Vec<serde_json::Value> {
        store
            .read_outbox_for_run(RUN, 0)
            .expect("outbox")
            .into_iter()
            .filter(|e| e.event_type == EVENT_POLICY_RESOLUTION)
            .map(|e| serde_json::from_str(&e.payload).expect("payload json"))
            .collect()
    }

    #[test]
    fn get_policy_at_version_loads_the_exact_release() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_policy(&store, &marker("a"), ver(1, 0, 0));
        release_policy(&store, &marker("b"), ver(1, 1, 0));

        let at_1_0 = store.get_policy_at_version(POLICY, &ver(1, 0, 0)).unwrap();
        assert!(at_1_0.cedar.contains("\"a\""));
        assert_eq!(at_1_0.version, Some(ver(1, 0, 0)));
        let err = store
            .get_policy_at_version(POLICY, &ver(2, 0, 0))
            .expect_err("no 2.0.0");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
        assert!(err.to_string().contains("2.0.0"), "{err}");
    }

    #[test]
    fn get_policy_at_version_visible_hides_foreign_drafts() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("bob", "bob").expect("bob");
        store
            .publish_policy_draft_with_owner(POLICY, &marker("d"), LOCAL_USER_ID)
            .expect("draft");
        store
            .get_policy_at_version_visible(POLICY, &draft_ver(), LOCAL_USER_ID)
            .expect("owner sees own draft");
        let err = store
            .get_policy_at_version_visible(POLICY, &draft_ver(), "bob")
            .expect_err("foreign draft");
        assert!(matches!(err, OrchestratorError::NotFound(_)));
    }

    #[test]
    fn digest_is_recomputed_from_stored_cedar_not_the_column() {
        let cedar = marker("a");
        let expected = policy_cedar_digest(&cedar);
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_policy(&store, &cedar, ver(1, 0, 0));
        assert_eq!(
            store.get_policy(POLICY).unwrap().digest,
            expected,
            "the catalog stores sha256 of the Cedar text"
        );
        release_agent(&store, &agent("v1", Some(floating())), ver(1, 0, 0));
        bind_run(
            &store,
            Some(ver(1, 0, 0)),
            vec![pinned(ver(1, 0, 0), &expected)],
        );

        // Rewrite the digest column only: the Cedar still hashes to the pin.
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE policies SET digest = 'bogus' WHERE urn = ?1",
                [POLICY],
            )
            .unwrap();
        }
        let ok = resolve_legacy_session_policy(&store, SESSION, Some(AGENT)).expect("resolve");
        assert!(ok.cedar.contains("\"a\""));

        // Rewrite the Cedar and keep the column at the pin: fails closed.
        {
            let conn = store.connection.lock().unwrap();
            conn.execute(
                "UPDATE policies SET cedar = ?1, digest = ?2 WHERE urn = ?3",
                rusqlite::params![marker("evil"), expected, POLICY],
            )
            .unwrap();
        }
        let err = resolve_legacy_session_policy(&store, SESSION, Some(AGENT))
            .expect_err("tampered cedar");
        let message = err.to_string();
        assert!(message.contains(POLICY), "{message}");
        assert!(message.contains("1.0.0"), "{message}");
        assert!(message.contains(&expected), "{message}");
        assert!(
            message.contains(&policy_cedar_digest(&marker("evil"))),
            "{message}"
        );

        let events = policy_events(&store);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["kind"], "PinViolation");
        assert_eq!(events[0]["policy_urn"], POLICY);
        assert_eq!(events[0]["policy_version"], "1.0.0");
        assert_eq!(events[0]["expected_digest"], expected);
    }

    #[test]
    fn missing_pinned_version_fails_closed_and_records_an_event() {
        let cedar = marker("a");
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_policy(&store, &cedar, ver(1, 0, 0));
        release_agent(&store, &agent("v1", Some(floating())), ver(1, 0, 0));
        let digest = policy_cedar_digest(&marker("gone"));
        bind_run(
            &store,
            Some(ver(1, 0, 0)),
            vec![pinned(ver(2, 0, 0), &digest)],
        );

        let err = resolve_legacy_session_policy(&store, SESSION, Some(AGENT))
            .expect_err("missing 2.0.0 must not fall back to 1.0.0");
        let message = err.to_string();
        assert!(
            message.contains(POLICY) && message.contains("2.0.0"),
            "{message}"
        );
        assert!(message.contains(&digest), "{message}");
        let events = policy_events(&store);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["kind"], "PinViolation");
        assert!(events[0]["actual_digest"].is_null());
    }

    #[test]
    fn pinned_policy_missing_entirely_fails_closed() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let digest = policy_cedar_digest(&marker("a"));
        let frozen = agent("v1", Some(pinned(ver(1, 0, 0), &digest)));
        store
            .put_agent_released_with_owner(&frozen, LOCAL_USER_ID)
            .expect("agent");
        bind_run(&store, None, vec![]);
        let err = resolve_legacy_session_policy(&store, SESSION, Some(AGENT))
            .expect_err("pin names a policy the catalog lacks");
        assert!(err.to_string().contains(POLICY), "{err}");
    }

    #[test]
    fn ref_without_digest_loads_latest_with_a_warning() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_policy(&store, &marker("a"), ver(1, 0, 0));
        release_policy(&store, &marker("b"), ver(1, 1, 0));
        let undigested = ResourceRef {
            resource_id: Some(rid(POLICY)),
            version: Some(ver(1, 0, 0)),
            ..Default::default()
        };
        store
            .put_agent_released_with_owner(&agent("old", Some(undigested)), LOCAL_USER_ID)
            .expect("agent");
        bind_run(&store, None, vec![]);

        let resolved = resolve_legacy_session_policy(&store, SESSION, Some(AGENT)).unwrap();
        assert!(resolved.cedar.contains("\"b\""));
        assert!(!resolved.cedar.contains("\"a\""));
        let warning = resolved.warning.expect("warning");
        assert!(
            warning.contains(POLICY) && warning.contains("digest"),
            "{warning}"
        );
        // The caller records it on the run.
        crate::security::surface_policy_resolution(&store, SESSION, AGENT, &{
            resolve_legacy_session_policy(&store, SESSION, Some(AGENT)).unwrap()
        });
        let events = policy_events(&store);
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn run_recorded_pin_wins_over_the_agents_floating_ref() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_policy(&store, &marker("a"), ver(1, 0, 0));
        release_policy(&store, &marker("b"), ver(1, 1, 0));
        let a_digest = policy_cedar_digest(&marker("a"));
        store
            .publish_agent_draft_with_owner(&agent("draft", Some(floating())), LOCAL_USER_ID)
            .expect("draft agent");
        bind_run(
            &store,
            Some(draft_ver()),
            vec![pinned(ver(1, 0, 0), &a_digest)],
        );

        let resolved = resolve_legacy_session_policy(&store, SESSION, Some(AGENT)).unwrap();
        assert!(resolved.cedar.contains("\"a\""));
        assert!(resolved.warning.is_none());
    }

    #[test]
    fn draft_run_floating_ref_follows_the_catalog_without_a_warning() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_policy(&store, &marker("a"), ver(1, 0, 0));
        store
            .publish_agent_draft_with_owner(&agent("draft", Some(floating())), LOCAL_USER_ID)
            .expect("draft agent");
        bind_run(&store, Some(draft_ver()), vec![]);
        let resolved = resolve_legacy_session_policy(&store, SESSION, Some(AGENT)).unwrap();
        assert!(resolved.cedar.contains("\"a\""));
        assert!(resolved.warning.is_none());
    }

    #[test]
    fn load_session_agent_uses_the_pinned_release_and_the_owner_draft() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_agent(&store, &agent("one", None), ver(1, 0, 0));
        release_agent(&store, &agent("two", None), ver(1, 1, 0));
        store
            .publish_agent_draft_with_owner(&agent("wip", None), LOCAL_USER_ID)
            .expect("draft");

        bind_run(&store, Some(ver(1, 0, 0)), vec![]);
        let loaded = load_session_agent(&store, SESSION, Some(AGENT)).unwrap();
        assert_eq!(loaded.agent.unwrap().display_name, "one");
        assert!(!loaded.draft_run);
    }

    #[test]
    fn load_session_agent_draft_run_loads_the_draft() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_agent(&store, &agent("two", None), ver(1, 1, 0));
        store
            .publish_agent_draft_with_owner(&agent("wip", None), LOCAL_USER_ID)
            .expect("draft");
        bind_run(&store, Some(draft_ver()), vec![]);
        let loaded = load_session_agent(&store, SESSION, Some(AGENT)).unwrap();
        assert_eq!(loaded.agent.unwrap().display_name, "wip");
        assert!(loaded.draft_run);
        assert_eq!(loaded.resolve_owner, LOCAL_USER_ID);
    }

    #[test]
    fn load_session_agent_without_a_pin_keeps_the_latest_release_lookup() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_agent(&store, &agent("one", None), ver(1, 0, 0));
        release_agent(&store, &agent("two", None), ver(1, 1, 0));
        bind_run(&store, None, vec![]);
        let loaded = load_session_agent(&store, SESSION, Some(AGENT)).unwrap();
        assert_eq!(loaded.agent.unwrap().display_name, "two");
        assert!(!loaded.draft_run);
    }

    #[test]
    fn load_session_agent_with_an_unloadable_pin_is_an_error() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        release_agent(&store, &agent("one", None), ver(1, 0, 0));
        bind_run(&store, Some(ver(9, 9, 9)), vec![]);
        let err = load_session_agent(&store, SESSION, Some(AGENT)).expect_err("no 9.9.9");
        assert!(matches!(err, OrchestratorError::NotFound(_)), "{err}");
    }
}
