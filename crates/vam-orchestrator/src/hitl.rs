//! HITL approval helpers: schema migration, amend validation, default amendable schema.
//!
//! Decision 9 seam: [`HitlApprover`] selects who may `decide_approval`. The
//! default is the parent run owner. A future Cedar `Reviewer::"<id>"` principal
//! is typed here — it is **not** a [`crate::users::UserRole`].

use crate::error::OrchestratorError;
use crate::owner::resolved_owner;
use crate::workflow::SqliteWorkflowStore;
use rusqlite::Connection;
use serde_json::Value;

pub const REQUESTED_ARGS_MAX_BYTES: usize = 64 * 1024;

/// Persisted `approvals.kind` for a suspended tool call.
pub const APPROVAL_KIND_TOOL_CALL: &str = "tool_call";

/// Persisted `approvals.kind` for a playbook gate step.
pub const APPROVAL_KIND_GATE: &str = "gate";

/// Persisted `approvals.kind` for an `ask_user` question (written from wave 5; the
/// stop of a parked run already cancels one).
pub const APPROVAL_KIND_QUESTION: &str = "question";

/// Reserved Cedar entity type for a future HITL reviewer principal.
/// Not minted or selected in this slice.
pub const REVIEWER_CEDAR_TYPE: &str = "Reviewer";
/// Prefix for a persisted `Reviewer::"<id>"` selector (`approver_user_id`).
pub const REVIEWER_PRINCIPAL_PREFIX: &str = "Reviewer::";

/// Who may decide a published HITL request.
///
/// This is **not** a control-plane [`crate::users::UserRole`]. Reviewer and
/// manager audiences are not shipped — only [`HitlApprover::Owner`] is
/// constructed on publish (default = run owner).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HitlApprover {
    /// Control-plane user who may decide. Default: the parent run's owner.
    Owner { user_id: String },
    /// Reserved Cedar `Reviewer::"<id>"` principal. Not selected by publish.
    Reviewer { principal: String },
}

impl HitlApprover {
    pub fn owner(user_id: impl Into<String>) -> Self {
        Self::Owner {
            user_id: resolved_owner(&user_id.into()).to_string(),
        }
    }

    /// Typed constructor for the future Cedar reviewer principal.
    /// Publish does not call this in this slice.
    pub fn reviewer(id: &str) -> Self {
        let id = id.trim().trim_matches('"');
        Self::Reviewer {
            principal: format!("{REVIEWER_CEDAR_TYPE}::\"{id}\""),
        }
    }

    /// Parse the persisted `approvals.approver_user_id` selector.
    pub fn parse(stored: &str) -> Self {
        let stored = stored.trim();
        if let Some(rest) = stored.strip_prefix(REVIEWER_PRINCIPAL_PREFIX) {
            let id = rest.trim().trim_matches('"');
            if !id.is_empty() {
                return Self::reviewer(id);
            }
        }
        Self::owner(stored)
    }

    pub fn persist(&self) -> String {
        match self {
            Self::Owner { user_id } => user_id.clone(),
            Self::Reviewer { principal } => principal.clone(),
        }
    }

    /// True when `caller_user_id` may `decide_approval`.
    ///
    /// [`HitlApprover::Reviewer`] never matches a control-plane user in this
    /// slice — there is no reviewer role or Cedar reviewer principal yet.
    pub fn allows_decider(&self, caller_user_id: &str) -> bool {
        match self {
            Self::Owner { user_id } => user_id == resolved_owner(caller_user_id),
            Self::Reviewer { .. } => false,
        }
    }
}

