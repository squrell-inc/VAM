//! The steps for notification channels (Slack first) through the fake transport of
//! `vam-notify-broker`: `node-notify`, `link-contact`, `unlink-contact`, `notify-prefs`,
//! `channel-click`, and the expects `channel-messages` and `broker-state`. Also the channel
//! benchmark probes (`channelShownWithin60s`, `channelMedianSecs`, `triggeredDecidedFromChannel`).
//!
//! This module is a child of `engine`, so it reads the engine's private state; the shared runner
//! files only register the steps. The wire shapes and the broker's `fake` subcommands are written
//! down in `e2e/README.md` under "Channels".
//!
//! Contract with the broker (`$VAM_E2E_NOTIFY_BROKER`, the build with the fake transport):
//! - it is started by the `notify-broker` piece with `--state-dir <dir>/state --fake-transport`;
//!   every Slack Web API call it would make is one line `{"method","body"}` in
//!   `<dir>/state/sent.jsonl`, the token removed; a message carries `body.channel` (the user's
//!   Slack id), `body.blocks` and `body.metadata.event_type` (`vam.<kind>`);
//! - `vam-notify-broker fake dm --state-dir <dir>/state --account <A> --text <t>` is a Slack user
//!   writing to the app; `fake click ...` is a user pressing a button (flags in the README). Both
//!   talk to the running broker over `<state-dir>/fake.sock`. `fake click` prints one JSON object
//!   `{"status":"OK|INVALID|NOT_LINKED|NOT_PERMITTED|ALREADY_RESOLVED|...","detail":""}`.

use super::{conn, Engine, Fail, Probe};
use crate::api::seg;
use crate::scenario::Class;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

// ---- step and expect shapes ------------------------------------------------------------------

/// The two secret pieces Slack needs, as vault refs.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SlackRefs {
    /// Alias of the `secret` piece holding the bot token.
    pub bot_token: String,
    /// Alias of the `secret` piece holding the app-level token.
    pub app_token: String,
}

/// `node-notify: { slack: { botToken, appToken }, appUrl }`: `PUT /v1/node/notify`.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeNotify {
    pub slack: SlackRefs,
    pub app_url: Option<String>,
}

/// `link-contact: { channel, account, as, verify }`: `POST /v1/me/contacts`, then (when `verify`)
/// the fake DM of the code from that account.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkContact {
    pub channel: String,
    /// The Slack account, `<team>/<user>`.
    pub account: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
    #[serde(default = "yes")]
    pub verify: bool,
}

fn yes() -> bool {
    true
}

/// `unlink-contact: { channel, as }`: `POST /v1/me/contacts/{id}/unlink` for the user's contact.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnlinkContact {
    pub channel: String,
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

/// `notify-prefs: { quietHours, always, as }`: `PUT /v1/me/notification-prefs`. `quietHours` is
/// `now` (runner shorthand: the next whole UTC minute, `HH:MM` to `HH:MM`) or
/// `{ start, end, tz }` as the API takes it.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NotifyPrefs {
    pub quiet_hours: Option<serde_yaml::Value>,
    /// Kinds that are sent even inside quiet hours.
    #[serde(default)]
    pub always: Vec<String>,
    #[serde(rename = "as")]
    pub as_: Option<String>,
}

/// `channel-click: { account, message, action, handle, via-socket, status }`: a Slack user presses
/// a button of the latest message of `message` kind. `handle` is `forged`, `replay` or
/// `from:<account>` (the handle of that account's message); omitted, the message's own. `via-socket`
/// is the alias of another broker piece whose gateway socket the click goes through. `status` is the
/// answer wanted (`OK` when omitted).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChannelClick {
    pub account: String,
    pub message: String,
    pub action: String,
    pub handle: Option<String>,
    #[serde(rename = "via-socket")]
    pub via_socket: Option<String>,
    pub status: Option<String>,
}

