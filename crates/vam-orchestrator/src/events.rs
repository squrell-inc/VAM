//! CloudEvents 1.0 structured JSON builders for the workflow outbox.

use prost::Message;
use prost_types::{Any, Timestamp};
use sha2::{Digest as Sha2Digest, Sha256};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use vam_spec::encoding::proto_to_json;
use vam_spec::v1::{
    ArtifactRef, EgressDecidedPayload, HitlDecidedPayload, HitlExpiredPayload, HitlPausedPayload,
    InferenceCompletedPayload, ResourceId, SecretAccessedPayload, SessionBootedPayload,
    VamCloudEvent,
};

pub const VAMD_SOURCE_URN: &str = "urn:vam:node:local.vam.dev/acme/vamd";

pub const TYPE_INFERENCE_COMPLETED: &str = "vam.v1.inference.completed";
/// A thread's older turns were replaced by a summary in the model's context (AR-2.10).
/// Emitted by the thread recorder once the `compaction` record is durable.
pub const TYPE_CONTEXT_COMPACTED: &str = "vam.v1.context.compacted";
// Names the runner may expect before any emitter exists (steer and outbox scenarios).
pub const TYPE_INBOX_RECEIVED: &str = "vam.v1.inbox.received";
pub const TYPE_INBOX_DELIVERED: &str = "vam.v1.inbox.delivered";
pub const TYPE_INBOX_DISCARDED: &str = "vam.v1.inbox.discarded";
pub const TYPE_INBOX_STARTED: &str = "vam.v1.inbox.started";
pub const TYPE_OUTBOX_MESSAGE: &str = "vam.v1.outbox.message";
pub const TYPE_OUTBOX_CARD: &str = "vam.v1.outbox.card";
/// A tool result that has a skill card did not get one. Payload: tool, callId, reason.
pub const TYPE_CARD_SKIPPED: &str = "vam.v1.card.skipped";
/// A turn or session-end workspace commit failed; the work was kept (AR-2.8).
/// Payload: workspace, session, run, kept id (never a host path), error, phase.
pub const TYPE_WORKSPACE_COMMIT_FAILED: &str = "vam.v1.workspace.commit_failed";
pub const TYPE_EGRESS_DECIDED: &str = "vam.v1.egress.decided";
pub const TYPE_SECRET_ACCESSED: &str = "vam.v1.secret.accessed";
pub const TYPE_SESSION_BOOTED: &str = "vam.v1.session.booted";
/// The model a run was given and why (`model`, `source`, and a `note` when the agent's
/// preferences could not be used).
/// A broker's fire was taken (started or queued): grant id, trigger ref, fire kind.
pub const TYPE_TRIGGER_FIRED: &str = "vam.v1.trigger.fired";
/// A broker's grant expires within 7 days: grant id, broker, tool, `expiresAt` (unix s).
pub const TYPE_GRANT_EXPIRING: &str = "vam.v1.grant.expiring";
/// An outside call was written to the effect journal (no emitter yet).
pub const TYPE_EFFECT_RECORDED: &str = "vam.v1.effect.recorded";
/// A call that already happened was answered from the journal, not sent again (no emitter yet).
pub const TYPE_EFFECT_REPLAYED: &str = "vam.v1.effect.replayed";
/// A call whose answer was lost was settled by a declared read or by the model (no emitter yet).
pub const TYPE_EFFECT_RECONCILED: &str = "vam.v1.effect.reconciled";
/// A call to an MCP connection that needs signing in again was parked until it is reconnected.
pub const TYPE_CONNECTION_NEEDS_REAUTH: &str = "vam.v1.connection.needs_reauth";
/// A task that crashed or was cut by a restart was picked up again (no emitter yet).
pub const TYPE_RUN_RESUMED: &str = "vam.v1.run.resumed";
/// The run's model kept failing and another allowlisted model took over: `from`, `to`,
/// `provider` (of `to`) and `reason` (`http_429`, `http_529`, `http_5xx` or `transport`).
/// Written once per task.
pub const TYPE_MODEL_FALLBACK: &str = "vam.v1.model.fallback";