pub fn initialize_hitl_schema(connection: &Connection) -> Result<(), OrchestratorError> {
    for ddl in [
        "ALTER TABLE approvals ADD COLUMN approval_message TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE approvals ADD COLUMN tool_name TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE approvals ADD COLUMN requested_args_json TEXT NOT NULL DEFAULT '{}'",
        "ALTER TABLE approvals ADD COLUMN amendable_schema_json TEXT NOT NULL DEFAULT '{}'",
        "ALTER TABLE approvals ADD COLUMN args_digest TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE approvals ADD COLUMN required_roles_json TEXT NOT NULL DEFAULT '[]'",
        "ALTER TABLE approvals ADD COLUMN created_at INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE approvals ADD COLUMN expires_at INTEGER",
        "ALTER TABLE approvals ADD COLUMN reviewer_principal TEXT",
        "ALTER TABLE approvals ADD COLUMN decision_revision INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE approvals ADD COLUMN published INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE approvals ADD COLUMN amended_args_json TEXT",
        "ALTER TABLE approvals ADD COLUMN amended_args_digest TEXT",
        "ALTER TABLE approvals ADD COLUMN rejection_reason TEXT",
        "ALTER TABLE approvals ADD COLUMN session_id TEXT",
        "ALTER TABLE approvals ADD COLUMN owner_user_id TEXT REFERENCES users(user_id) DEFAULT 'local'",
        "ALTER TABLE approvals ADD COLUMN approver_user_id TEXT",
        "ALTER TABLE approvals ADD COLUMN kind TEXT NOT NULL DEFAULT 'tool_call'",
        "ALTER TABLE approvals ADD COLUMN target TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE approvals ADD COLUMN always_offered INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE approvals ADD COLUMN locked_reason TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE approvals ADD COLUMN card_json TEXT",
    ] {
        let _ = connection.execute(ddl, []);
    }
    // Whether a resume was told about a decision (`resume_decisions`). A store from before the
    // column existed has already resumed every decision it holds: those count as delivered.
    if connection
        .execute("ALTER TABLE approvals ADD COLUMN resume_state TEXT", [])
        .is_ok()
    {
        let _ = connection.execute(
            "UPDATE approvals SET resume_state = 'delivered' WHERE decision IS NOT NULL",
            [],
        );
    }
    // Default selector = run owner (Phase 1 stamp). Empty/NULL backfill.
    let _ = connection.execute(
        "UPDATE approvals SET approver_user_id = COALESCE(NULLIF(approver_user_id, ''), owner_user_id, 'local')
         WHERE approver_user_id IS NULL OR approver_user_id = ''",
        [],
    );
    let _ = connection.execute(
        "CREATE INDEX IF NOT EXISTS idx_approvals_approver_user_id ON approvals(approver_user_id)",
        [],
    );

    connection
        .execute_batch(
            "
            CREATE TABLE IF NOT EXISTS hitl_pending (
                request_id TEXT PRIMARY KEY NOT NULL,
                run_id TEXT NOT NULL,
                step_id TEXT NOT NULL,
                attempt_id TEXT NOT NULL,
                session_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                requested_args_json TEXT NOT NULL,
                args_digest TEXT NOT NULL,
                approval_message TEXT NOT NULL,
                amendable_schema_json TEXT NOT NULL,
                required_roles_json TEXT NOT NULL DEFAULT '[]',
                expires_at INTEGER,
                created_at INTEGER NOT NULL,
                target TEXT NOT NULL DEFAULT '',
                always_offered INTEGER NOT NULL DEFAULT 0,
                locked_reason TEXT NOT NULL DEFAULT ''
            );

            CREATE TABLE IF NOT EXISTS hitl_grants (
                session_id TEXT NOT NULL,
                tool_name TEXT NOT NULL,
                args_digest TEXT NOT NULL,
                PRIMARY KEY (session_id, tool_name, args_digest)
            );
            ",
        )
        .map_err(OrchestratorError::Database)?;
    // Stamped from the session's tool target when the tool suspends (AX-5.3b).
    for ddl in [
        "ALTER TABLE hitl_pending ADD COLUMN target TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE hitl_pending ADD COLUMN always_offered INTEGER NOT NULL DEFAULT 0",
        "ALTER TABLE hitl_pending ADD COLUMN locked_reason TEXT NOT NULL DEFAULT ''",
        "ALTER TABLE hitl_pending ADD COLUMN card_json TEXT",
    ] {
        let _ = connection.execute(ddl, []);
    }
    Ok(())
}

