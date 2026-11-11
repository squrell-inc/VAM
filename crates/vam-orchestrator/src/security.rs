use crate::error::OrchestratorError;
use crate::trajectory::{
    emit_policy_decided, emit_skill_loaded, emit_tool_invoked, extract_skill_id,
    extract_workspace_path,
};
use crate::workflow::{PlaybookStepRecord, PlaybookStepState, SqliteWorkflowStore};
use ed25519_dalek::SigningKey;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use vam_inference::SessionBudget;
use vam_security::{
    mint_capability_token, ApprovalSigner, AuditLedger, CapabilityClaims, CedarPolicyEngine,
    DecisionOutcome, EvaluationContext, EvaluationRequest, InMemoryReplayStore,
    PolicyDecisionPoint, PolicyMode, ReplayStore,
};
use vam_spec::v1::{
    tool_intent_response::Disposition, ErrorStatus, ExecutionLease, GuardArgsWithin, PlaybookGuard,
    ResourceId, ToolIntentRequest, ToolIntentResponse,
};
use vam_storage::ChunkStore;
use vam_telemetry::TelemetryCapture;

const WORKSPACE_MOUNT: &str = "/mnt/drive9/workspace";
const SKILLS_MOUNT: &str = "/mnt/drive9/skills";
const TODO_FILE_CEDAR_PATH: &str = "/mnt/drive9/workspace/.vam/agent/todos.json";

/// Unknown / undeclared policy fallback: builtins only (no skill tools), plus shared FS rules.
pub const DEFAULT_BUILTIN_FALLBACK_POLICY: &str = vam_security::BUILTIN_EXECUTE_TOOL_POLICY;

/// Outbox event recorded on the run when policy resolution is fail-closed.
pub const EVENT_POLICY_RESOLUTION: &str = "vam.v1.policy.resolution";

/// Sample Cedar documents under `examples/`. Compile-checked in tests; not boot-seeded.
#[cfg(test)]
pub(crate) const EXAMPLE_SAMPLE_POLICIES: &[(&str, &str)] = &[
    (
        "urn:vam:policy:local.vam.dev/vam/chaos-isolated",
        include_str!("../../../examples/chaos-assistant/policy.cedar"),
    ),
    (
        "urn:vam:policy:local.vam.dev/vam/sqlite-memory-only",
        include_str!("../../../examples/sqlite-assistant/policy.cedar"),
    ),
    (
        "urn:vam:policy:local.vam.dev/vam/jira-egress-only",
        include_str!("../../../examples/jira-assistant/policy.cedar"),
    ),
    (
        "urn:vam:policy:local.vam.dev/vam/assistant-egress-only",
        include_str!("../../../examples/bun-assistant/policy.cedar"),
    ),
    (
        "urn:vam:policy:local.vam.dev/vam/hsp-echo-ts",
        include_str!("../../../examples/hsp-echo-ts/policy.cedar"),
    ),
];

/// How the session's capability policy was resolved at broker spawn.
/// Policy stays fixed for the life of the session (no hot-reload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyResolutionKind {
    Catalog,
    NoPolicyDeclared,
    DeclaredButMissing,
}

/// Which policy model a session runs under, recorded on its run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPolicyMode {
    /// A released agent with `capability_policy`: Cedar by URN, no `Tool` entities.
    Legacy,
    /// An agent with `permissions`: the platform policy set and per-tool entities.
    Permissions,
}

impl SessionPolicyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionPolicyMode::Legacy => "legacy",
            SessionPolicyMode::Permissions => "permissions",
        }
    }
}

/// Composed Cedar source plus the resolution outcome used at spawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSessionPolicy {
    pub cedar: String,
    pub mode: SessionPolicyMode,
    pub kind: PolicyResolutionKind,
    pub policy_urn: Option<String>,
    pub monitored_tools: Vec<String>,
    pub skill_ref_count: usize,
    pub warning: Option<String>,
}

/// Derive permit-under-approval plus forbid-unless-approved statements per monitored tool.
///
/// The permit makes `monitoredTools` self-sufficient: an agent with no Cedar policy still
/// gets pass-1 Deny plus pass-2 Allow, so the tool suspends for approval.
/// The forbid tightens a base policy that would otherwise permit the tool unconditionally.
/// Invalid tool names are skipped so they cannot break Cedar string literals.
pub fn derive_hitl_overlay(agent_urn: &str, monitored_tools: &[String]) -> String {
    let names: Vec<&str> = monitored_tools.iter().map(String::as_str).collect();
    vam_security::hitl_execute_overlay(agent_urn, &names)
}

/// Cedar overlay that permits the playbook `step_complete` host tool.
///
/// Composed only for sessions whose run has playbook step state. Not part of
/// [`DEFAULT_BUILTIN_FALLBACK_POLICY`] so non-playbook sessions never advertise it.
pub const PLAYBOOK_STEP_COMPLETE_OVERLAY: &str = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"step_complete"
);
"#;

/// Cedar overlay that permits the playbook `run_step` host tool.
///
/// Composed with [`PLAYBOOK_STEP_COMPLETE_OVERLAY`] only when the session's run
/// has playbook step state.
pub const PLAYBOOK_RUN_STEP_OVERLAY: &str = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"run_step"
);
"#;

/// Cedar overlay that permits the playbook finish probe.
///
/// The guest calls `check_playbook_finish` before committing success. Composed
/// only when the session's run has playbook step state.
pub const PLAYBOOK_FINISH_CHECK_OVERLAY: &str = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"check_playbook_finish"
);
"#;

/// Cedar overlay that permits `start_playbook`.
///
/// Composed only when the session's run offers playbooks. It permits the call;
/// the host still checks that the playbook is attached and the inputs are valid,
/// and may park the start for a person to confirm. A forbid in the agent's own
/// policy still wins.
pub const PLAYBOOK_START_OVERLAY: &str = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"start_playbook"
);
"#;

/// Cedar overlay that permits the outbox tools `post_message`, `show` and `ask_user` (AR-3.9, AR-3.10).
///
/// Always composed: an agent whose own policy or tool defaults would deny a tool it
/// never named keeps these two, as `start_playbook` is kept. A forbid in the agent's
/// own policy still wins. In permissions mode the same default is applied to the
/// session's tool rows (`session_targets`), because the platform policy's
/// `forbid agent_rule == "deny"` would otherwise beat this permit. `show` is
/// further limited by the run's `human.jig` switch, which the guest and the host's
/// `PostOutbox` both check.
pub const OUTBOX_OVERLAY: &str = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"post_message"
);

permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"show"
);

permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"ask_user"
);
"#;

/// Return the playbook Cedar overlay when the session's run has a playbook row
/// or offers playbooks.
pub fn derive_playbook_overlay(
    store: &SqliteWorkflowStore,
    session_id: &str,
) -> Result<String, OrchestratorError> {
    let Some(run_id) = store.find_run_id_for_session(session_id)? else {
        return Ok(String::new());
    };
    let bound = store.get_playbook_run(&run_id)?.is_some();
    let offers = !bound && !crate::playbook_activation::run_attached(store, &run_id).is_empty();
    if !bound && !offers {
        return Ok(String::new());
    }
    let steps = compose_policy(
        &compose_policy(
            PLAYBOOK_STEP_COMPLETE_OVERLAY.trim(),
            PLAYBOOK_RUN_STEP_OVERLAY.trim(),
        ),
        PLAYBOOK_FINISH_CHECK_OVERLAY.trim(),
    );
    if offers {
        Ok(compose_policy(&steps, PLAYBOOK_START_OVERLAY.trim()))
    } else {
        Ok(steps)
    }
}

/// Join a base Cedar document and an overlay the way [`resolve_session_policy`] does.
///
/// `#[doc(hidden)]` so `legacy_permissions_tests` can pin composed bytes without
/// making this a supported API. Same concatenation as `vam_security::compose_policy`.
#[doc(hidden)]
pub fn compose_policy(base: &str, overlay: &str) -> String {
    let overlay = overlay.trim();
    if overlay.is_empty() {
        base.to_string()
    } else {
        format!("{}\n\n{overlay}", base.trim_end())
    }
}

pub(crate) fn capability_policy_urn(agent: &vam_spec::v1::AgentSpec) -> Option<String> {
    agent
        .capability_policy
        .as_ref()
        .and_then(|cp| cp.policy_ref.as_ref())
        .and_then(|pr| pr.resource_id.as_ref())
        .map(|r| r.urn.clone())
        .filter(|u| !u.is_empty())
}

pub(crate) fn monitored_tools_from_agent(agent: &vam_spec::v1::AgentSpec) -> Vec<String> {
    agent
        .hitl_policy
        .as_ref()
        .map(|hp| hp.monitored_tools.clone())
        .unwrap_or_default()
}

pub(crate) fn session_agent_urn(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
) -> Result<Option<String>, OrchestratorError> {
    if let Some(urn) = principal_urn {
        if !urn.is_empty() {
            return Ok(Some(urn.to_string()));
        }
    }
    Ok(store
        .find_agent_urn_for_session(session_id)?
        .filter(|u| !u.is_empty()))
}

/// Builtins-only fallback. Unknown URNs no longer map to hardcoded assistant documents.
pub fn resolve_policy_for_urn(_policy_urn: &str) -> String {
    DEFAULT_BUILTIN_FALLBACK_POLICY.to_string()
}

/// Record a fail-closed policy resolution on the session's run outbox (visible in the UI).
pub fn surface_policy_resolution(
    store: &SqliteWorkflowStore,
    session_id: &str,
    agent_urn: &str,
    resolved: &ResolvedSessionPolicy,
) {
    let Some(warning) = resolved.warning.as_deref() else {
        return;
    };
    eprintln!(
        "[orchestrator] WARN session {session_id} agent={agent_urn} policy_urn={:?}: {warning}",
        resolved.policy_urn
    );
    let Ok(Some(run_id)) = store.find_run_id_for_session(session_id) else {
        return;
    };
    let payload = serde_json::json!({
        "message": warning,
        "kind": format!("{:?}", resolved.kind),
        "policy_urn": resolved.policy_urn,
        "agent_urn": agent_urn,
        "monitored_tools": resolved.monitored_tools,
    })
    .to_string();
    if let Err(e) = store.append_outbox_event(&run_id, EVENT_POLICY_RESOLUTION, &payload) {
        eprintln!("[orchestrator] failed to record policy resolution on run {run_id}: {e}");
    }
}

/// Resolve Cedar source for a session: catalog row, else builtins-only, plus HITL overlay.
/// In-flight sessions do not hot-reload; policy is fixed at broker spawn time.
///
/// Delegates to the legacy `capability_policy` path in [`crate::policy_legacy`], which
/// loads the run's pinned agent and enforces the pinned policy version and digest.
///
/// An agent with `permissions` is assembled from the platform policy set (AX-5.2),
/// each `advanced` attachment at its pin, and the playbook overlay. Any other
/// agent takes the legacy path unchanged.
pub fn resolve_session_policy(
    store: &SqliteWorkflowStore,
    session_id: &str,
    principal_urn: Option<&str>,
) -> Result<ResolvedSessionPolicy, OrchestratorError> {
    let session = crate::policy_legacy::load_session_agent(store, session_id, principal_urn)?;
    match session.agent.as_ref().and_then(|a| a.permissions.as_ref()) {
        Some(permissions) => {
            resolve_permissions_session_policy(store, session_id, &session, permissions)
        }
        None => {
            crate::policy_legacy::resolve_legacy_session_policy(store, session_id, principal_urn)
        }
    }
}

/// Path limb of the builtin policy (`ReadFile` / `WriteFile`). The platform tool
/// policy decides `ExecuteTool` only, and `evaluate_fs_path_limb` still checks
/// the path of the file builtins, so a permissions session needs these three
/// statements too. They match the tail of `BUILTIN_EXECUTE_TOOL_POLICY`.
pub(crate) const PERMISSIONS_PATH_LIMB_POLICY: &str = r#"
permit (principal, action == Action::"ReadFile", resource)
when {
    context has path &&
    (context.path like "/mnt/drive9/workspace/*" ||
     context.path like "/mnt/drive9/skills/*")
};

permit (principal, action == Action::"WriteFile", resource)
when {
    context has path &&
    context.path like "/mnt/drive9/workspace/*" &&
    !(context.path like "/mnt/drive9/skills/*") &&
    context has file_size_bytes &&
    context.file_size_bytes <= 10485760
};

forbid (principal, action == Action::"WriteFile", resource)
when { context has path && context.path like "/mnt/drive9/skills/*" };
"#;

fn version_text(version: &vam_spec::v1::Version) -> String {
    if version.prerelease.is_empty() {
        format!("{}.{}.{}", version.major, version.minor, version.patch)
    } else {
        format!(
            "{}.{}.{}-{}",
            version.major, version.minor, version.patch, version.prerelease
        )
    }
}

/// Explain origin for the agent scope, and whether "Always" can be saved: only
/// the agent owner's own draft run can write into the draft.
pub(crate) fn run_provenance(
    store: &SqliteWorkflowStore,
    session_id: &str,
    session: &crate::policy_legacy::SessionAgent,
) -> crate::session_targets::RunProvenance {
    let session_owner = store
        .session_owner_user_id(session_id)
        .unwrap_or_else(|_| crate::users::LOCAL_USER_ID.to_string());
    let owner_draft_run = session.draft_run && session_owner == session.resolve_owner;
    let agent_urn = session
        .agent
        .as_ref()
        .and_then(|a| a.resource_id.as_ref())
        .map(|id| id.urn.as_str())
        .unwrap_or_default();
    let agent_origin = if session.draft_run {
        "agent draft".to_string()
    } else {
        let pinned = session
            .run_spec
            .as_ref()
            .and_then(|spec| spec.agent_ref.as_ref())
            .and_then(|r| r.version.as_ref())
            .map(version_text);
        match pinned {
            Some(version) => format!("agent {version}"),
            None => format!("agent {agent_urn}"),
        }
    };
    crate::session_targets::RunProvenance {
        agent_origin,
        owner_draft_run,
    }
}

/// A pinned `advanced` attachment loaded at exactly its version, with the digest
/// recomputed from the stored Cedar. A floating ref follows the catalog the way
/// the legacy path does. Anything that cannot be loaded stops the session.
fn load_advanced_policy(
    store: &SqliteWorkflowStore,
    session_id: &str,
    session: &crate::policy_legacy::SessionAgent,
    policy_ref: &vam_spec::v1::ResourceRef,
) -> Result<(String, Option<String>), OrchestratorError> {
    let urn = policy_ref
        .resource_id
        .as_ref()
        .map(|id| id.urn.as_str())
        .filter(|urn| !urn.is_empty())
        .ok_or_else(|| {
            OrchestratorError::CorruptState("advanced policy ref names no policy".to_string())
        })?;
    let names_policy = |r: &&vam_spec::v1::ResourceRef| {
        r.resource_id.as_ref().map(|id| id.urn.as_str()) == Some(urn)
    };
    let pin_of = |r: &vam_spec::v1::ResourceRef| {
        let version = r.version.clone()?;
        let digest = r
            .digest
            .as_ref()
            .map(|d| d.hex_hash.clone())
            .filter(|hex| !crate::catalog_resolve::is_unpinned_digest(hex))?;
        Some((version, digest))
    };
    let recorded = session.run_spec.as_ref().and_then(|spec| {
        spec.resolved_refs
            .iter()
            .filter(names_policy)
            .find_map(pin_of)
    });
    let pin = recorded.or_else(|| pin_of(policy_ref));
    let fail = |message: String| {
        eprintln!("[orchestrator] ERROR session {session_id} advanced policy '{urn}': {message}");
        if let Ok(Some(run_id)) = store.find_run_id_for_session(session_id) {
            let payload = serde_json::json!({
                "message": message,
                "kind": "PinViolation",
                "policy_urn": urn,
            })
            .to_string();
            let _ = store.append_outbox_event(&run_id, EVENT_POLICY_RESOLUTION, &payload);
        }
        OrchestratorError::CorruptState(format!("policy pin violation: {message}"))
    };
    if let Some((version, expected)) = pin {
        let label = version_text(&version);
        let record = store
            .get_policy_at_version_visible(urn, &version, &session.resolve_owner)
            .map_err(|e| match e {
                OrchestratorError::NotFound(_) => fail(format!(
                    "pinned advanced policy '{urn}' version {label} is missing from the catalog \
                     (expected digest {expected}); refusing to start the session"
                )),
                other => other,
            })?;
        let actual = crate::catalog::policy_cedar_digest(&record.cedar);
        if !actual.eq_ignore_ascii_case(&expected) {
            return Err(fail(format!(
                "pinned advanced policy '{urn}' version {label} failed digest verification: \
                 expected digest {expected}, stored Cedar hashes to {actual}; refusing to start the session"
            )));
        }
        return Ok((record.cedar, None));
    }
    let record = match store.get_policy_visible(urn, &session.resolve_owner) {
        Ok(record) => record,
        Err(OrchestratorError::NotFound(_)) => store.get_policy(urn).map_err(|e| match e {
            OrchestratorError::NotFound(_) => fail(format!(
                "advanced policy '{urn}' is missing from the catalog; refusing to start the session"
            )),
            other => other,
        })?,
        Err(e) => return Err(e),
    };
    let warning = (!session.draft_run).then(|| {
        format!("advanced policy '{urn}' is not pinned by digest; loaded the latest release")
    });
    Ok((record.cedar, warning))
}

fn resolve_permissions_session_policy(
    store: &SqliteWorkflowStore,
    session_id: &str,
    session: &crate::policy_legacy::SessionAgent,
    permissions: &vam_spec::v1::AgentPermissions,
) -> Result<ResolvedSessionPolicy, OrchestratorError> {
    let mut overlay = String::from(PERMISSIONS_PATH_LIMB_POLICY);
    let mut warning = None;
    for policy_ref in &permissions.advanced {
        let (cedar, advanced_warning) =
            load_advanced_policy(store, session_id, session, policy_ref)?;
        overlay = compose_policy(&overlay, &cedar);
        warning = warning.or(advanced_warning);
    }
    let skill_ref_count = session
        .agent
        .as_ref()
        .map(|a| a.skill_refs.len())
        .unwrap_or(0);
    overlay = compose_policy(&overlay, &derive_playbook_overlay(store, session_id)?);
    overlay = compose_policy(&overlay, OUTBOX_OVERLAY.trim());
    Ok(ResolvedSessionPolicy {
        cedar: vam_security::compose_platform_policy(&overlay),
        mode: SessionPolicyMode::Permissions,
        kind: PolicyResolutionKind::Catalog,
        policy_urn: None,
        monitored_tools: Vec::new(),
        skill_ref_count,
        warning,
    })
}

fn fs_path_limb_action(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "read_file" | "glob" | "grep" => Some("ReadFile"),
        "write_file" | "edit_file" | "delete_file" | "todo_write" => Some("WriteFile"),
        _ => None,
    }
}

fn extract_raw_path_arg(args: &serde_json::Value) -> Option<String> {
    let serde_json::Value::Object(map) = args else {
        return None;
    };
    map.get("path")
        .or_else(|| map.get("filepath"))
        .and_then(|v| v.as_str())
        .filter(|p| !p.is_empty())
        .map(str::to_string)
}

pub fn normalize_cedar_path(raw: &str) -> Result<String, String> {
    if raw.is_empty() {
        return Err("path cannot be empty".to_string());
    }
    if raw.contains('\0') {
        return Err("path contains null byte".to_string());
    }
    if raw.split('/').any(|c| c == "..") {
        return Err("path traversal ('..') is not allowed".to_string());
    }

    if raw.starts_with(SKILLS_MOUNT) {
        if raw == SKILLS_MOUNT || raw.starts_with(&format!("{SKILLS_MOUNT}/")) {
            return Ok(raw.to_string());
        }
        return Err(format!(
            "path '{}' is outside the allowed skills mount",
            raw
        ));
    }

    if raw.starts_with(WORKSPACE_MOUNT) {
        return Ok(raw.to_string());
    }

    if raw.starts_with('/') {
        return Err(format!(
            "absolute path '{}' outside allowed mounts is not allowed",
            raw
        ));
    }

    use std::path::{Component, Path};
    let p = Path::new(raw);
    for component in p.components() {
        match component {
            Component::ParentDir => {
                return Err("path traversal ('..') is not allowed".to_string());
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err("absolute paths outside the workspace jail are not allowed".to_string());
            }
            _ => {}
        }
    }

    let rel = raw.trim_start_matches("./");
    Ok(format!("{WORKSPACE_MOUNT}/{rel}"))
}

fn resolve_fs_path_for_tool(tool_name: &str, args: &serde_json::Value) -> Result<String, String> {
    match tool_name {
        "todo_write" => Ok(TODO_FILE_CEDAR_PATH.to_string()),
        "glob" | "grep" => {
            if let Some(raw) = extract_raw_path_arg(args) {
                normalize_cedar_path(&raw)
            } else {
                Ok(format!("{WORKSPACE_MOUNT}/"))
            }
        }
        "read_file" | "write_file" | "edit_file" | "delete_file" => {
            let raw = extract_raw_path_arg(args).ok_or_else(|| {
                format!(
                    "{} requires a path argument for filesystem authorization",
                    tool_name
                )
            })?;
            normalize_cedar_path(&raw)
        }
        other => Err(format!("tool '{}' is not a filesystem primitive", other)),
    }
}

fn write_file_size_bytes(tool_name: &str, args: &serde_json::Value) -> Option<i64> {
    let serde_json::Value::Object(map) = args else {
        return None;
    };
    match tool_name {
        "write_file" => map
            .get("content")
            .and_then(|v| v.as_str())
            .map(|s| s.len() as i64),
        "edit_file" => map
            .get("new_string")
            .and_then(|v| v.as_str())
            .map(|s| s.len() as i64),
        "delete_file" => Some(0),
        "todo_write" => map
            .get("todos")
            .and_then(|v| serde_json::to_string(v).ok())
            .map(|s| s.len() as i64),
        _ => None,
    }
}

pub struct SessionSecurityContext {
    pub session_id: String,
    pub agent_urn: String,
    pub pdp: Arc<dyn PolicyDecisionPoint>,
    pub audit_ledger: Arc<AuditLedger>,
    pub approval_signer: Arc<ApprovalSigner>,
    pub budget: Arc<SessionBudget>,
    pub replay_store: Arc<dyn ReplayStore>,
    pub iteration_counter: std::sync::atomic::AtomicU64,
    telemetry_capture: TelemetryCapture,
    chunk_store: Option<ChunkStore>,
    /// Permissions mode: every request carries the tool's `Tool` entity, built
    /// from the session's `session_tool_targets` row. Legacy passes none.
    permissions_mode: bool,
    /// DNS for open-web admission (AR-1.4). The system resolver in production;
    /// tests inject answers so nothing touches the network.
    web_resolver: Arc<dyn vam_security::ssrf_guard::HostResolver + Send + Sync>,
    /// Keys the effect journal's idempotency keys. `None` falls back to the installed node keys.
    effect_secret: Option<crate::effect_key::EffectKeySecret>,
}

