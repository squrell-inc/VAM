//! Host side of the outbox (AR-3.9): `post_message` and `show`.
//!
//! The guest posts through the port-52 method `PostOutbox`. Everything it sends
//! is data from an untrusted guest, so the host checks it again, in this order:
//!
//! 1. the lease is bound to the session's run;
//! 2. the capability token is the one the intent for exactly this call minted
//!    (single use, signed, bound to the lease, the tool and the digest of the
//!    arguments rebuilt here from the post);
//! 3. for a card, the run's harness has `human.jig` on;
//! 4. the size caps (text 4 KiB, card data 16 KiB);
//! 5. the jig is a built-in Show jig;
//! 6. the data passes `vam_spec::jig::check_data`;
//! 7. the run is under its rate (60 accepted posts a minute).
//!
//! `ask_user` (AR-3.10) does not post: [`plan_ask_user`] checks the call the
//! intent carries (jig, `human.jig` for any Ask jig but `vam.question`, size,
//! `check_data`) and the PEP gate parks it as a question approval.
//!
//! A failure answers with a typed tool result and records nothing. Success emits
//! `outbox.message {text}` or `outbox.card {jig, data, card_id?}` on the run.
//! Strings are data: the app shows them as text. The text is the model's own
//! words, already in the thread log, so it gets no extra redaction here.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use vam_spec::outbox::{
    card_args, message_args, MAX_CARD_DATA_BYTES, MAX_POSTS_PER_MINUTE, MAX_TEXT_BYTES,
    POST_MESSAGE_TOOL, SHOW_JIGS, SHOW_TOOL,
};
use vam_spec::v1::{ErrorStatus, OutboxKind, PostOutboxRequest, PostOutboxResponse};

use crate::events::{TYPE_CARD_SKIPPED, TYPE_OUTBOX_CARD, TYPE_OUTBOX_MESSAGE};
use crate::run_checkpoint::bind_checkpoint_lease;
use crate::security::SessionSecurityContext;
use crate::session::ToolCatalogEntry;
use crate::workflow::SqliteWorkflowStore;

const INVALID_ARGUMENT: i32 = 3;
const PERMISSION_DENIED: i32 = 7;
const RESOURCE_EXHAUSTED: i32 = 8;
const INTERNAL: i32 = 13;

const RATE_WINDOW: Duration = Duration::from_secs(60);