impl SqliteWorkflowStore {
    pub fn initialize_hitl_schema(&self) -> Result<(), OrchestratorError> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        initialize_hitl_schema(&connection)
    }

    /// Persist a published approval whose selector defaults to `owner_user_id`.
    pub fn record_published_approval(
        &self,
        request_id: &str,
        run_id: &str,
        owner_user_id: &str,
        requested_args_json: &str,
    ) -> Result<(), OrchestratorError> {
        self.record_published_approval_of_kind(
            request_id,
            run_id,
            owner_user_id,
            requested_args_json,
            APPROVAL_KIND_TOOL_CALL,
            "tool",
        )
    }

    /// [`Self::record_published_approval`] with an approval kind and tool name
    /// (a `question` for `ask_user`, a `gate`). For tests of the API.
    pub fn record_published_approval_of_kind(
        &self,
        request_id: &str,
        run_id: &str,
        owner_user_id: &str,
        requested_args_json: &str,
        kind: &str,
        tool_name: &str,
    ) -> Result<(), OrchestratorError> {
        let owner = resolved_owner(owner_user_id).to_string();
        let connection = self
            .connection
            .lock()
            .map_err(|_| OrchestratorError::StorageUnavailable)?;
        connection.execute(
            "INSERT INTO approvals (
                request_id, run_id, step_id, attempt_id, checkpoint_digest,
                continuation_generation, published, session_id, owner_user_id,
                approver_user_id, requested_args_json, tool_name, kind
             ) VALUES (?1, ?2, 's', 'a', 'd', 0, 1, 'sess', ?3, ?3, ?4, ?5, ?6)",
            rusqlite::params![
                request_id,
                run_id,
                owner,
                requested_args_json,
                tool_name,
                kind
            ],
        )?;
        Ok(())
    }
}

/// Default amendable schema for Cedar intercept: object copy of tool args keys.
pub fn default_amendable_schema(args: &Value) -> Value {
    let props = match args {
        Value::Object(map) => map
            .keys()
            .map(|k| (k.clone(), serde_json::json!({ "type": "string" })))
            .collect::<serde_json::Map<String, Value>>(),
        _ => serde_json::Map::new(),
    };
    serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "properties": props,
        "additionalProperties": false
    })
}

/// A skill's ask card stored with an approval (CN-3.5): the checked IR bound to the requested
/// args, the args themselves, and the submit fields an approver may edit.
#[derive(Debug, Clone, PartialEq)]
pub struct AskApprovalCard {
    pub jig: String,
    pub skill: String,
    /// `bind_args(ir, args)`; never carries a preset `decision`.
    pub ir: Value,
    /// The requested args.
    pub data: Value,
    /// The card's submit `with` fields.
    pub submit: Vec<String>,
    /// The tool's input schema (field types for the amendable schema).
    pub input_schema: Value,
}

impl AskApprovalCard {
    /// The stored form. Submit fields and the schema are not stored: the amendable schema is.
    pub fn stored_json(&self) -> Value {
        serde_json::json!({
            "jig": self.jig, "skill": self.skill, "ir": self.ir, "data": self.data
        })
    }

    /// Only the submit `with` fields, each typed from the tool's input schema.
    /// `decision` is reserved and never offered.
    pub fn amendable_schema(&self) -> Value {
        let tool_props = self
            .input_schema
            .get("properties")
            .and_then(Value::as_object);
        let props = self
            .submit
            .iter()
            .filter(|name| name.as_str() != "decision")
            .filter_map(|name| {
                tool_props
                    .and_then(|p| p.get(name))
                    .map(|schema| (name.clone(), schema.clone()))
            })
            .collect::<serde_json::Map<String, Value>>();
        serde_json::json!({
            "$schema": "http://json-schema.org/draft-07/schema#",
            "type": "object",
            "properties": props,
            "additionalProperties": false
        })
    }
}

