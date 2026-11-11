//! The `notifications_list` host built-in agent tool (NT-1.7).
//!
//! Read-only. It lists what waits for the run's owner (unresolved AND unread) with host
//! fields only. It never carries `agentText`, never marks a record read or shown, and is
//! refused in any run a trigger, delegate or report started (`runs.source` not NULL). The
//! run, its owner and its source come from the leased run, never from the arguments.

use serde_json::{json, Value};

use crate::session::ToolCatalogEntry;
use crate::workflow::SqliteWorkflowStore;

pub const TOOL: &str = "notifications_list";
pub const DEFAULT_LIMIT: usize = 10;
pub const MAX_LIMIT: usize = 20;
/// Largest result, in bytes. Whole records are dropped to fit.
pub const MAX_RESULT_BYTES: usize = 4 * 1024;

/// True when the agent has a rule that names exactly `builtin:notifications_list` and allows
/// it. A default (`allow`, `by_side_effect`, `ask`, `tools:*`) never opts in.
pub fn explicit_allow(permissions: Option<&vam_spec::v1::AgentPermissions>) -> bool {
    let Ok(want) = vam_spec::permissions::Target::parse("builtin:notifications_list") else {
        return false;
    };
    permissions
        .and_then(|p| p.tools.as_ref())
        .is_some_and(|tools| {
            tools.rules.iter().any(|rule| {
                rule.decision == vam_spec::v1::PermissionDecision::Allow as i32
                    && vam_spec::permissions::Target::parse(&rule.target).is_ok_and(|t| t == want)
            })
        })
}

/// Host-side lookup for a session: the agent the run is pinned to, never the tool args.
pub fn session_opted_in(store: &SqliteWorkflowStore, session_id: &str) -> bool {
    crate::policy_legacy::load_session_agent(store, session_id, None)
        .ok()
        .and_then(|s| s.agent)
        .is_some_and(|a| explicit_allow(a.permissions.as_ref()))
}

/// Catalog entry: a host tool of the host's own, read-only.
pub fn catalog_entry() -> ToolCatalogEntry {
    ToolCatalogEntry {
        tool_name: TOOL.to_string(),
        command: "host".to_string(),
        entrypoint: TOOL.to_string(),
        digest: vam_spec::validation::host_tool_digest(TOOL),
        side_effect: vam_spec::v1::SideEffectClass::ReadOnly as i32,
        skill_name: String::new(),
        approval_required: false,
    }
}

/// Run the tool for `run_id`. `Err` is the text of a typed tool failure.
pub fn run(
    store: &SqliteWorkflowStore,
    run_id: &str,
    args: &prost_types::Struct,
) -> Result<String, String> {
    if run_id.trim().is_empty() {
        return Err("notifications_list needs a run".to_string());
    }
    // Read on every call; a missing run fails closed.
    match store.run_source(run_id) {
        Ok(None) => {}
        Ok(Some(_)) => {
            return Err(
                "notifications_list is refused here: only a run a person started may read the \
                 notification list"
                    .to_string(),
            )
        }
        Err(_) => return Err("notifications_list: the run could not be checked".to_string()),
    }
    let owner = store
        .run_owner_user_id(run_id)
        .map_err(|_| "notifications_list: the run's owner could not be found".to_string())?;
    if owner.trim().is_empty() {
        return Err("notifications_list: the run has no owner".to_string());
    }
    let (kinds, limit) = parse_args(args)?;
    let rows = store
        .peek_waiting_notifications(&owner, &kinds, limit)
        .map_err(|_| "notifications_list: the list could not be read".to_string())?;
    Ok(render(rows.iter().map(host_fields).collect()))
}