/// `expect: { channel-messages: { to, kind, count, buttons, openInVam, inputs, updated, mentions,
/// absent, within } }`: what the fake transport recorded for one Slack user.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChannelMessagesExpect {
    /// The Slack account (`<team>/<user>` or the user id).
    pub to: String,
    /// Only messages of this kind (`approval.pending`, `gate.opened`, ...).
    pub kind: Option<String>,
    /// Exactly this many messages sent.
    pub count: Option<usize>,
    /// The last message (the last update when `updated`) has this many buttons.
    pub buttons: Option<usize>,
    /// The last message carries (or lacks) an Open in VAM link.
    pub open_in_vam: Option<bool>,
    /// The last message has this many input elements.
    pub inputs: Option<usize>,
    /// `true`: the message was updated at least once.
    pub updated: Option<bool>,
    /// The last update's text names this word (the surface that decided).
    pub mentions: Option<String>,
    /// `true`: no message matches (holds when read; fails at once if one is there).
    pub absent: Option<bool>,
    pub within: Option<String>,
}

/// `expect: { broker-state: { piece, lacks } }`: no file in the broker's state directory holds a
/// secret piece's value or a literal text.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BrokerStateExpect {
    pub piece: String,
    /// Secret piece aliases (their values) or literal texts.
    pub lacks: Vec<String>,
}

// ---- helpers ---------------------------------------------------------------------------------

fn product(want: impl Into<String>, got: impl Into<String>) -> Fail {
    Fail::new(Class::Product, want, got)
}

fn user_of(account: &str) -> &str {
    account.rsplit('/').next().unwrap_or(account)
}

/// The `body.channel` of a recorded call names `to` (the account or its user id).
fn is_to(body: &Value, to: &str) -> bool {
    body["channel"]
        .as_str()
        .is_some_and(|c| c == to || user_of(c) == user_of(to))
}

fn kind_of(body: &Value) -> Option<&str> {
    body["metadata"]["event_type"]
        .as_str()
        .map(|t| t.strip_prefix("vam.").unwrap_or(t))
}

/// Block Kit elements of a message whose `type` is one of `types`.
fn count_elements(v: &Value, types: &[&str]) -> usize {
    match v {
        Value::Object(m) => {
            usize::from(
                m.get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|t| types.contains(&t)),
            ) + m.values().map(|x| count_elements(x, types)).sum::<usize>()
        }
        Value::Array(a) => a.iter().map(|x| count_elements(x, types)).sum(),
        _ => 0,
    }
}

const INPUTS: &[&str] = &[
    "input",
    "plain_text_input",
    "static_select",
    "multi_static_select",
    "datepicker",
    "checkboxes",
    "radio_buttons",
    "file_input",
    "number_input",
];

impl Engine {
    /// The state directory of the notification broker the scenario started.
    fn notify_state_dir(&self, want: &str) -> Result<PathBuf, Fail> {
        self.ctx
            .trig
            .brokers
            .iter()
            .find(|b| b.id == "vam.notify")
            .map(|b| b.dir.join("state"))
            .ok_or_else(|| Fail::new(Class::Scenario, "a `notify-broker` piece in `given`", want))
    }

    fn notify_bin(&self, want: &str) -> Result<String, Fail> {
        std::env::var("VAM_E2E_NOTIFY_BROKER")
            .ok()
            .filter(|b| !b.is_empty())
            .ok_or_else(|| Fail::new(Class::Env, want, "VAM_E2E_NOTIFY_BROKER is not set"))
    }

