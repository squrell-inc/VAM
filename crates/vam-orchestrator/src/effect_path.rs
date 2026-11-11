//! The effect journal at the host tool path (E10 Phase 4, AR-4.4): decide, persist, act.
//!
//! **Scope.** Every host-mediated outside call is journaled: host adapters, HSP broker tools and
//! MCP tools (catalog `command == "host"`, adapter ids `hsp:` and `mcp:` included). Out of scope,
//! on purpose: conversation tools (`post_message`, `show`, `ask_user`; `start_playbook` and the
//! other internal playbook adapters, which are the host's own bookkeeping) and in-VM tools (they
//! run inside the guest and roll back with the files). A session whose lease carries no run id has
//! no run to record against and is not journaled.
//!
//! **Order.** The intent path writes `intended` before the capability token is minted
//! ([`record_intent`]). The execute path writes `sent` immediately before each send point
//! ([`EffectCall::mark_sent`]) and settles the row after the answer. **A failed `intended` or
//! `sent` write blocks the call** (owner decision R1): nothing leaves the host and the guest gets
//! the typed [`UNAVAILABLE_MESSAGE`].
//!
//! **Connection writes (CN-4.7).** A code tool that writes through a connector binding (a
//! declared shape with a method other than GET or HEAD, on a tool that is not read-only; see
//! [`is_connection_write`]) is journaled too, though it runs in the guest. The intent path
//! journals it like a host call ([`record_intent`], at the intent). The write itself is
//! journaled where the host attaches the member's token ([`reserve_write`], [`WriteCall`]):
//! the ticket is reserved before the token is read, `sent` is written immediately before the
//! request leaves the host (a failed write sends nothing), and the far end's status settles the
//! row (2xx `confirmed`, 4xx `failed`, 5xx or a lost answer `unknown`). The token and the
//! request and response bodies never enter the journal: a confirmed row keeps only a one-line
//! summary the model is told on a replay.

use std::sync::atomic::{AtomicU8, Ordering};
use std::time::Duration;

use prost::Message;
use sha2::{Digest, Sha256};
use vam_spec::v1::{EffectClass as ProtoClass, EffectSpec, ErrorStatus, ExecuteHostToolResponse};

use crate::effect_class::{effective_class, EffectClass};
use crate::effect_key::{derive, EffectKeySecret};
use crate::effect_store::{EffectRow, EffectState, NewEffect};
use crate::workflow::SqliteWorkflowStore;

/// What the guest is told when the journal cannot be written. Same text at intent and execute.
pub const UNAVAILABLE_MESSAGE: &str = "effect_journal_unavailable: not sent: the host could not \
     record this call; nothing left the host";

/// The leading marker of [`UNAVAILABLE_MESSAGE`] (the guest renders it as a tool error, not as a
/// policy denial).
pub const UNAVAILABLE_MARKER: &str = "effect_journal_unavailable";

/// Longest a stop waits for host calls still in flight.
pub const INFLIGHT_WAIT_CAP: Duration = Duration::from_secs(60);

/// The typed error status of [`UNAVAILABLE_MESSAGE`] (`UNAVAILABLE`).
pub fn unavailable_status() -> ErrorStatus {
    ErrorStatus {
        code: 14,
        message: UNAVAILABLE_MESSAGE.to_string(),
        details: vec![],
        retryable: true,
    }
}

/// Host adapters that are the host's own bookkeeping, not outside calls.
pub fn is_internal_adapter(name: &str) -> bool {
    matches!(
        name,
        "step_complete"
            | "run_step"
            | "check_playbook_finish"
            | "start_playbook"
            | "notifications_list"
    )
}

/// Which adapter family a catalog entry's adapter id belongs to.
pub fn adapter_kind(adapter_id: &str) -> &'static str {
    if vam_spec::validation::parse_hosted_adapter_id(adapter_id).is_some() {
        "hsp"
    } else if vam_spec::validation::parse_mcp_adapter_id(adapter_id).is_some() {
        "mcp"
    } else {
        "host"
    }
}

/// True when a catalog entry is a host-mediated outside call the journal covers.
pub fn is_journaled(command: &str, adapter_id: &str, tool_name: &str) -> bool {
    command == "host" && !is_internal_adapter(adapter_id) && !is_internal_adapter(tool_name)
}

fn parse_class(raw: &str) -> Option<ProtoClass> {
    match raw.to_ascii_lowercase().replace('-', "_").as_str() {
        "read" | "effect_class_read" => Some(ProtoClass::Read),
        "idempotent" | "effect_class_idempotent" => Some(ProtoClass::Idempotent),
        "non_idempotent" | "nonidempotent" | "effect_class_non_idempotent" => {
            Some(ProtoClass::NonIdempotent)
        }
        _ => None,
    }
}