/// The merged args of a card approval: the requested args with only the amended submit fields
/// replaced. Any key outside the stored amendable schema (or `decision`) or a mistyped value is
/// refused. Returned canonical (sorted keys), so its digest is the one a resumed call computes.
pub fn merge_card_amend(
    requested_args_json: &str,
    amended: &Value,
    schema: &Value,
) -> Result<Value, OrchestratorError> {
    let obj = amended.as_object().ok_or_else(|| {
        OrchestratorError::InvalidContract("amended_args must be a JSON object".into())
    })?;
    if obj.contains_key("decision") {
        return Err(OrchestratorError::InvalidContract(
            "amended_args key 'decision' is reserved".into(),
        ));
    }
    validate_amended_args(amended, schema)?;
    validate_json_schema(amended, schema, "amended_args")?;
    let mut merged: Value = serde_json::from_str(requested_args_json).map_err(|e| {
        OrchestratorError::InvalidContract(format!("requested_args_json invalid: {e}"))
    })?;
    let map = merged.as_object_mut().ok_or_else(|| {
        OrchestratorError::InvalidContract("requested args must be a JSON object".into())
    })?;
    for (key, value) in obj {
        map.insert(key.clone(), value.clone());
    }
    let canonical =
        vam_spec::canonical_args_json(Some(&json_to_struct(&merged)?)).map_err(|e| {
            OrchestratorError::InvalidContract(format!("merged args cannot be canonicalized: {e}"))
        })?;
    Ok(canonical)
}

fn json_to_struct(value: &Value) -> Result<prost_types::Struct, OrchestratorError> {
    vam_spec::encoding::json_to_proto("google.protobuf.Struct", &value.to_string())
        .map_err(|e| OrchestratorError::InvalidContract(format!("args encode failed: {e}")))
}

/// What a `start_playbook` ask card needs: the amendable schema and the
/// reviewer message. The VAM app reads both (AR-0.10).
#[derive(Debug, Clone)]
pub struct StartPlaybookApproval {
    pub amendable_schema: Value,
    pub message: String,
    /// Why the card is confirmed each time. `None` keeps the default for the tool. A value that
    /// starts with [`REAUTH_REASON_PREFIX`] makes the approval a `reauth` park.
    pub locked_reason: Option<String>,
}

/// Persisted `approvals.kind` for a call parked until an MCP connection is signed in again.
/// A reconnect resolves it (decision `RECONNECTED`, never a grant); the call then gets its
/// normal policy decision.
pub const APPROVAL_KIND_REAUTH: &str = "reauth";

/// `locked_reason` of a reauth park: `reauth:<scope>:<owner>:<connection id>`.
pub const REAUTH_REASON_PREFIX: &str = "reauth:";

/// The key that names the connection a reauth park waits on. Scope, owner and id together, so a
/// reconnect of another owner's connection with the same id resolves nothing.
pub fn reauth_key(
    scope: crate::secrets::SecretScope,
    owner_user_id: &str,
    connection_id: &str,
) -> String {
    format!(
        "{REAUTH_REASON_PREFIX}{}:{owner_user_id}:{connection_id}",
        scope.as_str()
    )
}

/// The decision a reconnect records on a reauth park.
pub const DECISION_RECONNECTED: &str = "RECONNECTED";