impl SessionSecurityContext {
    #[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
    pub fn new(
        session_id: String,
        agent_urn: String,
        policy_src: &str,
        mode: PolicyMode,
        generation: u64,
        host_signing_key: SigningKey,
        initial_parent_snapshot: Option<String>,
        budget: Arc<SessionBudget>,
    ) -> Result<Self, OrchestratorError> {
        let engine = CedarPolicyEngine::from_default_schema(policy_src, mode, generation)
            .map_err(|e| OrchestratorError::Core(format!("Cedar PDP compile failed: {}", e)))?;

        let key_id = format!("host-key-{}", session_id);
        let signer = Arc::new(ApprovalSigner::new(
            key_id.clone(),
            host_signing_key.clone(),
        ));
        let audit_ledger = Arc::new(AuditLedger::new(
            key_id,
            host_signing_key,
            initial_parent_snapshot,
        ));
        let replay_store = Arc::new(InMemoryReplayStore::default());

        Ok(Self {
            session_id,
            agent_urn,
            pdp: Arc::new(engine),
            audit_ledger,
            approval_signer: signer,
            budget,
            replay_store,
            iteration_counter: std::sync::atomic::AtomicU64::new(1),
            telemetry_capture: TelemetryCapture::default(),
            chunk_store: None,
            permissions_mode: false,
            web_resolver: Arc::new(vam_security::ssrf_guard::SystemResolver),
            // Unit tests journal calls without installing node keys.
            effect_secret: if cfg!(test) {
                Some(crate::signing::NodeKeys::ephemeral().effect_key_secret())
            } else {
                None
            },
        })
    }

    /// Use this effect-key secret instead of the installed node keys (tests).
    pub fn with_effect_key_secret(mut self, secret: crate::effect_key::EffectKeySecret) -> Self {
        self.effect_secret = Some(secret);
        self
    }

    /// The host-only secret behind the effect journal's idempotency keys.
    pub(crate) fn effect_secret(&self) -> Option<crate::effect_key::EffectKeySecret> {
        self.effect_secret
            .clone()
            .or_else(|| crate::signing::node_keys().map(|keys| keys.effect_key_secret()))
    }

    /// Resolve DNS for open-web admission through `resolver` (tests).
    pub fn with_web_resolver(
        mut self,
        resolver: Arc<dyn vam_security::ssrf_guard::HostResolver + Send + Sync>,
    ) -> Self {
        self.web_resolver = resolver;
        self
    }

    /// Who is calling and how to resolve DNS, for HSP preflight.
    pub fn web_admission_env(&self) -> crate::web_admission::AdmissionEnv {
        crate::web_admission::AdmissionEnv::new(self.session_id.clone(), self.agent_urn.clone())
            .with_resolver(self.web_resolver.clone())
    }

    /// Run this session under permissions: requests carry `Tool` entities and a
    /// tool with no `session_tool_targets` row is denied.
    pub fn with_policy_mode(mut self, mode: SessionPolicyMode) -> Self {
        self.permissions_mode = mode == SessionPolicyMode::Permissions;
        self
    }

    /// `Tool` entities for one request. Legacy sessions pass none. In
    /// permissions mode a tool with no row yields [`None`], which the caller
    /// must treat as a denial.
    fn entities_for(
        &self,
        store: &SqliteWorkflowStore,
        tool_name: &str,
    ) -> Result<Vec<vam_security::ToolEntity>, String> {
        Ok(self
            .target_row_for(store, tool_name)?
            .map(|row| vec![row.entity()])
            .unwrap_or_default())
    }

    /// The session's own row for the tool (permissions mode); none in legacy
    /// mode. One read feeds both Cedar and the run's decision record, so a
    /// later "Always" cannot change what an earlier decision is said to rest on.
    fn target_row_for(
        &self,
        store: &SqliteWorkflowStore,
        tool_name: &str,
    ) -> Result<Option<crate::session_targets::SessionToolTarget>, String> {
        if !self.permissions_mode {
            return Ok(None);
        }
        match crate::session_targets::target_for(store, &self.session_id, tool_name) {
            Ok(Some(row)) => Ok(Some(row)),
            Ok(None) => Err(format!(
                "Tool '{tool_name}' has no permission target in this session"
            )),
            Err(e) => Err(format!(
                "Storage error reading the tool's permission target: {e}"
            )),
        }
    }

    /// Writes the run's record of the decision just made. It only reads the
    /// final outcome; it cannot change it.
    #[allow(clippy::too_many_arguments)]
    fn record_pep_decision(
        &self,
        store: &SqliteWorkflowStore,
        eval_request: &EvaluationRequest,
        catalog_entry: &crate::session::ToolCatalogEntry,
        target_row: Option<&crate::session_targets::SessionToolTarget>,
        outcome: &DecisionOutcome,
        decided_by: &'static str,
        guard_reason: Option<&str>,
        args_digest: &str,
        constraint_rule: Option<String>,
    ) {
        use crate::run_facts::decided_by as by;
        let (disposition, approval_request_id) = match outcome {
            DecisionOutcome::Allow { .. } => ("allowed", None),
            DecisionOutcome::Deny { .. } => ("denied", None),
            DecisionOutcome::Suspend {
                approval_request_id,
                ..
            } => ("asked", Some(approval_request_id.clone())),
        };
        let (deciding_scope, deciding_rule) = match decided_by {
            by::PLAYBOOK_GUARD => (
                "playbook".to_string(),
                guard_reason.unwrap_or_default().to_string(),
            ),
            // The diagnostic can name a path; the record never does.
            by::PATH_LIMB => ("path".to_string(), "path limit".to_string()),
            by::HOST_ERROR => ("host".to_string(), String::new()),
            _ => match target_row {
                Some(row) => (row.deciding_scope.clone(), row.deciding_rule.clone()),
                None => (
                    "policy".to_string(),
                    match outcome {
                        DecisionOutcome::Allow { matching_policies }
                        | DecisionOutcome::Deny {
                            matching_policies, ..
                        } => matching_policies.join(","),
                        DecisionOutcome::Suspend { .. } => String::new(),
                    },
                ),
            },
        };
        // A call that passes only because an earlier ask was approved.
        let via_grant =
            outcome.is_allowed() && eval_request.context.hitl_approved == Some(true) && {
                let mut without = eval_request.clone();
                without.context.hitl_approved = None;
                matches!(self.pdp.evaluate(&without), DecisionOutcome::Deny { .. })
            };
        self.record_decision_row(
            store,
            crate::run_facts::NewRunDecision {
                session_id: self.session_id.clone(),
                tool_name: catalog_entry.tool_name.clone(),
                target: target_row
                    .map(|row| row.target.clone())
                    .unwrap_or_else(|| catalog_entry.tool_name.clone()),
                side_effect: catalog_entry.side_effect as i64,
                disposition,
                decided_by,
                deciding_scope,
                deciding_rule,
                args_digest: args_digest.to_string(),
                approval_request_id,
                via_grant,
                // Only a call that went through names the guard it passed.
                constraint_rule: if outcome.is_allowed() {
                    constraint_rule
                } else {
                    None
                },
            },
        );
    }

    /// Stores a decision on the run bound to this session. Evidence only: the
    /// decision is already final, and a failure here is logged and counted by
    /// the store, never returned. The run comes from the session binding, not
    /// from the lease, which the guest echoes.
    fn record_decision_row(
        &self,
        store: &SqliteWorkflowStore,
        decision: crate::run_facts::NewRunDecision,
    ) {
        let Ok(Some(run_id)) = store.find_run_id_for_session(&self.session_id) else {
            return;
        };
        store.record_run_decision(&run_id, &decision);
    }

    /// Iteration, spend, and token totals the PEP should enforce for this call.
    ///
    /// When this session is bound to a run, that run's persisted counters are
    /// the source of truth and the in-memory counter is updated to match.
    /// Sessions with no run keep the in-memory counter. The lease's run id is
    /// not used: the guest does not choose which run is metered.
    fn metered_eval_counters(&self, store: &SqliteWorkflowStore) -> (i64, i64, i64) {
        let run_id = store
            .find_run_id_for_session(&self.session_id)
            .ok()
            .flatten()
            .unwrap_or_default();
        if !run_id.is_empty() {
            if let Ok(metering) = store.apply_run_metering(&run_id, &self.budget) {
                self.iteration_counter.store(
                    u64::try_from(metering.iteration)
                        .unwrap_or(u64::MAX)
                        .saturating_add(1),
                    std::sync::atomic::Ordering::SeqCst,
                );
                return (
                    metering.iteration,
                    metering.spend_micro,
                    metering.token_count,
                );
            }
        }
        let iteration = self
            .iteration_counter
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        (
            iteration as i64,
            self.budget.get_accumulated_cost(),
            self.budget.get_accumulated_tokens(),
        )
    }

    /// Wire host telemetry capture for PEP `tool.invoked` args (Drive9 CAS + previews).
    pub fn configure_pep_telemetry(&mut self, capture: TelemetryCapture, chunk_store: ChunkStore) {
        self.telemetry_capture = capture;
        self.chunk_store = Some(chunk_store);
    }

    /// Returns catalog tool names that Cedar would allow for advertise-time dry-run
    /// (side_effect only, no args). Fail-closed: omit on deny or storage error.
    /// Does not mint tokens, increment spend, or write audit records.
    pub fn advertised_tool_names(&self, store: &SqliteWorkflowStore) -> Vec<String> {
        let catalog = match store.get_tool_catalog(&self.session_id) {
            Ok(entries) => entries,
            Err(_) => return Vec::new(),
        };

        catalog
            .into_iter()
            .filter_map(|entry| {
                // Fail closed: a tool with no row is not advertised.
                let entities = self.entities_for(store, &entry.tool_name).ok()?;
                if self.is_advertisable(&entry.tool_name, entry.side_effect, entities) {
                    Some(entry.tool_name)
                } else {
                    None
                }
            })
            .collect()
    }

    fn is_advertisable(
        &self,
        tool_name: &str,
        side_effect: i32,
        entities: Vec<vam_security::ToolEntity>,
    ) -> bool {
        let eval_context = EvaluationContext {
            side_effect: Some(side_effect as i64),
            ..Default::default()
        };

        let eval_request = EvaluationRequest {
            principal: format!("Agent::\"{}\"", self.agent_urn),
            action: "Action::\"ExecuteTool\"".to_string(),
            resource: format!("Tool::\"{}\"", tool_name),
            context: eval_context.clone(),
            entities,
        };

        if matches!(
            self.pdp.evaluate(&eval_request),
            DecisionOutcome::Allow { .. }
        ) {
            return true;
        }

        // HITL-gated tools: advertise when Cedar would Allow only with hitl_approved,
        // so the model can invoke them and the execute path can Suspend for approval.
        let mut shadow_ctx = eval_context;
        shadow_ctx.hitl_approved = Some(true);
        let shadow_request = EvaluationRequest {
            principal: eval_request.principal,
            action: eval_request.action,
            resource: eval_request.resource,
            context: shadow_ctx,
            entities: eval_request.entities,
        };
        matches!(
            self.pdp.evaluate(&shadow_request),
            DecisionOutcome::Allow { .. }
        )
    }

    fn evaluate_fs_path_limb(
        &self,
        tool_name: &str,
        _base_context: &EvaluationContext,
        args: &serde_json::Value,
    ) -> Result<(), String> {
        let Some(action) = fs_path_limb_action(tool_name) else {
            return Ok(());
        };

        let normalized_path = resolve_fs_path_for_tool(tool_name, args)?;

        let mut path_context = EvaluationContext {
            path: Some(normalized_path.clone()),
            ..Default::default()
        };

        if action == "WriteFile" {
            let size = write_file_size_bytes(tool_name, args).ok_or_else(|| {
                "WriteFile authorization requires file_size_bytes (missing write payload)"
                    .to_string()
            })?;
            path_context.file_size_bytes = Some(size);
        }

        let path_request = EvaluationRequest {
            principal: format!("Agent::\"{}\"", self.agent_urn),
            action: format!("Action::\"{action}\""),
            resource: format!("Path::\"{normalized_path}\""),
            context: path_context,
            entities: Vec::new(),
        };

        match self.pdp.evaluate(&path_request) {
            DecisionOutcome::Allow { .. } => Ok(()),
            DecisionOutcome::Deny {
                diagnostic_message, ..
            } => Err(diagnostic_message),
            DecisionOutcome::Suspend {
                diagnostic_message, ..
            } => Err(diagnostic_message),
        }
    }

    /// True when the call is journaled: a host-mediated outside call, or a code tool that
    /// writes through a connection binding (CN-4.7).
    fn journals(
        &self,
        store: &SqliteWorkflowStore,
        catalog_entry: &crate::session::ToolCatalogEntry,
        tool_name: &str,
    ) -> bool {
        crate::effect_path::is_journaled(
            &catalog_entry.command,
            &catalog_entry.entrypoint,
            tool_name,
        ) || crate::effect_path::is_connection_write(
            store,
            &self.session_id,
            tool_name,
            catalog_entry.side_effect,
        )
    }

    /// The replay branch of the intent path (E10 Phase 4, AR-4.5): a task picked up after a
    /// crash or a restart re-intents the calls of the round the model had already decided. A call
    /// the run already made is answered from its journal row, never decided again and never sent
    /// again.
    ///
    /// Why this is safe: the answer is for the same run (taken from the session's binding, never
    /// from the guest's lease), call id, tool and arguments digest as a row the host itself wrote;
    /// the decision was made, and acted on, when the row was written; the answer is the stored,
    /// already-redacted result. No token is minted, so nothing can be executed with it, and the
    /// execute path and `grants::mint` are never reached.
    ///
    /// * `confirmed`, `failed`: the recorded result or failure (`effect.replayed`).
    /// * `sent`, `unknown`: the unknown-outcome text; a row `sent` with nothing in flight is
    ///   settled `unknown` first. Nothing is re-sent.
    /// * `intended`: not replayed; the call is decided as a new one (an approval keeps its id).
    /// * A call id that has a row for another tool or other arguments is refused: it neither
    ///   replays nor goes on to a second effect under the same id.
    fn replay_from_journal(
        &self,
        store: &SqliteWorkflowStore,
        request: &ToolIntentRequest,
        tool_name: &str,
        args_digest: &str,
    ) -> Option<ToolIntentResponse> {
        use crate::effect_store::EffectState;
        let call_id = request.call_id.as_str();
        if call_id.is_empty() {
            return None;
        }
        let run_id = store
            .find_run_id_for_session(&self.session_id)
            .ok()
            .flatten()?;
        let row = store.effect_get(&run_id, call_id).ok().flatten()?;
        let same_call = row.tool == tool_name && row.args_digest == args_digest;
        if !same_call {
            if row.state == EffectState::Intended && row.tool == tool_name {
                // An approval that amended the arguments keeps its call id; the intent path
                // re-binds the row.
                return None;
            }
            return Some(ToolIntentResponse {
                disposition: Disposition::Denied as i32,
                approval_request_id: None,
                error: Some(ErrorStatus {
                    code: 7, // PERMISSION_DENIED
                    message: format!(
                        "call id '{call_id}' was already used for a different call; \
                         nothing was sent"
                    ),
                    details: vec![],
                    retryable: false,
                }),
                capability_token: None,
                resource_envelope: None,
                replayed: None,
            });
        }
        let row = match row.state {
            EffectState::Intended => return None,
            EffectState::Sent if crate::effect_path::inflight_count(store, &run_id) == 0 => {
                // Nothing is sending it: the answer was lost.
                let _ = store.effect_mark_unknown(&run_id, call_id);
                store.effect_get(&run_id, call_id).ok().flatten()?
            }
            _ => row,
        };
        crate::events::append_run_event(
            store,
            &run_id,
            crate::events::TYPE_EFFECT_REPLAYED,
            &serde_json::json!({ "callId": call_id, "tool": tool_name }),
        );
        Some(ToolIntentResponse {
            disposition: Disposition::Replayed as i32,
            approval_request_id: None,
            error: None,
            capability_token: None,
            resource_envelope: None,
            replayed: Some(crate::effect_path::replay_answer(&row)),
        })
    }

    pub fn handle_tool_intent_request(
        &self,
        store: &SqliteWorkflowStore,
        request: ToolIntentRequest,
    ) -> ToolIntentResponse {
        self.handle_tool_intent_request_gated(
            store,
            request,
            crate::connector_park::ConnectorGate::Clear,
        )
    }