/// The class a broker manifest or an MCP connection's owner declared for a tool. Skill-manifest
/// declarations are not reachable here (the intent path has no skills directory), so skills use
/// the class inferred from their side-effect class, which is the safe default.
pub fn declared_effect(
    store: &SqliteWorkflowStore,
    session_id: &str,
    adapter_id: &str,
) -> Option<EffectSpec> {
    if let Some((broker_id, export)) = vam_spec::validation::parse_hosted_adapter_id(adapter_id) {
        let approval = store.get_hsp_broker_approval(&broker_id).ok()??;
        return approval
            .manifest
            .tools
            .iter()
            .find(|tool| tool.tool_name == export)
            .and_then(|tool| tool.effect.clone());
    }
    if let Some((connection_id, export)) = vam_spec::validation::parse_mcp_adapter_id(adapter_id) {
        let owner = store.session_owner_user_id(session_id).ok()?;
        let conn =
            crate::mcp_registry::visible_mcp_connection(store, &owner, &connection_id).ok()?;
        // The owner's declaration: the class and, optionally, a verify read.
        let mut declared = conn.effects.get(&export)?.clone();
        // The short class names ("read", "non_idempotent") are accepted beside the enum names.
        if let Some(class) = declared
            .get("class")
            .and_then(|c| c.as_str())
            .and_then(parse_class)
        {
            declared["class"] = serde_json::json!(class as i32);
        }
        let spec: EffectSpec =
            vam_spec::encoding::json_to_proto("vam.v1.EffectSpec", &declared.to_string()).ok()?;
        if spec.class == ProtoClass::Unspecified as i32 {
            return None;
        }
        return Some(spec);
    }
    None
}

/// The class a skill manifest declared for a code tool, as the session booted with it.
fn code_tool_effect(
    store: &SqliteWorkflowStore,
    session_id: &str,
    tool: &str,
) -> Option<EffectSpec> {
    let class = store.tool_egress(session_id, tool).ok()??.effect_class?;
    Some(EffectSpec {
        class: parse_class(&class)? as i32,
        verify: None,
    })
}

/// The class of a call: declared by the broker, the connection owner or (for a code tool) its
/// skill manifest, else inferred.
pub fn class_for(
    store: &SqliteWorkflowStore,
    session_id: &str,
    tool: &str,
    adapter_id: &str,
    side_effect: i32,
) -> EffectClass {
    let declared = declared_effect(store, session_id, adapter_id)
        .or_else(|| code_tool_effect(store, session_id, tool));
    effective_class(declared.as_ref(), side_effect)
}

/// True when the tool is a code tool that writes through a connector binding: it declares a
/// connection shape with a method other than GET or HEAD, and it is not read-only (a read that
/// happens to use POST, such as a free/busy lookup, is not an effect and is not journaled).
pub fn is_connection_write(
    store: &SqliteWorkflowStore,
    session_id: &str,
    tool: &str,
    side_effect: i32,
) -> bool {
    if side_effect == vam_spec::v1::SideEffectClass::ReadOnly as i32 {
        return false;
    }
    store
        .tool_egress(session_id, tool)
        .ok()
        .flatten()
        .is_some_and(|row| {
            row.requires_connector.is_some()
                && row.shapes.iter().any(|shape| {
                    shape
                        .methods
                        .iter()
                        .any(|m| !vam_network::is_read_method(m))
                })
        })
}

/// `<call id>/<step id>` rows name their parent.
fn parent_of(call_id: &str) -> Option<String> {
    call_id
        .rsplit_once('/')
        .map(|(parent, _)| parent.to_string())
}

/// What the intent path knows about a call it is deciding.
pub struct IntentFacts<'a> {
    pub store: &'a SqliteWorkflowStore,
    pub secret: Option<EffectKeySecret>,
    pub session_id: &'a str,
    pub run_id: &'a str,
    pub tool: &'a str,
    pub adapter_id: &'a str,
    pub side_effect: i32,
    /// The model's call id; empty from an old guest.
    pub call_id: &'a str,
    pub args_digest: &'a str,
    /// The approval a suspended call waits on.
    pub approval_id: Option<&'a str>,
}

