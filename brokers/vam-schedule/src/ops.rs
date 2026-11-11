//! The tools' logic: `schedule_create`, `schedule_list`, `schedule_get` and `schedule_update`.
//!
//! Pure over the [`Store`], a [`Clock`] and a wake handle, so the unit tests need no gRPC. The
//! caller's identity (`CallerContext`) and the grant (`RunGrant`, set by `vamd` only for a call
//! that carried a valid `run`) arrive as typed values; the arguments arrive as JSON.
//!
//! What the broker never does: read the token into an answer or a log, trust its own reading of
//! `run` (the target labels come from `RunGrant.target`), or accept an identity from the
//! arguments.

use std::str::FromStr;
use std::time::Duration as StdDuration;

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use vam_spec::v1::{OnBusy, RunGrant, VamErrorCode};

use crate::clock::Clock;
use crate::grant::GrantToken;
use crate::store::{Detail, Edit, FireRow, Inserted, NewSchedule, PendingRevoke, Rebind, Store};
use crate::tools;
use crate::when::{OnceWhen, When};

/// Keys that name an identity; the host stamps it, the model never supplies it.
const IDENTITY_KEYS: &[&str] = &[
    "user",
    "owner",
    "user_id",
    "agent_urn",
    "userId",
    "agentUrn",
    "ownerUserId",
];
const TITLE_MAX: usize = 200;
const PREVIEW_CHARS: usize = 200;
const DEFAULT_LIST_LIMIT: usize = 50;
const MAX_LIST_LIMIT: usize = 200;
const RECENT_FIRES: usize = 10;
const CLIENT_REQUEST_ID_MAX: usize = 128;

/// A typed tool error: the code, and a message that starts with the code's name so the model
/// (which only sees the message) can repair its call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolErr {
    pub code: VamErrorCode,
    pub message: String,
}

impl ToolErr {
    fn new(code: VamErrorCode, name: &str, detail: impl AsRef<str>) -> Self {
        Self {
            code,
            message: format!("{name}: {}", detail.as_ref()),
        }
    }
    pub fn invalid(detail: impl AsRef<str>) -> Self {
        Self::new(VamErrorCode::InvalidArgument, "INVALID_ARGUMENT", detail)
    }
    pub fn not_found() -> Self {
        Self::new(VamErrorCode::NotFound, "NOT_FOUND", "schedule not found")
    }
    pub fn precondition(detail: impl AsRef<str>) -> Self {
        Self::new(
            VamErrorCode::FailedPrecondition,
            "FAILED_PRECONDITION",
            detail,
        )
    }
    /// A store failure. The detail is logged without the arguments; the model gets a fixed text.
    pub fn internal(err: impl std::fmt::Display) -> Self {
        tracing::warn!(error = %err, "schedule tool: store error");
        Self::new(
            VamErrorCode::Internal,
            "INTERNAL",
            "the schedule store failed; try again",
        )
    }
}

impl From<crate::error::ScheduleError> for ToolErr {
    fn from(e: crate::error::ScheduleError) -> Self {
        Self::internal(e)
    }
}

/// Who is calling (host-stamped `CallerContext`).
#[derive(Debug, Clone)]
pub struct Caller {
    pub user_id: String,
    pub agent_urn: String,
}

/// What the tools need from the broker.
pub struct Ctx<'a> {
    pub store: &'a Store,
    pub clock: &'a dyn Clock,
    pub min_interval: StdDuration,
    pub wake: &'a Notify,
}

type Res = Result<Value, ToolErr>;

// ---- JSON <-> Struct --------------------------------------------------------------------

/// Convert a protobuf `Struct` to JSON.
pub fn struct_to_json(s: &prost_types::Struct) -> Value {
    Value::Object(
        s.fields
            .iter()
            .map(|(k, v)| (k.clone(), value_to_json(v)))
            .collect(),
    )
}

fn value_to_json(v: &prost_types::Value) -> Value {
    use prost_types::value::Kind;
    match &v.kind {
        None | Some(Kind::NullValue(_)) => Value::Null,
        Some(Kind::NumberValue(n)) => serde_json::Number::from_f64(*n)
            .map(Value::Number)
            .unwrap_or(Value::Null),
        Some(Kind::StringValue(s)) => Value::String(s.clone()),
        Some(Kind::BoolValue(b)) => Value::Bool(*b),
        Some(Kind::StructValue(s)) => struct_to_json(s),
        Some(Kind::ListValue(l)) => Value::Array(l.values.iter().map(value_to_json).collect()),
    }
}

/// Convert JSON to a protobuf `Struct` (a non-object gives an empty one).
pub fn json_to_struct(v: &Value) -> prost_types::Struct {
    match v {
        Value::Object(map) => prost_types::Struct {
            fields: map
                .iter()
                .map(|(k, v)| (k.clone(), json_to_value(v)))
                .collect(),
        },
        _ => prost_types::Struct::default(),
    }
}

fn json_to_value(v: &Value) -> prost_types::Value {
    use prost_types::value::Kind;
    prost_types::Value {
        kind: Some(match v {
            Value::Null => Kind::NullValue(0),
            Value::Bool(b) => Kind::BoolValue(*b),
            Value::Number(n) => Kind::NumberValue(n.as_f64().unwrap_or(0.0)),
            Value::String(s) => Kind::StringValue(s.clone()),
            Value::Array(a) => Kind::ListValue(prost_types::ListValue {
                values: a.iter().map(json_to_value).collect(),
            }),
            Value::Object(_) => Kind::StructValue(json_to_struct(v)),
        }),
    }
}

// ---- argument hygiene -------------------------------------------------------------------