/// What a question approval stores in place of a tool approval's amendable
/// schema and message: the jig's result schema and the question. Reuses
/// [`StartPlaybookApproval`] (a schema plus a message).
pub fn question_approval(jig: &str, data: &Value) -> Option<StartPlaybookApproval> {
    let builtin = vam_spec::jig::builtin(jig).ok()?;
    let message = match data.get("question").and_then(Value::as_str) {
        Some(q) if !q.is_empty() => q.to_string(),
        _ => "The agent asks for your input".to_string(),
    };
    Some(StartPlaybookApproval {
        amendable_schema: builtin.result_schema().clone(),
        message,
        locked_reason: None,
    })
}

/// Validate a question's answer before anything is written: size first, then
/// the jig's result check against the data that was asked (`check_result`, jig
/// decision 14), so an answer outside the offered choices is refused.
pub fn validate_question_answer(
    requested_args_json: &str,
    response_json: &str,
) -> Result<Value, OrchestratorError> {
    if response_json.len() > vam_spec::outbox::MAX_ANSWER_BYTES {
        return Err(OrchestratorError::InvalidContract(format!(
            "the answer is {} bytes; the limit is {}",
            response_json.len(),
            vam_spec::outbox::MAX_ANSWER_BYTES
        )));
    }
    let answer: Value = serde_json::from_str(response_json)
        .map_err(|_| OrchestratorError::InvalidContract("answer must be a JSON object".into()))?;
    if !answer.is_object() {
        return Err(OrchestratorError::InvalidContract(
            "answer must be a JSON object".into(),
        ));
    }
    let asked: Value = serde_json::from_str(requested_args_json).map_err(|e| {
        OrchestratorError::CorruptState(format!("stored question is unreadable: {e}"))
    })?;
    let jig = asked.get("jig").and_then(Value::as_str).unwrap_or("");
    let data = asked.get("data").cloned().unwrap_or(Value::Null);
    vam_spec::jig::check_result(jig, &data, &answer).map_err(|errs| {
        let detail = errs
            .iter()
            .map(|e| format!("{}: {}", e.path, e.message))
            .collect::<Vec<_>>()
            .join("; ");
        OrchestratorError::InvalidContract(format!("the answer breaks the question: {detail}"))
    })?;
    Ok(answer)
}

/// Amendable schema for a `start_playbook` approval.
///
/// The amended arguments replace the tool call, so the schema wraps the input
/// schema: `{urn, inputs}`. `urn` is fixed to the requested playbook. `title`
/// is the playbook name and `description` its `when`. `x-vam-playbook` carries
/// the urn, version and `has_inputs`; when `has_inputs` is false the app shows
/// no form and `properties.inputs` is an empty object schema.
pub fn start_playbook_amendable_schema(
    urn: &str,
    name: &str,
    when: &str,
    version: &str,
    has_inputs: bool,
    input_schema: &Value,
) -> Value {
    serde_json::json!({
        "$schema": "http://json-schema.org/draft-07/schema#",
        "type": "object",
        "title": name,
        "description": when,
        "x-vam-playbook": { "urn": urn, "version": version, "has_inputs": has_inputs },
        "properties": {
            "urn": { "const": urn },
            "inputs": input_schema,
        },
        "required": ["urn", "inputs"],
        "additionalProperties": false
    })
}

/// Reviewer copy for a `start_playbook` approval.
pub fn render_start_playbook_message(name: &str, when: &str) -> String {
    format!("Start playbook '{name}'? {when}\nReview the inputs and confirm, edit, or decline.")
}