/// Writes (or re-uses) the `intended` row of a call and returns the call id the token must carry.
/// `Ok(None)` when the run has no id (nothing to record against). An `Err` means the row could
/// not be written: the caller must not mint a token (R1).
pub fn record_intent(facts: &IntentFacts<'_>) -> Result<Option<String>, String> {
    if facts.run_id.is_empty() {
        return Ok(None);
    }
    let store = facts.store;
    let class = class_for(
        store,
        facts.session_id,
        facts.tool,
        facts.adapter_id,
        facts.side_effect,
    );
    let call_id = if !facts.call_id.is_empty() {
        facts.call_id.to_string()
    } else if let Some(row) = store
        .effect_find_intended(facts.run_id, facts.tool, Some(facts.args_digest))
        .map_err(|e| e.to_string())?
    {
        // An old guest re-intents the call an approval allowed without a call id.
        row.call_id
    } else {
        let digest8: String = facts.args_digest.chars().take(8).collect();
        let prefix = format!("legacy-{}-{}-", facts.tool, digest8);
        let n = store
            .effect_count_with_prefix(facts.run_id, &prefix)
            .map_err(|e| e.to_string())?
            + 1;
        let id = format!("{prefix}{n}");
        tracing::warn!(
            run_id = facts.run_id,
            tool = facts.tool,
            call_id = %id,
            "guest sent no call id: journaling under a host-derived id that is not stable \
             across a resume"
        );
        id
    };
    let secret = facts
        .secret
        .as_ref()
        .ok_or_else(|| "effect key secret is not installed".to_string())?;
    let key =
        derive(secret, facts.run_id, &call_id, facts.args_digest).map_err(|e| e.to_string())?;
    match store
        .effect_get(facts.run_id, &call_id)
        .map_err(|e| e.to_string())?
    {
        None => {
            store
                .effect_begin(&NewEffect {
                    run_id: facts.run_id.to_string(),
                    call_id: call_id.clone(),
                    parent_call_id: parent_of(&call_id),
                    tool: facts.tool.to_string(),
                    adapter: match adapter_kind(facts.adapter_id) {
                        // A code tool that writes through a connection (CN-4.7).
                        "host"
                            if is_connection_write(
                                store,
                                facts.session_id,
                                facts.tool,
                                facts.side_effect,
                            ) =>
                        {
                            "code"
                        }
                        kind => kind,
                    }
                    .to_string(),
                    class,
                    idempotency_key: key,
                    args_digest: facts.args_digest.to_string(),
                    approval_id: facts.approval_id.map(str::to_string),
                })
                .map_err(|e| e.to_string())?;
        }
        Some(row) => {
            if row.tool != facts.tool {
                return Err(format!(
                    "call id '{call_id}' was already used for tool '{}'",
                    row.tool
                ));
            }
            if row.state == EffectState::Intended {
                if row.args_digest != facts.args_digest {
                    store
                        .effect_rebind_args(facts.run_id, &call_id, facts.args_digest, &key)
                        .map_err(|e| e.to_string())?;
                }
                // A call that parked on a reconnect and then on the tool's own ask moves to
                // the new approval, so a reject or stop of that one settles the row.
                if let Some(approval) = facts
                    .approval_id
                    .filter(|approval| row.approval_id.as_deref() != Some(*approval))
                {
                    store
                        .effect_set_approval(facts.run_id, &call_id, approval)
                        .map_err(|e| e.to_string())?;
                }
            }
            // A row past `intended` is never re-sent: the execute path answers from it.
        }
    }
    Ok(Some(call_id))
}

// ---------------------------------------------------------------------------------------------
// In flight

fn inflight_enter(store: &SqliteWorkflowStore, run_id: &str) {
    if let Ok(mut map) = store.effect_inflight.lock() {
        *map.entry(run_id.to_string()).or_insert(0) += 1;
    }
}

fn inflight_leave(store: &SqliteWorkflowStore, run_id: &str) {
    if let Ok(mut map) = store.effect_inflight.lock() {
        if let Some(n) = map.get_mut(run_id) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                map.remove(run_id);
            }
        }
    }
}

/// Host calls of a run that are still in flight against this store's journal. The count lives
/// on the store, not in a process global: two journals (two stores) never see each other's calls.
pub fn inflight_count(store: &SqliteWorkflowStore, run_id: &str) -> usize {
    store
        .effect_inflight
        .lock()
        .ok()
        .and_then(|m| m.get(run_id).copied())
        .unwrap_or(0)
}

/// Waits (up to `cap`) until no host call of the run is in flight. A stop lets a call that left
/// the host finish and be journaled before the conversation records the end (V.3).
pub async fn wait_idle(store: &SqliteWorkflowStore, run_id: &str, cap: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + cap;
    while inflight_count(store, run_id) > 0 {
        if tokio::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    true
}

// ---------------------------------------------------------------------------------------------
// Execute side

const OPEN: u8 = 0;
const SENT: u8 = 1;
const SETTLED: u8 = 2;

/// One journaled call on the execute path. Created from the `intended` row, it writes `sent`
/// before the send and settles the row. Dropped while `sent` (the future was cancelled), it
/// leaves the row `unknown`.
pub struct EffectCall<'a> {
    store: &'a SqliteWorkflowStore,
    run_id: String,
    call_id: String,
    tool: String,
    class: EffectClass,
    key: String,
    generation: u64,
    phase: AtomicU8,
}

/// The execute path's view of a call's journal row.
pub enum ExecuteOpen<'a> {
    /// Not journaled (internal adapter or no run).
    Skip,
    /// The row is `intended`: send it.
    Proceed(EffectCall<'a>),
    /// The row says the call already left: answer from it, never send.
    Answer(ExecuteHostToolResponse),
}

/// Why the execute path refuses before anything is sent.
pub enum ExecuteRefusal {
    /// The journal could not be read or written (R1).
    Unavailable,
    /// The token, the call id or the arguments do not match the row.
    Mismatch(String),
}

fn answer_from_row(row: &EffectRow) -> ExecuteHostToolResponse {
    let tool_error = |text: String| ExecuteHostToolResponse {
        text_result: format!("tool_error: {text}"),
        artifact_relpath: String::new(),
        artifact_bytes: Vec::new(),
        extra_artifacts: Vec::new(),
        error: None,
    };
    match row.state {
        EffectState::Confirmed => row
            .result_bytes
            .as_deref()
            .and_then(|bytes| ExecuteHostToolResponse::decode(bytes).ok())
            .unwrap_or_else(|| {
                tool_error(format!(
                    "{} was already done and is not sent again; its result is in the conversation",
                    row.tool
                ))
            }),
        EffectState::Failed => tool_error(format!(
            "{} was already tried and failed ({}); it is not sent again under this call",
            row.tool,
            row.detail.as_deref().unwrap_or("failed")
        )),
        _ => tool_error(if row.reconciled_by.is_some() {
            not_found_text(&row.tool)
        } else {
            unknown_outcome_text(&row.tool)
        }),
    }
}