/// Refuse identity-shaped keys and every key the tool does not declare.
pub fn check_keys(tool: &str, args: &Map<String, Value>) -> Result<(), ToolErr> {
    for k in args.keys() {
        if IDENTITY_KEYS.contains(&k.as_str()) {
            return Err(ToolErr::invalid(format!(
                "`{k}` is not an argument: who you act for is set by the host"
            )));
        }
    }
    let allowed = tools::allowed_keys(tool);
    for k in args.keys() {
        if !allowed.contains(&k.as_str()) {
            return Err(ToolErr::invalid(format!(
                "unknown argument `{k}` (allowed: {})",
                allowed.join(", ")
            )));
        }
    }
    Ok(())
}

fn present<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a Value> {
    args.get(key).filter(|v| !v.is_null())
}

fn str_arg<'a>(args: &'a Map<String, Value>, key: &str) -> Result<Option<&'a str>, ToolErr> {
    match present(args, key) {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.as_str())),
        Some(_) => Err(ToolErr::invalid(format!("`{key}` must be a string"))),
    }
}

fn title_arg(args: &Map<String, Value>) -> Result<Option<String>, ToolErr> {
    let Some(t) = str_arg(args, "title")? else {
        return Ok(None);
    };
    let t = t.trim();
    if t.is_empty() {
        return Err(ToolErr::invalid("`title` must not be empty"));
    }
    if t.chars().count() > TITLE_MAX {
        return Err(ToolErr::invalid(format!(
            "`title` is at most {TITLE_MAX} characters"
        )));
    }
    Ok(Some(t.to_string()))
}

fn catch_up_arg(args: &Map<String, Value>) -> Result<Option<&'static str>, ToolErr> {
    match str_arg(args, "catchUp")? {
        None => Ok(None),
        Some(s) if s.eq_ignore_ascii_case("latest") => Ok(Some("latest")),
        Some(s) if s.eq_ignore_ascii_case("skip") => Ok(Some("skip")),
        Some(_) => Err(ToolErr::invalid("`catchUp` must be LATEST or SKIP")),
    }
}

fn run_arg(args: &Map<String, Value>) -> Result<Option<&Map<String, Value>>, ToolErr> {
    match present(args, "run") {
        None => Ok(None),
        Some(Value::Object(o)) => Ok(Some(o)),
        Some(_) => Err(ToolErr::invalid("`run` must be an object")),
    }
}

fn preview_of(run: &Map<String, Value>) -> String {
    run.get("message")
        .and_then(Value::as_str)
        .map(|m| m.chars().take(PREVIEW_CHARS).collect())
        .unwrap_or_default()
}

// ---- time -------------------------------------------------------------------------------

pub(crate) fn rfc(secs: i64) -> String {
    Utc.timestamp_opt(secs, 0)
        .single()
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_default()
}

fn local_label(secs: i64, zone: &str) -> String {
    let tz = Tz::from_str(zone).unwrap_or(Tz::UTC);
    match Utc.timestamp_opt(secs, 0).single() {
        Some(t) => format!(
            "{} {}",
            t.with_timezone(&tz).format("%Y-%m-%d %H:%M"),
            tz.name()
        ),
        None => String::new(),
    }
}

/// A `when` as parsed and as stored.
struct Parsed {
    kind: &'static str,
    expression: String,
    timezone: String,
    first: DateTime<Utc>,
}

fn parse_when(ctx: &Ctx<'_>, v: &Value, now: DateTime<Utc>) -> Result<Parsed, ToolErr> {
    let when = When::parse(v).map_err(|e| ToolErr::invalid(e.to_string()))?;
    let first = when
        .first_fire(now)
        .map_err(|e| ToolErr::invalid(e.to_string()))?;
    when.check_floor(first, ctx.min_interval)
        .map_err(|e| ToolErr::invalid(e.to_string()))?;
    let (kind, expression, timezone) = match &when {
        When::Once(o) => {
            let tz = match o {
                OnceWhen::At { tz, .. } => tz.name().to_string(),
                _ => "UTC".to_string(),
            };
            ("once", first.to_rfc3339_opts(SecondsFormat::Secs, true), tz)
        }
        When::Cron { expression, tz, .. } => ("cron", expression.clone(), tz.name().to_string()),
    };
    Ok(Parsed {
        kind,
        expression,
        timezone,
        first,
    })
}

// ---- the grant --------------------------------------------------------------------------

struct GrantInfo {
    id: String,
    token: GrantToken,
    expires_at: i64,
    target_json: String,
}

fn label(name: &str, id: &str) -> String {
    match (name.is_empty(), id.is_empty()) {
        (false, false) => format!("{name} ({id})"),
        (false, true) => name.to_string(),
        _ => id.to_string(),
    }
}

fn grant_info(g: Option<&RunGrant>) -> Result<GrantInfo, ToolErr> {
    let g = g.ok_or_else(|| {
        ToolErr::precondition(
            "the host issued no grant for this call; pass `run` (what each fire runs)",
        )
    })?;
    if g.token.is_empty() || g.grant_id.is_empty() {
        return Err(ToolErr::precondition(
            "the grant the host issued is incomplete",
        ));
    }
    let expires_at = g
        .expires_at
        .as_ref()
        .map(|t| t.seconds)
        .ok_or_else(|| ToolErr::precondition("the grant the host issued has no expiry"))?;
    let t = g.target.clone().unwrap_or_default();
    let on_busy = match OnBusy::try_from(t.on_busy) {
        Ok(OnBusy::Skip) => "SKIP",
        _ => "QUEUE",
    };
    let target = json!({
        "agentUrn": t.agent_urn,
        "space": label(&t.space_name, &t.space_id),
        "thread": label(&t.thread_name, &t.thread_id),
        "onBusy": on_busy,
    });
    Ok(GrantInfo {
        id: g.grant_id.clone(),
        token: GrantToken::new(g.token.clone()),
        expires_at,
        target_json: target.to_string(),
    })
}

