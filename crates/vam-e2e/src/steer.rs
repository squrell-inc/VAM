//! The steps that talk to a running task: `send` (steer or queue a message), `stop` (end the
//! task through the run's stop route) and `answer` (answer the pending question).
//!
//! This module is a child of `engine`, so it reads the engine's private state; the shared
//! runner files only register the three steps.

use super::{conn, decide_body, is_terminal, Engine, Fail, Probe, RunRec};
use crate::api::seg;
use crate::scenario::Class;
use serde::Deserialize;
use serde_json::{json, Value};

/// `send: { text, mode: steer|queue, refused, as }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Send {
    pub text: String,
    #[serde(default)]
    pub mode: SendMode,
    /// The step passes only when vamd answers 4xx.
    #[serde(default)]
    pub refused: bool,
    /// Alias of the `org` piece whose identity sends; the run's identity when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SendMode {
    /// Taken in by the running task at its next step.
    #[default]
    Steer,
    /// Held for the next task on the thread; it becomes a run prompt.
    Queue,
}

/// `stop: { refused }`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stop {
    /// The step passes only when vamd answers 4xx.
    #[serde(default)]
    pub refused: bool,
}

/// `answer: { response, refused, as }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub response: serde_yaml::Value,
    /// The step passes only when vamd answers 4xx and the question stays pending.
    #[serde(default)]
    pub refused: bool,
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

/// `decide-together: { option, answer, as }`: decides the pending gate and answers the pending
/// question of the latest run at the same moment (two requests sent in parallel), so two
/// decisions race to resume one parked run.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecideTogether {
    /// The option the pending gate takes.
    pub option: String,
    /// The response that answers the pending question.
    pub answer: serde_yaml::Value,
    /// Alias of the `org` piece whose identity decides; the run's identity when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

fn refusal(r: &crate::api::Reply, what: &str) -> Result<(), Fail> {
    if (400..500).contains(&r.status) {
        Ok(())
    } else {
        Err(Fail::new(
            Class::Product,
            format!("{what} is refused (a 4xx answer)"),
            format!("{} {}", r.status, r.body),
        ))
    }
}

/// The answer to a run post that queued the message (another thread on a busy workspace is
/// queued, not refused) names the run the message will start in `inbox.runId`. The runner
/// follows that run instead of retrying.
pub(super) fn queued_run_id(reply: &Value) -> Option<String> {
    if reply["inbox"]["mode"].as_str()? != "QUEUE" {
        return None;
    }
    reply["inbox"]["runId"]["urn"]
        .as_str()
        .filter(|id| !id.is_empty())
        .map(String::from)
}

impl Engine {
    pub(super) fn send(&mut self, m: &Send, scenario_id: &str) -> Result<(), Fail> {
        let i = self.last_run()?;
        let (alias, identity, thread, space_id, thread_id) = {
            let r = &self.ctx.runs[i];
            (
                r.agent.clone(),
                r.identity.clone(),
                r.thread_ref.clone(),
                r.space_id.clone(),
                r.thread_id.clone(),
            )
        };
        let (_, urn) = self.agent_of(Some(&alias))?;
        let key = match m.as_.as_deref() {
            Some(a) => self.key_of(Some(a))?,
            None => self.run_key(i),
        };
        let slug = self.workspace_slug(&urn, identity.as_deref(), None);
        // A queued message becomes a run prompt, so it carries the mock marker; a steer is
        // plain text for the task that is running.
        let (prompt, mode) = match m.mode {
            SendMode::Steer => (m.text.clone(), "STEER"),
            SendMode::Queue => (format!("{} mock-script:{scenario_id}", m.text), "QUEUE"),
        };
        let mut body = self.run_body(&urn, &slug, &thread, &prompt, None, None);
        body["spec"]["inboxMode"] = json!(mode);
        // The message goes to the run's Space and thread by id.
        body["spec"]["spaceId"] = json!(space_id);
        body["spec"]["threadId"] = json!(thread_id);
        // One post: the busy retry of a new run would hide the answer this step checks.
        let r = self
            .api
            .post_as(key.as_deref(), "/v1/runs", &body)
            .map_err(conn)?;
        if m.refused {
            return refusal(&r, "the message");
        }
        if !r.ok() {
            return Err(Fail::new(
                Class::Product,
                "the message is accepted",
                format!("POST /v1/runs ({mode}) -> {} {}", r.status, r.body),
            ));
        }
        if m.mode == SendMode::Queue {
            // The message starts its own run later: follow it.
            let v = r.json();
            let run_id = [&v["inbox"]["runId"], &v["status"]["runId"]]
                .iter()
                .find_map(|x| x["urn"].as_str().or(x.as_str()))
                .unwrap_or_default()
                .to_string();
            if run_id.is_empty() || run_id == self.ctx.runs[i].run_id {
                return Err(Fail::new(
                    Class::Product,
                    "the queued message names the run it will start",
                    r.body,
                ));
            }
            let workspace_id = self.ctx.runs[i].workspace_id.clone();
            self.ctx.runs.push(RunRec {
                agent: alias,
                identity,
                run_id,
                workspace_id,
                thread_ref: thread,
                space_id,
                thread_id,
                ..Default::default()
            });
        }
        Ok(())
    }

