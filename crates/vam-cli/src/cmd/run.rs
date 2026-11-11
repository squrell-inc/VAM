use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use vam_spec::v1::{
    AgentSpec, CreateRunRequest, ListAgentsResponse, PlaybookSpec, ResourceId, ResourceRef, RunSpec,
};

use crate::client::{self, new_idempotency_key, Client};
use crate::config;
use crate::output::{self, ExitCode};

/// Fingerprint used to detect playbook step progress deltas while following.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StepFingerprint {
    state: String,
    outcome: String,
    transition_revision: i64,
}

/// Run an installed agent, continue a thread, load a run file, or manage runs.
///
/// Primary path: `vam run <agent> -p "..."`. `<agent>` is a short name or a
/// URN. No run file is required.
/// Playbook: `vam run <agent> --playbook <urn> --input key=value`.
/// Reproducible / CI: `vam run --file <path>`.
/// Continue a thread: `vam run --workspace <sp_id> --thread <th_id> -p "..."`.
/// Dry run: `vam run <agent> --dry-run`. Resolves refs and prints the policy
/// table. Starts nothing.
/// A positional path is not a run file. `vam run continue` is not a command.
#[derive(Parser)]
#[command(args_conflicts_with_subcommands = true)]
pub struct RunArgs {
    /// Installed agent short name or URN.
    #[arg(value_name = "AGENT")]
    agent: Option<String>,

    /// Prompt for this run.
    #[arg(short, long)]
    prompt: Option<String>,

    /// Run spec file or directory. This is the reproducible / CI form.
    #[arg(long)]
    file: Option<String>,

    /// Space id (`sp_…`) to continue. No agent and no run file.
    ///
    /// List Spaces with `vam space ls`. Pair with `--thread` and `-p`.
    #[arg(long)]
    workspace: Option<String>,

    /// Model for this run (`inputs.model`). It must be on the node's allowed models when the
    /// operator set a list. Omitted: the agent's first allowed preference, then the node default.
    #[arg(long)]
    model: Option<String>,

    /// Stream events until the run finishes.
    #[arg(long)]
    follow: bool,

    /// Thread id (`th_…`) in the `--workspace` Space. Any other value is a label for a new
    /// thread, never a lookup. With no value a new thread is started.
    #[arg(long)]
    thread: Option<String>,

    /// Reuse this idempotency key (default: mint a fresh key every start).
    #[arg(long)]
    idempotency_key: Option<String>,

    /// Playbook URN (`urn:vam:playbook:...`) to run with the agent.
    #[arg(long)]
    playbook: Option<String>,

    /// Playbook input `KEY=VALUE`. Repeat for each field.
    /// Checked against the playbook `inputSchema`.
    #[arg(long = "input", value_name = "KEY=VALUE")]
    input: Vec<String>,

    /// Resolve refs, print allowed / denied / HITL tools, and do not start a run.
    #[arg(long)]
    dry_run: bool,

    #[command(subcommand)]
    command: Option<RunCmd>,
}