    /// As [`Self::handle_tool_intent_request`], with the connector gate the broker worked out for
    /// the tool's `requiresConnector` (CN-2.5): a refusal or a park, decided before the policy.
    pub(crate) fn handle_tool_intent_request_gated(
        &self,
        store: &SqliteWorkflowStore,
        request: ToolIntentRequest,
        gate: crate::connector_park::ConnectorGate,
    ) -> ToolIntentResponse {
        let tool_name = request.tool_name.clone();

        // 1. Check against host-authoritative catalog
        let catalog_entry = match store.get_tool(&self.session_id, &tool_name) {
            Ok(Some(entry)) => entry,
            Ok(None) => {
                let _ = self.audit_ledger.record_decision(
                    &self.session_id,
                    &format!("Agent::\"{}\"", self.agent_urn),
                    "Action::\"ExecuteTool\"",
                    &format!("Tool::\"{}\"", tool_name),
                    "DENY",
                    vec![],
                    "Tool not found in host-authoritative catalog",
                    &serde_json::json!({}),
                    &self.pdp.policy_bundle_digest(),
                );
                self.record_decision_row(
                    store,
                    crate::run_facts::NewRunDecision {
                        session_id: self.session_id.clone(),
                        tool_name: tool_name.clone(),
                        target: tool_name.clone(),
                        side_effect: 0,
                        disposition: "denied",
                        decided_by: crate::run_facts::decided_by::MISSING_CATALOG_ENTRY,
                        deciding_scope: "host".into(),
                        deciding_rule: "tool not in the host catalog".into(),
                        args_digest: String::new(),
                        approval_request_id: None,
                        via_grant: false,
                        constraint_rule: None,
                    },
                );

                return ToolIntentResponse {
                    disposition: Disposition::Denied as i32,
                    approval_request_id: None,
                    error: Some(ErrorStatus {
                        code: 7, // PERMISSION_DENIED
                        message: format!(
                            "Tool '{}' is not present in the host-authoritative tool catalog",
                            tool_name
                        ),
                        details: vec![],
                        retryable: false,
                    }),
                    capability_token: None,
                    resource_envelope: None,
                    replayed: None,
                };
            }
            Err(e) => {
                return ToolIntentResponse {
                    disposition: Disposition::Denied as i32,
                    approval_request_id: None,
                    error: Some(ErrorStatus {
                        code: 13, // INTERNAL
                        message: format!("Storage error querying tool catalog: {}", e),
                        details: vec![],
                        retryable: false,
                    }),
                    capability_token: None,
                    resource_envelope: None,
                    replayed: None,
                };
            }
        };

        // 1b. Check expected digest against observed digest for registered skills (Tamper detection)
        let observed_script_digest = request
            .script_digest
            .as_ref()
            .map(|d| d.hex_hash.clone())
            .unwrap_or_default();

        if !catalog_entry.digest.is_empty() && catalog_entry.digest != observed_script_digest {
            let diag = format!(
                "Script digest mismatch with host catalog (expected {}, observed {})",
                catalog_entry.digest, observed_script_digest
            );
            let _ = self.audit_ledger.record_decision_with_telemetry(
                &self.session_id,
                &format!("Agent::\"{}\"", self.agent_urn),
                "Action::\"ExecuteTool\"",
                &format!("Tool::\"{}\"", tool_name),
                "DENY",
                vec![],
                &diag,
                &serde_json::json!({
                    "expected_script_digest": catalog_entry.digest,
                    "observed_script_digest": observed_script_digest
                }),
                &self.pdp.policy_bundle_digest(),
                if observed_script_digest.is_empty() {
                    None
                } else {
                    Some(observed_script_digest.clone())
                },
                None,
                None,
                None,
                None,
            );

            self.record_decision_row(
                store,
                crate::run_facts::NewRunDecision {
                    session_id: self.session_id.clone(),
                    tool_name: tool_name.clone(),
                    target: tool_name.clone(),
                    side_effect: catalog_entry.side_effect as i64,
                    disposition: "denied",
                    decided_by: crate::run_facts::decided_by::SCRIPT_DIGEST_MISMATCH,
                    deciding_scope: "host".into(),
                    deciding_rule: "script digest differs from the host catalog".into(),
                    args_digest: String::new(),
                    approval_request_id: None,
                    via_grant: false,
                    constraint_rule: None,
                },
            );

            return ToolIntentResponse {
                disposition: Disposition::Denied as i32,
                approval_request_id: None,
                error: Some(ErrorStatus {
                    code: 7, // PERMISSION_DENIED
                    message: format!(
                        "Skill tamper detected: tool '{}' script digest mismatch (expected {}, observed {})",
                        tool_name, catalog_entry.digest, observed_script_digest
                    ),
                    details: vec![],
                    retryable: false,
                }),
                capability_token: None,
                resource_envelope: None,
                replayed: None,
            };
        }

        // 2. Canonical arguments digest. Same function the guest uses.
        let deny_uncanonical = |err: vam_spec::ArgsDigestError| ToolIntentResponse {
            disposition: Disposition::Denied as i32,
            approval_request_id: None,
            error: Some(ErrorStatus {
                code: 3, // INVALID_ARGUMENT
                message: format!("arguments cannot be canonicalized: {err}"),
                details: vec![],
                retryable: false,
            }),
            capability_token: None,
            resource_envelope: None,
            replayed: None,
        };
        let canonical_args_json = match vam_spec::canonical_args_json(request.arguments.as_ref()) {
            Ok(value) => value,
            Err(err) => return deny_uncanonical(err),
        };
        let computed_digest = match vam_spec::canonical_args_digest(request.arguments.as_ref()) {
            Ok(digest) => digest,
            Err(err) => return deny_uncanonical(err),
        };

        let run_id = request
            .lease
            .as_ref()
            .and_then(|l| l.run_id.as_ref())
            .map(|r| r.urn.as_str())
            .unwrap_or("");

        // A call the run already made is answered from the journal before anything else is
        // decided: no PDP, no decision row, no `tool.invoked`, no token, no grant (E10 Phase 4).
        if let Some(response) =
            self.replay_from_journal(store, &request, &tool_name, &computed_digest)
        {
            return response;
        }

        // A call to an MCP connection that must be signed in again parks until it is
        // reconnected; it is journaled `intended` and never sent (E10 Phase 4, AR-4.9).
        if let Some(response) = self.park_for_reauth(
            store,
            &request,
            &catalog_entry,
            &computed_digest,
            &canonical_args_json,
            run_id,
        ) {
            return response;
        }

        // A skill that needs a connector: refused, or parked until the member connects (CN-2.5).
        if let Some(response) = self.gate_connector(
            store,
            &request,
            &catalog_entry,
            &computed_digest,
            &canonical_args_json,
            run_id,
            gate,
        ) {
            return response;
        }

        let workspace_path = extract_workspace_path(&tool_name, &canonical_args_json);

        let redacted_args = crate::pep_capture::redact_json_value(canonical_args_json.clone());
        let redacted_for_event = if self.telemetry_capture.persist_bodies() {
            Some(redacted_args)
        } else {
            None
        };

        emit_tool_invoked(
            store,
            run_id,
            &tool_name,
            &computed_digest,
            workspace_path.as_deref(),
            self.telemetry_capture,
            self.chunk_store.as_ref(),
            redacted_for_event.as_ref(),
        );

        // 3. Construct evaluation context
        let mut eval_context = EvaluationContext {
            args_digest: Some(computed_digest.clone()),
            ..Default::default()
        };
        if !observed_script_digest.is_empty() {
            eval_context.script_digest = Some(observed_script_digest.clone());
        }
        eval_context.side_effect = Some(catalog_entry.side_effect as i64);

        if let serde_json::Value::Object(ref map) = canonical_args_json {
            if let Some(serde_json::Value::String(ref q)) = map.get("query") {
                eval_context.query = Some(q.clone());
            }
            if let Some(serde_json::Value::String(ref p)) =
                map.get("path").or_else(|| map.get("filepath"))
            {
                eval_context.path = Some(p.clone());
            }
        }

        // Run-level iteration, spend, and tokens (PB-1.2). The session's bound
        // run persists these counters; a later session continues from them.
        // Sessions with no run keep the in-memory counter.
        let (iteration, spend_micro, token_count) = self.metered_eval_counters(store);
        eval_context.iteration = Some(iteration);
        eval_context.spend_micro_amount = Some(spend_micro);
        eval_context.token_count = Some(token_count);
        eval_context.restored_from_snapshot = Some(self.audit_ledger.has_parent_snapshot());
        eval_context.session_lineage_depth = Some(if self.audit_ledger.has_parent_snapshot() {
            1
        } else {
            0
        });
        eval_context.lifecycle_generation = request.lease.as_ref().map(|l| l.lifecycle_generation);

        if self.store_has_grant(store, &tool_name, &computed_digest) {
            eval_context.hitl_approved = Some(true);
        }

        // PB-2.2 / PB-2.3: step rows are Cedar context.
        // Guards stay outside Cedar and are applied after the decision below.
        // `args_within` is part of that deny-only pass, after `allowed_when`.
        let (playbook_view, playbook_guard_error) =
            match load_playbook_pep_view(store, &self.session_id) {
                Ok(view) => (view, None),
                Err(err) => (
                    None,
                    Some(format!("playbook guard state unavailable: {err}")),
                ),
            };
        if let Some(view) = &playbook_view {
            eval_context
                .custom_attributes
                .insert("playbook".to_string(), playbook_context_json(&view.steps));
        }
        let guard_denial = playbook_guard_error.or_else(|| {
            playbook_view.as_ref().and_then(|view| {
                playbook_guard_denial(
                    view,
                    &tool_name,
                    catalog_entry.side_effect,
                    &canonical_args_json,
                )
            })
        });

        // The guard condition that denied, for the record (`None` when the denial
        // is a host error rather than a condition).
        let guard_rule: Option<String> = guard_denial.as_ref().and_then(|_| {
            playbook_view.as_ref().and_then(|view| {
                playbook_guard_rule(
                    view,
                    &tool_name,
                    catalog_entry.side_effect,
                    &canonical_args_json,
                )
            })
        });

        // The guard condition the call passed (evidence for the receipt's gate
        // reference). It is read after the denial check and cannot change it.
        let guard_constraint: Option<String> = if guard_denial.is_none() {
            playbook_view.as_ref().and_then(|view| {
                playbook_guard_constraint(
                    view,
                    &tool_name,
                    catalog_entry.side_effect,
                    &canonical_args_json,
                )
            })
        } else {
            None
        };

        // Permissions mode: the Tool entity comes from the session's own row, and
        // a tool with no row is denied before Cedar runs.
        let target_row = match self.target_row_for(store, &tool_name) {
            Ok(row) => row,
            Err(reason) => {
                self.record_decision_row(
                    store,
                    crate::run_facts::NewRunDecision {
                        session_id: self.session_id.clone(),
                        tool_name: tool_name.clone(),
                        target: tool_name.clone(),
                        side_effect: catalog_entry.side_effect as i64,
                        disposition: "denied",
                        decided_by: if reason.starts_with("Storage error") {
                            crate::run_facts::decided_by::HOST_ERROR
                        } else {
                            crate::run_facts::decided_by::MISSING_PERMISSIONS_ROW
                        },
                        deciding_scope: "session".into(),
                        deciding_rule: "no permission target for the tool".into(),
                        args_digest: computed_digest.clone(),
                        approval_request_id: None,
                        via_grant: false,
                        constraint_rule: None,
                    },
                );
                let _ = self.audit_ledger.record_decision(
                    &self.session_id,
                    &format!("Agent::\"{}\"", self.agent_urn),
                    "Action::\"ExecuteTool\"",
                    &format!("Tool::\"{}\"", tool_name),
                    "DENY",
                    vec![],
                    &reason,
                    &canonical_args_json,
                    &self.pdp.policy_bundle_digest(),
                );
                return ToolIntentResponse {
                    disposition: Disposition::Denied as i32,
                    approval_request_id: None,
                    error: Some(ErrorStatus {
                        code: 7, // PERMISSION_DENIED
                        message: reason,
                        details: vec![],
                        retryable: false,
                    }),
                    capability_token: None,
                    resource_envelope: None,
                    replayed: None,
                };
            }
        };

        let entities = target_row
            .as_ref()
            .map(|row| vec![row.entity()])
            .unwrap_or_default();

        let eval_request = EvaluationRequest {
            principal: format!("Agent::\"{}\"", self.agent_urn),
            action: "Action::\"ExecuteTool\"".to_string(),
            resource: format!("Tool::\"{}\"", tool_name),
            context: eval_context.clone(),
            entities,
        };

        // 4. Evaluate ExecuteTool through PDP, then dual-eval ReadFile/WriteFile for FS primitives.
        let mut outcome = self.pdp.evaluate(&eval_request);
        // What made the decision, for the run's record. Evidence only.
        let mut decided_by = crate::run_facts::decided_by::CEDAR_RULE;

        // Two-pass Cedar Suspend: Deny without hitl_approved, shadow Allow with hitl_approved → Suspend.
        if matches!(outcome, DecisionOutcome::Deny { .. }) {
            let mut shadow_ctx = eval_context.clone();
            shadow_ctx.hitl_approved = Some(true);
            let shadow_request = EvaluationRequest {
                principal: eval_request.principal.clone(),
                action: eval_request.action.clone(),
                resource: eval_request.resource.clone(),
                context: shadow_ctx,
                entities: eval_request.entities.clone(),
            };
            if let DecisionOutcome::Allow { .. } = self.pdp.evaluate(&shadow_request) {
                // A guard blocks the call before HITL. Registering a suspend
                // would ask a human to approve a tool the playbook still denies.
                if let Some(reason) = guard_denial.as_ref() {
                    decided_by = crate::run_facts::decided_by::PLAYBOOK_GUARD;
                    outcome = DecisionOutcome::Deny {
                        diagnostic_message: reason.clone(),
                        matching_policies: vec![],
                    };
                } else {
                    let request_id = self.allocate_hitl_request_id();
                    if let Some(key) = lease_attempt_key(request.lease.as_ref()) {
                        let args_json = canonical_args_json.to_string();
                        // Operator default (1h) unless VAMD_TOOL_HITL_TIMEOUT_SECS
                        // or --tool-hitl-timeout-secs wrote a different lifetime.
                        let timeout_secs = store
                            .tool_hitl_timeout_secs()
                            .unwrap_or(crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS);
                        let expires_at = unix_timestamp().saturating_add(timeout_secs);
                        // A tool whose skill ships an ask card asks with it (CN-3.5). Only this
                        // Cedar suspend carries a card; parks and custom suspends never do.
                        let card = crate::skill_cards::ask_approval_card(
                            &self.session_id,
                            &tool_name,
                            &canonical_args_json,
                        );
                        if let Err(e) = store.register_hitl_suspend_card(
                            &key,
                            &self.session_id,
                            &ResourceId {
                                urn: request_id.clone(),
                            },
                            &tool_name,
                            &args_json,
                            &computed_digest,
                            Some(expires_at),
                            card.as_ref(),
                        ) {
                            decided_by = crate::run_facts::decided_by::HOST_ERROR;
                            outcome = DecisionOutcome::Deny {
                                diagnostic_message: format!("failed to register HITL suspend: {e}"),
                                matching_policies: vec![],
                            };
                        } else {
                            outcome = DecisionOutcome::Suspend {
                                approval_request_id: request_id,
                                diagnostic_message: format!(
                                    "Tool '{}' requires human approval before execution",
                                    tool_name
                                ),
                            };
                        }
                    } else {
                        decided_by = crate::run_facts::decided_by::HOST_ERROR;
                        outcome = DecisionOutcome::Deny {
                            diagnostic_message: "HITL suspend requires an active execution lease"
                                .to_string(),
                            matching_policies: vec![],
                        };
                    }
                }
            }
        }

        // start_playbook: Cedar allowed the call. The host now refuses a start
        // it cannot honour, or parks it for a person to confirm. This only adds
        // a step before the start; it never allows what Cedar refused.
        if tool_name == crate::playbook_activation::START_PLAYBOOK_TOOL && outcome.is_allowed() {
            outcome = self.gate_start_playbook(
                store,
                &request,
                &canonical_args_json,
                &computed_digest,
                outcome,
            );
            if !outcome.is_allowed() {
                decided_by = crate::run_facts::decided_by::GATE;
            }
        }

        // ask_user: Cedar allowed the call. The host checks the question and
        // parks it for the run's owner to answer. It never allows what Cedar
        // refused, and a question is asked each time (no grant is read or kept).
        if tool_name == vam_spec::outbox::ASK_USER_TOOL && outcome.is_allowed() {
            outcome = self.gate_ask_user(store, &request, &computed_digest, outcome);
            if !outcome.is_allowed() {
                decided_by = crate::run_facts::decided_by::GATE;
            }
        }

        // run_step: Cedar allowed the call. When the step's arguments are
        // templated and any value is not user-supplied, the person approves the
        // resolved arguments first. This only adds a step; it never allows what
        // Cedar refused.
        if tool_name == crate::playbook_flight::RUN_STEP_TOOL && outcome.is_allowed() {
            outcome = self.gate_run_step(store, &request, outcome);
            if !outcome.is_allowed() {
                decided_by = crate::run_facts::decided_by::GATE;
            }
        }

        // A non-idempotent call identical to one whose effect may already have happened asks the
        // person first. It only adds a step; it never allows what Cedar refused.
        if outcome.is_allowed() && self.journals(store, &catalog_entry, &tool_name) {
            outcome = self.gate_identical_after_unknown(
                store,
                &request,
                &catalog_entry,
                &computed_digest,
                &canonical_args_json,
                run_id,
                outcome,
            );
            if !outcome.is_allowed() {
                decided_by = crate::run_facts::decided_by::GATE;
            }
        }

        if outcome.is_allowed() {
            if let Err(path_diag) =
                self.evaluate_fs_path_limb(&tool_name, &eval_context, &canonical_args_json)
            {
                decided_by = crate::run_facts::decided_by::PATH_LIMB;
                outcome = DecisionOutcome::Deny {
                    diagnostic_message: path_diag,
                    matching_policies: vec![],
                };
            }
        }

        // Deny-only, after Cedar. A Cedar deny (including the FS limb) is left
        // as-is, so a satisfied guard cannot grant what Cedar refused.
        if !outcome.is_denied() {
            if let Some(reason) = guard_denial.as_ref() {
                decided_by = crate::run_facts::decided_by::PLAYBOOK_GUARD;
                outcome = DecisionOutcome::Deny {
                    diagnostic_message: reason.clone(),
                    matching_policies: vec![],
                };
            }
        }

        let decision_str = match &outcome {
            DecisionOutcome::Allow { .. } => "ALLOW",
            DecisionOutcome::Deny { .. } => "DENY",
            DecisionOutcome::Suspend { .. } => "SUSPEND",
        };

        let matching_policies = match &outcome {
            DecisionOutcome::Allow { matching_policies } => matching_policies.clone(),
            DecisionOutcome::Deny {
                matching_policies, ..
            } => matching_policies.clone(),
            DecisionOutcome::Suspend { .. } => vec![],
        };

        let diag = match &outcome {
            DecisionOutcome::Deny {
                diagnostic_message, ..
            } => diagnostic_message.as_str(),
            DecisionOutcome::Suspend {
                diagnostic_message, ..
            } => diagnostic_message.as_str(),
            _ => "",
        };

        let _ = self.audit_ledger.record_decision_with_telemetry(
            &self.session_id,
            &eval_request.principal,
            &eval_request.action,
            &eval_request.resource,
            decision_str,
            matching_policies.clone(),
            diag,
            &canonical_args_json,
            &self.pdp.policy_bundle_digest(),
            if observed_script_digest.is_empty() {
                None
            } else {
                Some(observed_script_digest.clone())
            },
            None,
            None,
            None,
            None,
        );

        if matches!(outcome, DecisionOutcome::Deny { .. }) {
            vam_telemetry::record_security_denied();
            let ocsf_event = vam_telemetry::ocsf::from_cedar_decision(
                run_id,
                &self.session_id,
                &tool_name,
                "DENY",
                &computed_digest,
            );
            tracing::info!(
                target: "vam.telemetry.security",
                class_uid = ocsf_event.class_uid,
                category_uid = ocsf_event.category_uid,
                activity_id = ocsf_event.activity_id,
                "OCSF security lane authorization denial recorded"
            );
            crate::egress::emit_ocsf(&ocsf_event);
        }

        emit_policy_decided(
            store,
            run_id,
            &tool_name,
            decision_str,
            &computed_digest,
            workspace_path.as_deref(),
        );
        self.record_pep_decision(
            store,
            &eval_request,
            &catalog_entry,
            target_row.as_ref(),
            &outcome,
            decided_by,
            guard_rule.as_deref().or(guard_denial.as_deref()),
            &computed_digest,
            guard_constraint,
        );
        if tool_name == "load_skill" && decision_str == "ALLOW" {
            if let Some(skill_id) = extract_skill_id(&canonical_args_json) {
                emit_skill_loaded(store, run_id, &skill_id);
            }
        }

        // Decide, persist, act: the call is written `intended` before any token is minted or
        // approval is announced. A journal that cannot be written blocks the call (R1).
        if matches!(outcome, DecisionOutcome::Deny { .. })
            && !run_id.is_empty()
            && !request.call_id.is_empty()
            && self.journals(store, &catalog_entry, &tool_name)
        {
            // A call already journaled `intended` (it parked on a reconnect) and now denied
            // never leaves the host: its row is settled, not left waiting.
            if let Ok(Some(row)) = store.effect_get(run_id, &request.call_id) {
                if row.state == crate::effect_store::EffectState::Intended {
                    let _ = store.effect_mark_failed(run_id, &request.call_id, "denied", None);
                }
            }
        }
        let journal_call_id: Option<String> = if matches!(outcome, DecisionOutcome::Deny { .. })
            || !self.journals(store, &catalog_entry, &tool_name)
        {
            None
        } else {
            let approval = match &outcome {
                DecisionOutcome::Suspend {
                    approval_request_id,
                    ..
                } => Some(approval_request_id.clone()),
                _ => None,
            };
            match crate::effect_path::record_intent(&crate::effect_path::IntentFacts {
                store,
                secret: self.effect_secret(),
                session_id: &self.session_id,
                run_id,
                tool: &tool_name,
                adapter_id: &catalog_entry.entrypoint,
                side_effect: catalog_entry.side_effect,
                call_id: &request.call_id,
                args_digest: &computed_digest,
                approval_id: approval.as_deref(),
            }) {
                Ok(id) => id,
                Err(reason) => {
                    tracing::error!(run_id, tool = %tool_name, "effect journal: intended not recorded: {reason}");
                    return ToolIntentResponse {
                        disposition: Disposition::Denied as i32,
                        approval_request_id: None,
                        error: Some(crate::effect_path::unavailable_status()),
                        capability_token: None,
                        resource_envelope: None,
                        replayed: None,
                    };
                }
            }
        };

        match outcome {
            DecisionOutcome::Allow { .. } => {
                let now_secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);

                let lease_id = request
                    .lease
                    .as_ref()
                    .and_then(|l| l.lease_id.as_ref())
                    .map(|id| id.urn.clone())
                    .unwrap_or_default();

                let gen = request
                    .lease
                    .as_ref()
                    .map(|l| l.lifecycle_generation as u64)
                    .unwrap_or(1);

                use std::sync::atomic::{AtomicU64, Ordering};
                static TOKEN_COUNTER: AtomicU64 = AtomicU64::new(1);
                let nonce = format!(
                    "{}-{}",
                    now_secs,
                    TOKEN_COUNTER.fetch_add(1, Ordering::SeqCst)
                );

                // Derive policy resource envelope tier (S2 Phase C).
                // MCP calls do not use this tier's max_wall_time_ms. Their
                // wall time comes from the connection's timeout_ms.
                let tier = if matching_policies
                    .iter()
                    .any(|p| p.contains("heavy") || p.contains("@resource_tier(\"heavy\")"))
                    || catalog_entry.side_effect == 3
                {
                    "heavy"
                } else if matching_policies
                    .iter()
                    .any(|p| p.contains("light") || p.contains("@resource_tier(\"light\")"))
                    || catalog_entry.side_effect == 0
                {
                    "light"
                } else {
                    "standard"
                };

                let resource_envelope = match tier {
                    "heavy" => vam_spec::v1::ResourceEnvelope {
                        max_wall_time_ms: 60_000,
                        max_cpu_time_ms: 30_000,
                        max_memory_bytes: 2 * 1024 * 1024 * 1024, // 2 GiB
                        max_procs: 128,
                        max_open_files: 512,
                        max_file_size_bytes: 500 * 1024 * 1024, // 500 MiB
                        max_egress_bytes: 100 * 1024 * 1024,
                        max_output_bytes: 5 * 1024 * 1024,
                    },
                    "light" => vam_spec::v1::ResourceEnvelope {
                        max_wall_time_ms: 10_000,
                        max_cpu_time_ms: 2_000,
                        max_memory_bytes: 256 * 1024 * 1024, // 256 MiB
                        max_procs: 16,
                        max_open_files: 64,
                        max_file_size_bytes: 10 * 1024 * 1024, // 10 MiB
                        max_egress_bytes: 5 * 1024 * 1024,
                        max_output_bytes: 1024 * 1024,
                    },
                    _ => vam_spec::v1::ResourceEnvelope {
                        max_wall_time_ms: 30_000,
                        max_cpu_time_ms: 10_000,
                        max_memory_bytes: 1024 * 1024 * 1024, // 1 GiB
                        max_procs: 64,
                        max_open_files: 256,
                        max_file_size_bytes: 100 * 1024 * 1024, // 100 MiB
                        max_egress_bytes: 50 * 1024 * 1024,
                        max_output_bytes: 2 * 1024 * 1024,
                    },
                };

                let claims = CapabilityClaims {
                    session_id: self.session_id.clone(),
                    lease_id,
                    lifecycle_generation: gen,
                    tool_name,
                    arguments_digest: computed_digest,
                    script_digest: observed_script_digest,
                    call_id: journal_call_id.unwrap_or_else(|| request.call_id.clone()),
                    resource_envelope: Some(resource_envelope.clone()),
                    expires_at: now_secs + 30,
                    nonce,
                };

                let token_envelope = mint_capability_token(&self.approval_signer, &claims).ok();

                ToolIntentResponse {
                    disposition: Disposition::Authorized as i32,
                    approval_request_id: None,
                    error: None,
                    capability_token: token_envelope,
                    resource_envelope: Some(resource_envelope),
                    replayed: None,
                }
            }
            DecisionOutcome::Suspend {
                approval_request_id,
                diagnostic_message,
            } => {
                ToolIntentResponse {
                    disposition: Disposition::Suspended as i32,
                    approval_request_id: Some(ResourceId {
                        urn: approval_request_id,
                    }),
                    error: Some(ErrorStatus {
                        code: 9, // FAILED_PRECONDITION / SUSPENDED
                        message: diagnostic_message,
                        details: vec![],
                        retryable: true,
                    }),
                    capability_token: None,
                    resource_envelope: None,
                    replayed: None,
                }
            }
            DecisionOutcome::Deny {
                diagnostic_message, ..
            } => {
                ToolIntentResponse {
                    disposition: Disposition::Denied as i32,
                    approval_request_id: None,
                    error: Some(ErrorStatus {
                        code: 7, // PERMISSION_DENIED
                        message: diagnostic_message,
                        details: vec![],
                        retryable: false,
                    }),
                    capability_token: None,
                    resource_envelope: None,
                    replayed: None,
                }
            }
        }
    }

    /// Parks a call to an MCP connection that needs signing in again (E10 Phase 4, AR-4.9).
    ///
    /// Nothing is sent: the call is journaled `intended` with the park's approval id, the run
    /// waits PAUSED (`waiting_for: reauth`) and `connection.needs_reauth` is emitted. A reconnect
    /// resolves the park without a grant (`RECONNECTED`), and the call is then decided like any
    /// other. This runs before the policy decision, so the park never reveals or changes it.
    fn park_for_reauth(
        &self,
        store: &SqliteWorkflowStore,
        request: &ToolIntentRequest,
        catalog_entry: &crate::session::ToolCatalogEntry,
        args_digest: &str,
        args_json: &serde_json::Value,
        run_id: &str,
    ) -> Option<ToolIntentResponse> {
        if run_id.is_empty() || catalog_entry.command != "host" {
            return None;
        }
        let (connection_id, _) =
            vam_spec::validation::parse_mcp_adapter_id(&catalog_entry.entrypoint)?;
        let owner = store.session_owner_user_id(&self.session_id).ok()?;
        let conn =
            crate::mcp_registry::visible_mcp_connection(store, &owner, &connection_id).ok()?;
        if conn.auth_mode != crate::mcp_registry::McpAuthMode::Oauth
            || !crate::host_tools::mcp_connection_needs_reauth(&conn)
        {
            return None;
        }
        self.suspend_for_reauth(
            store,
            request,
            catalog_entry,
            args_digest,
            args_json,
            run_id,
            format!("Reconnect {connection_id} to continue"),
            crate::hitl::reauth_key(conn.scope, &conn.owner_user_id, &conn.connection_id),
            &connection_id,
        )
    }

    /// Parks the call until the connection named by `locked_reason` (a `reauth:` key) is
    /// connected again. Nothing is sent: the call is journaled `intended` with the park's
    /// approval id and the run waits PAUSED. Shared by MCP connections and connector connections.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn suspend_for_reauth(
        &self,
        store: &SqliteWorkflowStore,
        request: &ToolIntentRequest,
        catalog_entry: &crate::session::ToolCatalogEntry,
        args_digest: &str,
        args_json: &serde_json::Value,
        run_id: &str,
        message: String,
        locked_reason: String,
        connection_id: &str,
    ) -> Option<ToolIntentResponse> {
        let key = lease_attempt_key(request.lease.as_ref())?;
        let refuse = |code: i32, message: String| ToolIntentResponse {
            disposition: Disposition::Denied as i32,
            approval_request_id: None,
            error: Some(ErrorStatus {
                code,
                message,
                details: vec![],
                retryable: false,
            }),
            capability_token: None,
            resource_envelope: None,
            replayed: None,
        };
        let approval = crate::hitl::StartPlaybookApproval {
            amendable_schema: serde_json::json!({
                "type": "object", "properties": {}, "additionalProperties": false
            }),
            message: message.clone(),
            locked_reason: Some(locked_reason),
        };
        let request_id = self.allocate_hitl_request_id();
        let timeout_secs = store
            .tool_hitl_timeout_secs()
            .unwrap_or(crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS);
        let expires_at = unix_timestamp().saturating_add(timeout_secs);
        if let Err(e) = store.register_hitl_suspend_custom(
            &key,
            &self.session_id,
            &ResourceId {
                urn: request_id.clone(),
            },
            &request.tool_name,
            &args_json.to_string(),
            args_digest,
            Some(expires_at),
            Some(&approval),
        ) {
            return Some(refuse(
                13,
                format!("failed to register the reconnect wait: {e}"),
            ));
        }
        // Decide, persist, act: the call is `intended` before the wait is announced (R1).
        if let Err(reason) = crate::effect_path::record_intent(&crate::effect_path::IntentFacts {
            store,
            secret: self.effect_secret(),
            session_id: &self.session_id,
            run_id,
            tool: &request.tool_name,
            adapter_id: &catalog_entry.entrypoint,
            side_effect: catalog_entry.side_effect,
            call_id: &request.call_id,
            args_digest,
            approval_id: Some(&request_id),
        }) {
            tracing::error!(run_id, "effect journal: intended not recorded: {reason}");
            return Some(ToolIntentResponse {
                disposition: Disposition::Denied as i32,
                approval_request_id: None,
                error: Some(crate::effect_path::unavailable_status()),
                capability_token: None,
                resource_envelope: None,
                replayed: None,
            });
        }
        crate::events::append_run_event(
            store,
            run_id,
            crate::events::TYPE_CONNECTION_NEEDS_REAUTH,
            &serde_json::json!({ "connectionId": connection_id }),
        );
        Some(ToolIntentResponse {
            disposition: Disposition::Suspended as i32,
            approval_request_id: Some(ResourceId { urn: request_id }),
            error: Some(ErrorStatus {
                code: 9,
                message,
                details: vec![],
                retryable: true,
            }),
            capability_token: None,
            resource_envelope: None,
            replayed: None,
        })
    }

    /// A non-idempotent call identical (same tool and arguments digest) to a row of this run that
    /// is still `unknown`, checked or not, or was confirmed only by a verify read, asks the
    /// person before it is sent: the effect may already have happened ("never twice" wins over
    /// convenience). Runs after Cedar allowed the call; it never allows what Cedar refused. An
    /// approval is a grant for these exact arguments, as for any other ask.
    #[allow(clippy::too_many_arguments)]
    fn gate_identical_after_unknown(
        &self,
        store: &SqliteWorkflowStore,
        request: &ToolIntentRequest,
        catalog_entry: &crate::session::ToolCatalogEntry,
        args_digest: &str,
        args_json: &serde_json::Value,
        run_id: &str,
        allowed: DecisionOutcome,
    ) -> DecisionOutcome {
        if run_id.is_empty() {
            return allowed;
        }
        let class = crate::effect_path::class_for(
            store,
            &self.session_id,
            &catalog_entry.tool_name,
            &catalog_entry.entrypoint,
            catalog_entry.side_effect,
        );
        if class != crate::effect_class::EffectClass::NonIdempotent {
            return allowed;
        }
        let tool = request.tool_name.as_str();
        let Ok(Some(_maybe_done)) =
            store.effect_find_maybe_done(run_id, tool, args_digest, &request.call_id)
        else {
            return allowed;
        };
        if self.store_has_grant(store, tool, args_digest) {
            crate::effect_reconcile::emit_person_allowed(store, run_id, &request.call_id, tool);
            return allowed;
        }
        let deny = |message: String| DecisionOutcome::Deny {
            diagnostic_message: message,
            matching_policies: vec![],
        };
        let Some(key) = lease_attempt_key(request.lease.as_ref()) else {
            return deny("HITL suspend requires an active execution lease".to_string());
        };
        let message = crate::effect_path::ask_again_message(tool);
        let approval = crate::hitl::StartPlaybookApproval {
            amendable_schema: crate::hitl::default_amendable_schema(args_json),
            message,
            locked_reason: Some(
                "an identical call that may already have taken effect is confirmed each time"
                    .to_string(),
            ),
        };
        let request_id = self.allocate_hitl_request_id();
        let timeout_secs = store
            .tool_hitl_timeout_secs()
            .unwrap_or(crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS);
        let expires_at = unix_timestamp().saturating_add(timeout_secs);
        match store.register_hitl_suspend_custom(
            &key,
            &self.session_id,
            &ResourceId {
                urn: request_id.clone(),
            },
            tool,
            &args_json.to_string(),
            args_digest,
            Some(expires_at),
            Some(&approval),
        ) {
            Ok(()) => DecisionOutcome::Suspend {
                approval_request_id: request_id,
                diagnostic_message: format!(
                    "{tool} may already have taken effect; an identical call needs the user's confirmation"
                ),
            },
            Err(e) => deny(format!("failed to register HITL suspend: {e}")),
        }
    }

    /// Decide a `start_playbook` call after Cedar allowed it: refuse (with a
    /// message the model can act on), ask a person, or let it run.
    fn gate_start_playbook(
        &self,
        store: &SqliteWorkflowStore,
        request: &ToolIntentRequest,
        args_json: &serde_json::Value,
        args_digest: &str,
        allowed: DecisionOutcome,
    ) -> DecisionOutcome {
        let deny = |message: String| DecisionOutcome::Deny {
            diagnostic_message: message,
            matching_policies: vec![],
        };
        let run_id = request
            .lease
            .as_ref()
            .and_then(|l| l.run_id.as_ref())
            .map(|r| r.urn.as_str())
            .unwrap_or("");
        let args = request.arguments.clone().unwrap_or_default();
        let plan = match crate::playbook_activation::plan_start_playbook(
            store,
            &self.session_id,
            run_id,
            &args,
        ) {
            Ok(plan) => plan,
            Err(message) => return deny(message),
        };
        if !plan.ask
            || self.store_has_grant(
                store,
                crate::playbook_activation::START_PLAYBOOK_TOOL,
                args_digest,
            )
        {
            return allowed;
        }
        let Some(key) = lease_attempt_key(request.lease.as_ref()) else {
            return deny("HITL suspend requires an active execution lease".to_string());
        };
        let approval = crate::hitl::StartPlaybookApproval {
            amendable_schema: crate::hitl::start_playbook_amendable_schema(
                &plan.urn,
                &plan.name,
                &plan.when,
                &plan.version,
                plan.has_inputs,
                &plan.input_schema,
            ),
            message: crate::hitl::render_start_playbook_message(&plan.name, &plan.when),
            locked_reason: None,
        };
        // The approval shows the validated inputs, in a stable `{urn, inputs}` shape.
        let requested = serde_json::json!({
            "urn": plan.urn,
            "inputs": crate::playbook_activation::struct_to_json(&plan.inputs),
        });
        let _ = args_json;
        // Digest of the normalized arguments, so approving without an edit
        // resumes with arguments that match the stored grant.
        let normalized_digest = crate::playbook_activation::json_to_struct(&requested)
            .and_then(|s| vam_spec::canonical_args_digest(Some(&s)).ok())
            .unwrap_or_else(|| args_digest.to_string());
        let request_id = self.allocate_hitl_request_id();
        let timeout_secs = store
            .tool_hitl_timeout_secs()
            .unwrap_or(crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS);
        let expires_at = unix_timestamp().saturating_add(timeout_secs);
        match store.register_hitl_suspend_custom(
            &key,
            &self.session_id,
            &ResourceId {
                urn: request_id.clone(),
            },
            crate::playbook_activation::START_PLAYBOOK_TOOL,
            &requested.to_string(),
            &normalized_digest,
            Some(expires_at),
            Some(&approval),
        ) {
            Ok(()) => DecisionOutcome::Suspend {
                approval_request_id: request_id,
                diagnostic_message: format!(
                    "Starting playbook '{}' needs the user's confirmation",
                    plan.name
                ),
            },
            Err(e) => deny(format!("failed to register HITL suspend: {e}")),
        }
    }

    /// Decide an `ask_user` call after Cedar allowed it: refuse (with a message
    /// the model can act on) or park it as a question approval.
    fn gate_ask_user(
        &self,
        store: &SqliteWorkflowStore,
        request: &ToolIntentRequest,
        args_digest: &str,
        _allowed: DecisionOutcome,
    ) -> DecisionOutcome {
        let deny = |message: String| DecisionOutcome::Deny {
            diagnostic_message: message,
            matching_policies: vec![],
        };
        let run_id = request
            .lease
            .as_ref()
            .and_then(|l| l.run_id.as_ref())
            .map(|r| r.urn.as_str())
            .unwrap_or("");
        let args = request.arguments.clone().unwrap_or_default();
        let (jig, data) = match crate::outbox::plan_ask_user(store, run_id, &args) {
            Ok(planned) => planned,
            Err(message) => return deny(message),
        };
        let Some(approval) = crate::hitl::question_approval(&jig, &data) else {
            return deny(format!("ask_user cannot ask with '{jig}'"));
        };
        let Some(key) = lease_attempt_key(request.lease.as_ref()) else {
            return deny("HITL suspend requires an active execution lease".to_string());
        };
        let requested = serde_json::json!({ "jig": jig, "data": data });
        let request_id = self.allocate_hitl_request_id();
        let timeout_secs = store
            .tool_hitl_timeout_secs()
            .unwrap_or(crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS);
        let expires_at = unix_timestamp().saturating_add(timeout_secs);
        match store.register_hitl_suspend_custom(
            &key,
            &self.session_id,
            &ResourceId {
                urn: request_id.clone(),
            },
            vam_spec::outbox::ASK_USER_TOOL,
            &requested.to_string(),
            args_digest,
            Some(expires_at),
            Some(&approval),
        ) {
            Ok(()) => DecisionOutcome::Suspend {
                approval_request_id: request_id,
                diagnostic_message: "The agent is waiting for the user's answer".to_string(),
            },
            Err(e) => deny(format!("failed to register HITL suspend: {e}")),
        }
    }

    /// Provenance rule for templated bound steps (see `playbook_flight`).
    fn gate_run_step(
        &self,
        store: &SqliteWorkflowStore,
        request: &ToolIntentRequest,
        allowed: DecisionOutcome,
    ) -> DecisionOutcome {
        let run_id = request
            .lease
            .as_ref()
            .and_then(|l| l.run_id.as_ref())
            .map(|r| r.urn.as_str())
            .unwrap_or("");
        let step_id = request
            .arguments
            .as_ref()
            .and_then(|args| args.fields.get("step_id"))
            .and_then(|v| match v.kind.as_ref() {
                Some(prost_types::value::Kind::StringValue(text)) => Some(text.clone()),
                _ => None,
            })
            .unwrap_or_default();
        // A step that cannot be planned fails at execution with a message for the model.
        let Ok(plan) =
            crate::playbook_flight::plan_bound_step(store, &self.session_id, run_id, &step_id)
        else {
            return allowed;
        };
        if !plan.ask
            || self.store_has_grant(
                store,
                crate::playbook_flight::RUN_STEP_TOOL,
                &plan.grant_digest,
            )
        {
            return allowed;
        }
        let deny = |message: String| DecisionOutcome::Deny {
            diagnostic_message: message,
            matching_policies: vec![],
        };
        let Some(key) = lease_attempt_key(request.lease.as_ref()) else {
            return deny("HITL suspend requires an active execution lease".to_string());
        };
        let approval = crate::hitl::StartPlaybookApproval {
            amendable_schema: serde_json::json!({
                "type": "object",
                "properties": { "step_id": { "type": "string", "enum": [plan.step_id] } },
                "required": ["step_id"],
                "x-vam-run-step": {
                    "tool": plan.tool_name,
                    "step_id": plan.step_id,
                    "reason": plan.ask_reason,
                },
            }),
            message: format!(
                "Run '{}' for playbook step '{}' with these arguments? {}\nThe arguments are shown as the host resolved them.",
                plan.tool_name, plan.step_id, plan.ask_reason
            ),
            locked_reason: None,
        };
        let request_id = self.allocate_hitl_request_id();
        let timeout_secs = store
            .tool_hitl_timeout_secs()
            .unwrap_or(crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS);
        let expires_at = unix_timestamp().saturating_add(timeout_secs);
        match store.register_hitl_suspend_custom(
            &key,
            &self.session_id,
            &ResourceId {
                urn: request_id.clone(),
            },
            crate::playbook_flight::RUN_STEP_TOOL,
            &plan.requested.to_string(),
            &plan.grant_digest,
            Some(expires_at),
            Some(&approval),
        ) {
            Ok(()) => DecisionOutcome::Suspend {
                approval_request_id: request_id,
                diagnostic_message: format!(
                    "Step '{}' needs the user's approval of its arguments",
                    plan.step_id
                ),
            },
            Err(e) => deny(format!("failed to register HITL suspend: {e}")),
        }
    }

    fn store_has_grant(
        &self,
        store: &SqliteWorkflowStore,
        tool_name: &str,
        args_digest: &str,
    ) -> bool {
        store.has_hitl_grant(&self.session_id, tool_name, args_digest)
    }

    pub(crate) fn allocate_hitl_request_id(&self) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static HITL_COUNTER: AtomicU64 = AtomicU64::new(1);
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let seq = HITL_COUNTER.fetch_add(1, Ordering::SeqCst);
        format!("urn:vam:hitl:local.vam.dev/acme/req-{now}-{seq}")
    }
}