/// What the model is told about a call whose answer was lost: it was sent, nothing came back.
pub fn unknown_outcome_text(tool: &str) -> String {
    format!(
        "Outcome unknown: {tool} was sent, but the task was interrupted before an answer came \
         back. It may or may not have taken effect. Check with a read before doing it again; \
         calling it again with the same arguments will ask the user first."
    )
}

/// What the model is told when a declared check ran and found no sign of the effect. A check can
/// miss a write that has not shown up yet, so the row stays unknown and an identical call still
/// asks the person.
pub fn not_found_text(tool: &str) -> String {
    format!(
        "A check found no sign that {tool} took effect. Calling it again with the same \
         arguments will ask the user first."
    )
}

/// What the model is told when a declared check found the effect.
pub fn verified_text(result: &str) -> String {
    format!("Done (confirmed by a check after an interruption): {result}")
}

/// The ask shown to the person before an identical call that may already have taken effect.
pub fn ask_again_message(tool: &str) -> String {
    format!(
        "{tool} may already have taken effect: an identical call was interrupted before its \
         answer came back. Send it again?"
    )
}

/// The answer a resumed task gets for a call it already made (E10 Phase 4): the recorded result
/// of a `confirmed` call, unchanged (the conversation reads as if nothing happened), the recorded
/// failure of a `failed` one, and for `sent` or `unknown` the unknown-outcome text (or the
/// no-sign text once a check ran). Never sends.
pub fn replay_answer(row: &EffectRow) -> ExecuteHostToolResponse {
    match row.state {
        EffectState::Sent | EffectState::Unknown => {
            let text = if row.state == EffectState::Unknown && row.reconciled_by.is_some() {
                not_found_text(&row.tool)
            } else {
                unknown_outcome_text(&row.tool)
            };
            ExecuteHostToolResponse {
                text_result: format!("tool_error: {text}"),
                artifact_relpath: String::new(),
                artifact_bytes: Vec::new(),
                extra_artifacts: Vec::new(),
                error: None,
            }
        }
        _ => answer_from_row(row),
    }
}

/// Looks up the row of the call a token authorizes and checks it against the request.
#[allow(clippy::too_many_arguments)]
pub fn open_execute<'a>(
    store: &'a SqliteWorkflowStore,
    secret: Option<&EffectKeySecret>,
    run_id: &str,
    request_call_id: &str,
    claim_call_id: &str,
    tool: &str,
    args_digest: &str,
    generation: u64,
) -> Result<ExecuteOpen<'a>, ExecuteRefusal> {
    if run_id.is_empty() {
        return Ok(ExecuteOpen::Skip);
    }
    if claim_call_id.is_empty() {
        // A journaled tool always gets a call id in its token.
        return Err(ExecuteRefusal::Unavailable);
    }
    if !request_call_id.is_empty() && request_call_id != claim_call_id {
        return Err(ExecuteRefusal::Mismatch(format!(
            "the call id '{request_call_id}' is not the one the capability token was issued for"
        )));
    }
    let Ok(Some(row)) = store.effect_get(run_id, claim_call_id) else {
        return Err(ExecuteRefusal::Unavailable);
    };
    if row.tool != tool || row.args_digest != args_digest {
        return Err(ExecuteRefusal::Mismatch(format!(
            "call '{claim_call_id}' does not match its journal row (tool or arguments differ)"
        )));
    }
    let Some(secret) = secret else {
        return Err(ExecuteRefusal::Unavailable);
    };
    match derive(secret, run_id, claim_call_id, args_digest) {
        Ok(key) if key == row.idempotency_key => {}
        _ => {
            return Err(ExecuteRefusal::Mismatch(format!(
                "call '{claim_call_id}' does not match its idempotency key"
            )))
        }
    }
    if row.state != EffectState::Intended {
        return Ok(ExecuteOpen::Answer(answer_from_row(&row)));
    }
    inflight_enter(store, run_id);
    Ok(ExecuteOpen::Proceed(EffectCall {
        store,
        run_id: run_id.to_string(),
        call_id: claim_call_id.to_string(),
        tool: tool.to_string(),
        class: row.class,
        key: row.idempotency_key,
        generation,
        phase: AtomicU8::new(OPEN),
    }))
}

fn is_error_response(response: &ExecuteHostToolResponse) -> bool {
    response.error.is_some() || response.text_result.starts_with("tool_error:")
}