/// Q5: a grant that ends at or before the first fire could never start the run.
fn check_grant_outlives(expires_at: i64, first_fire: i64) -> Result<(), ToolErr> {
    if expires_at <= first_fire {
        return Err(ToolErr::invalid(format!(
            "the approval expires ({}) before the first fire ({}); set run.expiresAt later",
            rfc(expires_at),
            rfc(first_fire)
        )));
    }
    Ok(())
}

/// The schedule id of a grant: `sch_` + 26 hex characters of `sha256("vam.schedule.id\n" +
/// grant id)`. Derived from the grant (one schedule per grant, a retry derives the same id) but
/// not a substring of it: `vamd` redacts every run of 12 or more characters of the token from a
/// broker's answer, so an id built from the grant id's own characters reaches the model as
/// `sch***` (found in wave 4; the design's "grant id suffix" is amended).
pub fn schedule_id_of(grant_id: &str) -> Result<String, ToolErr> {
    let suffix = grant_id.strip_prefix("rg_").unwrap_or(grant_id);
    if suffix.is_empty() || !suffix.chars().all(|c| c.is_ascii_alphanumeric()) {
        return Err(ToolErr::precondition(
            "the grant id the host issued is malformed",
        ));
    }
    let digest = Sha256::digest(format!("vam.schedule.id\n{grant_id}").as_bytes());
    let hex: String = digest.iter().take(13).map(|b| format!("{b:02x}")).collect();
    Ok(format!("sch_{hex}"))
}

// ---- answers ----------------------------------------------------------------------------

fn when_json(d: &Detail) -> Value {
    if d.kind == "cron" {
        json!({"cron": {"expression": d.expression, "timezone": d.timezone}})
    } else {
        json!({"once": {"at": d.expression, "timezone": d.timezone}})
    }
}

fn item(d: &Detail, last_outcome: Option<String>) -> Map<String, Value> {
    let mut m = Map::new();
    m.insert("scheduleId".into(), json!(d.schedule_id));
    m.insert("title".into(), json!(d.title));
    m.insert("when".into(), when_json(d));
    m.insert("enabled".into(), json!(d.enabled));
    m.insert("state".into(), json!(d.state));
    if let Some(r) = &d.state_reason {
        m.insert("stateReason".into(), json!(r));
    }
    if let Some(n) = d.next_fire_at {
        m.insert("nextFireAt".into(), json!(rfc(n)));
        m.insert("nextFireLocal".into(), json!(local_label(n, &d.timezone)));
    }
    m.insert("expiresAt".into(), json!(rfc(d.grant_expires_at)));
    m.insert(
        "target".into(),
        serde_json::from_str(&d.target_json).unwrap_or(Value::Null),
    );
    m.insert(
        "catchUp".into(),
        json!(if d.catch_up == "skip" {
            "SKIP"
        } else {
            "LATEST"
        }),
    );
    if let Some(o) = last_outcome {
        m.insert("lastOutcome".into(), json!(o));
    }
    m
}

fn last_outcome(store: &Store, id: &str) -> Result<Option<String>, ToolErr> {
    Ok(store
        .recent_fires(id, 5)?
        .into_iter()
        .find_map(|f| f.outcome.filter(|o| o != "superseded")))
}

fn fire_json(f: &FireRow) -> Value {
    let mut m = Map::new();
    m.insert("fireAt".into(), json!(rfc(f.fire_at)));
    m.insert("kind".into(), json!(f.kind));
    m.insert(
        "outcome".into(),
        json!(f.outcome.clone().unwrap_or_else(|| "pending".into())),
    );
    if let Some(r) = &f.run_id {
        m.insert("runId".into(), json!(r));
    }
    m.insert("missedCount".into(), json!(f.missed_count));
    m.insert("attempts".into(), json!(f.attempts));
    Value::Object(m)
}

// ---- schedule_create --------------------------------------------------------------------

pub fn create(
    ctx: &Ctx<'_>,
    caller: &Caller,
    args: &Map<String, Value>,
    grant: Option<&RunGrant>,
) -> Res {
    check_keys(tools::SCHEDULE_CREATE, args)?;
    let title = title_arg(args)?.ok_or_else(|| ToolErr::invalid("`title` is required"))?;
    let when_v = present(args, "when").ok_or_else(|| ToolErr::invalid("`when` is required"))?;
    let run = run_arg(args)?.ok_or_else(|| ToolErr::invalid("`run` is required"))?;
    let catch_up = catch_up_arg(args)?.unwrap_or("latest");
    let crid = str_arg(args, "clientRequestId")?
        .map(str::trim)
        .filter(|s| !s.is_empty());
    if crid.is_some_and(|c| c.chars().count() > CLIENT_REQUEST_ID_MAX) {
        return Err(ToolErr::invalid(format!(
            "`clientRequestId` is at most {CLIENT_REQUEST_ID_MAX} characters"
        )));
    }
    let g = grant_info(grant)?;
    let now = ctx.clock.now();
    let now_s = now.timestamp();

    // Q12: a retried create answers the schedule it made, and gives the extra grant up.
    if let Some(c) = crid {
        if let Some(existing) =
            ctx.store
                .find_by_client_request(&caller.user_id, &caller.agent_urn, c)?
        {
            return duplicate(ctx, &existing, &g, now_s);
        }
    }

    let p = parse_when(ctx, when_v, now)?;
    check_grant_outlives(g.expires_at, p.first.timestamp())?;
    let schedule_id = schedule_id_of(&g.id)?;

    let new = NewSchedule {
        schedule_id: schedule_id.clone(),
        grant_id: g.id.clone(),
        token: g.token.clone(),
        grant_expires_at: g.expires_at,
        target_json: g.target_json.clone(),
        message_preview: preview_of(run),
        owner_user_id: caller.user_id.clone(),
        creator_agent_urn: caller.agent_urn.clone(),
        title,
        kind: p.kind.to_string(),
        expression: p.expression.clone(),
        timezone: p.timezone.clone(),
        catch_up: catch_up.to_string(),
        next_fire_at: Some(p.first.timestamp()),
        now: now_s,
    };
    match ctx.store.insert_schedule_for_request(&new, crid)? {
        Inserted::New => {}
        Inserted::Existing(id) => {
            let existing = ctx
                .store
                .get_for_write(&id, &caller.user_id, &caller.agent_urn)?
                .ok_or_else(ToolErr::not_found)?;
            return duplicate(ctx, &existing, &g, now_s);
        }
    }
    ctx.wake.notify_one();
    tracing::info!(schedule_id = %schedule_id, kind = p.kind, "schedule created");
    let d = ctx
        .store
        .get_visible(&schedule_id, &caller.user_id, &caller.agent_urn)?
        .ok_or_else(ToolErr::not_found)?;
    Ok(Value::Object(item(&d, None)))
}

