//! Reconciling a call whose answer was lost (E10 Phase 4, AR-4.7).
//!
//! A resumed task re-intents the calls of the round it had decided. A call whose row is
//! `unknown` is handled here, before the sync intent decides anything:
//!
//! * **read, idempotent:** put back to `intended` and sent again under the same key.
//! * **non-idempotent with a declared verify read:** the host runs the read as the agent
//!   (an intent with its own `policy.decided`, then the execute path, as a read-class row
//!   `<call id>/verify`). Found: the row becomes `confirmed` (reconciled by `verify`) and the
//!   model is told so. Not found: the row stays `unknown` with the check noted, because a read
//!   can miss a write that has not shown up yet.
//! * **non-idempotent without a verify read:** nothing runs; the model gets the unknown text.
//!
//! A non-idempotent call is never sent again by the host. An identical later call from the model
//! asks the person first (`SessionSecurityContext::gate_identical_after_unknown`).
//!
//! The verify declaration is read from a broker manifest or an MCP connection's owner-set
//! `effects` map. A skill-manifest declaration is not reachable from the intent path (a known
//! gap): such a tool is treated as having no verify read.

use prost_types::Struct;
use serde_json::Value;
use sha2::{Digest, Sha256};
use vam_spec::v1::tool_intent_response::Disposition;
use vam_spec::v1::{
    effect_found::Match, EffectFound, ExecuteHostToolRequest, ExecuteHostToolResponse,
    ToolIntentRequest,
};

use crate::effect_class::EffectClass;
use crate::effect_store::EffectState;
use crate::host_tools::HostToolRegistry;
use crate::secrets::SecretsVault;
use crate::security::SessionSecurityContext;
use crate::workflow::SqliteWorkflowStore;

/// `by` of an `effect.reconciled` event when a check settled the call.
pub const BY_VERIFY: &str = "verify";
/// `by` when the host itself re-sent a read or idempotent call.
pub const BY_HOST: &str = "host";
/// `by` when the person approved sending an identical call again.
pub const BY_PERSON: &str = "person";

fn text_of(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        other => other.to_string(),
    }
}

fn render_string(template: &str, args: &Value) -> Value {
    let lookup = |name: &str| args.get(name).cloned().unwrap_or(Value::Null);
    if let Some(name) = template
        .strip_prefix("{{args.")
        .and_then(|rest| rest.strip_suffix("}}"))
        .filter(|name| !name.contains("{{"))
    {
        return lookup(name);
    }
    let mut out = String::new();
    let mut rest = template;
    while let Some(start) = rest.find("{{args.") {
        out.push_str(&rest[..start]);
        let after = &rest[start + "{{args.".len()..];
        match after.find("}}") {
            Some(end) => {
                out.push_str(&text_of(&lookup(&after[..end])));
                rest = &after[end + 2..];
            }
            None => {
                out.push_str(&rest[start..]);
                rest = "";
            }
        }
    }
    out.push_str(rest);
    Value::String(out)
}

/// Fills `{{args.<name>}}` in every string of a verify read's argument template from the call's
/// arguments. A string that is exactly one placeholder takes the argument's value (any JSON
/// type); a placeholder inside longer text is replaced by the argument as text.
pub fn render_template(template: &Value, args: &Value) -> Value {
    match template {
        Value::String(text) => render_string(text, args),
        Value::Array(items) => {
            Value::Array(items.iter().map(|v| render_template(v, args)).collect())
        }
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), render_template(v, args)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn at_path<'a>(answer: &'a Value, path: &str) -> Option<&'a Value> {
    let mut current = answer;
    for part in path.split('.').filter(|p| !p.is_empty()) {
        current = match current {
            Value::Object(map) => map.get(part)?,
            Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(current)
}

fn is_empty_value(value: &Value) -> bool {
    match value {
        Value::Null => true,
        Value::Bool(b) => !b,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
    }
}

/// Whether a verify read's JSON answer shows the effect happened.
pub fn evaluate_found(found: &EffectFound, answer: &Value, args: &Value) -> bool {
    let Some(value) = at_path(answer, &found.path) else {
        return false;
    };
    match &found.r#match {
        Some(Match::NonEmpty(true)) => !is_empty_value(value),
        Some(Match::NonEmpty(false)) | None => false,
        Some(Match::Equals(expected)) => {
            let expected = text_of(&render_string(expected, args));
            text_of(value) == expected
        }
    }
}

