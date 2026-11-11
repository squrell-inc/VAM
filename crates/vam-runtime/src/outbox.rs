//! The outbox tools `post_message` and `show` (AR-3.9).
//!
//! The model posts a short progress message or a built-in card. Each call goes
//! through the same gates as every builtin: the argument check, then the PEP
//! intent (which returns a capability token for exactly these arguments), then
//! the port-52 method `PostOutbox`. The host checks the lease, the token, the
//! jig and the data again and answers with the tool result, so a guest that
//! skips the intent, or sends other data than it was authorized for, is refused.
//!
//! `show` is offered only when the run's harness has `human.jig` on.

use prost_types::value::Kind;
use prost_types::Struct;
use vam_spec::outbox::{
    is_outbox_tool, ASK_USER_TOOL, MAX_CARD_DATA_BYTES, MAX_TEXT_BYTES, POSTED, POST_MESSAGE_TOOL,
    SHOW_JIGS, SHOW_TOOL,
};
use vam_spec::v1::{ExecutionLease, OutboxKind, PostOutboxRequest, SignedEnvelope, ToolSpec};

use crate::pep::PostOutboxOutcome;

pub use vam_spec::outbox::tool_digest;

fn spec(name: &str, description: String, schema: &str) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description,
        input_schema_json: schema.to_string(),
        execution_provider: Some(vam_spec::v1::tool_spec::ExecutionProvider::Builtin(
            vam_spec::v1::BuiltinExecSpec {
                version: crate::primitives::BUILTIN_ABI_VERSION.to_string(),
            },
        )),
        required_secrets: vec![],
        side_effect: vam_spec::v1::SideEffectClass::ReadOnly as i32,
        output_schema_json: String::new(),
        timeout: None,
        ..Default::default()
    }
}

/// The input schema of the built-in Show jig `jig`, compact, for the description.
fn jig_schema_text(jig: &str) -> String {
    vam_spec::jig::builtin(jig)
        .map(|b| b.input_schema().to_string())
        .unwrap_or_default()
}

/// Both outbox tool specs. `show` is dropped by [`allowed_for`] unless the run
/// turned `human.jig` on.
pub fn tool_specs() -> Vec<ToolSpec> {
    let jigs: Vec<String> = SHOW_JIGS
        .iter()
        .map(|jig| format!("{jig} with data {}", jig_schema_text(jig)))
        .collect();
    let enum_list = SHOW_JIGS
        .iter()
        .map(|j| format!("\"{j}\""))
        .collect::<Vec<_>>()
        .join(",");
    vec![
        spec(
            POST_MESSAGE_TOOL,
            format!(
                "Post a short progress message to the user's chat while you work. Plain text, \
                 at most {MAX_TEXT_BYTES} bytes. Use it on long tasks to say what you are doing \
                 or what you found; it does not end the task and is not your final answer."
            ),
            r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#,
        ),
        spec(
            SHOW_TOOL,
            format!(
                "Show the user a card. `jig` names a built-in card and `data` fills it; the host \
                 checks the data before anything is shown, and strings are shown as plain text. \
                 A later show with the same card_id in `data` updates that card in place. \
                 Cards: {}.",
                jigs.join("; ")
            ),
            &format!(
                r#"{{"type":"object","properties":{{"jig":{{"type":"string","enum":[{enum_list}]}},"data":{{"type":"object"}}}},"required":["jig","data"]}}"#
            ),
        ),
        spec(
            ASK_USER_TOOL,
            format!(
                "Ask the user a question and wait for the answer. Use jig \"vam.question\" with \
                 data {}. The task pauses until the user answers; the answer comes back as \
                 {{\"answer\": \"...\"}}. When `choices` is given the answer is one of them. \
                 Ask only when you cannot go on without the user.",
                jig_schema_text("vam.question")
            ),
            r#"{"type":"object","properties":{"jig":{"type":"string","enum":["vam.question"]},"data":{"type":"object"}},"required":["jig","data"]}"#,
        ),
    ]
}