impl EffectCall<'_> {
    /// The idempotency key the vendor receives.
    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn class(&self) -> EffectClass {
        self.class
    }

    /// Writes `sent` immediately before the call leaves the host. An `Err` means nothing may be
    /// sent (R1).
    pub fn mark_sent(&self) -> Result<(), String> {
        match self
            .store
            .effect_mark_sent(&self.run_id, &self.call_id, self.generation)
        {
            Ok(_) => {
                self.phase.store(SENT, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => {
                tracing::error!(run_id = %self.run_id, call_id = %self.call_id, "effect journal: sent not recorded: {e}");
                Err(UNAVAILABLE_MESSAGE.to_string())
            }
        }
    }

    /// The call left and no definite answer came back (timeout, transport loss).
    pub fn settle_unknown(&self) {
        if self.phase.swap(SETTLED, Ordering::SeqCst) == SETTLED {
            return;
        }
        if let Err(e) = self.store.effect_mark_unknown(&self.run_id, &self.call_id) {
            tracing::error!(run_id = %self.run_id, call_id = %self.call_id, "effect journal: unknown not recorded: {e}");
        }
        self.emit("unknown");
    }

    /// A definite failure: the far end refused, or the call never left.
    pub fn settle_failed(&self, detail: &str) {
        if self.phase.swap(SETTLED, Ordering::SeqCst) == SETTLED {
            return;
        }
        let detail: String = detail.chars().take(240).collect();
        if let Err(e) = self
            .store
            .effect_mark_failed(&self.run_id, &self.call_id, &detail, None)
        {
            tracing::error!(run_id = %self.run_id, call_id = %self.call_id, "effect journal: failure not recorded: {e}");
        }
        self.emit("failed");
    }

    fn settle_confirmed(&self, response: &ExecuteHostToolResponse) {
        if self.phase.swap(SETTLED, Ordering::SeqCst) == SETTLED {
            return;
        }
        let bytes = response.encode_to_vec();
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        if let Err(e) = self.store.effect_mark_confirmed(
            &self.run_id,
            &self.call_id,
            &digest,
            Some(&bytes),
            None,
        ) {
            // The answer exists but could not be kept: the row stays `sent` and the drop turns it
            // `unknown` (never re-sent blindly).
            tracing::error!(run_id = %self.run_id, call_id = %self.call_id, "effect journal: confirmation not recorded: {e}");
            self.phase.store(SENT, Ordering::SeqCst);
            return;
        }
        self.emit("confirmed");
    }

    /// Settles whatever the send points did not: called once with the final response.
    pub fn finish(&self, response: &ExecuteHostToolResponse) {
        match self.phase.load(Ordering::SeqCst) {
            SETTLED => {}
            OPEN => {
                if is_error_response(response) {
                    let detail = response
                        .error
                        .as_ref()
                        .map(|e| e.message.clone())
                        .unwrap_or_else(|| response.text_result.clone());
                    self.settle_failed(&detail);
                } else {
                    self.settle_confirmed(response);
                }
            }
            _ => {
                if is_error_response(response) {
                    self.settle_unknown();
                } else {
                    self.settle_confirmed(response);
                }
            }
        }
    }

    fn emit(&self, state: &str) {
        emit_recorded(
            self.store,
            &self.run_id,
            &self.call_id,
            &self.tool,
            self.class,
            &self.key,
            state,
        );
    }
}

/// Appends `effect.recorded` for a settled row (reads are not announced).
fn emit_recorded(
    store: &SqliteWorkflowStore,
    run_id: &str,
    call_id: &str,
    tool: &str,
    class: EffectClass,
    key: &str,
    state: &str,
) {
    if class == EffectClass::Read {
        return;
    }
    let attempt = store
        .effect_get(run_id, call_id)
        .ok()
        .flatten()
        .map(|r| r.attempts)
        .unwrap_or(1);
    let key_prefix: String = key.chars().take(8).collect();
    crate::trajectory::append_trajectory_event(
        store,
        run_id,
        crate::events::TYPE_EFFECT_RECORDED,
        &serde_json::json!({
            "callId": call_id,
            "tool": tool,
            "class": class.as_str(),
            "state": state,
            "attempt": attempt,
            "keyPrefix": key_prefix,
        }),
    );
}

impl Drop for EffectCall<'_> {
    fn drop(&mut self) {
        if self.phase.load(Ordering::SeqCst) == SENT {
            // The send was cut off (the future was dropped): the outcome is not known.
            self.settle_unknown();
        }
        inflight_leave(self.store, &self.run_id);
    }
}

// ---------------------------------------------------------------------------------------------
// Connection writes (CN-4.7)

/// Why the egress path refuses a journaled write before anything is sent.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteRefusal {
    /// The journal could not be read or written, or the call has no row (R1).
    Unavailable,
    /// The call may already have written and its outcome is not known: not sent again.
    Refused(String),
}

impl WriteRefusal {
    /// The line the guest is told.
    pub fn text(&self) -> String {
        match self {
            Self::Unavailable => UNAVAILABLE_MESSAGE.to_string(),
            Self::Refused(why) => why.clone(),
        }
    }
}

/// What the model is told when a resumed call is answered from a confirmed connection write.
pub fn write_done_text(tool: &str, status: u16) -> String {
    format!(
        "Done: {tool} had already been sent before the task was interrupted (the service \
         answered {status}). It is not sent again."
    )
}

