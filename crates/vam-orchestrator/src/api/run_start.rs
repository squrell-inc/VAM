//! The one start path for a run (locked decision 8).
//!
//! `start_run` is what `create_run` did after auth, PDP and the idempotency header: resolve
//! the place, ask whether the workspace is busy, resolve the agent, model and playbook,
//! persist the run, boot the session. The HTTP handler and the queue starter both call it,
//! each with a [`RunCaller`] saying who the run is for and where it came from.

use serde::{Deserialize, Serialize};
use vam_spec::v1::InboxReceipt;

pub use super::inbox::BusyKind;
use super::*;

/// Where a run came from. A person's surface is the client's word; a trigger carries the
/// grant that fired it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RunSource {
    Person {
        surface: String,
    },
    Trigger {
        broker_id: String,
        grant_id: String,
        trigger_ref: String,
        fire_at: i64,
        fire_kind: String,
        surface: String,
    },
}

impl RunSource {
    pub fn is_trigger(&self) -> bool {
        matches!(self, RunSource::Trigger { .. })
    }

    pub fn surface(&self) -> &str {
        match self {
            RunSource::Person { surface } | RunSource::Trigger { surface, .. } => surface,
        }
    }

    /// What `runs.source` holds: `broker:<id>` for a trigger, NULL for a person.
    pub fn record(&self) -> Option<String> {
        match self {
            RunSource::Person { .. } => None,
            RunSource::Trigger { broker_id, .. } => Some(format!("broker:{broker_id}")),
        }
    }
}

/// Who a run starts for. Built from the authenticated request (HTTP) or from the stored
/// caller of a queued message (the starter); never from a handler.
#[derive(Debug, Clone)]
pub struct RunCaller {
    pub owner_user_id: String,
    pub principal: String,
    pub role: crate::users::UserRole,
    pub scopes: Vec<String>,
    /// The grant credential of a triggered run; None for a person.
    pub credential_id: Option<String>,
    /// Holds `users:admin` (computed once from the scopes).
    pub users_admin: bool,
    pub source: RunSource,
    /// A queued fire still waiting after this Unix time ends EXPIRED instead of starting.
    pub not_after: Option<i64>,
    /// The API key behind the request, kept so a queued start can re-check it.
    pub key_id: String,
    pub name: String,
    pub session_token: bool,
    /// The key record's own owner, which a queued message stores so its start reads the
    /// role exactly as the request did.
    pub key_owner_user_id: String,
}

impl RunCaller {
    pub fn from_auth(auth: &AuthContext, surface: &str) -> Self {
        let record = &auth.key.record;
        Self {
            owner_user_id: auth.owner_user_id().to_string(),
            principal: record.principal.clone(),
            role: auth.key.role,
            scopes: record.scopes.clone(),
            credential_id: None,
            users_admin: auth.has_users_admin(),
            source: RunSource::Person {
                surface: surface.to_string(),
            },
            not_after: None,
            key_id: record.key_id.clone(),
            name: record.name.clone(),
            session_token: matches!(auth.key.source, crate::keys::AuthSource::Session),
            key_owner_user_id: record.owner_user_id.clone(),
        }
    }
}

/// What the caller supplies that the request body does not carry.
pub struct StartOpts {
    /// A busy workspace ends the start as `BusyKind::Skipped` (a grant with `on_busy: SKIP`).
    pub skip_if_busy: bool,
    pub idempotency_key: String,
    pub traceparent: Option<vam_telemetry::Traceparent>,
}

/// What `start_run` did.
pub enum StartOutcome {
    /// A new run, booting.
    Started {
        run_id: String,
        workspace_id: String,
        thread_label: String,
        thread_id: String,
        session_id: String,
    },
    /// The idempotency key was seen: the run it started.
    Existing { run_id: String },
    /// The message went to the live run as a steer.
    Steered {
        run_id: String,
        receipt: InboxReceipt,
    },
    /// The message waits behind the live run as a queued run.
    Queued {
        receipt: InboxReceipt,
        workspace_id: String,
    },
}