/// The advertised allowlist for a run: `show` stays only with `human.jig` on.
pub fn allowed_for(allowed: Vec<String>, human_jig: bool) -> Vec<String> {
    if human_jig {
        allowed
    } else {
        allowed.into_iter().filter(|n| n != SHOW_TOOL).collect()
    }
}

fn custom(
    tool: &str,
    field: &str,
    expected: &str,
    got: &str,
    detail: &str,
) -> vam_spec::call_check::CallError {
    vam_spec::call_check::CallError::custom(tool, field, expected, got, detail)
}

/// The rules the schema subset cannot state: exactly the fields the host will
/// rebuild the intent's arguments from, and the size caps. A call that passes
/// here carries nothing the host would not recompute.
pub fn validate_args(
    tool: &str,
    args: &serde_json::Value,
) -> Result<(), vam_spec::call_check::CallError> {
    let Some(map) = args.as_object() else {
        return Ok(());
    };
    let allowed: &[&str] = if tool == SHOW_TOOL || tool == ASK_USER_TOOL {
        &["jig", "data"]
    } else {
        &["text"]
    };
    if let Some(extra) = map.keys().find(|k| !allowed.contains(&k.as_str())) {
        return Err(custom(
            tool,
            extra,
            &format!("only {}", allowed.join(" and ")),
            "an unexpected field",
            &format!(
                "unexpected field '{extra}'. {tool} takes only {}.",
                allowed.join(" and ")
            ),
        ));
    }
    if tool == POST_MESSAGE_TOOL {
        if let Some(text) = map.get("text").and_then(|t| t.as_str()) {
            if text.len() > MAX_TEXT_BYTES {
                return Err(custom(
                    tool,
                    "text",
                    &format!("at most {MAX_TEXT_BYTES} bytes"),
                    &format!("{} bytes", text.len()),
                    &format!(
                        "text is {} bytes; the limit is {MAX_TEXT_BYTES}. Shorten it.",
                        text.len()
                    ),
                ));
            }
        }
    }
    if tool == SHOW_TOOL || tool == ASK_USER_TOOL {
        if let Some(data) = map.get("data") {
            let size = data.to_string().len();
            if size > MAX_CARD_DATA_BYTES {
                return Err(custom(
                    tool,
                    "data",
                    &format!("at most {MAX_CARD_DATA_BYTES} bytes"),
                    &format!("{size} bytes"),
                    &format!(
                        "data is {size} bytes; the limit is {MAX_CARD_DATA_BYTES}. Send less."
                    ),
                ));
            }
        }
    }
    Ok(())
}

fn string_arg(args: &Struct, key: &str) -> Option<String> {
    match args.fields.get(key).and_then(|v| v.kind.as_ref()) {
        Some(Kind::StringValue(s)) => Some(s.clone()),
        _ => None,
    }
}

/// Build the `PostOutbox` request for an authorized call, or the typed tool
/// result when the arguments cannot be posted.
pub fn build_request(
    tool: &str,
    args: &Struct,
    token: &SignedEnvelope,
) -> Result<PostOutboxRequest, String> {
    let mut request = PostOutboxRequest {
        capability_token: Some(token.clone()),
        ..Default::default()
    };
    if tool == POST_MESSAGE_TOOL {
        let Some(text) = string_arg(args, "text") else {
            return Err(format!(
                "{}{tool}: text must be a string",
                vam_spec::call_check::INVALID_ARGS_PREFIX
            ));
        };
        request.kind = OutboxKind::Message as i32;
        request.text = text;
        return Ok(request);
    }
    let Some(jig) = string_arg(args, "jig") else {
        return Err(format!(
            "{}{tool}: jig must be a string",
            vam_spec::call_check::INVALID_ARGS_PREFIX
        ));
    };
    let Some(Kind::StructValue(data)) = args.fields.get("data").and_then(|v| v.kind.clone()) else {
        return Err(format!(
            "{}{tool}: data must be an object",
            vam_spec::call_check::INVALID_ARGS_PREFIX
        ));
    };
    request.kind = OutboxKind::Card as i32;
    request.jig = jig;
    request.data_json = vam_spec::harness::struct_to_json(&data).to_string();
    Ok(request)
}