fn parse_args(args: &prost_types::Struct) -> Result<(Vec<String>, usize), String> {
    let mut kinds = Vec::new();
    if let Some(v) = args.fields.get("kind") {
        use prost_types::value::Kind as K;
        match v.kind.as_ref() {
            Some(K::ListValue(list)) => {
                for item in &list.values {
                    match item.kind.as_ref() {
                        Some(K::StringValue(s)) => kinds.push(s.clone()),
                        _ => {
                            return Err("notifications_list: kind must be a list of strings".into())
                        }
                    }
                }
            }
            Some(K::StringValue(s)) => kinds.push(s.clone()),
            Some(K::NullValue(_)) | None => {}
            _ => return Err("notifications_list: kind must be a list of strings".into()),
        }
    }
    let mut limit = DEFAULT_LIMIT;
    if let Some(v) = args.fields.get("limit") {
        use prost_types::value::Kind as K;
        match v.kind.as_ref() {
            Some(K::NumberValue(n)) if n.is_finite() && *n >= 1.0 => {
                limit = (*n as usize).min(MAX_LIMIT);
            }
            Some(K::NullValue(_)) | None => {}
            _ => return Err("notifications_list: limit must be a number from 1 to 20".into()),
        }
    }
    Ok((kinds, limit))
}

/// The host fields of one record, and nothing else.
fn host_fields(r: &crate::notifications::Row) -> Value {
    let action: Value = serde_json::from_str(&r.action).unwrap_or(Value::Null);
    let mut m = serde_json::Map::new();
    let mut put = |k: &str, v: Value| {
        if !v.is_null() {
            m.insert(k.to_string(), v);
        }
    };
    put("id", json!(r.id));
    put("kind", json!(r.kind));
    put("severity", json!(r.severity));
    put("title", json!(r.title));
    put("body", json!(r.body));
    put("run_id", json!(r.run_id));
    put("thread_id", json!(r.thread_id));
    put("approval_id", json!(r.approval_id));
    put("agent_label", json!(r.agent_label));
    put(
        "via",
        r.via
            .as_deref()
            .and_then(|v| serde_json::from_str::<Value>(v).ok())
            .unwrap_or(Value::Null),
    );
    put("created_at", json!(r.created_at));
    // The action as a link and a command line for the person; the tool acts on neither.
    put(
        "action",
        json!({
            "verb": action.get("verb").cloned().unwrap_or(Value::Null),
            "link": action.get("target").cloned().unwrap_or(Value::Null),
            "cli": cli_line(r),
        }),
    );
    Value::Object(m)
}

fn cli_line(r: &crate::notifications::Row) -> String {
    match (&r.approval_id, r.is_decision()) {
        (Some(approval), true) => format!("vam approve decide {approval}"),
        _ => format!("vam notifications show {}", r.id),
    }
}