/// Why a start did not happen.
pub enum StartError {
    /// The workspace is taken and the message cannot wait or steer: today's retryable
    /// refusal (or, for a trigger, the reason it was skipped).
    Busy {
        kind: BusyKind,
        refusal: OrchestratorError,
    },
    Api(ApiError),
}

impl StartError {
    /// The error today's handler answers.
    pub fn into_api(self) -> ApiError {
        match self {
            StartError::Busy { refusal, .. } => refusal.into(),
            StartError::Api(err) => err,
        }
    }
}

impl From<ApiError> for StartError {
    fn from(err: ApiError) -> Self {
        StartError::Api(err)
    }
}

impl From<OrchestratorError> for StartError {
    fn from(err: OrchestratorError) -> Self {
        StartError::Api(err.into())
    }
}

impl StartOutcome {
    /// Today's HTTP answer for each outcome, byte for byte.
    pub fn to_http(self, state: &AppState) -> Result<Response, ApiError> {
        match self {
            StartOutcome::Existing { run_id } => {
                let resp = CreateRunResponse {
                    status: Some(build_run_status(state, &run_id)?),
                    inbox: None,
                };
                let (code, json) = json_response_accepted("vam.v1.CreateRunResponse", &resp)?;
                Ok(json_http_response(code, json))
            }
            StartOutcome::Steered { run_id, receipt } => {
                let resp = CreateRunResponse {
                    status: Some(build_run_status(state, &run_id)?),
                    inbox: Some(receipt),
                };
                let (code, json) = json_response_accepted("vam.v1.CreateRunResponse", &resp)?;
                Ok(json_http_response(code, json))
            }
            StartOutcome::Queued {
                receipt,
                workspace_id,
            } => super::inbox::queued_response(state, receipt, &workspace_id),
            StartOutcome::Started {
                run_id,
                workspace_id,
                thread_label,
                thread_id,
                session_id,
            } => {
                let resp = CreateRunResponse {
                    status: Some(build_run_status(state, &run_id)?),
                    inbox: None,
                };
                let (code, mut json) = json_response_accepted("vam.v1.CreateRunResponse", &resp)?;
                if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(&json) {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("workspaceId".into(), workspace_id.clone().into());
                        obj.insert("threadRef".into(), thread_label.into());
                        obj.insert("spaceId".into(), workspace_id.into());
                        obj.insert("threadId".into(), thread_id.into());
                        obj.insert("sessionId".into(), session_id.into());
                    }
                    json = serde_json::to_string(&v).unwrap_or(json);
                }
                Ok(json_http_response(code, json))
            }
        }
    }
}