/// The payload of a vendor result the host fenced as untrusted data
/// (`<<<VAM-UNTRUSTED-DATA …>>>`, a notice, `---`, the payload, `---`, the end marker). Text that
/// is not fenced is returned as it is. The check reads the payload only; the model is still given
/// the fenced text.
pub fn unfence(text: &str) -> &str {
    if !text.starts_with("<<<VAM-UNTRUSTED-DATA") {
        return text;
    }
    let Some(start) = text.find("\n---\n") else {
        return text;
    };
    let body = &text[start + "\n---\n".len()..];
    match body.rfind("\n---\n<<<END-VAM-UNTRUSTED-DATA") {
        Some(end) => &body[..end],
        None => body,
    }
}

/// What a verify read concluded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    pub found: bool,
    /// The verify read's answer text.
    pub text: String,
}

fn emit(
    store: &SqliteWorkflowStore,
    run_id: &str,
    call_id: &str,
    tool: &str,
    outcome: &str,
    by: &str,
) {
    crate::events::append_run_event(
        store,
        run_id,
        crate::events::TYPE_EFFECT_RECONCILED,
        &serde_json::json!({ "callId": call_id, "tool": tool, "outcome": outcome, "by": by }),
    );
}

/// Writes `effect.reconciled` for an identical call the person allowed to go again.
pub fn emit_person_allowed(store: &SqliteWorkflowStore, run_id: &str, call_id: &str, tool: &str) {
    emit(store, run_id, call_id, tool, "resent", BY_PERSON);
}

fn execute_response(text: String) -> ExecuteHostToolResponse {
    ExecuteHostToolResponse {
        text_result: text,
        artifact_relpath: String::new(),
        artifact_bytes: Vec::new(),
        extra_artifacts: Vec::new(),
        error: None,
    }
}

impl SessionSecurityContext {
    /// Settles the `unknown` row of the call a resumed guest is about to re-intent, if there is
    /// one. Safe to call for any request: it does nothing unless the request names a journaled
    /// call of this run, with the same tool and arguments, that is `unknown`.
    #[allow(clippy::too_many_arguments)]
    pub async fn reconcile_unknown(
        &self,
        store: &SqliteWorkflowStore,
        vault: Option<&SecretsVault>,
        registry: &HostToolRegistry,
        skills_dir: &std::path::Path,
        request: &ToolIntentRequest,
    ) {
        let call_id = request.call_id.as_str();
        if call_id.is_empty() {
            return;
        }
        let Ok(Some(run_id)) = store.find_run_id_for_session(&self.session_id) else {
            return;
        };
        let Ok(Some(mut row)) = store.effect_get(&run_id, call_id) else {
            return;
        };
        let Ok(digest) = vam_spec::canonical_args_digest(request.arguments.as_ref()) else {
            return;
        };
        if row.tool != request.tool_name || row.args_digest != digest {
            return;
        }
        if row.state == EffectState::Sent && crate::effect_path::inflight_count(store, &run_id) == 0
        {
            // Nothing is sending it: the answer was lost.
            if store.effect_mark_unknown(&run_id, call_id).is_err() {
                return;
            }
            match store.effect_get(&run_id, call_id) {
                Ok(Some(fresh)) => row = fresh,
                _ => return,
            }
        }
        if row.state != EffectState::Unknown {
            return;
        }
        match row.class {
            EffectClass::Read | EffectClass::Idempotent => {
                if store.effect_resend(&run_id, call_id).is_ok() {
                    emit(store, &run_id, call_id, &row.tool, "resent", BY_HOST);
                }
            }
            EffectClass::NonIdempotent => {
                if row.reconciled_by.is_some() {
                    // A check already ran for this call.
                    return;
                }
                let Some(args) = request.arguments.as_ref() else {
                    return;
                };
                let Some(verified) = self
                    .run_verify(
                        store,
                        vault,
                        registry,
                        skills_dir,
                        request.lease.as_ref(),
                        call_id,
                        &row.tool,
                        args,
                    )
                    .await
                else {
                    return;
                };
                if verified.found {
                    let response =
                        execute_response(crate::effect_path::verified_text(&verified.text));
                    let bytes = prost::Message::encode_to_vec(&response);
                    let result_digest = format!("sha256:{:x}", Sha256::digest(&bytes));
                    if store
                        .effect_mark_confirmed(
                            &run_id,
                            call_id,
                            &result_digest,
                            Some(&bytes),
                            Some(BY_VERIFY),
                        )
                        .is_ok()
                    {
                        emit(store, &run_id, call_id, &row.tool, "confirmed", BY_VERIFY);
                    }
                } else if store
                    .effect_note_checked(&run_id, call_id, BY_VERIFY)
                    .is_ok()
                {
                    emit(store, &run_id, call_id, &row.tool, "not_found", BY_VERIFY);
                }
            }
        }
    }