/// One journaled write of a code tool, reserved where the host attaches the member's token.
///
/// Created by [`reserve_write`] (the row is `intended`, and the run counts a call in flight so a
/// resume waits for it). [`vam_network::WriteTicket::commit_send`] writes `sent` immediately
/// before the request leaves the host, and `answered` settles the row from the far end's status.
/// Dropped after `commit_send` with no answer (the request failed in transit, or the future was
/// cancelled) it leaves the row `unknown`; dropped before, nothing was sent and the row is as it
/// was. The first write of a call is the call's own row; a further write is a child row
/// `<call id>/w<n>` that names the call as its parent.
pub struct WriteCall {
    store: std::sync::Arc<SqliteWorkflowStore>,
    run_id: String,
    call_id: String,
    tool: String,
    class: EffectClass,
    key: String,
    generation: u64,
    phase: AtomicU8,
}

/// Looks up the journal row of the connection write the envelope just admitted and reserves it.
///
/// * `Ok(None)`: not journaled (a read, a tool that is not a connection write, or a session
///   whose lease carries no run id). Nothing changes.
/// * `Ok(Some(call))`: the first write of the call, or a further write recorded as a child row.
/// * `Err`: nothing may be sent: the journal is unreadable or has no row for the call (R1), or an
///   earlier non-idempotent write of this call has an unknown outcome.
pub fn reserve_write(
    store: &std::sync::Arc<SqliteWorkflowStore>,
    session_id: &str,
    tool: &str,
    call_id: &str,
    generation: u64,
) -> Result<Option<WriteCall>, WriteRefusal> {
    let run_id = match store.find_run_id_for_session(session_id) {
        Ok(Some(run_id)) => run_id,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::error!(session_id, "effect journal: run not readable: {e}");
            return Err(WriteRefusal::Unavailable);
        }
    };
    let entry = match store.get_tool(session_id, tool) {
        Ok(Some(entry)) => entry,
        Ok(None) => return Ok(None),
        Err(e) => {
            tracing::error!(run_id, tool, "effect journal: tool not readable: {e}");
            return Err(WriteRefusal::Unavailable);
        }
    };
    if !is_connection_write(store, session_id, tool, entry.side_effect) {
        return Ok(None);
    }
    let row = match store.effect_get(&run_id, call_id) {
        Ok(Some(row)) => row,
        Ok(None) => {
            // A journaled tool always has its row from the intent (a guest that sent no call id
            // is journaled under an id the host chose, which the window does not carry).
            tracing::error!(run_id, tool, call_id, "effect journal: no row for a write");
            return Err(WriteRefusal::Unavailable);
        }
        Err(e) => {
            tracing::error!(
                run_id,
                tool,
                call_id,
                "effect journal: row not readable: {e}"
            );
            return Err(WriteRefusal::Unavailable);
        }
    };
    if row.tool != tool {
        tracing::error!(run_id, tool, call_id, row_tool = %row.tool, "effect journal: row is for another tool");
        return Err(WriteRefusal::Unavailable);
    }
    let reserved = |call_id: String, key: String| {
        inflight_enter(store, &run_id);
        WriteCall {
            store: store.clone(),
            run_id: run_id.clone(),
            call_id,
            tool: tool.to_string(),
            class: row.class,
            key,
            generation,
            phase: AtomicU8::new(OPEN),
        }
    };
    if row.state == EffectState::Intended {
        return Ok(Some(reserved(
            call_id.to_string(),
            row.idempotency_key.clone(),
        )));
    }
    // A further write of the same call: recorded as a child row of its own.
    let child_prefix = format!("{call_id}/w");
    let children: Vec<EffectRow> = store
        .effects_for_run(&run_id)
        .map_err(|e| {
            tracing::error!(
                run_id,
                tool,
                call_id,
                "effect journal: rows not readable: {e}"
            );
            WriteRefusal::Unavailable
        })?
        .rows
        .into_iter()
        .filter(|r| r.call_id.starts_with(&child_prefix))
        .collect();
    let unsettled = |state: EffectState| matches!(state, EffectState::Sent | EffectState::Unknown);
    if row.class == EffectClass::NonIdempotent
        && (unsettled(row.state) || children.iter().any(|c| unsettled(c.state)))
    {
        return Err(WriteRefusal::Refused(format!(
            "{tool} already wrote in this call and the outcome is not known; it is not sent \
             again. Check with a read before doing it again."
        )));
    }
    let n = children.len() + 2;
    let child_id = format!("{child_prefix}{n}");
    let key = format!("{}/w{n}", row.idempotency_key);
    store
        .effect_begin(&NewEffect {
            run_id: run_id.clone(),
            call_id: child_id.clone(),
            parent_call_id: Some(call_id.to_string()),
            tool: tool.to_string(),
            adapter: "code".to_string(),
            class: row.class,
            idempotency_key: key.clone(),
            args_digest: row.args_digest.clone(),
            approval_id: None,
        })
        .map_err(|e| {
            tracing::error!(
                run_id,
                tool,
                call_id,
                "effect journal: further write not recorded: {e}"
            );
            WriteRefusal::Unavailable
        })?;
    Ok(Some(reserved(child_id, key)))
}

impl WriteCall {
    fn emit(&self, state: &str) {
        emit_recorded(
            &self.store,
            &self.run_id,
            &self.call_id,
            &self.tool,
            self.class,
            &self.key,
            state,
        );
    }