#[derive(Debug, Subcommand)]
pub enum RunCmd {
    /// Launch an installed agent, or a run file passed with `--file`.
    ///
    /// A positional path is not a run file. Use `vam run --file <path>`.
    Start {
        /// Installed agent short name or URN. A file path belongs on `--file`.
        #[arg(value_name = "AGENT", required_unless_present = "file")]
        path: Option<String>,
        /// Run spec file or directory. Prefer `vam run --file <path>`.
        #[arg(long)]
        file: Option<String>,
        #[arg(short, long)]
        prompt: Option<String>,
        #[arg(long)]
        follow: bool,
        /// Override threadRef for a new conversation on the same workspace.
        #[arg(long)]
        thread: Option<String>,
        /// Reuse this idempotency key (default: mint a fresh key every start).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// List runs, newest first: one page of 50 (a hint names the next); `--all` follows every page.
    Ls {
        #[arg(long)]
        all: bool,
    },
    Get {
        id: String,
    },
    Cancel {
        id: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Send a message to a running task: read at its next step (steer), or run after it (`--queue`).
    Send {
        id: String,
        text: String,
        /// Run the message as the next task instead of steering the live one.
        #[arg(long)]
        queue: bool,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Stop a running task; prints the messages that never reached it.
    Stop {
        id: String,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Answer a question the agent asked (`--json`), or decline it (`--decline`).
    Answer {
        approval: String,
        /// The answer, a JSON object matching the question's schema.
        #[arg(long, conflicts_with = "decline")]
        json: Option<String>,
        /// Decline to answer; the task is told the question was declined.
        #[arg(long)]
        decline: bool,
        /// Reuse this idempotency key (default: mint a fresh key).
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    Events {
        id: String,
        #[arg(long)]
        follow: bool,
        #[arg(long, default_value = "0")]
        after: i64,
    },
    /// Fetch the CAS trajectory replay (event type / tool / disposition only)
    Trajectory {
        id: String,
    },
    /// Compare two run trajectories (two GETs + local diff)
    Compare {
        a: String,
        b: String,
    },
}

pub async fn run(host: Option<String>, token: Option<String>, args: RunArgs) -> Result<()> {
    let ready = if args.command.is_none() {
        Ready::Launch(direct_launch(args)?)
    } else {
        match args.command {
            Some(RunCmd::Start {
                path,
                file,
                prompt,
                follow,
                thread,
                idempotency_key,
            }) => Ready::Launch(launch_from_start(
                path,
                file,
                prompt,
                follow,
                thread,
                idempotency_key,
            )?),
            Some(other) => Ready::Other(other),
            None => return Err(usage(missing_agent_message())),
        }
    };
    let cfg = config::resolve(host, token).await?;
    let c = Client::new(&cfg)?;
    match ready {
        Ready::Launch(launch) => launch_run(&c, launch).await,
        Ready::Other(cmd) => {
            match cmd {
                RunCmd::Start { .. } => {
                    return Err(usage(
                "pass an agent or `vam run --file <path>`. A positional path is not a run file.",
            ));
                }
                RunCmd::Ls { all } => {
                    let body = client::list_paged(&c, "/v1/runs", "statuses", all).await?;
                    client::print_json(&body);
                }
                RunCmd::Get { id } => {
                    let body = c
                        .get(&format!("/v1/runs/{}", client::encode_path_segment(&id)))
                        .await?;
                    client::print_json(&body);
                }
                RunCmd::Cancel {
                    id,
                    idempotency_key,
                } => {
                    let json = "{}";
                    let key = client::resolve_idempotency_key(idempotency_key);
                    let body = c
                        .post_raw_json(
                            &format!("/v1/runs/{}/cancel", client::encode_path_segment(&id)),
                            json,
                            &key,
                        )
                        .await?;
                    client::print_json(&body);
                }
                RunCmd::Send {
                    id,
                    text,
                    queue,
                    idempotency_key,
                } => {
                    let key = client::resolve_idempotency_key(idempotency_key);
                    let json = send_body(&text, queue, &key);
                    let body = c
                        .post_raw_json(
                            &format!("/v1/runs/{}/messages", client::encode_path_segment(&id)),
                            &json,
                            &key,
                        )
                        .await?;
                    client::print_json(&body);
                }
                RunCmd::Stop {
                    id,
                    idempotency_key,
                } => {
                    let key = client::resolve_idempotency_key(idempotency_key);
                    let body = c
                        .post_raw_json(
                            &format!("/v1/runs/{}/stop", client::encode_path_segment(&id)),
                            "{}",
                            &key,
                        )
                        .await?;
                    client::print_json(&body);
                    for line in discarded_lines(&body) {
                        eprintln!("{line}");
                    }
                }
                RunCmd::Answer {
                    approval,
                    json,
                    decline,
                    idempotency_key,
                } => {
                    let key = client::resolve_idempotency_key(idempotency_key);
                    let payload = answer_body(json.as_deref(), decline, &key)?;
                    let body = c
                        .post_raw_json(
                            &format!(
                                "/v1/approvals/{}/decide",
                                client::encode_path_segment(&approval)
                            ),
                            &payload,
                            &key,
                        )
                        .await?;
                    client::print_json(&body);
                }
                RunCmd::Events { id, follow, after } => {
                    if follow {
                        follow_run_events(&c, &id, after).await?;
                    } else {
                        let body = c
                            .get(&format!(
                                "/v1/runs/{}/events?after={}",
                                client::encode_path_segment(&id),
                                after
                            ))
                            .await?;
                        client::print_json(&body);
                        for line in outbox_lines(&body) {
                            eprintln!("{line}");
                        }
                    }
                }
                RunCmd::Trajectory { id } => {
                    let body = fetch_trajectory(&c, &id).await?;
                    client::print_json(&body);
                }
                RunCmd::Compare { a, b } => {
                    let body_a = fetch_trajectory(&c, &a).await?;
                    let body_b = fetch_trajectory(&c, &b).await?;
                    let replay_a: vam_telemetry::TrajectoryReplay =
                        serde_json::from_str(&body_a).context("parse trajectory A")?;
                    let replay_b: vam_telemetry::TrajectoryReplay =
                        serde_json::from_str(&body_b).context("parse trajectory B")?;
                    let diff = vam_telemetry::compare_trajectories(&replay_a, &replay_b);
                    client::print_json(&serde_json::to_string(&diff)?);
                }
            }
            Ok(())
        }
    }
}

async fn fetch_trajectory(c: &Client, id: &str) -> Result<String> {
    c.get(&format!(
        "/v1/runs/{}/trajectory",
        client::encode_path_segment(id)
    ))
    .await
    .with_context(|| format!("GET /v1/runs/{{id}}/trajectory for {id}"))
}

/// The Space's label for the run line; a failed lookup only drops the label.
async fn space_label(c: &Client, space: &str) -> String {
    if space.is_empty() {
        return String::new();
    }
    let path = format!("/v1/spaces/{}", client::encode_path_segment(space));
    match c.get(&path).await {
        Ok(body) => serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["name"].as_str().map(str::to_string))
            .unwrap_or_default(),
        Err(_) => String::new(),
    }
}

async fn start_run(
    c: &Client,
    spec: RunSpec,
    inline_agent: Option<AgentSpec>,
    inline_playbook: Option<PlaybookSpec>,
    follow: bool,
) -> Result<()> {
    if let Some(agent) = &inline_agent {
        let req = vam_spec::v1::PutAgentRequest {
            spec: Some(agent.clone()),
            ..Default::default()
        };
        let _ = c
            .put_json("/v1/agents", "vam.v1.PutAgentRequest", &req, None)
            .await;
    }
    if let Some(pb) = &inline_playbook {
        let req = vam_spec::v1::PutPlaybookRequest {
            spec: Some(pb.clone()),
            ..Default::default()
        };
        let _ = c
            .put_json("/v1/playbooks", "vam.v1.PutPlaybookRequest", &req, None)
            .await;
    }
    let idem = if spec.idempotency_key.is_empty() {
        None
    } else {
        Some(spec.idempotency_key.clone())
    };
    let req = CreateRunRequest {
        spec: Some(spec),
        inline_agent,
        inline_playbook,
        ..Default::default()
    };
    let body = c
        .post_json("/v1/runs", "vam.v1.CreateRunRequest", &req, idem.as_deref())
        .await?;
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
        let space = v
            .get("spaceId")
            .or_else(|| v.get("workspaceId"))
            .and_then(|x| x.as_str())
            .unwrap_or("");
        let thread_id = v.get("threadId").and_then(|x| x.as_str()).unwrap_or("");
        let thread_label = v.get("threadRef").and_then(|x| x.as_str()).unwrap_or("");
        if !space.is_empty() || !thread_id.is_empty() {
            let space_label = space_label(c, space).await;
            eprintln!(
                "{}",
                super::space::place_line(space, &space_label, thread_id, thread_label)
            );
        }
    }
    client::print_json(&body);
    if follow {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
            let run_id = v
                .pointer("/status/runId/urn")
                .or_else(|| v.pointer("/runId/urn"))
                .or_else(|| v.get("runId"))
                .and_then(|x| x.as_str());
            if let Some(run_id) = run_id {
                follow_run_events(c, run_id, 0).await?;
            }
        }
    }
    Ok(())
}

/// Build stderr progress lines for playbook `stepStatuses` that changed since `prior`.
///
/// Uses the same state/outcome strings the status JSON exposes. Returns an empty
/// list when `stepStatuses` is absent or empty (non-playbook runs).
fn step_progress_delta_lines(
    status: &serde_json::Value,
    prior: &mut BTreeMap<String, StepFingerprint>,
) -> Vec<String> {
    let Some(steps) = status
        .pointer("/status/stepStatuses")
        .or_else(|| status.pointer("/stepStatuses"))
        .or_else(|| status.pointer("/status/step_statuses"))
        .or_else(|| status.get("step_statuses"))
        .and_then(|v| v.as_array())
    else {
        return Vec::new();
    };

    let mut lines = Vec::new();
    for step in steps {
        let id = step
            .get("stepId")
            .or_else(|| step.get("step_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if id.is_empty() {
            continue;
        }
        let state = match step.get("state") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Number(n)) => n.to_string(),
            _ => String::new(),
        };
        let outcome = step
            .get("outcome")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let transition_revision = step
            .get("transitionRevision")
            .or_else(|| step.get("transition_revision"))
            .and_then(json_to_i64)
            .unwrap_or(0);
        let fingerprint = StepFingerprint {
            state: state.clone(),
            outcome: outcome.clone(),
            transition_revision,
        };
        let changed = prior.get(id).map(|old| old != &fingerprint).unwrap_or(true);
        if changed {
            let outcome_suffix = if outcome.is_empty() {
                String::new()
            } else {
                format!(" ({outcome})")
            };
            lines.push(format!("[vam] step {id}: {state}{outcome_suffix}"));
            prior.insert(id.to_string(), fingerprint);
        }
    }
    lines
}

async fn poll_run_step(
    c: &Client,
    encoded: &str,
    after: &mut i64,
    last_state: &mut String,
    terminal_answer: &mut Option<String>,
    terminal_error: &mut Option<String>,
    step_priors: &mut BTreeMap<String, StepFingerprint>,
) -> Result<(bool, String)> {
    let mut terminal_from_events = false;

    let body = c
        .get(&format!("/v1/runs/{}/events?after={}", encoded, *after))
        .await?;
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&body) {
        let events = v
            .get("events")
            .and_then(|e| e.as_array())
            .or_else(|| v.as_array());
        if let Some(events) = events {
            for ev in events {
                if let Some(line) = outbox_line(ev) {
                    eprintln!("{line}");
                }
                let seq = ev
                    .get("sequence")
                    .or_else(|| ev.get("sequenceNumber"))
                    .or_else(|| ev.get("sequence_number"))
                    .and_then(json_to_i64)
                    .unwrap_or(0);

                let event_type = ev
                    .get("eventType")
                    .or_else(|| ev.get("event_type"))
                    .or_else(|| ev.get("type"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("");

                // Terminal detection must run before duplicate skip: if the server
                // re-delivers the same sequence (after cursor not advancing server-side),
                // we still need to exit follow.
                let mut event_terminal = is_terminal_event_type(event_type);
                if !event_terminal
                    && (event_type == "vam.v1.run.state" || event_type == "run.state")
                {
                    if let Some(state) = ev
                        .get("payload")
                        .or_else(|| ev.get("data"))
                        .and_then(json_run_state)
                    {
                        event_terminal = is_terminal_run_state(&state);
                    }
                }
                if event_terminal {
                    if let Some(payload) = ev.get("payload").or_else(|| ev.get("data")) {
                        let (ans, err) = extract_payload_text(payload);
                        if terminal_answer.is_none() {
                            *terminal_answer = ans;
                        }
                        if terminal_error.is_none() {
                            *terminal_error = err;
                        }
                    }
                    if seq > 0 {
                        *after = (*after).max(seq);
                    }
                    terminal_from_events = true;
                    break;
                }

                if seq > 0 && seq <= *after {
                    continue;
                }
                println!("{}", serde_json::to_string(ev)?);
                if seq > 0 {
                    *after = (*after).max(seq);
                }
            }
        }
    }

    let mut status_body = String::new();
    let mut terminal_from_status = false;
    if let Ok(status) = c.get(&format!("/v1/runs/{}", encoded)).await {
        status_body = status;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&status_body) {
            for line in step_progress_delta_lines(&v, step_priors) {
                eprintln!("{line}");
            }
            let state_val = v
                .pointer("/status/state")
                .or_else(|| v.pointer("/state"))
                .or_else(|| v.get("state"));
            if let Some(state) = state_val.and_then(json_run_state) {
                if state != *last_state {
                    eprintln!("[vam] run state: {}", state);
                    *last_state = state.clone();
                }
                if is_terminal_run_state(&state) {
                    terminal_from_status = true;
                }
            }
            if terminal_answer.is_none() {
                *terminal_answer = v
                    .pointer("/status/finalOutputs/answer")
                    .or_else(|| v.pointer("/finalOutputs/answer"))
                    .and_then(|a| a.as_str())
                    .map(str::to_string);
            }
            if terminal_error.is_none() {
                *terminal_error = v
                    .pointer("/status/error/message")
                    .or_else(|| v.pointer("/error/message"))
                    .or_else(|| v.pointer("/status/error/error"))
                    .or_else(|| v.pointer("/error/error"))
                    .and_then(|e| e.as_str())
                    .map(str::to_string);
            }
            // If the host persisted final outputs, the run is done even when state
            // parsing fails (e.g. stale vamd enum mapping).
            if !terminal_from_status && terminal_answer.is_some() {
                terminal_from_status = true;
            }
        }
    }

    Ok((terminal_from_events || terminal_from_status, status_body))
}

/// Body of `POST /v1/runs/{id}/messages`.
pub fn send_body(text: &str, queue: bool, key: &str) -> String {
    let mut v = serde_json::json!({ "text": text, "idempotencyKey": key });
    if queue {
        v["mode"] = "QUEUE".into();
    }
    v.to_string()
}

/// Body of `POST /v1/approvals/{id}/decide` for a question: SUBMITTED (4) with
/// the answer, or REJECTED (2) when declined.
pub fn answer_body(json: Option<&str>, decline: bool, key: &str) -> Result<String> {
    let v = match (json, decline) {
        (_, true) => serde_json::json!({
            "decision": 2, "rejectionReason": "declined by user", "idempotencyKey": key
        }),
        (Some(j), false) => {
            let r: serde_json::Value =
                serde_json::from_str(j).map_err(|e| usage(format!("--json is not JSON: {e}")))?;
            if !r.is_object() {
                return Err(usage("--json must be a JSON object".to_string()));
            }
            serde_json::json!({ "decision": 4, "response": r, "idempotencyKey": key })
        }
        (None, false) => return Err(usage("pass --json '{...}' or --decline".to_string())),
    };
    Ok(v.to_string())
}

/// One line per message a stop discarded.
pub fn discarded_lines(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    v.get("discarded")
        .and_then(|d| d.as_array())
        .map(|a| {
            a.iter()
                .map(|m| {
                    let t = m.get("text").and_then(|t| t.as_str()).unwrap_or("");
                    format!("[vam] discarded: {t}")
                })
                .collect()
        })
        .unwrap_or_default()
}

/// A readable line for an `outbox.message` or `outbox.card` event.
pub fn outbox_line(ev: &serde_json::Value) -> Option<String> {
    let ty = ev
        .get("eventType")
        .or_else(|| ev.get("event_type"))
        .or_else(|| ev.get("type"))
        .and_then(|t| t.as_str())?;
    let d = ev.get("payload").or_else(|| ev.get("data"))?;
    match ty {
        "outbox.message" => Some(format!("[vam] agent: {}", d.get("text")?.as_str()?)),
        "outbox.card" => Some(format!(
            "[vam] card {}: {}",
            d.get("jig")?.as_str()?,
            d.get("data").map(|x| x.to_string()).unwrap_or_default()
        )),
        _ => None,
    }
}

fn outbox_lines(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Vec::new();
    };
    let evs = v
        .get("events")
        .and_then(|e| e.as_array())
        .or_else(|| v.as_array());
    evs.map(|a| a.iter().filter_map(outbox_line).collect())
        .unwrap_or_default()
}