fn duplicate(ctx: &Ctx<'_>, existing: &Detail, extra: &GrantInfo, now: i64) -> Res {
    if extra.id != existing.grant_id {
        ctx.store.queue_revoke(&PendingRevoke {
            grant_id: extra.id.clone(),
            token: extra.token.clone(),
            reason: "duplicate create".into(),
            created_at: now,
            not_before: 0,
        })?;
        // Wake the loop so the drain does not wait out its sleep.
        ctx.wake.notify_one();
    }
    tracing::info!(schedule_id = %existing.schedule_id, "create repeated; existing schedule returned");
    let mut m = item(existing, last_outcome(ctx.store, &existing.schedule_id)?);
    m.insert("existing".into(), json!(true));
    Ok(Value::Object(m))
}

// ---- schedule_list / schedule_get -------------------------------------------------------

pub fn list(ctx: &Ctx<'_>, caller: &Caller, args: &Map<String, Value>) -> Res {
    check_keys(tools::SCHEDULE_LIST, args)?;
    let enabled_only = match present(args, "enabledOnly") {
        None => false,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err(ToolErr::invalid("`enabledOnly` must be a boolean")),
    };
    let limit = match present(args, "limit") {
        None => DEFAULT_LIST_LIMIT,
        Some(Value::Number(n)) => match n.as_f64() {
            Some(f) if f.fract() == 0.0 && f >= 1.0 && f <= MAX_LIST_LIMIT as f64 => f as usize,
            _ => {
                return Err(ToolErr::invalid(format!(
                    "`limit` must be a whole number from 1 to {MAX_LIST_LIMIT}"
                )))
            }
        },
        Some(_) => return Err(ToolErr::invalid("`limit` must be a number")),
    };
    let rows = ctx
        .store
        .list_visible(&caller.user_id, &caller.agent_urn, enabled_only, limit)?;
    let mut out = Vec::with_capacity(rows.len());
    for d in &rows {
        out.push(Value::Object(item(
            d,
            last_outcome(ctx.store, &d.schedule_id)?,
        )));
    }
    Ok(json!({ "schedules": out }))
}

pub(crate) fn schedule_id_arg(args: &Map<String, Value>) -> Result<String, ToolErr> {
    str_arg(args, "scheduleId")?
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolErr::invalid("`scheduleId` is required"))
}

pub fn get(ctx: &Ctx<'_>, caller: &Caller, args: &Map<String, Value>) -> Res {
    check_keys(tools::SCHEDULE_GET, args)?;
    let id = schedule_id_arg(args)?;
    let d = ctx
        .store
        .get_visible(&id, &caller.user_id, &caller.agent_urn)?
        .ok_or_else(ToolErr::not_found)?;
    let fires = ctx.store.recent_fires(&id, RECENT_FIRES)?;
    let mut m = item(
        &d,
        fires
            .iter()
            .find_map(|f| f.outcome.clone().filter(|o| o != "superseded")),
    );
    m.insert("message".into(), json!(d.message_preview));
    m.insert(
        "fires".into(),
        Value::Array(fires.iter().map(fire_json).collect()),
    );
    Ok(Value::Object(m))
}

// ---- schedule_update --------------------------------------------------------------------

