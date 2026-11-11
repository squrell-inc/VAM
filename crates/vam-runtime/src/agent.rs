use prost_types::Struct;
use std::io::Write;
use std::time::Duration;

use crate::hitl_checkpoint::{self, HitlContinuation};
use crate::inference::InferenceClient;
use crate::skills::{self, format_skills_catalog, SkillsRuntime};
use vam_spec::v1::{
    ChatMessage, ExecutionLease, InferenceRequest, PromptAction, TaskOutcome, TaskOutcomeState,
    TokenUsage, ToolCall,
};
use vam_vsock::guest_dial;

pub async fn run_react_agent_loop(
    lease: &ExecutionLease,
    prompt: &PromptAction,
    inputs: Option<&Struct>,
) -> Result<TaskOutcome, String> {
    run_react_agent_loop_with_trace(lease, prompt, inputs, None).await
}

pub async fn run_react_agent_loop_with_trace(
    lease: &ExecutionLease,
    prompt: &PromptAction,
    inputs: Option<&Struct>,
    trace_context: Option<&vam_spec::v1::TraceContext>,
) -> Result<TaskOutcome, String> {
    let mut inbox = crate::inbox::InboxCursor::default();
    let outcome = run_react_loop(lease, prompt, inputs, trace_context, &mut inbox).await;
    // Every way out of the loop passes here: a steer applied to the conversation is
    // acknowledged, so the host never holds it as pending after the task has ended.
    inbox.finish(lease).await;
    outcome
}

async fn run_react_loop(
    lease: &ExecutionLease,
    prompt: &PromptAction,
    inputs: Option<&Struct>,
    trace_context: Option<&vam_spec::v1::TraceContext>,
    inbox: &mut crate::inbox::InboxCursor,
) -> Result<TaskOutcome, String> {
    // The run's effective harness switches, resolved by the host at run creation.
    // Missing means today's defaults. Read here: `calls.*` (H-0.4) and `loop.*` (H-0.5).
    let harness = vam_spec::harness::Harness::from_inputs(inputs)
        .map_err(|e| format!("invalid harness input: {e}"))?;
    // Present only when `loop.detect` is on: off means no detector state.
    let mut loop_detector = harness
        .loop_
        .detect
        .then(|| crate::loop_detect::Detector::new(&harness.loop_));
    // Consecutive invalid calls to one tool, for `calls.repairBudget`.
    let mut invalid_streak: (String, u32) = (String::new(), 0);
    let hitl_denied = parse_hitl_denied_inputs(inputs);
    let hitl_resume = if hitl_denied.is_some() {
        None
    } else {
        parse_hitl_resume_inputs(inputs)
    };
    let hitl_resumed = hitl_denied.is_some() || hitl_resume.is_some();

    // 1. Compile initial prompt & instruction template
    let mut instruction = prompt.instruction_template.clone();
    if let Some(in_struct) = inputs {
        for (k, v) in &in_struct.fields {
            let placeholder = format!("{{{{inputs.{}}}}}", k);
            if let Some(prost_types::value::Kind::StringValue(ref s)) = v.kind {
                instruction = instruction.replace(&placeholder, s);
            }
        }
    }

    // Context appended to the user message, after the cached prefix: the
    // guidance of an explicitly started guidance-only playbook, and the playbook
    // status (with the gate decision) when a run resumes from a gate.
    if let Some(extra) = in_flight_context(inputs) {
        instruction.push_str("\n\n");
        instruction.push_str(&extra);
    }

    // 2. Initialize inference client by dialing port 53 (the broker plane)
    let stream = guest_dial(2, 53)
        .await
        .map_err(|e| format!("Failed to dial broker plane: {}", e))?;
    let client = InferenceClient::new(stream);

    let termination = resolve_termination(inputs);
    let mut claim_reprompted = false;
    let mut skills_runtime = SkillsRuntime::default();
    // MCP connections are marked `preloaded`: their tools are offered on turn
    // 1 and no `load_skill` call is needed. Cedar still filters the list.
    skills_runtime.preload();
    let skills_catalog = format_skills_catalog(&SkillsRuntime::scan_catalog());
    let mut playbook_procedure = playbook_procedure_from_inputs(inputs);
    // Playbooks the model may start. Offered only while none is bound.
    let mut offer_start = playbook_procedure.trim().is_empty()
        && !crate::instruction::attached_playbooks(inputs).is_empty();
    let allowed_names = match crate::pep::list_advertised_tools().await {
        // `show` only with `human.jig`; the host refuses it otherwise too.
        Ok(names) => crate::outbox::allowed_for(names, harness.human.jig),
        Err(e) => {
            tracing::error!(
                "Failed to list advertised tools from host PEP; fail-closed to empty allowlist: {}",
                e
            );
            Vec::new()
        }
    };
    let mut tools_list = rebuild_tools_list(
        &skills_runtime,
        &allowed_names,
        &playbook_procedure,
        offer_start,
        termination,
    );
    if tools_list.is_empty() {
        tracing::error!(
            "tools_list is empty after Cedar advertise filter; falling back to builtins"
        );
        tools_list = with_task_complete(
            filter_tools_by_advertised(crate::primitives::builtin_tool_specs(), &allowed_names),
            termination,
        );
    }
    let ws_root = crate::primitives::workspace_root();
    let mut system_prompt = build_agent_system_prompt(
        &ws_root,
        inputs,
        &tools_list,
        &skills_catalog,
        &skills_runtime.extensions_text(),
    );
    tracing::debug!(
        "system prompt: {} chars; tools ({}): {:?}",
        system_prompt.len(),
        tools_list.len(),
        tools_list
            .iter()
            .map(|t| t.name.as_str())
            .collect::<Vec<_>>()
    );
    let mut iteration_cap = resolve_max_iterations(inputs);
    let mut iterations_used_before_loop = 0usize;
    let mut max_iterations = iteration_cap;
    let step_timeout_secs = resolve_step_timeout_secs(inputs);
    let step_timeout = Duration::from_secs(step_timeout_secs);

    // 3. Initialize chat history
    let mut messages = vec![
        ChatMessage {
            role: "system".to_string(),
            content: system_prompt.clone(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        },
        ChatMessage {
            role: "user".to_string(),
            content: instruction.clone(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        },
    ];

    let mut total_tokens = TokenUsage::default();
    let mut nudge_count = 0usize;
    let mut last_answer_candidate = String::new();
    let mut force_finalize = false;
    let mut skill_tool_called = false;

    if let Some((tool_name, tool_args, reason)) = hitl_denied {
        let run_id = lease.run_id.as_ref().map(|r| r.urn.as_str()).unwrap_or("");
        let checkpoint = match load_resume_checkpoint(lease, inputs, run_id).await {
            Ok(checkpoint) => checkpoint,
            Err(message) => return Ok(fail_task(lease, 13, message, None)),
        };
        if let Some(cp) = checkpoint {
            inbox.resume_at(cp.inbox_seq);
            // The denial answers the pending call of the logged conversation,
            // so the context is kept (the real call id, siblings closed).
            reload_checkpoint_skills(&mut skills_runtime, &cp.loaded_skill_ids);
            tools_list = rebuild_tools_list(
                &skills_runtime,
                &allowed_names,
                &playbook_procedure,
                offer_start,
                termination,
            );
            if tools_list.is_empty() {
                tools_list = with_task_complete(
                    filter_tools_by_advertised(
                        crate::primitives::builtin_tool_specs(),
                        &allowed_names,
                    ),
                    termination,
                );
            }
            system_prompt = build_agent_system_prompt(
                &ws_root,
                inputs,
                &tools_list,
                &skills_catalog,
                &skills_runtime.extensions_text(),
            );
            let denied_nudge = hitl_denied_note(inputs).unwrap_or_else(default_hitl_denied_nudge);
            apply_hitl_checkpoint_restore(
                &cp,
                &system_prompt,
                format!(
                    "Error: Tool '{}' was denied by the user: {}",
                    tool_name, reason
                ),
                &denied_nudge,
                &tool_name,
                &skills_runtime,
                &mut messages,
                &mut total_tokens,
                &mut nudge_count,
                &mut last_answer_candidate,
                &mut skill_tool_called,
                &mut force_finalize,
                &mut iteration_cap,
                &mut iterations_used_before_loop,
                &mut max_iterations,
                &mut claim_reprompted,
            );
            force_finalize = true;
        } else {
            let tool_call_id = "hitl_denied_0".to_string();
            let tool_args_json = struct_to_json_string(&tool_args);
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: tool_call_id.clone(),
                    name: tool_name.clone(),
                    arguments: tool_args_json,
                }],
                tool_call_id: String::new(),
            });
            messages.push(ChatMessage {
                role: "tool".to_string(),
                content: format!(
                    "Error: Tool '{}' was denied by the user: {}",
                    tool_name, reason
                ),
                tool_calls: vec![],
                tool_call_id: tool_call_id.clone(),
            });
            let denied_nudge = hitl_denied_note(inputs).unwrap_or_else(default_hitl_denied_nudge);
            messages.push(ChatMessage {
                role: "user".to_string(),
                content: denied_nudge,
                tool_calls: vec![],
                tool_call_id: String::new(),
            });
            force_finalize = true;
        }
    } else if let Some((tool_name, tool_args)) = hitl_resume {
        let tool_args_json = struct_to_json_string(&tool_args);
        // A question's answer is the last message; there is no nudge after it.
        // An answer needs no nudge of its own. It carries a note only when the host recorded other
        // decisions while the run was parked (a gate decided at the same time).
        let resume_nudge = if tool_name == vam_spec::outbox::ASK_USER_TOOL {
            hitl_decisions_note(inputs).unwrap_or_default()
        } else {
            hitl_resume_note(inputs).unwrap_or_else(default_hitl_resume_nudge)
        };
        let run_id = lease.run_id.as_ref().map(|r| r.urn.as_str()).unwrap_or("");
        // The host-stored checkpoint is fetched and verified before the approved
        // tool runs. A digest that cannot be fetched or verified fails the task.
        let checkpoint = match load_resume_checkpoint(lease, inputs, run_id).await {
            Ok(checkpoint) => checkpoint,
            Err(message) => return Ok(fail_task(lease, 13, message, None)),
        };
        if let Some(ref cp) = checkpoint {
            inbox.resume_at(cp.inbox_seq);
            reload_checkpoint_skills(&mut skills_runtime, &cp.loaded_skill_ids);
            tools_list = rebuild_tools_list(
                &skills_runtime,
                &allowed_names,
                &playbook_procedure,
                offer_start,
                termination,
            );
            if tools_list.is_empty() {
                tools_list = with_task_complete(
                    filter_tools_by_advertised(
                        crate::primitives::builtin_tool_specs(),
                        &allowed_names,
                    ),
                    termination,
                );
            }
            system_prompt = build_agent_system_prompt(
                &ws_root,
                inputs,
                &tools_list,
                &skills_catalog,
                &skills_runtime.extensions_text(),
            );
        }
        // A question's answer is the human decision itself: it is the tool result,
        // and nothing is re-authorized or executed.
        let observed = if tool_name == vam_spec::outbox::ASK_USER_TOOL {
            Ok(hitl_answer(inputs).unwrap_or_else(|| {
                "tool_error: ask_user: the answer was lost; ask again".to_string()
            }))
        } else {
            execute_hitl_approved_tool_observation(
                lease,
                &tool_name,
                &tool_args,
                &mut skills_runtime,
                &playbook_procedure,
                harness.calls.result_cap as usize,
                checkpoint
                    .as_ref()
                    .map(|cp| cp.pending_tool_call_id.as_str())
                    .unwrap_or(""),
                checkpoint.as_ref(),
            )
            .await
        };
        match observed {
            Ok(observation) => {
                // The note says the approved tool ran; when its result is an error it did not do
                // its work, and the model must not be told otherwise.
                let resume_nudge = resume_nudge_after(&tool_name, &observation, resume_nudge);
                if playbook_started(&tool_name, &observation) && offer_start {
                    playbook_procedure = procedure_without_status(&observation);
                    offer_start = false;
                    tools_list = rebuild_tools_list(
                        &skills_runtime,
                        &allowed_names,
                        &playbook_procedure,
                        offer_start,
                        termination,
                    );
                    system_prompt = build_agent_system_prompt(
                        &ws_root,
                        inputs,
                        &tools_list,
                        &skills_catalog,
                        &skills_runtime.extensions_text(),
                    );
                }
                if let Some(cp) = checkpoint {
                    apply_hitl_checkpoint_restore(
                        &cp,
                        &system_prompt,
                        observation,
                        &resume_nudge,
                        &tool_name,
                        &skills_runtime,
                        &mut messages,
                        &mut total_tokens,
                        &mut nudge_count,
                        &mut last_answer_candidate,
                        &mut skill_tool_called,
                        &mut force_finalize,
                        &mut iteration_cap,
                        &mut iterations_used_before_loop,
                        &mut max_iterations,
                        &mut claim_reprompted,
                    );
                } else {
                    apply_hitl_resume_fallback(
                        &system_prompt,
                        &instruction,
                        &tool_name,
                        &tool_args_json,
                        observation,
                        &resume_nudge,
                        &mut messages,
                        &mut last_answer_candidate,
                        &mut force_finalize,
                    );
                }
            }
            Err(failed_outcome) => return Ok(failed_outcome),
        }
    }

    // A task picked up after a crash may start with the round the model had already decided.
    let mut resumed_round: Option<crate::context_window::DecidedRound> = None;
    let mut resumed_answers: std::collections::HashMap<String, ChatMessage> =
        std::collections::HashMap::new();
    if !hitl_resumed {
        match load_thread_context(lease, inputs).await {
            Ok(Some(ctx)) if ctx.crash.is_some() => {
                let crash = ctx.crash.clone().unwrap_or_else(|| unreachable!());
                warn_on_prompt_digest_change(&ctx, &system_prompt, &tools_list);
                let (history, decided) = crate::context_window::restore_history_for_crash(&ctx);
                // The skills the dead guest had loaded are loaded again from the conversation.
                reload_checkpoint_skills(
                    &mut skills_runtime,
                    &crate::context_window::loaded_skills_in(&history),
                );
                tools_list = rebuild_tools_list(
                    &skills_runtime,
                    &allowed_names,
                    &playbook_procedure,
                    offer_start,
                    termination,
                );
                if tools_list.is_empty() {
                    tools_list = with_task_complete(
                        filter_tools_by_advertised(
                            crate::primitives::builtin_tool_specs(),
                            &allowed_names,
                        ),
                        termination,
                    );
                }
                system_prompt = build_agent_system_prompt(
                    &ws_root,
                    inputs,
                    &tools_list,
                    &skills_catalog,
                    &skills_runtime.extensions_text(),
                );
                // The conversation is the logged one: the task's own prompt is already in it.
                let mut restored = vec![ChatMessage {
                    role: "system".to_string(),
                    content: system_prompt.clone(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                }];
                restored.extend(history);
                messages = restored;
                iterations_used_before_loop = crash.state.iterations_used;
                max_iterations = remaining_iterations(iteration_cap, iterations_used_before_loop);
                total_tokens.prompt_tokens = crash.state.prompt_tokens;
                total_tokens.completion_tokens = crash.state.completion_tokens;
                total_tokens.total_tokens = crash.state.total_tokens;
                inbox.resume_at(crash.state.inbox_seq);
                resumed_round = decided;
            }
            Ok(Some(ctx)) => {
                warn_on_prompt_digest_change(&ctx, &system_prompt, &tools_list);
                let mut restored = vec![messages[0].clone()];
                restored.extend(crate::context_window::restore_history(&ctx));
                restored.push(messages[1].clone());
                messages = restored;
            }
            Ok(None) => {}
            Err(message) => return Ok(fail_task(lease, 13, message, None)),
        }
    }
    let mut rounds_since_clear = 0usize;
    // Turn commits: asked after a round that ran a tool that may write files.
    // The host answering "not available" (code 12) switches this off.
    let mut turn_commits_enabled = true;
    // On a microVM the workspace filesystem is frozen around each commit request.
    let freezer = crate::freeze::drive9_freezer();

    // Compaction (AR-2.10). The window comes from the host; the provider's
    // reported prompt tokens feed the estimate; clearing never rewrites the
    // part the host recorded before a compaction.
    let context_window = context_window_from_inputs(inputs);
    let mut usage_tracker = crate::compaction::UsageTracker::default();
    let mut clear_floor = if messages
        .get(1)
        .is_some_and(vam_spec::compaction::is_summary_message)
    {
        messages.len()
    } else {
        0
    };
    let mut compaction_backoff = 0usize;
    let mut forced_compaction: Option<&'static str> = None;
    let mut limit_retry_used = false;

    'rounds: for iteration in 0..max_iterations {
        let start_iter = std::time::Instant::now();
        // Each round gets a fresh stepTimeout budget.
        let mut budget = RoundBudget::new(step_timeout);
        let round = iterations_used_before_loop + iteration + 1;
        // `tools.examples`: the descriptions the model sees; borrowed unchanged when off.
        let tools_view = crate::tool_examples::for_request(&tools_list, harness.tools.examples);
        // vamd always writes `inputs.model` (a model is a preference, never a gate), so the
        // fallback below is not reached on a run it created. It stays for a guest started
        // without a host (hermetic dev and unit tests).
        let mut model_pref = model_preference_from_inputs(inputs);
        if model_pref.is_none() {
            model_pref = Some(vam_spec::v1::ModelPreference {
                provider: "mock".to_string(),
                model_name: "mock-model".to_string(),
                temperature: 0.7,
                top_p: 1.0,
                max_completion_tokens: 0,
            });
        }

        let (text_response, tool_calls) = if let Some(decided) = resumed_round.take() {
            // The model had decided this round before the guest died (E10 Phase 4): act on it.
            // No steer, no compaction and no request come before its calls.
            resumed_answers = decided.answered;
            (decided.text, decided.calls)
        } else {
            // Steers: everything the user sent since the last boundary arrives as one
            // user message, after the previous round's turn commit and before the
            // compaction check. An empty inbox changes nothing.
            if let Some(text) = inbox.take(lease).await {
                apply_steer(&mut messages, text, &mut loop_detector);
            }

            // Compaction decision: before every model call, at the start of a
            // follow-up and after every tool round, one trigger.
            let mut compacted_now = false;
            compaction_backoff = compaction_backoff.saturating_sub(1);
            let fixed_bytes = system_prompt.len()
                + tools_view
                    .iter()
                    .map(prost::Message::encoded_len)
                    .sum::<usize>();
            let trigger = forced_compaction.take().or_else(|| {
                if compaction_backoff == 0 {
                    crate::compaction::decide(
                        &usage_tracker.estimate(&messages, fixed_bytes),
                        context_window,
                        u64::from(harness.context.compaction.trigger_percent),
                    )
                } else {
                    None
                }
            });
            if let Some(reason) = trigger {
                let task_user = messages
                    .iter()
                    .rposition(|m| m.role == "user" && m.content == instruction);
                if let Some(cut) = crate::compaction::plan_cut(
                    &messages,
                    task_user,
                    context_window,
                    harness.context.compaction.keep_rounds as usize,
                ) {
                    let todo_plan = read_todo_plan(&ws_root);
                    match run_compaction(
                        lease,
                        &client,
                        &model_pref,
                        &messages,
                        &tools_view,
                        cut,
                        reason,
                        todo_plan.as_deref(),
                        fixed_bytes,
                        &mut budget,
                        &mut total_tokens,
                    )
                    .await
                    {
                        CompactionResult::Compacted(new_messages) => {
                            messages = new_messages;
                            usage_tracker.reset();
                            clear_floor = messages.len();
                            rounds_since_clear = 0;
                            compacted_now = true;
                        }
                        CompactionResult::Skipped(why) => {
                            tracing::warn!("context compaction skipped: {why}");
                            compaction_backoff = crate::compaction::RETRY_AFTER_ROUNDS;
                        }
                        CompactionResult::Failed(outcome) => return Ok(outcome),
                    }
                }
            }

            // Batched clearing of old tool results: rarely, never per round, never on
            // a compaction round, and only after the kept tail of a compaction.
            rounds_since_clear += 1;
            if !compacted_now
                && rounds_since_clear >= harness.context.clearing.every_rounds as usize
                && crate::context_window::clear_old_results_from(
                    &mut messages,
                    clear_floor,
                    extract_reread_path,
                ) > 0
            {
                rounds_since_clear = 0;
                usage_tracker.reset();
            }

            // Reuse the Cedar-filtered tools_list for every ReAct iteration
            let request_len = messages.len();
            let mut reported_prompt_tokens = 0i64;
            let recitation = harness
                .context
                .recite_todo
                .then(|| read_todo_plan(&ws_root))
                .flatten();
            let inf_req =
                build_inference_request(&model_pref, &messages, &tools_view, recitation.as_deref());
            let resample_req = (harness.adapter.resample_on_invalid > 0).then(|| inf_req.clone());

            // Call broker for inference stream. The model call is charged to the round.
            let start_infer = std::time::Instant::now();
            let mut rx = match budget.guest_step(client.infer(inf_req)).await {
                Ok(started) => started?,
                Err(_) => {
                    return Ok(step_timeout_outcome(
                        lease,
                        step_timeout_secs,
                        round,
                        &total_tokens,
                    ));
                }
            };

            let mut text_response = String::new();
            let mut tool_calls = Vec::new();

            loop {
                let next = match budget.guest_step(rx.recv()).await {
                    Ok(next) => next,
                    Err(_) => {
                        return Ok(step_timeout_outcome(
                            lease,
                            step_timeout_secs,
                            round,
                            &total_tokens,
                        ));
                    }
                };
                let Some(chunk_res) = next else {
                    break;
                };
                let chunk = chunk_res?;
                if let Some(err) = chunk.error {
                    // The provider says the prompt does not fit: compact once and
                    // ask again once.
                    if err.message.starts_with(crate::compaction::CONTEXT_TOO_LONG)
                        && !limit_retry_used
                    {
                        limit_retry_used = true;
                        forced_compaction = Some(vam_spec::compaction::REASON_PROVIDER_LIMIT);
                        continue 'rounds;
                    }
                    // Host-side inference failures (missing API key, unroutable
                    // model, exhausted budget) are not recoverable by retrying.
                    return Ok(inference_failed_outcome(lease, err));
                }
                if !chunk.delta.is_empty() {
                    text_response.push_str(&chunk.delta);
                }
                if !chunk.tool_calls.is_empty() {
                    tool_calls.extend(chunk.tool_calls);
                }
                if let Some(ref usage) = chunk.usage {
                    reported_prompt_tokens = i64::from(usage.prompt_tokens);
                    total_tokens.prompt_tokens += usage.prompt_tokens;
                    total_tokens.completion_tokens += usage.completion_tokens;
                    total_tokens.total_tokens += usage.total_tokens;
                    total_tokens.cached_tokens += usage.cached_tokens;
                    total_tokens.cache_creation_tokens += usage.cache_creation_tokens;
                }
            }

            usage_tracker.note(reported_prompt_tokens, request_len);
            limit_retry_used = false;
            let infer_dur = start_infer.elapsed();
            let mut infer_attrs = std::collections::HashMap::new();
            if let Some(ref mp) = model_pref {
                infer_attrs.insert("gen_ai.request.model".to_string(), mp.model_name.clone());
            }
            infer_attrs.insert("iteration".to_string(), iteration.to_string());
            crate::telemetry::global_telemetry().record_span(
                "agent.inference",
                infer_dur,
                trace_context.cloned(),
                infer_attrs,
            );

            if !text_response.trim().is_empty() {
                last_answer_candidate = text_response.clone();
            }

            if tool_calls.is_empty() && !force_finalize {
                tool_calls = extract_tool_calls_from_text(&text_response);
                if !tool_calls.is_empty() {
                    text_response.clear();
                }
            } else if force_finalize {
                let text_has_calls = !extract_tool_calls_from_text(&text_response).is_empty();
                if !tool_calls.is_empty() && (text_response.trim().is_empty() || text_has_calls) {
                    // Keep the model's message as it wrote it, close its calls, and
                    // ask again: the log then sees an extension, not an edit.
                    messages.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: text_response.clone(),
                        tool_calls: tool_calls.clone(),
                        tool_call_id: String::new(),
                    });
                    for tc in &tool_calls {
                        messages.push(ChatMessage {
                            role: "tool".to_string(),
                            content:
                                "Not run: no more tool calls are allowed now. Answer with the \
                                  results you already have."
                                    .to_string(),
                            tool_calls: vec![],
                            tool_call_id: tc.id.clone(),
                        });
                    }
                    messages.push(ChatMessage {
                        role: "user".to_string(),
                        content: FORCE_FINALIZE_NUDGE.to_string(),
                        tool_calls: vec![],
                        tool_call_id: String::new(),
                    });
                    continue;
                }
                tool_calls.clear();
            }

            // `adapter.resampleOnInvalid`: a reply with a call the checks refuse is not shown to
            // the model; the same request is sent once more, once (see `resample`).
            if let Some(req) = resample_req.as_ref().filter(|_| {
                !force_finalize
                    && tool_calls.iter().any(|tc| {
                        checked_tool_args(
                            &tc.name,
                            &tc.arguments,
                            harness.calls.validate,
                            &tools_list,
                        )
                        .is_err()
                    })
            }) {
                if let Ok(Some(again)) = budget
                    .guest_step(crate::resample::again(&client, req))
                    .await
                {
                    for usage in &again.usage {
                        total_tokens.prompt_tokens += usage.prompt_tokens;
                        total_tokens.completion_tokens += usage.completion_tokens;
                        total_tokens.total_tokens += usage.total_tokens;
                        total_tokens.cached_tokens += usage.cached_tokens;
                        total_tokens.cache_creation_tokens += usage.cache_creation_tokens;
                    }
                    (text_response, tool_calls) = (again.text, again.tool_calls);
                    if tool_calls.is_empty() {
                        tool_calls = extract_tool_calls_from_text(&text_response);
                        if !tool_calls.is_empty() {
                            text_response.clear();
                        }
                    }
                }
            }

            (text_response, tool_calls)
        };

        if tool_calls.is_empty() {
            // A round without tool calls breaks every loop pattern.
            if let Some(detector) = loop_detector.as_mut() {
                detector.clear();
            }
        }
        if !tool_calls.is_empty() {
            nudge_count = 0;
            // Append assistant tool_calls message
            messages.push(ChatMessage {
                role: "assistant".to_string(),
                content: text_response.clone(),
                tool_calls: tool_calls.clone(),
                tool_call_id: String::new(),
            });
            let round_may_write = tool_calls
                .iter()
                .any(|tc| vam_spec::thread_context::tool_may_write_files(&tc.name));
            let results_from = messages.len();

            for tc in tool_calls {
                // A result the log already holds is put back, not run again.
                if let Some(done) = resumed_answers.remove(&tc.id) {
                    messages.push(done);
                    continue;
                }
                let tool_name = &tc.name;
                let args_json = &tc.arguments;

                // Arguments are never coerced to `{}`: unparsable or non-object
                // arguments, and (with `calls.validate`) a schema violation, are a
                // typed error to the model. The call does not reach the PEP or the tool.
                let mut args_struct = Struct::default();
                let args_canonical: String;
                let parsed =
                    checked_tool_args(tool_name, args_json, harness.calls.validate, &tools_list);
                match parsed {
                    Ok(val) => {
                        invalid_streak = (String::new(), 0);
                        args_canonical = val.to_string();
                        if let Some(prost_types::Value {
                            kind: Some(prost_types::value::Kind::StructValue(s)),
                        }) = mapi_val(val)
                        {
                            args_struct = s;
                        }
                    }
                    Err(call_error) => {
                        let message = call_error.message().to_string();
                        append_tool_observation(&ws_root, tool_name, "invalid_arguments", &message);
                        messages.push(ChatMessage {
                            role: "tool".to_string(),
                            content: message.clone(),
                            tool_calls: vec![],
                            tool_call_id: tc.id.clone(),
                        });
                        if let Some(detector) = loop_detector.as_mut() {
                            detector.observe(tool_name, args_json.trim(), &message);
                        }
                        if invalid_streak.0 == *tool_name {
                            invalid_streak.1 += 1;
                        } else {
                            invalid_streak = (tool_name.clone(), 1);
                        }
                        let budget = harness.calls.repair_budget;
                        if budget > 0 && invalid_streak.1 >= budget {
                            return Ok(fail_task(
                                lease,
                                3,
                                format!(
                                    "{} {} invalid calls in a row to {}",
                                    vam_spec::call_check::REPAIR_BUDGET_PREFIX,
                                    invalid_streak.1,
                                    tool_name
                                ),
                                Some(total_tokens.clone()),
                            ));
                        }
                        continue;
                    }
                }

                if tool_name == skills::TASK_COMPLETE_TOOL_NAME
                    && termination == Termination::ExplicitClaim
                {
                    // Guest intrinsic: no PEP call, no host tool. The summary goes
                    // through the same finish check as a text-only reply.
                    let summary = args_struct
                        .fields
                        .get("summary")
                        .and_then(|v| match &v.kind {
                            Some(prost_types::value::Kind::StringValue(text)) => {
                                Some(text.trim().to_string())
                            }
                            _ => None,
                        })
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| last_answer_candidate.trim().to_string());
                    let answer = finalize_answer(&summary);
                    // A steer that arrived while the task finished keeps it going.
                    if iteration + 1 < max_iterations && inbox.peek(lease).await {
                        messages.push(ChatMessage {
                            role: "tool".to_string(),
                            content: STEER_BEFORE_COMPLETE.to_string(),
                            tool_calls: vec![],
                            tool_call_id: tc.id.clone(),
                        });
                        continue;
                    }
                    match decide_playbook_finish(
                        lease,
                        answer,
                        &total_tokens,
                        &playbook_procedure,
                        &ws_root,
                        iteration + 1 < max_iterations && !force_finalize,
                    )
                    .await
                    {
                        FinishChoice::Commit(outcome) => {
                            return Ok(record_claim(outcome, termination, true));
                        }
                        FinishChoice::Continue(observation) => {
                            messages.push(ChatMessage {
                                role: "tool".to_string(),
                                content: observation,
                                tool_calls: vec![],
                                tool_call_id: tc.id.clone(),
                            });
                            continue;
                        }
                    }
                }

                let script_digest_opt = resolve_script_digest(
                    tool_name,
                    &args_struct,
                    &skills_runtime,
                    &playbook_procedure,
                );

                // Request authorization from host PEP
                let auth_res = crate::pep::request_tool_authorization(
                    lease,
                    tool_name,
                    Some(&args_struct),
                    script_digest_opt.as_deref(),
                    &tc.id,
                )
                .await;
                let observation_disposition;
                // A call this task already made (a resume after a crash): the host answers it
                // from its journal, with no token and nothing to execute.
                let mut replayed_output: Option<String> = None;
                let capability_token = match auth_res {
                    Ok(resp) => {
                        if resp.disposition
                            == vam_spec::v1::tool_intent_response::Disposition::Denied as i32
                        {
                            observation_disposition = "denied";
                            let diag = resp
                                .error
                                .as_ref()
                                .map(|e| e.message.clone())
                                .unwrap_or_else(|| {
                                    "Tool execution denied by host policy".to_string()
                                });
                            append_tool_observation(
                                &ws_root,
                                tool_name,
                                observation_disposition,
                                &diag,
                            );
                            if let Some(detector) = loop_detector.as_mut() {
                                detector.observe(
                                    tool_name,
                                    &args_canonical,
                                    &format!("Error: denied by security policy: {diag}"),
                                );
                            }
                            messages.push(ChatMessage {
                                role: "tool".to_string(),
                                content: denied_observation(tool_name, &diag),
                                tool_calls: vec![],
                                tool_call_id: tc.id.clone(),
                            });
                            continue;
                        }
                        if resp.disposition
                            == vam_spec::v1::tool_intent_response::Disposition::Suspended as i32
                        {
                            observation_disposition = "suspended";
                            let req_id = resp
                                .approval_request_id
                                .as_ref()
                                .map(|r| r.urn.as_str())
                                .unwrap_or("pending");
                            append_tool_observation(
                                &ws_root,
                                tool_name,
                                observation_disposition,
                                &format!("approval {}", req_id),
                            );
                            let pending_tool_call_id = tc.id.clone();
                            let run_urn = lease
                                .run_id
                                .as_ref()
                                .map(|r| r.urn.clone())
                                .unwrap_or_default();
                            // Only what the host cannot see: the results of the
                            // calls that ran after the last model request.
                            let tail_start = messages
                                .iter()
                                .rposition(|m| m.role == "assistant" && !m.tool_calls.is_empty())
                                .map(|i| i + 1)
                                .unwrap_or(messages.len());
                            let tail_record = vam_spec::thread_context::SuspendTail {
                                schema_version: vam_spec::thread_context::SUSPEND_TAIL_SCHEMA,
                                run_id: run_urn.clone(),
                                lifecycle_generation: lease.lifecycle_generation,
                                kind: "suspend".to_string(),
                                messages: messages[tail_start..]
                                    .iter()
                                    .filter(|m| m.role == "tool")
                                    .map(crate::context_window::chat_to_wire)
                                    .collect(),
                                pending: vam_spec::thread_context::PendingCall {
                                    tool_call_id: pending_tool_call_id.clone(),
                                    tool_name: tool_name.clone(),
                                    args_json: args_json.clone(),
                                },
                                state: vam_spec::thread_context::LoopState {
                                    loaded_skill_ids: skills_runtime.loaded_skill_ids(),
                                    iterations_used: iterations_used_before_loop + iteration + 1,
                                    max_iterations: iteration_cap,
                                    prompt_tokens: total_tokens.prompt_tokens,
                                    completion_tokens: total_tokens.completion_tokens,
                                    cached_tokens: total_tokens.cached_tokens,
                                    total_tokens: total_tokens.total_tokens,
                                    nudge_count,
                                    skill_tool_called,
                                    claim_reprompted,
                                    last_answer_candidate: crate::context_window::cap_text(
                                        &last_answer_candidate,
                                        vam_spec::thread_context::MAX_LAST_ANSWER_BYTES,
                                    ),
                                    inbox_seq: inbox.last_seq,
                                },
                            };
                            let tail_bytes = serde_json::to_vec(&tail_record).unwrap_or_default();
                            let tail_digest = vam_spec::thread_context::sha256_hex(&tail_bytes);
                            let checkpoint_digest = tail_digest.clone();
                            if let Err(tail_err) =
                                crate::pep::upload_run_checkpoint(lease, &tail_bytes, &tail_digest)
                                    .await
                            {
                                // The host would not take the tail (for example its log has
                                // a gap). The conversation lives in the host's thread log
                                // only (decision 23): the approval fails clearly instead of
                                // uploading a second copy.
                                tracing::warn!("suspend tail refused: {tail_err}");
                                return Ok(fail_task(
                                    lease,
                                    9,
                                    format!(
                                        "{STORAGE_COMMIT_FAILED}: the host could not record the \
                                         suspended task: {tail_err}"
                                    ),
                                    Some(total_tokens.clone()),
                                ));
                            }
                            let continuation =
                                workspace_checkpoint_artifact(&ws_root, &checkpoint_digest);
                            return Ok(TaskOutcome {
                                lease: Some(lease.clone()),
                                state: TaskOutcomeState::Suspended as i32,
                                output: None,
                                output_artifacts: vec![],
                                continuation: Some(continuation),
                                evidence: None,
                                observed_usage: Some(total_tokens),
                                error: None,
                            });
                        }
                        if resp.disposition
                            == vam_spec::v1::tool_intent_response::Disposition::Replayed as i32
                        {
                            observation_disposition = "replayed";
                            replayed_output = Some(match resp.replayed {
                                Some(recorded) => render_host_response(
                                    recorded,
                                    &ws_root,
                                    harness.calls.result_cap as usize,
                                ),
                                None => "tool_error: the host replayed this call without its \
                                         recorded answer"
                                    .to_string(),
                            });
                            None
                        } else {
                            observation_disposition = "authorized";
                            resp.capability_token
                        }
                    }
                    Err(e) => {
                        observation_disposition = "pep_error";
                        append_tool_observation(
                            &ws_root,
                            tool_name,
                            observation_disposition,
                            &e.to_string(),
                        );
                        #[cfg(test)]
                        {
                            tracing::warn!(
                                "PEP authorization request failed (test-only fallback): {}",
                                e
                            );
                            None
                        }
                        #[cfg(not(test))]
                        {
                            tracing::error!(
                                "PEP authorization request failed; denying tool by default: {}",
                                e
                            );
                            messages.push(ChatMessage {
                                role: "tool".to_string(),
                                content: format!(
                                    "Error: Tool '{}' was denied because the host security authority could not be reached: {}",
                                    tool_name, e
                                ),
                                tool_calls: vec![],
                                tool_call_id: tc.id.clone(),
                            });
                            continue;
                        }
                    }
                };

                append_tool_observation(&ws_root, tool_name, observation_disposition, "executing");

                let tool_output = if let Some(recorded) = replayed_output {
                    recorded
                } else if tool_name == "load_skill" {
                    match execute_load_skill(&mut skills_runtime, &args_struct) {
                        Ok(msg) => {
                            tools_list = rebuild_tools_list(
                                &skills_runtime,
                                &allowed_names,
                                &playbook_procedure,
                                offer_start,
                                termination,
                            );
                            system_prompt = build_agent_system_prompt(
                                &ws_root,
                                inputs,
                                &tools_list,
                                &skills_catalog,
                                &skills_runtime.extensions_text(),
                            );
                            if let Some(system_msg) = messages.first_mut() {
                                system_msg.content = system_prompt.clone();
                            }
                            msg
                        }
                        Err(e) => format!("tool_error: {}", e),
                    }
                } else if crate::outbox::is_tool(tool_name) {
                    crate::outbox::execute(
                        lease,
                        tool_name,
                        &args_struct,
                        capability_token.as_ref(),
                    )
                    .await
                } else if is_host_exec_tool(tool_name, &skills_runtime, &playbook_procedure) {
                    match capability_token.as_ref() {
                        Some(token) => {
                            execute_host_tool_call(
                                lease,
                                tool_name,
                                &args_struct,
                                token,
                                &ws_root,
                                harness.calls.result_cap as usize,
                                &tc.id,
                            )
                            .await
                        }
                        None => format!(
                            "tool_error: host tool '{}' requires an authorized capability token",
                            tool_name
                        ),
                    }
                } else if is_mcp_tool(tool_name, &skills_runtime) {
                    format!(
                        "tool_error: MCP tool '{}' is not implemented in this runtime",
                        tool_name
                    )
                } else {
                    let lease_urn = lease
                        .lease_id
                        .as_ref()
                        .map(|id| id.urn.clone())
                        .unwrap_or_default();
                    // Guest-local execution is charged to the round's stepTimeout.
                    let tr_res = budget
                        .guest_step(crate::exec::execute_tool_subprocess_with_token(
                            tool_name,
                            Some(&args_struct),
                            Duration::from_secs(60),
                            Some(lease_urn.clone()),
                            capability_token.as_ref(),
                            None,
                            trace_context,
                        ))
                        .await;
                    let tr_res = match tr_res {
                        Ok(tr_res) => tr_res,
                        Err(_) => {
                            // Kill the timed-out tool's process group; the task is failing.
                            crate::exec::cancel_active_lease(&lease_urn);
                            return Ok(step_timeout_outcome(
                                lease,
                                step_timeout_secs,
                                round,
                                &total_tokens,
                            ));
                        }
                    };

                    match tr_res {
                        Ok(tr) => format_tool_subprocess_output(&tr),
                        Err(e) => {
                            format!("error: Tool execution invocation failed: {}", e)
                        }
                    }
                };

                if playbook_started(tool_name, &tool_output) && offer_start {
                    // A playbook is now bound to this task: the step tools
                    // appear and the finish check applies. One accepted
                    // prompt-cache miss.
                    playbook_procedure = procedure_without_status(&tool_output);
                    offer_start = false;
                    tools_list = rebuild_tools_list(
                        &skills_runtime,
                        &allowed_names,
                        &playbook_procedure,
                        offer_start,
                        termination,
                    );
                    system_prompt = build_agent_system_prompt(
                        &ws_root,
                        inputs,
                        &tools_list,
                        &skills_catalog,
                        &skills_runtime.extensions_text(),
                    );
                    if let Some(system_msg) = messages.first_mut() {
                        system_msg.content = system_prompt.clone();
                    }
                }
                if let Some(detector) = loop_detector.as_mut() {
                    detector.observe(tool_name, &args_canonical, &tool_output);
                }
                messages.push(ChatMessage {
                    role: "tool".to_string(),
                    content: tool_output.clone(),
                    tool_calls: vec![],
                    tool_call_id: tc.id.clone(),
                });
                append_tool_observation(
                    &ws_root,
                    tool_name,
                    "completed",
                    &tool_output.chars().take(256).collect::<String>(),
                );
                if !tool_output.trim().is_empty() {
                    last_answer_candidate = tool_output;
                }
                if skills_runtime
                    .loaded_tools()
                    .iter()
                    .any(|tool| tool.name == *tool_name)
                {
                    skill_tool_called = true;
                }
            }
            // The results of one round together stay within a share of the window,
            // so the context can never jump from under the trigger past what a
            // summary request can carry.
            crate::compaction::cap_round_results(&mut messages[results_from..], context_window);
            if let Some(detector) = loop_detector.as_mut() {
                match detector.end_round() {
                    crate::loop_detect::Verdict::Continue => {}
                    crate::loop_detect::Verdict::Warn(_, observation) => {
                        messages.push(ChatMessage {
                            role: "user".to_string(),
                            content: observation,
                            tool_calls: vec![],
                            tool_call_id: String::new(),
                        });
                    }
                    crate::loop_detect::Verdict::Stop(kind, detail) => {
                        return Ok(fail_task(
                            lease,
                            10,
                            format!(
                                "{} {}: {}; the task was stopped",
                                vam_spec::call_check::LOOP_STOP_PREFIX,
                                kind.name(),
                                detail
                            ),
                            Some(total_tokens.clone()),
                        ));
                    }
                }
            }
            if round_may_write && turn_commits_enabled {
                if let Err(message) = request_turn_commit(
                    &freezer,
                    lease,
                    &messages,
                    round as u32,
                    &mut turn_commits_enabled,
                )
                .await
                {
                    return Ok(fail_task(lease, 9, message, Some(total_tokens.clone())));
                }
            }
            let mut iter_attrs = std::collections::HashMap::new();
            iter_attrs.insert("iteration".to_string(), iteration.to_string());
            crate::telemetry::global_telemetry().record_span(
                "agent.react_iteration",
                start_iter.elapsed(),
                trace_context.cloned(),
                iter_attrs,
            );
        } else {
            if force_finalize
                && (text_response.trim().is_empty()
                    || !extract_tool_calls_from_text(&text_response).is_empty())
            {
                if !text_response.trim().is_empty() {
                    // Text that only looks like a call: keep it as written.
                    messages.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: text_response.clone(),
                        tool_calls: vec![],
                        tool_call_id: String::new(),
                    });
                }
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: FORCE_FINALIZE_NUDGE.to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                });
                continue;
            }
            if text_response.trim().is_empty() {
                // The only nudge: a neutral one for an empty reply. It never names a tool.
                let empty_nudge = empty_response_nudge();
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: empty_nudge.to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                });
                continue;
            }
            if termination == Termination::ExplicitClaim
                && !claim_reprompted
                && !force_finalize
                && iteration + 1 < max_iterations
            {
                // A text-only reply is not the end under an explicit claim: ask once.
                claim_reprompted = true;
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: text_response.clone(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                });
                messages.push(ChatMessage {
                    role: "user".to_string(),
                    content: claim_reprompt().to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                });
                continue;
            }
            let mut iter_attrs = std::collections::HashMap::new();
            iter_attrs.insert("iteration".to_string(), iteration.to_string());
            crate::telemetry::global_telemetry().record_span(
                "agent.react_iteration",
                start_iter.elapsed(),
                trace_context.cloned(),
                iter_attrs,
            );
            if iteration + 1 < max_iterations && inbox.peek(lease).await {
                messages.push(ChatMessage {
                    role: "assistant".to_string(),
                    content: text_response.clone(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                });
                continue;
            }
            let answer_text = text_response.clone();
            // After a denied call the task was told to answer without more tools, so a finish
            // check that still lacks steps must not send it back to work: it would contradict
            // that instruction forever. The answer is committed and the host's own check ends
            // the run FAILED, naming the missing steps.
            match decide_playbook_finish(
                lease,
                text_response,
                &total_tokens,
                &playbook_procedure,
                &ws_root,
                iteration + 1 < max_iterations && !force_finalize,
            )
            .await
            {
                FinishChoice::Commit(outcome) => {
                    return Ok(record_claim(outcome, termination, false));
                }
                FinishChoice::Continue(observation) => {
                    messages.push(ChatMessage {
                        role: "assistant".to_string(),
                        content: answer_text,
                        tool_calls: vec![],
                        tool_call_id: String::new(),
                    });
                    messages.push(ChatMessage {
                        role: "user".to_string(),
                        content: observation,
                        tool_calls: vec![],
                        tool_call_id: String::new(),
                    });
                    continue;
                }
            }
        }
    }

    // The iteration cap is exhausted. Ask once more, then commit. The host
    // records FAILED when the playbook is still incomplete.
    match decide_playbook_finish(
        lease,
        finalize_answer(&last_answer_candidate),
        &total_tokens,
        &playbook_procedure,
        &ws_root,
        false,
    )
    .await
    {
        FinishChoice::Commit(outcome) => Ok(record_claim(outcome, termination, false)),
        FinishChoice::Continue(_) => Ok(record_claim(
            succeed_with_answer(lease, finalize_answer(&last_answer_candidate), total_tokens),
            termination,
            false,
        )),
    }
}