    /// Every call the fake transport recorded: `(method, body)`.
    fn sent_calls(&self, dir: &std::path::Path) -> Vec<(String, Value)> {
        std::fs::read_to_string(dir.join("sent.jsonl"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .map(|v| {
                (
                    v["method"].as_str().unwrap_or_default().to_string(),
                    v["body"].clone(),
                )
            })
            .collect()
    }

    fn secret_name(&self, alias: &str) -> Result<String, Fail> {
        self.ctx
            .pieces
            .get(alias)
            .filter(|p| p.kind == crate::world::PieceKind::Secret)
            .map(|p| p.name.clone())
            .ok_or_else(|| {
                Fail::new(
                    Class::Scenario,
                    format!("secret piece `{alias}`"),
                    "not a secret piece in `given`",
                )
            })
    }

    // ---- steps -------------------------------------------------------------------------------

    /// One vamd serves every scenario of a run and the owner is the same bootstrap user in each,
    /// so a Slack contact one scenario linked would refuse the next one's `link-contact` ("a slack
    /// contact is already linked"). The first channel step of a scenario starts the owner clean.
    fn unlink_owner_slack_contacts(&mut self) {
        let Ok(r) = self.api.get_as(None, "/v1/me/contacts") else {
            return;
        };
        if !r.ok() {
            return;
        }
        let ids: Vec<String> = r.json()["contacts"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|c| c["channel"].as_str() == Some("slack"))
                    .filter_map(|c| c["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        for id in ids {
            let _ = self.api.post_as(
                None,
                &format!("/v1/me/contacts/{}/unlink", seg(&id)),
                &json!({}),
            );
        }
    }

    pub(super) fn node_notify(&mut self, n: &NodeNotify) -> Result<(), Fail> {
        self.unlink_owner_slack_contacts();
        let bot = self.secret_name(&n.slack.bot_token)?;
        let app = self.secret_name(&n.slack.app_token)?;
        let mut body =
            json!({ "slack": { "enabled": true, "botTokenRef": bot, "appTokenRef": app } });
        if let Some(u) = &n.app_url {
            body["appUrl"] = json!(u);
        }
        let r = self.api.put("/v1/node/notify", &body).map_err(conn)?;
        if r.ok() {
            // vamd opens the notification gateway only for a registered, enabled channel: the
            // broker (the newest one started) is started again with its socket.
            return self.restart_with_gateway();
        }
        Err(product(
            "PUT /v1/node/notify is accepted",
            format!(
                "{} {}",
                r.status,
                self.scrub(&r.body.chars().take(300).collect::<String>())
            ),
        ))
    }

    pub(super) fn link_contact(&mut self, l: &LinkContact) -> Result<(), Fail> {
        let key = self.key_of(l.as_.as_deref())?;
        let want = format!("the {} contact {} is added", l.channel, l.account);
        let dir = self.notify_state_dir(&want)?;
        let r = self
            .api
            .post_as(
                key.as_deref(),
                "/v1/me/contacts",
                &json!({ "channel": l.channel, "address": l.account }),
            )
            .map_err(conn)?;
        if !r.ok() {
            return Err(product(
                want,
                format!(
                    "{} {}",
                    r.status,
                    r.body.chars().take(300).collect::<String>()
                ),
            ));
        }
        // A contact of a second identity would outlive its scenario (the next one's owner could
        // not link the same account), so cleanup unlinks it as the same identity.
        if key.is_some() {
            self.ctx.conn.contact_keys.push(key.clone());
        }
        if !l.verify {
            return Ok(());
        }
        // The code goes user -> bot (design 3.6): `POST /v1/me/contacts` returns it once, and the
        // account writes it to the bot in a DM (`fake dm`). vamd sends no message with the code.
        let what = format!(
            "the code for {} is written to the bot and confirms it",
            l.account
        );
        let bin = self.notify_bin(&what)?;
        let code = r.json()["code"].as_str().unwrap_or_default().to_string();
        if code.is_empty() {
            return Err(product(want, "POST /v1/me/contacts returned no `code`"));
        }
        let out = Command::new(&bin)
            .args(["fake", "dm", "--state-dir"])
            .arg(&dir)
            .args(["--account", &l.account, "--text", &code])
            .output()
            .map_err(|e| Fail::new(Class::Env, &what, format!("{bin}: {e}")))?;
        if !out.status.success() {
            return Err(product(
                what,
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        Ok(())
    }

    pub(super) fn unlink_contact(&mut self, u: &UnlinkContact) -> Result<(), Fail> {
        let key = self.key_of(u.as_.as_deref())?;
        let want = format!("the {} contact is unlinked", u.channel);
        let r = self
            .api
            .get_as(key.as_deref(), "/v1/me/contacts")
            .map_err(conn)?;
        let id = r.json()["contacts"]
            .as_array()
            .and_then(|a| {
                a.iter()
                    .find(|c| c["channel"].as_str() == Some(u.channel.as_str()))
            })
            .and_then(|c| c["id"].as_str().map(String::from));
        let Some(id) = id.filter(|_| r.ok()) else {
            return Err(product(
                want,
                format!(
                    "{} {}",
                    r.status,
                    r.body.chars().take(300).collect::<String>()
                ),
            ));
        };
        let r = self
            .api
            .post_as(
                key.as_deref(),
                &format!("/v1/me/contacts/{}/unlink", seg(&id)),
                &json!({}),
            )
            .map_err(conn)?;
        if r.ok() {
            return Ok(());
        }
        Err(product(
            want,
            format!(
                "{} {}",
                r.status,
                r.body.chars().take(300).collect::<String>()
            ),
        ))
    }

    pub(super) fn notify_prefs(&mut self, p: &NotifyPrefs) -> Result<(), Fail> {
        let key = self.key_of(p.as_.as_deref())?;
        let mut body = json!({ "always": p.always });
        match &p.quiet_hours {
            Some(serde_yaml::Value::String(s)) if s == "now" => {
                // The product takes HH:MM only: hold the next whole UTC minute. Near the end of a
                // minute, wait for the next one, so the window still holds when the approval parks.
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                if secs % 60 > 40 {
                    std::thread::sleep(std::time::Duration::from_secs(60 - secs % 60 + 1));
                }
                let secs = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let m = (secs / 60) % 1440;
                let hhmm = |m: u64| format!("{:02}:{:02}", (m % 1440) / 60, m % 60);
                body["quietHours"] = json!({ "start": hhmm(m), "end": hhmm(m + 1), "tz": "UTC" });
            }
            Some(v) => {
                body["quietHours"] = serde_json::to_value(v).unwrap_or(Value::Null);
            }
            None => {}
        }
        let want = "PUT /v1/me/notification-prefs is accepted";
        let r = self
            .api
            .put_as(key.as_deref(), "/v1/me/notification-prefs", &body)
            .map_err(conn)?;
        if r.ok() {
            return Ok(());
        }
        Err(product(
            want,
            format!(
                "{} {}",
                r.status,
                r.body.chars().take(300).collect::<String>()
            ),
        ))
    }

    pub(super) fn channel_click(&mut self, c: &ChannelClick) -> Result<(), Fail> {
        let want = format!(
            "the click of {} on {} ({}) answers {}",
            c.account,
            c.message,
            c.action,
            c.status.as_deref().unwrap_or("OK")
        );
        let dir = self.notify_state_dir(&want)?;
        let bin = self.notify_bin(&want)?;
        let mut cmd = Command::new(&bin);
        cmd.args(["fake", "click", "--state-dir"]).arg(&dir).args([
            "--account",
            &c.account,
            "--message",
            &c.message,
            "--action",
            &c.action,
        ]);
        if let Some(h) = &c.handle {
            cmd.args(["--handle", h]);
        }
        if let Some(alias) = &c.via_socket {
            let id = self
                .ctx
                .trig
                .brokers
                .iter()
                .map(|b| b.id.clone())
                .find(|id| id == alias)
                .ok_or_else(|| {
                    Fail::new(
                        Class::Scenario,
                        "a started broker whose id `via-socket` names",
                        alias.clone(),
                    )
                })?;
            let r = self
                .api
                .get(&format!("/v1/hosted/brokers/{}", seg(&id)))
                .map_err(conn)?;
            let socket = r.json()["broker"]["gatewaySocket"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if socket.is_empty() {
                return Err(product(
                    want,
                    format!("vamd reports no gatewaySocket for broker `{id}`"),
                ));
            }
            cmd.args(["--gateway-socket", &socket]);
        }
        let out = cmd
            .output()
            .map_err(|e| Fail::new(Class::Env, &want, format!("{bin}: {e}")))?;
        if !out.status.success() {
            return Err(product(
                want,
                String::from_utf8_lossy(&out.stderr).trim().to_string(),
            ));
        }
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let got = serde_json::from_str::<Value>(text.lines().last().unwrap_or_default())
            .ok()
            .and_then(|v| v["status"].as_str().map(String::from))
            .unwrap_or_default();
        if got == c.status.as_deref().unwrap_or("OK") {
            return Ok(());
        }
        Err(product(want, format!("answer: {}", text.trim())))
    }

    // ---- expects -----------------------------------------------------------------------------

    pub(super) fn expect_channel_messages(
        &mut self,
        x: &ChannelMessagesExpect,
    ) -> Result<(), Fail> {
        let what = format!(
            "the fake transport recorded {} message(s){} to {}",
            x.count
                .map(|c| c.to_string())
                .unwrap_or_else(|| if x.absent == Some(true) { "no" } else { "some" }.into()),
            x.kind
                .as_deref()
                .map(|k| format!(" of kind {k}"))
                .unwrap_or_default(),
            x.to,
        );
        let dir = self.notify_state_dir(&what)?;
        let budget = self.budget_of(x.within.as_deref());
        let mut seen = String::new();
        let r = self.wait_for(budget, None, |e| {
            let calls = e.sent_calls(&dir);
            let hit = |m: &str, b: &Value| {
                m.starts_with("chat.")
                    && is_to(b, &x.to)
                    && x.kind.as_deref().is_none_or(|k| kind_of(b) == Some(k))
            };
            let posts: Vec<&Value> = calls
                .iter()
                .filter(|(m, b)| m == "chat.postMessage" && hit(m, b))
                .map(|(_, b)| b)
                .collect();
            let updates: Vec<&Value> = calls
                .iter()
                .filter(|(m, b)| m == "chat.update" && hit(m, b))
                .map(|(_, b)| b)
                .collect();
            seen = format!("{} message(s), {} update(s)", posts.len(), updates.len());
            if x.absent == Some(true) {
                return Ok(if posts.is_empty() {
                    Probe::Met
                } else {
                    Probe::Impossible(seen.to_string())
                });
            }
            match x.count {
                Some(n) if posts.len() != n => return Ok(Probe::Pending),
                None if posts.is_empty() && x.updated != Some(true) => return Ok(Probe::Pending),
                _ => {}
            }
            if x.updated == Some(true) && updates.is_empty() {
                return Ok(Probe::Pending);
            }
            let view = if x.updated == Some(true) {
                updates.last()
            } else {
                posts.last()
            };
            let Some(view) = view else {
                return Ok(Probe::Met);
            };
            let text = view.to_string();
            let mut bad = Vec::new();
            if let Some(n) = x.buttons {
                let got = count_elements(&view["blocks"], &["button"]);
                if got != n {
                    bad.push(format!("buttons {got} (want {n})"));
                }
            }
            if let Some(n) = x.inputs {
                let got = count_elements(&view["blocks"], INPUTS);
                if got != n {
                    bad.push(format!("inputs {got} (want {n})"));
                }
            }
            if let Some(want) = x.open_in_vam {
                if text.contains("Open in VAM") != want {
                    bad.push(format!("Open in VAM link present: {} (want {want})", !want));
                }
            }
            if let Some(w) = &x.mentions {
                if !text.to_lowercase().contains(&w.to_lowercase()) {
                    bad.push(format!("text does not mention `{w}`"));
                }
            }
            if bad.is_empty() {
                Ok(Probe::Met)
            } else {
                seen = format!("{seen}; {}", bad.join("; "));
                Ok(Probe::Pending)
            }
        });
        r.map_err(|w| {
            let mut f = self.wait_fail(w, &what, None);
            f.observed = format!("{}; last read: {seen}", f.observed);
            if f.class == Class::Harness {
                f.class = Class::Product;
            }
            f
        })
    }

    pub(super) fn expect_broker_state(&mut self, x: &BrokerStateExpect) -> Result<(), Fail> {
        let what = format!(
            "the state directory of `{}` holds none of {:?}",
            x.piece, x.lacks
        );
        let dir = self.notify_state_dir(&what)?;
        let needles: Vec<(String, String)> = x
            .lacks
            .iter()
            .map(|l| match self.ctx.secrets.get(l) {
                Some(s) => (l.clone(), s.0.clone()),
                None => (l.clone(), l.clone()),
            })
            .filter(|(_, v)| !v.is_empty())
            .collect();
        let mut stack = vec![dir.clone()];
        let mut files = 0usize;
        while let Some(d) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&d) else {
                continue;
            };
            for ent in rd.flatten() {
                let p = ent.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                files += 1;
                let bytes = std::fs::read(&p).unwrap_or_default();
                let text = String::from_utf8_lossy(&bytes);
                for (name, v) in &needles {
                    if text.contains(v.as_str()) {
                        return Err(product(
                            what,
                            format!(
                                "{} holds `{name}`",
                                p.strip_prefix(&dir).unwrap_or(&p).display()
                            ),
                        ));
                    }
                }
            }
        }
        if files == 0 {
            return Err(product(
                what,
                "the state directory is empty (the broker wrote no delivery log)",
            ));
        }
        Ok(())
    }
}

// ---- probes ----------------------------------------------------------------------------------

#[derive(Default)]
struct Totals {
    /// record id -> (created, first shown) of records a verified contact was sent.
    shown: BTreeMap<String, (i64, i64)>,
    triggered_total: u64,
    triggered_done: u64,
    /// A `fire-trigger` ran since the last `node-notify` (the scenario's run is trigger-started).
    fired: bool,
}

static TOTALS: Mutex<Totals> = Mutex::new(Totals {
    shown: BTreeMap::new(),
    triggered_total: 0,
    triggered_done: 0,
    fired: false,
});

fn num(v: &Value) -> Option<i64> {
    v.as_i64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
        .or_else(|| v.as_f64().map(|f| f as i64))
}

/// Merges the channel lines into the `bench.notify` object of `summary.json`:
/// `channelShownWithin60s {within, total}`, `channelMedianSecs` (record created to first shown;
/// both `"unmeasured"` until a message was seen) and `triggeredDecidedFromChannel {completed, total}`.
pub fn merge_summary(mut v: Value) -> Value {
    let Ok(t) = TOTALS.lock() else { return v };
    let total = t.shown.len() as u64;
    let within = t.shown.values().filter(|(c, s)| s - c <= 60).count() as u64;
    let mut lat: Vec<i64> = t.shown.values().map(|(c, s)| s - c).collect();
    lat.sort_unstable();
    let median = match lat.len() {
        0 => Value::from("unmeasured"),
        n if n % 2 == 1 => Value::from(lat[n / 2]),
        n => Value::from((lat[n / 2 - 1] + lat[n / 2]) as f64 / 2.0),
    };
    let notify = &mut v["notify"];
    if notify.is_object() {
        notify["channelShownWithin60s"] = if total == 0 {
            Value::from("unmeasured")
        } else {
            json!({ "within": within, "total": total })
        };
        notify["channelMedianSecs"] = median;
        notify["triggeredDecidedFromChannel"] = if t.triggered_total == 0 {
            Value::from("unmeasured")
        } else {
            json!({ "completed": t.triggered_done, "total": t.triggered_total })
        };
    }
    v
}

impl Engine {
    /// Called after every step that passed; a probe never fails a scenario. After a
    /// `channel-messages` expect holds, the records of that kind that were shown give the latency
    /// of the channel (a delivery held by quiet hours is the user's choice and is left out); after
    /// a good click in a trigger-started scenario, the run is waited for.
    pub(super) fn channel_observe(&mut self, st: &crate::scenario::Step) {
        use crate::scenario::Step;
        match st {
            Step::NodeNotify(_) => {
                if let Ok(mut t) = TOTALS.lock() {
                    t.fired = false;
                }
            }
            Step::FireTrigger(_) => {
                if let Ok(mut t) = TOTALS.lock() {
                    t.fired = true;
                }
            }
            Step::Expect(e) => {
                let Some(x) = &e.channel_messages else { return };
                if x.absent == Some(true) || x.updated == Some(true) || x.kind.is_none() {
                    return;
                }
                let Ok(Ok(rows)) = self.list_records(None, "all") else {
                    return;
                };
                if let Ok(mut t) = TOTALS.lock() {
                    for r in rows
                        .iter()
                        .filter(|r| r["kind"].as_str() == x.kind.as_deref())
                    {
                        if let (Some(id), Some(c), Some(s)) = (
                            r["id"].as_str(),
                            num(&r["createdAt"]),
                            num(&r["firstShownAt"]),
                        ) {
                            t.shown.insert(id.to_string(), (c, s));
                        }
                    }
                }
            }
            Step::ChannelClick(c) => {
                if c.status.as_deref().is_some_and(|s| s != "OK") {
                    return;
                }
                if !TOTALS.lock().map(|t| t.fired).unwrap_or(false) {
                    return;
                }
                let Ok(i) = self.last_run() else { return };
                let mut done = false;
                for _ in 0..40 {
                    if self.refresh(i).is_err() {
                        break;
                    }
                    if self.ctx.runs[i].state.contains("SUCCEEDED") {
                        done = true;
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                if let Ok(mut t) = TOTALS.lock() {
                    t.triggered_total += 1;
                    t.triggered_done += u64::from(done);
                }
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_parse_in_their_documented_shapes() {
        use crate::scenario::Step;
        let s: Step = serde_yaml::from_str(
            "channel-click: { account: T1/U1, message: approval.pending, action: approve, handle: forged, via-socket: other, status: INVALID }",
        )
        .unwrap();
        assert!(matches!(s, Step::ChannelClick(c) if c.status.as_deref() == Some("INVALID")));
        assert!(serde_yaml::from_str::<Step>(
            "channel-click: { account: a, message: m, action: x, hndl: f }"
        )
        .is_err());
        let s: Step =
            serde_yaml::from_str("notify-prefs: { quietHours: now, always: [question.asked] }")
                .unwrap();
        assert!(matches!(s, Step::NotifyPrefs(p) if p.always.len() == 1));
        let s: Step = serde_yaml::from_str(
            "expect: { channel-messages: { to: T1/U1, kind: gate.opened, count: 1, buttons: 2, openInVam: true, within: 20s } }",
        )
        .unwrap();
        let Step::Expect(e) = s else {
            panic!("an expect")
        };
        assert_eq!(e.channel_messages.unwrap().buttons, Some(2));
    }

    #[test]
    fn elements_are_counted_by_type() {
        let blocks = json!([{"type":"actions","elements":[{"type":"button"},{"type":"button"}]},{"type":"input"}]);
        assert_eq!(count_elements(&blocks, &["button"]), 2);
        assert_eq!(count_elements(&blocks, INPUTS), 1);
        assert!(is_to(&json!({"channel": "U1"}), "T1/U1"));
        assert_eq!(
            kind_of(&json!({"metadata":{"event_type":"vam.gate.opened"}})),
            Some("gate.opened")
        );
    }
}