    fn settle_unknown(&self) {
        if self.phase.swap(SETTLED, Ordering::SeqCst) == SETTLED {
            return;
        }
        if let Err(e) = self.store.effect_mark_unknown(&self.run_id, &self.call_id) {
            tracing::error!(run_id = %self.run_id, call_id = %self.call_id, "effect journal: unknown not recorded: {e}");
        }
        self.emit("unknown");
    }

    fn settle_failed(&self, detail: &str) {
        if self.phase.swap(SETTLED, Ordering::SeqCst) == SETTLED {
            return;
        }
        if let Err(e) = self
            .store
            .effect_mark_failed(&self.run_id, &self.call_id, detail, None)
        {
            tracing::error!(run_id = %self.run_id, call_id = %self.call_id, "effect journal: failure not recorded: {e}");
        }
        self.emit("failed");
    }

    fn settle_confirmed(&self, status: u16) {
        let response = ExecuteHostToolResponse {
            text_result: write_done_text(&self.tool, status),
            artifact_relpath: String::new(),
            artifact_bytes: Vec::new(),
            extra_artifacts: Vec::new(),
            error: None,
        };
        let bytes = response.encode_to_vec();
        let digest = format!("sha256:{:x}", Sha256::digest(&bytes));
        if let Err(e) = self.store.effect_mark_confirmed(
            &self.run_id,
            &self.call_id,
            &digest,
            Some(&bytes),
            None,
        ) {
            // The answer came but could not be kept: the row stays `sent` and the drop turns it
            // `unknown` (never re-sent blindly).
            tracing::error!(run_id = %self.run_id, call_id = %self.call_id, "effect journal: confirmation not recorded: {e}");
            return;
        }
        self.phase.store(SETTLED, Ordering::SeqCst);
        self.emit("confirmed");
    }
}

impl vam_network::WriteTicket for WriteCall {
    fn commit_send(&mut self) -> Result<(), vam_network::ConnectionRefusal> {
        match self
            .store
            .effect_mark_sent(&self.run_id, &self.call_id, self.generation)
        {
            Ok(_) => {
                self.phase.store(SENT, Ordering::SeqCst);
                Ok(())
            }
            Err(e) => {
                tracing::error!(run_id = %self.run_id, call_id = %self.call_id, "effect journal: sent not recorded: {e}");
                Err(vam_network::ConnectionRefusal {
                    text: UNAVAILABLE_MESSAGE.to_string(),
                    rule: format!("tool:{} effect journal", self.tool),
                })
            }
        }
    }

    fn answered(&mut self, status: u16) {
        if self.phase.load(Ordering::SeqCst) != SENT {
            return;
        }
        match status {
            200..=299 => self.settle_confirmed(status),
            // The far end may have processed it before failing: not known.
            500..=599 => self.settle_unknown(),
            // Refused (408 and 429 included: they processed nothing): the effect did not happen.
            _ => self.settle_failed(&format!("answered {status}")),
        }
    }
}

impl Drop for WriteCall {
    fn drop(&mut self) {
        if self.phase.load(Ordering::SeqCst) == SENT {
            // The request left and no answer was recorded: the outcome is not known.
            self.settle_unknown();
        }
        inflight_leave(&self.store, &self.run_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (SqliteWorkflowStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = SqliteWorkflowStore::open(dir.path().join("vam.db")).unwrap();
        (store, dir)
    }

    fn secret() -> EffectKeySecret {
        crate::signing::NodeKeys::ephemeral().effect_key_secret()
    }

    fn facts<'a>(
        store: &'a SqliteWorkflowStore,
        secret: &EffectKeySecret,
        call_id: &'a str,
    ) -> IntentFacts<'a> {
        IntentFacts {
            store,
            secret: Some(secret.clone()),
            session_id: "s1",
            run_id: "run_1",
            tool: "t",
            adapter_id: "echo",
            side_effect: 3,
            call_id,
            args_digest: "abcdef0123456789",
            approval_id: None,
        }
    }

    #[test]
    fn scope_covers_the_three_adapters_and_skips_internal_ones() {
        assert!(is_journaled("host", "echo", "t"));
        assert!(is_journaled("host", "hosted:broker:tool", "t"));
        assert!(is_journaled("host", "mcp:conn:tool", "t"));
        assert!(!is_journaled("host", "start_playbook", "start_playbook"));
        assert!(!is_journaled("host", "run_step", "run_step"));
        assert!(!is_journaled("builtin", "post_message", "post_message"));
        assert_eq!(adapter_kind("hosted:broker:tool"), "hsp");
        assert_eq!(adapter_kind("mcp:conn:tool"), "mcp");
        assert_eq!(adapter_kind("echo"), "host");
    }

    #[test]
    fn a_legacy_call_gets_a_flagged_id_and_an_approval_resume_adopts_its_row() {
        let (s, _d) = store();
        let k = secret();
        let id = record_intent(&facts(&s, &k, "")).unwrap().unwrap();
        assert_eq!(id, "legacy-t-abcdef01-1");
        let second = record_intent(&facts(&s, &k, "")).unwrap().unwrap();
        // The first row has no approval, so a second legacy call is a new row.
        assert_eq!(second, "legacy-t-abcdef01-2");
        let mut with_approval = facts(&s, &k, "c9");
        with_approval.approval_id = Some("appr_1");
        record_intent(&with_approval).unwrap();
        let adopted = record_intent(&facts(&s, &k, "")).unwrap().unwrap();
        assert_eq!(adopted, "c9");
    }