/// Payload of [`TYPE_MODEL_FALLBACK`], shaped like `model.resolved`.
pub fn model_fallback_payload(switch: &vam_inference::ModelFallback) -> serde_json::Value {
    serde_json::json!({
        "from": switch.from,
        "to": switch.to,
        "provider": switch.provider,
        "reason": switch.reason,
    })
}
/// The one resume of a parked run was told several decisions: the request ids, kinds and
/// decisions it carried (`delivered`) and the late ones of an earlier resume it reported (`late`).
pub const TYPE_RESUME_DECISIONS: &str = "vam.v1.run.resume_decisions";
/// A decision landed after the resume read its decisions: recorded, started nothing, and is
/// reported to the task at the next resume.
pub const TYPE_DECISION_DEFERRED: &str = "vam.v1.hitl.decision_deferred";
pub const TYPE_MODEL_RESOLVED: &str = "vam.v1.model.resolved";
/// The scripted mock model answered a call from a rule (test-only, ES-1.4).
pub const TYPE_MOCK_RULE_MATCHED: &str = "vam.v1.mock.rule_matched";
/// The scripted mock model had no answer for a call (test-only, ES-1.4).
pub const TYPE_MOCK_UNMATCHED: &str = "vam.v1.mock.unmatched";
/// One web tool call (AR-1.6). Carries no query, snippet or page text.
pub const TYPE_WEB_USED: &str = "vam.v1.web.used";
/// The final answer cited `[S<n>]` ids no source of the run answers to (AR-1.8).
/// Flag only: carries ids and counts, never answer text.
pub const TYPE_WEB_CITATIONS_UNRESOLVED: &str = "vam.v1.web.citations_unresolved";

/// Payload of [`TYPE_WEB_CITATIONS_UNRESOLVED`]. Ids are kept only if they have
/// the shape `S<digits>`.
pub fn web_citations_unresolved_payload(
    unresolved: &[String],
    resolved: usize,
    total_sources: usize,
) -> serde_json::Value {
    let ids: Vec<&String> = unresolved
        .iter()
        .filter(|id| {
            id.len() <= 10
                && id.starts_with('S')
                && id.len() > 1
                && id[1..].bytes().all(|b| b.is_ascii_digit())
        })
        .take(50)
        .collect();
    serde_json::json!({
        "unresolved": ids,
        "resolved": resolved,
        "total_sources": total_sources,
    })
}

fn clean_label(raw: &str, max: usize) -> String {
    raw.chars()
        .take(max)
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Payload of [`TYPE_MOCK_RULE_MATCHED`]: script, rule index, optional step label.
pub fn mock_rule_matched_payload(
    script: &str,
    rule: usize,
    step: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "script": clean_label(script, 128),
        "rule": rule,
    });
    if let Some(step) = step {
        payload["step"] = serde_json::Value::String(clean_label(step, 128));
    }
    payload
}

/// Payload of [`TYPE_MOCK_UNMATCHED`]. `preview` is the caller's redacted text, cut to 80 chars.
pub fn mock_unmatched_payload(
    script: &str,
    reason: &str,
    last_role: &str,
    preview: &str,
    step: Option<&str>,
) -> serde_json::Value {
    let mut payload = serde_json::json!({
        "script": clean_label(script, 128),
        "reason": match reason {
            "no_rule" | "tool_not_offered" | "not_loaded" => reason,
            _ => "no_rule",
        },
        "last_role": clean_label(last_role, 16),
        "preview": preview
            .chars()
            .take(80)
            .map(|c| if c.is_control() { ' ' } else { c })
            .collect::<String>(),
    });
    if let Some(step) = step {
        payload["step"] = serde_json::Value::String(clean_label(step, 128));
    }
    payload
}

static EVENT_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

fn event_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let seq = EVENT_ID_COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{now}-{seq}")
}

fn now_timestamp() -> Timestamp {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    Timestamp {
        seconds: now.as_secs() as i64,
        nanos: now.subsec_nanos() as i32,
    }
}

fn pack_any<T: Message>(type_name: &str, msg: &T) -> Result<Any, String> {
    let mut buf = Vec::new();
    msg.encode(&mut buf)
        .map_err(|e| format!("encode {type_name}: {e}"))?;
    Ok(Any {
        type_url: format!("type.googleapis.com/{type_name}"),
        value: buf,
    })
}

