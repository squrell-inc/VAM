//! Operator notification settings for the node (`design/notifications.md` section 9.4): which
//! channel adapters are enabled, the vault secret **refs** each needs, the org's defaults for which
//! kinds go out, and the app URL for the Open in VAM link.
//!
//! Only ref names are stored. A value is read from the vault per delivery (wave 3) and never copied
//! here. A ref is checked to exist as an org-scope secret by `host_secret_exists`, which counts rows
//! and decrypts nothing.

use std::collections::BTreeMap;

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::OrchestratorError;
use crate::notifications::Kind;
use crate::workflow::SqliteWorkflowStore;

/// The closed list of channel adapters an operator can enable.
pub const CHANNELS: &[&str] = &["slack"];
const MAX_REF_LEN: usize = 128;
const MAX_URL_LEN: usize = 512;

/// One adapter. `enabled` is a switch; the refs name org-scope vault secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AdapterSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bot_token_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_token_ref: Option<String>,
}

/// The body of `PUT /v1/node/notify` and what is stored. A new adapter is a new field here.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NodeNotifySettings {
    #[serde(default)]
    pub slack: AdapterSettings,
    /// Org defaults per kind: `immediate` or `off`. A user's own setting wins.
    #[serde(default)]
    pub kind_defaults: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_url: Option<String>,
}

fn invalid(msg: impl Into<String>) -> OrchestratorError {
    OrchestratorError::InvalidContract(msg.into())
}

fn valid_ref_name(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_REF_LEN
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
}

impl NodeNotifySettings {
    /// Whether users may link a contact on this channel.
    pub fn channel_enabled(&self, channel: &str) -> bool {
        match channel {
            "slack" => self.slack.enabled,
            _ => false,
        }
    }

    /// Check the shape, then that every named ref exists (`ref_exists`, org scope). Nothing is
    /// read from a secret. Returns the normalised settings to store.
    pub fn validate(
        mut self,
        ref_exists: impl Fn(&str) -> Result<bool, OrchestratorError>,
    ) -> Result<Self, OrchestratorError> {
        for (kind, mode) in &self.kind_defaults {
            if Kind::parse(kind).is_none() {
                return Err(invalid(format!(
                    "kindDefaults: `{kind}` is not a notification kind"
                )));
            }
            if !matches!(mode.as_str(), "immediate" | "off") {
                return Err(invalid(format!(
                    "kindDefaults.{kind}: `{mode}` is not immediate or off"
                )));
            }
        }
        let slack = &mut self.slack;
        for (field, r) in [
            ("slack.botTokenRef", &mut slack.bot_token_ref),
            ("slack.appTokenRef", &mut slack.app_token_ref),
        ] {
            if let Some(v) = r.as_deref().map(str::trim) {
                if v.is_empty() {
                    *r = None;
                    continue;
                }
                if !valid_ref_name(v) {
                    return Err(invalid(format!("{field} is not a vault secret name")));
                }
                if !ref_exists(v)? {
                    return Err(invalid(format!(
                        "{field}: no org-scope vault secret named `{v}` (put it with PUT /v1/secrets, scope org)"
                    )));
                }
                *r = Some(v.to_string());
            }
        }
        if slack.enabled && (slack.bot_token_ref.is_none() || slack.app_token_ref.is_none()) {
            return Err(invalid(
                "slack is enabled: set slack.botTokenRef and slack.appTokenRef (vault secret names)",
            ));
        }
        if let Some(u) = self.app_url.as_deref().map(str::trim) {
            if u.is_empty() {
                self.app_url = None;
            } else {
                let parsed = url::Url::parse(u).map_err(|_| invalid("appUrl is not a URL"))?;
                if parsed.scheme() != "https"
                    || parsed.host_str().is_none()
                    || !parsed.username().is_empty()
                    || parsed.password().is_some()
                    || u.len() > MAX_URL_LEN
                {
                    return Err(invalid(
                        "appUrl must be an https URL without credentials (at most 512 characters)",
                    ));
                }
                self.app_url = Some(u.trim_end_matches('/').to_string());
            }
        }
        Ok(self)
    }

    /// The org default for a kind, if the operator set one.
    pub fn kind_default(&self, kind: Kind) -> Option<bool> {
        match self.kind_defaults.get(kind.as_str()).map(String::as_str) {
            Some("immediate") => Some(true),
            Some("off") => Some(false),
            _ => None,
        }
    }

    /// The settings without the ref names, for a reader who is not the operator.
    pub fn redacted(&self) -> Self {
        let mut r = self.clone();
        r.slack.bot_token_ref = None;
        r.slack.app_token_ref = None;
        r
    }
}