/// Tool result for a `task_complete` that a steer interrupted.
const STEER_BEFORE_COMPLETE: &str =
    "Not completed: the user sent a new message. Read it and continue the task.";

/// Appends the steers as one user message on the main lane. The user changed the
/// task, so the loop detector starts over (Q12).
fn apply_steer(
    messages: &mut Vec<ChatMessage>,
    text: String,
    loop_detector: &mut Option<crate::loop_detect::Detector>,
) {
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: text,
        tool_calls: vec![],
        tool_call_id: String::new(),
    });
    if let Some(detector) = loop_detector.as_mut() {
        detector.clear();
    }
}

/// Prefix of a failed-task message when the host's storage could not record a
/// turn commit or a suspend. The host copies it into the run's `reason`.
pub(crate) const STORAGE_COMMIT_FAILED: &str = "storage_commit_failed";

/// Asks the host to record a turn commit for the round that just ran. `Err`
/// carries the failure message only when the host's storage failed (the task
/// then fails and its work is kept); every other refusal is logged and the task
/// goes on, since a missing commit only makes a stop roll back further.
///
/// On a microVM the host copies the guest's disk image while this waits, so the
/// filesystem is synced and frozen around the request and thawed on the reply,
/// an error, a timeout or a dropped future (see [`crate::freeze`]). If the freeze
/// fails the round's commit is skipped: a copy of a filesystem nobody froze is
/// not a commit.
async fn request_turn_commit(
    freezer: &std::sync::Arc<dyn crate::freeze::FsFreezer>,
    lease: &ExecutionLease,
    messages: &[ChatMessage],
    round: u32,
    enabled: &mut bool,
) -> Result<(), String> {
    let start = messages
        .iter()
        .rposition(|m| m.role == "assistant")
        .map(|i| i + 1)
        .unwrap_or(messages.len());
    let tail: Vec<vam_spec::thread_context::WireMessage> = messages[start..]
        .iter()
        .take_while(|m| m.role == "tool")
        .map(crate::context_window::chat_to_wire)
        .collect();
    let digest = vam_spec::thread_context::tail_digest(&tail);
    let answered = crate::freeze::around_frozen(freezer, crate::freeze::FREEZE_WATCHDOG, || {
        crate::pep::commit_turn(lease, round, &digest, true)
    })
    .await;
    let answered = match answered {
        crate::freeze::Guarded::NotApplicable(answer) | crate::freeze::Guarded::Held(answer) => {
            answer
        }
        crate::freeze::Guarded::Skipped(why) => {
            eprintln!(
                "[guest-agent] turn commit skipped for round {round}: the workspace could not be frozen ({why})"
            );
            return Ok(());
        }
    };
    match answered {
        Ok(crate::pep::CommitTurnOutcome::Accepted) => Ok(()),
        Ok(crate::pep::CommitTurnOutcome::Unavailable) => {
            *enabled = false;
            Ok(())
        }
        Ok(crate::pep::CommitTurnOutcome::Refused(message)) => {
            if message.starts_with(STORAGE_COMMIT_FAILED) {
                Err(message)
            } else {
                tracing::warn!("turn commit refused for round {round}: {message}");
                Ok(())
            }
        }
        Err(e) => {
            tracing::warn!("turn commit request failed for round {round}: {e}");
            Ok(())
        }
    }
}

const FORCE_FINALIZE_NUDGE: &str =
    "Answer the user now using the tool results you already have. Do not call more tools.";

/// Fetches the thread context the host named for this task, if any.
async fn load_thread_context(
    lease: &ExecutionLease,
    inputs: Option<&Struct>,
) -> Result<Option<vam_spec::thread_context::ThreadContext>, String> {
    let Some(value) = inputs.and_then(|s| {
        s.fields
            .get(vam_spec::thread_context::THREAD_HEAD_DIGEST_INPUT)
    }) else {
        return Ok(None);
    };
    let digest = match value.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(d)) if !d.is_empty() => d.as_str(),
        _ => return Err("thread context digest is empty or not a string".to_string()),
    };
    let bytes = crate::pep::fetch_thread_context(lease, digest)
        .await
        .map_err(|e| format!("thread context {digest} could not be fetched: {e}"))?;
    let ctx = vam_spec::thread_context::ThreadContext::from_verified_bytes(&bytes, digest)
        .map_err(|e| format!("thread context {digest}: {e}"))?;
    let run_id = lease.run_id.as_ref().map(|r| r.urn.as_str()).unwrap_or("");
    if ctx.run_id != run_id {
        return Err(format!("thread context {digest} belongs to another run"));
    }
    Ok(Some(ctx))
}

/// The system prompt and tools are rebuilt here; the host logged digests of
/// what the thread's last request carried. A difference is a planned break (the
/// host records a `prompt_change` on the first request), never an error.
fn warn_on_prompt_digest_change(
    ctx: &vam_spec::thread_context::ThreadContext,
    system_prompt: &str,
    tools: &[vam_spec::v1::ToolSpec],
) {
    use vam_spec::thread_context::{sha256_hex, tools_blob};
    if let Some(logged) = ctx.system_sha256.as_deref() {
        if logged != sha256_hex(system_prompt.as_bytes()) {
            tracing::warn!("system prompt differs from the thread's logged prompt (prefix break)");
        }
    }
    if let Some(logged) = ctx.tools_sha256.as_deref() {
        let now = tools_blob(tools).map(|b| sha256_hex(b.as_bytes()));
        if now.as_deref() != Some(logged) {
            tracing::warn!("tool definitions differ from the thread's logged tools (prefix break)");
        }
    }
}

const DEFAULT_MAX_ITERATIONS: usize = 50;
const MAX_ITERATIONS_HARD_CAP: usize = 200;
const DEFAULT_STEP_TIMEOUT_SECS: u64 = 300;

/// Parse and (with `calls.validate`) schema-check one tool call's arguments.
/// Unparsable or non-object arguments are always an error, never `{}`; the schema
/// check covers the builtins, and `task_complete` for types only (today's
/// last-answer fallback for a missing summary stays).
fn checked_tool_args(
    tool_name: &str,
    raw_args: &str,
    validate: bool,
    tools_list: &[vam_spec::v1::ToolSpec],
) -> Result<serde_json::Value, vam_spec::call_check::CallError> {
    let value = vam_spec::call_check::parse_args(tool_name, raw_args)?;
    if crate::outbox::is_tool(tool_name) {
        // The host rebuilds the authorized arguments from the post, so the fields
        // must be exactly the ones it knows; the type check follows the schema.
        if validate {
            if let Some(spec) = crate::outbox::tool_specs()
                .into_iter()
                .find(|spec| spec.name == tool_name)
            {
                vam_spec::call_check::check_args(
                    tool_name,
                    &value,
                    &spec.input_schema_json,
                    false,
                )?;
            }
        }
        crate::outbox::validate_args(tool_name, &value)?;
        return Ok(value);
    }
    if validate {
        let schema = if crate::primitives::is_builtin(tool_name) {
            crate::primitives::builtin_tool_specs()
                .into_iter()
                .find(|spec| spec.name == tool_name)
                .map(|spec| (spec.input_schema_json, false))
        } else if tool_name == skills::TASK_COMPLETE_TOOL_NAME {
            tools_list
                .iter()
                .find(|spec| spec.name == tool_name)
                .map(|spec| (spec.input_schema_json.clone(), true))
        } else {
            None
        };
        if let Some((schema, waive_required)) = schema {
            vam_spec::call_check::check_args(tool_name, &value, &schema, waive_required)?;
        }
    }
    Ok(value)
}

fn remaining_iterations(cap: usize, used: usize) -> usize {
    cap.saturating_sub(used)
}

fn resolve_max_iterations(inputs: Option<&Struct>) -> usize {
    let Some(in_struct) = inputs else {
        return DEFAULT_MAX_ITERATIONS;
    };
    let Some(value) = in_struct.fields.get("max_iterations") else {
        return DEFAULT_MAX_ITERATIONS;
    };
    let parsed = match &value.kind {
        Some(prost_types::value::Kind::NumberValue(n)) if *n > 0.0 => *n as usize,
        Some(prost_types::value::Kind::StringValue(s)) => s
            .trim()
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_MAX_ITERATIONS),
        _ => DEFAULT_MAX_ITERATIONS,
    };
    parsed.min(MAX_ITERATIONS_HARD_CAP)
}

/// `stepTimeout` in whole seconds, read from `inputs.step_timeout_secs`
/// (string or number, at least 1). vamd writes it on every run.
fn resolve_step_timeout_secs(inputs: Option<&Struct>) -> u64 {
    let Some(value) = inputs.and_then(|s| s.fields.get("step_timeout_secs")) else {
        return DEFAULT_STEP_TIMEOUT_SECS;
    };
    match &value.kind {
        Some(prost_types::value::Kind::NumberValue(n)) if *n >= 1.0 => *n as u64,
        Some(prost_types::value::Kind::StringValue(s)) => s
            .trim()
            .parse::<u64>()
            .ok()
            .filter(|n| *n >= 1)
            .unwrap_or(DEFAULT_STEP_TIMEOUT_SECS),
        _ => DEFAULT_STEP_TIMEOUT_SECS,
    }
}

/// Guest-side active time for one ReAct round, bounded by `stepTimeout`.
///
/// A round is one model call plus the guest-local tool executions it starts.
/// Waits on the host (PEP authorization, host tools, the playbook finish check)
/// are not passed through the budget; the host bounds those itself.
struct RoundBudget {
    limit: Duration,
    spent: Duration,
}

impl RoundBudget {
    fn new(limit: Duration) -> Self {
        Self {
            limit,
            spent: Duration::ZERO,
        }
    }

    /// Await one guest-side step with the round's remaining time. The elapsed
    /// time is charged even when the step runs out of time.
    async fn guest_step<F: std::future::Future>(
        &mut self,
        step: F,
    ) -> Result<F::Output, tokio::time::error::Elapsed> {
        let started = std::time::Instant::now();
        let remaining = self.limit.saturating_sub(self.spent);
        let outcome = tokio::time::timeout(remaining, step).await;
        self.spent += started.elapsed();
        outcome
    }
}

/// Failed outcome with an explicit error code. `observed_usage` is set when the
/// task has already spent tokens.
fn fail_task(
    lease: &ExecutionLease,
    code: i32,
    message: String,
    observed_usage: Option<TokenUsage>,
) -> TaskOutcome {
    TaskOutcome {
        lease: Some(lease.clone()),
        state: TaskOutcomeState::Failed as i32,
        output: None,
        output_artifacts: vec![],
        continuation: None,
        evidence: None,
        observed_usage,
        error: Some(vam_spec::v1::ErrorStatus {
            code,
            message,
            details: vec![],
            retryable: false,
        }),
    }
}

/// A round ran out of `stepTimeout`. Code 4 is DEADLINE_EXCEEDED.
fn step_timeout_outcome(
    lease: &ExecutionLease,
    step_timeout_secs: u64,
    round: usize,
    total_tokens: &TokenUsage,
) -> TaskOutcome {
    fail_task(
        lease,
        4,
        format!("step exceeded stepTimeout of {step_timeout_secs}s (round {round})"),
        Some(total_tokens.clone()),
    )
}

fn finalize_answer(candidate: &str) -> String {
    let trimmed = candidate.trim();
    if trimmed.is_empty() {
        return "I reached the step limit before finishing. Please retry with a more specific request.".to_string();
    }
    trimmed.to_string()
}

const CHECK_PLAYBOOK_FINISH_TOOL: &str = "check_playbook_finish";

/// Guest fallback when a playbook finish check is incomplete and the host
/// observation is empty. The step sentence matches the host nudge (P4F-7).
fn playbook_finish_fallback_observation() -> String {
    [
        "Playbook finish is incomplete. Do not end the task yet.",
        "For each missing step: call run_step if it is bound, do the work and call step_complete if it is agentic, and wait for the human decision if it is a gate. Never invent a step's outcome.",
        vam_spec::playbook::PLAYBOOK_GATE_RULE,
        "Reply with run output that matches the playbook output schema.",
    ]
    .join("\n")
}

struct FinishReport {
    complete: bool,
    /// Only open gates are left. The host parks the run when the guest ends its turn, so the
    /// guest commits instead of asking the model again.
    parkable: bool,
    observation: String,
}

// Boxing would change the variant constructors; the enum is short-lived.
#[allow(clippy::large_enum_variant)]
enum FinishChoice {
    Commit(TaskOutcome),
    Continue(String),
}

#[cfg(test)]
type FinishProbe = std::sync::Arc<dyn Fn(&str) -> Result<FinishReport, String> + Send + Sync>;

#[cfg(test)]
static TEST_FINISH_PROBE: std::sync::Mutex<Option<FinishProbe>> = std::sync::Mutex::new(None);

/// Before committing success on a playbook run, ask the host. An incomplete
/// check becomes an observation and the loop continues while iterations remain.
/// Non-playbook runs commit immediately. The iteration cap is not raised.
async fn decide_playbook_finish(
    lease: &ExecutionLease,
    answer: String,
    total_tokens: &TokenUsage,
    playbook_procedure: &str,
    workspace_root: &std::path::Path,
    another_iteration: bool,
) -> FinishChoice {
    if playbook_procedure.trim().is_empty() {
        return FinishChoice::Commit(succeed_with_answer(lease, answer, total_tokens.clone()));
    }
    let output_json = vam_spec::playbook_run_output_json(&answer);
    match ask_playbook_finish(lease, &output_json, workspace_root).await {
        Ok(report) if report.complete || report.parkable => {
            FinishChoice::Commit(succeed_with_answer(lease, answer, total_tokens.clone()))
        }
        Ok(report) if another_iteration => {
            let observation = if report.observation.trim().is_empty() {
                playbook_finish_fallback_observation()
            } else {
                report.observation
            };
            FinishChoice::Continue(observation)
        }
        Ok(_) => FinishChoice::Commit(succeed_with_answer(lease, answer, total_tokens.clone())),
        Err(err) => {
            tracing::warn!("playbook finish probe failed; committing for the host check: {err}");
            FinishChoice::Commit(succeed_with_answer(lease, answer, total_tokens.clone()))
        }
    }
}

async fn ask_playbook_finish(
    lease: &ExecutionLease,
    output_json: &str,
    workspace_root: &std::path::Path,
) -> Result<FinishReport, String> {
    #[cfg(test)]
    if let Ok(guard) = TEST_FINISH_PROBE.lock() {
        if let Some(probe) = guard.clone() {
            drop(guard);
            return probe(output_json);
        }
    }
    let mut args = Struct::default();
    args.fields.insert(
        "output_json".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(
                output_json.to_string(),
            )),
        },
    );
    let digest = vam_spec::validation::host_tool_digest(CHECK_PLAYBOOK_FINISH_TOOL);
    let auth = crate::pep::request_tool_authorization(
        lease,
        CHECK_PLAYBOOK_FINISH_TOOL,
        Some(&args),
        Some(&digest),
        "",
    )
    .await
    .map_err(|err| err.to_string())?;
    if auth.disposition == vam_spec::v1::tool_intent_response::Disposition::Denied as i32 {
        let message = auth
            .error
            .map(|err| err.message)
            .unwrap_or_else(|| "check_playbook_finish denied".to_string());
        return Err(message);
    }
    let Some(token) = auth.capability_token else {
        return Err("check_playbook_finish returned no capability token".to_string());
    };
    let text = execute_host_tool_call(
        lease,
        CHECK_PLAYBOOK_FINISH_TOOL,
        &args,
        &token,
        workspace_root,
        MAX_TEXT_RESULT_NO_ARTIFACT,
        "",
    )
    .await;
    parse_finish_report(&text)
}

fn parse_finish_report(text: &str) -> Result<FinishReport, String> {
    let value: serde_json::Value = serde_json::from_str(text)
        .map_err(|_| format!("playbook finish probe returned non-JSON: {text}"))?;
    let complete = value
        .get("complete")
        .and_then(|flag| flag.as_bool())
        .ok_or_else(|| format!("playbook finish probe missing complete: {text}"))?;
    let observation = value
        .get("observation")
        .and_then(|item| item.as_str())
        .unwrap_or("")
        .to_string();
    let parkable = value
        .get("parkable")
        .and_then(|flag| flag.as_bool())
        .unwrap_or(false);
    Ok(FinishReport {
        complete,
        parkable,
        observation,
    })
}

/// Builds the model preference from the host-owned `model` / `model_provider`
/// inputs. An explicit provider wins over the model-name heuristic.
fn model_preference_from_inputs(inputs: Option<&Struct>) -> Option<vam_spec::v1::ModelPreference> {
    let in_struct = inputs?;
    let string_field = |key: &str| match in_struct.fields.get(key).and_then(|v| v.kind.as_ref()) {
        Some(prost_types::value::Kind::StringValue(s)) if !s.trim().is_empty() => {
            Some(s.trim().to_string())
        }
        _ => None,
    };
    let model = string_field("model")?;
    // One provider rule for the whole guest: the host-declared `model_provider`
    // (the guest does not infer it from the model id).
    let provider = resolve_model_provider(&model, in_struct);
    Some(vam_spec::v1::ModelPreference {
        provider,
        model_name: model,
        temperature: 0.1,
        top_p: 1.0,
        max_completion_tokens: 4096,
    })
}

fn context_window_from_inputs(inputs: Option<&Struct>) -> Option<u64> {
    let value = inputs?.fields.get("context_window")?;
    match value.kind.as_ref()? {
        prost_types::value::Kind::StringValue(text) => text.trim().parse::<u64>().ok(),
        prost_types::value::Kind::NumberValue(n) if *n > 0.0 => Some(*n as u64),
        _ => None,
    }
    .filter(|window| *window > 0)
}

/// One model request. With a recitation plan, the todo list rides as the last
/// message (a labelled note) and the request is labelled for the host; the
/// note is not added to `messages`, so it is never stored or summarised.
fn build_inference_request(
    model_pref: &Option<vam_spec::v1::ModelPreference>,
    messages: &[ChatMessage],
    tools: &[vam_spec::v1::ToolSpec],
    recitation: Option<&str>,
) -> InferenceRequest {
    let mut sent = messages.to_vec();
    let context_hint = recitation.map(|plan| {
        sent.push(vam_spec::compaction::recitation_note(plan));
        vam_spec::v1::ContextHint {
            lane: vam_spec::v1::context_hint::Lane::Note as i32,
            ..Default::default()
        }
    });
    InferenceRequest {
        model_preference: model_pref.clone(),
        messages: sent,
        tools: tools.to_vec(),
        context_hint,
    }
}

