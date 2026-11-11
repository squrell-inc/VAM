//! The steps that drive the schedule broker: `wait-scheduled-run`, `stop-broker`,
//! `start-broker`, the expect `scheduled-runs`, the `{{schedule-id}}` chat placeholder, and the
//! two-step start of a broker piece (`restartWithGateway`) with its cleanup.
//!
//! This module is a child of `engine`, so it reads the engine's private state; the shared
//! runner files only register the steps. The contract with the broker and the results the
//! scenarios read are in `e2e/README.md` ("Schedules").

use super::triggers::spawn_broker;
use super::{conn, parse_duration, part_text, Engine, Fail};
use crate::api::seg;
use crate::scenario::Class;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

/// The id the schedule broker registers under.
const SCHEDULE_BROKER: &str = "vam.schedule";

/// `wait-scheduled-run: { within, label }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WaitScheduledRun {
    /// How long to wait for the run to start (default `30s`).
    pub within: Option<String>,
    /// The `e2e-step` label of the message the schedule carries, so the mock rules for the run
    /// it starts are matched against a step (the schedule was made by an earlier chat).
    pub label: Option<String>,
}

/// `stop-broker: {}` and `start-broker: {}`
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerStep {}

/// `scheduled-runs: { count, within }`
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScheduledRunsExpect {
    /// Exactly this many scheduled runs started in the scenario, at the end of the window and
    /// never more while it lasted.
    pub count: usize,
    /// The window that is watched (default `5s`).
    pub within: Option<String>,
}

#[derive(Debug, Default)]
pub struct SchedState {
    /// The gateway socket vamd reported, by broker id (to start a stopped broker again).
    gateway: HashMap<String, String>,
}

fn product(expected: impl Into<String>, observed: impl Into<String>) -> Fail {
    Fail::new(Class::Product, expected, observed)
}

fn window(text: Option<&str>, default: Duration) -> Result<Duration, Fail> {
    match text {
        None => Ok(default),
        Some(t) => parse_duration(t).ok_or_else(|| {
            Fail::new(
                Class::Scenario,
                "a duration such as `30s`",
                format!("`{t}`"),
            )
        }),
    }
}