pub(crate) fn lease_attempt_key(
    lease: Option<&ExecutionLease>,
) -> Option<crate::error::AttemptKey> {
    let lease = lease?;
    let run_id = lease.run_id.as_ref()?.urn.clone();
    let attempt_id = lease.attempt_id.as_ref()?.urn.clone();
    if run_id.is_empty() || attempt_id.is_empty() || lease.step_id.is_empty() {
        return None;
    }
    Some(crate::error::AttemptKey {
        run_id,
        step_id: lease.step_id.clone(),
        attempt_id,
    })
}

fn unix_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

/// Step rows and guards for the session's bound run.
///
/// `Ok(None)` when the session has no run, or the run has no playbook. That
/// path leaves `context.playbook` unset and skips guards.
pub(crate) struct PlaybookPepView {
    pub(crate) steps: Vec<PlaybookStepRecord>,
    pub(crate) guards: Vec<PlaybookGuard>,
}

fn load_playbook_pep_view(
    store: &SqliteWorkflowStore,
    session_id: &str,
) -> Result<Option<PlaybookPepView>, OrchestratorError> {
    let Some(run_id) = store.find_run_id_for_session(session_id)? else {
        return Ok(None);
    };
    if run_id.is_empty() || store.get_playbook_run(&run_id)?.is_none() {
        return Ok(None);
    }
    let spec = store.get_playbook_spec(&run_id)?;
    let steps = store.list_playbook_steps(&run_id)?;
    Ok(Some(PlaybookPepView {
        steps,
        guards: spec.guards,
    }))
}

fn playbook_context_json(steps: &[PlaybookStepRecord]) -> serde_json::Value {
    serde_json::Value::Array(
        steps
            .iter()
            .map(|step| {
                serde_json::json!({
                    "id": step.step_id,
                    "state": step.state.as_str(),
                    "outcome": step.outcome,
                })
            })
            .collect(),
    )
}

/// First matching guard that denies the call.
///
/// A guard matches when the tool name is listed or the catalog side-effect
/// class is listed. An empty list matches nothing. `allowed_when` is the AND
/// of `after` tokens (bare step id, or `id:outcome`) and is checked first.
/// Each `args_within` entry then requires a supplied tool argument to be a
/// member of the referenced step output. An argument the call does not carry
/// is not a miss. A guard never grants a call.
pub(crate) fn playbook_guard_denial(
    view: &PlaybookPepView,
    tool_name: &str,
    side_effect: i32,
    arguments: &serde_json::Value,
) -> Option<String> {
    for guard in &view.guards {
        if !guard_targets(guard, tool_name, side_effect) {
            continue;
        }
        if let Some(reason) = allowed_when_denial(&view.steps, &guard.allowed_when) {
            return Some(reason);
        }
        if let Some(reason) = args_within_denial(&view.steps, &guard.args_within, arguments) {
            return Some(reason);
        }
    }
    None
}

/// The guard condition that denies the call, as the receipt names it
/// (`allowed_when confirm:go`, `args_within issue_key in steps.x.output`).
/// It walks the guards exactly as [`playbook_guard_denial`] does, so the two
/// agree on which condition failed. No argument value appears in it.
pub(crate) fn playbook_guard_rule(
    view: &PlaybookPepView,
    tool_name: &str,
    side_effect: i32,
    arguments: &serde_json::Value,
) -> Option<String> {
    for guard in &view.guards {
        if !guard_targets(guard, tool_name, side_effect) {
            continue;
        }
        if let Some(token) = guard
            .allowed_when
            .iter()
            .find(|token| precondition_denial(&view.steps, token).is_some())
        {
            return Some(format!("allowed_when {token}"));
        }
        if args_within_denial(&view.steps, &guard.args_within, arguments).is_some() {
            let hit = guard.args_within.iter().find(|c| {
                !c.argument.is_empty()
                    && supplied_argument(arguments, &c.argument)
                        .is_some_and(|v| !argument_within(&view.steps, &c.within, &v))
            });
            if let Some(c) = hit {
                return Some(format!("args_within {} in {}", c.argument, c.within));
            }
        }
    }
    None
}

/// The guard condition a call PASSED, in the form the receipt names it:
/// `args_within <arg> in <ref>` for a supplied constrained argument, else
/// `allowed_when <token>` for a guard that gated the call on a step. `None`
/// when no guard constrained it. Call only when [`playbook_guard_denial`] is
/// `None`. No argument value appears in the result.
pub(crate) fn playbook_guard_constraint(
    view: &PlaybookPepView,
    tool_name: &str,
    side_effect: i32,
    arguments: &serde_json::Value,
) -> Option<String> {
    let mut gated = None;
    for guard in &view.guards {
        if !guard_targets(guard, tool_name, side_effect) {
            continue;
        }
        if let Some(c) = guard
            .args_within
            .iter()
            .find(|c| !c.argument.is_empty() && supplied_argument(arguments, &c.argument).is_some())
        {
            return Some(format!("args_within {} in {}", c.argument, c.within));
        }
        if gated.is_none() {
            gated = guard
                .allowed_when
                .first()
                .map(|token| format!("allowed_when {token}"));
        }
    }
    gated
}

fn guard_targets(guard: &PlaybookGuard, tool_name: &str, side_effect: i32) -> bool {
    guard.tools.iter().any(|name| name == tool_name)
        || guard
            .side_effects
            .iter()
            .copied()
            .any(|class| class == side_effect)
}

fn allowed_when_denial(steps: &[PlaybookStepRecord], tokens: &[String]) -> Option<String> {
    for token in tokens {
        if let Some(reason) = precondition_denial(steps, token) {
            return Some(reason);
        }
    }
    None
}

/// `None` when the token holds. Otherwise a reason naming the step and the
/// required outcome (`completed` for a bare step id).
fn precondition_denial(steps: &[PlaybookStepRecord], token: &str) -> Option<String> {
    let Some((step_id, required)) = split_allowed_when(token) else {
        return Some(format!("blocked until step `{token}` is `completed`"));
    };
    let outcome_label = required.unwrap_or("completed");
    let satisfied = steps
        .iter()
        .find(|step| step.step_id == step_id)
        .is_some_and(|step| {
            step.state == PlaybookStepState::Completed
                && required.is_none_or(|expected| step.outcome == expected)
        });
    if satisfied {
        None
    } else {
        Some(format!(
            "blocked until step `{step_id}` is `{outcome_label}`"
        ))
    }
}

/// Same syntax as playbook `after`: bare step id, or `id:outcome`.
fn split_allowed_when(token: &str) -> Option<(&str, Option<&str>)> {
    if token.is_empty() {
        return None;
    }
    if let Some((step_id, outcome)) = token.split_once(':') {
        if step_id.is_empty() || outcome.is_empty() || outcome.contains(':') {
            return None;
        }
        return Some((step_id, Some(outcome)));
    }
    Some((token, None))
}

/// Deny when a supplied argument is not a member of the gate output path.
///
/// An empty list is the PB-2.2 condition-only guard. A call that omits the
/// named argument is unchanged. A present value must be a member of the value
/// at `within` (`steps.<id>.output…`, including `[*]`). A missing step output,
/// a path that does not resolve, or `[*]` on a non-array is a miss.
fn args_within_denial(
    steps: &[PlaybookStepRecord],
    constraints: &[GuardArgsWithin],
    arguments: &serde_json::Value,
) -> Option<String> {
    for constraint in constraints {
        if constraint.argument.is_empty() {
            continue;
        }
        let Some(candidates) = supplied_argument(arguments, &constraint.argument) else {
            continue;
        };
        if !argument_within(steps, &constraint.within, &candidates) {
            return Some(format!(
                "blocked by guard: argument `{}` is not within `{}`",
                constraint.argument, constraint.within
            ));
        }
    }
    None
}

/// `None` when the call does not carry `argument`. `Some` is every value that
/// must be a member of the gate output, including a `[*]` expansion.
fn supplied_argument(
    arguments: &serde_json::Value,
    argument: &str,
) -> Option<Vec<serde_json::Value>> {
    let path = argument_path(argument);
    match walk_path(arguments, &path) {
        Ok(values) => Some(values),
        Err(PathFault::Missing) => None,
        Err(PathFault::NonArray | PathFault::Malformed) => Some(Vec::new()),
    }
}

fn argument_path(argument: &str) -> String {
    let argument = argument.trim();
    if argument.starts_with('.') || argument.starts_with('[') {
        argument.to_string()
    } else {
        format!(".{argument}")
    }
}

fn argument_within(
    steps: &[PlaybookStepRecord],
    within: &str,
    candidates: &[serde_json::Value],
) -> bool {
    if candidates.is_empty() {
        return false;
    }
    let Some(set) = membership_set(steps, within) else {
        return false;
    };
    candidates
        .iter()
        .all(|candidate| set.iter().any(|member| member == candidate))
}

/// Values the argument must belong to. `[*]` fans out across an array. A
/// single array with no fan-out is that array's elements.
fn membership_set(steps: &[PlaybookStepRecord], within: &str) -> Option<Vec<serde_json::Value>> {
    let (step_id, path) = split_output_ref(within)?;
    let output_json = steps
        .iter()
        .find(|step| step.step_id == step_id)
        .and_then(|step| step.output_json.as_deref())?;
    let output: serde_json::Value = serde_json::from_str(output_json).ok()?;
    let values = walk_path(&output, &path).ok()?;
    if values.len() == 1 {
        if let Some(items) = values[0].as_array() {
            return Some(items.clone());
        }
    }
    Some(values)
}

/// `steps.<id>.output<path>`, optionally wrapped in `{{ }}`.
fn split_output_ref(raw: &str) -> Option<(String, String)> {
    let trimmed = raw.trim();
    let body = if let Some(inner) = trimmed.strip_prefix("{{") {
        let inner = inner.trim().strip_suffix("}}")?.trim();
        if inner.contains("{{") || inner.contains("}}") {
            return None;
        }
        inner
    } else if trimmed.contains("{{") || trimmed.contains("}}") {
        return None;
    } else {
        trimmed
    };
    let rest = body.strip_prefix("steps.")?;
    let output_at = rest.find(".output")?;
    let id = &rest[..output_at];
    if id.is_empty()
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    let path = &rest[output_at + ".output".len()..];
    if path.is_empty() || path.starts_with('.') || path.starts_with('[') {
        Some((id.to_string(), path.to_string()))
    } else {
        None
    }
}

#[derive(Debug)]
enum PathFault {
    Missing,
    NonArray,
    Malformed,
}