pub(crate) fn initialize_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS node_notify_settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            settings_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );
        ",
    )?;
    Ok(())
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl SqliteWorkflowStore {
    /// The stored settings, or the default (nothing enabled) when the operator has set none.
    pub fn node_notify_settings(
        &self,
    ) -> Result<(NodeNotifySettings, Option<i64>), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        let row: Option<(String, i64)> = connection
            .query_row(
                "SELECT settings_json, updated_at FROM node_notify_settings WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            None => Ok((NodeNotifySettings::default(), None)),
            Some((json, at)) => Ok((
                serde_json::from_str(&json).map_err(|e| OrchestratorError::Core(e.to_string()))?,
                Some(at),
            )),
        }
    }

    /// Replace the settings. The caller validated them (`NodeNotifySettings::validate`).
    pub fn put_node_notify_settings(
        &self,
        settings: &NodeNotifySettings,
    ) -> Result<i64, OrchestratorError> {
        let json =
            serde_json::to_string(settings).map_err(|e| OrchestratorError::Core(e.to_string()))?;
        let at = now_secs();
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO node_notify_settings (id, settings_json, updated_at) VALUES (1, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET settings_json = excluded.settings_json,
                                           updated_at = excluded.updated_at",
            params![json, at],
        )?;
        Ok(at)
    }

    /// Whether users may link a contact on `channel` now.
    pub fn notify_channel_enabled(&self, channel: &str) -> Result<bool, OrchestratorError> {
        Ok(self.node_notify_settings()?.0.channel_enabled(channel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exists(names: &'static [&'static str]) -> impl Fn(&str) -> Result<bool, OrchestratorError> {
        move |r| Ok(names.contains(&r))
    }

    fn parse(json: &str) -> NodeNotifySettings {
        serde_json::from_str(json).unwrap()
    }

    #[test]
    fn enabled_slack_needs_both_refs_that_exist() {
        let ok = parse(
            r#"{"slack":{"enabled":true,"botTokenRef":"notify/slack/bot-token","appTokenRef":"notify/slack/app-token"},"appUrl":"https://vam.example.test/"}"#,
        )
        .validate(exists(&["notify/slack/bot-token", "notify/slack/app-token"]))
        .unwrap();
        assert_eq!(ok.app_url.as_deref(), Some("https://vam.example.test"));
        assert!(ok.channel_enabled("slack") && !ok.channel_enabled("email"));
        let missing = parse(
            r#"{"slack":{"enabled":true,"botTokenRef":"notify/slack/bot-token","appTokenRef":"nope"}}"#,
        )
        .validate(exists(&["notify/slack/bot-token"]));
        assert!(
            matches!(missing, Err(OrchestratorError::InvalidContract(m)) if m.contains("nope"))
        );
        let one = parse(r#"{"slack":{"enabled":true,"botTokenRef":"a"}}"#).validate(exists(&["a"]));
        assert!(one.is_err());
        // Disabled needs no refs.
        assert!(parse(r#"{"slack":{"enabled":false}}"#)
            .validate(exists(&[]))
            .is_ok());
    }

    #[test]
    fn closed_list_and_bad_values_are_refused() {
        assert!(
            serde_json::from_str::<NodeNotifySettings>(r#"{"email":{"enabled":true}}"#).is_err()
        );
        // A raw token field does not exist: values cannot be sent at all.
        assert!(serde_json::from_str::<NodeNotifySettings>(
            r#"{"slack":{"enabled":true,"botToken":"xoxb-real"}}"#
        )
        .is_err());
        for bad in [
            r#"{"appUrl":"http://vam.example.test"}"#,
            r#"{"appUrl":"https://u:p@vam.example.test"}"#,
            r#"{"appUrl":"not a url"}"#,
            r#"{"kindDefaults":{"nope":"off"}}"#,
            r#"{"kindDefaults":{"run.finished":"digest"}}"#,
            r#"{"slack":{"botTokenRef":"has space"}}"#,
        ] {
            assert!(parse(bad).validate(exists(&[])).is_err(), "{bad}");
        }
        let d = parse(r#"{"kindDefaults":{"run.finished":"immediate","trigger.fired":"off"}}"#)
            .validate(exists(&[]))
            .unwrap();
        assert_eq!(d.kind_default(Kind::RunFinished), Some(true));
        assert_eq!(d.kind_default(Kind::TriggerFired), Some(false));
        assert_eq!(d.kind_default(Kind::BudgetHit), None);
    }

    #[test]
    fn redacted_drops_the_refs() {
        let s = parse(r#"{"slack":{"enabled":true,"botTokenRef":"a","appTokenRef":"b"}}"#);
        let r = s.redacted();
        assert!(
            r.slack.enabled && r.slack.bot_token_ref.is_none() && r.slack.app_token_ref.is_none()
        );
    }

    #[test]
    fn store_round_trips_refs_only() {
        let store = SqliteWorkflowStore::in_memory().unwrap();
        assert_eq!(
            store.node_notify_settings().unwrap().0,
            NodeNotifySettings::default()
        );
        assert!(!store.notify_channel_enabled("slack").unwrap());
        let s = parse(r#"{"slack":{"enabled":true,"botTokenRef":"a","appTokenRef":"b"}}"#);
        store.put_node_notify_settings(&s).unwrap();
        assert_eq!(store.node_notify_settings().unwrap().0, s);
        assert!(store.notify_channel_enabled("slack").unwrap());
    }
}