/// Extra amend checks for a `start_playbook` approval: the playbook cannot be
/// swapped, and `inputs` must satisfy the playbook's input schema.
pub fn validate_start_playbook_amend(
    requested_args_json: &str,
    amended: &Value,
    schema: &Value,
) -> Result<(), OrchestratorError> {
    let requested: Value = serde_json::from_str(requested_args_json).map_err(|e| {
        OrchestratorError::InvalidContract(format!("requested_args_json invalid: {e}"))
    })?;
    let invalid = |msg: String| OrchestratorError::InvalidContract(msg);
    if amended.get("urn") != requested.get("urn") {
        return Err(invalid(
            "amend rejected: the playbook urn cannot be changed".into(),
        ));
    }
    let inputs = amended
        .get("inputs")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let Some(inputs_obj) = inputs.as_object() else {
        return Err(invalid("amend rejected: inputs must be an object".into()));
    };
    let has_inputs = schema
        .pointer("/x-vam-playbook/has_inputs")
        .and_then(Value::as_bool)
        .unwrap_or(true);
    if !has_inputs {
        if !inputs_obj.is_empty() {
            return Err(invalid(
                "amend rejected: this playbook takes no inputs".into(),
            ));
        }
        return Ok(());
    }
    let input_schema = schema
        .pointer("/properties/inputs")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let schema_struct = crate::playbook_activation::json_to_struct(&input_schema);
    let inputs_struct = crate::playbook_activation::json_to_struct(&inputs);
    vam_spec::validate_playbook_inputs(schema_struct.as_ref(), inputs_struct.as_ref())
        .map_err(|e| invalid(format!("amend rejected: {e}")))
}

pub fn validate_requested_args_size(json: &str) -> Result<(), OrchestratorError> {
    if json.len() > REQUESTED_ARGS_MAX_BYTES {
        return Err(OrchestratorError::InvalidContract(format!(
            "requested_args_json exceeds {} byte cap",
            REQUESTED_ARGS_MAX_BYTES
        )));
    }
    Ok(())
}

/// Minimal draft-07 validation: amended args must match amendable_schema object properties.
/// Check a gate `SUBMITTED` response against the schema captured when the gate opened.
///
/// An empty schema accepts any JSON object. Otherwise this checks `type`,
/// `enum`, `format` (`date`, `vam-workspace-file`), `properties`, `required`,
/// `additionalProperties`, and array `items`.
pub fn validate_gate_response(
    response_json: &str,
    schema_json: &str,
) -> Result<serde_json::Value, OrchestratorError> {
    let response: Value = serde_json::from_str(response_json).map_err(|_| {
        OrchestratorError::InvalidContract("gate response must be a JSON object".into())
    })?;
    if !response.is_object() {
        return Err(OrchestratorError::InvalidContract(
            "gate response must be a JSON object".into(),
        ));
    }
    let schema: Value = if schema_json.trim().is_empty() {
        Value::Object(serde_json::Map::new())
    } else {
        serde_json::from_str(schema_json).map_err(|err| {
            OrchestratorError::InvalidContract(format!("gate response schema is corrupt: {err}"))
        })?
    };
    validate_json_schema(&response, &schema, "response")?;
    Ok(response)
}

pub(crate) fn validate_json_schema(
    value: &Value,
    schema: &Value,
    path: &str,
) -> Result<(), OrchestratorError> {
    let Some(schema) = schema.as_object() else {
        return Ok(());
    };
    if schema.is_empty() {
        return Ok(());
    }
    if let Some(enum_vals) = schema.get("enum").and_then(Value::as_array) {
        if !enum_vals.iter().any(|candidate| candidate == value) {
            return Err(OrchestratorError::InvalidContract(format!(
                "{path}: value is not in the schema enum"
            )));
        }
    }
    if let Some(kind) = schema.get("type").and_then(Value::as_str) {
        if !schema_type_matches(kind, value) {
            return Err(OrchestratorError::InvalidContract(format!(
                "{path}: expected {kind}"
            )));
        }
    }
    if let (Some(format), Some(text)) =
        (schema.get("format").and_then(Value::as_str), value.as_str())
    {
        vam_spec::schema_format::check_format(format, text)
            .map_err(|err| OrchestratorError::InvalidContract(format!("{path}: {err}")))?;
    }
    if let Some(fields) = value.as_object() {
        let properties = schema.get("properties").and_then(Value::as_object);
        let additional_ok = schema
            .get("additionalProperties")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        if let Some(required) = schema.get("required").and_then(Value::as_array) {
            for name in required.iter().filter_map(Value::as_str) {
                if !fields.contains_key(name) {
                    return Err(OrchestratorError::InvalidContract(format!(
                        "{path}.{name}: required by the gate response schema"
                    )));
                }
            }
        }
        for (key, field) in fields {
            match properties.and_then(|props| props.get(key)) {
                Some(property_schema) => {
                    validate_json_schema(field, property_schema, &format!("{path}.{key}"))?;
                }
                None if !additional_ok => {
                    return Err(OrchestratorError::InvalidContract(format!(
                        "{path}.{key}: not declared in the gate response schema"
                    )));
                }
                None => {}
            }
        }
    }
    if let Some(items) = value.as_array() {
        if let Some(item_schema) = schema.get("items") {
            for (index, item) in items.iter().enumerate() {
                validate_json_schema(item, item_schema, &format!("{path}[{index}]"))?;
            }
        }
    }
    Ok(())
}