/// `.field`, `[n]`, and `[*]` against one JSON value. `[*]` requires an array.
fn walk_path(value: &serde_json::Value, path: &str) -> Result<Vec<serde_json::Value>, PathFault> {
    if path.is_empty() {
        return Ok(vec![value.clone()]);
    }
    if let Some(rest) = path.strip_prefix("[*]") {
        let items = value.as_array().ok_or(PathFault::NonArray)?;
        let mut out = Vec::new();
        for item in items {
            out.extend(walk_path(item, rest)?);
        }
        return Ok(out);
    }
    if let Some(rest) = path.strip_prefix('[') {
        let (digits, after) = rest.split_once(']').ok_or(PathFault::Malformed)?;
        if digits.is_empty() || !digits.chars().all(|ch| ch.is_ascii_digit()) {
            return Err(PathFault::Malformed);
        }
        let index: usize = digits.parse().map_err(|_| PathFault::Malformed)?;
        let item = value
            .as_array()
            .ok_or(PathFault::NonArray)?
            .get(index)
            .ok_or(PathFault::Missing)?;
        return walk_path(item, after);
    }
    if let Some(rest) = path.strip_prefix('.') {
        let end = rest
            .find(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-')
            .unwrap_or(rest.len());
        if end == 0 {
            return Err(PathFault::Malformed);
        }
        let child = value
            .as_object()
            .ok_or(PathFault::Missing)?
            .get(&rest[..end])
            .ok_or(PathFault::Missing)?;
        return walk_path(child, &rest[end..]);
    }
    Err(PathFault::Malformed)
}

#[cfg(test)]
const EXAMPLE_CHAOS_POLICY: &str = include_str!("../../../examples/chaos-assistant/policy.cedar");
#[cfg(test)]
const EXAMPLE_SQLITE_POLICY: &str = include_str!("../../../examples/sqlite-assistant/policy.cedar");
#[cfg(test)]
const EXAMPLE_JIRA_POLICY: &str = include_str!("../../../examples/jira-assistant/policy.cedar");
#[cfg(test)]
const EXAMPLE_BUN_POLICY: &str = include_str!("../../../examples/bun-assistant/policy.cedar");

#[cfg(test)]
mod advertise_tests {
    use super::*;
    use crate::session::{builtin_tool_catalog, ToolCatalogEntry};
    use crate::workflow::SqliteWorkflowStore;
    use prost_types::Struct;
    use std::sync::Arc;
    use vam_inference::SessionBudget;
    use vam_spec::v1::tool_intent_response::Disposition;
    use vam_spec::v1::{Digest, ExecutionLease, ResourceId, ToolIntentRequest};

    fn create_test_lease(session_id: &str) -> ExecutionLease {
        ExecutionLease {
            run_id: Some(ResourceId {
                urn: "urn:vam:run:local.vam.dev/acme/jira-hitl".to_string(),
            }),
            step_id: "step-jira-hitl".to_string(),
            attempt_id: Some(ResourceId {
                urn: "urn:vam:attempt:local.vam.dev/acme/jira-hitl".to_string(),
            }),
            lease_id: Some(ResourceId {
                urn: format!("urn:vam:lease:local.vam.dev/acme/{session_id}"),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1, 2, 3],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: "idem-jira-hitl".to_string(),
        }
    }

    fn test_security_ctx(
        session_id: &str,
        agent_urn: &str,
        policy: &str,
    ) -> SessionSecurityContext {
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

    fn seed_catalog(store: &SqliteWorkflowStore, session_id: &str, extra: &[ToolCatalogEntry]) {
        store
            .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
            .expect("create session");
        let mut catalog = builtin_tool_catalog();
        catalog.extend_from_slice(extra);
        store
            .record_tool_catalog(session_id, &catalog)
            .expect("seed catalog");
    }

    #[test]
    fn chaos_policy_advertises_read_only_tools_and_skills() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-chaos";
        seed_catalog(
            &store,
            session_id,
            &[
                ToolCatalogEntry {
                    tool_name: "probe_sandbox".to_string(),
                    command: "skill".to_string(),
                    entrypoint: "probe_sandbox".to_string(),
                    digest: String::new(),
                    side_effect: 0,
                    skill_name: String::new(),
                    approval_required: false,
                },
                ToolCatalogEntry {
                    tool_name: "check_env".to_string(),
                    command: "skill".to_string(),
                    entrypoint: "check_env".to_string(),
                    digest: String::new(),
                    side_effect: 0,
                    skill_name: String::new(),
                    approval_required: false,
                },
            ],
        );

        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:local.vam.dev/vam/chaos-assistant",
            EXAMPLE_CHAOS_POLICY,
        );
        let advertised = ctx.advertised_tool_names(&store);
        assert!(advertised.contains(&"read_file".to_string()));
        assert!(advertised.contains(&"glob".to_string()));
        assert!(advertised.contains(&"probe_sandbox".to_string()));
        assert!(advertised.contains(&"check_env".to_string()));
        assert!(!advertised.contains(&"write_file".to_string()));
        assert!(!advertised.contains(&"run".to_string()));
        assert!(!advertised.contains(&"grep".to_string()));
    }

    #[test]
    fn sqlite_query_db_advertised_and_drop_denied_at_execute() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-sqlite";
        seed_catalog(
            &store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "query_db".to_string(),
                command: "skill".to_string(),
                entrypoint: "query_db".to_string(),
                digest: String::new(),
                side_effect: 1,
                skill_name: String::new(),
                approval_required: false,
            }],
        );

        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:local.vam.dev/vam/sqlite-assistant",
            EXAMPLE_SQLITE_POLICY,
        );
        let advertised = ctx.advertised_tool_names(&store);
        assert!(
            advertised.contains(&"query_db".to_string()),
            "query_db should be name-level permitted: {:?}",
            advertised
        );

        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "query".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "DROP TABLE facts".to_string(),
                )),
            },
        );

        let resp = ctx.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: None,
                tool_name: "query_db".to_string(),
                arguments: Some(args),
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        );
        assert_eq!(
            resp.disposition,
            Disposition::Denied as i32,
            "DROP should be denied at execute time"
        );
        assert!(resp.capability_token.is_none());
    }

    #[test]
    fn jira_mutate_tools_advertised_for_hitl_intercept() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-jira";
        seed_catalog(
            &store,
            session_id,
            &[
                ToolCatalogEntry {
                    tool_name: "jira_search_issues".to_string(),
                    command: "skill".to_string(),
                    entrypoint: "jira_search_issues".to_string(),
                    digest: String::new(),
                    side_effect: 1,
                    skill_name: String::new(),
                    approval_required: false,
                },
                ToolCatalogEntry {
                    tool_name: "jira_create_issue".to_string(),
                    command: "skill".to_string(),
                    entrypoint: "jira_create_issue".to_string(),
                    digest: String::new(),
                    side_effect: 3,
                    skill_name: String::new(),
                    approval_required: false,
                },
                ToolCatalogEntry {
                    tool_name: "jira_transition_issue".to_string(),
                    command: "skill".to_string(),
                    entrypoint: "jira_transition_issue".to_string(),
                    digest: String::new(),
                    side_effect: 3,
                    skill_name: String::new(),
                    approval_required: false,
                },
            ],
        );

        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:local.vam.dev/vam/jira-assistant",
            EXAMPLE_JIRA_POLICY,
        );
        let run_id = ResourceId {
            urn: "urn:vam:run:local.vam.dev/acme/jira-hitl".to_string(),
        };
        store.create_run(&run_id).expect("run");
        let lease = create_test_lease(session_id);
        store.issue_lease(&lease).expect("lease");
        let advertised = ctx.advertised_tool_names(&store);
        assert!(advertised.contains(&"jira_create_issue".to_string()));
        assert!(advertised.contains(&"jira_transition_issue".to_string()));
        assert!(advertised.contains(&"jira_search_issues".to_string()));

        let req = ToolIntentRequest {
            lease: Some(create_test_lease(session_id)),
            tool_name: "jira_transition_issue".to_string(),
            arguments: None,
            arguments_digest: None,
            script_digest: Some(Digest {
                algorithm: 1,
                hex_hash: "d-transition".to_string(),
            }),
            call_id: String::new(),
        };
        let resp = ctx.handle_tool_intent_request(&store, req);
        assert_eq!(resp.disposition, Disposition::Suspended as i32);
        assert!(resp.approval_request_id.is_some());
    }

    #[test]
    fn jira_transition_authorized_after_hitl_grant() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-jira-grant";
        seed_catalog(
            &store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "jira_transition_issue".to_string(),
                command: "skill".to_string(),
                entrypoint: "jira_transition_issue".to_string(),
                digest: String::new(),
                side_effect: 3,
                skill_name: String::new(),
                approval_required: false,
            }],
        );

        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:local.vam.dev/vam/jira-assistant",
            EXAMPLE_JIRA_POLICY,
        );
        let run_id = ResourceId {
            urn: "urn:vam:run:local.vam.dev/acme/jira-hitl".to_string(),
        };
        store.create_run(&run_id).expect("run");
        let lease = create_test_lease(session_id);
        store.issue_lease(&lease).expect("lease");

        let mut args = Struct::default();
        args.fields.insert(
            "issue_key".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "CGA-1298".to_string(),
                )),
            },
        );
        args.fields.insert(
            "transition_id".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "In Progress".to_string(),
                )),
            },
        );
        let args_json = serde_json::json!({
            "issue_key": "CGA-1298",
            "transition_id": "In Progress",
        });
        let digest = crate::events::args_digest_from_json(&args_json);

        let req = ToolIntentRequest {
            lease: Some(create_test_lease(session_id)),
            tool_name: "jira_transition_issue".to_string(),
            arguments: Some(args),
            arguments_digest: None,
            script_digest: Some(Digest {
                algorithm: 1,
                hex_hash: "d-transition".to_string(),
            }),
            call_id: String::new(),
        };

        let suspended = ctx.handle_tool_intent_request(&store, req.clone());
        assert_eq!(suspended.disposition, Disposition::Suspended as i32);

        {
            let connection = store.connection.lock().expect("lock");
            connection
                .execute(
                    "INSERT INTO hitl_grants (session_id, tool_name, args_digest) VALUES (?1, ?2, ?3)",
                    rusqlite::params![session_id, "jira_transition_issue", digest],
                )
                .expect("grant");
        }

        let authorized = ctx.handle_tool_intent_request(&store, req);
        assert_eq!(authorized.disposition, Disposition::Authorized as i32);
        assert!(authorized.capability_token.is_some());
    }

    #[test]
    fn jira_policy_workspace_id_principal_does_not_advertise() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-jira-ws";
        seed_catalog(&store, session_id, &[]);
        let ctx = test_security_ctx(session_id, "urn:vam:agent:jira-test-9", EXAMPLE_JIRA_POLICY);
        let advertised = ctx.advertised_tool_names(&store);
        assert!(
            advertised.is_empty(),
            "workspace-derived principal must not match jira policy: {advertised:?}"
        );
    }

    #[test]
    fn bun_policy_does_not_advertise_arbitrary_catalog_names() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-bun-adv";
        seed_catalog(
            &store,
            session_id,
            &[
                ToolCatalogEntry {
                    tool_name: "calculator".to_string(),
                    command: "skill".to_string(),
                    entrypoint: "calculator".to_string(),
                    digest: String::new(),
                    side_effect: 0,
                    skill_name: String::new(),
                    approval_required: false,
                },
                ToolCatalogEntry {
                    tool_name: "evil_tool".to_string(),
                    command: "skill".to_string(),
                    entrypoint: "evil".to_string(),
                    digest: String::new(),
                    side_effect: 0,
                    skill_name: String::new(),
                    approval_required: false,
                },
            ],
        );

        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:bun-assistant",
            EXAMPLE_BUN_POLICY,
        );
        let advertised = ctx.advertised_tool_names(&store);
        assert!(!advertised.contains(&"evil_tool".to_string()));
        assert!(advertised.contains(&"read_file".to_string()));
        assert!(advertised.contains(&"calculator".to_string()));
    }

    #[test]
    fn host_tool_advertised_when_policy_permits() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-host-adv";
        seed_catalog(
            &store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "host_echo".to_string(),
                command: "host".to_string(),
                entrypoint: "echo".to_string(),
                digest: vam_spec::validation::host_tool_digest("echo"),
                side_effect: 1,
                skill_name: String::new(),
                approval_required: false,
            }],
        );

        let policy = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"host_echo"
);
"#;
        let ctx = test_security_ctx(session_id, "urn:vam:agent:bun-assistant", policy);
        let advertised = ctx.advertised_tool_names(&store);
        assert!(advertised.contains(&"host_echo".to_string()));
    }

    #[test]
    fn host_hitl_tool_advertised_for_intercept() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-host-hitl";
        seed_catalog(
            &store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "host_danger".to_string(),
                command: "host".to_string(),
                entrypoint: "fail".to_string(),
                digest: vam_spec::validation::host_tool_digest("fail"),
                side_effect: 3,
                skill_name: String::new(),
                approval_required: false,
            }],
        );

        let policy = r#"
permit (
    principal == Agent::"urn:vam:agent:local.vam.dev/vam/jira-assistant",
    action == Action::"ExecuteTool",
    resource == Tool::"host_danger"
)
when {
    context has hitl_approved &&
    context.hitl_approved == true
};
"#;
        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:local.vam.dev/vam/jira-assistant",
            policy,
        );
        let advertised = ctx.advertised_tool_names(&store);
        assert!(advertised.contains(&"host_danger".to_string()));
    }

    #[test]
    fn advertise_dry_run_does_not_increment_iteration_counter() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-counter";
        seed_catalog(&store, session_id, &[]);

        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:local.vam.dev/vam/chaos-assistant",
            EXAMPLE_CHAOS_POLICY,
        );
        let before = ctx
            .iteration_counter
            .load(std::sync::atomic::Ordering::SeqCst);
        let _ = ctx.advertised_tool_names(&store);
        let after = ctx
            .iteration_counter
            .load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            before, after,
            "advertise dry-run must not touch iteration counter"
        );
    }
}

#[cfg(test)]
mod hitl_policy_tests {
    use super::*;
    use crate::session::{builtin_tool_catalog, ToolCatalogEntry};
    use crate::workflow::SqliteWorkflowStore;
    use std::sync::Arc;
    use vam_inference::SessionBudget;
    use vam_spec::v1::agent_spec::InstructionSource;
    use vam_spec::v1::tool_intent_response::Disposition;
    use vam_spec::v1::{
        AgentSpec, CapabilityPolicyRef, ExecutionLease, HitlPolicyRef, ResourceId, ResourceRef,
        ToolIntentRequest, Version,
    };

    fn test_security_ctx(
        session_id: &str,
        agent_urn: &str,
        policy: &str,
    ) -> SessionSecurityContext {
        SessionSecurityContext::new(
            session_id.to_string(),
            agent_urn.to_string(),
            policy,
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[9u8; 32]),
            None,
            Arc::new(SessionBudget::new(None)),
        )
        .expect("security context")
    }

    fn test_lease(session_id: &str, run_id: &str) -> ExecutionLease {
        ExecutionLease {
            run_id: Some(ResourceId {
                urn: run_id.to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(ResourceId {
                urn: format!("urn:vam:attempt:local.vam.dev/acme/{session_id}"),
            }),
            lease_id: Some(ResourceId {
                urn: format!("urn:vam:lease:local.vam.dev/acme/{session_id}"),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: format!("idem-{session_id}"),
        }
    }

    fn seed_session(
        store: &SqliteWorkflowStore,
        session_id: &str,
        extra: &[ToolCatalogEntry],
        run_id: &str,
    ) {
        store
            .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
            .expect("create session");
        let mut catalog = builtin_tool_catalog();
        catalog.extend_from_slice(extra);
        store
            .record_tool_catalog(session_id, &catalog)
            .expect("seed catalog");
        store
            .create_run(&ResourceId {
                urn: run_id.to_string(),
            })
            .expect("run");
        store
            .issue_lease(&test_lease(session_id, run_id))
            .expect("lease");
    }

    fn test_agent(urn: &str, policy_urn: Option<&str>, monitored: &[&str]) -> AgentSpec {
        AgentSpec {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            display_name: "Test agent".to_string(),
            description: "test".to_string(),
            role: "tester".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions(
                "test instructions".to_string(),
            )),
            capability_policy: policy_urn.map(|policy| CapabilityPolicyRef {
                policy_ref: Some(ResourceRef {
                    resource_id: Some(ResourceId {
                        urn: policy.to_string(),
                    }),
                    version: Some(Version {
                        major: 1,
                        minor: 0,
                        patch: 0,
                        prerelease: String::new(),
                        build: String::new(),
                    }),
                    digest: None,
                    locators: vec![],
                }),
                security_profile_name: String::new(),
            }),
            hitl_policy: Some(HitlPolicyRef {
                policy_ref: None,
                monitored_tools: monitored.iter().map(|s| (*s).to_string()).collect(),
            }),
            allow_any_playbook: true,
            ..Default::default()
        }
    }

    fn bind_run(store: &SqliteWorkflowStore, session_id: &str, run_id: &str, agent_urn: &str) {
        store
            .persist_run_spec(
                run_id,
                "{}",
                Some(session_id),
                agent_urn,
                None,
                "User::\"tester\"",
            )
            .expect("bind run");
    }

    #[test]
    fn overlay_for_non_jira_agent_suspends_on_pass_one() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-overlay-docs";
        let agent_urn = "urn:vam:agent:docs-assistant";
        let run_id = "urn:vam:run:local.vam.dev/acme/overlay-docs";
        seed_session(
            &store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "publish_doc".to_string(),
                command: "skill".to_string(),
                entrypoint: "publish_doc".to_string(),
                digest: String::new(),
                side_effect: 3,
                skill_name: String::new(),
                approval_required: false,
            }],
            run_id,
        );

        let overlay = derive_hitl_overlay(agent_urn, &["publish_doc".to_string()]);
        let policy = compose_policy(DEFAULT_BUILTIN_FALLBACK_POLICY, &overlay);
        let ctx = test_security_ctx(session_id, agent_urn, &policy);
        let resp = ctx.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(test_lease(session_id, run_id)),
                tool_name: "publish_doc".to_string(),
                arguments: None,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        );
        assert_eq!(resp.disposition, Disposition::Suspended as i32);
        assert!(resp.approval_request_id.is_some());
    }

    #[test]
    fn overlay_forbids_unapproved_call_that_base_policy_permits() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-overlay-tighten";
        let agent_urn = "urn:vam:agent:docs-assistant";
        let run_id = "urn:vam:run:local.vam.dev/acme/overlay-tighten";
        seed_session(
            &store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "drop_table".to_string(),
                command: "skill".to_string(),
                entrypoint: "drop_table".to_string(),
                digest: String::new(),
                side_effect: 3,
                skill_name: String::new(),
                approval_required: false,
            }],
            run_id,
        );

        let base = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"drop_table"
);
"#;
        let ctx_open = test_security_ctx(session_id, agent_urn, base);
        let open = ctx_open.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(test_lease(session_id, run_id)),
                tool_name: "drop_table".to_string(),
                arguments: None,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        );
        assert_eq!(open.disposition, Disposition::Authorized as i32);

        let overlay = derive_hitl_overlay(agent_urn, &["drop_table".to_string()]);
        let tightened = compose_policy(base, &overlay);
        let ctx_tight = test_security_ctx(session_id, agent_urn, &tightened);
        let gated = ctx_tight.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(test_lease(session_id, run_id)),
                tool_name: "drop_table".to_string(),
                arguments: None,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        );
        assert_eq!(gated.disposition, Disposition::Suspended as i32);
    }

    #[test]
    fn monitored_tools_without_capability_policy_still_suspend() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_catalog_schema().expect("catalog");
        let session_id = "sess-overlay-only";
        let agent_urn = "urn:vam:agent:local.vam.dev/acme/overlay-only";
        let run_id = "urn:vam:run:local.vam.dev/acme/overlay-only";
        seed_session(
            &store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "ship_release".to_string(),
                command: "skill".to_string(),
                entrypoint: "ship_release".to_string(),
                digest: String::new(),
                side_effect: 3,
                skill_name: String::new(),
                approval_required: false,
            }],
            run_id,
        );
        store
            .put_agent(&test_agent(agent_urn, None, &["ship_release"]))
            .expect("agent");
        bind_run(&store, session_id, run_id, agent_urn);

        let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).unwrap();
        assert_eq!(resolved.kind, PolicyResolutionKind::NoPolicyDeclared);
        assert!(resolved.cedar.contains("ship_release"));

        let ctx = test_security_ctx(session_id, agent_urn, &resolved.cedar);
        let resp = ctx.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(test_lease(session_id, run_id)),
                tool_name: "ship_release".to_string(),
                arguments: None,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        );
        assert_eq!(resp.disposition, Disposition::Suspended as i32);
    }

    #[test]
    fn declared_but_missing_policy_uses_builtins_and_warns() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_catalog_schema().expect("catalog");
        let session_id = "sess-missing-policy";
        let agent_urn = "urn:vam:agent:local.vam.dev/acme/missing-policy";
        let run_id = "urn:vam:run:local.vam.dev/acme/missing-policy";
        let missing_urn = "urn:vam:policy:local.vam.dev/acme/does-not-exist";
        store
            .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
            .expect("session");
        // Released without the freeze: a release refuses a missing policy, but a
        // policy can vanish after release and the session must still start.
        store
            .put_agent_released_with_owner(
                &test_agent(agent_urn, Some(missing_urn), &["drop_table"]),
                crate::users::LOCAL_USER_ID,
            )
            .expect("agent");
        bind_run(&store, session_id, run_id, agent_urn);

        let resolved = resolve_session_policy(&store, session_id, Some(agent_urn)).unwrap();
        assert_eq!(resolved.kind, PolicyResolutionKind::DeclaredButMissing);
        assert_eq!(resolved.policy_urn.as_deref(), Some(missing_urn));
        assert!(resolved.warning.is_some());
        assert!(resolved.cedar.contains("read_file"));
        assert!(resolved.cedar.contains("drop_table"));
        CedarPolicyEngine::from_default_schema(&resolved.cedar, PolicyMode::Enforce, 1)
            .expect("composed builtins plus overlay must compile");

        surface_policy_resolution(&store, session_id, agent_urn, &resolved);
        let events = store.read_outbox_for_run(run_id, 0).expect("outbox");
        assert!(
            events
                .iter()
                .any(|e| e.event_type == EVENT_POLICY_RESOLUTION),
            "missing policy must surface on the run: {events:?}"
        );
    }

    #[test]
    fn composed_base_plus_overlay_compiles_under_strict_validation() {
        let overlay = derive_hitl_overlay(
            "urn:vam:agent:local.vam.dev/vam/sqlite-assistant",
            &["insert_fact".to_string()],
        );
        let composed = compose_policy(EXAMPLE_SQLITE_POLICY, &overlay);
        CedarPolicyEngine::from_default_schema(&composed, PolicyMode::Enforce, 1)
            .expect("jira-style overlay on sqlite base must compile");
        SessionSecurityContext::new(
            "sess-compile".to_string(),
            "urn:vam:agent:local.vam.dev/vam/sqlite-assistant".to_string(),
            &composed,
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[11u8; 32]),
            None,
            Arc::new(SessionBudget::new(None)),
        )
        .expect("SessionSecurityContext validates under Strict");
    }

    #[test]
    fn overlay_skips_tool_names_that_break_cedar_literals() {
        let overlay = derive_hitl_overlay(
            "urn:vam:agent:docs-assistant",
            &[
                r#"evil";permit (principal, action, resource);"#.to_string(),
                "Not Valid".to_string(),
                "ok_tool".to_string(),
            ],
        );
        assert!(overlay.contains("Tool::\"ok_tool\""));
        assert!(!overlay.contains("evil"));
        assert!(!overlay.contains("Not Valid"));
        CedarPolicyEngine::from_default_schema(
            &compose_policy(DEFAULT_BUILTIN_FALLBACK_POLICY, &overlay),
            PolicyMode::Enforce,
            1,
        )
        .expect("sanitized overlay must compile");
    }

    #[test]
    fn resolve_policy_for_urn_is_builtins_only() {
        let src = resolve_policy_for_urn("urn:vam:policy:jira-egress-only");
        assert_eq!(src, DEFAULT_BUILTIN_FALLBACK_POLICY);
        assert!(!src.contains("jira_create_issue"));
    }

    #[test]
    fn fresh_store_does_not_seed_example_sample_policies() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        for (urn, _) in EXAMPLE_SAMPLE_POLICIES {
            assert!(
                matches!(
                    store.get_policy(urn),
                    Err(crate::error::OrchestratorError::NotFound(_))
                ),
                "{urn} must not be catalog-seeded on open"
            );
        }
    }

    #[test]
    fn sqlite_insert_fact_hitl_flow_with_overlay() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-sqlite-hitl";
        let agent_urn = "urn:vam:agent:local.vam.dev/vam/sqlite-assistant";
        let run_id = "urn:vam:run:local.vam.dev/acme/sqlite-hitl";
        seed_session(
            &store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "insert_fact".to_string(),
                command: "skill".to_string(),
                entrypoint: "insert_fact".to_string(),
                digest: String::new(),
                side_effect: 2,
                skill_name: String::new(),
                approval_required: false,
            }],
            run_id,
        );

        let overlay = derive_hitl_overlay(agent_urn, &["insert_fact".to_string()]);
        let policy = compose_policy(EXAMPLE_SQLITE_POLICY, &overlay);
        let ctx = test_security_ctx(session_id, agent_urn, &policy);

        let advertised = ctx.advertised_tool_names(&store);
        assert!(advertised.contains(&"insert_fact".to_string()));

        let req = ToolIntentRequest {
            lease: Some(test_lease(session_id, run_id)),
            tool_name: "insert_fact".to_string(),
            arguments: None,
            arguments_digest: None,
            script_digest: None,
            call_id: String::new(),
        };
        let suspended = ctx.handle_tool_intent_request(&store, req.clone());
        assert_eq!(suspended.disposition, Disposition::Suspended as i32);

        let digest = crate::events::args_digest_from_json(&serde_json::Value::Null);
        {
            let connection = store.connection.lock().expect("lock");
            connection
                .execute(
                    "INSERT INTO hitl_grants (session_id, tool_name, args_digest) VALUES (?1, ?2, ?3)",
                    rusqlite::params![session_id, "insert_fact", digest],
                )
                .expect("grant");
        }
        let authorized = ctx.handle_tool_intent_request(&store, req);
        assert_eq!(authorized.disposition, Disposition::Authorized as i32);
        assert!(authorized.capability_token.is_some());
    }

    fn suspend_insert_fact(store: &SqliteWorkflowStore, session_id: &str, run_id: &str) {
        let agent_urn = "urn:vam:agent:local.vam.dev/vam/sqlite-assistant";
        seed_session(
            store,
            session_id,
            &[ToolCatalogEntry {
                tool_name: "insert_fact".to_string(),
                command: "skill".to_string(),
                entrypoint: "insert_fact".to_string(),
                digest: String::new(),
                side_effect: 2,
                skill_name: String::new(),
                approval_required: false,
            }],
            run_id,
        );
        let overlay = derive_hitl_overlay(agent_urn, &["insert_fact".to_string()]);
        let policy = compose_policy(EXAMPLE_SQLITE_POLICY, &overlay);
        let ctx = test_security_ctx(session_id, agent_urn, &policy);
        let suspended = ctx.handle_tool_intent_request(
            store,
            ToolIntentRequest {
                lease: Some(test_lease(session_id, run_id)),
                tool_name: "insert_fact".to_string(),
                arguments: None,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        );
        assert_eq!(suspended.disposition, Disposition::Suspended as i32);
    }

    fn pending_expires_at(store: &SqliteWorkflowStore, session_id: &str) -> i64 {
        let connection = store.connection.lock().expect("lock");
        connection
            .query_row(
                "SELECT expires_at FROM hitl_pending WHERE session_id = ?1",
                [session_id],
                |row| row.get(0),
            )
            .expect("pending expiry")
    }

    #[test]
    fn tool_hitl_timeout_defaults_to_one_hour_and_is_configurable() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let before = unix_timestamp();
        suspend_insert_fact(
            &store,
            "sess-hitl-default",
            "urn:vam:run:local.vam.dev/acme/hitl-default",
        );
        let after = unix_timestamp();
        let expires = pending_expires_at(&store, "sess-hitl-default");
        let default_secs = crate::allowed_models::DEFAULT_TOOL_HITL_TIMEOUT_SECS;
        assert!(
            (before + default_secs..=after + default_secs).contains(&expires),
            "default expiry {expires} not in [{}, {}]",
            before + default_secs,
            after + default_secs
        );

        let configured = SqliteWorkflowStore::in_memory().expect("store");
        configured
            .set_tool_hitl_timeout_secs(90)
            .expect("operator timeout");
        assert!(configured.set_tool_hitl_timeout_secs(0).is_err());
        assert_eq!(
            configured.tool_hitl_timeout_secs().expect("read"),
            90,
            "a rejected write leaves the operator setting in place"
        );
        let before = unix_timestamp();
        suspend_insert_fact(
            &configured,
            "sess-hitl-90",
            "urn:vam:run:local.vam.dev/acme/hitl-90",
        );
        let after = unix_timestamp();
        let expires = pending_expires_at(&configured, "sess-hitl-90");
        assert!(
            (before + 90..=after + 90).contains(&expires),
            "configured expiry {expires} not in [{}, {}]",
            before + 90,
            after + 90
        );
        assert!(
            expires < before + default_secs,
            "configured timeout must replace the hardcoded +3600 path"
        );
    }

    #[test]
    fn playbook_overlay_allows_step_complete_only_when_playbook_run_exists() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.initialize_catalog_schema().expect("catalog");
        let session_id = "sess-playbook-overlay";
        let agent_urn = "urn:vam:agent:local.vam.dev/acme/playbook-agent";
        let run_id = "urn:vam:run:local.vam.dev/acme/playbook-overlay";
        store
            .create_session(
                session_id,
                None,
                "vm-playbook",
                "ws-playbook",
                "storage-default",
            )
            .expect("session");
        let mut catalog = builtin_tool_catalog();
        catalog.push(ToolCatalogEntry {
            tool_name: "step_complete".to_string(),
            command: "host".to_string(),
            entrypoint: "step_complete".to_string(),
            digest: vam_spec::validation::host_tool_digest("step_complete"),
            side_effect: 2,
            skill_name: String::new(),
            approval_required: false,
        });
        catalog.push(ToolCatalogEntry {
            tool_name: "run_step".to_string(),
            command: "host".to_string(),
            entrypoint: "run_step".to_string(),
            digest: vam_spec::validation::host_tool_digest("run_step"),
            side_effect: 2,
            skill_name: String::new(),
            approval_required: false,
        });
        catalog.push(ToolCatalogEntry {
            tool_name: "check_playbook_finish".to_string(),
            command: "host".to_string(),
            entrypoint: "check_playbook_finish".to_string(),
            digest: vam_spec::validation::host_tool_digest("check_playbook_finish"),
            side_effect: 1,
            skill_name: String::new(),
            approval_required: false,
        });
        store
            .record_tool_catalog(session_id, &catalog)
            .expect("catalog");
        store
            .put_agent(&test_agent(agent_urn, None, &[]))
            .expect("agent");
        // Bind the run with session_id first (INSERT OR IGNORE); do not create_run
        // beforehand or session_id stays unset and find_run_id_for_session misses.
        bind_run(&store, session_id, run_id, agent_urn);

        let without = resolve_session_policy(&store, session_id, Some(agent_urn)).unwrap();
        assert!(!without.cedar.contains("step_complete"));
        assert!(!without.cedar.contains("run_step"));
        assert!(!without.cedar.contains("check_playbook_finish"));
        let ctx_without = test_security_ctx(session_id, agent_urn, &without.cedar);
        assert!(!ctx_without
            .advertised_tool_names(&store)
            .contains(&"step_complete".to_string()));
        assert!(!ctx_without
            .advertised_tool_names(&store)
            .contains(&"run_step".to_string()));
        assert!(!ctx_without
            .advertised_tool_names(&store)
            .contains(&"check_playbook_finish".to_string()));

        store
            .init_playbook_run(
                run_id,
                &crate::workflow::PlaybookRunPin {
                    urn: "urn:vam:playbook:local.vam.dev/acme/demo".to_string(),
                    version: "1.0.0".to_string(),
                    digest: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .to_string(),
                },
                &vam_spec::v1::PlaybookSpec {
                    steps: vec![vam_spec::v1::PlaybookStep {
                        id: "first".to_string(),
                        instruction: "do first".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .expect("init playbook");

        let with = resolve_session_policy(&store, session_id, Some(agent_urn)).unwrap();
        assert!(
            with.cedar.contains("step_complete"),
            "expected playbook overlay in cedar: {}",
            with.cedar
        );
        assert!(
            with.cedar.contains("run_step"),
            "expected run_step overlay in cedar: {}",
            with.cedar
        );
        assert!(
            with.cedar.contains("check_playbook_finish"),
            "expected finish-check overlay in cedar: {}",
            with.cedar
        );
        CedarPolicyEngine::from_default_schema(&with.cedar, PolicyMode::Enforce, 1)
            .expect("playbook overlay must compile");
        let ctx_with = test_security_ctx(session_id, agent_urn, &with.cedar);
        assert!(ctx_with
            .advertised_tool_names(&store)
            .contains(&"step_complete".to_string()));
        assert!(ctx_with
            .advertised_tool_names(&store)
            .contains(&"run_step".to_string()));
        assert!(ctx_with
            .advertised_tool_names(&store)
            .contains(&"check_playbook_finish".to_string()));
        let authorized = ctx_with.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(test_lease(session_id, run_id)),
                tool_name: "step_complete".to_string(),
                arguments: None,
                arguments_digest: None,
                script_digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: vam_spec::validation::host_tool_digest("step_complete"),
                }),
                call_id: String::new(),
            },
        );
        assert_eq!(authorized.disposition, Disposition::Authorized as i32);
    }
}