pub fn update(
    ctx: &Ctx<'_>,
    caller: &Caller,
    args: &Map<String, Value>,
    grant: Option<&RunGrant>,
) -> Res {
    check_keys(tools::SCHEDULE_UPDATE, args)?;
    let id = schedule_id_arg(args)?;
    let title = title_arg(args)?;
    let catch_up = catch_up_arg(args)?;
    let enabled = match present(args, "enabled") {
        None => None,
        Some(Value::Bool(b)) => Some(*b),
        Some(_) => return Err(ToolErr::invalid("`enabled` must be a boolean")),
    };
    let when_v = present(args, "when");
    let run = run_arg(args)?;
    if title.is_none()
        && catch_up.is_none()
        && enabled.is_none()
        && when_v.is_none()
        && run.is_none()
    {
        return Err(ToolErr::invalid(
            "nothing to change: pass title, when, enabled, catchUp or run",
        ));
    }
    let g = match run {
        Some(_) => Some(grant_info(grant)?),
        None => None,
    };

    let row = ctx
        .store
        .get_for_write(&id, &caller.user_id, &caller.agent_urn)?
        .ok_or_else(ToolErr::not_found)?;
    let now = ctx.clock.now();
    let now_s = now.timestamp();

    // Which states may change. Completed and deleting are over. A revoked or expired grant
    // can only be replaced (a new `run`); a needs_attention one can also just be resumed.
    match row.state.as_str() {
        "active" | "needs_attention" => {}
        "revoked" | "expired" if g.is_some() => {}
        "revoked" | "expired" => {
            return Err(ToolErr::precondition(format!(
                "the approval is {}; pass `run` to replace it, or create a new schedule",
                row.state
            )))
        }
        other => {
            return Err(ToolErr::precondition(format!(
                "the schedule is {other}; create a new one"
            )))
        }
    }

    let mut e = Edit {
        schedule_id: row.schedule_id.clone(),
        expected_version: row.version,
        title: title.unwrap_or_else(|| row.title.clone()),
        kind: row.kind.clone(),
        expression: row.expression.clone(),
        timezone: row.timezone.clone(),
        catch_up: catch_up
            .map(str::to_string)
            .unwrap_or_else(|| row.catch_up.clone()),
        enabled: row.enabled,
        state: row.state.clone(),
        state_reason: row.state_reason.clone(),
        next_fire_at: row.next_fire_at,
        rebind: None,
        abandon_open_fire: false,
        now: now_s,
    };

    let reactivate = g.is_some() || enabled == Some(true);
    if reactivate && row.state != "active" {
        e.state = "active".into();
        e.state_reason = None;
        e.enabled = true;
    }
    if let Some(en) = enabled {
        e.enabled = en;
    }
    let resuming = e.enabled && (!row.enabled || row.state != "active");

    if let Some(v) = when_v {
        let p = parse_when(ctx, v, now)?;
        e.kind = p.kind.to_string();
        e.expression = p.expression;
        e.timezone = p.timezone;
        e.next_fire_at = Some(p.first.timestamp());
        e.abandon_open_fire = true;
    } else if resuming {
        // A paused or repaired cron does not catch up the time it was off; a one-off keeps its
        // time (it may still be inside its grace window) but must have one.
        if e.kind == "cron" {
            let w =
                When::parse(&json!({"cron": {"expression": e.expression, "timezone": e.timezone}}))
                    .map_err(|er| ToolErr::invalid(er.to_string()))?;
            let next = w
                .next_after(now)
                .map_err(|er| ToolErr::invalid(er.to_string()))?;
            e.next_fire_at = next.map(|t| t.timestamp());
        } else if e.next_fire_at.is_none() {
            return Err(ToolErr::precondition(
                "this one-off has no fire time left; pass `when` with a new time",
            ));
        }
    }

    let effective_expiry = g.as_ref().map_or(row.grant_expires_at, |g| g.expires_at);
    if let Some(n) = e.next_fire_at {
        if when_v.is_some() || g.is_some() {
            check_grant_outlives(effective_expiry, n)?;
        }
    }

    if let (Some(g), Some(run)) = (g, run) {
        e.abandon_open_fire = true;
        e.rebind = Some(Rebind {
            grant_id: g.id,
            token: g.token,
            grant_expires_at: g.expires_at,
            target_json: g.target_json,
            message_preview: preview_of(run),
            old: PendingRevoke {
                grant_id: row.grant_id.clone(),
                token: row.token.clone(),
                reason: "rebind".into(),
                created_at: now_s,
                not_before: 0,
            },
        });
    }

    if !ctx.store.apply_edit(&e)? {
        return Err(ToolErr::precondition(
            "the schedule changed while it was being updated; read it and try again",
        ));
    }
    ctx.wake.notify_one();
    tracing::info!(schedule_id = %row.schedule_id, rebind = e.rebind.is_some(), "schedule updated");
    let d = ctx
        .store
        .get_visible(&id, &caller.user_id, &caller.agent_urn)?
        .ok_or_else(ToolErr::not_found)?;
    Ok(Value::Object(item(&d, last_outcome(ctx.store, &id)?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::TestClock;
    use prost_types::Timestamp;
    use vam_spec::v1::GrantTarget;

    const NOW: &str = "2026-01-01T10:00:00Z";
    const SECRET: &str = "s3cretvalue";

    struct Rig {
        store: Store,
        clock: TestClock,
        wake: Notify,
    }

    fn rig() -> Rig {
        Rig {
            store: Store::open_in_memory().unwrap(),
            clock: TestClock::new(NOW.parse().unwrap()),
            wake: Notify::new(),
        }
    }

    impl Rig {
        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                store: &self.store,
                clock: &self.clock,
                min_interval: StdDuration::from_secs(900),
                wake: &self.wake,
            }
        }
    }

    /// The schedule id of the test grant `rg_<x>`.
    fn sid(x: &str) -> &'static str {
        Box::leak(schedule_id_of(&format!("rg_{x}")).unwrap().into_boxed_str())
    }

    fn caller(user: &str, agent: &str) -> Caller {
        Caller {
            user_id: user.into(),
            agent_urn: agent.into(),
        }
    }
    fn admin() -> Caller {
        caller("u_admin", "urn:vam:agent:a")
    }

    fn secs(s: &str) -> i64 {
        s.parse::<DateTime<Utc>>().unwrap().timestamp()
    }

    fn grant(id: &str, expires: &str) -> RunGrant {
        RunGrant {
            token: format!("vam_rg_rg_{id}.{SECRET}"),
            grant_id: format!("rg_{id}"),
            issued_at: Some(Timestamp {
                seconds: secs(NOW),
                nanos: 0,
            }),
            expires_at: Some(Timestamp {
                seconds: secs(expires),
                nanos: 0,
            }),
            target: Some(GrantTarget {
                agent_urn: "urn:vam:agent:a".into(),
                space_id: "sp_1".into(),
                space_name: "assistant".into(),
                thread_id: "th_1".into(),
                thread_name: "nightly".into(),
                on_busy: OnBusy::Skip as i32,
                ..Default::default()
            }),
        }
    }

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn create_args() -> Map<String, Value> {
        obj(json!({
            "title": "Nightly",
            "when": {"once": {"in": "PT1H"}},
            "run": {"message": "do the thing", "workspace": {"mode": "SAME"}}
        }))
    }

    fn make(r: &Rig, c: &Caller, id: &str) -> Value {
        create(
            &r.ctx(),
            c,
            &create_args(),
            Some(&grant(id, "2026-03-01T00:00:00Z")),
        )
        .unwrap()
    }

    #[test]
    fn create_stores_the_grant_and_answers_without_the_token() {
        let r = rig();
        let out = make(&r, &admin(), "ABC123");
        assert_eq!(out["scheduleId"], sid("ABC123"));
        assert_eq!(out["nextFireAt"], "2026-01-01T11:00:00Z");
        assert_eq!(out["state"], "active");
        assert_eq!(out["expiresAt"], "2026-03-01T00:00:00Z");
        // Labels come from the grant's target, not from the args.
        assert_eq!(out["target"]["space"], "assistant (sp_1)");
        assert_eq!(out["target"]["thread"], "nightly (th_1)");
        assert_eq!(out["target"]["onBusy"], "SKIP");
        assert!(!out.to_string().contains(SECRET));
        let row = r.store.get_schedule(sid("ABC123")).unwrap().unwrap();
        assert_eq!(row.grant_id, "rg_ABC123");
        assert_eq!(row.token.expose(), format!("vam_rg_rg_ABC123.{SECRET}"));
        assert_eq!(row.next_fire_at, Some(secs("2026-01-01T11:00:00Z")));
    }

    #[test]
    fn create_target_ignores_what_the_args_claim() {
        let r = rig();
        let mut a = create_args();
        a.insert(
            "run".into(),
            json!({"message": "m", "workspace": {"mode": "NEW", "name": "lie"}, "agentUrn": "urn:evil"}),
        );
        let out = create(
            &r.ctx(),
            &admin(),
            &a,
            Some(&grant("T1", "2026-03-01T00:00:00Z")),
        )
        .unwrap();
        assert_eq!(out["target"]["agentUrn"], "urn:vam:agent:a");
        assert_eq!(out["target"]["space"], "assistant (sp_1)");
    }

    #[test]
    fn create_without_a_grant_is_a_failed_precondition() {
        let r = rig();
        let e = create(&r.ctx(), &admin(), &create_args(), None).unwrap_err();
        assert_eq!(e.code, VamErrorCode::FailedPrecondition);
        assert!(e.message.starts_with("FAILED_PRECONDITION"));
    }

    #[test]
    fn create_refuses_a_bad_when_and_a_grant_that_ends_before_the_fire() {
        let r = rig();
        let g = grant("G1", "2026-03-01T00:00:00Z");
        let mut a = create_args();
        a.insert(
            "when".into(),
            json!({"once": {"at": "2025-01-01T00:00:00Z"}}),
        );
        let e = create(&r.ctx(), &admin(), &a, Some(&g)).unwrap_err();
        assert_eq!(e.code, VamErrorCode::InvalidArgument);
        // Q5: the grant expires at 10:30, the once fires at 11:00.
        let short = grant("G2", "2026-01-01T10:30:00Z");
        let e = create(&r.ctx(), &admin(), &create_args(), Some(&short)).unwrap_err();
        assert_eq!(e.code, VamErrorCode::InvalidArgument);
        assert!(e.message.contains("expires"), "{}", e.message);
        // Exactly at the fire time is still too late.
        let at = grant("G3", "2026-01-01T11:00:00Z");
        assert!(create(&r.ctx(), &admin(), &create_args(), Some(&at)).is_err());
        assert!(r
            .store
            .list_visible("u_admin", "urn:vam:agent:a", false, 10)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn create_refuses_a_cron_below_the_floor() {
        let r = rig();
        let mut a = create_args();
        a.insert(
            "when".into(),
            json!({"cron": {"expression": "*/5 * * * *", "timezone": "UTC"}}),
        );
        let e = create(
            &r.ctx(),
            &admin(),
            &a,
            Some(&grant("C1", "2026-03-01T00:00:00Z")),
        )
        .unwrap_err();
        assert_eq!(e.code, VamErrorCode::InvalidArgument);
    }

    #[test]
    fn hygiene_refuses_identity_and_unknown_keys_everywhere() {
        let r = rig();
        let g = grant("H1", "2026-03-01T00:00:00Z");
        for key in ["user", "owner", "user_id", "agent_urn", "bogus"] {
            let mut a = create_args();
            a.insert(key.into(), json!("x"));
            let e = create(&r.ctx(), &admin(), &a, Some(&g)).unwrap_err();
            assert_eq!(e.code, VamErrorCode::InvalidArgument, "{key}");
            let e = list(&r.ctx(), &admin(), &obj(json!({ key: "x" }))).unwrap_err();
            assert_eq!(e.code, VamErrorCode::InvalidArgument, "{key}");
            let e = get(
                &r.ctx(),
                &admin(),
                &obj(json!({"scheduleId": sid("x"), key: "x"})),
            )
            .unwrap_err();
            assert_eq!(e.code, VamErrorCode::InvalidArgument, "{key}");
            let e = update(
                &r.ctx(),
                &admin(),
                &obj(json!({"scheduleId": sid("x"), key: "x"})),
                None,
            )
            .unwrap_err();
            assert_eq!(e.code, VamErrorCode::InvalidArgument, "{key}");
        }
        let mut a = create_args();
        a.insert("run".into(), json!("not an object"));
        assert_eq!(
            create(&r.ctx(), &admin(), &a, Some(&g)).unwrap_err().code,
            VamErrorCode::InvalidArgument
        );
        let mut a = create_args();
        a.remove("title");
        assert_eq!(
            create(&r.ctx(), &admin(), &a, Some(&g)).unwrap_err().code,
            VamErrorCode::InvalidArgument
        );
    }

    #[test]
    fn a_retried_create_returns_the_first_and_queues_the_extra_grant() {
        let r = rig();
        let mut a = create_args();
        a.insert("clientRequestId".into(), json!("req-1"));
        let first = create(
            &r.ctx(),
            &admin(),
            &a,
            Some(&grant("D1", "2026-03-01T00:00:00Z")),
        )
        .unwrap();
        let second = create(
            &r.ctx(),
            &admin(),
            &a,
            Some(&grant("D2", "2026-03-01T00:00:00Z")),
        )
        .unwrap();
        assert_eq!(first["scheduleId"], second["scheduleId"]);
        assert_eq!(second["existing"], true);
        assert!(r.store.get_schedule(sid("D2")).unwrap().is_none());
        let due = r.store.due_revokes(i64::MAX).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].grant_id, "rg_D2");
        // The same id by another agent or user is a different schedule.
        let other = create(
            &r.ctx(),
            &caller("u_admin", "urn:vam:agent:b"),
            &a,
            Some(&grant("D3", "2026-03-01T00:00:00Z")),
        )
        .unwrap();
        assert_eq!(other["scheduleId"], sid("D3"));
    }

    #[test]
    fn list_is_owner_first_and_hides_the_message() {
        let r = rig();
        make(&r, &admin(), "L1");
        make(&r, &caller("u_member", "urn:vam:agent:a"), "L2");
        let mine = list(&r.ctx(), &admin(), &Map::new()).unwrap();
        let rows = mine["schedules"].as_array().unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["scheduleId"], sid("L1"));
        assert!(rows[0].get("message").is_none());
        assert!(!mine.to_string().contains("do the thing"));
        // Same user, another agent: not visible (visibility defaults to agent).
        let other_agent =
            list(&r.ctx(), &caller("u_admin", "urn:vam:agent:b"), &Map::new()).unwrap();
        assert!(other_agent.to_string().contains("\"schedules\":[]"));
        // A user with none gets exactly the empty list.
        let none = list(
            &r.ctx(),
            &caller("u_nobody", "urn:vam:agent:a"),
            &Map::new(),
        )
        .unwrap();
        assert_eq!(none.to_string(), "{\"schedules\":[]}");
    }

    #[test]
    fn get_is_owner_first_and_a_foreign_id_is_not_found() {
        let r = rig();
        make(&r, &admin(), "G9");
        let own = get(&r.ctx(), &admin(), &obj(json!({"scheduleId": sid("G9")}))).unwrap();
        assert_eq!(own["message"], "do the thing");
        assert_eq!(own["fires"], json!([]));
        for c in [
            caller("u_member", "urn:vam:agent:a"),
            caller("u_admin", "urn:vam:agent:b"),
        ] {
            let e = get(&r.ctx(), &c, &obj(json!({"scheduleId": sid("G9")}))).unwrap_err();
            assert_eq!(e.code, VamErrorCode::NotFound);
            assert!(e.message.starts_with("NOT_FOUND"));
        }
        let e = get(&r.ctx(), &admin(), &obj(json!({"scheduleId": sid("nope")}))).unwrap_err();
        assert_eq!(e.code, VamErrorCode::NotFound);
    }

    #[test]
    fn get_shows_the_fires_newest_first_capped_at_ten() {
        let r = rig();
        make(&r, &admin(), "F1");
        for i in 0..12 {
            r.store
                .begin_fire(sid("F1"), 1000 + i, "scheduled", &format!("k{i}"), None, 5)
                .unwrap();
        }
        r.store
            .settle(&crate::store::Settle {
                schedule_id: sid("F1").into(),
                fire: Some(crate::store::SettledFire {
                    fire_at: 1011,
                    outcome: "skipped_busy".into(),
                    run_id: None,
                }),
                missed: None,
                next_fire_at: None,
                state: "completed".into(),
                reason: None,
                enabled: false,
                revoke: None,
                now: 9,
                expected_version: 0,
            })
            .unwrap();
        let out = get(&r.ctx(), &admin(), &obj(json!({"scheduleId": sid("F1")}))).unwrap();
        let fires = out["fires"].as_array().unwrap();
        assert_eq!(fires.len(), 10);
        assert_eq!(fires[0]["outcome"], "skipped_busy");
        assert!(out.to_string().contains("\"outcome\":\"skipped_busy\""));
        assert_eq!(out["lastOutcome"], "skipped_busy");
    }

    #[test]
    fn update_changes_title_pause_catch_up_and_when() {
        let r = rig();
        make(&r, &admin(), "U1");
        let out = update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("U1"), "title": "Renamed", "catchUp": "skip", "enabled": false})),
            None,
        )
        .unwrap();
        assert_eq!(out["title"], "Renamed");
        assert_eq!(out["catchUp"], "SKIP");
        assert_eq!(out["enabled"], false);
        let out = update(
            &r.ctx(),
            &admin(),
            &obj(
                json!({"scheduleId": sid("U1"), "enabled": true, "when": {"once": {"in": "PT2H"}}}),
            ),
            None,
        )
        .unwrap();
        assert_eq!(out["nextFireAt"], "2026-01-01T12:00:00Z");
        assert_eq!(out["enabled"], true);
        let row = r
            .store
            .get_for_write(sid("U1"), "u_admin", "urn:vam:agent:a")
            .unwrap()
            .unwrap();
        assert_eq!(row.version, 2);
        // Nothing to change, and a past time, are refused.
        assert!(update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("U1")})),
            None
        )
        .is_err());
        let e = update(
            &r.ctx(),
            &admin(),
            &obj(
                json!({"scheduleId": sid("U1"), "when": {"once": {"at": "2020-01-01T00:00:00Z"}}}),
            ),
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, VamErrorCode::InvalidArgument);
    }

    #[test]
    fn update_is_for_the_owner_through_the_creating_agent_only() {
        let r = rig();
        make(&r, &admin(), "W1");
        let args = obj(json!({"scheduleId": sid("W1"), "title": "hijack"}));
        for c in [
            caller("u_member", "urn:vam:agent:a"),
            caller("u_admin", "urn:vam:agent:b"),
        ] {
            let e = update(&r.ctx(), &c, &args, None).unwrap_err();
            assert_eq!(e.code, VamErrorCode::NotFound);
        }
        let row = r.store.get_schedule(sid("W1")).unwrap().unwrap();
        assert_eq!(row.version, 0);
    }

    #[test]
    fn a_visible_to_user_schedule_is_readable_but_not_writable_by_another_agent() {
        let r = rig();
        make(&r, &admin(), "V1");
        r.store
            .conn_for_test()
            .execute("UPDATE schedules SET visibility = 'user'", [])
            .unwrap();
        let other = caller("u_admin", "urn:vam:agent:b");
        assert!(get(&r.ctx(), &other, &obj(json!({"scheduleId": sid("V1")}))).is_ok());
        assert_eq!(
            update(
                &r.ctx(),
                &other,
                &obj(json!({"scheduleId": sid("V1"), "title": "x"})),
                None
            )
            .unwrap_err()
            .code,
            VamErrorCode::NotFound
        );
    }

    #[test]
    fn update_with_run_rebinds_and_queues_the_old_token() {
        let r = rig();
        make(&r, &admin(), "R1");
        let new = grant("R2", "2026-03-01T00:00:00Z");
        let out = update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("R1"), "run": {"message": "new message"}})),
            Some(&new),
        )
        .unwrap();
        assert_eq!(out["scheduleId"], sid("R1"), "same row, same id");
        let row = r.store.get_schedule(sid("R1")).unwrap().unwrap();
        assert_eq!(row.grant_id, "rg_R2");
        let due = r.store.due_revokes(i64::MAX).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].grant_id, "rg_R1");
        assert_eq!(due[0].token.expose(), format!("vam_rg_rg_R1.{SECRET}"));
        let got = get(&r.ctx(), &admin(), &obj(json!({"scheduleId": sid("R1")}))).unwrap();
        assert_eq!(got["message"], "new message");
        // `run` without a grant is a failed precondition and changes nothing.
        let e = update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("R1"), "run": {"message": "x"}})),
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, VamErrorCode::FailedPrecondition);
    }

    #[test]
    fn a_rebind_repairs_a_revoked_schedule_and_resume_repairs_needs_attention() {
        let r = rig();
        let mut a = create_args();
        a.insert(
            "when".into(),
            json!({"cron": {"expression": "0 * * * *", "timezone": "UTC"}}),
        );
        create(
            &r.ctx(),
            &admin(),
            &a,
            Some(&grant("X1", "2026-03-01T00:00:00Z")),
        )
        .unwrap();
        let conn = r.store.conn_for_test();
        conn.execute(
            "UPDATE schedules SET state='revoked', enabled=0, next_fire_at=NULL, state_reason='revoked'",
            [],
        )
        .unwrap();
        drop(conn);
        // Without a new run: refused.
        let e = update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("X1"), "enabled": true})),
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, VamErrorCode::FailedPrecondition);
        // With a new run: active again, next fire recomputed.
        let out = update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("X1"), "run": {"message": "again"}})),
            Some(&grant("X2", "2026-03-01T00:00:00Z")),
        )
        .unwrap();
        assert_eq!(out["state"], "active");
        assert_eq!(out["enabled"], true);
        assert_eq!(out["nextFireAt"], "2026-01-01T11:00:00Z");
        // needs_attention: resume keeps the grant.
        r.store
            .conn_for_test()
            .execute(
                "UPDATE schedules SET state='needs_attention', enabled=0",
                [],
            )
            .unwrap();
        let out = update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("X1"), "enabled": true})),
            None,
        )
        .unwrap();
        assert_eq!(out["state"], "active");
        assert_eq!(
            r.store.get_schedule(sid("X1")).unwrap().unwrap().grant_id,
            "rg_X2"
        );
    }

    #[test]
    fn completed_schedules_cannot_be_updated() {
        let r = rig();
        make(&r, &admin(), "Z1");
        r.store
            .conn_for_test()
            .execute("UPDATE schedules SET state='completed', enabled=0", [])
            .unwrap();
        let e = update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("Z1"), "title": "t"})),
            None,
        )
        .unwrap_err();
        assert_eq!(e.code, VamErrorCode::FailedPrecondition);
    }

    #[test]
    fn the_token_is_never_in_any_answer() {
        let r = rig();
        let c = make(&r, &admin(), "S1");
        let l = list(&r.ctx(), &admin(), &Map::new()).unwrap();
        let g = get(&r.ctx(), &admin(), &obj(json!({"scheduleId": sid("S1")}))).unwrap();
        let u = update(
            &r.ctx(),
            &admin(),
            &obj(json!({"scheduleId": sid("S1"), "title": "n"})),
            None,
        )
        .unwrap();
        for v in [c, l, g, u] {
            let s = v.to_string();
            assert!(!s.contains(SECRET) && !s.contains("vam_rg_"), "{s}");
        }
    }

    #[test]
    fn json_struct_round_trip() {
        let v = json!({"a": [1.5, "x", true, null], "b": {"c": "d"}});
        assert_eq!(struct_to_json(&json_to_struct(&v)), v);
    }
}