fn schema_type_matches(expected: &str, value: &Value) -> bool {
    match expected {
        "string" => value.is_string(),
        "integer" => {
            value
                .as_f64()
                .is_some_and(|number| number.is_finite() && number.fract() == 0.0)
                || value.as_i64().is_some()
                || value.as_u64().is_some()
        }
        "number" => value.as_f64().is_some_and(|number| number.is_finite()),
        "boolean" => value.is_boolean(),
        "object" => value.is_object(),
        "array" => value.is_array(),
        "null" => value.is_null(),
        _ => true,
    }
}

pub fn validate_amended_args(amended: &Value, schema: &Value) -> Result<(), OrchestratorError> {
    let empty_schema = schema.as_object().is_none_or(|o| o.is_empty());
    if empty_schema {
        return Err(OrchestratorError::InvalidContract(
            "amend rejected: empty amendable_schema".into(),
        ));
    }
    let amended_obj = amended.as_object().ok_or_else(|| {
        OrchestratorError::InvalidContract("amended_args must be a JSON object".into())
    })?;
    let props = schema
        .get("properties")
        .and_then(|v| v.as_object())
        .ok_or_else(|| {
            OrchestratorError::InvalidContract("amendable_schema must define properties".into())
        })?;
    if props.is_empty() {
        return Err(OrchestratorError::InvalidContract(
            "amend rejected: empty amendable_schema properties".into(),
        ));
    }
    for key in amended_obj.keys() {
        if !props.contains_key(key) {
            return Err(OrchestratorError::InvalidContract(format!(
                "amended_args key '{}' not in amendable_schema",
                key
            )));
        }
    }
    Ok(())
}

pub fn render_approval_message(tool_name: &str) -> String {
    render_approval_message_with_description(tool_name, None)
}