#[cfg(test)]
mod pep_path_tests {
    use super::*;
    use crate::session::builtin_tool_catalog;
    use crate::workflow::SqliteWorkflowStore;
    use std::sync::Arc;
    use vam_inference::SessionBudget;
    use vam_spec::v1::ToolIntentRequest;
    use vam_spec::validation::builtin_tool_digest;

    fn test_security_ctx(
        session_id: &str,
        agent_urn: &str,
        policy: &str,
    ) -> SessionSecurityContext {
        SessionSecurityContext::new(
            session_id.to_string(),
            agent_urn.to_string(),
            policy,
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[8u8; 32]),
            None,
            Arc::new(SessionBudget::new(None)),
        )
        .expect("security context")
    }

    fn seed_bun_session(store: &SqliteWorkflowStore, session_id: &str) {
        store
            .create_session(session_id, None, "vm-test", "ws-test", "storage-default")
            .expect("create session");
        store
            .record_tool_catalog(session_id, &builtin_tool_catalog())
            .expect("seed catalog");
    }

    fn bind_run(store: &SqliteWorkflowStore, session_id: &str, run_id: &str, agent_urn: &str) {
        store
            .persist_run_spec(
                run_id,
                "{}",
                Some(session_id),
                agent_urn,
                None,
                "User::\"tester\"",
            )
            .expect("bind run");
    }

    fn test_lease(session_id: &str, run_id: &str) -> vam_spec::v1::ExecutionLease {
        vam_spec::v1::ExecutionLease {
            run_id: Some(vam_spec::v1::ResourceId {
                urn: run_id.to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(vam_spec::v1::ResourceId {
                urn: format!("urn:vam:attempt:local.vam.dev/acme/{session_id}"),
            }),
            lease_id: Some(vam_spec::v1::ResourceId {
                urn: format!("urn:vam:lease:local.vam.dev/acme/{session_id}"),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: format!("idem-{session_id}"),
        }
    }

    fn path_request(tool_name: &str, path: &str, content: Option<&str>) -> ToolIntentRequest {
        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "path".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(path.to_string())),
            },
        );
        if let Some(text) = content {
            args.fields.insert(
                "content".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(text.to_string())),
                },
            );
        }
        ToolIntentRequest {
            lease: None,
            tool_name: tool_name.to_string(),
            arguments: Some(args),
            arguments_digest: None,
            script_digest: Some(vam_spec::v1::Digest {
                algorithm: 1,
                hex_hash: builtin_tool_digest(tool_name),
            }),
            call_id: String::new(),
        }
    }

    #[test]
    fn normalize_cedar_path_maps_workspace_and_skills() {
        assert_eq!(
            normalize_cedar_path("notes/hello.txt").unwrap(),
            "/mnt/drive9/workspace/notes/hello.txt"
        );
        assert_eq!(
            normalize_cedar_path("/mnt/drive9/skills/db-tools/SCHEMA.md").unwrap(),
            "/mnt/drive9/skills/db-tools/SCHEMA.md"
        );
        assert!(normalize_cedar_path("../etc/passwd").is_err());
        assert!(normalize_cedar_path("/etc/passwd").is_err());
    }

    #[test]
    fn write_file_skills_path_denied_by_pep() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-pep-skills-write";
        seed_bun_session(&store, session_id);
        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:bun-assistant",
            EXAMPLE_BUN_POLICY,
        );

        let resp = ctx.handle_tool_intent_request(
            &store,
            path_request(
                "write_file",
                "/mnt/drive9/skills/db-tools/evil.txt",
                Some("pwn"),
            ),
        );
        assert_eq!(resp.disposition, Disposition::Denied as i32);
        assert!(resp.capability_token.is_none());
    }

    #[test]
    fn write_file_workspace_allowed_by_pep() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-pep-workspace-write";
        seed_bun_session(&store, session_id);
        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:bun-assistant",
            EXAMPLE_BUN_POLICY,
        );

        let resp = ctx.handle_tool_intent_request(
            &store,
            path_request("write_file", "notes/hello.txt", Some("hello")),
        );
        assert_eq!(
            resp.disposition,
            Disposition::Authorized as i32,
            "write_file workspace: {:?}",
            resp.error
        );
        assert!(resp.capability_token.is_some());
    }

    #[test]
    fn write_file_over_10mb_denied_by_pep() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-pep-large-write";
        seed_bun_session(&store, session_id);
        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:bun-assistant",
            EXAMPLE_BUN_POLICY,
        );

        let huge = "x".repeat(11 * 1024 * 1024);
        let resp = ctx.handle_tool_intent_request(
            &store,
            path_request("write_file", "notes/large.bin", Some(&huge)),
        );
        assert_eq!(resp.disposition, Disposition::Denied as i32);
        assert!(resp.capability_token.is_none());
    }

    #[test]
    fn read_file_skills_schema_allowed_by_pep() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-pep-skills-read";
        seed_bun_session(&store, session_id);
        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:bun-assistant",
            EXAMPLE_BUN_POLICY,
        );

        let resp = ctx.handle_tool_intent_request(
            &store,
            path_request("read_file", "/mnt/drive9/skills/db-tools/SCHEMA.md", None),
        );
        assert_eq!(resp.disposition, Disposition::Authorized as i32);
        assert!(resp.capability_token.is_some());
    }

    #[test]
    fn pep_tool_invoked_redacted_full_includes_preview_not_secrets() {
        use crate::trajectory::EVENT_TOOL_INVOKED;
        use tempfile::tempdir;
        use vam_storage::{ChunkStore, Drive9Layout};
        use vam_telemetry::TelemetryCapture;

        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-args-capture";
        let run_id = "urn:vam:run:local.vam.dev/acme/args-capture";
        seed_bun_session(&store, session_id);
        bind_run(
            &store,
            session_id,
            run_id,
            "urn:vam:agent:local.vam.dev/vam/chaos-assistant",
        );

        let dir = tempdir().expect("tempdir");
        let chunk_store = ChunkStore::new(Drive9Layout::new(dir.path().join("drive9")));

        let mut ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:local.vam.dev/vam/chaos-assistant",
            EXAMPLE_CHAOS_POLICY,
        );
        ctx.configure_pep_telemetry(TelemetryCapture::RedactedFull, chunk_store);

        let secret = "sk-live-abcdefghijklmnopqrst";
        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "pattern".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue("**/*.rs".to_string())),
            },
        );
        args.fields.insert(
            "token".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(secret.to_string())),
            },
        );

        let _resp = ctx.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(test_lease(session_id, run_id)),
                tool_name: "glob".to_string(),
                arguments: Some(args),
                arguments_digest: None,
                script_digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: builtin_tool_digest("glob"),
                }),
                call_id: String::new(),
            },
        );

        let events = store.read_outbox_for_run(run_id, 0).expect("outbox");
        let invoked = events
            .iter()
            .find(|e| e.event_type == EVENT_TOOL_INVOKED)
            .expect("tool.invoked");
        assert!(invoked.payload.contains("args_preview"));
        assert!(invoked.payload.contains("args_cas"));
        assert!(invoked.payload.contains("**/*.rs"));
        assert!(
            !invoked.payload.contains(secret),
            "secret must be redacted before outbox: {}",
            invoked.payload
        );
    }

    #[test]
    fn pep_tool_invoked_none_omits_arg_bodies() {
        use crate::trajectory::EVENT_TOOL_INVOKED;

        let store = SqliteWorkflowStore::in_memory().expect("store");
        let session_id = "sess-args-none";
        let run_id = "urn:vam:run:local.vam.dev/acme/args-none";
        seed_bun_session(&store, session_id);
        bind_run(
            &store,
            session_id,
            run_id,
            "urn:vam:agent:local.vam.dev/vam/chaos-assistant",
        );

        let ctx = test_security_ctx(
            session_id,
            "urn:vam:agent:local.vam.dev/vam/chaos-assistant",
            EXAMPLE_CHAOS_POLICY,
        );

        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "pattern".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue("*.md".to_string())),
            },
        );

        let _resp = ctx.handle_tool_intent_request(
            &store,
            ToolIntentRequest {
                lease: Some(test_lease(session_id, run_id)),
                tool_name: "glob".to_string(),
                arguments: Some(args),
                arguments_digest: None,
                script_digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: builtin_tool_digest("glob"),
                }),
                call_id: String::new(),
            },
        );

        let events = store.read_outbox_for_run(run_id, 0).expect("outbox");
        let invoked = events
            .iter()
            .find(|e| e.event_type == EVENT_TOOL_INVOKED)
            .expect("tool.invoked");
        assert!(!invoked.payload.contains("args_preview"));
        assert!(!invoked.payload.contains("args_cas"));
    }
}

#[cfg(test)]
mod policy_schema_tests {
    use super::*;
    use vam_security::{CedarPolicyEngine, PolicyMode};

    #[test]
    fn default_policies_and_examples_compile_against_schema() {
        CedarPolicyEngine::from_default_schema(
            DEFAULT_BUILTIN_FALLBACK_POLICY,
            PolicyMode::Enforce,
            1,
        )
        .unwrap_or_else(|e| panic!("builtin fallback must compile: {e}"));
        for (urn, src) in EXAMPLE_SAMPLE_POLICIES {
            CedarPolicyEngine::from_default_schema(src, PolicyMode::Enforce, 1).unwrap_or_else(
                |e| panic!("sample policy '{urn}' must compile against vam.cedarschema: {e}"),
            );
        }
    }
}

#[cfg(test)]
mod run_metering_tests {
    use super::*;
    use crate::session::ToolCatalogEntry;
    use crate::workflow::SqliteWorkflowStore;
    use std::sync::Arc;
    use vam_inference::SessionBudget;
    use vam_security::PolicyMode;
    use vam_spec::v1::tool_intent_response::Disposition;
    use vam_spec::v1::{Digest, ExecutionLease, ResourceId, ToolIntentRequest, ToolIntentResponse};

    const AGENT: &str = "urn:vam:agent:meter";
    const RUN: &str = "urn:vam:run:local.vam.dev/acme/meter";
    const OTHER_RUN: &str = "urn:vam:run:local.vam.dev/acme/meter-other";
    const POLICY: &str = r#"
        permit(
            principal == Agent::"urn:vam:agent:meter",
            action == Action::"ExecuteTool",
            resource
        ) when {
            context has iteration && context.iteration <= 1
            && context has spend_micro_amount && context.spend_micro_amount < 1000
            && context has token_count && context.token_count < 1000
        };
    "#;

    fn lease(session_id: &str, run_id: &str) -> ExecutionLease {
        ExecutionLease {
            run_id: Some(ResourceId {
                urn: run_id.to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(ResourceId {
                urn: format!("urn:vam:attempt:{session_id}"),
            }),
            lease_id: Some(ResourceId {
                urn: format!("urn:vam:lease:{session_id}"),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: format!("idem-{session_id}"),
        }
    }

    fn request(session_id: &str, run_id: &str) -> ToolIntentRequest {
        ToolIntentRequest {
            lease: Some(lease(session_id, run_id)),
            tool_name: "step_tool".to_string(),
            arguments: None,
            arguments_digest: None,
            script_digest: Some(Digest {
                algorithm: 1,
                hex_hash: "digest_step".to_string(),
            }),
            call_id: String::new(),
        }
    }

    fn ctx(session_id: &str, budget: Arc<SessionBudget>) -> SessionSecurityContext {
        SessionSecurityContext::new(
            session_id.to_string(),
            AGENT.to_string(),
            POLICY,
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
            None,
            budget,
        )
        .expect("security context")
    }

    fn seed(store: &SqliteWorkflowStore, session_id: &str) {
        store
            .create_session(
                session_id,
                None,
                &format!("vm-{session_id}"),
                "ws-test",
                "storage-default",
            )
            .expect("create session");
        store
            .record_tool_catalog(
                session_id,
                &[ToolCatalogEntry {
                    tool_name: "step_tool".to_string(),
                    command: "python3".to_string(),
                    entrypoint: "step.py".to_string(),
                    digest: "digest_step".to_string(),
                    side_effect: 1,
                    skill_name: String::new(),
                    approval_required: false,
                }],
            )
            .expect("catalog");
    }

    fn denial(response: &ToolIntentResponse) -> String {
        response
            .error
            .as_ref()
            .map(|err| err.message.clone())
            .unwrap_or_default()
    }

    #[test]
    fn pep_enforces_persisted_counters_after_session_change() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        seed(&store, "sess-a");
        seed(&store, "sess-b");
        seed(&store, "sess-c");
        store
            .persist_run_spec(RUN, "{}", Some("sess-a"), AGENT, None, "User::\"tester\"")
            .expect("bind run");

        let budget_a = Arc::new(SessionBudget::new(None));
        budget_a
            .record_usage(6, 4, 1_000_000, 1_000_000)
            .expect("usage");
        let ctx_a = ctx("sess-a", budget_a);
        let first = ctx_a.handle_tool_intent_request(&store, request("sess-a", RUN));
        assert_eq!(
            first.disposition,
            Disposition::Authorized as i32,
            "{}",
            denial(&first)
        );

        let stored = store.get_run_metering(RUN).expect("read").expect("row");
        assert_eq!(stored.iteration, 1);
        assert_eq!(stored.spend_micro, 10);
        assert_eq!(stored.token_count, 10);
        assert_eq!(stored.prompt_tokens, 6);
        assert_eq!(stored.completion_tokens, 4);

        // A new session starts with an empty budget. After the run is rebound,
        // the PEP must still see the run's iteration and spend, so this call
        // is over the iteration cap.
        store
            .set_run_session_id(RUN, "sess-b")
            .expect("rebind session");
        let budget_b = Arc::new(SessionBudget::new(None));
        assert_eq!(budget_b.get_accumulated_cost(), 0);
        let ctx_b = ctx("sess-b", budget_b.clone());
        let second = ctx_b.handle_tool_intent_request(&store, request("sess-b", RUN));
        assert_eq!(
            second.disposition,
            Disposition::Denied as i32,
            "{}",
            denial(&second)
        );
        assert_eq!(budget_b.get_accumulated_cost(), 10);
        assert_eq!(budget_b.get_accumulated_tokens(), 10);
        assert_eq!(budget_b.get_accumulated_prompt_tokens(), 6);
        assert_eq!(budget_b.get_accumulated_completion_tokens(), 4);

        let stored = store.get_run_metering(RUN).expect("read").expect("row");
        assert_eq!(stored.iteration, 2);
        assert_eq!(stored.spend_micro, 10);
        assert_eq!(stored.token_count, 10);

        store
            .persist_run_spec(
                OTHER_RUN,
                "{}",
                Some("sess-c"),
                AGENT,
                None,
                "User::\"tester\"",
            )
            .expect("bind other run");
        let budget_c = Arc::new(SessionBudget::new(None));
        let ctx_c = ctx("sess-c", budget_c.clone());
        let other = ctx_c.handle_tool_intent_request(&store, request("sess-c", OTHER_RUN));
        assert_eq!(
            other.disposition,
            Disposition::Authorized as i32,
            "{}",
            denial(&other)
        );
        let other_row = store
            .get_run_metering(OTHER_RUN)
            .expect("read")
            .expect("row");
        assert_eq!(other_row.iteration, 1);
        assert_eq!(other_row.spend_micro, 0);
        assert_eq!(budget_c.get_accumulated_cost(), 0);
    }
}

#[cfg(test)]
mod playbook_guard_tests {
    use super::*;
    use crate::session::ToolCatalogEntry;
    use crate::workflow::{PlaybookRunPin, PlaybookStepCompletion};
    use std::sync::Arc;
    use vam_inference::SessionBudget;
    use vam_spec::v1::tool_intent_response::Disposition;
    use vam_spec::v1::{
        ExecutionLease, GuardArgsWithin, PlaybookGuard, PlaybookSpec, PlaybookStep, ResourceId,
        SideEffectClass, StepKind, StepOutcome, ToolIntentRequest,
    };

    const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/guard-agent";

    const PERMIT_TOOLS: &str = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_transition_issue"
);
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_search_issues"
);
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"post_comment"
);
"#;

