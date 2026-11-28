//! The `fix-request` step: asks the node to tell an admin about a denial, using the
//! `decisionRef` of a denial the latest run's events carry.
//!
//! This module is a child of `engine`, so it reads the engine's private state; the shared
//! runner files only register the step.
//!
//! Wire shapes the step reads and sends (docs: orchestrator/denials):
//! - a `policy.decided` or `egress.decided` event whose payload holds `denial` (or `data.denial`)
//!   with a `decisionRef` (also read as `decision_ref`);
//! - `POST /v1/fix-requests` takes `{ "decisionRef": "<ref>" }`.

use super::{conn, field_problems, Engine, Fail};
use crate::events::bare;
use crate::scenario::Class;
use serde::Deserialize;
use serde_json::{json, Value};

/// Where the step takes the denial from.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DenialSource {
    /// The newest `policy.decided` or `egress.decided` event of the latest run that carries a
    /// denial with a `decisionRef`.
    #[default]
    LatestDenial,
}

/// One status or a list of statuses the node may answer.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum Statuses {
    One(u16),
    Any(Vec<u16>),
}

impl Statuses {
    fn allows(&self, got: u16) -> bool {
        match self {
            Statuses::One(s) => *s == got,
            Statuses::Any(l) => l.contains(&got),
        }
    }
    fn shown(&self) -> String {
        match self {
            Statuses::One(s) => s.to_string(),
            Statuses::Any(l) => l
                .iter()
                .map(u16::to_string)
                .collect::<Vec<_>>()
                .join(" or "),
        }
    }
}

/// `fix-request: { from: latest-denial, as, tool, status, contains, fields, ref, remember,
/// sameMessageAs }`: `POST /v1/fix-requests` with the latest run's latest denial's
/// `decisionRef`, as `as` (the bootstrap key when omitted), judged by `status` (one number or a
/// list). `ref` sends that literal reference instead (a reference no denial has). `remember:
/// name` keeps the answer's `message`; `sameMessageAs: name` needs the answer's `message` to be
/// the one kept under that name.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct FixRequest {
    #[serde(default)]
    pub from: DenialSource,
    /// A literal `decisionRef` to send instead of one read from the run's events.
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    /// Keep the answer's `message` under this name.
    pub remember: Option<String>,
    /// The answer's `message` equals the one kept under this name.
    pub same_message_as: Option<String>,
    /// Alias of the `org` piece that sends the request; the bootstrap key when omitted.
    #[serde(rename = "as")]
    pub as_: Option<String>,
    /// Only a denial whose event names this tool.
    pub tool: Option<String>,
    /// The status the node must answer (one number, or a list of allowed ones).
    pub status: Statuses,
    /// A substring the answer body must hold.
    pub contains: Option<String>,
    /// Dotted camelCase paths over the JSON answer, with the usual matchers.
    pub fields: Option<std::collections::BTreeMap<String, serde_yaml::Value>>,
}

/// The `decisionRef` of the newest denial in `events` (`policy.decided` and `egress.decided`),
/// when `tool` is given only of an event naming that tool.
pub fn latest_decision_ref(events: &[Value], tool: Option<&str>) -> Option<String> {
    events.iter().rev().find_map(|ev| {
        let kind = bare(ev["eventType"].as_str().unwrap_or_default());
        if kind != "policy.decided" && kind != "egress.decided" {
            return None;
        }
        let payload = match &ev["payload"] {
            Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
            other => other.clone(),
        };
        if let Some(t) = tool {
            let named = ["tool_name", "toolName", "tool"]
                .iter()
                .any(|k| payload[*k].as_str() == Some(t));
            if !named {
                return None;
            }
        }
        let denial = [&payload["denial"], &payload["data"]["denial"]]
            .into_iter()
            .find(|d| d.is_object())?;
        ["decisionRef", "decision_ref"]
            .iter()
            .find_map(|k| denial[*k].as_str())
            .filter(|r| !r.is_empty())
            .map(String::from)
    })
}