    #[test]
    fn an_amended_approval_rebinds_the_row_and_its_key() {
        let (s, _d) = store();
        let k = secret();
        let mut suspended = facts(&s, &k, "c1");
        suspended.approval_id = Some("appr_1");
        record_intent(&suspended).unwrap();
        let before = s.effect_get("run_1", "c1").unwrap().unwrap();
        let mut amended = facts(&s, &k, "c1");
        amended.args_digest = "ffffffff00000000";
        record_intent(&amended).unwrap();
        let after = s.effect_get("run_1", "c1").unwrap().unwrap();
        assert_eq!(after.args_digest, "ffffffff00000000");
        assert_ne!(before.idempotency_key, after.idempotency_key);
        assert_eq!(after.approval_id.as_deref(), Some("appr_1"));
    }

    #[test]
    fn no_run_means_no_journal_and_no_secret_fails_closed() {
        let (s, _d) = store();
        let k = secret();
        let mut no_run = facts(&s, &k, "c1");
        no_run.run_id = "";
        assert_eq!(record_intent(&no_run).unwrap(), None);
        let mut no_secret = facts(&s, &k, "c1");
        no_secret.secret = None;
        assert!(record_intent(&no_secret).is_err());
        assert!(s.effect_get("run_1", "c1").unwrap().is_none());
    }

    #[test]
    fn execute_checks_the_row_and_sends_only_an_intended_call() {
        let (s, _d) = store();
        let k = secret();
        record_intent(&facts(&s, &k, "c1")).unwrap();
        let open = |call: &str, claim: &str, tool: &str, digest: &str| {
            open_execute(&s, Some(&k), "run_1", call, claim, tool, digest, 1)
        };
        assert!(matches!(
            open("c2", "c1", "t", "abcdef0123456789"),
            Err(ExecuteRefusal::Mismatch(_))
        ));
        assert!(matches!(
            open("c1", "c1", "other", "abcdef0123456789"),
            Err(ExecuteRefusal::Mismatch(_))
        ));
        assert!(matches!(
            open("c1", "c1", "t", "0000"),
            Err(ExecuteRefusal::Mismatch(_))
        ));
        assert!(matches!(
            open("c1", "", "t", "abcdef0123456789"),
            Err(ExecuteRefusal::Unavailable)
        ));
        let Ok(ExecuteOpen::Proceed(call)) = open("", "c1", "t", "abcdef0123456789") else {
            panic!("an intended row proceeds");
        };
        call.mark_sent().unwrap();
        call.finish(&ExecuteHostToolResponse {
            text_result: "T-1".into(),
            ..Default::default()
        });
        drop(call);
        let row = s.effect_get("run_1", "c1").unwrap().unwrap();
        assert_eq!(row.state, EffectState::Confirmed);
        // A second execute under the same call id is answered from the row, not sent.
        let Ok(ExecuteOpen::Answer(again)) = open("c1", "c1", "t", "abcdef0123456789") else {
            panic!("a confirmed row answers");
        };
        assert_eq!(again.text_result, "T-1");
    }

    #[tokio::test]
    async fn a_stop_waits_for_a_call_in_flight() {
        let (s, _d) = store();
        let k = secret();
        let mut f = facts(&s, &k, "c1");
        f.run_id = "run_inflight";
        record_intent(&f).unwrap();
        let Ok(ExecuteOpen::Proceed(call)) = open_execute(
            &s,
            Some(&k),
            "run_inflight",
            "c1",
            "c1",
            "t",
            "abcdef0123456789",
            1,
        ) else {
            panic!()
        };
        assert_eq!(inflight_count(&s, "run_inflight"), 1);
        assert!(!wait_idle(&s, "run_inflight", Duration::from_millis(60)).await);
        drop(call);
        assert!(wait_idle(&s, "run_inflight", Duration::from_millis(60)).await);
    }

    #[test]
    fn a_dropped_send_is_unknown_and_an_error_before_send_is_failed() {
        let (s, _d) = store();
        let k = secret();
        record_intent(&facts(&s, &k, "c1")).unwrap();
        record_intent(&facts(&s, &k, "c2")).unwrap();
        let Ok(ExecuteOpen::Proceed(call)) = open_execute(
            &s,
            Some(&k),
            "run_1",
            "c1",
            "c1",
            "t",
            "abcdef0123456789",
            1,
        ) else {
            panic!()
        };
        call.mark_sent().unwrap();
        drop(call);
        assert_eq!(
            s.effect_get("run_1", "c1").unwrap().unwrap().state,
            EffectState::Unknown
        );
        let Ok(ExecuteOpen::Proceed(call)) = open_execute(
            &s,
            Some(&k),
            "run_1",
            "c2",
            "c2",
            "t",
            "abcdef0123456789",
            1,
        ) else {
            panic!()
        };
        call.finish(&ExecuteHostToolResponse {
            text_result: "tool_error: nope".into(),
            ..Default::default()
        });
        drop(call);
        let row = s.effect_get("run_1", "c2").unwrap().unwrap();
        assert_eq!(row.state, EffectState::Failed);
        assert_eq!(row.attempts, 0);
    }
}