    fn ctx(session_id: &str, policy: &str) -> SessionSecurityContext {
        SessionSecurityContext::new(
            session_id.to_string(),
            AGENT.to_string(),
            policy,
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[4u8; 32]),
            None,
            Arc::new(SessionBudget::new(None)),
        )
        .expect("security context")
    }

    fn lease(session_id: &str, run_id: &str) -> ExecutionLease {
        ExecutionLease {
            run_id: Some(ResourceId {
                urn: run_id.to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(ResourceId {
                urn: format!("urn:vam:attempt:local.vam.dev/acme/{session_id}"),
            }),
            lease_id: Some(ResourceId {
                urn: format!("urn:vam:lease:local.vam.dev/acme/{session_id}"),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: format!("idem-{session_id}"),
        }
    }

    fn catalog_tool(name: &str, side_effect: i32) -> ToolCatalogEntry {
        ToolCatalogEntry {
            tool_name: name.to_string(),
            command: "host".to_string(),
            entrypoint: name.to_string(),
            digest: String::new(),
            side_effect,
            skill_name: String::new(),
            approval_required: false,
        }
    }

    fn seed(session_id: &str, run_id: &str) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_session(session_id, None, "vm", "ws", "storage-default")
            .expect("session");
        store
            .record_tool_catalog(
                session_id,
                &[
                    catalog_tool("jira_transition_issue", SideEffectClass::ReadOnly as i32),
                    catalog_tool("jira_search_issues", SideEffectClass::ReadOnly as i32),
                    catalog_tool("post_comment", SideEffectClass::MutableExternal as i32),
                ],
            )
            .expect("catalog");
        store
            .persist_run_spec(
                run_id,
                "{}",
                Some(session_id),
                AGENT,
                None,
                "User::\"tester\"",
            )
            .expect("bind");
        store
    }

    fn pin() -> PlaybookRunPin {
        PlaybookRunPin {
            urn: "urn:vam:playbook:local.vam.dev/acme/guards".to_string(),
            version: "1.0.0".to_string(),
            digest: "d".repeat(64),
        }
    }

    fn task(id: &str, after: &[&str], outcome: Option<&str>) -> PlaybookStep {
        PlaybookStep {
            id: id.to_string(),
            instruction: format!("do {id}"),
            after: after.iter().map(|token| (*token).to_string()).collect(),
            outcomes: outcome
                .map(|label| {
                    vec![StepOutcome {
                        label: label.to_string(),
                        ..Default::default()
                    }]
                })
                .unwrap_or_default(),
            ..Default::default()
        }
    }

    fn init(store: &SqliteWorkflowStore, run_id: &str, guard: PlaybookGuard) {
        store
            .init_playbook_run(
                run_id,
                &pin(),
                &PlaybookSpec {
                    steps: vec![
                        task("query", &[], None),
                        task("approve", &["query"], Some("approved")),
                    ],
                    guards: vec![guard],
                    activation: None,
                    ..Default::default()
                },
            )
            .expect("init playbook");
    }

    fn complete(store: &SqliteWorkflowStore, run_id: &str, step_id: &str, outcome: &str) {
        store
            .complete_playbook_step(
                run_id,
                step_id,
                &PlaybookStepCompletion {
                    outcome: outcome.to_string(),
                    output_json: None,
                    agent_summary: None,
                },
            )
            .expect("complete step");
    }

    fn call(
        ctx: &SessionSecurityContext,
        store: &SqliteWorkflowStore,
        session_id: &str,
        run_id: &str,
        tool_name: &str,
    ) -> ToolIntentResponse {
        ctx.handle_tool_intent_request(
            store,
            ToolIntentRequest {
                lease: Some(lease(session_id, run_id)),
                tool_name: tool_name.to_string(),
                arguments: None,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        )
    }

    fn message(response: &ToolIntentResponse) -> String {
        response
            .error
            .as_ref()
            .map(|err| err.message.clone())
            .unwrap_or_default()
    }

    fn transition_guard() -> PlaybookGuard {
        PlaybookGuard {
            tools: vec!["jira_transition_issue".to_string()],
            side_effects: vec![SideEffectClass::MutableExternal as i32],
            allowed_when: vec!["query:done".to_string(), "approve:approved".to_string()],
            // The call has no issue_key. args_within constrains a supplied
            // argument; omitting it is not a miss, so this still allows once
            // allowed_when holds.
            args_within: vec![GuardArgsWithin {
                argument: "issue_key".to_string(),
                within: "steps.approve.output.selected[*].key".to_string(),
            }],
        }
    }

    #[test]
    fn a_passed_guard_condition_is_named_without_a_value() {
        let view = PlaybookPepView {
            steps: vec![],
            guards: vec![transition_guard()],
        };
        let tool = "jira_transition_issue";
        let se = SideEffectClass::MutableExternal as i32;
        // A supplied constrained argument names the args_within condition.
        let with_arg = serde_json::json!({"issue_key": "SECRET-1"});
        let rule = playbook_guard_constraint(&view, tool, se, &with_arg).unwrap();
        assert_eq!(
            rule,
            "args_within issue_key in steps.approve.output.selected[*].key"
        );
        assert!(!rule.contains("SECRET"));
        // Without the argument the allowed_when condition is what gated it.
        let rule = playbook_guard_constraint(&view, tool, se, &serde_json::json!({})).unwrap();
        assert_eq!(rule, "allowed_when query:done");
        // An unguarded tool has no constraint.
        assert!(playbook_guard_constraint(&view, "read_file", 0, &serde_json::json!({})).is_none());
    }

    #[test]
    fn named_tool_and_side_effect_wait_for_allowed_when_even_when_cedar_allows() {
        let session_id = "sess-guard-allow";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-allow";
        let store = seed(session_id, run_id);
        init(&store, run_id, transition_guard());
        let ctx = ctx(session_id, PERMIT_TOOLS);

        let search = call(&ctx, &store, session_id, run_id, "jira_search_issues");
        assert_eq!(
            search.disposition,
            Disposition::Authorized as i32,
            "unguarded tool: {}",
            message(&search)
        );

        let blocked = call(&ctx, &store, session_id, run_id, "jira_transition_issue");
        assert_eq!(blocked.disposition, Disposition::Denied as i32);
        assert!(blocked.capability_token.is_none());
        assert_eq!(message(&blocked), "blocked until step `query` is `done`");

        let class_blocked = call(&ctx, &store, session_id, run_id, "post_comment");
        assert_eq!(class_blocked.disposition, Disposition::Denied as i32);
        assert_eq!(
            message(&class_blocked),
            "blocked until step `query` is `done`"
        );

        complete(&store, run_id, "query", "done");
        let waiting_on_approve = call(&ctx, &store, session_id, run_id, "jira_transition_issue");
        assert_eq!(waiting_on_approve.disposition, Disposition::Denied as i32);
        assert_eq!(
            message(&waiting_on_approve),
            "blocked until step `approve` is `approved`"
        );

        complete(&store, run_id, "approve", "approved");
        let allowed = call(&ctx, &store, session_id, run_id, "jira_transition_issue");
        assert_eq!(
            allowed.disposition,
            Disposition::Authorized as i32,
            "{}",
            message(&allowed)
        );
        assert!(allowed.capability_token.is_some());
        let class_allowed = call(&ctx, &store, session_id, run_id, "post_comment");
        assert_eq!(
            class_allowed.disposition,
            Disposition::Authorized as i32,
            "{}",
            message(&class_allowed)
        );
    }

    #[test]
    fn cedar_deny_stands_when_the_guard_allows() {
        let session_id = "sess-guard-cedar-deny";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-cedar-deny";
        let store = seed(session_id, run_id);
        init(&store, run_id, transition_guard());
        complete(&store, run_id, "query", "done");
        complete(&store, run_id, "approve", "approved");

        let policy = r#"
forbid (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_transition_issue"
);
"#;
        let ctx = ctx(session_id, policy);
        let denied = call(&ctx, &store, session_id, run_id, "jira_transition_issue");
        assert_eq!(denied.disposition, Disposition::Denied as i32);
        assert!(denied.capability_token.is_none());
        let text = message(&denied);
        assert!(
            !text.contains("blocked until"),
            "guard must not replace a Cedar deny: {text}"
        );
        assert!(text.contains("denied") || text.contains("Deny"), "{text}");
    }

    #[test]
    fn cedar_policy_denies_from_context_playbook_step_state() {
        let policy = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_search_issues"
);
forbid (
    principal,
    action == Action::"ExecuteTool",
    resource
)
when {
    context has playbook &&
    context.playbook.contains({id: "approve", state: "pending", outcome: ""})
};
"#;
        CedarPolicyEngine::from_default_schema(policy, PolicyMode::Enforce, 1)
            .expect("context.playbook must compile against the default schema");

        let session_id = "sess-guard-context";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-context";
        let store = seed(session_id, run_id);
        init(
            &store,
            run_id,
            PlaybookGuard {
                tools: vec![],
                side_effects: vec![],
                allowed_when: vec![],
                args_within: vec![],
            },
        );
        let ctx = ctx(session_id, policy);
        let denied = call(&ctx, &store, session_id, run_id, "jira_search_issues");
        assert_eq!(denied.disposition, Disposition::Denied as i32);
        assert!(
            !message(&denied).contains("blocked until"),
            "step-state deny is Cedar, not a guard: {}",
            message(&denied)
        );

        complete(&store, run_id, "query", "done");
        let allowed = call(&ctx, &store, session_id, run_id, "jira_search_issues");
        assert_eq!(
            allowed.disposition,
            Disposition::Authorized as i32,
            "approve is no longer pending: {}",
            message(&allowed)
        );
    }

    #[test]
    fn session_without_a_playbook_run_is_unchanged() {
        let session_id = "sess-guard-none";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-none";
        let store = seed(session_id, run_id);
        let ctx = ctx(session_id, PERMIT_TOOLS);
        let allowed = call(&ctx, &store, session_id, run_id, "jira_transition_issue");
        assert_eq!(
            allowed.disposition,
            Disposition::Authorized as i32,
            "{}",
            message(&allowed)
        );
        assert!(allowed.capability_token.is_some());
    }

    #[test]
    fn guard_denies_before_hitl_suspend_and_does_not_grant_it() {
        let policy = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_transition_issue"
)
when {
    context has hitl_approved && context.hitl_approved == true
};
forbid (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_transition_issue"
)
unless {
    context has hitl_approved && context.hitl_approved == true
};
"#;
        let session_id = "sess-guard-hitl";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-hitl";
        let store = seed(session_id, run_id);
        store
            .issue_lease(&lease(session_id, run_id))
            .expect("attempt");
        init(&store, run_id, transition_guard());
        let ctx = ctx(session_id, policy);

        let blocked = call(&ctx, &store, session_id, run_id, "jira_transition_issue");
        assert_eq!(blocked.disposition, Disposition::Denied as i32);
        assert!(blocked.approval_request_id.is_none());
        assert_eq!(message(&blocked), "blocked until step `query` is `done`");

        complete(&store, run_id, "query", "done");
        complete(&store, run_id, "approve", "approved");
        let suspended = call(&ctx, &store, session_id, run_id, "jira_transition_issue");
        assert_eq!(
            suspended.disposition,
            Disposition::Suspended as i32,
            "a satisfied guard must not grant the Cedar HITL deny: {}",
            message(&suspended)
        );
    }

    #[test]
    fn decisions_a_guard_denial_is_recorded_with_the_guard_as_the_rule() {
        let policy = r#"
permit (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_transition_issue"
);
"#;
        let session_id = "sess-guard-record";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-record";
        let store = seed(session_id, run_id);
        store
            .issue_lease(&lease(session_id, run_id))
            .expect("attempt");
        init(&store, run_id, transition_guard());
        let ctx = ctx(session_id, policy);
        let blocked = call(&ctx, &store, session_id, run_id, "jira_transition_issue");
        assert_eq!(blocked.disposition, Disposition::Denied as i32);
        let rows = store.run_decisions(run_id).expect("rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].disposition, "denied");
        assert_eq!(rows[0].decided_by, "playbook_guard");
        assert_eq!(rows[0].deciding_scope, "playbook");
        assert_eq!(rows[0].deciding_rule, "allowed_when query:done");
    }

    const WITHIN: &str = "steps.approve.output.selected[*].key";

    fn scope_guard(args_within: Vec<GuardArgsWithin>) -> PlaybookGuard {
        PlaybookGuard {
            tools: vec!["jira_transition_issue".to_string()],
            allowed_when: vec!["approve:approved".to_string()],
            args_within,
            ..Default::default()
        }
    }

    fn issue_key_within() -> GuardArgsWithin {
        GuardArgsWithin {
            argument: "issue_key".to_string(),
            within: WITHIN.to_string(),
        }
    }

    fn within_denial() -> String {
        format!("blocked by guard: argument `issue_key` is not within `{WITHIN}`")
    }

    fn selection_gate() -> PlaybookStep {
        PlaybookStep {
            id: "approve".to_string(),
            kind: StepKind::Gate as i32,
            question: "Which issues should move?".to_string(),
            options: vec![
                StepOutcome {
                    label: "approved".to_string(),
                    ..Default::default()
                },
                StepOutcome {
                    label: "rejected".to_string(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }
    }

    fn init_steps(
        store: &SqliteWorkflowStore,
        run_id: &str,
        steps: Vec<PlaybookStep>,
        guard: PlaybookGuard,
    ) {
        store
            .init_playbook_run(
                run_id,
                &pin(),
                &PlaybookSpec {
                    steps,
                    guards: vec![guard],
                    activation: None,
                    ..Default::default()
                },
            )
            .expect("init playbook");
    }

    fn submit_selection(store: &SqliteWorkflowStore, run_id: &str, response: &str) {
        store
            .decide_approval(
                &ResourceId {
                    urn: format!("{run_id}/gate/approve"),
                },
                &ResourceId {
                    urn: format!("urn:vam:token:local.vam.dev/acme/{run_id}"),
                },
                "SUBMITTED",
                "User::\"local\"",
                crate::users::LOCAL_USER_ID,
                None,
                None,
                None,
                Some(response),
            )
            .expect("submit gate");
    }

    fn call_issue(
        ctx: &SessionSecurityContext,
        store: &SqliteWorkflowStore,
        session_id: &str,
        run_id: &str,
        issue_key: Option<&str>,
    ) -> ToolIntentResponse {
        let arguments = issue_key.map(|key| {
            let mut fields = prost_types::Struct::default();
            fields.fields.insert(
                "issue_key".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(key.to_string())),
                },
            );
            fields
        });
        ctx.handle_tool_intent_request(
            store,
            ToolIntentRequest {
                lease: Some(lease(session_id, run_id)),
                tool_name: "jira_transition_issue".to_string(),
                arguments,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        )
    }

    fn selected(keys: &[&str]) -> String {
        let items: Vec<String> = keys
            .iter()
            .map(|key| format!(r#"{{"key":"{key}"}}"#))
            .collect();
        format!(
            r#"{{"option":"approved","selected":[{}]}}"#,
            items.join(",")
        )
    }

    #[test]
    fn selection_gate_output_limits_a_later_tool_argument() {
        let session_id = "sess-guard-within";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-within";
        let store = seed(session_id, run_id);
        init_steps(
            &store,
            run_id,
            vec![selection_gate()],
            scope_guard(vec![issue_key_within()]),
        );
        let ctx = ctx(session_id, PERMIT_TOOLS);

        let early = call_issue(&ctx, &store, session_id, run_id, Some("PROJ-1"));
        assert_eq!(early.disposition, Disposition::Denied as i32);
        assert_eq!(
            message(&early),
            "blocked until step `approve` is `approved`"
        );

        submit_selection(&store, run_id, &selected(&["PROJ-1", "PROJ-2"]));

        let inside = call_issue(&ctx, &store, session_id, run_id, Some("PROJ-1"));
        assert_eq!(
            inside.disposition,
            Disposition::Authorized as i32,
            "{}",
            message(&inside)
        );
        assert!(inside.capability_token.is_some());

        let second = call_issue(&ctx, &store, session_id, run_id, Some("PROJ-2"));
        assert_eq!(
            second.disposition,
            Disposition::Authorized as i32,
            "[*] matches an element: {}",
            message(&second)
        );

        let outside = call_issue(&ctx, &store, session_id, run_id, Some("OTHER"));
        assert_eq!(outside.disposition, Disposition::Denied as i32);
        assert!(outside.capability_token.is_none());
        assert_eq!(message(&outside), within_denial());
    }

    #[test]
    fn cedar_deny_wins_when_the_argument_is_inside_the_set() {
        let session_id = "sess-guard-within-cedar";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-within-cedar";
        let store = seed(session_id, run_id);
        init_steps(
            &store,
            run_id,
            vec![selection_gate()],
            scope_guard(vec![issue_key_within()]),
        );
        submit_selection(&store, run_id, &selected(&["PROJ-1"]));

        let policy = r#"
forbid (
    principal,
    action == Action::"ExecuteTool",
    resource == Tool::"jira_transition_issue"
);
"#;
        let ctx = ctx(session_id, policy);
        let denied = call_issue(&ctx, &store, session_id, run_id, Some("PROJ-1"));
        assert_eq!(denied.disposition, Disposition::Denied as i32);
        assert!(denied.capability_token.is_none());
        let text = message(&denied);
        assert!(
            !text.contains("not within"),
            "an in-set argument must not turn a Cedar deny into a guard miss: {text}"
        );
        assert!(text.contains("denied") || text.contains("Deny"), "{text}");
    }

    #[test]
    fn missing_or_non_array_output_denies_the_argument() {
        let session_id = "sess-guard-within-miss";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-within-miss";
        let store = seed(session_id, run_id);
        init_steps(
            &store,
            run_id,
            vec![task("approve", &[], Some("approved"))],
            scope_guard(vec![issue_key_within()]),
        );
        let permit = ctx(session_id, PERMIT_TOOLS);
        complete(&store, run_id, "approve", "approved");

        let missing = call_issue(&permit, &store, session_id, run_id, Some("PROJ-1"));
        assert_eq!(missing.disposition, Disposition::Denied as i32);
        assert_eq!(message(&missing), within_denial());

        let session_id = "sess-guard-within-shape";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-within-shape";
        let store = seed(session_id, run_id);
        init_steps(
            &store,
            run_id,
            vec![task("approve", &[], Some("approved"))],
            scope_guard(vec![issue_key_within()]),
        );
        store
            .complete_playbook_step(
                run_id,
                "approve",
                &PlaybookStepCompletion {
                    outcome: "approved".to_string(),
                    output_json: Some(r#"{"selected":{"key":"PROJ-1"}}"#.to_string()),
                    agent_summary: None,
                },
            )
            .expect("complete");
        let shaped = ctx(session_id, PERMIT_TOOLS);
        let non_array = call_issue(&shaped, &store, session_id, run_id, Some("PROJ-1"));
        assert_eq!(non_array.disposition, Disposition::Denied as i32);
        assert_eq!(message(&non_array), within_denial());
    }

    #[test]
    fn empty_args_within_stays_a_condition_only_guard() {
        let session_id = "sess-guard-within-empty";
        let run_id = "urn:vam:run:local.vam.dev/acme/guard-within-empty";
        let store = seed(session_id, run_id);
        init_steps(
            &store,
            run_id,
            vec![task("approve", &[], Some("approved"))],
            scope_guard(vec![]),
        );
        complete(&store, run_id, "approve", "approved");
        let ctx = ctx(session_id, PERMIT_TOOLS);
        let allowed = call_issue(&ctx, &store, session_id, run_id, Some("OTHER"));
        assert_eq!(
            allowed.disposition,
            Disposition::Authorized as i32,
            "{}",
            message(&allowed)
        );
        assert!(allowed.capability_token.is_some());
    }
}

#[cfg(test)]
mod permissions_mode_tests {
    use super::*;
    use crate::session::{builtin_tool_catalog, ToolCatalogEntry};
    use crate::session_targets::{
        assemble_session_targets, PlannedMcpTool, LEGACY_LOCKED_REASON, RELEASED_LOCKED_REASON,
    };
    use crate::users::LOCAL_USER_ID;
    use vam_security::{CedarPolicyEngine, PolicyMode};
    use vam_spec::v1::{
        agent_spec::InstructionSource, AgentPermissions, AgentSpec, ExecutionLease,
        PermissionDecision, PermissionRule, ResourceId, ResourceRef, RunSpec, ToolDefault,
        ToolIntentRequest, ToolPermissions, Version,
    };

    const AGENT: &str = "urn:vam:agent:local.vam.dev/acme/perm-agent";
    const SESSION: &str = "sess-perm";
    const RUN: &str = "urn:vam:run:local.vam.dev/acme/perm-run";
    const MCP_TOOL: &str = "atlassian__create_issue";

    fn rid(urn: &str) -> ResourceId {
        ResourceId {
            urn: urn.to_string(),
        }
    }

    fn release_ver() -> Version {
        Version {
            major: 1,
            minor: 1,
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

    fn permissions(rules: &[(&str, PermissionDecision)]) -> AgentPermissions {
        AgentPermissions {
            tools: Some(ToolPermissions {
                default: ToolDefault::BySideEffect as i32,
                rules: rules
                    .iter()
                    .map(|(target, decision)| PermissionRule {
                        target: target.to_string(),
                        decision: *decision as i32,
                    })
                    .collect(),
            }),
            ..Default::default()
        }
    }

    fn agent(permissions: Option<AgentPermissions>) -> AgentSpec {
        // Draft publish (AX-5.6a) fills permission defaults into an agent with
        // neither `permissions` nor a legacy policy, so the legacy fixture
        // carries a `capability_policy`, as a real legacy draft does.
        let capability_policy = permissions
            .is_none()
            .then(|| vam_spec::v1::CapabilityPolicyRef {
                security_profile_name: "restricted-egress".to_string(),
                ..Default::default()
            });
        AgentSpec {
            resource_id: Some(rid(AGENT)),
            display_name: "Permissions agent".to_string(),
            description: "test".to_string(),
            role: "assistant".to_string(),
            instruction_source: Some(InstructionSource::SystemInstructions("x".to_string())),
            permissions,
            capability_policy,
            allow_any_playbook: true,
            ..Default::default()
        }
    }

    fn tool(
        name: &str,
        command: &str,
        skill: &str,
        side_effect: i32,
        ask: bool,
    ) -> ToolCatalogEntry {
        ToolCatalogEntry {
            tool_name: name.to_string(),
            command: command.to_string(),
            entrypoint: name.to_string(),
            digest: String::new(),
            side_effect,
            skill_name: skill.to_string(),
            approval_required: ask,
        }
    }

    fn catalog() -> Vec<ToolCatalogEntry> {
        let mut catalog = builtin_tool_catalog();
        catalog.push(tool("jira_search", "bun", "jira", 1, false));
        catalog.push(tool("jira_create_issue", "bun", "jira", 2, true));
        catalog.push(tool(MCP_TOOL, "host", "", 3, false));
        catalog.push(crate::notifications_tool::catalog_entry());
        catalog
    }

    fn mcp_plan() -> Vec<PlannedMcpTool> {
        vec![PlannedMcpTool {
            connection_id: "atlassian".to_string(),
            server_url: "https://mcp.atlassian.com/".to_string(),
            vendor_tool: "create_issue".to_string(),
            guest_name: MCP_TOOL.to_string(),
            side_effect: 3,
            ask: false,
        }]
    }

    fn lease() -> ExecutionLease {
        ExecutionLease {
            run_id: Some(rid(RUN)),
            step_id: "step-1".to_string(),
            attempt_id: Some(rid("urn:vam:attempt:local.vam.dev/acme/perm")),
            lease_id: Some(rid("urn:vam:lease:local.vam.dev/acme/perm")),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4_102_444_800,
                nanos: 0,
            }),
            idempotency_key: "idem-perm".to_string(),
        }
    }

    /// A session whose run pins `pinned` (draft or release) of an agent with `permissions`.
    fn setup(permissions: Option<AgentPermissions>, pinned: Version) -> SqliteWorkflowStore {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store
            .create_session(SESSION, None, "vm", "ws", "storage-default")
            .expect("session");
        store
            .publish_agent_draft_with_owner(&agent(permissions), LOCAL_USER_ID)
            .expect("draft");
        if pinned.prerelease.is_empty() {
            store.release_agent(AGENT, &pinned).expect("release");
        }
        let spec = RunSpec {
            run_id: Some(rid(RUN)),
            agent_ref: Some(ResourceRef {
                resource_id: Some(rid(AGENT)),
                version: Some(pinned),
                ..Default::default()
            }),
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
        store.issue_lease(&lease()).expect("lease");
        store
            .record_tool_catalog(SESSION, &catalog())
            .expect("catalog");
        assemble_session_targets(&store, SESSION, &catalog(), &mcp_plan()).expect("assemble");
        store
    }

    fn context(store: &SqliteWorkflowStore) -> SessionSecurityContext {
        let resolved = resolve_session_policy(store, SESSION, Some(AGENT)).expect("resolve");
        CedarPolicyEngine::from_default_schema(&resolved.cedar, PolicyMode::Enforce, 1)
            .expect("composed policy compiles");
        SessionSecurityContext::new(
            SESSION.to_string(),
            AGENT.to_string(),
            &resolved.cedar,
            PolicyMode::Enforce,
            1,
            ed25519_dalek::SigningKey::from_bytes(&[7u8; 32]),
            None,
            Arc::new(SessionBudget::new(None)),
        )
        .expect("context")
        .with_policy_mode(resolved.mode)
    }

    fn call(
        ctx: &SessionSecurityContext,
        store: &SqliteWorkflowStore,
        name: &str,
    ) -> ToolIntentResponse {
        ctx.handle_tool_intent_request(
            store,
            ToolIntentRequest {
                lease: Some(lease()),
                tool_name: name.to_string(),
                arguments: None,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        )
    }

    fn read_file_call(
        ctx: &SessionSecurityContext,
        store: &SqliteWorkflowStore,
    ) -> ToolIntentResponse {
        let mut args = prost_types::Struct::default();
        args.fields.insert(
            "path".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "/mnt/drive9/workspace/notes.txt".to_string(),
                )),
            },
        );
        ctx.handle_tool_intent_request(
            store,
            ToolIntentRequest {
                lease: Some(lease()),
                tool_name: "read_file".to_string(),
                arguments: Some(args),
                arguments_digest: None,
                script_digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: vam_spec::validation::builtin_tool_digest("read_file"),
                }),
                call_id: String::new(),
            },
        )
    }

    fn pending_stamp(store: &SqliteWorkflowStore) -> (String, i64, String) {
        let connection = store.connection.lock().unwrap();
        connection
            .query_row(
                "SELECT target, always_offered, locked_reason FROM hitl_pending WHERE session_id = ?1",
                [SESSION],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("pending row")
    }

    #[test]
    fn default_permissions_run_local_skill_tools_and_suspend_the_mcp_tool() {
        let store = setup(Some(permissions(&[])), draft_ver());
        let resolved = resolve_session_policy(&store, SESSION, Some(AGENT)).unwrap();
        assert_eq!(resolved.mode, SessionPolicyMode::Permissions);
        assert_eq!(
            store.run_policy_mode(SESSION).unwrap().as_deref(),
            Some("permissions")
        );
        let ctx = context(&store);

        assert_eq!(
            call(&ctx, &store, "jira_search").disposition,
            Disposition::Authorized as i32
        );
        assert_eq!(
            read_file_call(&ctx, &store).disposition,
            Disposition::Authorized as i32,
            "builtins and the path limb keep working"
        );
        let suspended = call(&ctx, &store, MCP_TOOL);
        assert_eq!(suspended.disposition, Disposition::Suspended as i32);
        let (target, always, reason) = pending_stamp(&store);
        assert_eq!(target, "mcp:mcp.atlassian.com/create_issue");
        assert_eq!(always, 1, "the owner's draft run may save Always: {reason}");
        assert_eq!(reason, "");
        assert!(ctx
            .advertised_tool_names(&store)
            .contains(&MCP_TOOL.to_string()));
    }

    #[test]
    fn approval_required_skill_tool_under_allow_all_suspends_and_names_the_skill() {
        let rules = [("tools:*", PermissionDecision::Allow)];
        let store = setup(Some(permissions(&rules)), draft_ver());
        let ctx = context(&store);
        assert_eq!(
            call(&ctx, &store, "jira_search").disposition,
            Disposition::Authorized as i32
        );
        let suspended = call(&ctx, &store, "jira_create_issue");
        assert_eq!(suspended.disposition, Disposition::Suspended as i32);
        let (target, always, reason) = pending_stamp(&store);
        assert_eq!(target, "skill:jira/jira_create_issue");
        assert_eq!(always, 0);
        assert!(reason.contains("skill jira"), "{reason}");
        assert!(reason.contains("jira_create_issue"), "{reason}");
    }

    #[test]
    fn released_run_stamps_always_off_with_the_released_reason() {
        let store = setup(Some(permissions(&[])), release_ver());
        let ctx = context(&store);
        assert_eq!(
            call(&ctx, &store, MCP_TOOL).disposition,
            Disposition::Suspended as i32
        );
        let (target, always, reason) = pending_stamp(&store);
        assert_eq!(target, "mcp:mcp.atlassian.com/create_issue");
        assert_eq!(always, 0);
        assert_eq!(reason, RELEASED_LOCKED_REASON);
    }

    #[test]
    fn deny_rule_hides_and_denies_the_tool() {
        let rules = [("skill:jira/jira_search", PermissionDecision::Deny)];
        let store = setup(Some(permissions(&rules)), draft_ver());
        let ctx = context(&store);
        assert!(!ctx
            .advertised_tool_names(&store)
            .contains(&"jira_search".to_string()));
        assert_eq!(
            call(&ctx, &store, "jira_search").disposition,
            Disposition::Denied as i32
        );
    }

    fn rows(store: &SqliteWorkflowStore) -> Vec<crate::run_facts::RunDecision> {
        store.run_decisions(RUN).expect("decisions")
    }

    fn call_with_args(
        ctx: &SessionSecurityContext,
        store: &SqliteWorkflowStore,
        name: &str,
        args: serde_json::Value,
    ) -> ToolIntentResponse {
        let arguments = vam_spec::encoding::json_to_proto::<prost_types::Struct>(
            "google.protobuf.Struct",
            &args.to_string(),
        )
        .ok();
        ctx.handle_tool_intent_request(
            store,
            ToolIntentRequest {
                lease: Some(lease()),
                tool_name: name.to_string(),
                arguments,
                arguments_digest: None,
                script_digest: None,
                call_id: String::new(),
            },
        )
    }

    /// The skill-card map is keyed by session id and these tests share `SESSION`.
    static CARD_MAP: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn seed_ask_card(tool: &str) {
        let schema = serde_json::json!({"type": "object", "properties": {
            "title": {"type": "string"}, "project": {"type": "string"}}});
        let ir = serde_json::json!({
            "jig": "jira.create", "state": {"title": "", "decision": ""},
            "nodes": {"ok": {"type": "Button", "props": {"action": {
                "event": "submit", "set": {"decision": "approve"}, "with": ["title"]}}}}
        });
        let mut cards = std::collections::HashMap::new();
        cards.insert(
            tool.to_string(),
            crate::skill_cards::ToolCards {
                ask: Some(crate::skill_cards::ToolCard {
                    skill: "urn:vam:skill:local.vam.dev/e2e/jira".into(),
                    jig: "jira.create".into(),
                    ir,
                    schema,
                }),
                ..Default::default()
            },
        );
        crate::skill_cards::seed_for_test(SESSION, cards);
    }

    fn pending_card(store: &SqliteWorkflowStore) -> (Option<String>, String) {
        store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT card_json, amendable_schema_json FROM hitl_pending WHERE session_id = ?1",
                [SESSION],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("pending row")
    }

    #[test]
    fn a_ui_ask_tool_suspends_with_its_card_and_the_card_amendable_schema() {
        let _map = CARD_MAP.lock().unwrap_or_else(|e| e.into_inner());
        let rules = [("tools:*", PermissionDecision::Allow)];
        let store = setup(Some(permissions(&rules)), draft_ver());
        seed_ask_card("jira_create_issue");
        let ctx = context(&store);
        let r = call_with_args(
            &ctx,
            &store,
            "jira_create_issue",
            serde_json::json!({"title": "T1", "project": "P", "decision": "approve"}),
        );
        assert_eq!(r.disposition, Disposition::Suspended as i32);
        let (card, schema) = pending_card(&store);
        let card: serde_json::Value = serde_json::from_str(&card.expect("card stored")).unwrap();
        assert_eq!(card["jig"], "jira.create");
        assert_eq!(card["ir"]["state"]["title"], "T1");
        assert_eq!(
            card["ir"]["state"]["decision"], "",
            "decision cannot be preset"
        );
        assert_eq!(card["data"]["project"], "P");
        let schema: serde_json::Value = serde_json::from_str(&schema).unwrap();
        assert_eq!(
            schema["properties"],
            serde_json::json!({"title": {"type": "string"}})
        );
        assert_eq!(schema["additionalProperties"], false);
        crate::skill_cards::forget_session(SESSION);
    }

    #[test]
    fn a_tool_without_ui_ask_or_absent_from_the_map_suspends_as_before() {
        let _map = CARD_MAP.lock().unwrap_or_else(|e| e.into_inner());
        let rules = [("tools:*", PermissionDecision::Allow)];
        // Another tool has the card; this one is absent from the map.
        let store = setup(Some(permissions(&rules)), draft_ver());
        seed_ask_card("some_other_tool");
        let ctx = context(&store);
        let r = call_with_args(
            &ctx,
            &store,
            "jira_create_issue",
            serde_json::json!({"title": "T1"}),
        );
        assert_eq!(r.disposition, Disposition::Suspended as i32);
        let (card, schema) = pending_card(&store);
        assert_eq!(card, None);
        let schema: serde_json::Value = serde_json::from_str(&schema).unwrap();
        assert_eq!(
            schema,
            crate::hitl::default_amendable_schema(&serde_json::json!({"title": "T1"}))
        );
        // A tool with a show card only (no ask) behaves the same.
        crate::skill_cards::seed_for_test(
            SESSION,
            [(
                "jira_create_issue".to_string(),
                crate::skill_cards::ToolCards {
                    declares_show: true,
                    ..Default::default()
                },
            )]
            .into(),
        );
        assert!(crate::skill_cards::ask_approval_card(
            SESSION,
            "jira_create_issue",
            &serde_json::json!({"title": "T1"})
        )
        .is_none());
        crate::skill_cards::forget_session(SESSION);
    }

    #[test]
    fn decisions_record_allowed_and_asked_with_the_rows_scope_and_rule() {
        let store = setup(Some(permissions(&[])), draft_ver());
        let ctx = context(&store);
        let allowed = call(&ctx, &store, "jira_search");
        let asked = call(&ctx, &store, MCP_TOOL);
        assert_eq!(allowed.disposition, Disposition::Authorized as i32);
        assert_eq!(asked.disposition, Disposition::Suspended as i32);
        let rows = rows(&store);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].disposition, "allowed");
        assert_eq!(rows[1].disposition, "asked");
        assert_eq!(rows[1].decided_by, "cedar_rule");
        assert_eq!(rows[1].target, "mcp:mcp.atlassian.com/create_issue");
        assert_eq!(
            rows[1].approval_request_id.as_deref(),
            asked.approval_request_id.as_ref().map(|r| r.urn.as_str())
        );
        let (scope, rule): (String, String) = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT deciding_scope, deciding_rule FROM session_tool_targets
                 WHERE session_id = ?1 AND tool_name = ?2",
                [SESSION, MCP_TOOL],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (
                rows[1].deciding_scope.clone(),
                rows[1].deciding_rule.clone()
            ),
            (scope, rule)
        );
        assert!(!rows[1].via_grant);
    }

    #[test]
    fn decisions_an_always_rule_mid_run_keeps_the_earlier_rows_rule() {
        let store = setup(Some(permissions(&[])), draft_ver());
        let ctx = context(&store);
        assert_eq!(
            call(&ctx, &store, MCP_TOOL).disposition,
            Disposition::Suspended as i32
        );
        {
            // What "Always allow" does to the session's row.
            store
                .connection
                .lock()
                .unwrap()
                .execute(
                    "UPDATE session_tool_targets
                     SET agent_rule = 'allow', deciding_scope = 'agent', deciding_rule = 'always-rule'
                     WHERE session_id = ?1 AND tool_name = ?2",
                    [SESSION, MCP_TOOL],
                )
                .unwrap();
        }
        let second = call(&ctx, &store, MCP_TOOL);
        let rows = rows(&store);
        assert_eq!(rows.len(), 2);
        assert_ne!(
            rows[0].deciding_rule, "always-rule",
            "earlier row is not rewritten"
        );
        assert_eq!(rows[1].deciding_rule, "always-rule");
        assert_eq!(rows[1].deciding_scope, "agent");
        assert_eq!(
            rows[1].disposition == "allowed",
            second.disposition == Disposition::Authorized as i32
        );
    }

    #[test]
    fn decisions_a_call_passing_on_an_approved_ask_is_marked_via_grant() {
        let store = setup(Some(permissions(&[])), draft_ver());
        let ctx = context(&store);
        call(&ctx, &store, MCP_TOOL);
        let digest = rows(&store)[0].args_digest.clone();
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "INSERT INTO hitl_grants (session_id, tool_name, args_digest) VALUES (?1, ?2, ?3)",
                [SESSION, MCP_TOOL, digest.as_str()],
            )
            .unwrap();
        let again = call(&ctx, &store, MCP_TOOL);
        assert_eq!(again.disposition, Disposition::Authorized as i32);
        let rows = rows(&store);
        assert_eq!(rows[1].disposition, "allowed");
        assert!(rows[1].via_grant);
        assert!(!rows[0].via_grant);
    }

    #[test]
    fn decisions_a_missing_permission_row_is_recorded_as_a_denial() {
        let store = setup(
            Some(permissions(&[("tools:*", PermissionDecision::Allow)])),
            draft_ver(),
        );
        let ctx = context(&store);
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "DELETE FROM session_tool_targets WHERE session_id = ?1 AND tool_name = 'jira_search'",
                [SESSION],
            )
            .unwrap();
        call(&ctx, &store, "jira_search");
        let rows = rows(&store);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].disposition, "denied");
        assert_eq!(rows[0].decided_by, "missing_permissions_row");
    }

    #[test]
    fn decisions_an_unknown_tool_is_recorded_as_a_denial() {
        let store = setup(Some(permissions(&[])), draft_ver());
        let ctx = context(&store);
        let denied = call(&ctx, &store, "no_such_tool");
        assert_eq!(denied.disposition, Disposition::Denied as i32);
        let rows = rows(&store);
        assert_eq!(rows[0].decided_by, "missing_catalog_entry");
    }

    #[test]
    fn decisions_a_failed_write_changes_nothing_for_the_caller() {
        let with = setup(Some(permissions(&[])), draft_ver());
        let without = setup(Some(permissions(&[])), draft_ver());
        without
            .record_run_request(RUN, "local", "api")
            .expect("request row");
        without
            .connection
            .lock()
            .unwrap()
            .execute_batch("DROP TABLE run_decisions")
            .unwrap();
        let (a, b) = (context(&with), context(&without));
        for tool in ["jira_search", MCP_TOOL, "no_such_tool"] {
            let x = call(&a, &with, tool);
            let y = call(&b, &without, tool);
            assert_eq!(x.disposition, y.disposition, "{tool}");
            assert_eq!(x.error.is_some(), y.error.is_some(), "{tool}");
            assert_eq!(
                x.capability_token.is_some(),
                y.capability_token.is_some(),
                "{tool}"
            );
        }
        assert_eq!(without.run_facts(RUN).unwrap().decision_write_failures, 3);
    }

    #[test]
    fn a_tool_with_no_target_row_is_denied_in_permissions_mode() {
        let store = setup(
            Some(permissions(&[("tools:*", PermissionDecision::Allow)])),
            draft_ver(),
        );
        let ctx = context(&store);
        assert_eq!(
            call(&ctx, &store, "jira_search").disposition,
            Disposition::Authorized as i32
        );
        {
            let connection = store.connection.lock().unwrap();
            connection
                .execute(
                    "DELETE FROM session_tool_targets WHERE session_id = ?1 AND tool_name = 'jira_search'",
                    [SESSION],
                )
                .unwrap();
        }
        let denied = call(&ctx, &store, "jira_search");
        assert_eq!(denied.disposition, Disposition::Denied as i32);
        assert!(denied
            .error
            .unwrap()
            .message
            .contains("no permission target"));
        assert!(!ctx
            .advertised_tool_names(&store)
            .contains(&"jira_search".to_string()));
    }

    #[test]
    fn the_stamp_survives_removing_the_connection_and_the_approval_row() {
        let store = setup(Some(permissions(&[])), draft_ver());
        let ctx = context(&store);
        assert_eq!(
            call(&ctx, &store, MCP_TOOL).disposition,
            Disposition::Suspended as i32
        );
        // The connection is gone and the session rows are rewritten for a plan
        // with no MCP tools; the stamped target does not move.
        assemble_session_targets(&store, SESSION, &catalog(), &[]).expect("reassemble");
        let (target, ..) = pending_stamp(&store);
        assert_eq!(target, "mcp:mcp.atlassian.com/create_issue");
    }

    fn with_default(
        default: ToolDefault,
        rules: &[(&str, PermissionDecision)],
    ) -> AgentPermissions {
        let mut p = permissions(rules);
        p.tools.as_mut().unwrap().default = default as i32;
        p
    }

    fn deny_default(rules: &[(&str, PermissionDecision)]) -> AgentPermissions {
        with_default(ToolDefault::Deny, rules)
    }

    #[test]
    fn notifications_list_is_offered_only_by_its_allow_rule_whatever_the_default() {
        let name = "notifications_list".to_string();
        for default in [
            ToolDefault::Allow,
            ToolDefault::BySideEffect,
            ToolDefault::Ask,
            ToolDefault::Deny,
        ] {
            // No rule: not offered, and the call is denied.
            let store = setup(Some(with_default(default, &[])), draft_ver());
            let ctx = context(&store);
            assert!(
                !ctx.advertised_tool_names(&store).contains(&name),
                "{default:?} offered it without a rule"
            );
            assert_eq!(
                call(&ctx, &store, "notifications_list").disposition,
                Disposition::Denied as i32,
                "{default:?}"
            );
            // A rule that is not an allow does not opt in either.
            for decision in [PermissionDecision::Ask, PermissionDecision::Deny] {
                let store = setup(
                    Some(with_default(
                        default,
                        &[("builtin:notifications_list", decision)],
                    )),
                    draft_ver(),
                );
                let ctx = context(&store);
                assert!(!ctx.advertised_tool_names(&store).contains(&name));
            }
            // The allow rule opts in.
            let store = setup(
                Some(with_default(
                    default,
                    &[("builtin:notifications_list", PermissionDecision::Allow)],
                )),
                draft_ver(),
            );
            let ctx = context(&store);
            assert!(
                ctx.advertised_tool_names(&store).contains(&name),
                "{default:?} did not offer it with the rule"
            );
            assert!(crate::notifications_tool::session_opted_in(&store, SESSION));
        }
        let store = setup(Some(with_default(ToolDefault::Allow, &[])), draft_ver());
        assert!(!crate::notifications_tool::session_opted_in(
            &store, SESSION
        ));
    }

    #[test]
    fn a_playbook_run_without_the_rule_is_not_offered_notifications_list() {
        let store = setup(Some(deny_default(&[])), draft_ver());
        store
            .init_playbook_run(
                RUN,
                &crate::workflow::PlaybookRunPin {
                    urn: "urn:vam:playbook:local.vam.dev/acme/demo".to_string(),
                    version: "1.0.0".to_string(),
                    digest: "cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc"
                        .to_string(),
                },
                &vam_spec::v1::PlaybookSpec {
                    steps: vec![vam_spec::v1::PlaybookStep {
                        id: "first".to_string(),
                        instruction: "do first".to_string(),
                        ..Default::default()
                    }],
                    ..Default::default()
                },
            )
            .expect("init playbook");
        let ctx = context(&store);
        assert!(!ctx
            .advertised_tool_names(&store)
            .contains(&"notifications_list".to_string()));
        assert_eq!(
            call(&ctx, &store, "notifications_list").disposition,
            Disposition::Denied as i32
        );
    }

    #[test]
    fn legacy_agent_keeps_legacy_mode_and_passes_no_entities() {
        let store = setup(None, release_ver());
        let resolved = resolve_session_policy(&store, SESSION, Some(AGENT)).unwrap();
        assert_eq!(resolved.mode, SessionPolicyMode::Legacy);
        assert_eq!(
            store.run_policy_mode(SESSION).unwrap().as_deref(),
            Some("legacy")
        );
        let row = crate::session_targets::target_for(&store, SESSION, "jira_search")
            .unwrap()
            .expect("legacy rows are still stored");
        assert!(!row.always_offered);
        assert_eq!(row.locked_reason, LEGACY_LOCKED_REASON);
        // Legacy contexts never look at the rows: a missing row changes nothing.
        let ctx = context(&store);
        assert!(!ctx.permissions_mode);
        assert!(ctx.entities_for(&store, "no_such_tool").unwrap().is_empty());
    }
}