/// Waits until `path` accepts a connection.
fn wait_listening(path: &std::path::Path) -> bool {
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        if UnixStream::connect(path).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

impl Engine {
    // ---- broker lifecycle --------------------------------------------------------------------

    /// The gateway socket vamd reports for a registered broker.
    fn gateway_of(&self, id: &str) -> Result<String, Fail> {
        let r = self
            .api
            .get(&format!("/v1/hosted/brokers/{}", seg(id)))
            .map_err(conn)?;
        let gateway = r.json()["broker"]["gatewaySocket"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        if !r.ok() || gateway.is_empty() {
            return Err(product(
                format!("vamd reports a gatewaySocket for broker `{id}`"),
                format!(
                    "{} {}",
                    r.status,
                    r.body.chars().take(200).collect::<String>()
                ),
            ));
        }
        Ok(gateway)
    }

    /// The `restartWithGateway` half of a broker piece: reads `gatewaySocket` and starts the
    /// newest broker process again with `--gateway-socket <path>`.
    pub(super) fn restart_with_gateway(&mut self) -> Result<(), Fail> {
        let id = self
            .ctx
            .trig
            .brokers
            .last()
            .map(|b| b.id.clone())
            .ok_or_else(|| Fail::new(Class::Harness, "a started broker", "none"))?;
        let gateway = self.gateway_of(&id)?;
        self.ctx.sched.gateway.insert(id.clone(), gateway);
        self.stop_broker_proc(&id)?;
        self.start_broker_proc(&id)
    }

    fn broker_index(&self, id: &str) -> Result<usize, Fail> {
        self.ctx
            .trig
            .brokers
            .iter()
            .position(|b| b.id == id)
            .ok_or_else(|| {
                Fail::new(
                    Class::Scenario,
                    "a `schedule-broker` piece in `given`",
                    "none",
                )
            })
    }

    /// SIGTERM, then SIGKILL after 5 s.
    fn stop_broker_proc(&mut self, id: &str) -> Result<(), Fail> {
        let i = self.broker_index(id)?;
        let b = &mut self.ctx.trig.brokers[i];
        let pid = b.child.id().to_string();
        let _ = std::process::Command::new("kill")
            .args(["-TERM", &pid])
            .status();
        let start = Instant::now();
        loop {
            match b.child.try_wait() {
                Ok(Some(_)) => return Ok(()),
                Ok(None) if start.elapsed() < Duration::from_secs(5) => {
                    std::thread::sleep(Duration::from_millis(50))
                }
                _ => {
                    let _ = b.child.kill();
                    let _ = b.child.wait();
                    return Ok(());
                }
            }
        }
    }

    /// Starts the broker again with the same dir, socket and gateway socket (no re-`PUT`).
    fn start_broker_proc(&mut self, id: &str) -> Result<(), Fail> {
        let i = self.broker_index(id)?;
        let gateway = self.ctx.sched.gateway.get(id).cloned();
        let b = &mut self.ctx.trig.brokers[i];
        let child = spawn_broker(&b.bin, &b.args, gateway.as_deref()).map_err(|e| {
            Fail::new(
                Class::Env,
                "the broker starts again",
                format!("{}: {e}", b.bin),
            )
        })?;
        b.child = child;
        if !wait_listening(&b.socket) {
            return Err(Fail::new(
                Class::Env,
                "the broker listens again",
                format!("{} does not accept after 5s", b.socket.display()),
            ));
        }
        Ok(())
    }

    pub(super) fn stop_broker(&mut self, _x: &BrokerStep) -> Result<(), Fail> {
        self.stop_broker_proc(SCHEDULE_BROKER)
    }

    pub(super) fn start_broker(&mut self, _x: &BrokerStep) -> Result<(), Fail> {
        self.start_broker_proc(SCHEDULE_BROKER)
    }

    /// Cleanup: removes the scenario's brokers from the node (so their tools do not reach later
    /// scenarios), then stops the processes and removes their dirs.
    pub(super) fn stop_brokers(&mut self) {
        let ids: Vec<String> = self.ctx.trig.brokers.iter().map(|b| b.id.clone()).collect();
        for id in ids {
            let _ = self.api.delete(&format!("/v1/hosted/brokers/{}", seg(&id)));
        }
        self.ctx.trig.stop();
    }

    // ---- scheduled runs ----------------------------------------------------------------------

    /// The runs the node lists for the scenario's agents that the schedule broker started.
    fn scheduled_runs(&self) -> Result<Vec<Value>, Fail> {
        // A scheduled run belongs to whoever approved the grant: the identity of the run that
        // created the schedule (the bootstrap key when that run had none).
        let key = self.last_run().ok().and_then(|i| self.run_key(i));
        let urns: Vec<&str> = self
            .ctx
            .runs
            .iter()
            .map(|r| r.agent_urn.as_str())
            .filter(|u| !u.is_empty())
            .collect();
        let r = self
            .api
            .get_as(key.as_deref(), "/v1/runs?pageSize=1000")
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(
                "the runs are listed",
                format!("GET /v1/runs -> {} {}", r.status, r.body),
            ));
        }
        Ok(r.json()["statuses"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|s| {
                s["source"]["kind"].as_str() == Some("trigger")
                    && s["source"]["brokerId"].as_str() == Some(SCHEDULE_BROKER)
                    && s["agentUrn"].as_str().is_some_and(|u| urns.contains(&u))
            })
            .collect())
    }

    pub(super) fn wait_scheduled_run(&mut self, w: &WaitScheduledRun) -> Result<(), Fail> {
        let within = window(w.within.as_deref(), Duration::from_secs(30))?;
        let base = self.ctx.runs[self.last_run()?].clone();
        let start = Instant::now();
        loop {
            let seen: Vec<String> = self.ctx.runs.iter().map(|r| r.run_id.clone()).collect();
            let fresh = self.scheduled_runs()?.into_iter().find(|s| {
                s["runId"]["urn"]
                    .as_str()
                    .is_some_and(|id| !seen.iter().any(|x| x == id))
            });
            if let Some(s) = fresh {
                let text = |k: &str| s[k].as_str().unwrap_or_default().to_string();
                self.ctx.runs.push(crate::engine::RunRec {
                    identity: base.identity.clone(),
                    run_id: s["runId"]["urn"].as_str().unwrap_or_default().to_string(),
                    session_id: text("sessionId"),
                    workspace_id: text("workspaceId"),
                    thread_ref: text("threadRef"),
                    space_id: text("spaceId"),
                    thread_id: text("threadId"),
                    agent_urn: text("agentUrn"),
                    state: String::new(),
                    steps: Vec::new(),
                    error: String::new(),
                    events: Vec::new(),
                    ..base.clone()
                });
                return Ok(());
            }
            if start.elapsed() >= within {
                return Err(product(
                    format!(
                        "a run started by the schedule broker within {}s",
                        within.as_secs()
                    ),
                    "none started",
                ));
            }
            std::thread::sleep(self.cfg.poll);
        }
    }

    pub(super) fn expect_scheduled_runs(&mut self, x: &ScheduledRunsExpect) -> Result<(), Fail> {
        let within = window(x.within.as_deref(), Duration::from_secs(5))?;
        let start = Instant::now();
        loop {
            let n = self.scheduled_runs()?.len();
            if n > x.count {
                return Err(product(
                    format!(
                        "{} run(s) started by the schedule broker, never more",
                        x.count
                    ),
                    format!("{n} started"),
                ));
            }
            if start.elapsed() >= within {
                return if n == x.count {
                    Ok(())
                } else {
                    Err(product(
                        format!("{} run(s) started by the schedule broker", x.count),
                        format!("{n} started within {}s", within.as_secs()),
                    ))
                };
            }
            std::thread::sleep(self.cfg.poll);
        }
    }

    // ---- {{schedule-id}} ---------------------------------------------------------------------

    /// Replaces `{{schedule-id}}` in a chat text with the newest `sch_…` word in a tool result
    /// of the latest run's thread. Text without the placeholder is returned as it is; when no
    /// id is there the placeholder stays, and the step fails where it is used.
    pub(super) fn expand_schedule_id(&self, text: &str) -> String {
        if !text.contains("{{schedule-id}}") {
            return text.to_string();
        }
        let Ok(i) = self.last_run() else {
            return text.to_string();
        };
        let (ws, thread) = (
            self.ctx.runs[i].workspace_id.clone(),
            self.ctx.runs[i].thread_id.clone(),
        );
        let Ok(r) = self.api.get(&format!(
            "/v1/workspaces/{}/threads/{}/messages?view=all&limit=500",
            seg(&ws),
            seg(&thread)
        )) else {
            return text.to_string();
        };
        let body = r.json();
        let id = body["records"]
            .as_array()
            .into_iter()
            .flatten()
            .rev()
            .filter(|r| r["kind"].as_str() == Some("message"))
            .filter(|r| r["message"]["role"].as_str() == Some("tool"))
            .find_map(|r| {
                let mut t = String::new();
                for part in r["message"]["parts"].as_array().into_iter().flatten() {
                    part_text(part, &mut t);
                }
                t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .rfind(|w| w.starts_with("sch_"))
                    .map(str::to_string)
            });
        match id {
            Some(id) => text.replace("{{schedule-id}}", &id),
            None => text.to_string(),
        }
    }
}