/// `description` is the exact prose the model will see. Approvers must see it
/// too, because a vendor writes it.
pub fn render_approval_message_with_description(
    tool_name: &str,
    description: Option<&str>,
) -> String {
    let base = format!(
        "Human approval required before executing tool '{tool_name}'. Review the requested arguments and approve, amend, or reject."
    );
    match description.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => format!("{base}\nTool description the model sees: {text}"),
        None => base,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn gate_responses_check_date_and_workspace_file_formats() {
        let schema = r#"{"type":"object","properties":{
            "when":{"type":"string","format":"date"},
            "doc":{"type":"string","format":"vam-workspace-file"},
            "cats":{"type":"array","items":{"enum":["a","b"]}}}}"#;
        assert!(validate_gate_response(
            r#"{"when":"2026-12-01","doc":"uploads/w9.pdf","cats":["a"]}"#,
            schema
        )
        .is_ok());
        for bad in [
            r#"{"when":"2026-02-30"}"#,
            r#"{"when":"tomorrow"}"#,
            r#"{"doc":"../secret"}"#,
            r#"{"doc":"/etc/passwd"}"#,
            r#"{"cats":["c"]}"#,
        ] {
            assert!(validate_gate_response(bad, schema).is_err(), "{bad}");
        }
    }

    use super::*;

    #[test]
    fn approval_message_shows_the_model_description() {
        let message = render_approval_message_with_description(
            "linear__create_issue",
            Some("Create a Linear issue"),
        );
        assert!(message.contains("linear__create_issue"));
        assert!(message.contains("Tool description the model sees: Create a Linear issue"));
        assert!(!render_approval_message("read_file").contains("Tool description"));
    }

    #[test]
    fn empty_amendable_schema_rejects_amend() {
        let err = validate_amended_args(&serde_json::json!({ "a": "b" }), &serde_json::json!({}))
            .expect_err("empty schema");
        assert!(err.to_string().contains("empty amendable_schema"));
    }

    #[test]
    fn amended_key_must_be_in_schema() {
        let schema = default_amendable_schema(&serde_json::json!({ "title": "x" }));
        validate_amended_args(&serde_json::json!({ "title": "ok" }), &schema).expect("valid");
        let err = validate_amended_args(&serde_json::json!({ "extra": "nope" }), &schema)
            .expect_err("unknown key");
        assert!(err.to_string().contains("extra"));
    }

    #[test]
    fn default_approver_is_run_owner() {
        let selector = HitlApprover::owner("alice");
        assert_eq!(
            selector,
            HitlApprover::Owner {
                user_id: "alice".to_string()
            }
        );
        assert!(selector.allows_decider("alice"));
        assert!(!selector.allows_decider("bob"));
        assert_eq!(HitlApprover::parse(""), HitlApprover::owner("local"));
        assert_eq!(HitlApprover::parse("alice"), HitlApprover::owner("alice"));
    }

    #[test]
    fn foreign_caller_cannot_decide_or_read_requested_args() {
        let store = SqliteWorkflowStore::in_memory().expect("store");
        store.create_user("bob", "bob").expect("bob");
        store.create_user("alice", "alice").expect("alice");
        store
            .persist_run_spec_with_owner(
                "urn:vam:run:local.vam.dev/acme/run-bob",
                "{}",
                None,
                "agent",
                Some("k-bob-hitl"),
                "User::\"bob\"",
                "bob",
            )
            .expect("run");
        store
            .record_published_approval(
                "appr-bob",
                "urn:vam:run:local.vam.dev/acme/run-bob",
                "bob",
                r#"{"title":"bob-secret"}"#,
            )
            .expect("publish");
        let listed = store
            .list_published_approvals_for_owner("alice")
            .expect("alice list");
        assert!(listed.is_empty());
        assert!(store.get_approval_for_owner("appr-bob", "alice").is_err());
        let err = store
            .decide_approval(
                &vam_spec::v1::ResourceId {
                    urn: "appr-bob".to_string(),
                },
                &vam_spec::v1::ResourceId {
                    urn: "urn:vam:token:local.vam.dev/acme/t1".to_string(),
                },
                "APPROVED",
                "User::\"alice\"",
                "alice",
                None,
                None,
                None,
                None,
            )
            .expect_err("alice must not decide bob");
        assert!(matches!(err, OrchestratorError::ApprovalNotFound(_)));
    }

    #[test]
    fn reviewer_cedar_principal_is_typed_seam_only() {
        let reviewer = HitlApprover::reviewer("finance");
        assert_eq!(
            reviewer,
            HitlApprover::Reviewer {
                principal: "Reviewer::\"finance\"".to_string()
            }
        );
        assert!(!reviewer.allows_decider("alice"));
        assert!(!reviewer.allows_decider("finance"));
        assert_eq!(
            HitlApprover::parse("Reviewer::\"finance\""),
            HitlApprover::reviewer("finance")
        );
        assert!(crate::users::UserRole::parse("reviewer")
            .expect_err("HITL reviewer is not a user role")
            .to_string()
            .contains("role must be"));
    }
}
