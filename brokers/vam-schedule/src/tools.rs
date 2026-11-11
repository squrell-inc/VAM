//! The six tools, with the side effects the broker's manifest declares, and the schemas and
//! descriptions the model fills them from (wave 4: create, list, get and update).

use serde_json::json;
use vam_spec::v1::tool_spec::ExecutionProvider;
use vam_spec::v1::{HostExecSpec, SideEffectClass, ToolSpec};

pub const SCHEDULE_CREATE: &str = "schedule_create";
pub const SCHEDULE_LIST: &str = "schedule_list";
pub const SCHEDULE_GET: &str = "schedule_get";
pub const SCHEDULE_UPDATE: &str = "schedule_update";
pub const SCHEDULE_DELETE: &str = "schedule_delete";
pub const SCHEDULE_RUN_NOW: &str = "schedule_run_now";

const ADAPTER_ID: &str = "broker-schedule";

fn spec(name: &str, description: &str, schema: &str, side_effect: SideEffectClass) -> ToolSpec {
    ToolSpec {
        name: name.to_string(),
        description: description.to_string(),
        input_schema_json: schema.to_string(),
        execution_provider: Some(ExecutionProvider::HostExec(HostExecSpec {
            adapter_id: ADAPTER_ID.to_string(),
            config: Default::default(),
        })),
        side_effect: side_effect as i32,
        ..Default::default()
    }
}

fn run_schema() -> serde_json::Value {
    let place = |what: &str| {
        json!({
            "type": "object",
            "description": what,
            "properties": {
                "mode": {"type": "string", "enum": ["SAME", "NEW", "ID"]},
                "id": {"type": "string", "description": "With ID: the sp_ or th_ id."},
                "name": {"type": "string", "description": "With NEW: the label."}
            },
            "additionalProperties": false
        })
    };
    json!({
        "type": "object",
        "description": "What each fire runs. The user approves exactly this request once; every fire does it.",
        "properties": {
            "agentUrn": {"type": "string", "description": "The agent to run. Empty means you."},
            "workspace": place("Where the run happens. SAME (default) is this chat's Space; NEW makes a Space once (give a name); ID is an owned Space bound to that agent."),
            "thread": place("The conversation. NEW (default) makes one dedicated thread: name it after the schedule's title, or it is labelled untitled. SAME only with workspace SAME. ID is an existing thread of that Space."),
            "message": {"type": "string", "maxLength": 32768, "description": "The user turn every run starts with. Write it so it stands alone: the run has none of this chat."},
            "playbookRef": {"type": "object", "description": "A playbook to start instead of a message."},
            "inputs": {"type": "object", "description": "Playbook inputs."},
            "onBusy": {"type": "string", "enum": ["QUEUE", "SKIP"], "description": "If the Space is busy at the time: QUEUE (default) waits, SKIP starts nothing."},
            "expiresAt": {"type": "string", "description": "RFC 3339. The approval ends then (default 90 days, at most 365). For a one-off, set it a little after the fire time. Links in the message are allowed; the scheduled run applies its own egress when it fires."}
        },
        "additionalProperties": false
    })
}