/// Whole records in, at most [`MAX_RESULT_BYTES`] out.
fn render(records: Vec<Value>) -> String {
    let total = records.len();
    let mut kept: Vec<Value> = Vec::new();
    for rec in records {
        kept.push(rec);
        let text =
            json!({"notifications": kept, "count": kept.len(), "total": total, "truncated": true})
                .to_string();
        if text.len() > MAX_RESULT_BYTES {
            kept.pop();
            break;
        }
    }
    let shown = kept.len();
    json!({
        "notifications": kept,
        "count": shown,
        "total": total,
        "truncated": shown < total,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::notifications::{Kind, NewNotification, Severity};

    fn args(v: Value) -> prost_types::Struct {
        let mut fields = std::collections::BTreeMap::new();
        if let Value::Object(m) = v {
            for (k, v) in m {
                fields.insert(k, to_pb(&v));
            }
        }
        prost_types::Struct { fields }
    }

    fn to_pb(v: &Value) -> prost_types::Value {
        use prost_types::value::Kind as K;
        prost_types::Value {
            kind: Some(match v {
                Value::Number(n) => K::NumberValue(n.as_f64().unwrap()),
                Value::String(s) => K::StringValue(s.clone()),
                Value::Array(a) => K::ListValue(prost_types::ListValue {
                    values: a.iter().map(to_pb).collect(),
                }),
                _ => K::NullValue(0),
            }),
        }
    }

    fn setup() -> (SqliteWorkflowStore, String) {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        let run_id = "run-1".to_string();
        {
            let c = store.connection.lock().unwrap();
            c.execute(
                "INSERT INTO runs (run_id, owner_user_id, state, revision) VALUES (?1, 'local', 'running', 1)",
                [&run_id],
            )
            .expect("run");
        }
        (store, run_id)
    }

    fn add(store: &SqliteWorkflowStore, subject: &str, kind: Kind, approval: &str) {
        let c = store.connection.lock().unwrap();
        let n = NewNotification {
            user_id: crate::users::LOCAL_USER_ID.into(),
            kind,
            severity: Severity::Action,
            subject_key: subject.into(),
            run_id: None,
            space_id: None,
            thread_id: None,
            approval_id: Some(approval.into()),
            grant_id: None,
            receipt_digest: None,
            agent_urn: String::new(),
            agent_label: "writer".into(),
            via: None,
            title: "Approval waiting".into(),
            body: "writer wants to use write_file".into(),
            action: json!({"verb":"decide","target":{"kind":"approval","id":approval}}),
        };
        crate::notifications::record(&c, &n).expect("record");
    }

    #[test]
    fn lists_host_fields_without_marking_anything() {
        let (store, run) = setup();
        add(&store, "a", Kind::ApprovalPending, "ap-1");
        let text = super::run(&store, &run, &args(json!({}))).unwrap();
        let v: Value = serde_json::from_str(&text).unwrap();
        let rec = &v["notifications"][0];
        assert_eq!(rec["approval_id"], "ap-1");
        assert_eq!(rec["action"]["cli"], "vam approve decide ap-1");
        assert!(!text.contains("agentText") && !text.contains("agent_text"));
        let (read, shown): (Option<i64>, Option<i64>) = store
            .connection
            .lock()
            .unwrap()
            .query_row(
                "SELECT read_at, first_shown_at FROM notifications WHERE user_id = 'local'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(read.is_none() && shown.is_none());
    }

    #[test]
    fn a_run_with_a_source_is_refused_on_every_call() {
        let (store, run) = setup();
        add(&store, "a", Kind::ApprovalPending, "ap-1");
        assert!(super::run(&store, &run, &args(json!({}))).is_ok());
        store
            .connection
            .lock()
            .unwrap()
            .execute(
                "UPDATE runs SET source = 'broker:b' WHERE run_id = 'run-1'",
                [],
            )
            .unwrap();
        let err = super::run(&store, &run, &args(json!({}))).unwrap_err();
        assert!(err.contains("refused"), "{err}");
    }

    #[test]
    fn limit_is_clamped_kind_filters_and_the_result_is_capped() {
        let (store, run) = setup();
        for i in 0..30 {
            add(
                &store,
                &format!("s{i}"),
                Kind::ApprovalPending,
                &format!("ap-{i}"),
            );
        }
        let v: Value =
            serde_json::from_str(&super::run(&store, &run, &args(json!({"limit": 99}))).unwrap())
                .unwrap();
        assert!(v["count"].as_u64().unwrap() <= MAX_LIMIT as u64);
        let text = super::run(&store, &run, &args(json!({"limit": 20}))).unwrap();
        assert!(text.len() <= MAX_RESULT_BYTES, "{}", text.len());
        let none = super::run(&store, &run, &args(json!({"kind": ["run.finished"]}))).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&none).unwrap()["count"], 0);
    }

    #[test]
    fn another_users_rows_are_never_listed() {
        let (store, run) = setup();
        add(&store, "a", Kind::ApprovalPending, "ap-1");
        store
            .connection
            .lock()
            .unwrap()
            .execute("UPDATE notifications SET user_id = 'other'", [])
            .unwrap();
        let v: Value =
            serde_json::from_str(&super::run(&store, &run, &args(json!({}))).unwrap()).unwrap();
        assert_eq!(v["count"], 0);
    }

    #[test]
    fn the_tool_source_has_no_write_path() {
        let src = include_str!("notifications_tool.rs");
        let body = src.split("#[cfg(test)]").next().unwrap();
        for bad in [
            "UPDATE",
            "INSERT",
            "DELETE",
            "mark_",
            "get_notification(",
            "list_notifications(",
        ] {
            assert!(!body.contains(bad), "{bad}");
        }
    }
}