/// The agent's todo list, if it keeps one, for the summary request.
fn read_todo_plan(ws_root: &std::path::Path) -> Option<String> {
    let text = std::fs::read_to_string(ws_root.join(".vam/agent/todos.json")).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| vam_spec::compaction::cap_str(text, 4 * 1024))
}

// Boxing would change the variant constructors; the enum is short-lived.
#[allow(clippy::large_enum_variant)]
enum CompactionResult {
    /// The conversation after the compaction.
    Compacted(Vec<ChatMessage>),
    /// Not compacted this time; the task goes on with the full context.
    Skipped(String),
    /// The task cannot go on.
    Failed(TaskOutcome),
}

/// The summary exchange: the current messages plus one instruction, labelled
/// for the host with a context hint. The host verifies the hint against its
/// own record and writes the `compaction` record; the guest builds the same
/// summary message from the reply and carries on from it.
#[allow(clippy::too_many_arguments)]
async fn run_compaction(
    lease: &ExecutionLease,
    client: &InferenceClient,
    model_pref: &Option<vam_spec::v1::ModelPreference>,
    messages: &[ChatMessage],
    tools: &[vam_spec::v1::ToolSpec],
    cut: crate::compaction::Cut,
    reason: &str,
    todo_plan: Option<&str>,
    fixed_bytes: usize,
    budget: &mut RoundBudget,
    total_tokens: &mut TokenUsage,
) -> CompactionResult {
    let hint = vam_spec::v1::ContextHint {
        lane: vam_spec::v1::context_hint::Lane::Summary as i32,
        keep_from_index: cut.hint_index(),
        keep_from_msg_sha256: vam_spec::compaction::message_digest(&messages[cut.keep_from]),
        reason: reason.to_string(),
    };
    let mut last_problem = String::new();
    for _attempt in 0..2 {
        let request = InferenceRequest {
            model_preference: model_pref.clone(),
            messages: crate::compaction::summary_request_messages(messages, todo_plan),
            tools: tools.to_vec(),
            context_hint: Some(hint.clone()),
        };
        let mut rx = match budget.guest_step(client.infer(request)).await {
            Ok(Ok(rx)) => rx,
            Ok(Err(e)) => return CompactionResult::Skipped(format!("summary call failed: {e}")),
            Err(_) => return CompactionResult::Skipped("summary call timed out".to_string()),
        };
        let mut text = String::new();
        let mut called_tools = false;
        loop {
            let chunk = match budget.guest_step(rx.recv()).await {
                Ok(Some(Ok(chunk))) => chunk,
                Ok(Some(Err(e))) => {
                    return CompactionResult::Skipped(format!("summary stream failed: {e}"))
                }
                Ok(None) => break,
                Err(_) => return CompactionResult::Skipped("summary call timed out".to_string()),
            };
            if let Some(err) = chunk.error {
                if err
                    .message
                    .starts_with(crate::compaction::COMPACTION_REFUSED)
                {
                    return CompactionResult::Skipped(err.message);
                }
                return CompactionResult::Failed(inference_failed_outcome(lease, err));
            }
            text.push_str(&chunk.delta);
            called_tools |= !chunk.tool_calls.is_empty();
            if let Some(usage) = chunk.usage {
                total_tokens.prompt_tokens += usage.prompt_tokens;
                total_tokens.completion_tokens += usage.completion_tokens;
                total_tokens.total_tokens += usage.total_tokens;
                total_tokens.cached_tokens += usage.cached_tokens;
                total_tokens.cache_creation_tokens += usage.cache_creation_tokens;
            }
        }
        if called_tools || text.trim().is_empty() {
            last_problem = "the model returned no summary text".to_string();
            continue;
        }
        let compacted = crate::compaction::compacted_messages(messages, cut, &text);
        let bytes = fixed_bytes + vam_spec::compaction::approx_bytes_all(&compacted);
        if bytes > crate::compaction::GUARD_BYTES {
            return CompactionResult::Failed(fail_task(
                lease,
                8,
                format!(
                    "{}: the conversation is still {bytes} bytes after compaction",
                    crate::compaction::CONTEXT_TOO_LARGE
                ),
                Some(total_tokens.clone()),
            ));
        }
        return CompactionResult::Compacted(compacted);
    }
    CompactionResult::Skipped(last_problem)
}

fn inference_failed_outcome(
    lease: &ExecutionLease,
    mut err: vam_spec::v1::ErrorStatus,
) -> TaskOutcome {
    // The host could not record the conversation (its thread log is full, or
    // its disk failed): that is a storage failure of the task, not the model's.
    let unrecorded = err.message.starts_with("thread_log_quota_exceeded:")
        || err.message.starts_with("thread_log_unavailable:");
    // The conversation does not fit and could not be made to: a typed failure.
    let too_large = err
        .message
        .starts_with(crate::compaction::CONTEXT_TOO_LARGE)
        || err.message.starts_with(crate::compaction::CONTEXT_TOO_LONG);
    err.message = if too_large {
        err.message
    } else if unrecorded {
        format!("{STORAGE_COMMIT_FAILED}: {}", err.message)
    } else {
        format!("inference failed: {}", err.message)
    };
    TaskOutcome {
        lease: Some(lease.clone()),
        state: TaskOutcomeState::Failed as i32,
        output: None,
        output_artifacts: vec![],
        continuation: None,
        evidence: None,
        observed_usage: None,
        error: Some(err),
    }
}

fn succeed_with_answer(
    lease: &ExecutionLease,
    answer: String,
    total_tokens: TokenUsage,
) -> TaskOutcome {
    let mut out_struct = Struct::default();
    out_struct.fields.insert(
        "answer".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(answer)),
        },
    );
    TaskOutcome {
        lease: Some(lease.clone()),
        state: TaskOutcomeState::Succeeded as i32,
        output: Some(out_struct),
        output_artifacts: vec![],
        continuation: None,
        evidence: None,
        observed_usage: Some(total_tokens),
        error: None,
    }
}

fn extract_tool_calls_from_text(text: &str) -> Vec<ToolCall> {
    vam_spec::text_tool_calls::extract_tool_calls_from_text(text)
        .into_iter()
        .map(|c| ToolCall {
            id: c.id,
            name: c.name,
            arguments: c.arguments,
        })
        .collect()
}

/// The reply to an empty model response. It is the only nudge the loop sends, and it
/// never names a tool or guesses that the model owes one: that is the model's decision.
fn empty_response_nudge() -> &'static str {
    "Your last response was empty. Continue the assigned task now."
}

fn parse_hitl_denied_inputs(inputs: Option<&Struct>) -> Option<(String, Struct, String)> {
    let inputs = inputs?;
    let denied = inputs.fields.get("hitl_denied")?;
    if !matches!(
        denied.kind.as_ref(),
        Some(prost_types::value::Kind::BoolValue(true))
    ) {
        return None;
    }
    let tool_name = match inputs.fields.get("denied_tool")?.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(name)) if !name.is_empty() => name.clone(),
        _ => return None,
    };
    let tool_args = match inputs.fields.get("denied_tool_args")?.kind.as_ref() {
        Some(prost_types::value::Kind::StructValue(args)) => args.clone(),
        _ => return None,
    };
    let reason = match inputs.fields.get("denial_reason")?.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(reason)) if !reason.is_empty() => reason.clone(),
        _ => "Denied by user".to_string(),
    };
    Some((tool_name, tool_args, reason))
}

/// Resolve the checkpoint a resume task names, by digest.
///
/// `Ok(None)` means the task names no checkpoint, and the caller uses the
/// fallback resume. A digest that cannot be fetched or verified is an error that
/// names the checkpoint. The inline `hitl_continuation` input is never read.
async fn load_resume_checkpoint(
    lease: &ExecutionLease,
    inputs: Option<&Struct>,
    run_id: &str,
) -> Result<Option<HitlContinuation>, String> {
    // A run parked after the switch resumes from the host-assembled thread log.
    if let Some(ctx) = load_thread_context(lease, inputs).await? {
        return hitl_checkpoint::continuation_from_context(&ctx)
            .map(Some)
            .ok_or_else(|| "the thread context names no suspended tool call".to_string());
    }
    let Some(value) = inputs.and_then(|s| s.fields.get("hitl_continuation_digest")) else {
        return Ok(None);
    };
    let digest = match value.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(digest)) if !digest.is_empty() => {
            digest.as_str()
        }
        _ => return Err("HITL checkpoint digest is empty or not a string".to_string()),
    };
    let bytes = crate::pep::fetch_run_checkpoint(lease, digest)
        .await
        .map_err(|e| format!("HITL checkpoint {digest} could not be fetched: {e}"))?;
    let json = std::str::from_utf8(&bytes)
        .map_err(|_| format!("HITL checkpoint {digest} is not UTF-8 JSON"))?;
    hitl_checkpoint::continuation_from_host_payload(json, run_id, digest)
        .map(Some)
        .ok_or_else(|| format!("HITL checkpoint {digest} failed digest or schema verification"))
}

fn parse_hitl_resume_inputs(inputs: Option<&Struct>) -> Option<(String, Struct)> {
    let inputs = inputs?;
    let resume = inputs.fields.get("hitl_resume")?;
    if !matches!(
        resume.kind.as_ref(),
        Some(prost_types::value::Kind::BoolValue(true))
    ) {
        return None;
    }
    let tool_name = match inputs.fields.get("approved_tool")?.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(name)) if !name.is_empty() => name.clone(),
        _ => return None,
    };
    let tool_args = match inputs.fields.get("approved_tool_args")?.kind.as_ref() {
        Some(prost_types::value::Kind::StructValue(args)) => args.clone(),
        _ => return None,
    };
    Some((tool_name, tool_args))
}

fn ensure_skill_for_tool(runtime: &mut SkillsRuntime, tool_name: &str) -> Result<(), String> {
    if crate::outbox::is_tool(tool_name) {
        return Ok(());
    }
    if find_loaded_tool_spec(tool_name, runtime).is_some() {
        return Ok(());
    }
    if crate::primitives::is_builtin(tool_name) {
        return Ok(());
    }
    if let Some(skill_id) = SkillsRuntime::catalog_skill_id_for_tool(tool_name) {
        runtime.load(&skill_id)?;
        if find_loaded_tool_spec(tool_name, runtime).is_some() {
            return Ok(());
        }
        return Err(format!(
            "tool '{}' is unavailable after loading skill '{}'",
            tool_name, skill_id
        ));
    }
    // Host-exec / HSP tools are advertised by the host and have no guest skill.
    Ok(())
}

fn reload_checkpoint_skills(runtime: &mut SkillsRuntime, skill_ids: &[String]) {
    for skill_id in skill_ids {
        if skill_id.is_empty() {
            continue;
        }
        if let Err(e) = runtime.load(skill_id) {
            tracing::warn!(
                "failed to reload skill '{}' on HITL resume: {}",
                skill_id,
                e
            );
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn apply_hitl_checkpoint_restore(
    checkpoint: &HitlContinuation,
    system_prompt: &str,
    observation: String,
    resume_nudge: &str,
    tool_name: &str,
    skills_runtime: &SkillsRuntime,
    messages: &mut Vec<ChatMessage>,
    total_tokens: &mut TokenUsage,
    nudge_count: &mut usize,
    last_answer_candidate: &mut String,
    skill_tool_called: &mut bool,
    force_finalize: &mut bool,
    iteration_cap: &mut usize,
    iterations_used_before_loop: &mut usize,
    max_iterations: &mut usize,
    claim_reprompted: &mut bool,
) {
    *claim_reprompted = checkpoint.claim_reprompted;
    *messages = hitl_checkpoint::restore_messages_from_checkpoint(
        checkpoint,
        system_prompt,
        observation.clone(),
        resume_nudge,
    );
    total_tokens.prompt_tokens = checkpoint.prompt_tokens;
    total_tokens.completion_tokens = checkpoint.completion_tokens;
    total_tokens.cached_tokens = checkpoint.cached_tokens;
    total_tokens.total_tokens = checkpoint.total_tokens;
    *nudge_count = checkpoint.nudge_count;
    // A question's answer is input, never the task's final answer.
    *last_answer_candidate =
        if observation.trim().is_empty() || tool_name == vam_spec::outbox::ASK_USER_TOOL {
            checkpoint.last_answer_candidate.clone()
        } else {
            observation
        };
    *skill_tool_called = checkpoint.skill_tool_called;
    if skills_runtime
        .loaded_tools()
        .iter()
        .any(|tool| tool.name == tool_name)
    {
        *skill_tool_called = true;
    }
    *iteration_cap = checkpoint.max_iterations;
    *iterations_used_before_loop = checkpoint.iterations_used;
    *max_iterations = remaining_iterations(*iteration_cap, *iterations_used_before_loop);
    *force_finalize = false;
}

#[allow(clippy::too_many_arguments)]
fn apply_hitl_resume_fallback(
    system_prompt: &str,
    instruction: &str,
    tool_name: &str,
    tool_args_json: &str,
    observation: String,
    resume_nudge: &str,
    messages: &mut Vec<ChatMessage>,
    last_answer_candidate: &mut String,
    force_finalize: &mut bool,
) {
    let tool_call_id = "hitl_resume_0".to_string();
    messages.clear();
    messages.push(ChatMessage {
        role: "system".to_string(),
        content: system_prompt.to_string(),
        tool_calls: vec![],
        tool_call_id: String::new(),
    });
    messages.push(ChatMessage {
        role: "user".to_string(),
        content: instruction.to_string(),
        tool_calls: vec![],
        tool_call_id: String::new(),
    });
    messages.push(ChatMessage {
        role: "assistant".to_string(),
        content: String::new(),
        tool_calls: vec![ToolCall {
            id: tool_call_id.clone(),
            name: tool_name.to_string(),
            arguments: tool_args_json.to_string(),
        }],
        tool_call_id: String::new(),
    });
    messages.push(ChatMessage {
        role: "tool".to_string(),
        content: observation.clone(),
        tool_calls: vec![],
        tool_call_id,
    });
    if !resume_nudge.is_empty() {
        messages.push(ChatMessage {
            role: "user".to_string(),
            content: resume_nudge.to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        });
    }
    if !observation.trim().is_empty() && tool_name != vam_spec::outbox::ASK_USER_TOOL {
        *last_answer_candidate = observation;
    }
    *force_finalize = true;
}

fn format_tool_subprocess_output(tr: &crate::exec::SubprocessToolResult) -> String {
    let stdout_str = String::from_utf8_lossy(&tr.stdout).into_owned();
    let stderr_str = String::from_utf8_lossy(&tr.stderr).into_owned();

    if stderr_str.starts_with("tool_error:") {
        stderr_str
    } else if let Some(ref err) = tr.error {
        if !stderr_str.trim().is_empty() {
            format!(
                "error (code {}): {}\ndetail: {}",
                err.code,
                err.message,
                stderr_str.trim()
            )
        } else {
            format!("error (code {}): {}", err.code, err.message)
        }
    } else if tr.exit_code != 0 {
        if !stderr_str.trim().is_empty() {
            format!(
                "error (exit code {}): {}\nstderr: {}",
                tr.exit_code, tr.system_logs, stderr_str
            )
        } else {
            format!("error (exit code {}): {}", tr.exit_code, tr.system_logs)
        }
    } else if stdout_str.is_empty() {
        format!("(empty output)\n{}", tr.system_logs)
    } else {
        stdout_str
    }
}

/// The user's answer to an `ask_user` question, as JSON text.
fn hitl_answer(inputs: Option<&Struct>) -> Option<String> {
    let inputs = inputs?;
    match inputs.fields.get("hitl_answer")?.kind.as_ref() {
        Some(prost_types::value::Kind::StructValue(answer)) => {
            Some(vam_spec::harness::struct_to_json(answer).to_string())
        }
        _ => None,
    }
}

/// What else was decided while the run was parked, written by the host for the one resume.
fn hitl_decisions_note(inputs: Option<&Struct>) -> Option<String> {
    let inputs = inputs?;
    match inputs.fields.get("hitl_decisions_note")?.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(note)) if !note.trim().is_empty() => {
            Some(note.clone())
        }
        _ => None,
    }
}

fn hitl_denied_note(inputs: Option<&Struct>) -> Option<String> {
    let inputs = inputs?;
    match inputs.fields.get("hitl_denied_note")?.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(note)) if !note.is_empty() => Some(note.clone()),
        _ => None,
    }
}

fn default_hitl_denied_nudge() -> String {
    "The user denied this tool action. Briefly explain that you could not proceed.".to_string()
}

fn hitl_resume_note(inputs: Option<&Struct>) -> Option<String> {
    let inputs = inputs?;
    match inputs.fields.get("hitl_resume_note")?.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(note)) if !note.is_empty() => Some(note.clone()),
        _ => None,
    }
}

/// The resume note once the approved tool has run. An error result means the action did not
/// happen, so the note says that instead of "already executed". It keeps the opening words so a
/// rule keyed on "Human approval was granted" still matches.
fn resume_nudge_after(tool_name: &str, observation: &str, nudge: String) -> String {
    if tool_name == vam_spec::outbox::ASK_USER_TOOL
        || !crate::loop_detect::is_error_result(observation)
    {
        return nudge;
    }
    format!(
        "Human approval was granted, but the approved tool '{tool_name}' did not complete: its \
         result above is an error, so the action did not happen. Tell the user it was not done \
         and why. Do not say it succeeded."
    )
}

fn default_hitl_resume_nudge() -> String {
    "Human approval was granted and the approved tool has already executed. \
     Continue the original task from this point. Use other tools as needed to finish remaining \
     steps. Do not re-invoke the same approved tool unless the remaining work requires it."
        .to_string()
}

fn struct_to_json_string(value: &Struct) -> String {
    let mut obj = serde_json::Map::new();
    for (key, val) in &value.fields {
        if let Some(json_val) = prost_value_to_json(val) {
            obj.insert(key.clone(), json_val);
        }
    }
    serde_json::Value::Object(obj).to_string()
}

fn prost_value_to_json(value: &prost_types::Value) -> Option<serde_json::Value> {
    match value.kind.as_ref()? {
        prost_types::value::Kind::NullValue(_) => Some(serde_json::Value::Null),
        prost_types::value::Kind::BoolValue(b) => Some(serde_json::Value::Bool(*b)),
        prost_types::value::Kind::NumberValue(n) => {
            serde_json::Number::from_f64(*n).map(serde_json::Value::Number)
        }
        prost_types::value::Kind::StringValue(s) => Some(serde_json::Value::String(s.clone())),
        prost_types::value::Kind::ListValue(list) => Some(serde_json::Value::Array(
            list.values.iter().filter_map(prost_value_to_json).collect(),
        )),
        prost_types::value::Kind::StructValue(s) => {
            let mut obj = serde_json::Map::new();
            for (key, val) in &s.fields {
                if let Some(json_val) = prost_value_to_json(val) {
                    obj.insert(key.clone(), json_val);
                }
            }
            Some(serde_json::Value::Object(obj))
        }
    }
}

/// The tool message for a call the host denied.
fn denied_observation(tool_name: &str, diag: &str) -> String {
    if diag.starts_with(JOURNAL_UNAVAILABLE_MARKER) {
        format!("Error: Tool '{}' was not sent: {}", tool_name, diag)
    } else {
        format!(
            "Error: Tool '{}' was denied by security policy: {}",
            tool_name, diag
        )
    }
}

/// The suspend tail of a resumed call that parks again (after a reconnect, on the tool's own
/// ask): the same pending call and the state the checkpoint held. It carries no results: the
/// host logged them at the first suspend.
fn resuspend_tail(
    lease: &ExecutionLease,
    checkpoint: &HitlContinuation,
) -> vam_spec::thread_context::SuspendTail {
    vam_spec::thread_context::SuspendTail {
        schema_version: vam_spec::thread_context::SUSPEND_TAIL_SCHEMA,
        run_id: lease
            .run_id
            .as_ref()
            .map(|r| r.urn.clone())
            .unwrap_or_default(),
        lifecycle_generation: lease.lifecycle_generation,
        kind: "suspend".to_string(),
        messages: vec![],
        pending: vam_spec::thread_context::PendingCall {
            tool_call_id: checkpoint.pending_tool_call_id.clone(),
            tool_name: checkpoint.pending_tool_name.clone(),
            args_json: checkpoint.pending_tool_args_json.clone(),
        },
        state: vam_spec::thread_context::LoopState {
            loaded_skill_ids: checkpoint.loaded_skill_ids.clone(),
            iterations_used: checkpoint.iterations_used,
            max_iterations: checkpoint.max_iterations,
            prompt_tokens: checkpoint.prompt_tokens,
            completion_tokens: checkpoint.completion_tokens,
            cached_tokens: checkpoint.cached_tokens,
            total_tokens: checkpoint.total_tokens,
            nudge_count: checkpoint.nudge_count,
            skill_tool_called: checkpoint.skill_tool_called,
            claim_reprompted: checkpoint.claim_reprompted,
            last_answer_candidate: checkpoint.last_answer_candidate.clone(),
            inbox_seq: checkpoint.inbox_seq,
        },
    }
}

/// Parks the task again on the new approval, like a first-time ask.
async fn resuspend_outcome(
    lease: &ExecutionLease,
    ws_root: &std::path::Path,
    checkpoint: &HitlContinuation,
) -> TaskOutcome {
    let tail_bytes = serde_json::to_vec(&resuspend_tail(lease, checkpoint)).unwrap_or_default();
    let digest = vam_spec::thread_context::sha256_hex(&tail_bytes);
    if let Err(err) = crate::pep::upload_run_checkpoint(lease, &tail_bytes, &digest).await {
        tracing::warn!("suspend tail refused: {err}");
        return fail_task(
            lease,
            9,
            format!("{STORAGE_COMMIT_FAILED}: the host could not record the suspended task: {err}"),
            None,
        );
    }
    TaskOutcome {
        lease: Some(lease.clone()),
        state: TaskOutcomeState::Suspended as i32,
        output: None,
        output_artifacts: vec![],
        continuation: Some(workspace_checkpoint_artifact(ws_root, &digest)),
        evidence: None,
        observed_usage: None,
        error: None,
    }
}

fn pep_auth_failed_outcome(lease: &ExecutionLease, message: String) -> TaskOutcome {
    TaskOutcome {
        lease: Some(lease.clone()),
        state: TaskOutcomeState::Failed as i32,
        output: None,
        output_artifacts: vec![],
        continuation: None,
        evidence: None,
        observed_usage: None,
        error: Some(vam_spec::v1::ErrorStatus {
            code: 13,
            message,
            details: vec![],
            retryable: false,
        }),
    }
}

#[allow(clippy::too_many_arguments)] // flat parameter list is the established call shape; a params struct would change callers
async fn execute_hitl_approved_tool_observation(
    lease: &ExecutionLease,
    tool_name: &str,
    tool_args: &Struct,
    skills_runtime: &mut SkillsRuntime,
    playbook_procedure: &str,
    result_cap: usize,
    call_id: &str,
    checkpoint: Option<&HitlContinuation>,
) -> Result<String, TaskOutcome> {
    let ws_root = crate::primitives::workspace_root();
    if let Err(e) = ensure_skill_for_tool(skills_runtime, tool_name) {
        return Ok(format!("tool_error: {}", e));
    }

    let script_digest_opt =
        resolve_script_digest(tool_name, tool_args, skills_runtime, playbook_procedure);
    let auth_res = match crate::pep::request_tool_authorization(
        lease,
        tool_name,
        Some(tool_args),
        script_digest_opt.as_deref(),
        call_id,
    )
    .await
    {
        Ok(resp) => resp,
        Err(e) => {
            return Err(pep_auth_failed_outcome(
                lease,
                format!("PEP authorization failed: {}", e),
            ));
        }
    };

    // The host decides the re-intent afresh. Approval is not the only answer: a reconnect
    // resumes a call whose own policy may still ask or deny, and a call the journal already
    // holds is replayed. Each is handled as in the loop; none is a task failure.
    {
        use vam_spec::v1::tool_intent_response::Disposition;
        let disposition = auth_res.disposition;
        if disposition == Disposition::Replayed as i32 {
            return Ok(match auth_res.replayed {
                Some(recorded) => render_host_response(recorded, &ws_root, result_cap),
                None => "tool_error: the host replayed this call without its recorded answer"
                    .to_string(),
            });
        }
        if disposition == Disposition::Denied as i32 {
            let diag = auth_res
                .error
                .as_ref()
                .map(|e| e.message.clone())
                .unwrap_or_else(|| "Tool execution denied by host policy".to_string());
            append_tool_observation(&ws_root, tool_name, "denied", &diag);
            return Ok(denied_observation(tool_name, &diag));
        }
        if disposition == Disposition::Suspended as i32 {
            if let Some(checkpoint) = checkpoint {
                let approval = auth_res
                    .approval_request_id
                    .as_ref()
                    .map(|r| r.urn.as_str())
                    .unwrap_or("pending");
                append_tool_observation(
                    &ws_root,
                    tool_name,
                    "suspended",
                    &format!("approval {approval}"),
                );
                return Err(resuspend_outcome(lease, &ws_root, checkpoint).await);
            }
        }
    }

    if auth_res.disposition != vam_spec::v1::tool_intent_response::Disposition::Authorized as i32 {
        let diag = auth_res
            .error
            .as_ref()
            .map(|e| e.message.clone())
            .unwrap_or_else(|| "tool not authorized after HITL approval".to_string());
        return Err(pep_auth_failed_outcome(
            lease,
            format!(
                "Tool '{}' was not authorized after HITL approval: {}",
                tool_name, diag
            ),
        ));
    }

    let capability_token = match auth_res.capability_token {
        Some(token) => token,
        None => {
            return Err(pep_auth_failed_outcome(
                lease,
                "missing capability token after HITL authorization".to_string(),
            ));
        }
    };

    append_tool_observation(&ws_root, tool_name, "authorized", "hitl-resume executing");

    let tool_output = if crate::outbox::is_tool(tool_name) {
        crate::outbox::execute(lease, tool_name, tool_args, Some(&capability_token)).await
    } else if is_host_exec_tool(tool_name, skills_runtime, playbook_procedure) {
        execute_host_tool_call(
            lease,
            tool_name,
            tool_args,
            &capability_token,
            &ws_root,
            result_cap,
            call_id,
        )
        .await
    } else {
        match crate::exec::execute_tool_subprocess_with_token(
            tool_name,
            Some(tool_args),
            Duration::from_secs(60),
            Some(
                lease
                    .lease_id
                    .as_ref()
                    .map(|id| id.urn.clone())
                    .unwrap_or_default(),
            ),
            Some(&capability_token),
            None,
            None,
        )
        .await
        {
            Ok(tr) => format_tool_subprocess_output(&tr),
            Err(e) => format!("error: Tool execution invocation failed: {}", e),
        }
    };

    append_tool_observation(
        &ws_root,
        tool_name,
        "completed",
        &tool_output.chars().take(256).collect::<String>(),
    );

    Ok(tool_output)
}

fn execute_load_skill(runtime: &mut SkillsRuntime, args: &Struct) -> Result<String, String> {
    let skill_id = args
        .fields
        .get("skill")
        .and_then(|v| {
            if let Some(prost_types::value::Kind::StringValue(s)) = &v.kind {
                Some(s.clone())
            } else {
                None
            }
        })
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "load_skill requires 'skill'".to_string())?;
    runtime.load(&skill_id)
}

fn find_loaded_tool_spec<'a>(
    tool_name: &str,
    skills_runtime: &'a SkillsRuntime,
) -> Option<&'a vam_spec::v1::ToolSpec> {
    skills_runtime
        .loaded_tools()
        .iter()
        .find(|t| t.name == tool_name)
}

fn is_host_exec_tool(
    tool_name: &str,
    skills_runtime: &SkillsRuntime,
    playbook_procedure: &str,
) -> bool {
    skills::boot_candidate_tools(skills_runtime.loaded_tools(), playbook_procedure, true)
        .iter()
        .find(|t| t.name == tool_name)
        .and_then(|t| t.execution_provider.as_ref())
        .map(|p| matches!(p, vam_spec::v1::tool_spec::ExecutionProvider::HostExec(_)))
        .unwrap_or(false)
}

fn is_mcp_tool(tool_name: &str, skills_runtime: &SkillsRuntime) -> bool {
    find_loaded_tool_spec(tool_name, skills_runtime)
        .and_then(|t| t.execution_provider.as_ref())
        .map(|p| matches!(p, vam_spec::v1::tool_spec::ExecutionProvider::McpServer(_)))
        .unwrap_or(false)
}

fn resolve_script_digest(
    tool_name: &str,
    args_struct: &Struct,
    skills_runtime: &SkillsRuntime,
    playbook_procedure: &str,
) -> Option<String> {
    if crate::outbox::is_tool(tool_name) {
        return Some(crate::outbox::tool_digest(tool_name));
    }
    if let Some(spec) =
        skills::boot_candidate_tools(skills_runtime.loaded_tools(), playbook_procedure, true)
            .into_iter()
            .find(|t| t.name == tool_name)
    {
        if let Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(host)) =
            &spec.execution_provider
        {
            return Some(vam_spec::validation::host_tool_digest(&host.adapter_id));
        }
    }
    let (digest, _) = crate::exec::compute_script_digest(tool_name, Some(args_struct));
    if digest.is_empty() {
        None
    } else {
        Some(digest)
    }
}

const HOST_ARTIFACTS_DIR: &str = ".vam/host-artifacts";
const OBSERVATIONS_JSONL: &str = ".vam/agent/observations.jsonl";
/// A fenced MCP result is at most 8 KiB of text plus the fence (AR-0.6), so
/// the cap leaves room for the closing line. Larger text is cut with a note.
pub(crate) const MAX_TEXT_RESULT_NO_ARTIFACT: usize = 16 * 1024;
const ARTIFACT_WRITTEN_PREFIX: &str = "artifact written to ";

fn workspace_checkpoint_artifact(
    ws_root: &std::path::Path,
    digest_hex: &str,
) -> vam_spec::v1::ArtifactRef {
    let digest_hex = digest_hex.to_string();
    vam_spec::v1::ArtifactRef {
        artifact_id: Some(vam_spec::v1::ResourceId {
            urn: "urn:vam:artifact:local.vam.dev/acme/workspace-checkpoint".to_string(),
        }),
        digest: Some(vam_spec::v1::Digest {
            algorithm: 1,
            hex_hash: digest_hex,
        }),
        media_type: "application/vnd.vam.workspace-checkpoint".to_string(),
        size_bytes: 0,
        uri: format!("file://{}", ws_root.display()),
    }
}

fn append_tool_observation(
    workspace_root: &std::path::Path,
    tool_name: &str,
    disposition: &str,
    summary: &str,
) {
    let dir = workspace_root.join(".vam/agent");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let line = serde_json::json!({
        "tool": tool_name,
        "disposition": disposition,
        "summary": summary.chars().take(256).collect::<String>(),
        "ts": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
    });
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(workspace_root.join(OBSERVATIONS_JSONL))
    {
        let _ = writeln!(file, "{}", line);
    }
}

fn sanitize_artifact_name(name: &str) -> String {
    let base = std::path::Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("artifact.bin");
    base.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .collect()
}

fn write_host_artifact(
    workspace_root: &std::path::Path,
    relpath: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let safe_name = sanitize_artifact_name(relpath);
    if safe_name.is_empty() {
        return Err("invalid artifact name".to_string());
    }
    let dir = workspace_root.join(HOST_ARTIFACTS_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("failed to create host artifacts dir: {}", e))?;
    let path = dir.join(&safe_name);
    std::fs::write(&path, bytes).map_err(|e| format!("failed to write host artifact: {}", e))?;
    Ok(format!("{}/{}", HOST_ARTIFACTS_DIR, safe_name))
}

/// The host's typed error when its effect journal cannot record a call (nothing was sent).
const JOURNAL_UNAVAILABLE_MARKER: &str = "effect_journal_unavailable";