fn when_schema() -> serde_json::Value {
    json!({
        "type": "object",
        "description": "Exactly one of once or cron.",
        "properties": {
            "once": {
                "type": "object",
                "description": "A single fire. Give either in (ISO-8601 duration counted from when the schedule is saved, such as PT30M, PT2H, P1D) or at (a local wall time like 2026-11-02T08:00 with timezone, or an instant with an offset like 2026-11-02T08:00:00Z). Prefer in for relative asks.",
                "properties": {
                    "in": {"type": "string"},
                    "at": {"type": "string"},
                    "timezone": {"type": "string", "description": "IANA zone for a local wall time."}
                },
                "additionalProperties": false
            },
            "cron": {
                "type": "object",
                "description": "A repeat: five fields (minute hour day-of-month month day-of-week) in an IANA zone. Fires closer than the operator's minimum interval (15 minutes by default) are refused.",
                "properties": {
                    "expression": {"type": "string"},
                    "timezone": {"type": "string"}
                },
                "required": ["expression", "timezone"],
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    })
}

fn catch_up_schema() -> serde_json::Value {
    json!({"type": "string", "enum": ["LATEST", "SKIP"], "description": "After downtime: LATEST (default) runs one missed fire inside the grace window, SKIP runs none."})
}

/// The keys each tool accepts at the top level (anything else is refused).
pub fn allowed_keys(tool: &str) -> &'static [&'static str] {
    match tool {
        SCHEDULE_CREATE => &["title", "when", "catchUp", "clientRequestId", "run"],
        SCHEDULE_LIST => &["enabledOnly", "limit"],
        SCHEDULE_GET | SCHEDULE_DELETE | SCHEDULE_RUN_NOW => &["scheduleId"],
        SCHEDULE_UPDATE => &["scheduleId", "title", "when", "enabled", "catchUp", "run"],
        _ => &[],
    }
}

/// The six tool specs.
pub fn tool_specs() -> Vec<ToolSpec> {
    let id = r#"{"type":"object","properties":{"scheduleId":{"type":"string"}},"required":["scheduleId"],"additionalProperties":false}"#;
    let create = json!({
        "type": "object",
        "properties": {
            "title": {"type": "string", "maxLength": 200, "description": "A short name the user will recognise."},
            "when": when_schema(),
            "catchUp": catch_up_schema(),
            "clientRequestId": {"type": "string", "description": "Optional. Repeat the same value when retrying a create so it is not made twice."},
            "run": run_schema()
        },
        "required": ["title", "when", "run"],
        "additionalProperties": false
    })
    .to_string();
    let list = json!({
        "type": "object",
        "properties": {
            "enabledOnly": {"type": "boolean"},
            "limit": {"type": "integer", "minimum": 1, "maximum": 200}
        },
        "additionalProperties": false
    })
    .to_string();
    let update = json!({
        "type": "object",
        "properties": {
            "scheduleId": {"type": "string"},
            "title": {"type": "string", "maxLength": 200},
            "when": when_schema(),
            "enabled": {"type": "boolean", "description": "false pauses the schedule, true resumes it."},
            "catchUp": catch_up_schema(),
            "run": run_schema()
        },
        "required": ["scheduleId"],
        "additionalProperties": false
    })
    .to_string();
    vec![
        spec(
            SCHEDULE_CREATE,
            "Create a schedule: a run that starts by itself at a time, with no one in the chat. The user approves this call once and that approval covers every fire, so fill `run` completely from the user's words and your run context: the agent, the Space (workspace), the thread, the message, what to do if the Space is busy, and an expiry. Use when.once.in for relative asks (\"in two hours\"), when.once.at for a date and time, when.cron for repeats. Answers the schedule id, the next fire time and where it will run.",
            &create,
            SideEffectClass::MutableExternal,
        ),
        spec(
            SCHEDULE_LIST,
            "List the user's schedules: id, title, when, next fire, state, where it runs, expiry and last outcome. No message text; use schedule_get for that.",
            &list,
            SideEffectClass::ReadOnly,
        ),
        spec(
            SCHEDULE_GET,
            "Show one schedule with its message and its last 10 fires. Answers NOT_FOUND for an id that is not the user's.",
            id,
            SideEffectClass::ReadOnly,
        ),
        spec(
            SCHEDULE_UPDATE,
            "Change a schedule you made: title, when, enabled (pause or resume) or catchUp. Passing `run` replaces what each fire runs; the user approves it again and the old approval is withdrawn.",
            &update,
            SideEffectClass::MutableExternal,
        ),
        spec(
            SCHEDULE_DELETE,
            "Delete a schedule you made. Its approval is withdrawn at the gateway first, so it can never start another run; if the gateway cannot be reached the schedule shows as deleting and the withdrawal is retried until it is done. Answers deleted, or deleting.",
            id,
            SideEffectClass::MutableExternal,
        ),
        spec(
            SCHEDULE_RUN_NOW,
            "Start the schedule's run once, right now, with its stored approval; the user is asked again for this call. Does not change the schedule's next fire. Answers the outcome (accepted, queued) and the run id, or why nothing started. Not for a one-off that already finished: its approval is gone.",
            id,
            SideEffectClass::MutableExternal,
        ),
    ]
}