    pub(super) fn stop(&mut self, s: &Stop) -> Result<(), Fail> {
        let i = self.last_run()?;
        self.refresh(i)?;
        let run_id = self.ctx.runs[i].run_id.clone();
        let key = self.run_key(i);
        let r = self
            .api
            .post_as(
                key.as_deref(),
                &format!("/v1/runs/{}/stop", seg(&run_id)),
                &json!({}),
            )
            .map_err(conn)?;
        if s.refused {
            return refusal(&r, "the stop");
        }
        if !r.ok() {
            return Err(Fail::new(
                Class::Product,
                "the run stops",
                format!("POST /v1/runs/{{id}}/stop -> {} {}", r.status, r.body),
            ));
        }
        let budget = self.cfg.step_budget;
        self.wait_for(budget, Some(i), |e| {
            Ok(if is_terminal(&e.ctx.runs[i].state) {
                Probe::Met
            } else {
                Probe::Pending
            })
        })
        .map_err(|e| self.wait_fail(e, "the run ends after the stop", Some(i)))
    }

    pub(super) fn answer(&mut self, a: &Answer) -> Result<(), Fail> {
        let i = self.last_run()?;
        let run_id = self.ctx.runs[i].run_id.clone();
        let key = match a.as_.as_deref() {
            Some(x) => self.key_of(Some(x))?,
            None => self.run_key(i),
        };
        let budget = self.cfg.step_budget;
        let mut found: Option<Value> = None;
        self.wait_for(budget, Some(i), |e| {
            let q = e
                .listed_approvals(&run_id, key.as_deref())?
                .into_iter()
                .rfind(|c| c["kind"].as_str() == Some("APPROVAL_KIND_QUESTION"));
            Ok(match q {
                Some(c) => {
                    found = Some(c);
                    Probe::Met
                }
                None => Probe::Pending,
            })
        })
        .map_err(|e| self.wait_fail(e, "a pending question for the run", Some(i)))?;
        let req_id = found.expect("probe met")["requestId"]["urn"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let response = serde_json::to_value(&a.response).unwrap_or(Value::Null);
        let body = decide_body(&req_id, "HITL_DECISION_SUBMITTED", Some(response), false);
        let r = self
            .api
            .post_as(
                key.as_deref(),
                &format!("/v1/approvals/{}/decide", seg(&req_id)),
                &body,
            )
            .map_err(conn)?;
        if a.refused {
            refusal(&r, "the answer")?;
            let still = self
                .listed_approvals(&run_id, key.as_deref())?
                .iter()
                .any(|c| c["requestId"]["urn"].as_str() == Some(req_id.as_str()));
            return if still {
                Ok(())
            } else {
                Err(Fail::new(
                    Class::Product,
                    "the refused question stays pending",
                    format!("refused with {} but no longer pending", r.status),
                ))
            };
        }
        if !r.ok() {
            return Err(Fail::new(
                Class::Product,
                "the answer is accepted",
                format!("{} {}", r.status, r.body),
            ));
        }
        self.wait_for(budget, Some(i), |e| {
            Ok(if e.ctx.runs[i].state != "RUN_STATE_PAUSED" {
                Probe::Met
            } else {
                Probe::Pending
            })
        })
        .map_err(|e| self.wait_fail(e, "the run resumes after the answer", Some(i)))
    }

    /// `decide-together`: both pending decisions are posted in parallel; each must be accepted and
    /// the run must leave PAUSED.
    pub(super) fn decide_together(&mut self, d: &DecideTogether) -> Result<(), Fail> {
        let i = self.last_run()?;
        let run_id = self.ctx.runs[i].run_id.clone();
        let key = match d.as_.as_deref() {
            Some(x) => self.key_of(Some(x))?,
            None => self.run_key(i),
        };
        let budget = self.cfg.step_budget;
        let mut found: Option<(String, String)> = None;
        self.wait_for(budget, Some(i), |e| {
            let all = e.listed_approvals(&run_id, key.as_deref())?;
            let id_of = |kind: &str| {
                all.iter()
                    .rfind(|c| c["kind"].as_str() == Some(kind))
                    .and_then(|c| c["requestId"]["urn"].as_str().map(String::from))
            };
            Ok(
                match (id_of("APPROVAL_KIND_GATE"), id_of("APPROVAL_KIND_QUESTION")) {
                    (Some(g), Some(q)) => {
                        found = Some((g, q));
                        Probe::Met
                    }
                    _ => Probe::Pending,
                },
            )
        })
        .map_err(|e| {
            self.wait_fail(
                e,
                "a pending gate and a pending question for the run",
                Some(i),
            )
        })?;
        let (gate_id, question_id) = found.expect("probe met");
        let response = serde_json::to_value(&d.answer).unwrap_or(Value::Null);
        let gate_body = decide_body(
            &gate_id,
            "HITL_DECISION_SUBMITTED",
            Some(json!({ "option": d.option })),
            false,
        );
        let question_body = decide_body(
            &question_id,
            "HITL_DECISION_SUBMITTED",
            Some(response),
            false,
        );
        // One client per request: a client is not shared between threads.
        let (base, boot_key, ca) = (
            self.api.base().to_string(),
            self.api.key().to_string(),
            self.api.ca_path().map(std::path::Path::to_path_buf),
        );
        let uniq: String = run_id
            .chars()
            .rev()
            .take(12)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let post = |tag: String, path: String, body: Value| {
            let (base, boot_key, ca, key) =
                (base.clone(), boot_key.clone(), ca.clone(), key.clone());
            move || {
                crate::api::Api::with_ca_path(&base, &boot_key, &tag, ca).post_as(
                    key.as_deref(),
                    &path,
                    &body,
                )
            }
        };
        let gate_call = post(
            format!("together-gate-{uniq}"),
            format!("/v1/approvals/{}/decide", seg(&gate_id)),
            gate_body,
        );
        let question_call = post(
            format!("together-question-{uniq}"),
            format!("/v1/approvals/{}/decide", seg(&question_id)),
            question_body,
        );
        let (gate_reply, question_reply) = std::thread::scope(|scope| {
            let gate = scope.spawn(gate_call);
            let question = scope.spawn(question_call);
            (gate.join(), question.join())
        });
        for (what, reply) in [
            ("the gate decision", gate_reply),
            ("the answer", question_reply),
        ] {
            let reply = reply
                .map_err(|_| Fail::new(Class::Env, what, "the request thread panicked"))?
                .map_err(conn)?;
            if !reply.ok() {
                return Err(Fail::new(
                    Class::Product,
                    format!("{what} is accepted"),
                    format!("{} {}", reply.status, reply.body),
                ));
            }
        }
        self.wait_for(budget, Some(i), |e| {
            Ok(if e.ctx.runs[i].state != "RUN_STATE_PAUSED" {
                Probe::Met
            } else {
                Probe::Pending
            })
        })
        .map_err(|e| self.wait_fail(e, "the run resumes after both decisions", Some(i)))
    }
}