fn build_cloud_event(
    event_type: &str,
    subject_urn: &str,
    run_id: &str,
    sequence: i64,
    data: Any,
) -> Result<VamCloudEvent, String> {
    Ok(VamCloudEvent {
        id: event_id(),
        source: Some(ResourceId {
            urn: VAMD_SOURCE_URN.to_string(),
        }),
        spec_version: "1.0".to_string(),
        r#type: event_type.to_string(),
        time: Some(now_timestamp()),
        subject: Some(ResourceId {
            urn: subject_urn.to_string(),
        }),
        data_content_type: "application/json".to_string(),
        data: Some(data),
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        sequence_number: sequence,
        trace_context: None,
    })
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
pub fn hitl_paused_json(
    run_id: &str,
    sequence: i64,
    request_id: &str,
    tool_name: &str,
    reason: &str,
    attempt_id: &str,
    checkpoint: &ArtifactRef,
    args_digest: &str,
    transition_revision: i64,
) -> Result<String, String> {
    let payload = HitlPausedPayload {
        token_id: None,
        tool_name: tool_name.to_string(),
        reason: reason.to_string(),
        approval_request_id: Some(ResourceId {
            urn: request_id.to_string(),
        }),
        attempt_id: Some(ResourceId {
            urn: attempt_id.to_string(),
        }),
        workspace_checkpoint: Some(checkpoint.clone()),
        transition_revision,
        requested_args_digest: args_digest.to_string(),
    };
    let any = pack_any("vam.v1.HitlPausedPayload", &payload)?;
    let event = build_cloud_event("vam.v1.hitl.paused", request_id, run_id, sequence, any)?;
    proto_to_json("vam.v1.VamCloudEvent", &event)
}

pub fn hitl_decided_json(
    run_id: &str,
    sequence: i64,
    request_id: &str,
    decision: &str,
    reviewer_principal: &str,
    args_digest: &str,
    transition_revision: i64,
) -> Result<String, String> {
    let payload = HitlDecidedPayload {
        approval_request_id: Some(ResourceId {
            urn: request_id.to_string(),
        }),
        decision: decision.to_string(),
        reviewer_principal: reviewer_principal.to_string(),
        args_digest: args_digest.to_string(),
        transition_revision,
    };
    let any = pack_any("vam.v1.HitlDecidedPayload", &payload)?;
    let event = build_cloud_event("vam.v1.hitl.decided", request_id, run_id, sequence, any)?;
    proto_to_json("vam.v1.VamCloudEvent", &event)
}

pub fn hitl_expired_json(
    run_id: &str,
    sequence: i64,
    request_id: &str,
    step_id: &str,
    attempt_id: &str,
) -> Result<String, String> {
    let payload = HitlExpiredPayload {
        approval_request_id: Some(ResourceId {
            urn: request_id.to_string(),
        }),
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        step_id: step_id.to_string(),
        attempt_id: Some(ResourceId {
            urn: attempt_id.to_string(),
        }),
    };
    let any = pack_any("vam.v1.HitlExpiredPayload", &payload)?;
    let event = build_cloud_event("vam.v1.hitl.expired", request_id, run_id, sequence, any)?;
    proto_to_json("vam.v1.VamCloudEvent", &event)
}

/// A CloudEvents 1.0 JSON event whose `data` is `data` as given (the outbox events
/// `outbox.message {text}` and `outbox.card {jig, data, card_id?}`, AR-3.9). The
/// envelope is the same as the typed events'; only `data` is free-form, because
/// its shape is the built-in jig's and is checked before it gets here.
pub fn outbox_event_json(
    event_type: &str,
    run_id: &str,
    data: serde_json::Value,
) -> Result<String, String> {
    let event = VamCloudEvent {
        id: event_id(),
        source: Some(ResourceId {
            urn: VAMD_SOURCE_URN.to_string(),
        }),
        spec_version: "1.0".to_string(),
        r#type: event_type.to_string(),
        time: Some(now_timestamp()),
        subject: Some(ResourceId {
            urn: format!("{run_id}/outbox"),
        }),
        data_content_type: "application/json".to_string(),
        data: None,
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        sequence_number: 0,
        trace_context: None,
    };
    let mut json: serde_json::Value =
        serde_json::from_str(&proto_to_json("vam.v1.VamCloudEvent", &event)?)
            .map_err(|e| e.to_string())?;
    json["data"] = data;
    Ok(json.to_string())
}

pub fn args_digest_from_json(value: &serde_json::Value) -> String {
    format!("{:x}", Sha256::digest(value.to_string().as_bytes()))
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
pub fn inference_completed_json(
    run_id: &str,
    sequence: i64,
    model_name: &str,
    provider: &str,
    prompt_tokens: i64,
    completion_tokens: i64,
    duration_ms: i64,
    spend_micro: Option<i64>,
    outcome_preview: Option<&str>,
    tool_calls_preview: Option<&str>,
    outcome_cas: Option<&str>,
    tool_calls_cas: Option<&str>,
    cache: vam_inference::CacheTokens,
    extras: &vam_inference::CompletionExtras,
) -> Result<String, String> {
    let payload = InferenceCompletedPayload {
        model_name: model_name.to_string(),
        provider: provider.to_string(),
        prompt_tokens,
        completion_tokens,
        total_tokens: prompt_tokens + completion_tokens,
        duration_ms,
        spend_micro,
        outcome_preview: outcome_preview.map(|s| s.to_string()),
        tool_calls_preview: tool_calls_preview.map(|s| s.to_string()),
        outcome_cas: outcome_cas.map(|s| s.to_string()),
        tool_calls_cas: tool_calls_cas.map(|s| s.to_string()),
        cached_tokens: (cache.read > 0).then_some(cache.read),
        cache_creation_tokens: (cache.written > 0).then_some(cache.written),
        cached_tokens_unknown: cache.unknown.then_some(true),
        reasoning_bytes: extras.reasoning_bytes,
        reasoning_tool_markup: extras.reasoning_tool_markup,
        reasoning_mode: extras.reasoning_mode.clone(),
        adapter_notes: extras.adapter_notes.clone(),
        resamples: extras.resamples,
    };
    let any = pack_any("vam.v1.InferenceCompletedPayload", &payload)?;
    let event = build_cloud_event(
        "vam.v1.inference.completed",
        &format!("{run_id}/inference/{model_name}"),
        run_id,
        sequence,
        any,
    )?;
    proto_to_json("vam.v1.VamCloudEvent", &event)
}

pub fn egress_decided_json(
    run_id: &str,
    sequence: i64,
    domain: &str,
    port: u32,
    disposition: &str,
    policy_id: &str,
) -> Result<String, String> {
    let domain_digest = format!("{:x}", Sha256::digest(domain.as_bytes()));
    let payload = EgressDecidedPayload {
        domain: domain.to_string(),
        port,
        disposition: disposition.to_string(),
        policy_id: policy_id.to_string(),
        domain_digest,
    };
    let any = pack_any("vam.v1.EgressDecidedPayload", &payload)?;
    let event = build_cloud_event(
        "vam.v1.egress.decided",
        &format!("{run_id}/egress/{domain}"),
        run_id,
        sequence,
        any,
    )?;
    proto_to_json("vam.v1.VamCloudEvent", &event)
}

pub fn secret_accessed_json(
    run_id: &str,
    sequence: i64,
    secret_name: &str,
    adapter_id: &str,
) -> Result<String, String> {
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let payload = SecretAccessedPayload {
        secret_name: secret_name.to_string(),
        adapter_id: adapter_id.to_string(),
        access_time_unix_ms: now_ms,
    };
    let any = pack_any("vam.v1.SecretAccessedPayload", &payload)?;
    let event = build_cloud_event(
        "vam.v1.secret.accessed",
        &format!("{run_id}/secret/{secret_name}"),
        run_id,
        sequence,
        any,
    )?;
    proto_to_json("vam.v1.VamCloudEvent", &event)
}

pub fn session_booted_json(
    run_id: &str,
    sequence: i64,
    session_id: &str,
    boot_path: &str,
    boot_duration_ms: i64,
    snapshot_id: Option<String>,
) -> Result<String, String> {
    let payload = SessionBootedPayload {
        session_id: session_id.to_string(),
        boot_path: boot_path.to_string(),
        boot_duration_ms,
        snapshot_id,
    };
    let any = pack_any("vam.v1.SessionBootedPayload", &payload)?;
    let event = build_cloud_event("vam.v1.session.booted", session_id, run_id, sequence, any)?;
    proto_to_json("vam.v1.VamCloudEvent", &event)
}

/// Records `session.booted` for a session: under the session id (as before) and, when the session
/// boots for a run, under the run id too, so the run's events (`GET .../events`) show it.
pub fn append_session_booted(
    store: &crate::workflow::SqliteWorkflowStore,
    session_id: &str,
    boot_path: &str,
) {
    if let Ok(payload) = session_booted_json(session_id, 0, session_id, boot_path, 0, None) {
        let _ = store.append_outbox_event(session_id, TYPE_SESSION_BOOTED, &payload);
    }
    if let Ok(Some(run_id)) = store.find_run_id_for_session(session_id) {
        if let Ok(payload) = session_booted_json(&run_id, 0, session_id, boot_path, 0, None) {
            let _ = store.append_outbox_event(&run_id, TYPE_SESSION_BOOTED, &payload);
        }
    }
}

/// Records `egress.decided` for a session: under the session id (as before) and, when the session
/// belongs to a run, under the run id too, so the run's events (`GET .../events`) and its evidence
/// pack show the decision. The run copy's subject is the run id.
pub fn append_egress_decided(
    store: &crate::workflow::SqliteWorkflowStore,
    session_id: &str,
    domain: &str,
    port: u32,
    disposition: &str,
    policy_id: &str,
) {
    if let Ok(payload) = egress_decided_json(session_id, 0, domain, port, disposition, policy_id) {
        let _ = store.append_outbox_event(session_id, TYPE_EGRESS_DECIDED, &payload);
    }
    if let Ok(Some(run_id)) = store.find_run_id_for_session(session_id) {
        if let Ok(payload) = egress_decided_json(&run_id, 0, domain, port, disposition, policy_id) {
            let _ = store.append_outbox_event(&run_id, TYPE_EGRESS_DECIDED, &payload);
        }
    }
}

/// A run event whose `data` is the plain JSON object `data`: the CloudEvent envelope the typed
/// events carry, so a reader finds the fields at `data.<name>` (`GET .../events`, the e2e
/// checks). For events with no typed payload message.
pub fn cloud_event_json(
    run_id: &str,
    event_type: &str,
    data: &serde_json::Value,
) -> Result<String, String> {
    let mut event = build_cloud_event_without_data(event_type, run_id, run_id, 0)?;
    event.data = None;
    let json = proto_to_json("vam.v1.VamCloudEvent", &event)?;
    let mut envelope: serde_json::Value =
        serde_json::from_str(&json).map_err(|e| format!("event envelope: {e}"))?;
    if let Some(object) = envelope.as_object_mut() {
        object.insert("data".to_string(), data.clone());
    }
    Ok(envelope.to_string())
}

fn build_cloud_event_without_data(
    event_type: &str,
    subject_urn: &str,
    run_id: &str,
    sequence: i64,
) -> Result<VamCloudEvent, String> {
    build_cloud_event(
        event_type,
        subject_urn,
        run_id,
        sequence,
        Any {
            type_url: String::new(),
            value: Vec::new(),
        },
    )
}

/// Appends a run event built by [`cloud_event_json`]. A failure is logged, never raised.
pub fn append_run_event(
    store: &crate::workflow::SqliteWorkflowStore,
    run_id: &str,
    event_type: &str,
    data: &serde_json::Value,
) {
    if run_id.is_empty() {
        return;
    }
    match cloud_event_json(run_id, event_type, data) {
        Ok(json) => {
            if let Err(e) = store.append_outbox_event(run_id, event_type, &json) {
                tracing::warn!("{event_type} not recorded for {run_id}: {e}");
            }
        }
        Err(e) => tracing::warn!("{event_type} not built for {run_id}: {e}"),
    }
}

pub fn cloud_event_from_raw(
    run_id: &str,
    sequence: i64,
    event_type: &str,
    payload_json: &str,
) -> VamCloudEvent {
    VamCloudEvent {
        id: event_id(),
        source: Some(ResourceId {
            urn: VAMD_SOURCE_URN.to_string(),
        }),
        spec_version: "1.0".to_string(),
        r#type: event_type.to_string(),
        time: Some(now_timestamp()),
        subject: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        data_content_type: "application/json".to_string(),
        data: Some(Any {
            type_url: "type.googleapis.com/google.protobuf.Struct".to_string(),
            value: payload_json.as_bytes().to_vec(),
        }),
        run_id: Some(ResourceId {
            urn: run_id.to_string(),
        }),
        sequence_number: sequence,
        trace_context: None,
    }
}

/// Payload of [`TYPE_WEB_USED`]. Only fixed-vocabulary fields and counts:
/// `host` is the hostname of the call's `url` argument (no path, query or
/// userinfo), `status` is `ok`, `redirected` or `error`, and `code` is the
/// broker's typed error code when it has the shape `[a-z_]{1,32}`.
pub fn web_used_payload(
    broker: &str,
    tool: &str,
    host: &str,
    status: &str,
    code: Option<&str>,
    bytes: usize,
    sources_added: usize,
) -> serde_json::Value {
    let clean = |raw: &str, max: usize| -> String {
        raw.chars()
            .take(max)
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':' | '/') {
                    c
                } else {
                    '_'
                }
            })
            .collect()
    };
    let mut payload = serde_json::json!({
        "broker": clean(broker, 128),
        "tool": clean(tool, 128),
        "host": clean(host, 253),
        "status": match status {
            "ok" | "redirected" => status,
            _ => "error",
        },
        "bytes": bytes,
        "sources_added": sources_added,
    });
    if let Some(code) = code.filter(|c| {
        (1..=32).contains(&c.len()) && c.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_')
    }) {
        payload["code"] = serde_json::Value::String(code.to_string());
    }
    payload
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_plain_event_carries_its_fields_under_data() {
        let json = cloud_event_json(
            "urn:vam:run:local.vam.dev/acme/r",
            TYPE_RUN_RESUMED,
            &serde_json::json!({"reason": "guest_crashed", "generation": 2}),
        )
        .unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["type"], TYPE_RUN_RESUMED);
        assert_eq!(v["data"]["reason"], "guest_crashed", "{json}");
        assert_eq!(v["data"]["generation"], 2.0);
    }

    #[test]
    fn session_booted_lands_on_the_run_of_the_session() {
        let store = crate::workflow::SqliteWorkflowStore::in_memory().expect("store");
        store
            .persist_run_spec(
                "run-1",
                "{}",
                Some("sess-1"),
                "urn:vam:agent:local.vam.dev/acme/a",
                None,
                "User::\"tester\"",
            )
            .expect("run");
        append_session_booted(&store, "sess-1", "restore");
        let on_run = store.read_outbox_for_run("run-1", 0).expect("run events");
        let booted: Vec<_> = on_run
            .iter()
            .filter(|e| e.event_type == TYPE_SESSION_BOOTED)
            .collect();
        assert_eq!(booted.len(), 1, "{on_run:?}");
        assert!(booted[0].payload.contains("restore"));
        // The session-keyed row stays for readers that use it.
        let on_session = store
            .read_outbox_for_run("sess-1", 0)
            .expect("session events");
        assert!(on_session
            .iter()
            .any(|e| e.event_type == TYPE_SESSION_BOOTED));
    }

    #[test]
    fn session_booted_without_a_run_stays_on_the_session() {
        let store = crate::workflow::SqliteWorkflowStore::in_memory().expect("store");
        append_session_booted(&store, "sess-2", "cold");
        let on_session = store.read_outbox_for_run("sess-2", 0).expect("events");
        assert_eq!(on_session.len(), 1);
    }

    use super::*;
    use vam_spec::v1::Digest;

    fn persist_run(store: &crate::workflow::SqliteWorkflowStore, run_id: &str, session_id: &str) {
        store
            .persist_run_spec(
                run_id,
                "{}",
                Some(session_id),
                "urn:vam:agent:local.vam.dev/acme/a",
                None,
                "User::\"tester\"",
            )
            .expect("run");
    }

    fn decode_egress(payload: &str) -> (String, EgressDecidedPayload) {
        use prost::Message;
        let event: VamCloudEvent =
            vam_spec::encoding::json_to_proto("vam.v1.VamCloudEvent", payload).expect("event");
        let subject = event.subject.expect("subject").urn;
        let data = event.data.expect("data");
        (
            subject,
            EgressDecidedPayload::decode(data.value.as_slice()).expect("payload"),
        )
    }

    #[test]
    fn egress_decided_lands_on_the_session_and_its_run() {
        let store = crate::workflow::SqliteWorkflowStore::in_memory().expect("store");
        persist_run(&store, "run-e1", "sess-e1");
        append_egress_decided(&store, "sess-e1", "evil.example", 443, "DENY", "p-1");

        let on_run = store.read_outbox_for_run("run-e1", 0).expect("run events");
        assert_eq!(on_run.len(), 1, "{on_run:?}");
        assert_eq!(on_run[0].event_type, TYPE_EGRESS_DECIDED);
        let (subject, body) = decode_egress(&on_run[0].payload);
        assert!(subject.starts_with("run-e1/egress/"), "{subject}");
        assert_eq!(body.domain, "evil.example");
        assert_eq!(body.port, 443);
        assert_eq!(body.disposition, "DENY");

        let on_session = store
            .read_outbox_for_run("sess-e1", 0)
            .expect("session events");
        assert_eq!(on_session.len(), 1, "{on_session:?}");
        assert_eq!(on_session[0].event_type, TYPE_EGRESS_DECIDED);
        let (subject, body) = decode_egress(&on_session[0].payload);
        assert!(subject.starts_with("sess-e1/egress/"), "{subject}");
        assert_eq!(body.disposition, "DENY");
    }

    #[test]
    fn egress_decided_without_a_run_stays_on_the_session() {
        let store = crate::workflow::SqliteWorkflowStore::in_memory().expect("store");
        append_egress_decided(&store, "sess-e2", "ok.example", 443, "ALLOW", "p-1");
        let on_session = store.read_outbox_for_run("sess-e2", 0).expect("events");
        assert_eq!(on_session.len(), 1);
        let all = store.read_outbox(0).expect("all events");
        assert_eq!(all.len(), 1, "no run copy without a run: {all:?}");
    }

    fn artifact() -> ArtifactRef {
        ArtifactRef {
            artifact_id: Some(ResourceId {
                urn: "urn:vam:artifact:local.vam.dev/acme/checkpoint-1".to_string(),
            }),
            digest: Some(Digest {
                algorithm: 1,
                hex_hash: "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                    .to_string(),
            }),
            media_type: "application/octet-stream".to_string(),
            size_bytes: 0,
            uri: "memory://checkpoint".to_string(),
        }
    }

    #[test]
    fn hitl_paused_cloud_event_has_no_raw_args() {
        let json = hitl_paused_json(
            "urn:vam:run:local.vam.dev/acme/run-1",
            1,
            "urn:vam:hitl:local.vam.dev/acme/req-1",
            "jira_create_issue",
            "Policy requires human approval",
            "urn:vam:attempt:local.vam.dev/acme/attempt-1",
            &artifact(),
            "abc123",
            1,
        )
        .expect("event json");
        assert!(json.contains("vam.v1.hitl.paused"));
        assert!(json.contains("requestedArgsDigest"));
        assert!(!json.contains("\"arguments\""));
    }

    #[test]
    fn hitl_decided_round_trips_type() {
        let json = hitl_decided_json(
            "urn:vam:run:local.vam.dev/acme/run-1",
            2,
            "urn:vam:hitl:local.vam.dev/acme/req-1",
            "APPROVED",
            "User::reviewer",
            "abc123",
            2,
        )
        .expect("event json");
        assert!(json.contains("vam.v1.hitl.decided"));
        assert!(json.contains("reviewerPrincipal"));
    }

    #[test]
    fn inference_completed_reports_cache_tokens_when_present() {
        let json = inference_completed_json(
            "urn:vam:run:local.vam.dev/acme/run-cache",
            1,
            "claude-sonnet-5-5",
            "anthropic",
            130,
            42,
            100,
            None,
            None,
            None,
            None,
            None,
            vam_inference::CacheTokens {
                read: 100,
                written: 20,
                unknown: false,
            },
            &vam_inference::CompletionExtras::default(),
        )
        .expect("event json");
        assert!(json.contains("\"cachedTokens\":\"100\"") || json.contains("\"cachedTokens\":100"));
        assert!(json.contains("cacheCreationTokens"));
    }

    #[test]
    fn inference_completed_includes_outcome_not_prompt() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-inf";
        let json = inference_completed_json(
            run_id,
            1,
            "mock-model",
            "mock",
            10,
            5,
            100,
            Some(42),
            Some("assistant outcome only"),
            Some(r#"[{"name":"glob","arguments":"{}"}]"#),
            Some("abc123"),
            None,
            vam_inference::CacheTokens::default(),
            &vam_inference::CompletionExtras::default(),
        )
        .expect("event json");
        assert!(json.contains("vam.v1.inference.completed"));
        assert!(!json.contains("cachedTokens"));
        assert!(json.contains("outcomePreview"));
        assert!(json.contains("assistant outcome only"));
        assert!(json.contains(&format!("\"urn\":\"{run_id}\"")));
        assert!(!json.contains("system"));
        assert!(!json.contains("System Prompt"));
        // A reply with no reasoning and no options adds nothing to the event.
        assert!(!json.contains("reasoningBytes") && !json.contains("adapterNotes"));
        assert!(!json.contains("reasoning"));
        assert!(!json.contains("resamples"));
    }

    #[test]
    fn inference_completed_reports_one_resample_as_a_string() {
        let extras = vam_inference::CompletionExtras {
            resamples: Some(1),
            ..Default::default()
        };
        let json = inference_completed_json(
            "urn:vam:run:local.vam.dev/acme/run-rs",
            1,
            "mock-model",
            "mock",
            10,
            5,
            100,
            None,
            None,
            None,
            None,
            None,
            vam_inference::CacheTokens::default(),
            &extras,
        )
        .expect("event json");
        // int64 is a JSON string; scenarios assert "1".
        assert!(json.contains("\"resamples\":\"1\""), "{json}");
    }

    #[test]
    fn inference_completed_reports_reasoning_size_mode_and_notes() {
        let extras = vam_inference::CompletionExtras {
            reasoning_bytes: Some(19),
            reasoning_tool_markup: Some(1),
            reasoning_mode: Some("strip".into()),
            adapter_notes: vec!["tool_choice_unsupported".into()],
            resamples: None,
        };
        let json = inference_completed_json(
            "urn:vam:run:local.vam.dev/acme/run-r",
            1,
            "glm-5",
            "vercel",
            10,
            5,
            100,
            None,
            None,
            None,
            None,
            None,
            vam_inference::CacheTokens::default(),
            &extras,
        )
        .expect("event json");
        assert!(
            json.contains("\"reasoningBytes\":\"19\"") || json.contains("\"reasoningBytes\":19")
        );
        assert!(json.contains("reasoningToolMarkup"));
        assert!(json.contains("\"reasoningMode\":\"strip\""));
        assert!(json.contains("tool_choice_unsupported"));
    }

    #[test]
    fn mock_payloads_have_fixed_key_sets_and_bounded_text() {
        let keys = |v: &serde_json::Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        let matched = mock_rule_matched_payload("s c", 2, Some("start"));
        assert_eq!(keys(&matched), ["rule", "script", "step"]);
        assert_eq!(matched["script"], "s_c");
        assert_eq!(matched["rule"], 2);
        assert_eq!(
            keys(&mock_rule_matched_payload("s", 0, None)),
            ["rule", "script"]
        );
        let long = "x".repeat(500);
        let unmatched = mock_unmatched_payload("s", "weird", "user", &long, None);
        assert_eq!(
            keys(&unmatched),
            ["last_role", "preview", "reason", "script"]
        );
        assert_eq!(unmatched["reason"], "no_rule");
        assert_eq!(unmatched["preview"].as_str().unwrap().len(), 80);
    }

    #[test]
    fn web_used_payload_has_a_fixed_key_set() {
        let payload = web_used_payload("vam.web", "web_fetch", "example.com", "ok", None, 10, 1);
        let mut keys: Vec<&str> = payload
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            ["broker", "bytes", "host", "sources_added", "status", "tool"]
        );
        let failed = web_used_payload(
            "vam.web",
            "web_fetch",
            "e x.com",
            "weird status with query=secret",
            Some("blocked"),
            0,
            0,
        );
        assert_eq!(failed["status"], "error");
        assert_eq!(failed["code"], "blocked");
        assert_eq!(failed["host"], "e_x.com");
        let bad_code = web_used_payload("b", "t", "h", "error", Some("Blocked: do x"), 0, 0);
        assert!(bad_code.get("code").is_none());
    }
}