/// The catalog entries of the outbox tools. They are builtins of the guest (so the
/// permissions target is `builtin:<tool>`) that never go through `ExecuteHostTool`.
/// Merged with the other builtins, so a skill tool of the same name fails closed.
pub fn catalog_entries() -> Vec<ToolCatalogEntry> {
    vam_spec::outbox::OUTBOX_TOOLS
        .iter()
        .map(|name| ToolCatalogEntry {
            tool_name: (*name).to_string(),
            command: "builtin".to_string(),
            entrypoint: (*name).to_string(),
            digest: vam_spec::outbox::tool_digest(name),
            side_effect: vam_spec::v1::SideEffectClass::ReadOnly as i32,
            skill_name: String::new(),
            approval_required: false,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Rate: accepted posts per run in a sliding minute, kept in memory. A vamd
// restart starts every window empty.

static WINDOWS: LazyLock<Mutex<HashMap<String, VecDeque<Instant>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Most runs the limiter tracks before it sweeps the idle ones.
const SWEEP_AT: usize = 1024;

/// Record one accepted post for `run_id` at `now`, or say the minute is spent.
fn admit_at(run_id: &str, now: Instant) -> bool {
    let Ok(mut windows) = WINDOWS.lock() else {
        return false;
    };
    if windows.len() >= SWEEP_AT {
        windows.retain(|_, w| {
            w.back()
                .is_some_and(|t| now.duration_since(*t) < RATE_WINDOW)
        });
    }
    let window = windows.entry(run_id.to_string()).or_default();
    while window
        .front()
        .is_some_and(|t| now.duration_since(*t) >= RATE_WINDOW)
    {
        window.pop_front();
    }
    if window.len() >= MAX_POSTS_PER_MINUTE {
        return false;
    }
    window.push_back(now);
    true
}

// ---------------------------------------------------------------------------

fn reply(code: i32, message: impl Into<String>) -> PostOutboxResponse {
    PostOutboxResponse {
        error: Some(ErrorStatus {
            code,
            message: message.into(),
            details: vec![],
            retryable: false,
        }),
    }
}

/// Check an `ask_user` call before any approval exists. Returns the jig and the
/// data, or the message the model sees. Nothing is written here.
pub(crate) fn plan_ask_user(
    store: &SqliteWorkflowStore,
    run_id: &str,
    args: &prost_types::Struct,
) -> Result<(String, serde_json::Value), String> {
    use vam_spec::outbox::{ASK_JIGS, ASK_JIG_ALWAYS};
    let args = crate::playbook_activation::struct_to_json(args);
    let jig = args.get("jig").and_then(|v| v.as_str()).unwrap_or("");
    if !ASK_JIGS.contains(&jig) {
        return Err(format!(
            "ask_user cannot ask with '{jig}'. Cards you can ask with: {}.",
            ASK_JIG_ALWAYS
        ));
    }
    if jig != ASK_JIG_ALWAYS && !run_has_jig(store, run_id) {
        return Err(format!(
            "ask_user cannot ask with '{jig}' here (the harness switch human.jig is off). \
             Use {ASK_JIG_ALWAYS}."
        ));
    }
    let Some(data) = args.get("data").filter(|d| d.is_object()).cloned() else {
        return Err("ask_user: data must be an object".to_string());
    };
    let size = data.to_string().len();
    if size > MAX_CARD_DATA_BYTES {
        return Err(format!(
            "ask_user data is {size} bytes; the limit is {MAX_CARD_DATA_BYTES}. Send less."
        ));
    }
    if let Err(errors) = vam_spec::jig::check_data(jig, &data) {
        return Err(
            invalid_data_message(jig, &errors).replace("call show again", "call ask_user again")
        );
    }
    Ok((jig.to_string(), data))
}

/// The run's `human.jig` switch from its stored spec. Missing or unreadable is off.
fn run_has_jig(store: &SqliteWorkflowStore, run_id: &str) -> bool {
    store
        .get_run_spec_context(run_id)
        .ok()
        .and_then(|(spec_json, _, _)| spec_json)
        .and_then(|json| {
            vam_spec::encoding::json_to_proto::<vam_spec::v1::RunSpec>("vam.v1.RunSpec", &json).ok()
        })
        .and_then(|spec| vam_spec::harness::Harness::from_inputs(spec.inputs.as_ref()).ok())
        .is_some_and(|h| h.human.jig)
}

/// The signed intent for this exact call, consumed.
fn verify_intent(
    ctx: &SessionSecurityContext,
    store: &SqliteWorkflowStore,
    request: &PostOutboxRequest,
    tool: &str,
    args: &prost_types::Struct,
) -> Result<(), String> {
    const REFUSED: &str = "PostOutbox needs an authorized intent for exactly this call";
    let Some(token) = request.capability_token.as_ref() else {
        return Err(format!("{REFUSED}: no capability token"));
    };
    let entry = match store.get_tool(&ctx.session_id, tool) {
        Ok(Some(entry)) if entry.command == "builtin" => entry,
        _ => return Err(format!("{tool} is not an outbox tool of this session")),
    };
    let digest = vam_spec::canonical_args_digest(Some(args))
        .map_err(|e| format!("the arguments cannot be canonicalized: {e}"))?;
    let lease = request.lease.as_ref();
    let lease_id = lease
        .and_then(|l| l.lease_id.as_ref())
        .map(|id| id.urn.clone())
        .unwrap_or_default();
    let generation = lease.map(|l| l.lifecycle_generation as u64).unwrap_or(1);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    vam_security::verify_capability_token(
        ctx.approval_signer.key_id(),
        &ctx.approval_signer.verifying_key(),
        token,
        &ctx.session_id,
        &lease_id,
        generation,
        tool,
        &digest,
        &entry.digest,
        None,
        now,
        ctx.replay_store.as_ref(),
    )
    .map(|_| ())
    .map_err(|e| format!("{REFUSED}: {e}"))
}

/// The first few problems of a data check, as the model reads them.
fn invalid_data_message(jig: &str, errors: &[vam_spec::jig::JigError]) -> String {
    let shown: Vec<String> = errors.iter().take(3).map(|e| e.to_string()).collect();
    let more = if errors.len() > 3 {
        format!(" (and {} more)", errors.len() - 3)
    } else {
        String::new()
    };
    format!(
        "{}{jig}: {}{more}. Fix the data and call show again.",
        vam_spec::outbox::INVALID_DATA_PREFIX,
        shown.join("; ")
    )
}

/// Answer one `PostOutbox`. See the module header for the order of the checks.
pub fn handle_post_outbox(
    ctx: &SessionSecurityContext,
    store: &Arc<SqliteWorkflowStore>,
    session_id: &str,
    request: &PostOutboxRequest,
) -> PostOutboxResponse {
    post_outbox_at(ctx, store, session_id, request, Instant::now())
}

fn post_outbox_at(
    ctx: &SessionSecurityContext,
    store: &Arc<SqliteWorkflowStore>,
    session_id: &str,
    request: &PostOutboxRequest,
    now: Instant,
) -> PostOutboxResponse {
    // 1. The lease belongs to this session's run.
    let bound = match bind_checkpoint_lease(store, session_id, request.lease.as_ref()) {
        Ok(bound) => bound,
        Err((code, message)) => return reply(code, message),
    };
    let kind = OutboxKind::try_from(request.kind).unwrap_or(OutboxKind::Unspecified);
    let tool = match kind {
        OutboxKind::Message => POST_MESSAGE_TOOL,
        OutboxKind::Card => SHOW_TOOL,
        OutboxKind::Unspecified => {
            return reply(INVALID_ARGUMENT, "PostOutbox kind must be message or card")
        }
    };

    // No token, nothing is parsed or read.
    if request.capability_token.is_none() {
        return reply(
            PERMISSION_DENIED,
            "PostOutbox needs an authorized intent for exactly this call: no capability token",
        );
    }
    // Rebuild what the intent signed. Card data that is not a JSON object cannot be
    // a card. A frame is at most 64 KiB, so parsing it first is cheap.
    let mut data: Option<serde_json::Value> = None;
    let args = if kind == OutboxKind::Message {
        message_args(&request.text)
    } else {
        match serde_json::from_str::<serde_json::Value>(&request.data_json) {
            Ok(value) if value.is_object() => {
                data = Some(value);
            }
            _ => {
                return reply(
                    INVALID_ARGUMENT,
                    format!(
                        "{}{}: data must be one JSON object",
                        vam_spec::outbox::INVALID_DATA_PREFIX,
                        request.jig
                    ),
                )
            }
        }
        match data.as_ref().and_then(|d| card_args(&request.jig, d)) {
            Some(args) => args,
            None => return reply(INVALID_ARGUMENT, "data must be one JSON object"),
        }
    };

    // 2. The intent for exactly this call.
    if let Err(message) = verify_intent(ctx, store, request, tool, &args) {
        return reply(PERMISSION_DENIED, message);
    }

    // 3. `show` only on a run that turned `human.jig` on.
    if kind == OutboxKind::Card && !run_has_jig(store, &bound.run_id) {
        return reply(
            PERMISSION_DENIED,
            "show is not enabled for this run (the harness switch human.jig is off)",
        );
    }

    // 4. Caps.
    if kind == OutboxKind::Message {
        if request.text.trim().is_empty() {
            return reply(INVALID_ARGUMENT, "post_message text is empty");
        }
        if request.text.len() > MAX_TEXT_BYTES {
            return reply(
                INVALID_ARGUMENT,
                format!(
                    "post_message text is {} bytes; the limit is {MAX_TEXT_BYTES}. Shorten it.",
                    request.text.len()
                ),
            );
        }
    } else if request.data_json.len() > MAX_CARD_DATA_BYTES {
        return reply(
            INVALID_ARGUMENT,
            format!(
                "show data is {} bytes; the limit is {MAX_CARD_DATA_BYTES}. Send less.",
                request.data_json.len()
            ),
        );
    }

    // 5 and 6. A built-in Show jig, and data that passes its schema.
    if kind == OutboxKind::Card {
        if !SHOW_JIGS.contains(&request.jig.as_str()) {
            return reply(
                INVALID_ARGUMENT,
                format!(
                    "show cannot display '{}'. Cards you can show: {}.",
                    request.jig,
                    SHOW_JIGS.join(", ")
                ),
            );
        }
        let value = data.as_ref().cloned().unwrap_or(serde_json::Value::Null);
        if let Err(errors) = vam_spec::jig::check_data(&request.jig, &value) {
            return reply(
                INVALID_ARGUMENT,
                invalid_data_message(&request.jig, &errors),
            );
        }
    }

    // 7. The rate, spent only by a post that is about to be recorded.
    if !admit_at(&bound.run_id, now) {
        return reply(
            RESOURCE_EXHAUSTED,
            format!(
                "{}: at most {MAX_POSTS_PER_MINUTE} posts a minute. Wait a moment, then post again.",
                vam_spec::outbox::RATE_LIMITED_PREFIX
            ),
        );
    }

    let (event_type, payload) = match kind {
        OutboxKind::Message => (
            TYPE_OUTBOX_MESSAGE,
            serde_json::json!({ "text": request.text }),
        ),
        _ => {
            let data = data.unwrap_or(serde_json::Value::Null);
            let mut payload = serde_json::json!({ "jig": request.jig, "data": data });
            if let Some(card_id) = payload["data"].get("card_id").cloned() {
                payload["card_id"] = card_id;
            }
            (TYPE_OUTBOX_CARD, payload)
        }
    };
    // CloudEvents envelope with the payload as `data`, like the typed events.
    match crate::events::outbox_event_json(event_type, &bound.run_id, payload) {
        Ok(event) => {
            if let Err(err) = store.append_outbox_event(&bound.run_id, event_type, &event) {
                tracing::warn!("outbox event for {} was not recorded: {err}", bound.run_id);
                return reply(INTERNAL, "the host could not record the post");
            }
        }
        Err(err) => {
            return reply(
                INTERNAL,
                format!("the host could not build the event: {err}"),
            )
        }
    }
    PostOutboxResponse { error: None }
}

// ---------------------------------------------------------------------------
// Tool cards (CN-3.4): the host attaches the skill's `show` card to a successful result.

static CARD_CALLS: LazyLock<Mutex<std::collections::HashSet<(String, String)>>> =
    LazyLock::new(|| Mutex::new(std::collections::HashSet::new()));
const MAX_CARD_CALLS: usize = 4096;

fn first_sight_of_call(run_id: &str, call_id: &str) -> bool {
    if call_id.is_empty() {
        return true;
    }
    let Ok(mut seen) = CARD_CALLS.lock() else {
        return true;
    };
    if seen.len() >= MAX_CARD_CALLS {
        seen.clear();
    }
    seen.insert((run_id.to_string(), call_id.to_string()))
}

fn append_card_event(
    store: &SqliteWorkflowStore,
    run_id: &str,
    event_type: &str,
    payload: serde_json::Value,
) {
    match crate::events::outbox_event_json(event_type, run_id, payload) {
        Ok(event) => {
            if let Err(err) = store.append_outbox_event(run_id, event_type, &event) {
                tracing::warn!("card event for {run_id} was not recorded: {err}");
            }
        }
        Err(err) => tracing::warn!("card event for {run_id} was not built: {err}"),
    }
}

/// After a successful call of a tool that declares `ui.show`: emit `outbox.card` with the
/// skill's checked IR and the result as data, or `card.skipped {tool, callId, reason}`. The tool
/// result is never changed. A tool without `ui.show` emits nothing. Not gated on `human.jig`.
pub fn attach_show_card(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    tool: &str,
    call_id: &str,
    result_text: &str,
) {
    attach_show_card_at(
        store,
        session_id,
        run_id,
        tool,
        call_id,
        result_text,
        Instant::now(),
    )
}

fn attach_show_card_at(
    store: &SqliteWorkflowStore,
    session_id: &str,
    run_id: &str,
    tool: &str,
    call_id: &str,
    result_text: &str,
    now: Instant,
) {
    let Some(cards) = crate::skill_cards::cards_for(session_id, tool) else {
        return;
    };
    if !cards.declares_show || run_id.is_empty() || !first_sight_of_call(run_id, call_id) {
        return;
    }
    let skip = |reason: &str| {
        append_card_event(
            store,
            run_id,
            TYPE_CARD_SKIPPED,
            serde_json::json!({ "tool": tool, "callId": call_id, "reason": reason }),
        );
    };
    let Some(card) = cards.show else {
        return skip("card_unavailable");
    };
    if result_text.len() > MAX_CARD_DATA_BYTES {
        return skip("result_too_large");
    }
    let data = match serde_json::from_str::<serde_json::Value>(result_text) {
        Ok(v) if v.is_object() => v,
        _ => return skip("result_not_an_object"),
    };
    if let Err(errors) = vam_spec::jig::check_data_against(&card.schema, &data) {
        tracing::debug!(
            "tool card data for {tool}: {}",
            invalid_data_message(&card.jig, &errors)
        );
        return skip("result_does_not_fit");
    }
    if !admit_at(run_id, now) {
        return skip("rate_limited");
    }
    let mut payload = serde_json::json!({
        "jig": card.jig, "ir": card.ir, "data": data,
        "tool": tool, "callId": call_id, "skill": card.skill, "source": "tool",
    });
    if let Some(card_id) = payload["data"].get("card_id").cloned() {
        payload["card_id"] = card_id;
    }
    append_card_event(store, run_id, TYPE_OUTBOX_CARD, payload);
}

#[cfg(test)]
mod tests;