/// Run an authorized outbox call and return the tool result for the model.
pub async fn execute(
    lease: &ExecutionLease,
    tool: &str,
    args: &Struct,
    token: Option<&SignedEnvelope>,
) -> String {
    if tool == ASK_USER_TOOL {
        // The host turns the call into a question and suspends the task; only a
        // resume carries the answer. Reaching here means no question was asked.
        return "tool_error: ask_user could not ask the user".to_string();
    }
    let Some(token) = token else {
        return format!("tool_error: {tool} requires an authorized capability token");
    };
    let request = match build_request(tool, args, token) {
        Ok(request) => request,
        Err(message) => return message,
    };
    match crate::pep::post_outbox(lease, request).await {
        Ok(PostOutboxOutcome::Posted) => POSTED.to_string(),
        Ok(PostOutboxOutcome::Refused(message)) => message,
        Ok(PostOutboxOutcome::Unavailable) => {
            format!("tool_error: this host cannot post to the user yet; {tool} did nothing")
        }
        Err(err) => format!("tool_error: {tool} could not reach the host: {err}"),
    }
}

pub fn is_tool(name: &str) -> bool {
    is_outbox_tool(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pep::test_support::{connect, shared_host};
    use serde_json::json;

    fn args(v: serde_json::Value) -> Struct {
        vam_spec::harness::json_to_struct(&v).unwrap()
    }

    #[test]
    fn specs_name_both_tools_and_list_the_jig_schema() {
        let specs = tool_specs();
        let names: Vec<_> = specs.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, ["post_message", "show", "ask_user"]);
        let show = &specs[1];
        assert!(show.description.contains("vam.progress"));
        assert!(
            show.description.contains("\"done\""),
            "{}",
            show.description
        );
        let schema: serde_json::Value = serde_json::from_str(&show.input_schema_json).unwrap();
        assert_eq!(schema["properties"]["jig"]["enum"], json!(["vam.progress"]));
        for spec in &specs {
            serde_json::from_str::<serde_json::Value>(&spec.input_schema_json).unwrap();
            assert_eq!(
                spec.side_effect,
                vam_spec::v1::SideEffectClass::ReadOnly as i32
            );
        }
    }

    #[test]
    fn show_is_dropped_unless_the_jig_switch_is_on() {
        let all = vec!["run".to_string(), "post_message".into(), "show".into()];
        assert_eq!(allowed_for(all.clone(), false), ["run", "post_message"]);
        assert_eq!(allowed_for(all.clone(), true), all);
    }

    #[test]
    fn extra_fields_and_oversize_values_are_typed_errors() {
        assert!(validate_args("post_message", &json!({"text": "hi"})).is_ok());
        let err = validate_args("post_message", &json!({"text": "hi", "x": 1})).unwrap_err();
        assert!(
            err.message()
                .starts_with("invalid arguments for post_message"),
            "{err}"
        );
        assert!(err.message().contains("'x'"));
        let long = "a".repeat(MAX_TEXT_BYTES + 1);
        let err = validate_args("post_message", &json!({"text": long})).unwrap_err();
        assert!(err.message().contains("4096"), "{err}");
        assert!(
            validate_args("post_message", &json!({"text": "a".repeat(MAX_TEXT_BYTES)})).is_ok()
        );
        assert!(validate_args("show", &json!({"jig": "vam.progress", "data": {}})).is_ok());
        let err = validate_args(
            "show",
            &json!({"jig": "vam.progress", "data": {}, "card": 1}),
        )
        .unwrap_err();
        assert!(err.message().contains("'card'"));
        let big =
            json!({"jig": "vam.progress", "data": {"label": "x".repeat(MAX_CARD_DATA_BYTES)}});
        assert!(validate_args("show", &big)
            .unwrap_err()
            .message()
            .contains("16384"));
    }

    #[test]
    fn requests_carry_the_token_and_the_data_the_intent_signed() {
        let token = SignedEnvelope {
            domain: "d".into(),
            ..Default::default()
        };
        let msg =
            build_request("post_message", &args(json!({"text": "Starting"})), &token).unwrap();
        assert_eq!(msg.kind, OutboxKind::Message as i32);
        assert_eq!(msg.text, "Starting");
        assert_eq!(msg.capability_token.as_ref().unwrap().domain, "d");

        let card_args =
            args(json!({"jig": "vam.progress", "data": {"label": "x", "done": 1, "total": 2.5}}));
        let card = build_request("show", &card_args, &token).unwrap();
        assert_eq!(card.kind, OutboxKind::Card as i32);
        assert_eq!(card.jig, "vam.progress");
        let sent: serde_json::Value = serde_json::from_str(&card.data_json).unwrap();
        assert_eq!(sent, json!({"label": "x", "done": 1, "total": 2.5}));
        // What the host rebuilds equals what was authorized.
        let host = vam_spec::outbox::card_args(&card.jig, &sent).unwrap();
        assert_eq!(
            vam_spec::canonical_args_digest(Some(&host)).unwrap(),
            vam_spec::canonical_args_digest(Some(&card_args)).unwrap()
        );
        assert!(build_request("post_message", &args(json!({"text": 3})), &token).is_err());
        assert!(build_request(
            "show",
            &args(json!({"jig": "vam.progress", "data": 3})),
            &token
        )
        .is_err());
    }

    #[tokio::test]
    async fn outcomes_become_tool_results() {
        let token = SignedEnvelope::default();
        let host = shared_host(None);
        let mut stream = connect(&host);
        let request = build_request("post_message", &args(json!({"text": "hi"})), &token).unwrap();
        assert_eq!(
            crate::pep::post_outbox_on(&mut stream, request.clone())
                .await
                .unwrap(),
            PostOutboxOutcome::Posted
        );
        assert_eq!(host.lock().unwrap().posts_seen.len(), 1);

        host.lock().unwrap().post_outbox_error = Some((
            3,
            "invalid data for vam.progress: /done: expected integer".into(),
        ));
        assert_eq!(
            crate::pep::post_outbox_on(&mut stream, request.clone())
                .await
                .unwrap(),
            PostOutboxOutcome::Refused(
                "invalid data for vam.progress: /done: expected integer".into()
            )
        );
        host.lock().unwrap().post_outbox_error = Some((12, "unknown method".into()));
        assert_eq!(
            crate::pep::post_outbox_on(&mut stream, request)
                .await
                .unwrap(),
            PostOutboxOutcome::Unavailable
        );
    }

    #[tokio::test]
    async fn no_token_means_no_post() {
        let out = execute(
            &ExecutionLease::default(),
            "post_message",
            &args(json!({"text": "hi"})),
            None,
        )
        .await;
        assert!(
            out.starts_with("tool_error:") && out.contains("capability token"),
            "{out}"
        );
    }

    #[test]
    fn ask_user_is_always_offered_and_takes_only_jig_and_data() {
        let advertised = vec![
            "read_file".to_string(),
            "ask_user".to_string(),
            "show".to_string(),
        ];
        let off = allowed_for(advertised.clone(), false);
        assert!(off.contains(&"ask_user".to_string()) && !off.contains(&"show".to_string()));
        assert!(allowed_for(advertised, true).contains(&"ask_user".to_string()));
        assert!(validate_args(
            "ask_user",
            &json!({"jig": "vam.question", "data": {"question": "q"}})
        )
        .is_ok());
        assert!(validate_args(
            "ask_user",
            &json!({"jig": "vam.question", "data": {}, "x": 1})
        )
        .is_err());
        let big = json!({"jig": "vam.question", "data": {"question": "x".repeat(20_000)}});
        assert!(validate_args("ask_user", &big).is_err());
    }
}