pub async fn follow_run_events(c: &Client, run_id: &str, mut after: i64) -> Result<()> {
    let encoded = client::encode_path_segment(run_id);
    let mut last_state = String::new();
    let mut terminal_answer: Option<String> = None;
    let mut terminal_error: Option<String> = None;
    let mut step_priors: BTreeMap<String, StepFingerprint> = BTreeMap::new();
    eprintln!(
        "[vam] following run {} (events are sparse; watch vamd for broker traces)",
        run_id
    );
    loop {
        tokio::select! {
            step_res = poll_run_step(c, &encoded, &mut after, &mut last_state, &mut terminal_answer, &mut terminal_error, &mut step_priors) => {
                let (is_done, status_body) = step_res?;
                if is_done {
                    if !status_body.is_empty() {
                        client::print_json(&status_body);
                    }
                    if let Some(answer) = terminal_answer {
                        eprintln!("[vam] agent answer:\n{}", answer);
                    } else if let Some(error) = terminal_error {
                        eprintln!("[vam] run error:\n{}", error);
                    }
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[vam] Interrupt received. Cancelling run {}... (Ctrl+C again to force exit)", run_id);
                let _ = c.post_raw_json(
                    &format!("/v1/runs/{}/cancel", encoded),
                    "{}",
                    &new_idempotency_key(),
                ).await;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                        eprintln!("[vam] Cancel request sent. Exiting follow.");
                    }
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("[vam] Second interrupt. Exiting immediately.");
                    }
                }
                break;
            }
        }

        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {}
            _ = tokio::signal::ctrl_c() => {
                eprintln!("\n[vam] Interrupt received. Cancelling run {}... (Ctrl+C again to force exit)", run_id);
                let _ = c.post_raw_json(
                    &format!("/v1/runs/{}/cancel", encoded),
                    "{}",
                    &new_idempotency_key(),
                ).await;
                tokio::select! {
                    _ = tokio::time::sleep(std::time::Duration::from_secs(2)) => {
                        eprintln!("[vam] Cancel request sent. Exiting follow.");
                    }
                    _ = tokio::signal::ctrl_c() => {
                        eprintln!("[vam] Second interrupt. Exiting immediately.");
                    }
                }
                break;
            }
        }
    }
    Ok(())
}

fn extract_payload_text(payload_val: &serde_json::Value) -> (Option<String>, Option<String>) {
    let mut answer = None;
    let mut error = None;

    if let Some(payload_str) = payload_val.as_str() {
        if let Ok(result) = serde_json::from_str::<serde_json::Value>(payload_str) {
            answer = result
                .get("answer")
                .and_then(|a| a.as_str())
                .map(str::to_string);
            error = result
                .get("error")
                .or_else(|| result.get("message"))
                .and_then(|e| e.as_str())
                .map(str::to_string);
        } else if !payload_str.is_empty() && !is_terminal_run_state(payload_str) {
            answer = Some(payload_str.to_string());
        }
    } else if let Some(obj) = payload_val.as_object() {
        answer = obj
            .get("answer")
            .and_then(|a| a.as_str())
            .map(str::to_string);
        error = obj
            .get("error")
            .or_else(|| obj.get("message"))
            .and_then(|e| e.as_str())
            .map(str::to_string);
    }

    (answer, error)
}

fn json_to_i64(v: &serde_json::Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_u64().map(|u| u as i64))
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

fn json_run_state(v: &serde_json::Value) -> Option<String> {
    if let Some(s) = v.as_str() {
        if let Ok(n) = s.parse::<i64>() {
            return numeric_run_state(n);
        }
        return Some(s.to_string());
    }
    v.as_i64()
        .or_else(|| v.as_u64().map(|u| u as i64))
        .and_then(numeric_run_state)
}

fn numeric_run_state(n: i64) -> Option<String> {
    Some(match n {
        0 => "RUN_STATE_UNSPECIFIED".into(),
        1 => "RUN_STATE_PENDING".into(),
        2 => "RUN_STATE_RUNNING".into(),
        3 => "RUN_STATE_PAUSED".into(),
        4 => "RUN_STATE_SUCCEEDED".into(),
        5 => "RUN_STATE_FAILED".into(),
        6 => "RUN_STATE_CANCELLED".into(),
        7 => "RUN_STATE_TIMED_OUT".into(),
        8 => "RUN_STATE_EXPIRED".into(),
        9 => "RUN_STATE_REJECTED".into(),
        other => format!("RUN_STATE_{}", other),
    })
}

fn is_terminal_event_type(event_type: &str) -> bool {
    let lower = event_type.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        "vam.v1.run.completed"
            | "vam.v1.run.failed"
            | "vam.v1.run.cancelled"
            | "vam.v1.run.timed_out"
            | "vam.v1.run.expired"
            | "vam.v1.run.rejected"
            | "run.completed"
            | "run.failed"
            | "run.cancelled"
            | "run.timed_out"
            | "run.expired"
            | "run.rejected"
    ) || lower.ends_with(".run.completed")
        || lower.ends_with(".run.failed")
        || lower.ends_with(".run.cancelled")
        || lower.ends_with(".run.timed_out")
        || lower.ends_with(".run.expired")
        || lower.ends_with(".run.rejected")
}