    /// Runs the verify read a tool declares, as the agent: its own intent (and `policy.decided`),
    /// then the execute path, journaled as the read-class row `<call id>/verify`. `None` when the
    /// tool declares no verify read, the read cannot run, or its answer cannot be read; the call
    /// is then left as it was.
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn run_verify(
        &self,
        store: &SqliteWorkflowStore,
        vault: Option<&SecretsVault>,
        registry: &HostToolRegistry,
        skills_dir: &std::path::Path,
        lease: Option<&vam_spec::v1::ExecutionLease>,
        call_id: &str,
        tool: &str,
        args: &Struct,
    ) -> Option<Verified> {
        let entry = store.get_tool(&self.session_id, tool).ok()??;
        let spec = crate::effect_path::declared_effect(store, &self.session_id, &entry.entrypoint)?;
        let verify = spec.verify?;
        let found_rule = verify.found?;
        let sibling = if let Some((broker, _)) =
            vam_spec::validation::parse_hosted_adapter_id(&entry.entrypoint)
        {
            vam_spec::validation::hosted_adapter_id(&broker, &verify.tool)
        } else if let Some((connection, _)) =
            vam_spec::validation::parse_mcp_adapter_id(&entry.entrypoint)
        {
            vam_spec::validation::mcp_adapter_id(&connection, &verify.tool)
        } else {
            return None;
        };
        // The verify tool must be a read in this session's catalog.
        let verify_entry = store
            .get_tool_catalog(&self.session_id)
            .ok()?
            .into_iter()
            .find(|e| e.entrypoint == sibling)?;
        if verify_entry.side_effect != vam_spec::v1::SideEffectClass::ReadOnly as i32 {
            return None;
        }
        let args_json = crate::playbook_activation::struct_to_json(args);
        let template = verify
            .args_template
            .as_ref()
            .map(crate::playbook_activation::struct_to_json)
            .unwrap_or_else(|| serde_json::json!({}));
        let rendered = render_template(&template, &args_json);
        let verify_args = crate::playbook_activation::json_to_struct(&rendered)?;
        let verify_digest = vam_spec::canonical_args_digest(Some(&verify_args)).ok()?;
        let script_digest = if verify_entry.digest.is_empty() {
            None
        } else {
            Some(vam_spec::v1::Digest {
                algorithm: 1,
                hex_hash: verify_entry.digest.clone(),
            })
        };
        let verify_call_id = format!("{call_id}/verify");
        let intent = self.handle_tool_intent_request(
            store,
            ToolIntentRequest {
                lease: lease.cloned(),
                tool_name: verify_entry.tool_name.clone(),
                arguments: Some(verify_args.clone()),
                arguments_digest: Some(vam_spec::v1::Digest {
                    algorithm: 1,
                    hex_hash: verify_digest,
                }),
                script_digest,
                call_id: verify_call_id.clone(),
            },
        );
        let response = if intent.disposition == Disposition::Replayed as i32 {
            intent.replayed?
        } else if intent.disposition == Disposition::Authorized as i32 {
            let token = intent.capability_token?;
            Box::pin(self.handle_execute_host_tool(
                store,
                vault,
                registry,
                skills_dir,
                ExecuteHostToolRequest {
                    lease: lease.cloned(),
                    tool_name: verify_entry.tool_name.clone(),
                    arguments: Some(verify_args),
                    capability_token: Some(token),
                    call_id: verify_call_id,
                },
            ))
            .await
        } else {
            return None;
        };
        if response.error.is_some() || response.text_result.starts_with("tool_error:") {
            return None;
        }
        let answer: Value = serde_json::from_str(unfence(&response.text_result)).ok()?;
        Some(Verified {
            found: evaluate_found(&found_rule, &answer, &args_json),
            text: response.text_result,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn non_empty(path: &str) -> EffectFound {
        EffectFound {
            path: path.into(),
            r#match: Some(Match::NonEmpty(true)),
        }
    }

    #[test]
    fn a_placeholder_alone_keeps_the_argument_type_and_inside_text_is_substituted() {
        let args = json!({"title": "Q4 plan", "n": 3});
        let template =
            json!({"title": "{{args.title}}", "n": "{{args.n}}", "q": "t={{args.title}};", "k": 1});
        let rendered = render_template(&template, &args);
        assert_eq!(rendered["title"], "Q4 plan");
        assert_eq!(rendered["n"], 3);
        assert_eq!(rendered["q"], "t=Q4 plan;");
        assert_eq!(rendered["k"], 1);
    }

    #[test]
    fn non_empty_follows_the_dotted_path_and_treats_false_zero_and_empty_as_not_found() {
        let found = non_empty("a.b");
        let args = json!({});
        assert!(evaluate_found(&found, &json!({"a": {"b": "x"}}), &args));
        assert!(evaluate_found(&found, &json!({"a": {"b": [1]}}), &args));
        for empty in [
            json!(null),
            json!(false),
            json!(0),
            json!(""),
            json!([]),
            json!({}),
        ] {
            assert!(!evaluate_found(&found, &json!({"a": {"b": empty}}), &args));
        }
        assert!(!evaluate_found(&found, &json!({"a": {}}), &args));
        assert!(evaluate_found(
            &non_empty("items.0.id"),
            &json!({"items": [{"id": "T-1"}]}),
            &args
        ));
    }

    #[test]
    fn a_fenced_vendor_answer_is_read_through_its_fence() {
        let fenced = "<<<VAM-UNTRUSTED-DATA v1 id=ab source=hsp:x kind=result>>>\nUntrusted data.\n---\n{\"found\":true,\"id\":\"T-1\"}\n---\n<<<END-VAM-UNTRUSTED-DATA id=ab>>>";
        assert_eq!(unfence(fenced), "{\"found\":true,\"id\":\"T-1\"}");
        assert_eq!(unfence("{\"a\":1}"), "{\"a\":1}");
    }

    #[test]
    fn equals_compares_as_text_after_filling_the_call_arguments() {
        let found = EffectFound {
            path: "title".into(),
            r#match: Some(Match::Equals("{{args.title}}".into())),
        };
        let args = json!({"title": "Q4 plan"});
        assert!(evaluate_found(&found, &json!({"title": "Q4 plan"}), &args));
        assert!(!evaluate_found(&found, &json!({"title": "Q3 plan"}), &args));
        let number = EffectFound {
            path: "n".into(),
            r#match: Some(Match::Equals("3".into())),
        };
        assert!(evaluate_found(&number, &json!({"n": 3}), &args));
    }
}