impl Engine {
    pub(super) fn fix_request(&mut self, x: &FixRequest) -> Result<(), Fail> {
        let bearer = match &x.as_ {
            Some(a) => self
                .key_of(Some(a))?
                .unwrap_or_else(|| self.api.key().to_string()),
            None => self.api.key().to_string(),
        };
        let reference = match &x.reference {
            Some(r) => r.clone(),
            None => self.latest_denial_ref(x.tool.as_deref())?,
        };
        let want = format!(
            "POST /v1/fix-requests as {} answers {}",
            x.as_.as_deref().unwrap_or("the bootstrap key"),
            x.status.shown()
        );
        let r = self
            .api
            .call(
                reqwest::Method::POST,
                "/v1/fix-requests",
                Some(&json!({ "decisionRef": reference })),
                Some(&bearer),
                true,
            )
            .map_err(conn)?;
        let seen = format!("{} {}", r.status, r.body).replace(&bearer, "<key>");
        if !x.status.allows(r.status) {
            return Err(Fail::new(Class::Product, want, seen));
        }
        if let Some(sub) = &x.contains {
            if !r.body.contains(sub.as_str()) {
                return Err(Fail::new(
                    Class::Product,
                    format!("{want} with `{sub}`"),
                    seen,
                ));
            }
        }
        if let Some(fields) = &x.fields {
            let bad = field_problems(&r.json(), fields);
            if !bad.is_empty() {
                return Err(Fail::new(
                    Class::Product,
                    format!("{want} with the expected fields"),
                    bad.join("; "),
                ));
            }
        }
        let message = r.json()["message"].as_str().map(String::from);
        if let Some(name) = &x.remember {
            self.ctx
                .cli_vars
                .insert(name.clone(), message.clone().unwrap_or_default());
        }
        if let Some(name) = &x.same_message_as {
            let kept = self.ctx.cli_vars.get(name).cloned();
            if kept.is_none() {
                return Err(Fail::new(
                    Class::Scenario,
                    format!("an earlier `fix-request` that remembered `{name}`"),
                    "none yet",
                ));
            }
            if message != kept {
                return Err(Fail::new(
                    Class::Product,
                    format!("{want} with the message `{}`", kept.unwrap_or_default()),
                    seen,
                ));
            }
        }
        Ok(())
    }

    /// The `decisionRef` of the latest run's newest denial, waited for until the step budget.
    fn latest_denial_ref(&mut self, tool: Option<&str>) -> Result<String, Fail> {
        let i = self.last_run()?;
        let tool = tool.map(String::from);
        let what = format!(
            "the latest run has a denial with a decisionRef{}",
            tool.as_ref()
                .map(|t| format!(" for tool {t}"))
                .unwrap_or_default()
        );
        let budget = self.cfg.step_budget;
        let mut found: Option<String> = None;
        let r = self.wait_for(budget, Some(i), |en| {
            found = latest_decision_ref(&en.ctx.runs[i].events, tool.as_deref());
            Ok(if found.is_some() {
                super::Probe::Met
            } else {
                super::Probe::Pending
            })
        });
        if let Err(w) = r {
            let mut f = self.wait_fail(w, &what, Some(i));
            // The run is fine; the host did not write the denial the request needs.
            if f.class == Class::Harness {
                f.class = Class::Product;
            }
            return Err(f);
        }
        Ok(found.unwrap_or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: &str, payload: Value) -> Value {
        json!({ "eventType": format!("vam.v1.{kind}"), "payload": payload })
    }

    #[test]
    fn step_parses_and_rejects_unknown_keys() {
        let x: FixRequest =
            serde_yaml::from_str("{ from: latest-denial, as: bob, status: 404 }").unwrap();
        assert_eq!(
            (x.from, x.as_.as_deref()),
            (DenialSource::LatestDenial, Some("bob"))
        );
        assert!(x.status.allows(404) && !x.status.allows(200));
        let x: FixRequest = serde_yaml::from_str(
            "{ ref: dnl_none, status: [200, 201], remember: gone, sameMessageAs: gone }",
        )
        .unwrap();
        assert!(x.status.allows(201) && !x.status.allows(404));
        assert_eq!(x.status.shown(), "200 or 201");
        assert_eq!(x.reference.as_deref(), Some("dnl_none"));
        assert_eq!(x.same_message_as.as_deref(), Some("gone"));
        assert!(serde_yaml::from_str::<FixRequest>("{ status: 200 }").is_ok());
        assert!(serde_yaml::from_str::<FixRequest>("{ status: 200, nope: 1 }").is_err());
        assert!(serde_yaml::from_str::<FixRequest>("{ from: first-denial, status: 200 }").is_err());
        assert!(serde_yaml::from_str::<FixRequest>("{ as: bob }").is_err());
    }

    #[test]
    fn takes_the_newest_denial_with_a_ref() {
        let events = vec![
            ev(
                "policy.decided",
                json!({"tool_name":"a","denial":{"decisionRef":"dnl_1"}}),
            ),
            ev(
                "policy.decided",
                json!({"tool_name":"b","denial":{"reason":"POLICY_RULE"}}),
            ),
            ev("tool.failed", json!({"denial":{"decisionRef":"dnl_x"}})),
            ev(
                "egress.decided",
                json!({"data":{"denial":{"decision_ref":"dnl_2"}}}),
            ),
        ];
        assert_eq!(latest_decision_ref(&events, None).as_deref(), Some("dnl_2"));
        assert_eq!(
            latest_decision_ref(&events, Some("a")).as_deref(),
            Some("dnl_1")
        );
        assert_eq!(latest_decision_ref(&events, Some("b")), None);
        assert_eq!(latest_decision_ref(&[], None), None);
    }

    #[test]
    fn reads_a_payload_sent_as_a_json_string() {
        let events = vec![ev(
            "policy.decided",
            json!(r#"{"tool_name":"a","denial":{"decisionRef":"dnl_9"}}"#),
        )];
        assert_eq!(
            latest_decision_ref(&events, Some("a")).as_deref(),
            Some("dnl_9")
        );
        let empty = vec![ev("policy.decided", json!({"denial":{"decisionRef":""}}))];
        assert_eq!(latest_decision_ref(&empty, None), None);
    }
}