async fn execute_host_tool_call(
    lease: &ExecutionLease,
    tool_name: &str,
    args: &Struct,
    token: &vam_spec::v1::SignedEnvelope,
    workspace_root: &std::path::Path,
    result_cap: usize,
    call_id: &str,
) -> String {
    match crate::pep::execute_host_tool(lease, tool_name, Some(args), token, call_id).await {
        Ok(resp) => render_host_response(resp, workspace_root, result_cap),
        Err(e) => format!("tool_error: host execute failed: {}", e),
    }
}

/// What the model is shown for a host tool's answer: the text, with any files it carried saved
/// under the workspace. The answer of an executed call and the replayed answer of a call that
/// already happened read the same.
fn render_host_response(
    resp: vam_spec::v1::ExecuteHostToolResponse,
    workspace_root: &std::path::Path,
    result_cap: usize,
) -> String {
    if let Some(err) = resp.error {
        // The journal could not record the call: nothing was sent. Say so plainly.
        if err.message.starts_with(JOURNAL_UNAVAILABLE_MARKER) {
            return format!("tool_error: {}", err.message);
        }
        return format!("denied by security policy: {}", err.message);
    }
    let output = resp.text_result;
    let mut written = Vec::new();
    if !resp.artifact_bytes.is_empty() && !resp.artifact_relpath.is_empty() {
        match write_host_artifact(workspace_root, &resp.artifact_relpath, &resp.artifact_bytes) {
            Ok(path) => written.push(path),
            Err(e) => return format!("tool_error: {}", e),
        }
    }
    for extra in &resp.extra_artifacts {
        match write_host_artifact(workspace_root, &extra.relpath, &extra.bytes) {
            Ok(path) => written.push(path),
            Err(e) => return format!("tool_error: {}", e),
        }
    }
    format_host_tool_response(&output, &written, result_cap)
}

fn format_host_tool_response(text_result: &str, artifact_paths: &[String], cap: usize) -> String {
    if artifact_paths.is_empty() {
        return cap_text_result(text_result, cap);
    }
    // The host bounds the text that goes with files (a fenced preview), and
    // the path lines come last so nothing in the text can pose as one.
    let body = cap_text_result_with(text_result, "files saved", cap);
    let lines = artifact_paths
        .iter()
        .map(|path| format!("{ARTIFACT_WRITTEN_PREFIX}{path}"))
        .collect::<Vec<_>>()
        .join("\n");
    if body.trim().is_empty() {
        lines
    } else {
        format!("{}\n{}", body.trim_end(), lines)
    }
}

fn cap_text_result(text: &str, cap: usize) -> String {
    cap_text_result_with(
        text,
        "no file saved; re-run or write results to notes/",
        cap,
    )
}

fn cap_text_result_with(text: &str, why: &str, cap: usize) -> String {
    if text.len() <= cap {
        return text.to_string();
    }
    let mut end = cap;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n[truncated: kept {} of {} bytes; {}]",
        &text[..end],
        end,
        text.len(),
        why
    )
}

/// Paths from the guest-written "artifact written to" lines at the very end of
/// a tool message. Vendor text sits before them (inside its fence), so it
/// cannot add to this block.
fn trailing_artifact_paths(content: &str) -> Vec<String> {
    let mut paths: Vec<String> = content
        .lines()
        .rev()
        .take_while(|line| line.starts_with(ARTIFACT_WRITTEN_PREFIX))
        .map(|line| line[ARTIFACT_WRITTEN_PREFIX.len()..].trim().to_string())
        .filter(|path| !path.is_empty())
        .collect();
    paths.reverse();
    paths
}

/// Replaces a bulky tool output with a pointer to the file it was written to.
/// Not called by the round loop since the thread log records what the model saw
/// (AR-2.1A); batched clearing of old results (AR-2.12) reuses it.
#[cfg_attr(not(test), allow(dead_code))]
fn compact_tool_output(content: &str) -> String {
    let paths = trailing_artifact_paths(content);
    match paths.as_slice() {
        [] => {}
        [path] => return format!("[Output truncated — read_file {} if needed]", path),
        _ => {
            return format!(
                "[Output truncated — read_file one of these if needed: {}]",
                paths.join(", ")
            )
        }
    }
    if let Some(path) = extract_reread_path(content) {
        format!("[Output truncated — read_file {} if needed]", path)
    } else {
        "[Output of tool truncated; not on disk — re-run or write to notes/ next time]".to_string()
    }
}

fn extract_reread_path(content: &str) -> Option<String> {
    if let Some(path) = extract_skills_mount_path(content) {
        return Some(path);
    }
    extract_workspace_path(content)
}

fn extract_skills_mount_path(content: &str) -> Option<String> {
    const GUEST_PREFIX: &str = "/mnt/drive9/skills/";
    let mut source_path = None;
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(crate::primitives::SOURCE_STAMP_PREFIX) {
            let rest = rest.trim();
            if rest.starts_with("/mnt/drive9/skills/") || rest == "/mnt/drive9/skills" {
                source_path = Some(rest.to_string());
            }
            continue;
        }
        if let Some(idx) = trimmed.find(GUEST_PREFIX) {
            let rest = &trimmed[idx..];
            let candidate = rest.split(':').next().unwrap_or(rest);
            let candidate = candidate
                .split_whitespace()
                .next()
                .unwrap_or(candidate)
                .trim_matches(|c: char| c == '`' || c == '"' || c == '\'');
            if candidate.starts_with(GUEST_PREFIX) && candidate.len() > GUEST_PREFIX.len() {
                return Some(candidate.to_string());
            }
        }
    }
    source_path
}

fn extract_workspace_path(content: &str) -> Option<String> {
    if let Some(path) = trailing_artifact_paths(content).into_iter().next() {
        return Some(path);
    }

    for line in content.lines() {
        if let Some(to_idx) = line.find(" bytes to ") {
            if line.starts_with("Wrote ") {
                let path = line[to_idx + " bytes to ".len()..].trim();
                if !path.is_empty() {
                    return Some(path.to_string());
                }
            }
        }
        if let Some(idx) = line.find("notes/") {
            let rest = &line[idx..];
            let path = rest
                .split_whitespace()
                .next()
                .unwrap_or(rest)
                .trim_matches(|c: char| c == '`' || c == '"' || c == '\'');
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
        if line.contains("PLAN.md") {
            return Some("PLAN.md".to_string());
        }
        if let Some(idx) = line.find(".vam/host-artifacts/") {
            let rest = &line[idx..];
            let path = rest
                .split_whitespace()
                .next()
                .unwrap_or(rest)
                .trim_matches(|c: char| c == '`' || c == '"' || c == '\'');
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn build_agent_system_prompt(
    workspace_root: &std::path::Path,
    inputs: Option<&Struct>,
    tools: &[vam_spec::v1::ToolSpec],
    skills_catalog: &str,
    skill_extensions: &str,
) -> String {
    let reminders = vam_spec::harness::Harness::from_inputs(inputs)
        .map(|h| h.context.reminders)
        .unwrap_or(true);
    crate::instruction::build_system_prompt_with(
        workspace_root,
        inputs,
        tools,
        skills_catalog,
        skill_extensions,
        reminders,
    )
}

/// True when `output` is the result of a successful `start_playbook`.
fn playbook_started(tool_name: &str, output: &str) -> bool {
    tool_name == skills::START_PLAYBOOK_TOOL_NAME
        && output.starts_with(vam_spec::playbook::PLAYBOOK_STARTED_PREFIX)
}

/// Marker that starts the status block at the end of a `start_playbook` result.
const PLAYBOOK_STATUS_MARKER: &str = "\n\n[Playbook status:";

/// The procedure part of a `start_playbook` result. The status block that follows
/// it is for the conversation only: it never goes into the system prompt.
fn procedure_without_status(output: &str) -> String {
    match output.find(PLAYBOOK_STATUS_MARKER) {
        Some(at) => output[..at].to_string(),
        None => output.to_string(),
    }
}

fn input_string(inputs: Option<&Struct>, key: &str) -> Option<String> {
    match inputs?.fields.get(key)?.kind.as_ref() {
        Some(prost_types::value::Kind::StringValue(text)) if !text.trim().is_empty() => {
            Some(text.clone())
        }
        _ => None,
    }
}

/// Text appended to the first user message: playbook guidance, and the status
/// block after a gate decision.
fn in_flight_context(inputs: Option<&Struct>) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(guidance) = input_string(inputs, "playbook_guidance") {
        parts.push(format!("Guidance for this task:\n{guidance}"));
    }
    if let Some(status) = input_string(inputs, "playbook_status") {
        let note = hitl_resume_note(inputs).unwrap_or_default();
        parts.push(format!(
            "{note}\n{}\n\n{status}",
            vam_spec::playbook::PLAYBOOK_GATE_RULE
        ));
    }
    (!parts.is_empty()).then(|| parts.join("\n\n").trim().to_string())
}

fn playbook_procedure_from_inputs(inputs: Option<&Struct>) -> String {
    inputs
        .and_then(|s| s.fields.get("playbook_procedure"))
        .and_then(|v| match &v.kind {
            Some(prost_types::value::Kind::StringValue(text)) => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn rebuild_tools_list(
    skills_runtime: &SkillsRuntime,
    allowed_names: &[String],
    playbook_procedure: &str,
    offer_start: bool,
    termination: Termination,
) -> Vec<vam_spec::v1::ToolSpec> {
    let candidates = skills::boot_candidate_tools(
        skills_runtime.loaded_tools(),
        playbook_procedure,
        offer_start,
    );
    with_task_complete(
        filter_tools_by_advertised(candidates, allowed_names),
        termination,
    )
}

/// `task_complete` is a guest intrinsic: it is added after the Cedar filter, is
/// last in the list, and is only offered under an explicit completion claim.
/// An empty list stays empty so the builtin fallback still applies.
fn with_task_complete(
    mut tools: Vec<vam_spec::v1::ToolSpec>,
    termination: Termination,
) -> Vec<vam_spec::v1::ToolSpec> {
    if termination == Termination::ExplicitClaim && !tools.is_empty() {
        tools.push(skills::task_complete_tool_spec());
    }
    tools
}

/// `reasoningPolicy.termination`, as vamd writes it into the run inputs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Termination {
    /// Unset: a reply without tool calls ends the task.
    Unset,
    /// The model must call `task_complete`.
    ExplicitClaim,
    /// The playbook output check decides. The finish check already does this.
    OutputSatisfied,
}

fn resolve_termination(inputs: Option<&Struct>) -> Termination {
    let text = inputs
        .and_then(|s| s.fields.get("termination"))
        .and_then(|v| match &v.kind {
            Some(prost_types::value::Kind::StringValue(text)) => Some(text.trim()),
            _ => None,
        })
        .unwrap_or_default();
    match text {
        "explicit_claim" => Termination::ExplicitClaim,
        "output_satisfied" => Termination::OutputSatisfied,
        _ => Termination::Unset,
    }
}

/// Records whether the model claimed completion on an outcome. Only runs under
/// an explicit claim carry the field; vamd copies it to the run's final outputs.
fn record_claim(mut outcome: TaskOutcome, termination: Termination, claimed: bool) -> TaskOutcome {
    if termination != Termination::ExplicitClaim {
        return outcome;
    }
    let output = outcome.output.get_or_insert_with(Struct::default);
    output.fields.insert(
        "claimed".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(claimed)),
        },
    );
    outcome
}

/// The one neutral re-prompt after a text-only reply under an explicit claim.
fn claim_reprompt() -> &'static str {
    "The task is not marked complete. If it is finished, call task_complete with a short summary. Otherwise continue the task."
}

fn mapi_val(v: serde_json::Value) -> Option<prost_types::Value> {
    match v {
        serde_json::Value::Null => Some(prost_types::Value {
            kind: Some(prost_types::value::Kind::NullValue(0)),
        }),
        serde_json::Value::Bool(b) => Some(prost_types::Value {
            kind: Some(prost_types::value::Kind::BoolValue(b)),
        }),
        serde_json::Value::Number(n) => n.as_f64().map(|f| prost_types::Value {
            kind: Some(prost_types::value::Kind::NumberValue(f)),
        }),
        serde_json::Value::String(s) => Some(prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(s)),
        }),
        serde_json::Value::Array(arr) => {
            let mut values = Vec::new();
            for item in arr {
                if let Some(val) = mapi_val(item) {
                    values.push(val);
                }
            }
            Some(prost_types::Value {
                kind: Some(prost_types::value::Kind::ListValue(
                    prost_types::ListValue { values },
                )),
            })
        }
        serde_json::Value::Object(obj) => {
            let mut fields = std::collections::BTreeMap::new();
            for (k, val) in obj {
                if let Some(val_p) = mapi_val(val) {
                    fields.insert(k, val_p);
                }
            }
            Some(prost_types::Value {
                kind: Some(prost_types::value::Kind::StructValue(Struct { fields })),
            })
        }
    }
}

/// Keeps candidate tools whose names appear in the host-advertised allowlist.
pub(crate) fn filter_tools_by_advertised(
    candidates: Vec<vam_spec::v1::ToolSpec>,
    allowed: &[String],
) -> Vec<vam_spec::v1::ToolSpec> {
    if allowed.is_empty() {
        return Vec::new();
    }
    let allowed_set: std::collections::HashSet<&str> = allowed.iter().map(|s| s.as_str()).collect();
    candidates
        .into_iter()
        .filter(|t| allowed_set.contains(t.name.as_str()))
        .collect()
}

#[cfg(test)]
mod prompt_tests {
    use super::*;
    use crate::instruction;
    use crate::skills::{format_skills_catalog, SkillCatalogEntry, SkillsRuntime};

    #[test]
    fn max_iterations_accepts_string_and_number() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "max_iterations".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue("20".into())),
            },
        );
        assert_eq!(
            resolve_max_iterations(Some(&Struct {
                fields: fields.clone()
            })),
            20
        );
        fields.insert(
            "max_iterations".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::NumberValue(15.0)),
            },
        );
        assert_eq!(resolve_max_iterations(Some(&Struct { fields })), 15);
        assert_eq!(resolve_max_iterations(None), DEFAULT_MAX_ITERATIONS);
    }

    fn termination_inputs(value: &str) -> Struct {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "termination".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(value.to_string())),
            },
        );
        Struct { fields }
    }

    #[test]
    fn termination_input_is_read_and_defaults_to_unset() {
        assert_eq!(resolve_termination(None), Termination::Unset);
        assert_eq!(
            resolve_termination(Some(&termination_inputs("explicit_claim"))),
            Termination::ExplicitClaim
        );
        assert_eq!(
            resolve_termination(Some(&termination_inputs("output_satisfied"))),
            Termination::OutputSatisfied
        );
        assert_eq!(
            resolve_termination(Some(&termination_inputs("junk"))),
            Termination::Unset
        );
    }

    #[test]
    fn task_complete_is_offered_last_and_only_under_an_explicit_claim() {
        let runtime = SkillsRuntime::default();
        let allowed = vec!["read_file".to_string()];
        let unset = rebuild_tools_list(&runtime, &allowed, "", false, Termination::Unset);
        assert!(!unset.iter().any(|t| t.name == "task_complete"));
        let satisfied =
            rebuild_tools_list(&runtime, &allowed, "", false, Termination::OutputSatisfied);
        assert!(!satisfied.iter().any(|t| t.name == "task_complete"));
        let explicit =
            rebuild_tools_list(&runtime, &allowed, "", false, Termination::ExplicitClaim);
        assert_eq!(
            explicit.last().map(|t| t.name.as_str()),
            Some("task_complete")
        );
        assert_eq!(explicit.len(), unset.len() + 1);
        // Nothing advertised: the list stays empty so the builtin fallback applies.
        assert!(
            rebuild_tools_list(&runtime, &[], "", false, Termination::ExplicitClaim).is_empty()
        );
    }

    #[test]
    fn jira_demo_argument_rewrite_is_gone() {
        let mut args = serde_json::json!({"issue_key": "CGA-1"});
        vam_spec::text_tool_calls::normalize_tool_arguments("jira_search_issues", &mut args);
        assert_eq!(args, serde_json::json!({"issue_key": "CGA-1"}));
        assert_eq!(
            vam_spec::text_tool_calls::cmd_array_to_arguments(
                "jira_search_issues",
                &[serde_json::json!("key = CGA-1")]
            ),
            serde_json::json!({"input": "key = CGA-1"}).to_string()
        );
        let calls = extract_tool_calls_from_text(r#"{"tool":"jira_get_issue","arguments":{}}"#);
        assert_eq!(calls[0].name, "jira_get_issue");
    }

    #[test]
    fn max_iterations_clamps_to_hard_cap() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "max_iterations".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::NumberValue(1000.0)),
            },
        );
        assert_eq!(
            resolve_max_iterations(Some(&Struct { fields })),
            MAX_ITERATIONS_HARD_CAP
        );
    }

    #[test]
    fn iteration_budget_exhausts_at_max_iterations() {
        assert_eq!(remaining_iterations(5, 0), 5);
        assert_eq!(remaining_iterations(5, 4), 1);
        assert_eq!(remaining_iterations(5, 5), 0);
        assert_eq!(remaining_iterations(9, 3), 6);
        assert_eq!(remaining_iterations(3, 10), 0);
    }

    #[test]
    fn a_host_that_cannot_record_the_conversation_fails_the_task_as_a_storage_failure() {
        let lease = ExecutionLease::default();
        let err = |message: &str| vam_spec::v1::ErrorStatus {
            code: 8,
            message: message.to_string(),
            details: vec![],
            retryable: false,
        };
        for reason in ["thread_log_quota_exceeded", "thread_log_unavailable"] {
            let outcome =
                inference_failed_outcome(&lease, err(&format!("{reason}: the log is full")));
            let message = outcome.error.expect("an error").message;
            assert!(message.starts_with(STORAGE_COMMIT_FAILED), "{message}");
            assert!(message.contains(reason), "{message}");
        }
        // Any other inference failure keeps its own wording.
        let other = inference_failed_outcome(&lease, err("budget exhausted"))
            .error
            .expect("an error")
            .message;
        assert_eq!(other, "inference failed: budget exhausted");
    }

    #[test]
    fn the_empty_reply_nudge_is_neutral_and_names_no_tool() {
        let nudge = empty_response_nudge();
        assert!(nudge.contains("empty"));
        for banned in [
            "Call tool",
            "Call the required tool",
            "load_skill",
            "describe planned steps",
        ] {
            assert!(!nudge.contains(banned), "nudge must not say {banned:?}");
        }
    }

    #[test]
    fn extract_tool_json_from_prose() {
        let text = r#"We need load_skill.{"tool":"load_skill","arguments":{"catalog_id":"jira-integration"}}{"tool":"jira_search_issues","arguments":{"jql":"key = CGA-1298"}}"#;
        let calls = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "load_skill");
        assert!(calls[0].arguments.contains("jira-integration"));
        assert_eq!(calls[1].name, "jira_search_issues");
    }

    #[test]
    fn extract_cmd_array_tool_from_text() {
        let text = r#"{"cmd":["load_skill","jira-integration"]}"#;
        let calls = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "load_skill");
        assert!(calls[0].arguments.contains("jira-integration"));
    }

    #[test]
    fn extract_tool_input_string_from_text() {
        let text = r#"{"tool":"load_skill","tool_input":"jira-integration"}{"tool":"jira_search_issues","tool_input":"key = CGA-1298"}"#;
        let calls = extract_tool_calls_from_text(text);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "load_skill");
        assert!(calls[0].arguments.contains("jira-integration"));
        assert_eq!(calls[1].name, "jira_search_issues");
        assert!(calls[1].arguments.contains("CGA-1298"));
    }

    #[test]
    fn build_system_prompt_renders_template_with_tools() {
        let tools = crate::primitives::builtin_tool_specs();
        let ws = crate::primitives::workspace_root();
        let catalog = format_skills_catalog(&[]);
        let prompt = instruction::build_system_prompt(&ws, None, &tools, &catalog, "");
        assert!(prompt.contains("write_file"));
        assert!(prompt.contains("glob"));
        assert!(prompt.contains("load_skill"));
        assert!(prompt.contains("tool_error:"));
    }

    #[test]
    fn two_system_prompt_and_tool_builds_are_identical() {
        let runtime = SkillsRuntime::default();
        let allowed = vec!["read_file".to_string(), "write_file".to_string()];
        let ws = std::path::Path::new("/nonexistent-workspace");
        let build = || {
            let tools =
                rebuild_tools_list(&runtime, &allowed, "", false, Termination::ExplicitClaim);
            let prompt =
                build_agent_system_prompt(ws, None, &tools, "", &runtime.extensions_text());
            (prompt, tools)
        };
        let (first_prompt, first_tools) = build();
        let (second_prompt, second_tools) = build();
        assert_eq!(first_prompt, second_prompt);
        assert_eq!(first_tools, second_tools);
    }

    #[test]
    fn build_system_prompt_merges_agent_instructions() {
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "system_instructions".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "Call load_skill before calculator.".into(),
                )),
            },
        );
        fields.insert(
            "agent_role".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue("Math Helper".into())),
            },
        );
        let inputs = prost_types::Struct { fields };
        let tools = crate::primitives::builtin_tool_specs();
        let ws = crate::primitives::workspace_root();
        let catalog = "- **Calculator** (`assistant-tools`): Math tools";
        let prompt = instruction::build_system_prompt(&ws, Some(&inputs), &tools, catalog, "");
        assert!(prompt.contains("write_file"), "tool inventory preserved");
        assert!(prompt.contains("Call load_skill before calculator."));
        assert!(prompt.contains("Math Helper"));
        assert!(prompt.contains("assistant-tools"));
    }

    #[test]
    fn boot_tools_exclude_unloaded_skill_tools() {
        let runtime = SkillsRuntime::default();
        let allowed: Vec<String> = crate::primitives::builtin_tool_specs()
            .into_iter()
            .map(|t| t.name)
            .chain(["calculator".to_string()])
            .collect();
        let tools = rebuild_tools_list(&runtime, &allowed, "", false, Termination::Unset);
        assert!(tools.iter().any(|t| t.name == "load_skill"));
        assert!(!tools.iter().any(|t| t.name == "calculator"));
        assert!(!tools
            .iter()
            .any(|t| t.name == skills::STEP_COMPLETE_TOOL_NAME));
    }

    #[test]
    fn after_load_skill_tools_intersect_cedar() {
        let _guard = crate::skills::skills_mount_test_guard();

        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        std::env::set_var("VAM_SKILLS_MOUNT", &mount);
        std::fs::create_dir_all(&mount).unwrap();
        let skill_dir = mount.join("assistant-tools");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILLS.md"),
            "# Calculator\n\nUse calculator for math.",
        )
        .unwrap();
        std::fs::write(
            skill_dir.join("manifest.json"),
            r#"{"tools":[{"name":"calculator","description":"Math","input_schema_json":"{}"}]}"#,
        )
        .unwrap();

        let mut runtime = SkillsRuntime::default();
        runtime.load("assistant-tools").unwrap();

        let allowed = vec![
            "load_skill".to_string(),
            "read_file".to_string(),
            "calculator".to_string(),
        ];
        let tools = rebuild_tools_list(&runtime, &allowed, "", false, Termination::Unset);
        assert!(tools.iter().any(|t| t.name == "calculator"));

        let denied = vec!["load_skill".to_string(), "read_file".to_string()];
        let tools = rebuild_tools_list(&runtime, &denied, "", false, Termination::Unset);
        assert!(!tools.iter().any(|t| t.name == "calculator"));
    }

    #[test]
    fn preloaded_mcp_tool_is_offered_on_turn_one_and_cedar_still_hides_denied_ones() {
        let _guard = crate::skills::skills_mount_test_guard();

        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        std::env::set_var("VAM_SKILLS_MOUNT", &mount);
        let skill_dir = mount.join("mcp-notion");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILLS.md"),
            "# mcp-notion\n\nUse the tools.",
        )
        .unwrap();
        std::fs::write(
            skill_dir.join("manifest.json"),
            serde_json::json!({
                "preloaded": true,
                "tools": [
                    {"name": "notion__create_page", "description": "Create", "input_schema_json": "{}",
                     "host_exec": {"adapter_id": "mcp:notion:create_page"}},
                    {"name": "notion__delete_page", "description": "Delete", "input_schema_json": "{}",
                     "host_exec": {"adapter_id": "mcp:notion:delete_page"}}
                ]
            })
            .to_string(),
        )
        .unwrap();

        let mut runtime = SkillsRuntime::default();
        runtime.preload();
        let allowed = vec![
            "load_skill".to_string(),
            "read_file".to_string(),
            "notion__create_page".to_string(),
        ];
        let tools = rebuild_tools_list(&runtime, &allowed, "", false, Termination::Unset);
        assert!(tools.iter().any(|t| t.name == "notion__create_page"));
        assert!(
            !tools.iter().any(|t| t.name == "notion__delete_page"),
            "a Cedar-denied tool is still hidden"
        );
        // The first tool call needs no load_skill: the tool is already known.
        assert!(ensure_skill_for_tool(&mut runtime, "notion__create_page").is_ok());
        assert!(is_host_exec_tool("notion__create_page", &runtime, ""));
        assert!(!runtime.loaded_skill_ids().is_empty());

        let inventory = instruction::format_tools_inventory(&tools);
        assert!(inventory.contains("notion__create_page"));
        assert!(
            !inventory.contains("Create\n"),
            "no description copy: {inventory}"
        );
    }

    #[test]
    fn step_complete_tool_present_only_with_playbook_procedure() {
        let runtime = SkillsRuntime::default();
        let allowed = vec![
            "load_skill".to_string(),
            "read_file".to_string(),
            skills::STEP_COMPLETE_TOOL_NAME.to_string(),
            skills::RUN_STEP_TOOL_NAME.to_string(),
        ];
        let without = rebuild_tools_list(&runtime, &allowed, "", false, Termination::Unset);
        assert!(!without
            .iter()
            .any(|t| t.name == skills::STEP_COMPLETE_TOOL_NAME));
        assert!(!without.iter().any(|t| t.name == skills::RUN_STEP_TOOL_NAME));
        assert!(!is_host_exec_tool(
            skills::STEP_COMPLETE_TOOL_NAME,
            &runtime,
            ""
        ));

        let with = rebuild_tools_list(
            &runtime,
            &allowed,
            "1. Search issues",
            false,
            Termination::Unset,
        );
        let spec = with
            .iter()
            .find(|t| t.name == skills::STEP_COMPLETE_TOOL_NAME)
            .expect("step_complete injected");
        assert!(matches!(
            spec.execution_provider,
            Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(_))
        ));
        assert!(is_host_exec_tool(
            skills::STEP_COMPLETE_TOOL_NAME,
            &runtime,
            "1. Search issues"
        ));
        assert_eq!(
            resolve_script_digest(
                skills::STEP_COMPLETE_TOOL_NAME,
                &Struct::default(),
                &runtime,
                "1. Search issues"
            ),
            Some(vam_spec::validation::host_tool_digest(
                skills::STEP_COMPLETE_TOOL_NAME
            ))
        );
        let run_step = with
            .iter()
            .find(|t| t.name == skills::RUN_STEP_TOOL_NAME)
            .expect("run_step injected");
        assert!(matches!(
            run_step.execution_provider,
            Some(vam_spec::v1::tool_spec::ExecutionProvider::HostExec(_))
        ));
        assert!(is_host_exec_tool(
            skills::RUN_STEP_TOOL_NAME,
            &runtime,
            "1. Search issues"
        ));
        assert_eq!(
            resolve_script_digest(
                skills::RUN_STEP_TOOL_NAME,
                &Struct::default(),
                &runtime,
                "1. Search issues"
            ),
            Some(vam_spec::validation::host_tool_digest(
                skills::RUN_STEP_TOOL_NAME
            ))
        );
    }

    #[test]
    fn catalog_string_has_no_schemas_or_body() {
        let catalog = format_skills_catalog(&[SkillCatalogEntry {
            skill_id: "assistant-tools".into(),
            name: "Calculator".into(),
            description: "Math evaluation".into(),
            preloaded: false,
        }]);
        assert!(catalog.contains("assistant-tools"));
        assert!(catalog.contains("Math evaluation"));
        assert!(!catalog.contains("input_schema"));
        assert!(!catalog.contains("Use calculator"));
    }

    #[test]
    fn write_host_artifact_sanitizes_path() {
        let _guard = crate::skills::skills_mount_test_guard();

        let dir = tempfile::tempdir().unwrap();
        let written = write_host_artifact(dir.path(), "../../etc/passwd", b"data").unwrap();
        assert!(written.starts_with(".vam/host-artifacts/"));
        assert!(!written.contains(".."));
        let artifact_path = dir.path().join(written);
        assert!(artifact_path.is_file());
    }

    #[test]
    fn filter_tools_by_advertised_keeps_only_allowed_names() {
        let candidates = vec![
            vam_spec::v1::ToolSpec {
                name: "read_file".to_string(),
                description: "read".to_string(),
                input_schema_json: String::new(),
                output_schema_json: String::new(),
                side_effect: 0,
                timeout: None,
                execution_provider: None,
                required_secrets: vec![],
                ..Default::default()
            },
            vam_spec::v1::ToolSpec {
                name: "write_file".to_string(),
                description: "write".to_string(),
                input_schema_json: String::new(),
                output_schema_json: String::new(),
                side_effect: 0,
                timeout: None,
                execution_provider: None,
                required_secrets: vec![],
                ..Default::default()
            },
        ];
        let allowed = vec!["read_file".to_string(), "glob".to_string()];
        let filtered = filter_tools_by_advertised(candidates, &allowed);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "read_file");
    }

    #[test]
    fn filter_tools_by_advertised_empty_allowlist_is_fail_closed() {
        let candidates = crate::primitives::builtin_tool_specs();
        let filtered = filter_tools_by_advertised(candidates, &[]);
        assert!(filtered.is_empty());
    }

    #[test]
    fn prompt_inventory_matches_filtered_tool_count() {
        let candidates = crate::primitives::builtin_tool_specs();
        let allowed = vec![
            "read_file".to_string(),
            "glob".to_string(),
            "load_skill".to_string(),
        ];
        let filtered = filter_tools_by_advertised(candidates, &allowed);
        let inventory = crate::instruction::format_tools_inventory(&filtered);
        let lines: Vec<&str> = inventory.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), filtered.len());
    }

    #[test]
    fn compact_tool_output_prefers_file_pointer() {
        let blob = format!("research dump\nnotes/research.md\n{}", "x".repeat(3000));
        let compacted = compact_tool_output(&blob);
        assert!(compacted.contains("read_file notes/research.md"));
        assert!(compacted.len() < blob.len());
    }

    #[test]
    fn compact_tool_output_retains_skills_mount_pointer() {
        let blob = format!(
            "Source: /mnt/drive9/skills/oracle-sql/docs/po_headers_all.md\n{}\n",
            "x".repeat(3000)
        );
        let compacted = compact_tool_output(&blob);
        assert!(compacted
            .contains("read_file /mnt/drive9/skills/oracle-sql/docs/po_headers_all.md if needed"));
        assert!(!compacted.contains("not on disk"));
    }

    #[test]
    fn compact_tool_output_prefers_grep_hit_file_over_search_root() {
        let blob = format!(
            "Source: /mnt/drive9/skills/oracle-sql/docs\n/mnt/drive9/skills/oracle-sql/docs/tables/po_headers_all.md:12:CREATE TABLE\n{}",
            "y".repeat(3000)
        );
        let compacted = compact_tool_output(&blob);
        assert!(compacted.contains(
            "read_file /mnt/drive9/skills/oracle-sql/docs/tables/po_headers_all.md if needed"
        ));
        assert!(!compacted.contains("not on disk"));
    }

    #[test]
    fn host_artifact_response_omits_large_body() {
        let _guard = crate::skills::skills_mount_test_guard();

        let dir = tempfile::tempdir().unwrap();
        let body = "secret-payload".repeat(500);
        let written = write_host_artifact(dir.path(), "payload.bin", body.as_bytes()).unwrap();
        let msg = format_host_tool_response("", &[written], MAX_TEXT_RESULT_NO_ARTIFACT);
        assert!(msg.contains(".vam/host-artifacts/"));
        assert!(!msg.contains("secret-payload"));
    }

    #[test]
    fn cap_text_result_says_how_much_was_kept() {
        let big = "z".repeat(MAX_TEXT_RESULT_NO_ARTIFACT + 100);
        let capped = cap_text_result(&big, MAX_TEXT_RESULT_NO_ARTIFACT);
        assert!(capped.contains(&format!(
            "[truncated: kept {} of {} bytes",
            MAX_TEXT_RESULT_NO_ARTIFACT,
            big.len()
        )));
    }

    #[test]
    fn fenced_result_up_to_the_host_bound_keeps_its_closing_line() {
        // vamd sends at most 8 KiB of text plus its fence; the guest cap must
        // not cut the end line of that.
        let body = "q".repeat(8 * 1024);
        let fenced =
            format!("<<<VAM-UNTRUSTED-DATA v1 id=x>>>\n{body}\n<<<END-VAM-UNTRUSTED-DATA id=x>>>");
        let out = format_host_tool_response(&fenced, &[], MAX_TEXT_RESULT_NO_ARTIFACT);
        assert_eq!(out, fenced);
        let with_file = format_host_tool_response(
            &fenced,
            &[".vam/host-artifacts/a.md".into()],
            MAX_TEXT_RESULT_NO_ARTIFACT,
        );
        assert!(with_file.contains("<<<END-VAM-UNTRUSTED-DATA id=x>>>"));
        assert!(with_file.ends_with("artifact written to .vam/host-artifacts/a.md"));
    }

    #[test]
    fn compaction_names_the_file_and_ignores_a_decoy_in_vendor_text() {
        let decoy = "artifact written to /etc/passwd";
        let content = format!(
            "<<<VAM-UNTRUSTED-DATA v1 id=x>>>\n{decoy}\n{}\n<<<END-VAM-UNTRUSTED-DATA id=x>>>\nartifact written to .vam/host-artifacts/mcp-n-search-ab12cd34.md",
            "x".repeat(3000)
        );
        let compacted = compact_tool_output(&content);
        assert!(compacted.contains(".vam/host-artifacts/mcp-n-search-ab12cd34.md"));
        assert!(!compacted.contains("passwd"));
        assert!(!compacted.contains("not on disk"));

        let two = "preview\nartifact written to .vam/host-artifacts/a.md\nartifact written to .vam/host-artifacts/b.png";
        let compacted = compact_tool_output(two);
        assert!(compacted.contains("a.md") && compacted.contains("b.png"));

        assert!(compact_tool_output(&"y".repeat(3000)).contains("not on disk"));
    }

    #[test]
    fn extra_artifacts_are_written_and_listed() {
        let _guard = crate::skills::skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let first = write_host_artifact(dir.path(), "mcp-n-t-1.md", b"full").unwrap();
        let second = write_host_artifact(dir.path(), "mcp-n-t-1-1.png", &[0x89, b'P']).unwrap();
        let msg = format_host_tool_response(
            "preview",
            &[first.clone(), second.clone()],
            MAX_TEXT_RESULT_NO_ARTIFACT,
        );
        assert!(msg.starts_with("preview\n"));
        assert!(msg.contains(&format!("artifact written to {first}")));
        assert!(msg.contains(&format!("artifact written to {second}")));
        assert_eq!(
            std::fs::read(dir.path().join(&second)).unwrap(),
            vec![0x89, b'P']
        );
    }

    #[test]
    fn a_lower_result_cap_truncates_sooner() {
        let text = "q".repeat(5000);
        assert_eq!(cap_text_result(&text, MAX_TEXT_RESULT_NO_ARTIFACT), text);
        let capped = cap_text_result(&text, 1024);
        assert!(capped.contains("[truncated: kept 1024 of 5000 bytes"));
    }

    #[test]
    fn harness_defaults_equal_the_constants_the_loop_used() {
        let h = vam_spec::harness::Harness::default();
        assert_eq!(h.calls.result_cap as usize, MAX_TEXT_RESULT_NO_ARTIFACT);
        assert_eq!(
            u64::from(h.context.compaction.trigger_percent),
            crate::compaction::TRIGGER_PERCENT
        );
        assert_eq!(
            h.context.compaction.keep_rounds as usize,
            crate::compaction::KEEP_ROUNDS
        );
        assert_eq!(
            h.context.clearing.every_rounds as usize,
            crate::context_window::CLEAR_EVERY_ROUNDS
        );
        assert_eq!(h.context.prompt_variant, "v11");
        assert!(crate::instruction::DEFAULT_TEMPLATE.contains("template_version: 11"));
        assert!(h.context.reminders);
    }

    #[test]
    fn cap_text_result_truncates_without_artifact() {
        let big = "z".repeat(MAX_TEXT_RESULT_NO_ARTIFACT + 100);
        let capped = cap_text_result(&big, MAX_TEXT_RESULT_NO_ARTIFACT);
        assert!(capped.len() < big.len());
        assert!(capped.contains("write results to notes/"));
    }
}