/// Starts a run for `caller` from `req`, or hands the message to the live run / the queue.
pub async fn start_run(
    state: &AppState,
    caller: &RunCaller,
    req: CreateRunRequest,
    opts: StartOpts,
) -> Result<StartOutcome, StartError> {
    let mut spec = req
        .spec
        .ok_or_else(|| OrchestratorError::InvalidContract("RunSpec required".into()))?;
    if spec.idempotency_key.is_empty() {
        spec.idempotency_key = opts.idempotency_key.clone();
    }

    if let Some(existing_run_id) = state
        .store
        .find_run_by_idempotency_key(caller.owner_user_id.as_str(), &spec.idempotency_key)?
    {
        return Ok(StartOutcome::Existing {
            run_id: existing_run_id,
        });
    }

    let requested_urn = crate::run_context::requested_agent_urn(&spec).or_else(|| {
        req.inline_agent
            .as_ref()
            .and_then(|agent| agent.resource_id.as_ref())
            .map(|id| id.urn.clone())
            .filter(|urn| !urn.is_empty())
    });
    // Ids are the identity (locked decision 10): a Space id (owner checked) or the agent's
    // default Space, a thread id (inside that Space) or a new thread. Names are labels only.
    let kind = if spec
        .agent_ref
        .as_ref()
        .and_then(|r| r.version.as_ref())
        .is_some_and(|v| v.prerelease == "draft")
    {
        crate::spaces::DefaultKind::Draft
    } else {
        crate::spaces::DefaultKind::Released
    };
    let place = crate::spaces::resolve_place(
        &state.store,
        &crate::spaces::PlaceRequest {
            owner: caller.owner_user_id.as_str(),
            space_id: &spec.space_id,
            thread_id: &spec.thread_id,
            thread_label: &crate::run_context::thread_label_from_spec(&spec),
            space_label: &crate::run_context::space_label_from_uri(&spec.workspace_ref_uri),
            agent_urn: requested_urn.as_deref(),
            kind,
        },
    )?;
    let workspace_id = place.space.space_id.clone();
    let thread_ref = place.thread.thread_id.clone();
    let thread_label = place.thread.label.clone();
    spec.workspace_ref_uri = format!("volume://{workspace_id}");
    spec.thread_ref = thread_ref.clone();
    spec.space_id = workspace_id.clone();
    spec.thread_id = thread_ref.clone();

    // A live task on this workspace takes the message as a steer or a queued message
    // (same thread); any other answer is today's retryable refusal.
    match inbox::admit(inbox::Candidate {
        state,
        caller,
        spec: &spec,
        has_inline: req.inline_agent.is_some() || req.inline_playbook.is_some(),
        requested_urn: requested_urn.as_deref(),
        workspace_id: &workspace_id,
        thread_ref: &thread_ref,
        skip_if_busy: opts.skip_if_busy,
    })? {
        inbox::Admit::Proceed => {}
        inbox::Admit::Steered { run_id, receipt } => {
            return Ok(StartOutcome::Steered { run_id, receipt })
        }
        inbox::Admit::Queued { receipt } => {
            return Ok(StartOutcome::Queued {
                receipt,
                workspace_id,
            })
        }
        inbox::Admit::Busy { kind, refusal } => return Err(StartError::Busy { kind, refusal }),
    }

    let workspace_meta = read_bound_workspace_meta(&workspace_id)?;

    if let (Some(bound), Some(requested)) = (
        workspace_meta.as_ref().and_then(|m| m.agent_urn.as_deref()),
        requested_urn.as_deref(),
    ) {
        if bound != requested {
            return Err(OrchestratorError::AgentWorkspaceMismatch {
                workspace_id: workspace_id.clone(),
                bound: bound.to_string(),
                requested: requested.to_string(),
            }
            .into());
        }
    }

    // The Space row holds the bound agent (the workspace file keeps the version pin).
    if let Some(requested) = requested_urn.as_deref() {
        match place.space.agent_urn.as_deref().filter(|u| !u.is_empty()) {
            Some(bound) if bound != requested => {
                return Err(OrchestratorError::AgentWorkspaceMismatch {
                    workspace_id: workspace_id.clone(),
                    bound: bound.to_string(),
                    requested: requested.to_string(),
                }
                .into());
            }
            Some(_) => {}
            None => state.store.bind_space_agent(
                caller.owner_user_id.as_str(),
                &workspace_id,
                requested,
            )?,
        }
    }

    // A run with no agentRef and an inline agent does not load the workspace's
    // bound agent, so the bound pin must not be applied to it.
    let pin_meta = workspace_meta
        .as_ref()
        .filter(|_| spec.agent_ref.is_some() || req.inline_agent.is_none());
    crate::catalog_resolve::apply_workspace_version_pin(&mut spec, pin_meta, &workspace_id)?;

    let agent = if let Some(inline) = req.inline_agent {
        if inline.resource_id.is_some() {
            let _ = state
                .store
                .put_agent_with_owner(&inline, caller.owner_user_id.as_str())?;
        }
        inline
    } else if let Some(urn) = requested_urn {
        crate::catalog_resolve::load_agent_for_run(
            &state.store,
            &spec,
            &urn,
            caller.owner_user_id.as_str(),
            caller.users_admin,
        )?
    } else if let Some(meta) = &workspace_meta {
        if let Some(bound) = &meta.agent_urn {
            crate::catalog_resolve::load_agent_for_run(
                &state.store,
                &spec,
                bound,
                caller.owner_user_id.as_str(),
                caller.users_admin,
            )?
        } else {
            return Err(OrchestratorError::WorkspaceNotBound {
                workspace_id: workspace_id.clone(),
            }
            .into());
        }
    } else {
        return Err(OrchestratorError::InvalidContract(
            "agent_ref or inline_agent required for unbound workspace".into(),
        )
        .into());
    };

    if let Some(pb) = req.inline_playbook.as_ref() {
        if pb.resource_id.is_some() {
            let _ = state
                .store
                .put_playbook_with_owner(pb, caller.owner_user_id.as_str())?;
        }
    }

    // Admission: fail fast when the run's skills declare required
    // secrets the caller has not configured. Lists binding + scope.
    admit_run_secret_requirements(state, caller.owner_user_id.as_str(), &agent)?;

    let (allowlist, node_default) = state.store.node_model_settings()?;
    let catalog_default = vam_inference::ModelCatalog::new()
        .default_model()
        .to_string();
    let resolved = crate::allowed_models::resolve_run_model(
        &mut spec,
        &agent,
        &crate::allowed_models::NodeModels {
            allowlist: &allowlist,
            default: node_default.as_deref(),
            catalog_default: &catalog_default,
        },
    )?;
    let model = resolved.model.clone();
    // The host decides the provider; the guest never guesses it from the model id.
    let default_gateway = state.store.get_operator_default_gateway()?;
    let provider = crate::allowed_models::resolve_model_provider(
        &mut spec,
        &agent,
        &model,
        &vam_inference::ModelCatalog::new(),
        &default_gateway,
    )?;
    crate::allowed_models::require_scripted_mode_for_model(&model, state.e2e_mock_scripts)?;
    #[cfg(feature = "process-guest")]
    crate::process_mode::require_mock_model(&model).map_err(|e| match e {
        // The provider is decided before this refusal, so the answer shows what the host chose.
        OrchestratorError::InvalidContract(m) => {
            OrchestratorError::InvalidContract(format!("{m} (provider: {provider})"))
        }
        other => other,
    })?;

    let run_id = spec
        .run_id
        .as_ref()
        .map(|r| r.urn.clone())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| format!("urn:vam:run:local.vam.dev/acme/run-{}", uuid_like()));
    spec.run_id = Some(ResourceId {
        urn: run_id.clone(),
    });

    crate::run_context::ensure_playbook_ref(&mut spec, req.inline_playbook.as_ref());
    crate::catalog_resolve::record_resolved_refs(
        &state.store,
        &mut spec,
        &agent,
        caller.owner_user_id.as_str(),
        caller.users_admin,
    )?;
    // Resolved once, then persisted. Resume and active-time expiry read the
    // stored RunSpec; they do not resolve budgets again.
    crate::run_context::resolve_run_budgets(&mut spec, &agent);
    // The harness profile follows the model's catalog entry unless the caller named one.
    crate::run_context::default_harness_profile(
        &mut spec,
        crate::run_context::catalog_profile_for(&model).as_deref(),
    );
    crate::run_context::resolve_harness(&mut spec)?;
    let prepared = crate::run_context::prepare_playbook_admission(
        &state.store,
        &spec,
        caller.owner_user_id.as_str(),
        caller.users_admin,
    )?;
    if let Some(prepared) = &prepared {
        spec.inputs = Some(crate::run_context::insert_playbook_context(
            spec.inputs.clone(),
            prepared,
        ));
        spec.inputs = crate::playbook_activation::insert_attached(spec.inputs.clone(), &[]);
    } else {
        // No playbook named: the run stays a plain run, and the model may start
        // one of the agent's attached playbooks with `start_playbook`. The
        // default is never bound automatically.
        let attached = crate::playbook_activation::build_attached(
            &state.store,
            &spec,
            &agent,
            caller.owner_user_id.as_str(),
            caller.users_admin,
        )?;
        spec.inputs = crate::playbook_activation::insert_attached(spec.inputs.clone(), &attached);
    }

    let agent_urn = crate::run_context::agent_urn(&agent);
    let agent_digest = crate::run_context::agent_digest_hex(&agent, &spec);
    let spec_json = vam_spec::encoding::proto_to_json("vam.v1.RunSpec", &spec)
        .map_err(OrchestratorError::Core)?;

    let session_id = format!("sess-run-{}", uuid_like());
    state.store.persist_run_spec_with_origin(
        &run_id,
        &spec_json,
        Some(&session_id),
        &agent_urn,
        Some(&spec.idempotency_key),
        &caller.principal,
        caller.owner_user_id.as_str(),
        caller.source.record().as_deref(),
        caller.credential_id.as_deref(),
    )?;

    state.store.create_run(&ResourceId {
        urn: run_id.clone(),
    })?;
    // Which model the run got and why; evidence only, so a failure is logged.
    {
        let mut payload = serde_json::json!({
            "model": resolved.model,
            "source": resolved.source.as_str(),
            "provider": provider,
        });
        if let Some(note) = &resolved.note {
            payload["note"] = serde_json::json!(note);
        }
        if let Err(err) = state.store.append_outbox_event(
            &run_id,
            crate::events::TYPE_MODEL_RESOLVED,
            &payload.to_string(),
        ) {
            tracing::warn!("model.resolved write failed for {run_id}: {err}");
        }
    }
    // Who asked and from where; evidence only, so a failure is logged.
    if let Err(err) = state.store.record_run_request_for(&run_id, caller) {
        tracing::warn!("run request write failed for {run_id}: {err}");
    }
    // The task's own message is the first of the URLs `web_fetch` may take
    // (AR-1.5). Earlier tasks of the thread do not count.
    if let Err(err) = state
        .store
        .record_prompt_context(&run_id, &extract_prompt_from_inputs(spec.inputs.as_ref()))
    {
        tracing::warn!("prompt context write failed for {run_id}: {err}");
    }
    if let Some(prepared) = &prepared {
        if let Err(err) =
            crate::run_context::commit_playbook_admission(&state.store, &run_id, prepared)
        {
            let _ = state.store.update_run_state(&run_id, "FAILED");
            return Err(err.into());
        }
    }

    // Phase 5: host-initiated OnRequest hooks before session / VM boot.
    let on_request = crate::hsp_hooks::HspHookDispatcher::new(state.store.as_ref())
        .dispatch_on_request(&run_id, &session_id, Some(spec.clone()))
        .await
        .map_err(ApiError::from)?;
    if on_request.aborted {
        let _ = state.store.update_run_state(&run_id, "FAILED");
        return Err(OrchestratorError::Hsp(
            on_request
                .abort_reason
                .unwrap_or_else(|| "on_request hook aborted run".into()),
        )
        .into());
    }

    if let Err(e) = state.store.create_session_with_owner(
        &session_id,
        Some(&format!("run-session-{}", spec.idempotency_key)),
        &format!("vm-{}", session_id),
        &workspace_id,
        "default",
        caller.owner_user_id.as_str(),
    ) {
        let _ = state.store.update_run_state(&run_id, "FAILED");
        return Err(e.into());
    }

    let manager = state.manager.clone();
    let store_bg = state.store.clone();
    let run_id_bg = run_id.clone();
    let session_id_bg = session_id.clone();
    let workspace_id_bg = workspace_id.clone();
    let thread_ref_bg = thread_ref.clone();
    let spec_idem = spec.idempotency_key.clone();
    let spec_json_bg = spec_json.clone();
    let agent_urn_bg = agent_urn.clone();
    let agent_digest_bg = agent_digest.clone();
    let user_prompt = crate::run_context::extract_prompt_from_inputs(&spec);
    let run_inputs = crate::run_context::merge_agent_into_inputs(spec.inputs.clone(), &agent);
    let owner = caller.principal.clone();
    let owner_user_id = caller.owner_user_id.as_str().to_string();
    let model_bg = model.clone();

    let phase_timer =
        crate::timing::PhaseTimer::shared(run_id.clone(), session_id.clone(), workspace_id.clone());
    let inbound_parent = opts.traceparent.clone();

    tokio::spawn(async move {
        run_lifecycle(
            manager,
            store_bg,
            run_id_bg,
            session_id_bg,
            workspace_id_bg,
            thread_ref_bg,
            spec_idem,
            spec_json_bg,
            agent_urn_bg,
            agent_digest_bg,
            user_prompt,
            run_inputs,
            model_bg,
            owner,
            owner_user_id,
            Some(phase_timer),
            inbound_parent,
        )
        .await;
    });

    Ok(StartOutcome::Started {
        run_id,
        workspace_id,
        thread_label,
        thread_id: thread_ref,
        session_id,
    })
}