fn is_terminal_run_state(state: &str) -> bool {
    matches!(
        state.to_ascii_uppercase().as_str(),
        "RUN_STATE_SUCCEEDED"
            | "RUN_STATE_FAILED"
            | "RUN_STATE_CANCELLED"
            | "RUN_STATE_TIMED_OUT"
            | "RUN_STATE_EXPIRED"
            | "RUN_STATE_REJECTED"
            | "SUCCEEDED"
            | "FAILED"
            | "CANCELLED"
            | "TIMED_OUT"
            | "EXPIRED"
            | "REJECTED"
    )
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod tests {
    use super::{
        classify_run_target, direct_launch, extract_payload_text, is_terminal_event_type,
        is_terminal_run_state, json_run_state, json_to_i64, launch_from_start,
        resolve_installed_agent, spec_for_agent, spec_for_continue, step_progress_delta_lines,
        ClassifiedTarget, InstalledAgent, LaunchSource, RunArgs, StepFingerprint,
    };
    use crate::output::{self, ExitCode};
    use clap::{CommandFactory, Parser};
    use serde_json::json;
    use std::collections::BTreeMap;

    const SP: &str = "sp_k3q7xw2m5b4n6a7c3d2e3f4g5h";
    const TH: &str = "th_k3q7xw2m5b4n6a7c3d2e3f4g5h";

    #[test]
    fn sequence_string_from_proto_json_parses() {
        assert_eq!(json_to_i64(&json!("64")), Some(64));
        assert_eq!(json_to_i64(&json!(64)), Some(64));
    }

    #[test]
    fn step_progress_prints_deltas_and_skips_unchanged() {
        let mut prior = BTreeMap::new();
        let first = json!({
            "status": {
                "state": "RUN_STATE_RUNNING",
                "stepStatuses": [
                    {
                        "stepId": "query",
                        "state": "STEP_STATE_AVAILABLE",
                        "outcome": "",
                        "transitionRevision": "1"
                    }
                ]
            }
        });
        let lines = step_progress_delta_lines(&first, &mut prior);
        assert_eq!(
            lines,
            vec!["[vam] step query: STEP_STATE_AVAILABLE".to_string()]
        );
        assert!(step_progress_delta_lines(&first, &mut prior).is_empty());

        let next = json!({
            "status": {
                "state": "RUN_STATE_RUNNING",
                "stepStatuses": [
                    {
                        "stepId": "query",
                        "state": "STEP_STATE_SUCCESS",
                        "outcome": "done",
                        "transitionRevision": "2"
                    },
                    {
                        "stepId": "report",
                        "state": "STEP_STATE_AVAILABLE",
                        "outcome": "",
                        "transitionRevision": "1"
                    }
                ]
            }
        });
        let lines = step_progress_delta_lines(&next, &mut prior);
        assert_eq!(
            lines,
            vec![
                "[vam] step query: STEP_STATE_SUCCESS (done)".to_string(),
                "[vam] step report: STEP_STATE_AVAILABLE".to_string(),
            ]
        );
        assert_eq!(
            prior.get("query"),
            Some(&StepFingerprint {
                state: "STEP_STATE_SUCCESS".into(),
                outcome: "done".into(),
                transition_revision: 2,
            })
        );
    }

    #[test]
    fn step_progress_silent_without_step_statuses() {
        let mut prior = BTreeMap::new();
        let status = json!({"status": {"state": "RUN_STATE_RUNNING"}});
        assert!(step_progress_delta_lines(&status, &mut prior).is_empty());
        let empty = json!({"status": {"state": "RUN_STATE_RUNNING", "stepStatuses": []}});
        assert!(step_progress_delta_lines(&empty, &mut prior).is_empty());
    }

    #[test]
    fn run_state_accepts_string_or_number() {
        assert_eq!(
            json_run_state(&json!("RUN_STATE_SUCCEEDED")).as_deref(),
            Some("RUN_STATE_SUCCEEDED")
        );
        assert_eq!(
            json_run_state(&json!(4)).as_deref(),
            Some("RUN_STATE_SUCCEEDED")
        );
        assert_eq!(
            json_run_state(&json!("5")).as_deref(),
            Some("RUN_STATE_FAILED")
        );
        assert!(is_terminal_run_state("RUN_STATE_SUCCEEDED"));
        assert!(is_terminal_run_state("FAILED"));
        assert!(is_terminal_run_state("CANCELLED"));
        assert!(is_terminal_run_state("TIMED_OUT"));
        assert!(is_terminal_run_state("RUN_STATE_CANCELLED"));
        assert!(!is_terminal_run_state("RUN_STATE_RUNNING"));
        assert!(!is_terminal_run_state("RUN_STATE_PENDING"));
    }

    #[test]
    fn terminal_event_types_recognized() {
        assert!(is_terminal_event_type("vam.v1.run.completed"));
        assert!(is_terminal_event_type("vam.v1.run.failed"));
        assert!(is_terminal_event_type("vam.v1.run.cancelled"));
        assert!(is_terminal_event_type("run.completed"));
        assert!(is_terminal_event_type("custom.run.completed"));
        assert!(!is_terminal_event_type("vam.v1.run.created"));
        assert!(!is_terminal_event_type("vam.v1.run.state"));
        assert!(!is_terminal_event_type("vam.v1.hsp.hook.completed"));
        assert!(!is_terminal_event_type("vam.v1.host_tool.completed"));
        assert!(!is_terminal_event_type("vam.v1.hsp.hook.failed"));
    }

    #[test]
    fn extract_payload_handles_string_and_object_json() {
        let (ans, err) = extract_payload_text(&json!(r#"{"answer":"42"}"#));
        assert_eq!(ans.as_deref(), Some("42"));
        assert_eq!(err, None);

        let (ans2, err2) = extract_payload_text(&json!({"answer": "hello"}));
        assert_eq!(ans2.as_deref(), Some("hello"));
        assert_eq!(err2, None);

        let (ans3, err3) = extract_payload_text(&json!(r#"{"error":"boom"}"#));
        assert_eq!(ans3, None);
        assert_eq!(err3.as_deref(), Some("boom"));
    }

    #[test]
    fn duplicate_events_are_skipped_by_sequence() {
        let mut after = 10_i64;
        for seq in [10_i64, 11, 11, 12] {
            if seq <= after {
                continue;
            }
            after = after.max(seq);
        }
        assert_eq!(after, 12);
    }

    #[test]
    fn terminal_event_detected_before_duplicate_skip() {
        let ev = json!({
            "sequence": "92",
            "eventType": "vam.v1.run.completed",
            "payload": "{\"answer\":\"42\"}"
        });
        let seq = ev.get("sequence").and_then(json_to_i64).unwrap_or(0);
        let after: i64 = 92;
        let event_type = ev.get("eventType").and_then(|t| t.as_str()).unwrap_or("");
        assert!(is_terminal_event_type(event_type));
        assert!(seq > 0 && seq <= after);
    }

    #[test]
    fn cancel_accepts_idempotency_key() {
        let args =
            RunArgs::try_parse_from(["vam", "cancel", "run-1", "--idempotency-key", "cancel-key"])
                .unwrap();
        match args.command {
            Some(super::RunCmd::Cancel {
                id,
                idempotency_key,
            }) => {
                assert_eq!(id, "run-1");
                assert_eq!(idempotency_key.as_deref(), Some("cancel-key"));
            }
            other => panic!("expected cancel, got {other:?}"),
        }
    }

    #[test]
    fn load_run_inputs_from_dir_finds_run_json() {
        let dir = tempfile::tempdir().unwrap();
        let run_json = dir.path().join("run.json");
        std::fs::write(
            &run_json,
            r#"{"spec":{"workspaceRefUri":"volume://test-ws","inputs":{"prompt":"default prompt"}}}"#,
        )
        .unwrap();
        let (spec, _, _) =
            super::load_run_inputs(dir.path().to_str().unwrap(), Some("override prompt")).unwrap();
        assert_eq!(spec.workspace_ref_uri, "volume://test-ws");
        let prompt_field = spec
            .inputs
            .unwrap()
            .fields
            .get("prompt")
            .unwrap()
            .kind
            .clone();
        assert_eq!(
            prompt_field,
            Some(prost_types::value::Kind::StringValue(
                "override prompt".into()
            ))
        );
    }

    #[test]
    fn load_run_inputs_from_dir_without_run_json_defaults_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let (spec, _, _) =
            super::load_run_inputs(dir.path().to_str().unwrap(), Some("hello")).unwrap();
        assert!(spec.workspace_ref_uri.starts_with("volume://"));
    }

    #[test]
    fn primary_path_parses_short_name_and_prompt() {
        let args =
            RunArgs::try_parse_from(["vam", "jira-assistant", "-p", "summarize open issues"])
                .unwrap();
        assert_eq!(args.agent.as_deref(), Some("jira-assistant"));
        assert_eq!(args.prompt.as_deref(), Some("summarize open issues"));
        assert!(args.file.is_none());
        assert!(args.command.is_none());
    }

    #[test]
    fn urn_and_file_flag_parse() {
        let urn = "urn:vam:agent:examples.local/vam/jira-assistant";
        let args = RunArgs::try_parse_from(["vam", urn, "--prompt", "hello"]).unwrap();
        assert_eq!(args.agent.as_deref(), Some(urn));
        let file = RunArgs::try_parse_from(["vam", "--file", "run.json"]).unwrap();
        assert_eq!(file.file.as_deref(), Some("run.json"));
        assert!(file.agent.is_none());
    }

    #[test]
    fn start_file_flag_parses_and_positional_path_is_rejected() {
        let listed = RunArgs::try_parse_from(["vam", "ls"]).unwrap();
        assert!(matches!(
            listed.command,
            Some(super::RunCmd::Ls { all: false })
        ));
        let all = RunArgs::try_parse_from(["vam", "ls", "--all"]).unwrap();
        assert!(matches!(all.command, Some(super::RunCmd::Ls { all: true })));
        let by_file = RunArgs::try_parse_from(["vam", "start", "--file", "run.json"]).unwrap();
        match by_file.command {
            Some(super::RunCmd::Start { path, file, .. }) => {
                assert!(path.is_none());
                assert_eq!(file.as_deref(), Some("run.json"));
            }
            other => panic!("expected start --file, got {other:?}"),
        }
        let launch =
            launch_from_start(None, Some("run.json".into()), None, true, None, None).unwrap();
        assert!(matches!(launch.source, LaunchSource::File(path) if path == "run.json"));
        assert!(launch.follow);
        let positional = RunArgs::try_parse_from([
            "vam",
            "start",
            "examples/jira-assistant/run.json",
            "--follow",
        ])
        .unwrap();
        let err = match positional.command {
            Some(super::RunCmd::Start {
                path,
                file,
                prompt,
                follow,
                thread,
                idempotency_key,
            }) => {
                launch_from_start(path, file, prompt, follow, thread, idempotency_key).unwrap_err()
            }
            other => panic!("expected start, got {other:?}"),
        };
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
        let msg = err.to_string();
        assert!(msg.contains("vam run --file"), "{msg}");
    }

    #[test]
    fn short_name_is_not_a_path_and_urn_stays_a_urn() {
        assert!(matches!(
            classify_run_target("jira-assistant").unwrap(),
            ClassifiedTarget::Short(name) if name == "jira-assistant"
        ));
        let urn = "urn:vam:agent:examples.local/vam/jira-assistant";
        assert!(matches!(
            classify_run_target(urn).unwrap(),
            ClassifiedTarget::Urn(got) if got == urn
        ));
        for path in [
            "examples/jira-assistant/run.json",
            "run.json",
            "./examples/bun-assistant",
        ] {
            let err = classify_run_target(path).unwrap_err();
            assert_eq!(output::classify(&err).0, ExitCode::Usage);
            assert!(err.to_string().contains("--file"), "{path}");
        }
    }

    #[test]
    fn file_flag_is_the_run_file_and_a_positional_path_is_not() {
        let file = RunArgs::try_parse_from([
            "vam",
            "--file",
            "examples/jira-assistant/run.json",
            "--follow",
        ])
        .unwrap();
        let launch = direct_launch(file).unwrap();
        assert!(matches!(
            launch.source,
            LaunchSource::File(path) if path == "examples/jira-assistant/run.json"
        ));
        let positional =
            RunArgs::try_parse_from(["vam", "examples/jira-assistant/run.json"]).unwrap();
        let err = direct_launch(positional).unwrap_err();
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
        assert!(err
            .to_string()
            .contains("vam run --file examples/jira-assistant/run.json"));
    }

    #[test]
    fn workspace_thread_prompt_continues_without_a_continue_verb() {
        let args = RunArgs::try_parse_from([
            "vam",
            "--workspace",
            SP,
            "--thread",
            "feature-login",
            "-p",
            "add a badge",
            "--model",
            "openai/gpt-4o-mini",
        ])
        .unwrap();
        assert!(args.command.is_none());
        let launch = direct_launch(args).unwrap();
        match launch.source {
            LaunchSource::Continue { workspace, model } => {
                assert_eq!(workspace, SP);
                assert_eq!(model.as_deref(), Some("openai/gpt-4o-mini"));
            }
            _ => panic!("expected continue launch"),
        }
        assert_eq!(launch.thread.as_deref(), Some("feature-login"));
        assert_eq!(launch.prompt.as_deref(), Some("add a badge"));

        let default_thread =
            RunArgs::try_parse_from(["vam", "--workspace", SP, "-p", "next"]).unwrap();
        let launch = direct_launch(default_thread).unwrap();
        // No thread given: a new thread (no default name).
        assert_eq!(launch.thread, None);

        let spec = spec_for_continue(
            SP,
            "feature-login",
            "add a badge",
            Some("openai/gpt-4o-mini"),
            None,
        )
        .unwrap();
        assert!(spec.agent_ref.is_none());
        assert_eq!(spec.space_id, SP);
        // A non-id thread is a label for a new thread, never a lookup.
        assert_eq!(spec.thread_ref, "feature-login");
        assert!(spec.thread_id.is_empty());
        let by_id = spec_for_continue(SP, TH, "next", None, None).unwrap();
        assert_eq!(
            (by_id.thread_id.as_str(), by_id.thread_ref.as_str()),
            (TH, "")
        );
        // A label is not a Space.
        let err = spec_for_continue("assistant", TH, "next", None, None).unwrap_err();
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
        let fields = spec.inputs.unwrap().fields;
        assert_eq!(
            fields.get("prompt").and_then(|v| v.kind.clone()),
            Some(prost_types::value::Kind::StringValue("add a badge".into()))
        );
        assert!(fields.contains_key("model"));
    }

    #[test]
    fn continue_verb_and_incomplete_thread_are_usage() {
        let continued = RunArgs::try_parse_from([
            "vam",
            "continue",
            "--workspace",
            SP,
            "--thread",
            "main",
            "-p",
            "next",
        ])
        .unwrap();
        let err = direct_launch(continued).unwrap_err();
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
        let msg = err.to_string();
        assert!(msg.contains("not a command"), "{msg}");
        assert!(msg.contains("--workspace"), "{msg}");
        assert!(msg.contains("-p"), "{msg}");

        let bare = RunArgs::try_parse_from(["vam", "continue"]).unwrap();
        let err = direct_launch(bare).unwrap_err();
        assert!(err.to_string().contains("not a command"));

        let no_prompt =
            RunArgs::try_parse_from(["vam", "--workspace", SP, "--thread", "main"]).unwrap();
        let err = direct_launch(no_prompt).unwrap_err();
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
        assert!(err.to_string().contains("-p"));

        let both =
            RunArgs::try_parse_from(["vam", "jira-assistant", "--file", "run.json", "-p", "hi"])
                .unwrap();
        let err = direct_launch(both).unwrap_err();
        assert_eq!(output::classify(&err).0, ExitCode::Usage);

        // `--model` on a first launch is accepted and carried to the run.
        let model_first =
            RunArgs::try_parse_from(["vam", "jira-assistant", "--model", " m ", "-p", "hi"])
                .unwrap();
        let launch = direct_launch(model_first).unwrap();
        assert_eq!(launch.model.as_deref(), Some("m"));
        let blank =
            RunArgs::try_parse_from(["vam", "jira-assistant", "--model", " ", "-p", "hi"]).unwrap();
        assert_eq!(
            output::classify(&direct_launch(blank).unwrap_err()).0,
            ExitCode::Usage
        );

        let help = RunArgs::command().render_long_help().to_string();
        assert!(help.contains("--file"));
        assert!(help.contains("--workspace"));
        assert!(
            !help
                .lines()
                .any(|line| line.trim_start().starts_with("continue ")),
            "continue must not be a subcommand:\n{help}"
        );
    }

    #[test]
    fn short_name_resolves_to_one_installed_urn() {
        let agents = vec![
            InstalledAgent {
                urn: "urn:vam:agent:examples.local/vam/jira-assistant".into(),
            },
            InstalledAgent {
                urn: "urn:vam:agent:examples.local/vam/bun-assistant".into(),
            },
        ];
        let urn = resolve_installed_agent("jira-assistant", &agents).unwrap();
        assert_eq!(urn, "urn:vam:agent:examples.local/vam/jira-assistant");
        let err = resolve_installed_agent("missing", &agents).unwrap_err();
        assert_eq!(output::classify(&err).0, ExitCode::NotFound);
        assert!(err.to_string().contains("vam agent ls"));
    }

    #[test]
    fn ambiguous_short_name_lists_urns() {
        let agents = vec![
            InstalledAgent {
                urn: "urn:vam:agent:examples.local/vam/jira-assistant".into(),
            },
            InstalledAgent {
                urn: "urn:vam:agent:local.vam.dev/acme/jira-assistant".into(),
            },
        ];
        let err = resolve_installed_agent("jira-assistant", &agents).unwrap_err();
        assert_eq!(output::classify(&err).0, ExitCode::Usage);
        let msg = err.to_string();
        assert!(msg.contains("urn:vam:agent:examples.local/vam/jira-assistant"));
        assert!(msg.contains("urn:vam:agent:local.vam.dev/acme/jira-assistant"));
    }

    #[test]
    fn dry_run_and_playbook_inputs_parse() {
        let args = RunArgs::try_parse_from([
            "vam",
            "jira-assistant",
            "--playbook",
            "urn:vam:playbook:examples.local/vam/jira-sprint-backlog-cleanup",
            "--input",
            "project_key=PROJ",
            "--input",
            "cutoff_days=14",
            "--dry-run",
        ])
        .unwrap();
        let launch = direct_launch(args).unwrap();
        assert!(launch.dry_run);
        assert_eq!(
            launch.playbook.as_deref(),
            Some("urn:vam:playbook:examples.local/vam/jira-sprint-backlog-cleanup")
        );
        assert_eq!(
            launch.inputs,
            vec![
                ("project_key".into(), "PROJ".into()),
                ("cutoff_days".into(), "14".into())
            ]
        );
        match launch.source {
            LaunchSource::Agent(ClassifiedTarget::Short(name)) => {
                assert_eq!(name, "jira-assistant");
            }
            _ => panic!("expected agent launch"),
        }
    }

    #[test]
    fn playbook_flag_rejects_file_workspace_and_bad_urn() {
        let file = RunArgs::try_parse_from([
            "vam",
            "--file",
            "run.json",
            "--playbook",
            "urn:vam:playbook:examples.local/vam/cleanup",
        ])
        .unwrap();
        let err = direct_launch(file).unwrap_err().to_string();
        assert!(err.contains("--playbook"));

        let workspace =
            RunArgs::try_parse_from(["vam", "--workspace", SP, "--dry-run", "-p", "next"]).unwrap();
        let err = direct_launch(workspace).unwrap_err().to_string();
        assert!(err.contains("--dry-run"));

        let bad =
            RunArgs::try_parse_from(["vam", "jira-assistant", "--playbook", "cleanup"]).unwrap();
        let err = direct_launch(bad).unwrap_err().to_string();
        assert!(err.contains("urn:vam:playbook:"));

        let bare_input =
            RunArgs::try_parse_from(["vam", "jira-assistant", "--input", "k=v"]).unwrap();
        let err = direct_launch(bare_input).unwrap_err().to_string();
        assert!(err.contains("--input"));
    }

    #[test]
    fn catalog_spec_has_prompt_and_no_version_pin() {
        let urn = "urn:vam:agent:examples.local/vam/jira-assistant";
        let spec = spec_for_agent(urn, Some("summarize the sprint"), None, None).unwrap();
        let agent_ref = spec.agent_ref.unwrap();
        assert_eq!(agent_ref.resource_id.unwrap().urn, urn);
        assert!(agent_ref.version.is_none());
        assert!(agent_ref.digest.is_none());
        assert_eq!(spec.workspace_ref_uri, "volume://jira-assistant");
        let prompt = spec.inputs.unwrap().fields.get("prompt").unwrap().clone();
        assert_eq!(
            prompt.kind,
            Some(prost_types::value::Kind::StringValue(
                "summarize the sprint".into()
            ))
        );
    }
}

enum Ready {
    Launch(Launch),
    Other(RunCmd),
}

#[derive(Debug)]
struct Launch {
    source: LaunchSource,
    prompt: Option<String>,
    follow: bool,
    thread: Option<String>,
    idempotency_key: Option<String>,
    playbook: Option<String>,
    inputs: Vec<(String, String)>,
    dry_run: bool,
    /// `--model` on a first launch (agent or run file); a thread continue carries its own.
    model: Option<String>,
}

#[derive(Debug)]
enum LaunchSource {
    /// Explicit `--file`. Always read from disk, even when the name is bare.
    File(String),
    /// Installed agent short name or URN. Never a filesystem path.
    Agent(ClassifiedTarget),
    /// Continue `volume://<workspace>` on a thread. No agent ref and no run file.
    Continue {
        workspace: String,
        model: Option<String>,
    },
}

#[derive(Debug)]
enum ClassifiedTarget {
    Urn(String),
    Short(String),
}

struct LaunchInput {
    target: Option<String>,
    file: Option<String>,
    workspace: Option<String>,
    model: Option<String>,
    prompt: Option<String>,
    follow: bool,
    thread: Option<String>,
    idempotency_key: Option<String>,
    playbook: Option<String>,
    inputs: Vec<(String, String)>,
    dry_run: bool,
    /// True for `vam run` with no subcommand.
    direct: bool,
}

struct InstalledAgent {
    urn: String,
}

fn direct_launch(args: RunArgs) -> Result<Launch> {
    let RunArgs {
        agent,
        prompt,
        file,
        workspace,
        model,
        follow,
        thread,
        idempotency_key,
        playbook,
        input,
        dry_run,
        command: _,
    } = args;
    let inputs = parse_input_pairs(&input)?;
    launch_from_input(LaunchInput {
        target: agent,
        file,
        workspace,
        model,
        prompt,
        follow,
        thread,
        idempotency_key,
        playbook,
        inputs,
        dry_run,
        direct: true,
    })
}

fn launch_from_start(
    path: Option<String>,
    file: Option<String>,
    prompt: Option<String>,
    follow: bool,
    thread: Option<String>,
    idempotency_key: Option<String>,
) -> Result<Launch> {
    launch_from_input(LaunchInput {
        target: path,
        file,
        workspace: None,
        model: None,
        prompt,
        follow,
        thread,
        idempotency_key,
        playbook: None,
        inputs: Vec::new(),
        dry_run: false,
        direct: false,
    })
}

fn launch_from_input(input: LaunchInput) -> Result<Launch> {
    let LaunchInput {
        target,
        file,
        workspace,
        model,
        prompt,
        follow,
        thread,
        idempotency_key,
        playbook,
        inputs,
        dry_run,
        direct,
    } = input;

    if dry_run && follow {
        return Err(usage(
            "--dry-run does not start a run, so --follow does not apply. Drop --follow.",
        ));
    }
    let playbook = match playbook {
        Some(urn) => {
            let urn = urn.trim().to_string();
            if !urn.starts_with("urn:vam:playbook:") {
                return Err(usage(format!(
                    "'{urn}' is not a playbook URN. Playbook URNs start with urn:vam:playbook:. Example: vam run <agent> --playbook urn:vam:playbook:... --input key=value"
                )));
            }
            Some(urn)
        }
        None => None,
    };
    if !inputs.is_empty() && playbook.is_none() {
        return Err(usage(
            "--input belongs to a playbook run. Example: vam run <agent> --playbook <urn> --input key=value",
        ));
    }
    if playbook.is_some() && (file.is_some() || workspace.is_some()) {
        return Err(usage(
            "--playbook runs an installed agent. Example: vam run <agent> --playbook <urn> --input key=value",
        ));
    }
    if dry_run && workspace.is_some() {
        return Err(usage(
            "--dry-run needs an agent. Example: vam run <agent> --dry-run",
        ));
    }

    if let Some(token) = target.as_deref() {
        reject_continue_verb(token)?;
        reject_run_file_positional(token)?;
    }
    let model = model.map(|m| m.trim().to_string());
    if model.as_deref() == Some("") {
        return Err(usage(
            "--model needs a model id. Example: vam run <agent> --model <model> -p \"...\"",
        ));
    }
    let workspace = match workspace {
        Some(name) => {
            let name = name.trim().to_string();
            if name.is_empty() {
                return Err(usage(
                    "missing Space id. Continue with: vam run --workspace <sp_id> --thread <th_id> -p \"...\"",
                ));
            }
            Some(name)
        }
        None => None,
    };
    let chosen = usize::from(target.is_some())
        + usize::from(file.is_some())
        + usize::from(workspace.is_some());
    if chosen > 1 {
        return Err(usage(
            "pass one way to run. Agent: vam run <agent> -p \"...\". File: vam run --file <path>. Thread: vam run --workspace <sp_id> --thread <th_id> -p \"...\"",
        ));
    }
    if let Some(workspace) = workspace {
        let prompt = prompt.filter(|p| !p.trim().is_empty()).ok_or_else(|| {
            usage(
                "missing prompt. Continue a thread with: vam run --workspace <sp_id> --thread <th_id> -p \"...\"",
            )
        })?;
        super::space::require_space_id(&workspace)?;
        let thread = thread.filter(|t| !t.trim().is_empty());
        return Ok(Launch {
            source: LaunchSource::Continue { workspace, model },
            prompt: Some(prompt),
            follow,
            thread,
            idempotency_key,
            playbook: None,
            inputs,
            dry_run,
            model: None,
        });
    }
    if let Some(path) = file {
        return Ok(Launch {
            source: LaunchSource::File(path),
            prompt,
            follow,
            thread,
            idempotency_key,
            playbook: None,
            inputs,
            dry_run,
            model,
        });
    }
    if playbook.is_some() && target.is_none() {
        return Err(usage(
            "--playbook needs an agent. Example: vam run <agent> --playbook <urn> --input key=value",
        ));
    }
    if let Some(target) = target {
        let classified = classify_run_target(&target)?;
        return Ok(Launch {
            source: LaunchSource::Agent(classified),
            prompt,
            follow,
            thread,
            idempotency_key,
            playbook,
            inputs,
            dry_run,
            model,
        });
    }
    if direct {
        return Err(usage(missing_agent_message()));
    }
    Err(usage("pass a run file with: vam run --file <path>"))
}

fn reject_continue_verb(token: &str) -> Result<()> {
    if token.trim() == "continue" {
        return Err(usage(
            "`vam run continue` is not a command. Continue a thread with: vam run --workspace <sp_id> --thread <th_id> -p \"...\"",
        ));
    }
    Ok(())
}

fn reject_run_file_positional(token: &str) -> Result<()> {
    let token = token.trim();
    if token.starts_with("urn:") {
        return Ok(());
    }
    if looks_like_run_path(token) {
        return Err(usage(format!(
            "'{token}' is a run file path. Pass it behind --file: vam run --file {token}"
        )));
    }
    Ok(())
}

fn missing_agent_message() -> String {
    "missing agent. Run an installed agent with: vam run <agent> -p \"...\"\nExample: vam run jira-assistant -p \"summarize open issues\"\nA URN also works: vam run urn:vam:agent:... -p \"...\"\nA reproducible run file: vam run --file <path>\nContinue a thread: vam run --workspace <sp_id> --thread <th_id> -p \"...\""
        .to_string()
}

async fn launch_run(c: &Client, launch: Launch) -> Result<()> {
    let Launch {
        source,
        prompt,
        follow,
        thread,
        idempotency_key,
        playbook,
        inputs,
        dry_run,
        model,
    } = launch;
    match source {
        LaunchSource::File(path) => {
            if dry_run {
                return dry_run_file(c, &path).await;
            }
            start_from_path(
                c,
                &path,
                prompt.as_deref(),
                follow,
                thread,
                idempotency_key,
                model.as_deref(),
            )
            .await
        }
        LaunchSource::Agent(target) => {
            let urn = match target {
                ClassifiedTarget::Urn(urn) => urn,
                ClassifiedTarget::Short(name) => resolve_short_name(c, &name).await?,
            };
            let mut spec = spec_for_agent(&urn, prompt.as_deref(), thread, idempotency_key)?;
            let mut loaded_agent = None;
            let mut loaded_playbook = None;
            if let Some(playbook_urn) = playbook {
                let playbook_spec = super::dry_run::fetch_playbook(c, &playbook_urn).await?;
                let agent = super::dry_run::fetch_agent(c, &urn).await?;
                super::dry_run::ensure_playbook_allowed(&agent, &playbook_urn)?;
                let mut pairs = inputs;
                if let Some(prompt) = prompt.as_deref() {
                    if !pairs.iter().any(|(key, _)| key == "prompt") {
                        pairs.push(("prompt".to_string(), prompt.to_string()));
                    }
                }
                let parsed =
                    vam_spec::parse_playbook_inputs(playbook_spec.input_schema.as_ref(), &pairs)
                        .map_err(|err| output::fail(ExitCode::Invalid, err.to_string()))?;
                spec.playbook_ref = Some(ResourceRef {
                    resource_id: Some(ResourceId { urn: playbook_urn }),
                    ..Default::default()
                });
                spec.inputs = Some(parsed);
                loaded_agent = Some(agent);
                loaded_playbook = Some(playbook_spec);
            }
            if dry_run {
                let agent = match loaded_agent {
                    Some(agent) => agent,
                    None => super::dry_run::fetch_agent(c, &urn).await?,
                };
                let report = super::dry_run::evaluate(c, agent, loaded_playbook).await?;
                super::dry_run::emit_report(&report);
                return Ok(());
            }
            put_model_input(&mut spec, model.as_deref());
            start_run(c, spec, None, None, follow).await
        }
        LaunchSource::Continue { workspace, model } => {
            let spec = spec_for_continue(
                &workspace,
                thread.as_deref().unwrap_or(""),
                prompt.as_deref().unwrap_or(""),
                model.as_deref(),
                idempotency_key,
            )?;
            start_run(c, spec, None, None, follow).await
        }
    }
}

async fn dry_run_file(c: &Client, path: &str) -> Result<()> {
    let (spec, inline_agent, inline_playbook) = load_run_inputs(path, None)?;
    let agent = if let Some(agent) = inline_agent {
        agent
    } else if let Some(urn) = spec
        .agent_ref
        .as_ref()
        .and_then(|resource_ref| resource_ref.resource_id.as_ref())
        .map(|id| id.urn.clone())
        .filter(|urn| !urn.is_empty())
    {
        super::dry_run::fetch_agent(c, &urn).await?
    } else {
        return Err(usage(
            "dry-run needs an agent. The run file has no agent spec and no agentRef. Example: vam run <agent> --dry-run",
        ));
    };
    let playbook = if inline_playbook.is_some() {
        inline_playbook
    } else if let Some(urn) = spec
        .playbook_ref
        .as_ref()
        .and_then(|resource_ref| resource_ref.resource_id.as_ref())
        .map(|id| id.urn.clone())
        .filter(|urn| !urn.is_empty())
    {
        Some(super::dry_run::fetch_playbook(c, &urn).await?)
    } else {
        None
    };
    let report = super::dry_run::evaluate(c, agent, playbook).await?;
    super::dry_run::emit_report(&report);
    Ok(())
}

async fn start_from_path(
    c: &Client,
    path: &str,
    prompt: Option<&str>,
    follow: bool,
    thread: Option<String>,
    idempotency_key: Option<String>,
    model: Option<&str>,
) -> Result<()> {
    let (mut spec, inline_agent, inline_playbook) = load_run_inputs(path, prompt)?;
    if let Some(t) = label_only(thread)? {
        spec.thread_ref = t;
    }
    put_model_input(&mut spec, model);
    spec.idempotency_key = idempotency_key.unwrap_or_else(new_idempotency_key);
    start_run(c, spec, inline_agent, inline_playbook, follow).await
}

fn classify_run_target(token: &str) -> Result<ClassifiedTarget> {
    let token = token.trim();
    if token.is_empty() {
        return Err(usage(missing_agent_message()));
    }
    if token.starts_with("urn:") {
        if token.starts_with("urn:vam:agent:") {
            return Ok(ClassifiedTarget::Urn(token.to_string()));
        }
        return Err(usage(format!(
            "'{token}' is not an agent URN. Agent URNs start with urn:vam:agent:. Pass a short name or that URN: vam run <agent> -p \"...\""
        )));
    }
    if looks_like_run_path(token) {
        return Err(usage(format!(
            "'{token}' is a run file path. Pass it behind --file: vam run --file {token}"
        )));
    }
    Ok(ClassifiedTarget::Short(token.to_string()))
}

fn looks_like_run_path(token: &str) -> bool {
    token == "."
        || token == ".."
        || token.starts_with("./")
        || token.starts_with("../")
        || token.starts_with("~/")
        || token.contains('/')
        || token.contains('\\')
        || token.ends_with(".json")
}

/// Without a Space a `th_…` id cannot be resolved: it is refused rather than used as a label.
fn label_only(thread: Option<String>) -> Result<Option<String>> {
    match thread {
        Some(t) if super::space::is_thread_id(t.trim()) => Err(usage(
            "a thread id needs its Space. Continue it with: vam run --workspace <sp_id> --thread <th_id> -p \"...\"",
        )),
        other => Ok(other),
    }
}

fn agent_slug(urn: &str) -> &str {
    urn.rsplit('/')
        .find(|segment| !segment.is_empty())
        .unwrap_or(urn)
}

/// Build a run that names a catalog agent and leaves version unset.
/// The host pins the latest released version. Do not hardcode 1.0.0.
fn spec_for_agent(
    urn: &str,
    prompt: Option<&str>,
    thread: Option<String>,
    idempotency_key: Option<String>,
) -> Result<RunSpec> {
    let thread = label_only(thread)?;
    let slug = agent_slug(urn);
    if slug.is_empty() || slug.contains("..") {
        return Err(usage(format!(
            "agent URN '{urn}' has no usable short name. Pass urn:vam:agent:<registry>/<namespace>/<name>"
        )));
    }
    let mut spec = RunSpec {
        agent_ref: Some(ResourceRef {
            resource_id: Some(ResourceId {
                urn: urn.to_string(),
            }),
            ..Default::default()
        }),
        workspace_ref_uri: format!("volume://{slug}"),
        idempotency_key: idempotency_key.unwrap_or_else(new_idempotency_key),
        thread_ref: thread.unwrap_or_default(),
        ..Default::default()
    };
    if let Some(prompt) = prompt {
        merge_prompt_into_spec(&mut spec, prompt, true);
    }
    Ok(spec)
}

fn spec_for_continue(
    workspace: &str,
    thread: &str,
    prompt: &str,
    model: Option<&str>,
    idempotency_key: Option<String>,
) -> Result<RunSpec> {
    let workspace = workspace.trim();
    super::space::require_space_id(workspace)?;
    let thread = thread.trim();
    let prompt = prompt.trim();
    if prompt.is_empty() {
        return Err(usage(
            "missing prompt. Continue a thread with: vam run --workspace <sp_id> --thread <th_id> -p \"...\"",
        ));
    }
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("prompt".to_string(), string_value(prompt));
    if let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) {
        fields.insert("model".to_string(), string_value(model));
    }
    // A `th_…` id continues that thread; anything else is the label of a new thread.
    let (thread_id, thread_ref) = if super::space::is_thread_id(thread) {
        (thread.to_string(), String::new())
    } else {
        (String::new(), thread.to_string())
    };
    Ok(RunSpec {
        workspace_ref_uri: format!("volume://{workspace}"),
        space_id: workspace.to_string(),
        thread_id,
        thread_ref,
        idempotency_key: idempotency_key.unwrap_or_else(new_idempotency_key),
        inputs: Some(prost_types::Struct { fields }),
        ..Default::default()
    })
}

/// Sets `--model` as the run's `inputs.model`, over any model the run file names.
fn put_model_input(spec: &mut RunSpec, model: Option<&str>) {
    let Some(model) = model.map(str::trim).filter(|m| !m.is_empty()) else {
        return;
    };
    let mut inputs = spec.inputs.take().unwrap_or_default();
    inputs
        .fields
        .insert("model".to_string(), string_value(model));
    spec.inputs = Some(inputs);
}

fn string_value(text: &str) -> prost_types::Value {
    prost_types::Value {
        kind: Some(prost_types::value::Kind::StringValue(text.to_string())),
    }
}

async fn resolve_short_name(c: &Client, name: &str) -> Result<String> {
    let body = c.get("/v1/agents").await?;
    let listed: ListAgentsResponse = client::parse_json("vam.v1.ListAgentsResponse", &body)?;
    resolve_installed_agent(name, &installed_agents(&listed))
}

fn installed_agents(listed: &ListAgentsResponse) -> Vec<InstalledAgent> {
    let specs = if !listed.specs.is_empty() {
        listed.specs.clone()
    } else {
        listed
            .records
            .iter()
            .filter_map(|record| record.spec.clone())
            .collect::<Vec<_>>()
    };
    let mut agents = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
    for spec in specs {
        let urn = spec
            .resource_id
            .map(|id| id.urn)
            .unwrap_or_default()
            .trim()
            .to_string();
        if urn.is_empty() || !seen.insert(urn.clone()) {
            continue;
        }
        agents.push(InstalledAgent { urn });
    }
    agents
}

fn resolve_installed_agent(name: &str, agents: &[InstalledAgent]) -> Result<String> {
    let mut matches: Vec<&str> = agents
        .iter()
        .filter(|agent| agent.urn == name || agent_slug(&agent.urn) == name)
        .map(|agent| agent.urn.as_str())
        .collect();
    matches.sort_unstable();
    matches.dedup();
    match matches.as_slice() {
        [] => Err(not_found(format!(
            "no installed agent matches '{name}'. Run `vam agent ls` to see installed agents, or publish one with `vam agent put <agent.json>`. Pass a URN if you already know it: vam run urn:vam:agent:... -p \"...\""
        ))),
        [urn] => Ok((*urn).to_string()),
        many => {
            let listed = many.join("\n  ");
            Err(usage(format!(
                "'{name}' matches more than one installed agent:\n  {listed}\nPass the URN: vam run <urn> -p \"...\""
            )))
        }
    }
}

fn parse_input_pairs(raw: &[String]) -> Result<Vec<(String, String)>> {
    let mut pairs = Vec::with_capacity(raw.len());
    let mut seen = std::collections::BTreeSet::new();
    for item in raw {
        let Some((key, value)) = item.split_once('=') else {
            return Err(usage(format!(
                "--input '{item}' must be KEY=VALUE. Example: --input project_key=PROJ"
            )));
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(usage(
                "--input needs a name before '='. Example: --input project_key=PROJ",
            ));
        }
        if !seen.insert(key.to_string()) {
            return Err(usage(format!("--input '{key}' is set more than once")));
        }
        pairs.push((key.to_string(), value.to_string()));
    }
    Ok(pairs)
}

fn usage(message: impl Into<String>) -> anyhow::Error {
    output::fail(ExitCode::Usage, message)
}

fn not_found(message: impl Into<String>) -> anyhow::Error {
    output::fail(ExitCode::NotFound, message)
}

fn load_run_inputs(
    path: &str,
    prompt: Option<&str>,
) -> Result<(RunSpec, Option<AgentSpec>, Option<PlaybookSpec>)> {
    let p = Path::new(path);
    if p.is_dir() {
        let run_path = p.join("run.json");
        if run_path.exists() {
            if let Some(s) = run_path.to_str() {
                return load_run_inputs(s, prompt);
            }
        }

        let mut spec = RunSpec {
            idempotency_key: new_idempotency_key(),
            ..Default::default()
        };
        let dir_name = p
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("assistant");
        spec.workspace_ref_uri = format!("volume://{}", dir_name);
        let mut inline_agent = None;
        let mut inline_playbook = None;

        let agent_path = p.join("agent.json");
        if agent_path.exists() {
            let raw = fs::read_to_string(&agent_path)?;
            inline_agent = Some(
                vam_spec::encoding::json_to_proto("vam.v1.AgentSpec", &raw)
                    .map_err(|e| anyhow::anyhow!(e))?,
            );
        }
        let pb_path = p.join("playbook.json");
        if pb_path.exists() {
            let raw = fs::read_to_string(&pb_path)?;
            inline_playbook = Some(
                vam_spec::encoding::json_to_proto("vam.v1.PlaybookSpec", &raw)
                    .map_err(|e| anyhow::anyhow!(e))?,
            );
        }
        if let Some(p) = prompt {
            merge_prompt_into_spec(&mut spec, p, true);
        }
        return Ok((spec, inline_agent, inline_playbook));
    }

    let raw = fs::read_to_string(p).with_context(|| format!("read run spec {:?}", p))?;
    if raw.contains("\"spec\"") {
        let mut req: CreateRunRequest =
            vam_spec::encoding::json_to_proto("vam.v1.CreateRunRequest", &raw)
                .map_err(|e| anyhow::anyhow!(e))?;
        if req.inline_agent.is_none() {
            if let Some(parent) = p.parent() {
                let agent_path = parent.join("agent.json");
                if agent_path.exists() {
                    let agent_raw = fs::read_to_string(&agent_path)?;
                    req.inline_agent = Some(
                        vam_spec::encoding::json_to_proto("vam.v1.AgentSpec", &agent_raw)
                            .map_err(|e| anyhow::anyhow!(e))?,
                    );
                }
            }
        }
        if let Some(p) = prompt {
            if let Some(spec) = req.spec.as_mut() {
                merge_prompt_into_spec(spec, p, false);
            }
        }
        return Ok((
            req.spec.unwrap_or_default(),
            req.inline_agent,
            req.inline_playbook,
        ));
    }
    let mut spec: RunSpec = vam_spec::encoding::json_to_proto("vam.v1.RunSpec", &raw)
        .map_err(|e| anyhow::anyhow!(e))?;
    if let Some(p) = prompt {
        merge_prompt_into_spec(&mut spec, p, false);
    }
    Ok((spec, None, None))
}

fn merge_prompt_into_spec(spec: &mut RunSpec, prompt: &str, replace_inputs: bool) {
    if replace_inputs || spec.inputs.is_none() {
        spec.inputs = Some(prost_types::Struct {
            fields: [(
                "prompt".to_string(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(prompt.to_string())),
                },
            )]
            .into_iter()
            .collect(),
        });
        return;
    }
    let inputs = spec.inputs.get_or_insert_with(prost_types::Struct::default);
    inputs.fields.insert(
        "prompt".to_string(),
        prost_types::Value {
            kind: Some(prost_types::value::Kind::StringValue(prompt.to_string())),
        },
    );
}