/// Provider for the run's model. The host decides it at run creation and writes
/// `model_provider`; the guest never guesses from the model id. Only the mock model is
/// recognised locally. With no host value (a guest started without a host, as in unit tests)
/// the provider is empty and the broker's catalog decides by model name alone.
fn resolve_model_provider(model: &str, inputs: &prost_types::Struct) -> String {
    if model == "mock-model" {
        return "mock".to_string();
    }
    if let Some(prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(p)),
    }) = inputs.fields.get("model_provider")
    {
        if !p.trim().is_empty() {
            return p.trim().to_string();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    fn user_msg(text: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: text.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }
    }

    /// An approved tool that failed to start (or answered an error) is not reported to
    /// the model as executed; a successful one keeps the host's note.
    #[test]
    fn a_failed_approved_tool_is_not_called_executed() {
        let note = super::default_hitl_resume_nudge();
        let spawn_failed = "error: Tool execution invocation failed: task handler failed: \
                            Failed to spawn child: Permission denied (os error 13)";
        for failed in [
            spawn_failed,
            "error (exit code 1): \nstderr: {\"error\":\"Microsoft Graph answered 500\"}",
            "tool_error: mail_send: the journal is unavailable",
            "Error: Tool 'mail_send' was denied by security policy: no",
        ] {
            let after = super::resume_nudge_after("mail_send", failed, note.clone());
            assert!(after.starts_with("Human approval was granted"), "{after}");
            assert!(!after.contains("already executed"), "{after}");
            assert!(
                after.contains("did not complete") && after.contains("mail_send"),
                "{after}"
            );
        }
        let ok = super::resume_nudge_after("mail_send", "{\"sent\":true}", note.clone());
        assert_eq!(ok, note);
        // A question's answer is never an error to report.
        let answer = super::resume_nudge_after(
            vam_spec::outbox::ASK_USER_TOOL,
            "error-free answer",
            String::new(),
        );
        assert!(answer.is_empty());
    }

    #[test]
    fn recitation_off_request_is_byte_identical() {
        use prost::Message;
        let messages = vec![user_msg("sys"), user_msg("hello")];
        let got = super::build_inference_request(&None, &messages, &[], None);
        let want = InferenceRequest {
            model_preference: None,
            messages: messages.clone(),
            tools: vec![],
            context_hint: None,
        };
        assert_eq!(got.encode_to_vec(), want.encode_to_vec());
        assert!(got.context_hint.is_none());
    }

    #[test]
    fn recitation_on_adds_one_labelled_last_note_and_leaves_messages_alone() {
        let messages = vec![user_msg("sys"), user_msg("hello")];
        let before = messages.clone();
        let got = super::build_inference_request(&None, &messages, &[], Some("- [ ] water"));
        assert_eq!(messages, before, "the guest's messages are never changed");
        assert_eq!(got.messages.len(), 3);
        assert_eq!(&got.messages[..2], &messages[..]);
        let note = got.messages.last().unwrap();
        assert_eq!(note.role, "user");
        assert_eq!(
            note.content,
            format!("{}- [ ] water", vam_spec::compaction::NOTE_LABEL)
        );
        assert!(vam_spec::compaction::is_recitation_note(note));
        let hint = got.context_hint.unwrap();
        assert_eq!(hint.lane, vam_spec::v1::context_hint::Lane::Note as i32);
    }

    #[test]
    fn recitation_note_is_capped() {
        let plan = "x".repeat(10_000);
        let note = vam_spec::compaction::recitation_note(&plan);
        assert_eq!(
            note.content.len(),
            vam_spec::compaction::NOTE_LABEL.len() + 4 * 1024
        );
        assert!(vam_spec::compaction::is_recitation_note(&note));
    }

    #[test]
    fn summary_request_never_carries_the_note() {
        let messages = vec![user_msg("sys"), user_msg("hello")];
        let summary = crate::compaction::summary_request_messages(&messages, Some("- [ ] a"));
        assert!(!summary
            .iter()
            .any(|m| m.content.starts_with(vam_spec::compaction::NOTE_LABEL)));
    }

    #[test]
    fn checked_tool_args_validates_builtins_and_never_coerces() {
        use super::checked_tool_args;
        let tools = vec![skills::task_complete_tool_spec()];
        // validate on: missing required is typed.
        let err = checked_tool_args("read_file", "{}", true, &[]).unwrap_err();
        assert_eq!(
            err.message().split('.').next().unwrap(),
            "invalid arguments for read_file: missing required field 'path'"
        );
        // validate off: schema checks are skipped, `{}` passes as the object it is.
        assert!(checked_tool_args("read_file", "{}", false, &[]).is_ok());
        assert!(checked_tool_args("read_file", r#"{"path":"a","offset":"x"}"#, false, &[]).is_ok());
        assert!(checked_tool_args("read_file", r#"{"path":"a","offset":"x"}"#, true, &[]).is_err());
        // unparsable and non-object arguments are errors with or without validate.
        for validate in [true, false] {
            for raw in [r#"{"path": "#, "[]", "5", "\"s\""] {
                assert!(
                    checked_tool_args("read_file", raw, validate, &[]).is_err(),
                    "{raw}"
                );
                assert!(checked_tool_args("some_host_tool", raw, validate, &[]).is_err());
            }
        }
        // empty arguments are `{}`; non-builtin tools keep their host check.
        assert!(checked_tool_args("some_host_tool", "", true, &[]).is_ok());
        assert!(checked_tool_args("some_host_tool", "{}", true, &[]).is_ok());
        // task_complete: required waived, types still checked.
        let name = skills::TASK_COMPLETE_TOOL_NAME;
        assert!(checked_tool_args(name, "{}", true, &tools).is_ok());
        assert!(checked_tool_args(name, r#"{"summary":"done"}"#, true, &tools).is_ok());
        assert!(checked_tool_args(name, r#"{"summary":3}"#, true, &tools).is_err());
    }

    use super::*;
    use std::sync::Arc;
    use tokio::net::UnixListener;
    use vam_inference::{
        InferenceBrokerServer, MockScript, MockScripts, ModelCatalog, SessionBudget,
        SimpleSecretLookup,
    };
    use vam_spec::v1::{ExecutionLease, PromptAction, TaskOutcomeState};

    fn model_inputs(pairs: &[(&str, &str)]) -> prost_types::Struct {
        prost_types::Struct {
            fields: pairs
                .iter()
                .map(|(k, v)| {
                    (
                        k.to_string(),
                        prost_types::Value {
                            kind: Some(prost_types::value::Kind::StringValue(v.to_string())),
                        },
                    )
                })
                .collect(),
        }
    }

    #[test]
    fn model_provider_comes_from_the_host_and_is_never_guessed() {
        let m = "anthropic/claude-haiku-5.5";
        let with = model_inputs(&[("model", m), ("model_provider", "vercel")]);
        assert_eq!(resolve_model_provider(m, &with), "vercel");
        // No host value: the guest does not guess from the slash.
        let without = model_inputs(&[("model", m)]);
        assert_eq!(resolve_model_provider(m, &without), "");
        let empty = model_inputs(&[("model", m), ("model_provider", " ")]);
        assert_eq!(resolve_model_provider(m, &empty), "");
        let bare = model_inputs(&[("model", "gpt")]);
        assert_eq!(resolve_model_provider("gpt", &bare), "");
    }

    #[test]
    fn mock_model_ignores_model_provider_input() {
        let i = model_inputs(&[("model", "mock-model"), ("model_provider", "vercel")]);
        assert_eq!(resolve_model_provider("mock-model", &i), "mock");
    }

    #[test]
    fn parse_hitl_denied_inputs_reads_tool_reason_and_args() {
        let mut tool_args = Struct::default();
        tool_args.fields.insert(
            "issue_key".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "CGA-1299".to_string(),
                )),
            },
        );
        let mut inputs = Struct::default();
        inputs.fields.insert(
            "hitl_denied".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        inputs.fields.insert(
            "denied_tool".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "jira_transition_issue".to_string(),
                )),
            },
        );
        inputs.fields.insert(
            "denied_tool_args".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StructValue(tool_args)),
            },
        );
        inputs.fields.insert(
            "denial_reason".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "Denied by user".to_string(),
                )),
            },
        );
        let (tool, args, reason) =
            parse_hitl_denied_inputs(Some(&inputs)).expect("denied resume inputs");
        assert_eq!(tool, "jira_transition_issue");
        assert_eq!(reason, "Denied by user");
        assert_eq!(
            args.fields.get("issue_key").and_then(|v| match &v.kind {
                Some(prost_types::value::Kind::StringValue(s)) => Some(s.as_str()),
                _ => None,
            }),
            Some("CGA-1299")
        );
    }

    #[test]
    fn hitl_resume_without_struct_args_falls_through_to_react_loop() {
        let mut inputs = Struct::default();
        inputs.fields.insert(
            "hitl_resume".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        inputs.fields.insert(
            "approved_tool".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "jira_transition_issue".to_string(),
                )),
            },
        );
        inputs.fields.insert(
            "prompt".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "Transition issue CGA-1298 to Done".to_string(),
                )),
            },
        );
        assert!(
            parse_hitl_resume_inputs(Some(&inputs)).is_none(),
            "missing approved_tool_args must not short-circuit the ReAct loop"
        );
    }

    #[test]
    fn hitl_resume_note_is_read_from_inputs() {
        let mut inputs = Struct::default();
        inputs.fields.insert(
            "hitl_resume_note".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "Summarize without re-invoking.".to_string(),
                )),
            },
        );
        assert_eq!(
            hitl_resume_note(Some(&inputs)).as_deref(),
            Some("Summarize without re-invoking.")
        );
    }

    #[test]
    fn hitl_decisions_note_is_read_from_inputs_and_blank_is_none() {
        let mut inputs = Struct::default();
        assert_eq!(hitl_decisions_note(Some(&inputs)), None);
        inputs.fields.insert(
            "hitl_decisions_note".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue("   ".to_string())),
            },
        );
        assert_eq!(hitl_decisions_note(Some(&inputs)), None);
        inputs.fields.insert(
            "hitl_decisions_note".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "Gate 'confirm' was decided with outcome 'go'.".to_string(),
                )),
            },
        );
        assert_eq!(
            hitl_decisions_note(Some(&inputs)).as_deref(),
            Some("Gate 'confirm' was decided with outcome 'go'.")
        );
    }

    #[test]
    fn parse_hitl_resume_inputs_requires_struct_args() {
        let mut inputs = Struct::default();
        inputs.fields.insert(
            "hitl_resume".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        inputs.fields.insert(
            "approved_tool".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "jira_transition_issue".to_string(),
                )),
            },
        );
        assert!(parse_hitl_resume_inputs(Some(&inputs)).is_none());

        let mut tool_args = Struct::default();
        tool_args.fields.insert(
            "issue_key".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    "CGA-1298".to_string(),
                )),
            },
        );
        inputs.fields.insert(
            "approved_tool_args".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StructValue(tool_args)),
            },
        );
        let (tool, args) = parse_hitl_resume_inputs(Some(&inputs)).expect("resume inputs");
        assert_eq!(tool, "jira_transition_issue");
        assert!(args.fields.contains_key("issue_key"));
    }

    /// Serializes the tests that stand in for the host on the guest transport.
    /// Holding it selects the explicit Unix transport with a private base path
    /// (`<base>_<port>`, as the host lays it out); dropping it restores vsock.
    struct MockBroker {
        _guard: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
    }

    impl Drop for MockBroker {
        fn drop(&mut self) {
            vam_vsock::guest::set_transport(vam_vsock::GuestTransport::Vsock);
        }
    }

    fn mock_broker_lock() -> MockBroker {
        static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
        let guard = LOCK
            .get_or_init(|| std::sync::Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().expect("mock vsock dir");
        vam_vsock::guest::set_transport(vam_vsock::GuestTransport::Uds {
            base: dir.path().join("vsock"),
        });
        MockBroker {
            _guard: guard,
            _dir: dir,
        }
    }

    /// Socket path the guest dials for `port` under the current mock transport.
    fn mock_port_path(port: u32) -> String {
        match vam_vsock::guest::transport() {
            vam_vsock::GuestTransport::Uds { base } => format!("{}_{}", base.display(), port),
            other => panic!("mock_broker_lock() not held: transport is {other:?}"),
        }
    }

    fn checkpoint_test_lease() -> ExecutionLease {
        ExecutionLease {
            run_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:run:local.vam.dev/acme/run-socket".to_string(),
            }),
            step_id: "step-socket".to_string(),
            attempt_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:attempt:socket".to_string(),
            }),
            lease_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:lease:socket".to_string(),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: None,
            idempotency_key: "socket".to_string(),
        }
    }

    #[cfg(unix)]
    fn bind_mock_port(port: u32) -> UnixListener {
        let path = mock_port_path(port);
        let _ = std::fs::remove_file(&path);
        UnixListener::bind(&path).expect("bind mock port")
    }

    #[cfg(unix)]
    fn unbind_mock_port(port: u32) {
        let _ = std::fs::remove_file(mock_port_path(port));
    }

    /// Serve every port-52 connection the way the host does.
    #[cfg(unix)]
    fn spawn_checkpoint_host(
        listener: UnixListener,
        host: crate::pep::test_support::SharedMockHost,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                tokio::spawn(crate::pep::test_support::serve(stream, host.clone()));
            }
        })
    }

    /// Serve port-53 inference connections with the mock-model broker.
    #[cfg(unix)]
    fn spawn_inference_broker(listener: UnixListener) -> tokio::task::JoinHandle<()> {
        let catalog = Arc::new(ModelCatalog::new());
        let budget = Arc::new(SessionBudget::new(None));
        let secrets = Arc::new(SimpleSecretLookup::new());
        let broker = Arc::new(InferenceBrokerServer::new(catalog, budget, secrets));
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let broker = broker.clone();
                tokio::spawn(async move { broker.handle_connection(stream).await });
            }
        })
    }

    /// Serve port-53 inference connections with the mock broker, answering scripted requests.
    #[cfg(unix)]
    fn spawn_scripted_inference_broker(
        listener: UnixListener,
        scripts: Arc<MockScripts>,
    ) -> tokio::task::JoinHandle<()> {
        let catalog = Arc::new(ModelCatalog::new());
        let budget = Arc::new(SessionBudget::new(None));
        let secrets = Arc::new(SimpleSecretLookup::new());
        let broker = Arc::new(
            InferenceBrokerServer::new(catalog, budget, secrets).with_mock_scripts(scripts),
        );
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let broker = broker.clone();
                tokio::spawn(async move { broker.handle_connection(stream).await });
            }
        })
    }

    /// Accept port-53 connections and never answer them, so a model call hangs.
    #[cfg(unix)]
    fn spawn_silent_broker(listener: UnixListener) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        })
    }

    /// Run an async socket-backed test body on its own runtime.
    #[cfg(unix)]
    fn run_socket_test<F: std::future::Future<Output = ()>>(body: F) {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("test runtime")
            .block_on(body);
    }

    #[cfg(unix)]
    fn resume_checkpoint() -> HitlContinuation {
        HitlContinuation {
            schema_version: hitl_checkpoint::SCHEMA_VERSION,
            run_id: "urn:vam:run:local.vam.dev/acme/run-socket".to_string(),
            lifecycle_generation: 1,
            messages: vec![],
            loaded_skill_ids: vec![],
            iterations_used: 1,
            max_iterations: 50,
            prompt_tokens: 3,
            completion_tokens: 4,
            cached_tokens: 0,
            total_tokens: 7,
            nudge_count: 0,
            last_tool_name: String::new(),
            same_tool_rounds: 0,
            skill_tool_called: false,
            last_answer_candidate: String::new(),
            pending_tool_call_id: "call_pending".to_string(),
            pending_tool_name: "echo.ping".to_string(),
            pending_tool_args_json: "{}".to_string(),
            unanswered_tool_call_ids: vec![],
            claim_reprompted: false,
            inbox_seq: 0,
        }
    }

    /// The re-intent of a resumed approval, answered by the mock host as `setup` says.
    #[cfg(unix)]
    async fn resumed_reintent(
        setup: impl FnOnce(&mut crate::pep::test_support::MockCheckpointHost),
    ) -> (
        Result<String, TaskOutcome>,
        crate::pep::test_support::SharedMockHost,
    ) {
        let host = crate::pep::test_support::shared_host(None);
        setup(&mut host.lock().expect("host"));
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let checkpoint = resume_checkpoint();
        let mut skills = SkillsRuntime::default();
        let result = execute_hitl_approved_tool_observation(
            &checkpoint_test_lease(),
            "echo.ping",
            &Struct::default(),
            &mut skills,
            "",
            4096,
            "call_pending",
            Some(&checkpoint),
        )
        .await;
        serving_52.abort();
        unbind_mock_port(52);
        (result, host)
    }

    #[cfg(unix)]
    #[test]
    fn a_resumed_call_the_host_asks_about_again_parks_on_the_new_approval() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            use vam_spec::v1::tool_intent_response::Disposition;
            let (result, host) = resumed_reintent(|h| {
                h.intent_disposition = Disposition::Suspended as i32;
            })
            .await;
            let outcome = result.expect_err("the task parks, it does not continue");
            assert_eq!(outcome.state, TaskOutcomeState::Suspended as i32);
            assert!(outcome.error.is_none());
            assert!(outcome.continuation.is_some());
            let guard = host.lock().expect("host");
            assert!(!guard.methods_seen.iter().any(|m| m == "ExecuteHostTool"));
            let stored = guard.stored.clone().expect("the tail is uploaded");
            let tail: vam_spec::thread_context::SuspendTail =
                serde_json::from_slice(&stored).expect("a suspend tail");
            assert_eq!(tail.kind, "suspend");
            assert_eq!(tail.pending.tool_call_id, "call_pending");
            assert_eq!(tail.pending.tool_name, "echo.ping");
            assert!(
                tail.messages.is_empty(),
                "the host already holds the results"
            );
            assert_eq!(tail.state.total_tokens, 7);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_resumed_call_the_host_denies_gives_the_model_the_denial_and_the_task_goes_on() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (result, host) = resumed_reintent(|h| {
                h.denied_tools
                    .insert("echo.ping".to_string(), "not allowed here".to_string());
            })
            .await;
            let observation = result.expect("a denial is a tool result, not a failure");
            assert_eq!(
                observation,
                "Error: Tool 'echo.ping' was denied by security policy: not allowed here"
            );
            let guard = host.lock().expect("host");
            assert!(!guard.methods_seen.iter().any(|m| m == "ExecuteHostTool"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_resumed_call_the_journal_already_holds_is_replayed_and_never_sent() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (result, host) = resumed_reintent(|h| {
                h.replayed_tools
                    .insert("echo.ping".to_string(), "pong (recorded)".to_string());
            })
            .await;
            let observation = result.expect("a replay is the tool result, not a failure");
            assert!(observation.contains("pong (recorded)"), "{observation}");
            let guard = host.lock().expect("host");
            assert!(!guard.methods_seen.iter().any(|m| m == "ExecuteHostTool"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn resume_fails_before_the_approved_tool_when_the_checkpoint_does_not_verify() {
        // The broker lock is taken by this sync test, never held across an await.
        let _broker = mock_broker_lock();
        run_socket_test(resume_fails_before_the_approved_tool_body());
    }

    #[cfg(unix)]
    async fn resume_fails_before_the_approved_tool_body() {
        use sha2::Digest;
        let host = crate::pep::test_support::shared_host(Some(b"{\"tampered\":true}".to_vec()));
        let claimed = format!(
            "{:x}",
            sha2::Sha256::digest(b"the checkpoint the resume names")
        );
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let holding_53 = spawn_silent_broker(bind_mock_port(53));

        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "hitl_resume".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        string_input(&mut fields, "approved_tool", "echo.ping");
        fields.insert(
            "approved_tool_args".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StructValue(Struct::default())),
            },
        );
        string_input(&mut fields, "hitl_continuation_digest", &claimed);
        let inputs = Struct { fields };
        let prompt = PromptAction {
            instruction_template: "continue".to_string(),
            inject_context_keys: vec![],
        };

        let outcome =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, Some(&inputs), None)
                .await;

        serving_52.abort();
        holding_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);

        let outcome = outcome.expect("task outcome");
        assert_eq!(outcome.state, TaskOutcomeState::Failed as i32);
        let message = outcome.error.expect("error").message;
        assert!(
            message.contains(&claimed),
            "the error should name the checkpoint: {message}"
        );
        let seen = host.lock().expect("host").methods_seen.clone();
        assert!(
            !seen.iter().any(|m| m == "RequestToolAuthorization"),
            "the approved tool must not be authorized: {seen:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_suspend_uploads_only_the_tail_and_the_resume_fetches_the_context() {
        // The broker lock is taken by this sync test, never held across an await.
        let _broker = mock_broker_lock();
        run_socket_test(one_mib_suspend_resume_body());
    }

    #[cfg(unix)]
    async fn one_mib_suspend_resume_body() {
        use vam_spec::v1::tool_intent_response::Disposition;

        let host = crate::pep::test_support::shared_host(None);
        host.lock().expect("host").intent_disposition = Disposition::Suspended as i32;
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let serving_53 = spawn_inference_broker(bind_mock_port(53));

        // A user turn of about 1.1 MiB that asks the mock model for the echo tool.
        let prompt = PromptAction {
            instruction_template: format!("run-echo now. {}", "context ".repeat(140_000)),
            inject_context_keys: vec![],
        };
        let suspended =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, None, None).await;
        let suspended = suspended.expect("suspend outcome");
        assert_eq!(
            suspended.state,
            TaskOutcomeState::Suspended as i32,
            "{:?}",
            suspended.error
        );

        // The 1.1 MiB user turn is the host's to know (it saw the request); the
        // guest sends only the tail and the loop state.
        let stored = host
            .lock()
            .expect("host")
            .stored
            .clone()
            .expect("the host stores the upload");
        assert!(
            stored.len() < 4096,
            "the tail is small: {} bytes",
            stored.len()
        );
        let tail: vam_spec::thread_context::SuspendTail =
            serde_json::from_slice(&stored).expect("a suspend tail");
        assert_eq!(tail.schema_version, 3);
        assert_eq!(tail.kind, "suspend");
        assert!(
            tail.messages.is_empty(),
            "no sibling ran before the pending call"
        );
        assert_eq!(tail.pending.tool_name, "echo");
        assert_eq!(tail.state.iterations_used, 1);
        assert!(
            !crate::primitives::workspace_root()
                .join(".vam/agent/hitl-continuation.json")
                .exists(),
            "the guest no longer writes the conversation into the workspace"
        );
        let artifact_digest = suspended
            .continuation
            .as_ref()
            .and_then(|c| c.digest.as_ref())
            .map(|d| d.hex_hash.clone())
            .unwrap_or_default();
        assert_eq!(
            artifact_digest,
            vam_spec::thread_context::sha256_hex(&stored)
        );

        // Resume: the host serves the assembled context of the suspended run.
        let (ctx_bytes, ctx_digest) =
            suspended_context("urn:vam:run:local.vam.dev/acme/run-socket");
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            guard.stored = Some(ctx_bytes);
        }
        let mut args = std::collections::BTreeMap::new();
        string_input(&mut args, "query", "hello");
        let mut fields = std::collections::BTreeMap::new();
        fields.insert(
            "hitl_resume".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        string_input(&mut fields, "approved_tool", "echo");
        fields.insert(
            "approved_tool_args".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StructValue(Struct {
                    fields: args,
                })),
            },
        );
        string_input(&mut fields, "thread_head_digest", &ctx_digest);
        let inputs = Struct { fields };
        let resumed = run_react_agent_loop_with_trace(
            &checkpoint_test_lease(),
            &PromptAction {
                instruction_template: "run-echo now.".to_string(),
                inject_context_keys: vec![],
            },
            Some(&inputs),
            None,
        )
        .await;

        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);

        let resumed = resumed.expect("resume outcome");
        assert_eq!(
            resumed.state,
            TaskOutcomeState::Succeeded as i32,
            "{:?}",
            resumed.error
        );
        let seen = host.lock().expect("host").methods_seen.clone();
        assert!(
            seen.iter().any(|m| m == "FetchRunCheckpoint"),
            "the resume fetched the context by digest: {seen:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_denied_resume_keeps_the_conversation_and_answers_the_real_call() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let run = "urn:vam:run:local.vam.dev/acme/run-socket";
            let (bytes, digest) = suspended_context(run);
            let host = crate::pep::test_support::shared_host(Some(bytes));
            let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
            let script: MockScript = serde_json::from_str(
                r#"{"id": "denied", "rules": [
                    {"when": {"anyContains": "run-echo now."}, "reply": {"text": "kept history"}},
                    {"when": {}, "reply": {"text": "lost history"}}]}"#,
            )
            .expect("script");
            let scripts = MockScripts::from_scripts(vec![script]).expect("scripts");
            let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));
            let mut fields = std::collections::BTreeMap::new();
            string_input(&mut fields, "system_instructions", "mock-script:denied");
            fields.insert(
                "hitl_denied".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::BoolValue(true)),
                },
            );
            string_input(&mut fields, "denied_tool", "echo");
            fields.insert(
                "denied_tool_args".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StructValue(Struct::default())),
                },
            );
            string_input(&mut fields, "denial_reason", "no thanks");
            string_input(&mut fields, "thread_head_digest", &digest);
            let outcome = run_react_agent_loop_with_trace(
                &checkpoint_test_lease(),
                &PromptAction {
                    instruction_template: "unused".to_string(),
                    inject_context_keys: vec![],
                },
                Some(&Struct { fields }),
                None,
            )
            .await
            .expect("outcome");
            serving_52.abort();
            serving_53.abort();
            unbind_mock_port(52);
            unbind_mock_port(53);
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            assert_eq!(output_answer(&outcome), "kept history");
        });
    }

    /// The host-assembled context of a run suspended on `echo` call `c1`, with
    /// a sibling call `c2` that was never run.
    #[cfg(unix)]
    fn suspended_context(run_id: &str) -> (Vec<u8>, String) {
        use vam_spec::thread_context::{
            sha256_hex, LoopState, PendingCall, SuspendResume, ThreadContext, WireMessage,
            WireToolCall, THREAD_CONTEXT_FORMAT, THREAD_CONTEXT_VERSION,
        };
        let msg = |role: &str, content: &str| WireMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        };
        let mut calls = msg("assistant", "");
        calls.tool_calls = ["c1", "c2"]
            .iter()
            .map(|id| WireToolCall {
                id: (*id).into(),
                name: "echo".into(),
                arguments: "{}".into(),
            })
            .collect();
        let ctx = ThreadContext {
            format: THREAD_CONTEXT_FORMAT.into(),
            version: THREAD_CONTEXT_VERSION,
            run_id: run_id.into(),
            head_seq: 4,
            head_digest: "d".into(),
            system_sha256: None,
            tools_sha256: None,
            messages: vec![msg("user", "run-echo now."), calls],
            blocks: vec![],
            suspend: Some(SuspendResume {
                lifecycle_generation: 1,
                pending: PendingCall {
                    tool_call_id: "c1".into(),
                    tool_name: "echo".into(),
                    args_json: "{}".into(),
                },
                unanswered: vec!["c2".into()],
                state: LoopState {
                    iterations_used: 1,
                    max_iterations: 8,
                    claim_reprompted: true,
                    ..Default::default()
                },
            }),
            crash: None,
        };
        let bytes = serde_json::to_vec(&ctx).expect("encode");
        let digest = sha256_hex(&bytes);
        (bytes, digest)
    }

    /// A script that reads a missing workspace file on the first user turn, then answers from the result.
    #[cfg(unix)]
    const TOOL_AND_ANSWER_SCRIPT: &str = r#"{"id": "tool-and-answer", "rules": [
        {"when": {"step": "start", "stepStart": true},
         "reply": {"toolCall": {"name": "read_file", "args": {"path": "notes/absent.txt"}}}},
        {"when": {"lastRole": "tool", "toolName": "read_file"},
         "reply": {"text": "The read finished."}}]}"#;

    #[cfg(unix)]
    #[test]
    fn scripted_model_calls_a_tool_then_answers_from_its_result() {
        // The broker lock is taken by this sync test, never held across an await.
        let _broker = mock_broker_lock();
        run_socket_test(scripted_tool_call_body());
    }

    #[cfg(unix)]
    async fn scripted_tool_call_body() {
        use vam_spec::v1::tool_intent_response::Disposition;

        let host = crate::pep::test_support::shared_host(None);
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            // The model is offered only the tools the host advertises.
            guard.advertised_tools = vec!["read_file".to_string()];
        }
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript =
            serde_json::from_str(TOOL_AND_ANSWER_SCRIPT).expect("the script parses");
        let scripts = MockScripts::from_scripts(vec![script]).expect("the script loads");
        let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));

        // The system prompt carries the marker that selects the script (D5).
        let mut fields = std::collections::BTreeMap::new();
        string_input(
            &mut fields,
            "system_instructions",
            "Answer with the tools. mock-script:tool-and-answer",
        );
        let inputs = Struct { fields };
        let prompt = PromptAction {
            instruction_template: "use the echo tool, then answer e2e-step:start".to_string(),
            inject_context_keys: vec![],
        };
        let outcome =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, Some(&inputs), None)
                .await;

        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);

        let outcome = outcome.expect("the run finishes");
        assert_eq!(
            outcome.state,
            TaskOutcomeState::Succeeded as i32,
            "{:?}",
            outcome.error
        );
        let answer = outcome
            .output
            .as_ref()
            .and_then(|output| output.fields.get("answer"))
            .and_then(|value| value.kind.as_ref());
        assert!(
            matches!(
                answer,
                Some(prost_types::value::Kind::StringValue(text))
                    if text == "The read finished."
            ),
            "the answer comes from the scripted reply after the tool result: {answer:?}"
        );
        let seen = host.lock().expect("host").methods_seen.clone();
        assert!(
            seen.iter().any(|m| m == "RequestToolAuthorization"),
            "the scripted tool call went through the host PEP: {seen:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_refused_suspend_tail_fails_the_approval_and_uploads_no_second_copy() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            use vam_spec::v1::tool_intent_response::Disposition;

            let host = crate::pep::test_support::shared_host(None);
            {
                let mut guard = host.lock().expect("host");
                guard.intent_disposition = Disposition::Suspended as i32;
                guard.refuse_upload_at = Some(0);
            }
            let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
            let serving_53 = spawn_inference_broker(bind_mock_port(53));
            let prompt = PromptAction {
                instruction_template: "run-echo now.".to_string(),
                inject_context_keys: vec![],
            };
            let outcome =
                run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, None, None)
                    .await;
            serving_52.abort();
            serving_53.abort();
            unbind_mock_port(52);
            unbind_mock_port(53);

            let outcome = outcome.expect("an outcome");
            assert_eq!(outcome.state, TaskOutcomeState::Failed as i32);
            let message = outcome.error.expect("an error").message;
            assert!(message.starts_with(STORAGE_COMMIT_FAILED), "{message}");
            let guard = host.lock().expect("host");
            let uploads = guard
                .methods_seen
                .iter()
                .filter(|m| *m == "UploadRunCheckpoint")
                .count();
            assert_eq!(
                uploads, 1,
                "the tail was tried once; no full continuation followed"
            );
            assert!(guard.stored.is_none());
        });
    }

    /// A script whose tool may write files (the edit fails on a missing file, so nothing is written).
    #[cfg(unix)]
    const WRITE_AND_ANSWER_SCRIPT: &str = r#"{"id": "write-and-answer", "rules": [
        {"when": {"step": "start", "stepStart": true},
         "reply": {"toolCall": {"name": "edit_file", "args": {"path": "notes/absent.txt", "old_string": "a", "new_string": "b"}}}},
        {"when": {"lastRole": "tool", "toolName": "edit_file"},
         "reply": {"text": "Saved."}}]}"#;

    /// The context a guest gets after a crash: the conversation the host recorded, which of its
    /// calls are still open, and the host's loop state.
    #[cfg(unix)]
    fn crash_context_bytes(
        messages: Vec<vam_spec::thread_context::WireMessage>,
        unanswered: Vec<&str>,
        iterations_used: usize,
    ) -> (Vec<u8>, String) {
        use vam_spec::thread_context::{
            sha256_hex, CrashResume, LoopState, THREAD_CONTEXT_FORMAT, THREAD_CONTEXT_VERSION,
        };
        let ctx = vam_spec::thread_context::ThreadContext {
            format: THREAD_CONTEXT_FORMAT.into(),
            version: THREAD_CONTEXT_VERSION,
            run_id: "urn:vam:run:local.vam.dev/acme/run-socket".into(),
            head_seq: 3,
            head_digest: "d".into(),
            system_sha256: None,
            tools_sha256: None,
            messages,
            blocks: vec![],
            suspend: None,
            crash: Some(CrashResume {
                reason: "guest_crashed".into(),
                resume: 1,
                unanswered: unanswered.into_iter().map(String::from).collect(),
                state: LoopState {
                    iterations_used,
                    ..Default::default()
                },
            }),
        };
        let bytes = serde_json::to_vec(&ctx).expect("encode");
        let digest = sha256_hex(&bytes);
        (bytes, digest)
    }

    #[cfg(unix)]
    fn wire(
        role: &str,
        content: &str,
        calls: &[(&str, &str, &str)],
        id: &str,
    ) -> vam_spec::thread_context::WireMessage {
        vam_spec::thread_context::WireMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: calls
                .iter()
                .map(
                    |(id, name, arguments)| vam_spec::thread_context::WireToolCall {
                        id: (*id).into(),
                        name: (*name).into(),
                        arguments: (*arguments).into(),
                    },
                )
                .collect(),
            tool_call_id: id.into(),
        }
    }

    /// Resumes a task from `bytes` against a mock host (`prepare` sets it up) and a model that
    /// answers only the one request the script allows; returns the outcome and the host.
    #[cfg(unix)]
    async fn run_crash_resume(
        bytes: Vec<u8>,
        digest: String,
        prepare: impl FnOnce(&mut crate::pep::test_support::MockCheckpointHost),
    ) -> (TaskOutcome, crate::pep::test_support::SharedMockHost) {
        use vam_spec::v1::tool_intent_response::Disposition;
        let host = crate::pep::test_support::shared_host(Some(bytes));
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            guard.advertised_tools = vec!["edit_file".to_string(), "run".to_string()];
            guard.commit_turn_mode = 1;
            prepare(&mut guard);
        }
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript =
            serde_json::from_str(CRASH_RESUME_SCRIPT).expect("the script parses");
        let scripts = MockScripts::from_scripts(vec![script]).expect("the script loads");
        let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));
        let mut fields = std::collections::BTreeMap::new();
        string_input(
            &mut fields,
            "system_instructions",
            "Resume. mock-script:crash-resume",
        );
        string_input(
            &mut fields,
            vam_spec::thread_context::THREAD_HEAD_DIGEST_INPUT,
            &digest,
        );
        let inputs = Struct { fields };
        let prompt = PromptAction {
            instruction_template: "go e2e-step:start".to_string(),
            inject_context_keys: vec![],
        };
        let outcome =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, Some(&inputs), None)
                .await;
        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);
        (outcome.expect("the run finishes"), host)
    }

    /// The model answers only a request that carries the result of `run` (the round's last
    /// call). A request made before the round's calls ran has no rule and fails the task.
    #[cfg(unix)]
    const CRASH_RESUME_SCRIPT: &str = r#"{"id": "crash-resume", "rules": [
        {"when": {"lastRole": "tool", "toolName": "run"},
         "reply": {"text": "Resumed."}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_resumed_round_reruns_the_in_vm_call_replays_the_host_call_and_asks_the_model_once() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (bytes, digest) = crash_context_bytes(
                vec![
                    wire("user", "go e2e-step:start", &[], ""),
                    wire(
                        "assistant",
                        "",
                        &[
                            (
                                "c1",
                                "edit_file",
                                r#"{"path":"notes/absent.txt","old_string":"a","new_string":"b"}"#,
                            ),
                            ("c2", "run", r#"{"command":"echo hi"}"#),
                        ],
                        "",
                    ),
                ],
                vec!["c1", "c2"],
                3,
            );
            let (outcome, host) = run_crash_resume(bytes, digest, |h| {
                h.replayed_tools
                    .insert("run".to_string(), "T-1 recorded".to_string());
            })
            .await;
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            let host = host.lock().expect("host");
            assert_eq!(
                host.intents_seen,
                vec!["edit_file".to_string(), "run".to_string()],
                "the round's calls go through the intent path, in order"
            );
            assert!(
                !host.methods_seen.iter().any(|m| m == "ExecuteHostTool"),
                "a replayed call is never executed: {:?}",
                host.methods_seen
            );
            // The round is the one after the host's count (3 used), so its commit is round 4.
            assert_eq!(host.commit_turns_seen.first().map(|c| c.0), Some(4));
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_resume_with_no_decided_round_starts_with_the_next_model_request() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (bytes, digest) = crash_context_bytes(
                vec![
                    wire("user", "go e2e-step:start", &[], ""),
                    wire(
                        "assistant",
                        "",
                        &[("c1", "run", r#"{"command":"echo hi"}"#)],
                        "",
                    ),
                    wire("tool", "done", &[], "c1"),
                ],
                vec![],
                1,
            );
            let (outcome, host) = run_crash_resume(bytes, digest, |_| {}).await;
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            assert!(
                host.lock().expect("host").intents_seen.is_empty(),
                "nothing is run before the model is asked"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn results_the_log_already_holds_are_put_back_not_run_again() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            // c1 has a logged result; only c2 is open.
            let (bytes, digest) = crash_context_bytes(
                vec![
                    wire("user", "go e2e-step:start", &[], ""),
                    wire(
                        "assistant",
                        "",
                        &[
                            (
                                "c1",
                                "edit_file",
                                r#"{"path":"a","old_string":"a","new_string":"b"}"#,
                            ),
                            ("c2", "run", r#"{"command":"echo hi"}"#),
                        ],
                        "",
                    ),
                    wire("tool", "logged result of c1", &[], "c1"),
                ],
                vec!["c2"],
                2,
            );
            let (outcome, host) = run_crash_resume(bytes, digest, |h| {
                h.replayed_tools
                    .insert("run".to_string(), "T-2".to_string());
            })
            .await;
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            assert_eq!(
                host.lock().expect("host").intents_seen,
                vec!["run".to_string()]
            );
        });
    }

    /// Runs the write script against a mock host that answers `CommitTurn` the
    /// way `commit_turn_mode` says; returns the outcome and the commits it saw.
    #[cfg(unix)]
    async fn run_write_script(commit_turn_mode: u8) -> (TaskOutcome, Vec<(u32, String)>) {
        use vam_spec::v1::tool_intent_response::Disposition;

        let host = crate::pep::test_support::shared_host(None);
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            guard.advertised_tools = vec!["edit_file".to_string()];
            guard.commit_turn_mode = commit_turn_mode;
        }
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript =
            serde_json::from_str(WRITE_AND_ANSWER_SCRIPT).expect("the script parses");
        let scripts = MockScripts::from_scripts(vec![script]).expect("the script loads");
        let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));
        let mut fields = std::collections::BTreeMap::new();
        string_input(
            &mut fields,
            "system_instructions",
            "Write then answer. mock-script:write-and-answer",
        );
        let inputs = Struct { fields };
        let prompt = PromptAction {
            instruction_template: "save the note e2e-step:start".to_string(),
            inject_context_keys: vec![],
        };
        let outcome =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, Some(&inputs), None)
                .await;
        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);
        let seen = host.lock().expect("host").commit_turns_seen.clone();
        (outcome.expect("the run finishes"), seen)
    }

    #[cfg(unix)]
    #[test]
    fn a_round_that_may_write_files_asks_for_a_turn_commit_with_the_digest_of_its_results() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (outcome, seen) = run_write_script(1).await;
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            assert_eq!(seen.len(), 1, "one write round, one commit: {seen:?}");
            assert_eq!(seen[0].0, 1, "rounds are 1-based");
            assert_eq!(seen[0].1.len(), 64, "a SHA-256 digest: {}", seen[0].1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_host_without_turn_commits_is_asked_once_and_the_task_goes_on() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (outcome, seen) = run_write_script(2).await;
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            assert_eq!(seen.len(), 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_refusal_that_is_not_a_storage_failure_does_not_stop_the_task() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (outcome, seen) = run_write_script(0).await;
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            assert_eq!(seen.len(), 1);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_storage_failure_while_committing_a_turn_fails_the_task_with_the_storage_reason() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (outcome, _) = run_write_script(3).await;
            assert_eq!(outcome.state, TaskOutcomeState::Failed as i32);
            let message = outcome.error.expect("an error").message;
            assert!(message.starts_with(STORAGE_COMMIT_FAILED), "{message}");
        });
    }

    /// Records the freezer calls, and how many commits the host had seen at the
    /// thaw (a commit asked for while frozen has been seen by then).
    #[cfg(unix)]
    struct OrderedFreezer {
        events: std::sync::Mutex<Vec<String>>,
        host: crate::pep::test_support::SharedMockHost,
        fail_freeze: bool,
    }

    #[cfg(unix)]
    impl crate::freeze::FsFreezer for OrderedFreezer {
        fn applies(&self) -> bool {
            true
        }
        fn sync(&self) -> std::io::Result<()> {
            self.events.lock().unwrap().push("sync".into());
            Ok(())
        }
        fn freeze(&self) -> std::io::Result<()> {
            self.events.lock().unwrap().push("freeze".into());
            if self.fail_freeze {
                return Err(std::io::Error::from_raw_os_error(16));
            }
            Ok(())
        }
        fn thaw(&self) -> std::io::Result<()> {
            let seen = self.host.lock().unwrap().commit_turns_seen.len();
            self.events
                .lock()
                .unwrap()
                .push(format!("thaw after {seen} commit(s) reached the host"));
            Ok(())
        }
    }

    #[cfg(unix)]
    fn tool_round_messages() -> Vec<ChatMessage> {
        vec![
            ChatMessage {
                role: "assistant".into(),
                content: String::new(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            },
            ChatMessage {
                role: "tool".into(),
                content: "ok".into(),
                tool_calls: vec![],
                tool_call_id: "call-1".into(),
            },
        ]
    }

    #[cfg(unix)]
    #[test]
    fn the_turn_commit_request_is_made_between_the_freeze_and_the_thaw() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let host = crate::pep::test_support::shared_host(None);
            host.lock().expect("host").commit_turn_mode = 1;
            let serving = spawn_checkpoint_host(bind_mock_port(52), host.clone());
            let freezer = Arc::new(OrderedFreezer {
                events: Default::default(),
                host: host.clone(),
                fail_freeze: false,
            });
            let dynamic: Arc<dyn crate::freeze::FsFreezer> = freezer.clone();
            let mut enabled = true;
            request_turn_commit(
                &dynamic,
                &checkpoint_test_lease(),
                &tool_round_messages(),
                1,
                &mut enabled,
            )
            .await
            .expect("accepted");
            serving.abort();
            unbind_mock_port(52);
            assert_eq!(
                *freezer.events.lock().unwrap(),
                vec![
                    "sync".to_string(),
                    "freeze".to_string(),
                    "thaw after 1 commit(s) reached the host".to_string()
                ]
            );
            assert!(enabled);
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_freeze_skips_the_commit_and_the_task_goes_on() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let host = crate::pep::test_support::shared_host(None);
            host.lock().expect("host").commit_turn_mode = 1;
            let serving = spawn_checkpoint_host(bind_mock_port(52), host.clone());
            let freezer = Arc::new(OrderedFreezer {
                events: Default::default(),
                host: host.clone(),
                fail_freeze: true,
            });
            let dynamic: Arc<dyn crate::freeze::FsFreezer> = freezer.clone();
            let mut enabled = true;
            request_turn_commit(
                &dynamic,
                &checkpoint_test_lease(),
                &tool_round_messages(),
                1,
                &mut enabled,
            )
            .await
            .expect("not an error");
            serving.abort();
            unbind_mock_port(52);
            assert!(host.lock().unwrap().commit_turns_seen.is_empty());
            assert_eq!(
                *freezer.events.lock().unwrap(),
                vec!["sync".to_string(), "freeze".to_string()]
            );
            // Skipping a round does not switch commits off for the next one.
            assert!(enabled);
        });
    }

    /// A follow-up script: answers from the earlier tool result when the history has it.
    #[cfg(unix)]
    const FOLLOW_UP_SCRIPT: &str = r#"{"id": "follow-up", "rules": [
        {"when": {"step": "next", "anyContains": "kiwi-42"},
         "reply": {"text": "The note says kiwi-42."}},
        {"when": {"step": "next"}, "reply": {"text": "I do not know."}}]}"#;

    #[cfg(unix)]
    fn earlier_task_context(run_id: &str) -> (Vec<u8>, String) {
        use vam_spec::thread_context::{
            sha256_hex, ThreadContext, WireMessage, WireToolCall, THREAD_CONTEXT_FORMAT,
            THREAD_CONTEXT_VERSION,
        };
        let msg = |role: &str, content: &str| WireMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        };
        let mut call = msg("assistant", "");
        call.tool_calls = vec![WireToolCall {
            id: "c1".into(),
            name: "write_file".into(),
            arguments: "{}".into(),
        }];
        let mut result = msg("tool", "kiwi-42");
        result.tool_call_id = "c1".into();
        let ctx = ThreadContext {
            format: THREAD_CONTEXT_FORMAT.into(),
            version: THREAD_CONTEXT_VERSION,
            run_id: run_id.into(),
            head_seq: 5,
            head_digest: "d".into(),
            system_sha256: Some("0".repeat(64)),
            tools_sha256: None,
            messages: vec![
                msg("user", "e2e-step:first Save the note"),
                call,
                result,
                msg("assistant", "Saved."),
            ],
            blocks: vec![],
            suspend: None,
            crash: None,
        };
        let bytes = serde_json::to_vec(&ctx).expect("encode");
        let digest = sha256_hex(&bytes);
        (bytes, digest)
    }

    #[cfg(unix)]
    async fn run_follow_up(stored: Option<Vec<u8>>, digest: Option<&str>) -> TaskOutcome {
        let host = crate::pep::test_support::shared_host(stored);
        host.lock().expect("host").advertised_tools = vec!["read_file".to_string()];
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript = serde_json::from_str(FOLLOW_UP_SCRIPT).expect("script");
        let scripts = MockScripts::from_scripts(vec![script]).expect("scripts");
        let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));
        let mut fields = std::collections::BTreeMap::new();
        string_input(&mut fields, "system_instructions", "mock-script:follow-up");
        if let Some(digest) = digest {
            string_input(&mut fields, "thread_head_digest", digest);
        }
        let prompt = PromptAction {
            instruction_template: "e2e-step:next What does the note say?".to_string(),
            inject_context_keys: vec![],
        };
        let outcome = run_react_agent_loop_with_trace(
            &checkpoint_test_lease(),
            &prompt,
            Some(&Struct { fields }),
            None,
        )
        .await
        .expect("task outcome");
        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);
        outcome
    }

    #[cfg(unix)]
    #[test]
    fn a_follow_up_starts_from_the_fetched_thread_context() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let run = "urn:vam:run:local.vam.dev/acme/run-socket";
            let (bytes, digest) = earlier_task_context(run);
            let with = run_follow_up(Some(bytes), Some(&digest)).await;
            assert_eq!(
                with.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                with.error
            );
            assert_eq!(output_answer(&with), "The note says kiwi-42.");
            let without = run_follow_up(None, None).await;
            assert_eq!(output_answer(&without), "I do not know.");
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_thread_context_for_another_run_or_a_bad_digest_fails_the_task() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (other, digest) = earlier_task_context("urn:vam:run:local.vam.dev/acme/other");
            let foreign = run_follow_up(Some(other), Some(&digest)).await;
            assert_eq!(foreign.state, TaskOutcomeState::Failed as i32);
            assert!(foreign
                .error
                .expect("error")
                .message
                .contains("another run"));
            let (bytes, _) = earlier_task_context("urn:vam:run:local.vam.dev/acme/run-socket");
            let bad = run_follow_up(Some(bytes), Some(&"1".repeat(64))).await;
            assert_eq!(bad.state, TaskOutcomeState::Failed as i32);
        });
    }

    /// Runs the loop against a scripted model with the given termination input.
    #[cfg(unix)]
    async fn run_claim_script(script: &str, id: &str, termination: Option<&str>) -> TaskOutcome {
        use vam_spec::v1::tool_intent_response::Disposition;

        let host = crate::pep::test_support::shared_host(None);
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            guard.advertised_tools = vec!["read_file".to_string()];
        }
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript = serde_json::from_str(script).expect("the script parses");
        let scripts = MockScripts::from_scripts(vec![script]).expect("the script loads");
        let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));

        let mut fields = std::collections::BTreeMap::new();
        string_input(
            &mut fields,
            "system_instructions",
            &format!("Work on the task. mock-script:{id}"),
        );
        if let Some(termination) = termination {
            string_input(&mut fields, "termination", termination);
        }
        let inputs = Struct { fields };
        let prompt = PromptAction {
            instruction_template: "do the task e2e-step:start".to_string(),
            inject_context_keys: vec![],
        };
        let outcome =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, Some(&inputs), None)
                .await;
        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);
        outcome.expect("the run finishes")
    }

    fn output_answer(outcome: &TaskOutcome) -> String {
        match outcome
            .output
            .as_ref()
            .and_then(|o| o.fields.get("answer"))
            .and_then(|v| v.kind.as_ref())
        {
            Some(prost_types::value::Kind::StringValue(text)) => text.clone(),
            other => panic!("no answer: {other:?}"),
        }
    }

    const RESAMPLE_SCRIPT: &str = r#"{"id":"resample","rules":[
        {"when":{"step":"go","anyContains":"invalid arguments for read_file"},"reply":{"text":"gave up"}},
        {"when":{"step":"go","resampled":true},"reply":{"text":"second"}},
        {"when":{"step":"go"},"reply":{"toolCall":{"name":"read_file","rawArgs":"{\"path\": \"a"}}}]}"#;

    /// The model never fixes its call: the resample is as bad as the first reply.
    const RESAMPLE_HOPELESS: &str = r#"{"id":"resample","rules":[
        {"when":{"step":"go","anyContains":"invalid arguments for read_file"},"reply":{"text":"gave up"}},
        {"when":{"step":"go"},"reply":{"toolCall":{"name":"read_file","rawArgs":"{\"path\": \"a"}}}]}"#;

    /// Runs the loop against `script` with `adapter.resampleOnInvalid` = `switch`
    /// (0 leaves the switch out); returns the outcome and the rule index of every request.
    #[cfg(unix)]
    async fn run_resample_case(switch: u32, script: &str) -> (TaskOutcome, Vec<usize>) {
        use vam_spec::v1::tool_intent_response::Disposition;

        let host = crate::pep::test_support::shared_host(None);
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            guard.advertised_tools = vec!["read_file".to_string()];
        }
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript = serde_json::from_str(script).expect("the script parses");
        let scripts = MockScripts::from_scripts(vec![script]).expect("the script loads");
        let seen: Arc<std::sync::Mutex<Vec<usize>>> = Arc::default();
        let trace = {
            let seen = seen.clone();
            Arc::new(move |t: &vam_inference::MockTrace| {
                if let vam_inference::MockTrace::RuleMatched { rule, .. } = t {
                    seen.lock().unwrap().push(*rule);
                }
            })
        };
        let broker = Arc::new(
            InferenceBrokerServer::new(
                Arc::new(ModelCatalog::new()),
                Arc::new(SessionBudget::new(None)),
                Arc::new(SimpleSecretLookup::new()),
            )
            .with_mock_scripts(Arc::new(scripts))
            .with_mock_trace_callback(trace),
        );
        let listener = bind_mock_port(53);
        let serving_53 = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let broker = broker.clone();
                tokio::spawn(async move { broker.handle_connection(stream).await });
            }
        });

        let mut fields = std::collections::BTreeMap::new();
        string_input(
            &mut fields,
            "system_instructions",
            "Work on the task. mock-script:resample",
        );
        if switch > 0 {
            fields.insert(
                "harness".to_string(),
                mapi_val(
                    serde_json::json!({"switches": {"adapter": {"resampleOnInvalid": switch}}}),
                )
                .expect("harness input"),
            );
        }
        let prompt = PromptAction {
            instruction_template: "do the task e2e-step:go".to_string(),
            inject_context_keys: vec![],
        };
        let outcome = run_react_agent_loop_with_trace(
            &checkpoint_test_lease(),
            &prompt,
            Some(&Struct { fields }),
            None,
        )
        .await;
        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);
        let rules = seen.lock().unwrap().clone();
        (outcome.expect("the run finishes"), rules)
    }

    #[cfg(unix)]
    #[test]
    fn an_invalid_call_is_resampled_once_when_the_switch_is_on() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            // Off: the invalid call gets the typed error at once. Two requests, no resample.
            let (off, rules) = run_resample_case(0, RESAMPLE_SCRIPT).await;
            assert_eq!(output_answer(&off), "gave up");
            assert_eq!(rules, vec![2, 0]);

            // On, and the mock gives a valid reply to the resample: it is used as it is and
            // the discarded call is never shown. Two requests; the second is the resample.
            // (The scripted resample is text; a hot reply is not a typed error.)
            let (on, rules) = run_resample_case(1, RESAMPLE_SCRIPT).await;
            assert_eq!(output_answer(&on), "second");
            assert_eq!(rules, vec![2, 1]);

            // On, and the resample is invalid too: exactly one resample, then the typed
            // error, then the model's next request. Three requests, never more.
            let (twice, rules) = run_resample_case(1, RESAMPLE_HOPELESS).await;
            assert_eq!(output_answer(&twice), "gave up");
            assert_eq!(rules, vec![1, 1, 0]);

            // Off, same script: the first bad call is the typed error. Two requests.
            let (off, rules) = run_resample_case(0, RESAMPLE_HOPELESS).await;
            assert_eq!(output_answer(&off), "gave up");
            assert_eq!(rules, vec![1, 0]);
        });
    }

    fn output_claimed(outcome: &TaskOutcome) -> Option<bool> {
        match outcome
            .output
            .as_ref()
            .and_then(|o| o.fields.get("claimed"))
            .and_then(|v| v.kind.as_ref())
        {
            Some(prost_types::value::Kind::BoolValue(flag)) => Some(*flag),
            _ => None,
        }
    }

    const CLAIM_SCRIPT: &str = r#"{"id": "claim-done", "rules": [
        {"when": {"step": "start", "stepStart": true},
         "reply": {"toolCall": {"name": "task_complete", "args": {"summary": "All done."}}}}]}"#;

    const TEXT_ONLY_SCRIPT: &str = r#"{"id": "text-only", "rules": [
        {"when": {"lastRole": "user", "contains": "not marked complete"},
         "reply": {"text": "Second text."}},
        {"when": {"lastRole": "user"}, "reply": {"text": "First text."}}]}"#;

    const CLAIM_AFTER_REPROMPT_SCRIPT: &str = r#"{"id": "claim-late", "rules": [
        {"when": {"lastRole": "user", "contains": "not marked complete"},
         "reply": {"toolCall": {"name": "task_complete", "args": {"summary": "Done after the nudge."}}}},
        {"when": {"lastRole": "user"}, "reply": {"text": "First text."}}]}"#;

    const OFFERED: &str = r#"[{"urn":"urn:vam:playbook:local.vam.dev/acme/pr","name":"Draft a PR","when":"Use when the user wants to draft a pull request.","default":true}]"#;

    fn activation_inputs(instruction_marker: &str, extra: &[(&str, &str)]) -> Struct {
        let mut fields = std::collections::BTreeMap::new();
        string_input(
            &mut fields,
            "system_instructions",
            &format!("Work on the task. mock-script:{instruction_marker}"),
        );
        string_input(&mut fields, "attached_playbooks", OFFERED);
        for (key, value) in extra {
            string_input(&mut fields, key, value);
        }
        Struct { fields }
    }

    /// Runs the loop against a scripted model and a mock host that advertises
    /// the playbook tools. `setup` configures the host before the run.
    #[cfg(unix)]
    async fn run_activation_script(
        script: &str,
        instruction: &str,
        inputs: Struct,
        setup: impl FnOnce(&mut crate::pep::test_support::MockCheckpointHost),
    ) -> (TaskOutcome, Vec<String>) {
        let (outcome, intents, _) =
            run_activation_script_full(script, instruction, inputs, setup).await;
        (outcome, intents)
    }

    /// [`run_activation_script`] that also returns the `(after_seq, ack_seq)` of every
    /// `PullInbox` the mock host received.
    #[cfg(unix)]
    async fn run_activation_script_full(
        script: &str,
        instruction: &str,
        inputs: Struct,
        setup: impl FnOnce(&mut crate::pep::test_support::MockCheckpointHost),
    ) -> (TaskOutcome, Vec<String>, Vec<(i64, i64)>) {
        use vam_spec::v1::tool_intent_response::Disposition;

        let host = crate::pep::test_support::shared_host(None);
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            guard.advertised_tools = [
                "read_file",
                "start_playbook",
                "step_complete",
                "run_step",
                "check_playbook_finish",
            ]
            .iter()
            .map(|name| name.to_string())
            .collect();
            setup(&mut guard);
        }
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript = serde_json::from_str(script).expect("the script parses");
        let scripts = MockScripts::from_scripts(vec![script]).expect("the script loads");
        let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));
        let prompt = PromptAction {
            instruction_template: instruction.to_string(),
            inject_context_keys: vec![],
        };
        let outcome =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, Some(&inputs), None)
                .await;
        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);
        let intents = host.lock().expect("host").intents_seen.clone();
        let pulls = host.lock().expect("host").inbox_pulls_seen.clone();
        (outcome.expect("the run finishes"), intents, pulls)
    }

    /// What the mock host saw during an outbox run.
    #[cfg(unix)]
    struct OutboxRun {
        outcome: TaskOutcome,
        methods: Vec<String>,
        intents: Vec<String>,
        posts: Vec<vam_spec::v1::PostOutboxRequest>,
    }

    /// Runs the loop against a scripted model and a mock host that advertises the
    /// outbox tools. `jig` turns the harness switch `human.jig` on.
    #[cfg(unix)]
    async fn run_outbox_script(
        script: &str,
        marker: &str,
        jig: bool,
        setup: impl FnOnce(&mut crate::pep::test_support::MockCheckpointHost),
    ) -> OutboxRun {
        use vam_spec::v1::tool_intent_response::Disposition;

        let host = crate::pep::test_support::shared_host(None);
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            guard.advertised_tools = ["read_file", "post_message", "show"]
                .iter()
                .map(|name| name.to_string())
                .collect();
            setup(&mut guard);
        }
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript = serde_json::from_str(script).expect("the script parses");
        let scripts = MockScripts::from_scripts(vec![script]).expect("the script loads");
        let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));
        let mut inputs = Struct::default();
        string_input(
            &mut inputs.fields,
            "system_instructions",
            &format!("Work on the task. mock-script:{marker}"),
        );
        if jig {
            let resolved = vam_spec::harness::Harness::resolve(&serde_json::json!({
                "human": {"jig": true}
            }))
            .expect("harness");
            let record = vam_spec::harness::Harness::record(&resolved).expect("record");
            inputs.fields.insert(
                vam_spec::harness::INPUT_KEY.to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StructValue(
                        vam_spec::harness::json_to_struct(&record).expect("struct"),
                    )),
                },
            );
        }
        let prompt = PromptAction {
            instruction_template: "Do the work".to_string(),
            inject_context_keys: vec![],
        };
        let outcome =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, Some(&inputs), None)
                .await;
        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);
        let guard = host.lock().expect("host");
        OutboxRun {
            outcome: outcome.expect("the run finishes"),
            methods: guard.methods_seen.clone(),
            intents: guard.intents_seen.clone(),
            posts: guard.posts_seen.clone(),
        }
    }

    const POST_SCRIPT: &str = r#"{"id": "post-ok", "rules": [
        {"when": {"lastRole": "tool", "contains": "posted"},
         "reply": {"text": "Done."}},
        {"when": {"lastRole": "user"},
         "reply": {"toolCall": {"name": "post_message", "args": {"text": "Starting: 1 of 2"}}}}]}"#;

    #[cfg(unix)]
    #[test]
    fn post_message_asks_the_policy_first_then_posts_what_was_authorized() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let run = run_outbox_script(POST_SCRIPT, "post-ok", false, |_| {}).await;
            assert_eq!(run.outcome.state, TaskOutcomeState::Succeeded as i32);
            assert_eq!(output_answer(&run.outcome), "Done.");
            assert!(
                run.intents.iter().any(|n| n == "post_message"),
                "{:?}",
                run.intents
            );
            assert_eq!(run.posts.len(), 1);
            assert_eq!(run.posts[0].text, "Starting: 1 of 2");
            assert!(run.posts[0].capability_token.is_some());
            let intent = run
                .methods
                .iter()
                .position(|m| m == "RequestToolAuthorization");
            let posted = run.methods.iter().position(|m| m == "PostOutbox");
            assert!(
                intent.is_some() && posted.is_some() && intent < posted,
                "{:?}",
                run.methods
            );
        });
    }

    const POST_EXTRA_SCRIPT: &str = r#"{"id": "post-extra", "rules": [
        {"when": {"lastRole": "tool", "contains": "invalid arguments for post_message"},
         "reply": {"text": "Fixed."}},
        {"when": {"lastRole": "user"},
         "reply": {"toolCall": {"name": "post_message", "args": {"text": "hi", "priority": "high"}}}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_call_with_an_unexpected_field_never_reaches_the_host() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let run = run_outbox_script(POST_EXTRA_SCRIPT, "post-extra", false, |_| {}).await;
            assert_eq!(output_answer(&run.outcome), "Fixed.");
            assert!(
                !run.intents.iter().any(|n| n == "post_message"),
                "{:?}",
                run.intents
            );
            assert!(run.posts.is_empty());
        });
    }

    const SHOW_SCRIPT: &str = r#"{"id": "show-ok", "rules": [
        {"when": {"lastRole": "tool", "contains": "invalid data for vam.progress"},
         "reply": {"text": "The host said no."}},
        {"when": {"lastRole": "user"},
         "reply": {"toolCall": {"name": "show", "args": {"jig": "vam.progress",
           "data": {"label": "Checking", "done": 1, "total": 2.5, "meta": {"a": [1, {"b": null}]}}}}}}]}"#;

    #[cfg(unix)]
    #[test]
    fn show_posts_the_card_data_as_json_and_the_hosts_refusal_reaches_the_model() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let run = run_outbox_script(SHOW_SCRIPT, "show-ok", true, |host| {
                host.post_outbox_error = Some((
                    3,
                    "invalid data for vam.progress: /total: expected integer".into(),
                ));
            })
            .await;
            assert_eq!(output_answer(&run.outcome), "The host said no.");
            assert_eq!(run.posts.len(), 1);
            assert_eq!(run.posts[0].jig, "vam.progress");
            let sent: serde_json::Value = serde_json::from_str(&run.posts[0].data_json).unwrap();
            assert_eq!(
                sent,
                serde_json::json!({"label": "Checking", "done": 1, "total": 2.5,
                                   "meta": {"a": [1, {"b": null}]}})
            );
        });
    }

    #[test]
    fn the_outbox_tools_are_offered_by_the_advertised_list_and_show_by_the_switch() {
        let runtime = SkillsRuntime::default();
        let advertised = vec![
            "read_file".to_string(),
            "post_message".into(),
            "show".into(),
        ];
        let names = |allowed: Vec<String>| -> Vec<String> {
            rebuild_tools_list(&runtime, &allowed, "", false, Termination::Unset)
                .into_iter()
                .map(|t| t.name)
                .collect()
        };
        let on = names(crate::outbox::allowed_for(advertised.clone(), true));
        assert!(on.contains(&"post_message".to_string()) && on.contains(&"show".to_string()));
        let off = names(crate::outbox::allowed_for(advertised, false));
        assert!(off.contains(&"post_message".to_string()) && !off.contains(&"show".to_string()));
        // An agent that denies post_message (not advertised) does not see it.
        let denied = names(vec!["read_file".to_string()]);
        assert!(!denied.contains(&"post_message".to_string()));
    }

    #[test]
    fn the_progress_line_is_in_the_prompt_only_with_post_message() {
        let runtime = SkillsRuntime::default();
        let ws = std::path::Path::new("/nonexistent-workspace");
        let with = rebuild_tools_list(
            &runtime,
            &["read_file".to_string(), "post_message".to_string()],
            "",
            false,
            Termination::Unset,
        );
        let prompt = build_agent_system_prompt(ws, None, &with, "", "");
        assert!(
            prompt.contains("`post_message`") && prompt.contains("## Progress"),
            "{prompt}"
        );
        let without = rebuild_tools_list(
            &runtime,
            &["read_file".to_string()],
            "",
            false,
            Termination::Unset,
        );
        let prompt = build_agent_system_prompt(ws, None, &without, "", "");
        assert!(!prompt.contains("## Progress"), "{prompt}");
        // A tool list with only post_message (no file, shell or todo tools) gets it too.
        let only = rebuild_tools_list(
            &runtime,
            &["post_message".to_string()],
            "",
            false,
            Termination::Unset,
        );
        assert!(build_agent_system_prompt(ws, None, &only, "", "").contains("## Progress"));
    }

    const PR_URN: &str = "urn:vam:playbook:local.vam.dev/acme/pr";

    #[test]
    fn start_playbook_is_offered_until_one_is_bound_then_the_step_tools_appear() {
        let runtime = SkillsRuntime::default();
        let allowed: Vec<String> = crate::primitives::builtin_tool_specs()
            .into_iter()
            .map(|t| t.name)
            .chain(
                ["start_playbook", "step_complete", "run_step"]
                    .iter()
                    .map(|n| n.to_string()),
            )
            .collect();
        let inputs = activation_inputs("x", &[]);
        let ws = std::path::Path::new("/nonexistent-workspace");

        let before = rebuild_tools_list(&runtime, &allowed, "", true, Termination::Unset);
        assert!(before.iter().any(|t| t.name == "start_playbook"));
        assert!(!before.iter().any(|t| t.name == "step_complete"));
        let prompt = build_agent_system_prompt(ws, Some(&inputs), &before, "", "");
        assert!(prompt.contains("## Playbooks you can start"), "{prompt}");
        assert!(prompt.contains("Use when the user wants to draft a pull request."));
        assert!(prompt.contains(PR_URN));

        let after = rebuild_tools_list(
            &runtime,
            &allowed,
            "# Draft a PR",
            false,
            Termination::Unset,
        );
        assert!(!after.iter().any(|t| t.name == "start_playbook"));
        assert!(after.iter().any(|t| t.name == "step_complete"));
        assert!(after.iter().any(|t| t.name == "run_step"));
        let prompt = build_agent_system_prompt(ws, Some(&inputs), &after, "", "");
        assert!(!prompt.contains("Playbooks you can start"));

        // Nothing offered: no tool and no section.
        let plain = rebuild_tools_list(&runtime, &allowed, "", false, Termination::Unset);
        assert!(!plain.iter().any(|t| t.name == "start_playbook"));
    }

    #[test]
    fn a_started_playbook_is_recognised_only_from_the_started_result() {
        assert!(playbook_started(
            "start_playbook",
            "Started playbook 'Draft a PR'. Follow it."
        ));
        assert!(!playbook_started("start_playbook", "tool_error: nope"));
        assert!(!playbook_started(
            "start_playbook",
            "Error: Tool 'start_playbook' was denied by security policy: x"
        ));
        assert!(!playbook_started("step_complete", "Started playbook 'x'."));
    }

    const HI_SCRIPT: &str = r#"{"id": "act-hi", "rules": [
        {"when": {"lastRole": "user"}, "reply": {"text": "Hello! How can I help?"}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_greeting_starts_no_playbook() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (outcome, intents) =
                run_activation_script(HI_SCRIPT, "Hi", activation_inputs("act-hi", &[]), |_| {})
                    .await;
            assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
            assert_eq!(output_answer(&outcome), "Hello! How can I help?");
            assert!(
                !intents.iter().any(|name| name == "start_playbook"),
                "{intents:?}"
            );
        });
    }

    const START_SCRIPT: &str = r#"{"id": "act-start", "rules": [
        {"when": {"lastRole": "tool", "contains": "Started playbook"},
         "reply": {"toolCall": {"name": "step_complete", "args": {"step_id": "draft", "outcome": "done"}}}},
        {"when": {"lastRole": "tool", "contains": "step recorded"},
         "reply": {"text": "The pull request is drafted."}},
        {"when": {"lastRole": "user"},
         "reply": {"toolCall": {"name": "start_playbook", "args": {"urn": "urn:vam:playbook:local.vam.dev/acme/pr", "inputs": {"title": "Fix"}}}}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_matching_request_starts_the_playbook_and_the_step_tools_work_afterwards() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (outcome, intents) = run_activation_script(
                START_SCRIPT,
                "Please draft a pull request for the fix",
                activation_inputs("act-start", &[]),
                |host| {
                    host.host_tool_text.insert(
                        "start_playbook".to_string(),
                        "Started playbook 'Draft a PR'. Follow the procedure below.\n\n# Draft a PR"
                            .to_string(),
                    );
                    host.host_tool_text
                        .insert("step_complete".to_string(), "step recorded".to_string());
                    host.host_tool_text.insert(
                        "check_playbook_finish".to_string(),
                        r#"{"complete":true,"parkable":false,"observation":""}"#.to_string(),
                    );
                },
            )
            .await;
            assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
            assert_eq!(output_answer(&outcome), "The pull request is drafted.");
            let start = intents.iter().position(|n| n == "start_playbook");
            let step = intents.iter().position(|n| n == "step_complete");
            assert!(
                start.is_some() && step.is_some() && start < step,
                "{intents:?}"
            );
            assert!(
                intents.iter().any(|n| n == "check_playbook_finish"),
                "the bound playbook's finish check ran at the end: {intents:?}"
            );
        });
    }

    const REFUSED_SCRIPT: &str = r#"{"id": "act-refused", "rules": [
        {"when": {"lastRole": "tool", "contains": "already follows playbook"},
         "reply": {"text": "I will continue with the current playbook."}},
        {"when": {"lastRole": "user"},
         "reply": {"toolCall": {"name": "start_playbook", "args": {"urn": "urn:vam:playbook:local.vam.dev/acme/pr"}}}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_refused_start_reaches_the_model_with_the_hosts_message() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (outcome, _) = run_activation_script(
                REFUSED_SCRIPT,
                "Draft a PR",
                activation_inputs("act-refused", &[]),
                |host| {
                    host.denied_tools.insert(
                        "start_playbook".to_string(),
                        "This task already follows playbook 'x'. A task runs one playbook."
                            .to_string(),
                    );
                },
            )
            .await;
            assert_eq!(
                output_answer(&outcome),
                "I will continue with the current playbook.",
                "the model saw the refusal text and answered on it"
            );
        });
    }

    const STEER_FINAL_SCRIPT: &str = r#"{"id": "steer-final", "rules": [
        {"when": {"lastRole": "user", "contains": "Use the title Final"},
         "reply": {"text": "Final answer after the steer."}},
        {"when": {"lastRole": "user"},
         "reply": {"text": "first answer"}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_steer_pending_at_a_final_answer_runs_another_round() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let (outcome, _) = run_activation_script(
                STEER_FINAL_SCRIPT,
                "Draft the title",
                activation_inputs("steer-final", &[]),
                |host| {
                    host.inbox = vec![(1, "Use the title Final instead".to_string())];
                    // The round-top pull finds nothing; the check before the final
                    // answer finds the steer.
                    host.inbox_hold_pulls = 1;
                },
            )
            .await;
            assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
            assert_eq!(output_answer(&outcome), "Final answer after the steer.");
        });
    }

    fn max_acked(pulls: &[(i64, i64)]) -> i64 {
        pulls.iter().map(|(_, ack)| *ack).max().unwrap_or(0)
    }

    #[cfg(unix)]
    #[test]
    fn the_batch_loop_acknowledges_only_what_was_applied_before_it() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let script = r#"{"id": "ack-batches", "rules": [
                {"when": {"lastRole": "user", "contains": "alpha\n\nbeta\n\ngamma"},
                 "reply": {"text": "done"}},
                {"when": {"lastRole": "user"}, "reply": {"text": "waiting"}}]}"#;
            let (_, _, pulls) = run_activation_script_full(
                script,
                "Start",
                activation_inputs("ack-batches", &[]),
                |host| {
                    host.inbox = vec![
                        (1, "alpha".to_string()),
                        (2, "beta".to_string()),
                        (3, "gamma".to_string()),
                    ];
                    host.inbox_batch_bytes = 1;
                },
            )
            .await;
            // The first cycle fetched 0, 1, 2, 3 and acknowledged nothing: the model
            // had not seen any of it. Only a later pull names seq 3 as applied.
            assert_eq!(&pulls[..4], &[(0, 0), (1, 0), (2, 0), (3, 0)]);
            assert!(pulls[4..]
                .iter()
                .all(|(after, ack)| *after == 3 && *ack == 3));
            assert!(
                !pulls[4..].is_empty(),
                "applied steers are acknowledged: {pulls:?}"
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_applied_steer_is_acknowledged_when_the_iteration_cap_leaves_no_later_pull() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let script = r#"{"id": "ack-cap", "rules": [
                {"when": {"lastRole": "user"}, "reply": {"text": "only answer"}}]}"#;
            let inputs = activation_inputs("ack-cap", &[("max_iterations", "1")]);
            let (outcome, _, pulls) = run_activation_script_full(script, "Start", inputs, |host| {
                host.inbox = vec![(1, "late thought".to_string())];
            })
            .await;
            assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
            // One round: the steer was applied at its top and no peek ran before the
            // answer (no iteration left). The end of the task still acknowledges it.
            assert_eq!(pulls.first(), Some(&(0, 0)));
            assert_eq!(max_acked(&pulls), 1, "{pulls:?}");
            assert_eq!(pulls.last(), Some(&(1, 1)));
        });
    }

    #[cfg(unix)]
    #[test]
    fn an_applied_steer_is_acknowledged_when_the_task_ends_after_tool_rounds() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            // The model asks for a tool the host denies every time; the invalid-call
            // budget is not involved, the task simply runs out of rounds in failure.
            let script = r#"{"id": "ack-fail", "rules": [
                {"when": {"lastRole": "user"}, "reply": {"toolCall": {"name": "no_such_tool", "args": {}}}},
                {"when": {"lastRole": "tool"}, "reply": {"toolCall": {"name": "no_such_tool", "args": {}}}}]}"#;
            let inputs = activation_inputs("ack-fail", &[("max_iterations", "2")]);
            let (_, _, pulls) = run_activation_script_full(script, "Start", inputs, |host| {
                host.inbox = vec![(1, "change of plan".to_string())];
            })
            .await;
            assert_eq!(max_acked(&pulls), 1, "{pulls:?}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn several_steers_in_bounded_batches_arrive_as_one_user_message() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let script = r#"{"id": "steer-batches", "rules": [
                {"when": {"lastRole": "user", "contains": "alpha\n\nbeta\n\ngamma"},
                 "reply": {"text": "saw all three in one message"}},
                {"when": {"lastRole": "user"},
                 "reply": {"text": "saw something else"}}]}"#;
            let (outcome, _) = run_activation_script(
                script,
                "Start",
                activation_inputs("steer-batches", &[]),
                |host| {
                    host.inbox = vec![
                        (1, "alpha".to_string()),
                        (2, "beta".to_string()),
                        (3, "gamma".to_string()),
                    ];
                    // One message per reply: the guest asks again until a reply is empty.
                    host.inbox_batch_bytes = 1;
                },
            )
            .await;
            assert_eq!(output_answer(&outcome), "saw all three in one message");
        });
    }

    #[test]
    fn a_delivered_steer_clears_the_loop_detectors_history() {
        let harness = vam_spec::harness::Harness::default();
        let mut detector = Some(crate::loop_detect::Detector::new(&harness.loop_));
        for _ in 0..3 {
            detector
                .as_mut()
                .unwrap()
                .observe("run", "{\"command\":\"true\"}", "ok");
            let _ = detector.as_mut().unwrap().end_round();
        }
        let mut messages = vec![ChatMessage {
            role: "tool".to_string(),
            content: "ok".to_string(),
            tool_calls: vec![],
            tool_call_id: "c1".to_string(),
        }];
        apply_steer(&mut messages, "new plan".to_string(), &mut detector);
        let last = messages.last().unwrap();
        assert_eq!(
            (last.role.as_str(), last.content.as_str()),
            ("user", "new plan")
        );
        // The same call again starts from nothing: no verdict yet.
        detector
            .as_mut()
            .unwrap()
            .observe("run", "{\"command\":\"true\"}", "ok");
        assert!(matches!(
            detector.as_mut().unwrap().end_round(),
            crate::loop_detect::Verdict::Continue
        ));
    }

    #[test]
    fn a_resume_starts_pulling_after_the_seq_the_suspended_task_applied() {
        use vam_spec::thread_context::{LoopState, PendingCall, SuspendResume, ThreadContext};
        let ctx = ThreadContext {
            format: "vam.thread-context".to_string(),
            version: 1,
            run_id: "run-1".to_string(),
            head_seq: 0,
            head_digest: String::new(),
            system_sha256: None,
            tools_sha256: None,
            messages: vec![],
            blocks: vec![],
            suspend: Some(SuspendResume {
                lifecycle_generation: 1,
                pending: PendingCall {
                    tool_call_id: "c1".into(),
                    tool_name: "echo".into(),
                    args_json: "{}".into(),
                },
                unanswered: vec![],
                state: LoopState {
                    inbox_seq: 7,
                    ..Default::default()
                },
            }),
            crash: None,
        };
        let continuation =
            crate::hitl_checkpoint::continuation_from_context(&ctx).expect("suspend");
        assert_eq!(continuation.inbox_seq, 7);
    }

    const CLAIM_BOUND_SCRIPT: &str = r#"{"id": "act-claim", "rules": [
        {"when": {"lastRole": "tool", "contains": "Playbook finish is incomplete"},
         "reply": {"toolCall": {"name": "task_complete", "args": {"summary": "Done for real."}}}},
        {"when": {"lastRole": "tool", "contains": "Started playbook"},
         "reply": {"toolCall": {"name": "task_complete", "args": {"summary": "Done early."}}}},
        {"when": {"lastRole": "user"},
         "reply": {"toolCall": {"name": "start_playbook", "args": {"urn": "urn:vam:playbook:local.vam.dev/acme/pr"}}}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_playbook_bound_mid_run_still_decides_at_task_complete() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let probes = std::sync::Arc::new(std::sync::Mutex::new(0usize));
            let seen = probes.clone();
            let _guard = FinishProbeGuard::install(move |_output| {
                let mut count = seen.lock().expect("probes");
                *count += 1;
                if *count == 1 {
                    Ok(FinishReport {
                        complete: false,
                        parkable: false,
                        observation: "Playbook finish is incomplete. Do not end the task yet.\nMissing required steps: draft.".to_string(),
                    })
                } else {
                    Ok(FinishReport {
                        complete: true,
                        parkable: false,
                        observation: String::new(),
                    })
                }
            });
            let (outcome, intents) = run_activation_script(
                CLAIM_BOUND_SCRIPT,
                "Please draft a pull request",
                activation_inputs("act-claim", &[("termination", "explicit_claim")]),
                |host| {
                    host.host_tool_text.insert(
                        "start_playbook".to_string(),
                        "Started playbook 'Draft a PR'. Follow the procedure below.\n\n# Draft a PR"
                            .to_string(),
                    );
                },
            )
            .await;
            assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
            assert_eq!(output_answer(&outcome), "Done for real.");
            assert_eq!(
                *probes.lock().expect("probes"),
                2,
                "the first task_complete was held by the playbook's finish check"
            );
            assert!(intents.iter().any(|n| n == "start_playbook"), "{intents:?}");
        });
    }

    #[cfg(unix)]
    #[test]
    fn explicit_claim_ends_on_task_complete() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let outcome =
                run_claim_script(CLAIM_SCRIPT, "claim-done", Some("explicit_claim")).await;
            assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
            assert_eq!(output_answer(&outcome), "All done.");
            assert_eq!(output_claimed(&outcome), Some(true));
        });
    }

    #[cfg(unix)]
    #[test]
    fn explicit_claim_text_only_reply_gets_one_reprompt_then_ends_unclaimed() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let outcome =
                run_claim_script(TEXT_ONLY_SCRIPT, "text-only", Some("explicit_claim")).await;
            assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
            // The second text came from the re-prompt turn; no third turn happened.
            assert_eq!(output_answer(&outcome), "Second text.");
            assert_eq!(output_claimed(&outcome), Some(false));
        });
    }

    #[cfg(unix)]
    #[test]
    fn explicit_claim_after_the_reprompt_counts_as_claimed() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let outcome = run_claim_script(
                CLAIM_AFTER_REPROMPT_SCRIPT,
                "claim-late",
                Some("explicit_claim"),
            )
            .await;
            assert_eq!(output_answer(&outcome), "Done after the nudge.");
            assert_eq!(output_claimed(&outcome), Some(true));
        });
    }

    #[cfg(unix)]
    #[test]
    fn unset_termination_keeps_the_text_only_end() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let outcome = run_claim_script(TEXT_ONLY_SCRIPT, "text-only", None).await;
            assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
            assert_eq!(output_answer(&outcome), "First text.");
            assert_eq!(output_claimed(&outcome), None);
        });
    }

    #[cfg(unix)]
    #[test]
    fn step_timeout_fails_the_round_when_the_model_never_answers() {
        // The broker lock is taken by this sync test, never held across an await.
        let _broker = mock_broker_lock();
        run_socket_test(step_timeout_fails_the_round_body());
    }

    #[cfg(unix)]
    async fn step_timeout_fails_the_round_body() {
        let serving_52 = spawn_checkpoint_host(
            bind_mock_port(52),
            crate::pep::test_support::shared_host(None),
        );
        let holding_53 = spawn_silent_broker(bind_mock_port(53));

        let mut fields = std::collections::BTreeMap::new();
        string_input(&mut fields, "step_timeout_secs", "1");
        let inputs = Struct { fields };
        let prompt = PromptAction {
            instruction_template: "say hello".to_string(),
            inject_context_keys: vec![],
        };

        let outcome =
            run_react_agent_loop_with_trace(&checkpoint_test_lease(), &prompt, Some(&inputs), None)
                .await;

        serving_52.abort();
        holding_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);

        let outcome = outcome.expect("task outcome");
        assert_eq!(outcome.state, TaskOutcomeState::Failed as i32);
        assert_eq!(
            outcome.error.expect("error").message,
            "step exceeded stepTimeout of 1s (round 1)"
        );
        assert_eq!(outcome.observed_usage, Some(TokenUsage::default()));
    }

    struct FinishProbeGuard;

    impl FinishProbeGuard {
        fn install<F>(probe: F) -> Self
        where
            F: Fn(&str) -> Result<FinishReport, String> + Send + Sync + 'static,
        {
            *TEST_FINISH_PROBE.lock().expect("probe lock") = Some(std::sync::Arc::new(probe));
            Self
        }
    }

    impl Drop for FinishProbeGuard {
        fn drop(&mut self) {
            if let Ok(mut guard) = TEST_FINISH_PROBE.lock() {
                *guard = None;
            }
        }
    }

    fn number_input(
        fields: &mut std::collections::BTreeMap<String, prost_types::Value>,
        key: &str,
        n: f64,
    ) {
        fields.insert(
            key.to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::NumberValue(n)),
            },
        );
    }

    fn string_input(
        fields: &mut std::collections::BTreeMap<String, prost_types::Value>,
        key: &str,
        value: &str,
    ) {
        fields.insert(
            key.to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(value.to_string())),
            },
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn test_agent_react_loop_e2e_mock() {
        let _broker = mock_broker_lock();
        let mock_sock_path = mock_port_path(53);
        let _ = std::fs::remove_file(&mock_sock_path);
        let listener = UnixListener::bind(&mock_sock_path).unwrap();

        let catalog = Arc::new(ModelCatalog::new());
        let budget = Arc::new(SessionBudget::new(None));
        let secrets = Arc::new(SimpleSecretLookup::new());
        let broker_server = Arc::new(InferenceBrokerServer::new(catalog, budget, secrets));

        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                broker_server.handle_connection(stream).await;
            }
        });

        let lease = ExecutionLease {
            run_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:run:123".to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:attempt:1".to_string(),
            }),
            lease_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:lease:1".to_string(),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4102444800,
                nanos: 0,
            }),
            idempotency_key: "idemp".to_string(),
        };

        let prompt = PromptAction {
            instruction_template: "please run-echo now".to_string(),
            inject_context_keys: vec![],
        };

        let outcome_res = run_react_agent_loop(&lease, &prompt, None).await;

        assert!(outcome_res.is_ok());
        let outcome = outcome_res.unwrap();
        assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);

        let out_struct = outcome.output.unwrap();
        let answer_val = out_struct.fields.get("answer").unwrap();
        if let Some(prost_types::value::Kind::StringValue(ref s)) = answer_val.kind {
            assert!(
                s.contains("mock answer") || !s.is_empty(),
                "Answer is: {}",
                s
            );
        }

        server_handle.abort();
        let _ = std::fs::remove_file(&mock_sock_path);
    }

    #[test]
    fn in_flight_context_adds_guidance_and_the_gate_status_after_the_instruction() {
        let mut inputs = Struct::default();
        assert_eq!(in_flight_context(Some(&inputs)), None);
        let text = |s: &str| prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(s.to_string())),
        };
        inputs
            .fields
            .insert("playbook_guidance".into(), text("Use the second person."));
        let only_guidance = in_flight_context(Some(&inputs)).expect("guidance");
        assert!(only_guidance.contains("Use the second person."));
        assert!(!only_guidance.contains("Playbook status"));
        inputs.fields.insert(
            "hitl_resume_note".into(),
            text("Gate 'mode' was decided with outcome 'guided'."),
        );
        inputs.fields.insert(
            "playbook_status".into(),
            text("[Playbook status: X]\nAvailable now: find (bound: call run_step)"),
        );
        let both = in_flight_context(Some(&inputs)).expect("both");
        assert!(both.contains("Gate 'mode' was decided"));
        assert!(both.contains("Available now: find"));
        assert!(both.contains(vam_spec::playbook::PLAYBOOK_GATE_RULE));
    }

    #[test]
    fn the_status_block_never_becomes_part_of_the_system_prompt_procedure() {
        let output = format!(
            "{}'X'. Follow the procedure.\n\n# X\n## a\n\n[Playbook status: X]\nAvailable now: a",
            vam_spec::playbook::PLAYBOOK_STARTED_PREFIX
        );
        assert!(playbook_started("start_playbook", &output));
        let procedure = procedure_without_status(&output);
        assert!(procedure.ends_with("## a"), "{procedure}");
        assert!(!procedure.contains("Playbook status"));
        let guidance = format!(
            "{}'G'. Follow this guidance:\n\nBe brief.",
            vam_spec::playbook::PLAYBOOK_GUIDANCE_PREFIX
        );
        assert!(!playbook_started("start_playbook", &guidance));
    }

    #[test]
    fn playbook_finish_fallback_nudge_names_the_right_action() {
        let nudge = super::playbook_finish_fallback_observation();
        assert!(nudge.contains(
            "For each missing step: call run_step if it is bound, do the work and call step_complete if it is agentic, and wait for the human decision if it is a gate. Never invent a step's outcome."
        ));
        assert!(nudge.contains("Reply with run output that matches the playbook output schema."));
        assert!(!nudge.contains(
            "Call step_complete for each missing step, wait for open gates to be decided"
        ));
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn playbook_finish_feedback_continues_then_commits_within_max_iterations() {
        let _broker = mock_broker_lock();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let seen_probe = seen.clone();
        let _guard = FinishProbeGuard::install(move |output| {
            let mut log = seen_probe.lock().expect("seen");
            log.push(output.to_string());
            if log.len() == 1 {
                Ok(FinishReport {
                    complete: false,
                    parkable: false,
                    observation: "Playbook finish is incomplete. Do not end the task yet.\nMissing required steps: first.\nFor each missing step: call run_step if it is bound, do the work and call step_complete if it is agentic, and wait for the human decision if it is a gate. Never invent a step's outcome.\nReply with run output that matches the playbook output schema.".to_string(),
                })
            } else {
                Ok(FinishReport {
                    complete: true,
                    parkable: false,
                    observation: String::new(),
                })
            }
        });

        let mut fields = std::collections::BTreeMap::new();
        number_input(&mut fields, "max_iterations", 5.0);
        string_input(
            &mut fields,
            "playbook_procedure",
            "1. Finish the first step",
        );
        let inputs = Struct { fields };

        let outcome =
            run_mock_playbook_turn("vam-playbook-finish-fixture please finish", Some(inputs)).await;
        assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
        let answer = outcome
            .output
            .as_ref()
            .and_then(|output| output.fields.get("answer"))
            .and_then(|value| match &value.kind {
                Some(prost_types::value::Kind::StringValue(text)) => Some(text.clone()),
                _ => None,
            })
            .expect("answer");
        assert!(
            answer.contains("ok"),
            "continued past the first finish and committed the later answer: {answer}"
        );
        {
            let log = seen.lock().expect("seen");
            assert!(
                log.len() >= 2,
                "first incomplete finish must not commit; probes: {log:?}"
            );
            assert!(
                log[0].contains("finishing early"),
                "first probe saw the early finish: {}",
                log[0]
            );
            assert!(
                !answer.contains("finishing early"),
                "the early finish was not committed"
            );
        }
        drop(_guard);

        let seen_cap = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let seen_cap_probe = seen_cap.clone();
        let _cap_guard = FinishProbeGuard::install(move |_output| {
            *seen_cap_probe.lock().expect("cap") += 1;
            Ok(FinishReport {
                complete: false,
                parkable: false,
                observation: "Playbook finish is incomplete. Do not end the task yet.\nMissing required steps: first.\nCall step_complete for each missing step.".to_string(),
            })
        });
        let mut fields = std::collections::BTreeMap::new();
        number_input(&mut fields, "max_iterations", 1.0);
        string_input(
            &mut fields,
            "playbook_procedure",
            "1. Finish the first step",
        );
        let capped = run_mock_playbook_turn(
            "vam-playbook-finish-fixture please finish",
            Some(Struct { fields }),
        )
        .await;
        assert_eq!(capped.state, TaskOutcomeState::Succeeded as i32);
        assert_eq!(
            *seen_cap.lock().expect("cap"),
            1,
            "the cap commits after one probe instead of looping past max_iterations"
        );
        let capped_answer = capped
            .output
            .as_ref()
            .and_then(|output| output.fields.get("answer"))
            .and_then(|value| match &value.kind {
                Some(prost_types::value::Kind::StringValue(text)) => Some(text.clone()),
                _ => None,
            })
            .expect("capped answer");
        assert!(
            capped_answer.contains("finishing early"),
            "at the cap the guest still returns the answer for the host to fail: {capped_answer}"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_denied_call_ends_a_playbook_run_at_the_first_incomplete_finish() {
        let _broker = mock_broker_lock();
        // A host whose finish check always says steps are missing and never parks.
        let probes = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let probes_seen = probes.clone();
        let _guard = FinishProbeGuard::install(move |_output| {
            *probes_seen.lock().expect("probes") += 1;
            Ok(FinishReport {
                complete: false,
                parkable: false,
                observation: "Playbook finish is incomplete. Do not end the task yet.\nMissing required steps: first.\nfirst: Do this step yourself with the tools you have.".to_string(),
            })
        });
        let mut fields = std::collections::BTreeMap::new();
        number_input(&mut fields, "max_iterations", 20.0);
        string_input(
            &mut fields,
            "playbook_procedure",
            "1. Finish the first step",
        );
        // The user turned down the task's question.
        fields.insert(
            "hitl_denied".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::BoolValue(true)),
            },
        );
        string_input(&mut fields, "denied_tool", "ask_user");
        fields.insert(
            "denied_tool_args".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StructValue(Struct::default())),
            },
        );
        string_input(&mut fields, "denial_reason", "not now");

        let outcome = run_mock_playbook_turn(
            "vam-playbook-finish-fixture please finish",
            Some(Struct { fields }),
        )
        .await;
        assert_eq!(
            outcome.state,
            TaskOutcomeState::Succeeded as i32,
            "the guest commits its answer; the host's own check fails the run"
        );
        assert_eq!(
            *probes.lock().expect("probes"),
            1,
            "after a denial the finish check is asked once and never sends the task back to work"
        );
    }

    #[test]
    fn finish_report_reads_parkable_and_defaults_it_to_false() {
        let parked = parse_finish_report(
            r#"{"complete":false,"parkable":true,"open_gates":["decide"],"observation":"x"}"#,
        )
        .expect("report");
        assert!(!parked.complete && parked.parkable);
        let older_host = parse_finish_report(r#"{"complete":false,"observation":"x"}"#)
            .expect("report from a host that has no parkable field");
        assert!(!older_host.parkable);
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn a_run_that_waits_only_on_a_gate_commits_at_the_first_finish_instead_of_looping() {
        let _broker = mock_broker_lock();
        let probes = std::sync::Arc::new(std::sync::Mutex::new(0usize));
        let probes_seen = probes.clone();
        let _guard = FinishProbeGuard::install(move |_output| {
            *probes_seen.lock().expect("probes") += 1;
            Ok(FinishReport {
                complete: false,
                parkable: true,
                observation:
                    "Playbook finish is incomplete. Do not end the task yet.\nOpen gates: decide."
                        .to_string(),
            })
        });
        let mut fields = std::collections::BTreeMap::new();
        number_input(&mut fields, "max_iterations", 50.0);
        string_input(
            &mut fields,
            "playbook_procedure",
            "1. Wait for the decision",
        );
        let outcome = run_mock_playbook_turn(
            "vam-playbook-finish-fixture please finish",
            Some(Struct { fields }),
        )
        .await;
        assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
        assert_eq!(
            *probes.lock().expect("probes"),
            1,
            "the guest must not ask the model again while only a gate is open"
        );
    }

    #[test]
    fn model_preference_uses_explicit_provider() {
        let inputs = model_inputs(&[
            ("model", "openai/gpt-4o-mini"),
            ("model_provider", "vercel"),
        ]);
        let pref = model_preference_from_inputs(Some(&inputs)).unwrap();
        assert_eq!(pref.provider, "vercel");
        assert_eq!(pref.model_name, "openai/gpt-4o-mini");
    }

    #[test]
    fn model_preference_takes_the_host_provider_and_never_guesses_from_the_slash() {
        let inputs = model_inputs(&[("model", "openai/gpt-4o-mini")]);
        let pref = model_preference_from_inputs(Some(&inputs)).unwrap();
        assert_eq!(pref.provider, "");
        let inputs = model_inputs(&[
            ("model", "openai/gpt-4o-mini"),
            ("model_provider", "vercel"),
        ]);
        let pref = model_preference_from_inputs(Some(&inputs)).unwrap();
        assert_eq!(pref.provider, "vercel");
        let inputs = model_inputs(&[("model", "mock-model"), ("model_provider", "vercel")]);
        assert_eq!(
            model_preference_from_inputs(Some(&inputs))
                .unwrap()
                .provider,
            "mock"
        );
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn inference_error_fails_run_instead_of_looping() {
        let _broker = mock_broker_lock();
        // The test broker has no API keys, so the host answers with an error chunk.
        let inputs = model_inputs(&[("model", "nokey/some-model"), ("model_provider", "nokey")]);
        let outcome = run_mock_playbook_turn("hello", Some(inputs)).await;
        assert_eq!(outcome.state, TaskOutcomeState::Failed as i32);
        let msg = outcome.error.expect("error status").message;
        assert!(msg.starts_with("inference failed:"), "{msg}");
    }

    #[cfg(unix)]
    async fn run_mock_playbook_turn(instruction: &str, inputs: Option<Struct>) -> TaskOutcome {
        let mock_sock_path = mock_port_path(53);
        let _ = std::fs::remove_file(&mock_sock_path);
        let listener = UnixListener::bind(&mock_sock_path).unwrap();
        let catalog = Arc::new(ModelCatalog::new());
        let budget = Arc::new(SessionBudget::new(None));
        let secrets = Arc::new(SimpleSecretLookup::new());
        let broker_server = Arc::new(InferenceBrokerServer::new(catalog, budget, secrets));
        let server_handle = tokio::spawn(async move {
            if let Ok((stream, _)) = listener.accept().await {
                broker_server.handle_connection(stream).await;
            }
        });
        let lease = ExecutionLease {
            run_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:run:finish-probe".to_string(),
            }),
            step_id: "step-1".to_string(),
            attempt_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:attempt:1".to_string(),
            }),
            lease_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:lease:1".to_string(),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: Some(prost_types::Timestamp {
                seconds: 4102444800,
                nanos: 0,
            }),
            idempotency_key: "idemp".to_string(),
        };
        let prompt = PromptAction {
            instruction_template: instruction.to_string(),
            inject_context_keys: vec![],
        };
        let outcome = run_react_agent_loop(&lease, &prompt, inputs.as_ref())
            .await
            .expect("loop");
        server_handle.abort();
        let _ = std::fs::remove_file(&mock_sock_path);
        outcome
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "current_thread")]
    async fn prose_that_sounds_like_deferred_tool_intent_is_not_nudged() {
        let _broker = mock_broker_lock();
        // The prompt carries words the old keyword list matched ("search", "fetch", "issue ", "cga-"), and
        // the reply says "I'll check ... fetch the details". The runtime must commit it as the answer
        // without adding a user message of its own.
        let outcome = run_mock_playbook_turn(
            "vam-nudge-probe search the issue CGA-1 and fetch the details",
            None,
        )
        .await;
        assert_eq!(outcome.state, TaskOutcomeState::Succeeded as i32);
        let answer = match outcome
            .output
            .as_ref()
            .and_then(|out| out.fields.get("answer"))
            .and_then(|v| v.kind.as_ref())
        {
            Some(prost_types::value::Kind::StringValue(s)) => s.clone(),
            other => panic!("no string answer: {other:?}"),
        };
        assert!(
            answer.contains("user_messages=1"),
            "the loop added a user message: {answer}"
        );
    }

    fn write_test_skill(base: &std::path::Path, skill_id: &str, skills_md: &str, manifest: &str) {
        let dir = base.join(skill_id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILLS.md"), skills_md).unwrap();
        std::fs::write(dir.join("manifest.json"), manifest).unwrap();
    }

    fn sample_resume_checkpoint() -> HitlContinuation {
        HitlContinuation {
            schema_version: hitl_checkpoint::SCHEMA_VERSION,
            run_id: "urn:vam:run:local.vam.dev/acme/run-resume".to_string(),
            lifecycle_generation: 2,
            messages: vec![
                hitl_checkpoint::SerdeChatMessage {
                    role: "user".to_string(),
                    content: "Post then read notes.".to_string(),
                    tool_calls: vec![],
                    tool_call_id: String::new(),
                },
                hitl_checkpoint::SerdeChatMessage {
                    role: "assistant".to_string(),
                    content: String::new(),
                    tool_calls: vec![
                        hitl_checkpoint::SerdeToolCall {
                            id: "call_pending".to_string(),
                            name: "slack_post_message".to_string(),
                            arguments: r#"{"channel":"ops"}"#.to_string(),
                        },
                        hitl_checkpoint::SerdeToolCall {
                            id: "call_sibling".to_string(),
                            name: "read_file".to_string(),
                            arguments: r#"{"path":"notes.md"}"#.to_string(),
                        },
                    ],
                    tool_call_id: String::new(),
                },
            ],
            loaded_skill_ids: vec!["slack-notify".to_string()],
            iterations_used: 3,
            max_iterations: 9,
            prompt_tokens: 80,
            completion_tokens: 20,
            cached_tokens: 0,
            total_tokens: 100,
            nudge_count: 1,
            last_tool_name: "load_skill".to_string(),
            same_tool_rounds: 1,
            skill_tool_called: false,
            last_answer_candidate: "working".to_string(),
            pending_tool_call_id: "call_pending".to_string(),
            pending_tool_name: "slack_post_message".to_string(),
            pending_tool_args_json: r#"{"channel":"ops"}"#.to_string(),
            unanswered_tool_call_ids: vec!["call_sibling".to_string()],
            claim_reprompted: false,
            inbox_seq: 0,
        }
    }

    #[test]
    fn ensure_skill_for_tool_resolves_non_jira_skill() {
        let _guard = crate::skills::skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        std::fs::create_dir_all(&mount).unwrap();
        write_test_skill(
            &mount,
            "slack-notify",
            "# Slack\n\nPost to Slack.",
            r#"{"tools":[{"name":"slack_post_message","description":"Post","input_schema_json":"{}"}]}"#,
        );
        std::env::set_var("VAM_SKILLS_MOUNT", &mount);

        let mut runtime = SkillsRuntime::default();
        ensure_skill_for_tool(&mut runtime, "slack_post_message").expect("load slack skill");
        assert!(runtime.is_loaded("slack-notify"));
        assert!(runtime
            .loaded_tools()
            .iter()
            .any(|t| t.name == "slack_post_message"));

        std::env::remove_var("VAM_SKILLS_MOUNT");
    }

    #[test]
    fn ensure_skill_for_tool_allows_host_exec_without_skill() {
        let _guard = crate::skills::skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        std::fs::create_dir_all(&mount).unwrap();
        std::env::set_var("VAM_SKILLS_MOUNT", &mount);

        let mut runtime = SkillsRuntime::default();
        ensure_skill_for_tool(&mut runtime, "echo.ping").expect("HSP tool needs no skill");
        assert!(runtime.loaded_skill_ids().is_empty());

        std::env::remove_var("VAM_SKILLS_MOUNT");
    }

    #[test]
    fn hitl_checkpoint_restore_continues_without_force_finalize() {
        let _guard = crate::skills::skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        std::fs::create_dir_all(&mount).unwrap();
        write_test_skill(
            &mount,
            "slack-notify",
            "# Slack\n\nPost to Slack.",
            r#"{"tools":[{"name":"slack_post_message","description":"Post","input_schema_json":"{}"}]}"#,
        );
        std::env::set_var("VAM_SKILLS_MOUNT", &mount);

        let mut skills_runtime = SkillsRuntime::default();
        let checkpoint = sample_resume_checkpoint();
        reload_checkpoint_skills(&mut skills_runtime, &checkpoint.loaded_skill_ids);
        assert!(skills_runtime.is_loaded("slack-notify"));

        let mut messages = Vec::new();
        let mut total_tokens = TokenUsage::default();
        let mut nudge_count = 0usize;
        let mut last_answer_candidate = String::new();
        let mut skill_tool_called = false;
        let mut force_finalize = true;
        let mut iteration_cap = 5usize;
        let mut iterations_used_before_loop = 0usize;
        let mut max_iterations = 5usize;

        apply_hitl_checkpoint_restore(
            &checkpoint,
            "REBUILT SYSTEM PROMPT",
            "posted to ops".to_string(),
            "Continue the original task.",
            "slack_post_message",
            &skills_runtime,
            &mut messages,
            &mut total_tokens,
            &mut nudge_count,
            &mut last_answer_candidate,
            &mut skill_tool_called,
            &mut force_finalize,
            &mut iteration_cap,
            &mut iterations_used_before_loop,
            &mut max_iterations,
            &mut false,
        );

        assert!(
            !force_finalize,
            "resume with checkpoint must continue the loop"
        );
        assert_eq!(messages[0].role, "system");
        assert_eq!(messages[0].content, "REBUILT SYSTEM PROMPT");
        assert_eq!(max_iterations, 6);
        assert_eq!(iterations_used_before_loop, 3);
        assert_eq!(total_tokens.total_tokens, 100);
        assert_eq!(last_answer_candidate, "posted to ops");
        assert!(skill_tool_called);
        assert!(messages
            .iter()
            .any(|m| m.role == "tool" && m.tool_call_id == "call_pending"));
        assert!(
            !messages.iter().any(|m| m.tool_call_id == "hitl_resume_0"),
            "must use the checkpoint pending tool_call_id"
        );
        let sibling = messages
            .iter()
            .find(|m| m.role == "tool" && m.tool_call_id == "call_sibling")
            .expect("sibling placeholder");
        assert!(sibling.content.contains("sibling tool call"));

        std::env::remove_var("VAM_SKILLS_MOUNT");
    }

    #[test]
    fn hitl_resume_without_checkpoint_falls_back_to_summarize() {
        let mut messages = vec![ChatMessage {
            role: "system".to_string(),
            content: "sys".to_string(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        }];
        let mut last_answer_candidate = String::new();
        let mut force_finalize = false;
        apply_hitl_resume_fallback(
            "sys",
            "do the thing",
            "jira_transition_issue",
            r#"{"issue_key":"CGA-1"}"#,
            "transitioned".to_string(),
            "Continue the original task.",
            &mut messages,
            &mut last_answer_candidate,
            &mut force_finalize,
        );
        assert!(force_finalize);
        assert!(messages.iter().any(|m| m.tool_call_id == "hitl_resume_0"));
        assert_eq!(last_answer_candidate, "transitioned");
    }

    #[test]
    fn hitl_checkpoint_with_high_same_tool_rounds_does_not_force_finalize() {
        let _guard = crate::skills::skills_mount_test_guard();
        let dir = tempfile::tempdir().unwrap();
        let mount = dir.path().join("skills");
        std::fs::create_dir_all(&mount).unwrap();
        write_test_skill(
            &mount,
            "slack-notify",
            "# Slack\n\nPost to Slack.",
            r#"{"tools":[{"name":"slack_post_message","description":"Post","input_schema_json":"{}"}]}"#,
        );
        std::env::set_var("VAM_SKILLS_MOUNT", &mount);

        let mut skills_runtime = SkillsRuntime::default();
        let mut checkpoint = sample_resume_checkpoint();
        checkpoint.last_tool_name = "grep".to_string();
        checkpoint.same_tool_rounds = 99;
        reload_checkpoint_skills(&mut skills_runtime, &checkpoint.loaded_skill_ids);

        let mut messages = Vec::new();
        let mut total_tokens = TokenUsage::default();
        let mut nudge_count = 0usize;
        let mut last_answer_candidate = String::new();
        let mut skill_tool_called = false;
        let mut force_finalize = true;
        let mut iteration_cap = 5usize;
        let mut iterations_used_before_loop = 0usize;
        let mut max_iterations = 5usize;

        apply_hitl_checkpoint_restore(
            &checkpoint,
            "REBUILT SYSTEM PROMPT",
            "posted to ops".to_string(),
            "Continue the original task.",
            "grep",
            &skills_runtime,
            &mut messages,
            &mut total_tokens,
            &mut nudge_count,
            &mut last_answer_candidate,
            &mut skill_tool_called,
            &mut force_finalize,
            &mut iteration_cap,
            &mut iterations_used_before_loop,
            &mut max_iterations,
            &mut false,
        );

        assert!(
            !force_finalize,
            "retired same-tool checkpoint state must not force finalization"
        );
        assert_eq!(max_iterations, 6);

        std::env::remove_var("VAM_SKILLS_MOUNT");
    }

    #[test]
    fn default_hitl_resume_nudge_instructs_continuation() {
        let note = default_hitl_resume_nudge();
        assert!(note.contains("Continue the original task"));
        assert!(!note.contains("Summarize the result"));
    }

    fn resume_lease(run_id: &str) -> ExecutionLease {
        ExecutionLease {
            run_id: Some(vam_spec::v1::ResourceId {
                urn: run_id.to_string(),
            }),
            step_id: "step-resume".to_string(),
            attempt_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:attempt:resume".to_string(),
            }),
            lease_id: Some(vam_spec::v1::ResourceId {
                urn: "urn:vam:lease:resume".to_string(),
            }),
            lifecycle_generation: 1,
            fencing_token: vec![1],
            expires_at: None,
            idempotency_key: "resume".to_string(),
        }
    }

    #[tokio::test]
    async fn resume_without_digest_ignores_the_inline_continuation() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-1";
        let (bytes, _) =
            hitl_checkpoint::encode_hitl_continuation(&sample_resume_checkpoint()).expect("encode");
        let mut inputs = Struct::default();
        inputs.fields.insert(
            "hitl_continuation".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(
                    String::from_utf8(bytes).expect("utf8"),
                )),
            },
        );

        let lease = resume_lease(run_id);
        assert!(
            matches!(
                load_resume_checkpoint(&lease, Some(&inputs), run_id).await,
                Ok(None)
            ),
            "an inline continuation without a digest is not read"
        );
        assert!(matches!(
            load_resume_checkpoint(&lease, None, run_id).await,
            Ok(None)
        ));
    }

    #[tokio::test]
    async fn resume_with_an_empty_digest_fails_before_contacting_the_host() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-1";
        let mut inputs = Struct::default();
        inputs.fields.insert(
            "hitl_continuation_digest".to_string(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(String::new())),
            },
        );
        let err = load_resume_checkpoint(&resume_lease(run_id), Some(&inputs), run_id)
            .await
            .expect_err("an empty digest cannot be verified");
        assert!(err.contains("digest is empty"), "{err}");
    }

    #[tokio::test]
    async fn one_mib_conversation_uploads_in_chunks_and_resumes_through_fetch() {
        let run_id = "urn:vam:run:local.vam.dev/acme/run-big";
        let lease = resume_lease(run_id);
        let long_user = "research notes ".repeat(75_000);
        let messages = vec![
            ChatMessage {
                role: "system".to_string(),
                content: "SYSTEM PROMPT v1".to_string(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            },
            ChatMessage {
                role: "user".to_string(),
                content: long_user.clone(),
                tool_calls: vec![],
                tool_call_id: String::new(),
            },
            ChatMessage {
                role: "assistant".to_string(),
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: "call_pending".to_string(),
                    name: "echo.ping".to_string(),
                    arguments: "{}".to_string(),
                }],
                tool_call_id: String::new(),
            },
        ];
        let continuation = HitlContinuation {
            schema_version: hitl_checkpoint::SCHEMA_VERSION,
            run_id: run_id.to_string(),
            lifecycle_generation: 1,
            messages: hitl_checkpoint::serde_messages_without_system(&messages),
            loaded_skill_ids: vec![],
            iterations_used: 1,
            max_iterations: 50,
            prompt_tokens: 0,
            completion_tokens: 0,
            cached_tokens: 0,
            total_tokens: 0,
            nudge_count: 0,
            last_tool_name: String::new(),
            same_tool_rounds: 0,
            skill_tool_called: false,
            last_answer_candidate: String::new(),
            pending_tool_call_id: "call_pending".to_string(),
            pending_tool_name: "echo.ping".to_string(),
            pending_tool_args_json: "{}".to_string(),
            unanswered_tool_call_ids: vec![],
            claim_reprompted: false,
            inbox_seq: 0,
        };
        let (bytes, digest) =
            hitl_checkpoint::encode_hitl_continuation(&continuation).expect("encode");
        assert!(bytes.len() > 1024 * 1024);
        assert!(bytes.len() <= crate::pep::MAX_CHECKPOINT_BYTES);

        let host = crate::pep::test_support::shared_host(None);
        let mut uploading = crate::pep::test_support::connect(&host);
        crate::pep::upload_checkpoint_chunks(&mut uploading, &lease, &bytes, &digest)
            .await
            .expect("upload in chunks");
        let mut fetching = crate::pep::test_support::connect(&host);
        let fetched = crate::pep::fetch_checkpoint_chunks(&mut fetching, &lease, &digest)
            .await
            .expect("fetch by digest");
        assert_eq!(fetched, bytes);

        let json = std::str::from_utf8(&fetched).expect("utf8");
        let restored = hitl_checkpoint::continuation_from_host_payload(json, run_id, &digest)
            .expect("digest and schema verify");
        let rebuilt = "REBUILT SYSTEM PROMPT v2";
        let resumed = hitl_checkpoint::restore_messages_from_checkpoint(
            &restored,
            rebuilt,
            "approved result".to_string(),
            "Continue the original task.",
        );
        assert_eq!(resumed[0].role, "system");
        assert_eq!(resumed[0].content, rebuilt);
        assert_eq!(resumed[1].content, long_user);
        assert_eq!(resumed[2].tool_calls[0].id, "call_pending");
        assert_eq!(resumed[3].role, "tool");
        assert_eq!(resumed[3].content, "approved result");
    }

    #[tokio::test]
    async fn round_budget_charges_guest_time_but_not_host_waits() {
        let mut budget = RoundBudget::new(Duration::from_millis(400));
        assert!(budget
            .guest_step(tokio::time::sleep(Duration::from_millis(50)))
            .await
            .is_ok());
        // A host-mediated wait is not passed through the budget.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            budget
                .guest_step(tokio::time::sleep(Duration::from_millis(50)))
                .await
                .is_ok(),
            "host wait time must not count against the round"
        );
        assert!(
            budget
                .guest_step(tokio::time::sleep(Duration::from_millis(1000)))
                .await
                .is_err(),
            "guest time still counts against the round"
        );
    }

    #[test]
    fn step_timeout_defaults_to_300_and_accepts_string_and_number() {
        let input = |kind| {
            let mut fields = std::collections::BTreeMap::new();
            fields.insert(
                "step_timeout_secs".to_string(),
                prost_types::Value { kind: Some(kind) },
            );
            Struct { fields }
        };
        assert_eq!(resolve_step_timeout_secs(None), 300);
        assert_eq!(
            resolve_step_timeout_secs(Some(&input(prost_types::value::Kind::StringValue(
                "60".into()
            )))),
            60
        );
        assert_eq!(
            resolve_step_timeout_secs(Some(&input(prost_types::value::Kind::NumberValue(45.0)))),
            45
        );
        for bad in ["0", "-3", "soon"] {
            assert_eq!(
                resolve_step_timeout_secs(Some(&input(prost_types::value::Kind::StringValue(
                    bad.into()
                )))),
                300,
                "{bad} is not a usable stepTimeout"
            );
        }
    }

    #[test]
    fn max_iterations_defaults_to_fifty_and_caps_at_two_hundred() {
        let input = |kind| {
            let mut fields = std::collections::BTreeMap::new();
            fields.insert(
                "max_iterations".to_string(),
                prost_types::Value { kind: Some(kind) },
            );
            Struct { fields }
        };
        assert_eq!(resolve_max_iterations(None), 50);
        assert_eq!(
            resolve_max_iterations(Some(&input(prost_types::value::Kind::StringValue(
                "500".into()
            )))),
            200
        );
        assert_eq!(
            resolve_max_iterations(Some(&input(prost_types::value::Kind::StringValue(
                "8".into()
            )))),
            8
        );
    }

    #[test]
    fn workspace_checkpoint_artifact_has_digest() {
        let tmp = std::env::temp_dir().join(format!("vam-ws-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&tmp);
        let artifact = workspace_checkpoint_artifact(&tmp, &"a".repeat(64));
        assert!(artifact.digest.is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    // ---- compaction (AR-2.10) ----

    /// Runs the loop against a scripted model with a context window input and
    /// returns the outcome. The mock broker has no recorder, so only the
    /// guest's side of the compaction is exercised.
    #[cfg(unix)]
    async fn run_window_script(
        script: &str,
        id: &str,
        window: &str,
        prompt: &str,
        thread: Option<(Vec<u8>, String)>,
    ) -> TaskOutcome {
        use vam_spec::v1::tool_intent_response::Disposition;

        let (stored, digest) = match thread {
            Some((bytes, digest)) => (Some(bytes), Some(digest)),
            None => (None, None),
        };
        let host = crate::pep::test_support::shared_host(stored);
        {
            let mut guard = host.lock().expect("host");
            guard.intent_disposition = Disposition::Authorized as i32;
            guard.capability_granted = true;
            guard.advertised_tools = vec!["read_file".to_string()];
        }
        let serving_52 = spawn_checkpoint_host(bind_mock_port(52), host.clone());
        let script: MockScript = serde_json::from_str(script).expect("the script parses");
        let scripts = MockScripts::from_scripts(vec![script]).expect("the script loads");
        let serving_53 = spawn_scripted_inference_broker(bind_mock_port(53), Arc::new(scripts));
        let mut fields = std::collections::BTreeMap::new();
        string_input(
            &mut fields,
            "system_instructions",
            &format!("Work on the task. mock-script:{id}"),
        );
        string_input(&mut fields, "context_window", window);
        if let Some(digest) = digest {
            string_input(&mut fields, "thread_head_digest", &digest);
        }
        let prompt = PromptAction {
            instruction_template: prompt.to_string(),
            inject_context_keys: vec![],
        };
        let outcome = run_react_agent_loop_with_trace(
            &checkpoint_test_lease(),
            &prompt,
            Some(&Struct { fields }),
            None,
        )
        .await;
        serving_52.abort();
        serving_53.abort();
        unbind_mock_port(52);
        unbind_mock_port(53);
        outcome.expect("the run finishes")
    }

    /// A run that crosses 70% of the window after a tool result in the middle
    /// of the task: it compacts there, goes on from the summary and the kept
    /// round, and finishes.
    const MID_TASK_SCRIPT: &str = r#"{"id": "mid-task", "rules": [
        {"when": {"contains": "[context compaction]"},
         "reply": {"text": "Summary: step one is done, read x next."}},
        {"when": {"step": "go", "toolName": "read_file", "anyContains": "Summary: step one is done"},
         "reply": {"text": "Finished after compaction."}},
        {"when": {"step": "go", "toolName": "read_file"},
         "reply": {"text": "No compaction happened."}},
        {"when": {"step": "go", "stepStart": true},
         "reply": {"text": "Reading.", "fillerBytes": 70000,
                   "toolCall": {"name": "read_file", "args": {"path": "x"}}}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_run_that_crosses_the_trigger_after_a_tool_result_compacts_and_finishes() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            // 70% of 20,000 tokens is 14,000; the first reply alone is about 17,500.
            let outcome = run_window_script(
                MID_TASK_SCRIPT,
                "mid-task",
                "20000",
                "do it e2e-step:go",
                None,
            )
            .await;
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            assert_eq!(output_answer(&outcome), "Finished after compaction.");
        });
    }

    #[cfg(unix)]
    #[test]
    fn below_the_trigger_nothing_is_compacted() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let outcome = run_window_script(
                MID_TASK_SCRIPT,
                "mid-task",
                "200000",
                "do it e2e-step:go",
                None,
            )
            .await;
            assert_eq!(output_answer(&outcome), "No compaction happened.");
        });
    }

    /// An earlier task left a long answer; the follow-up compacts before its first call.
    #[cfg(unix)]
    fn long_task_context(run_id: &str, filler: usize) -> (Vec<u8>, String) {
        use vam_spec::thread_context::{
            sha256_hex, ThreadContext, WireMessage, THREAD_CONTEXT_FORMAT, THREAD_CONTEXT_VERSION,
        };
        let msg = |role: &str, content: &str| WireMessage {
            role: role.into(),
            content: content.into(),
            tool_calls: vec![],
            tool_call_id: String::new(),
        };
        let ctx = ThreadContext {
            format: THREAD_CONTEXT_FORMAT.into(),
            version: THREAD_CONTEXT_VERSION,
            run_id: run_id.into(),
            head_seq: 3,
            head_digest: "d".into(),
            system_sha256: Some("0".repeat(64)),
            tools_sha256: None,
            messages: vec![
                msg("user", "e2e-step:first Always name the file bravo.txt"),
                msg("assistant", &"a".repeat(filler)),
            ],
            blocks: vec![],
            suspend: None,
            crash: None,
        };
        let bytes = serde_json::to_vec(&ctx).expect("encode");
        let digest = sha256_hex(&bytes);
        (bytes, digest)
    }

    #[cfg(unix)]
    const START_COMPACTION_SCRIPT: &str = r#"{"id": "start-compaction", "rules": [
        {"when": {"contains": "[context compaction]"},
         "reply": {"text": "Summary: the user set a naming rule."}},
        {"when": {"step": "next", "anyContains": "Summary: the user set a naming rule."},
         "reply": {"text": "Compacted."}},
        {"when": {"step": "next"}, "reply": {"text": "Not compacted."}}]}"#;

    #[cfg(unix)]
    #[test]
    fn a_follow_up_over_the_trigger_compacts_before_its_first_call() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let run = "urn:vam:run:local.vam.dev/acme/run-socket";
            // 70% of 16,000 tokens is 11,200 tokens, 44.8 KB; the earlier answer is 60 KB.
            let outcome = run_window_script(
                START_COMPACTION_SCRIPT,
                "start-compaction",
                "16000",
                "e2e-step:next Write the report",
                Some(long_task_context(run, 60_000)),
            )
            .await;
            assert_eq!(
                outcome.state,
                TaskOutcomeState::Succeeded as i32,
                "{:?}",
                outcome.error
            );
            assert_eq!(output_answer(&outcome), "Compacted.");
        });
    }

    #[cfg(unix)]
    #[test]
    fn a_follow_up_under_the_trigger_is_not_compacted() {
        let _broker = mock_broker_lock();
        run_socket_test(async {
            let run = "urn:vam:run:local.vam.dev/acme/run-socket";
            let outcome = run_window_script(
                START_COMPACTION_SCRIPT,
                "start-compaction",
                "16000",
                "e2e-step:next Write the report",
                Some(long_task_context(run, 8_000)),
            )
            .await;
            assert_eq!(output_answer(&outcome), "Not compacted.");
        });
    }

    #[test]
    fn a_context_that_cannot_be_made_to_fit_fails_with_its_typed_reason() {
        let lease = checkpoint_test_lease();
        for prefix in [
            crate::compaction::CONTEXT_TOO_LARGE,
            crate::compaction::CONTEXT_TOO_LONG,
        ] {
            let outcome = inference_failed_outcome(
                &lease,
                vam_spec::v1::ErrorStatus {
                    code: 8,
                    message: format!("{prefix}: too big"),
                    details: vec![],
                    retryable: false,
                },
            );
            let message = outcome.error.expect("error").message;
            assert!(message.starts_with(prefix), "{message}");
        }
    }

    #[test]
    fn the_context_window_comes_from_the_host_input() {
        let mut fields = std::collections::BTreeMap::new();
        string_input(&mut fields, "context_window", "64000");
        assert_eq!(
            context_window_from_inputs(Some(&Struct { fields })),
            Some(64_000)
        );
        let mut bad = std::collections::BTreeMap::new();
        string_input(&mut bad, "context_window", "lots");
        assert_eq!(
            context_window_from_inputs(Some(&Struct { fields: bad })),
            None
        );
        assert_eq!(context_window_from_inputs(None), None);
    }
}
