//! The fire loop (design/schedule-broker.md "The clock" and "Firing").
//!
//! One loop sleeps until the earliest due time (at most 30 s, woken early by [`Scheduler::wake_handle`]),
//! starts at most 4 fires at once, and records each fire's outcome and advances its schedule in one
//! transaction. Time comes from an injected [`Clock`]; sleeping happens only in [`Scheduler::run`],
//! so tests drive [`Scheduler::run_pass`] with a `TestClock`.
//!
//! Crash safety: the ledger row (with the idempotency key) is written before the gateway is
//! called and `next_fire_at` moves only in the settling transaction. A fire that reached `vamd`
//! before a crash is therefore retried with the same key and answered DUPLICATE.
//!
//! Logs carry schedule ids and outcome names only; never the token, the message or the args.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use serde_json::json;
use tokio::sync::{watch, Notify};
use tokio::task::JoinSet;
use vam_spec::v1::{BusyReason, DenyReason};

use crate::clock::Clock;
use crate::error::ScheduleError;
use crate::gateway::{FireKind, FireOutcome, FireRequest, GatewayClient, GatewayError};
use crate::grant::GrantToken;
use crate::idempotency::{idempotency_key, KeyKind};
use crate::store::{
    MissedRun, PendingRevoke, ScheduleRow, Settle, SettledFire, Store, OUTCOME_BUSY_RETRY,
};
use crate::when::{When, ONCE_GRACE};

/// Fires in flight at once.
pub const MAX_IN_FLIGHT: usize = 4;
/// The longest the loop sleeps.
pub const IDLE_CAP_SECS: i64 = 30;
/// How often queued revokes are retried.
pub const REVOKE_EVERY_SECS: i64 = 60;
/// A single due time this late or less is a normal fire, not a catch-up (two idle caps).
pub const ON_TIME_SLACK_SECS: i64 = 60;
/// First backoff step and its cap.
pub const BACKOFF_BASE_MS: u64 = 2_000;
pub const BACKOFF_CAP_MS: u64 = 300_000;
/// `not_after` of a one-off fire.
pub const ONCE_NOT_AFTER_SECS: i64 = 86_400;
const WALK_CAP: usize = 200_000;
const KIND_SCHEDULED: &str = "scheduled";

/// What the loop needs from the trigger gateway. [`GatewayClient`] is the real one.
#[async_trait]
pub trait Gateway: Send + Sync {
    async fn fire(&self, req: &FireRequest) -> Result<FireOutcome, GatewayError>;
    async fn revoke(&self, token: &GrantToken) -> Result<(), GatewayError>;
}

#[async_trait]
impl Gateway for GatewayClient {
    async fn fire(&self, req: &FireRequest) -> Result<FireOutcome, GatewayError> {
        GatewayClient::fire(self, req).await
    }
    async fn revoke(&self, token: &GrantToken) -> Result<(), GatewayError> {
        GatewayClient::revoke(self, token).await
    }
}

/// Full-jitter exponential backoff: uniform in `[0, min(cap, base * 2^(attempt-1)))`, never less
/// than `retry_after_ms`. `unit` is a random number in `[0, 1)`; `attempt` counts from 1.
pub fn backoff_delay_ms(attempt: i64, retry_after_ms: u32, unit: f64) -> u64 {
    let exp = (attempt.max(1) - 1).min(20) as u32;
    let ceiling = BACKOFF_BASE_MS
        .saturating_mul(1u64 << exp)
        .min(BACKOFF_CAP_MS);
    let jittered = (ceiling as f64 * unit.clamp(0.0, 1.0)) as u64;
    jittered.max(u64::from(retry_after_ms))
}

fn dt(secs: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(secs, 0)
        .single()
        .unwrap_or(DateTime::<Utc>::UNIX_EPOCH)
}

/// The schedule's `When`, for a cron row; `None` for a one-off.
fn build_when(row: &ScheduleRow) -> Result<Option<When>, String> {
    if row.kind != "cron" {
        return Ok(None);
    }
    When::parse(&json!({"cron": {"expression": row.expression, "timezone": row.timezone}}))
        .map(Some)
        .map_err(|e| e.to_string())
}

/// The time after `fire_at` this schedule fires next; `None` for a one-off.
fn advance(when: Option<&When>, fire_at: i64) -> Option<i64> {
    when.and_then(|w| w.next_after(dt(fire_at)).ok().flatten())
        .map(|t| t.timestamp())
}

/// `not_after`: the next nominal time for cron, `fire_at` + 24 h for a one-off.
fn not_after(when: Option<&When>, fire_at: i64) -> i64 {
    advance(when, fire_at).unwrap_or(fire_at + ONCE_NOT_AFTER_SECS)
}

#[derive(Debug, PartialEq, Eq)]
enum Plan {
    /// Fire `fire_at`; older missed times (if any) are folded into one `missed` row.
    Fire {
        fire_at: i64,
        folded: Option<MissedRun>,
    },
    /// Nothing is fired; all `count` due times are one `missed` row. `latest` is the newest.
    Missed { first: i64, count: i64, latest: i64 },
}

/// Catch-up (design "The clock"): decide what to do with a schedule whose time `first` has come.
fn plan(row: &ScheduleRow, when: Option<&When>, first: i64, now: i64) -> Plan {
    let mut latest = first;
    let mut count = 1i64;
    if let Some(w) = when {
        let mut walked = 0;
        while let Ok(Some(n)) = w.next_after(dt(latest)) {
            if n.timestamp() > now || walked >= WALK_CAP {
                break;
            }
            latest = n.timestamp();
            count += 1;
            walked += 1;
        }
    }
    let grace = match when {
        Some(w) => w.grace_window(dt(latest)),
        None => ONCE_GRACE,
    }
    .num_seconds();
    let late = now - latest;
    if count == 1 && late <= ON_TIME_SLACK_SECS {
        Plan::Fire {
            fire_at: latest,
            folded: None,
        }
    } else if row.catch_up == "latest" && late <= grace {
        Plan::Fire {
            fire_at: latest,
            folded: (count > 1).then_some(MissedRun {
                first_fire_at: first,
                count: count - 1,
            }),
        }
    } else {
        Plan::Missed {
            first,
            count,
            latest,
        }
    }
}

/// How a fire ended, before it is turned into a [`Settle`].
enum End {
    Delivered {
        outcome: &'static str,
        run_id: String,
        /// QUEUED or DUPLICATE: the run may still be waiting, so the grant outlives `not_after`.
        keep_grant_until_not_after: bool,
    },
    SkippedBusy,
    GaveUp {
        outcome: &'static str,
    },
    Denied(DenyReason),
}

enum Answer {
    End(End),
    Retry { after_ms: u32, busy: bool },
}

fn classify(answer: Result<FireOutcome, GatewayError>) -> Answer {
    match answer {
        Ok(FireOutcome::Accepted { run_id }) => Answer::End(End::Delivered {
            outcome: "accepted",
            run_id,
            keep_grant_until_not_after: false,
        }),
        Ok(FireOutcome::Queued { run_id }) => Answer::End(End::Delivered {
            outcome: "queued",
            run_id,
            keep_grant_until_not_after: true,
        }),
        // A DUPLICATE may be the earlier QUEUED fire seen again after a crash: treat it like QUEUED.
        Ok(FireOutcome::Duplicate { run_id }) => Answer::End(End::Delivered {
            outcome: "duplicate",
            run_id,
            keep_grant_until_not_after: true,
        }),
        Ok(FireOutcome::Busy {
            reason: BusyReason::Skipped | BusyReason::AlreadyQueued,
            ..
        }) => Answer::End(End::SkippedBusy),
        Ok(FireOutcome::Busy { retry_after_ms, .. }) => Answer::Retry {
            after_ms: retry_after_ms,
            busy: true,
        },
        Ok(FireOutcome::RateLimited { retry_after_ms })
        | Ok(FireOutcome::Unavailable { retry_after_ms }) => Answer::Retry {
            after_ms: retry_after_ms,
            busy: false,
        },
        Ok(FireOutcome::Denied { reason }) => Answer::End(End::Denied(reason)),
        Err(_) => Answer::Retry {
            after_ms: 0,
            busy: false,
        },
    }
}

fn deny_word(r: DenyReason) -> String {
    r.as_str_name()
        .trim_start_matches("DENY_REASON_")
        .to_ascii_lowercase()
}

type Jitter = Arc<dyn Fn() -> f64 + Send + Sync>;

/// The fire loop. Cheap to clone; clones share the store, gateway, clock and wake handle.
#[derive(Clone)]
pub struct Scheduler {
    store: Arc<Store>,
    gateway: Arc<dyn Gateway>,
    clock: Arc<dyn Clock>,
    wake: Arc<Notify>,
    jitter: Jitter,
}

impl Scheduler {
    pub fn new(store: Arc<Store>, gateway: Arc<dyn Gateway>, clock: Arc<dyn Clock>) -> Self {
        Self {
            store,
            gateway,
            clock,
            wake: Arc::new(Notify::new()),
            jitter: Arc::new(rand::random::<f64>),
        }
    }

    /// Replace the jitter source (tests).
    pub fn with_jitter(mut self, j: impl Fn() -> f64 + Send + Sync + 'static) -> Self {
        self.jitter = Arc::new(j);
        self
    }

    /// Notify this after a create, update or delete so the loop re-reads the earliest time.
    pub fn wake_handle(&self) -> Arc<Notify> {
        self.wake.clone()
    }

    fn now(&self) -> i64 {
        self.clock.now().timestamp()
    }

    /// Run until `shutdown` changes.
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        let mut set: JoinSet<(String, bool)> = JoinSet::new();
        let mut inflight: HashSet<String> = HashSet::new();
        let mut last_revoke = i64::MIN;
        loop {
            let now = self.now();
            if now.saturating_sub(last_revoke) >= REVOKE_EVERY_SECS {
                self.revoke_pass().await;
                last_revoke = now;
            }
            if let Err(e) = self.dispatch_due(&mut set, &mut inflight) {
                tracing::warn!(error = %e, "fire loop: selecting due schedules failed");
            }
            let excluded: Vec<String> = inflight.iter().cloned().collect();
            let wait = if inflight.len() >= MAX_IN_FLIGHT {
                IDLE_CAP_SECS
            } else {
                match self.store.next_wake(&excluded) {
                    Ok(Some(t)) => (t - now).clamp(0, IDLE_CAP_SECS),
                    Ok(None) => IDLE_CAP_SECS,
                    Err(e) => {
                        tracing::warn!(error = %e, "fire loop: reading the next wake failed");
                        1
                    }
                }
            };
            tokio::select! {
                Some(done) = set.join_next(), if !set.is_empty() => {
                    if let Ok((id, ok)) = done {
                        inflight.remove(&id);
                        if !ok {
                            // A store error: do not spin on the same row.
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(wait.max(0) as u64)) => {}
                _ = shutdown.changed() => break,
            }
        }
        set.abort_all();
    }

    /// One pass for tests: start every due fire (4 at a time) and wait for them all.
    pub async fn run_pass(&self) {
        let mut set: JoinSet<(String, bool)> = JoinSet::new();
        let mut inflight: HashSet<String> = HashSet::new();
        loop {
            if let Err(e) = self.dispatch_due(&mut set, &mut inflight) {
                tracing::warn!(error = %e, "fire loop: selecting due schedules failed");
            }
            match set.join_next().await {
                Some(Ok((id, _))) => {
                    inflight.remove(&id);
                }
                Some(Err(_)) => {}
                None => break,
            }
        }
    }

    fn dispatch_due(
        &self,
        set: &mut JoinSet<(String, bool)>,
        inflight: &mut HashSet<String>,
    ) -> Result<(), ScheduleError> {
        let room = MAX_IN_FLIGHT.saturating_sub(inflight.len());
        if room == 0 {
            return Ok(());
        }
        let due = self
            .store
            .due_schedules(self.now(), room + inflight.len())?;
        let fresh: Vec<ScheduleRow> = due
            .into_iter()
            .filter(|r| !inflight.contains(&r.schedule_id))
            .take(room)
            .collect();
        for row in fresh {
            let id = row.schedule_id.clone();
            inflight.insert(id.clone());
            let this = self.clone();
            set.spawn(async move {
                let ok = match this.fire_one(row).await {
                    Ok(()) => true,
                    Err(e) => {
                        tracing::warn!(schedule_id = %id, error = %e, "fire failed to record");
                        false
                    }
                };
                (id, ok)
            });
        }
        Ok(())
    }

    /// Try every queued revoke whose `not_before` has passed; a gateway answer removes it (and a
    /// `deleting` schedule waiting on it).
    pub async fn revoke_pass(&self) {
        let due = match self.store.due_revokes(self.now()) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!(error = %e, "fire loop: reading queued revokes failed");
                return;
            }
        };
        for r in due {
            self.try_revoke(&r).await;
        }
    }

    async fn try_revoke(&self, r: &PendingRevoke) {
        if self.gateway.revoke(&r.token).await.is_ok() {
            if let Err(e) = self.store.finish_revoke_purge(&r.grant_id) {
                tracing::warn!(grant_id = %r.grant_id, error = %e, "recording a revoke failed");
            }
        }
    }

    async fn fire_one(&self, row: ScheduleRow) -> Result<(), ScheduleError> {
        let now = self.now();
        let when = match build_when(&row) {
            Ok(w) => w,
            Err(reason) => {
                tracing::warn!(schedule_id = %row.schedule_id, "schedule cannot be read: {reason}");
                return self
                    .settle_row(
                        &row,
                        Settle {
                            schedule_id: row.schedule_id.clone(),
                            fire: None,
                            missed: None,
                            next_fire_at: None,
                            state: "needs_attention".into(),
                            reason: Some("the schedule's time cannot be read".into()),
                            enabled: false,
                            revoke: None,
                            now,
                            expected_version: row.version,
                        },
                    )
                    .await;
            }
        };
        let when = when.as_ref();

        // A fire begun before (a retry, or a crash) is resumed with its own time and key.
        let open = self.store.open_fire(&row.schedule_id)?;
        let (fire_at, folded, prior_attempts, last_busy) = match open {
            Some(o) => (
                o.fire_at,
                None,
                o.attempts,
                o.outcome.as_deref() == Some(OUTCOME_BUSY_RETRY),
            ),
            None => {
                let Some(first) = row.next_fire_at else {
                    return Ok(());
                };
                match plan(&row, when, first, now) {
                    Plan::Missed {
                        first,
                        count,
                        latest,
                    } => {
                        return self
                            .settle_missed(
                                &row,
                                when,
                                MissedRun {
                                    first_fire_at: first,
                                    count,
                                },
                                latest,
                                now,
                            )
                            .await;
                    }
                    Plan::Fire { fire_at, folded } => (fire_at, folded, 0, false),
                }
            }
        };
        let na = not_after(when, fire_at);
        if prior_attempts > 0 && now >= na {
            let outcome = if last_busy {
                "skipped_busy"
            } else {
                "failed_unavailable"
            };
            return self
                .end_fire(&row, when, fire_at, na, now, End::GaveUp { outcome })
                .await;
        }

        let key = idempotency_key(&row.schedule_id, fire_at, KeyKind::Scheduled);
        let attempts =
            self.store
                .begin_fire(&row.schedule_id, fire_at, KIND_SCHEDULED, &key, folded, now)?;
        let req = FireRequest {
            token: row.token.clone(),
            idempotency_key: key,
            fire_at,
            trigger_ref: row.schedule_id.clone(),
            kind: FireKind::Scheduled,
            not_after: Some(na),
        };
        let answer = self.gateway.fire(&req).await;
        let now = self.now();
        match classify(answer) {
            Answer::End(end) => self.end_fire(&row, when, fire_at, na, now, end).await,
            Answer::Retry { after_ms, busy } => {
                let delay = backoff_delay_ms(attempts, after_ms, (self.jitter)());
                // At least a second, so a zero jitter never spins.
                let until = (now + ((delay as i64 + 999) / 1000).max(1)).min(na);
                self.store.schedule_retry(
                    &row.schedule_id,
                    fire_at,
                    until,
                    attempts,
                    busy.then_some(OUTCOME_BUSY_RETRY),
                    now,
                    row.version,
                )?;
                tracing::info!(schedule_id = %row.schedule_id, attempts, backoff_until = until, "fire will be retried");
                Ok(())
            }
        }
    }

    /// The whole backlog was missed (or `catch_up = skip`): one `missed` row, then advance.
    async fn settle_missed(
        &self,
        row: &ScheduleRow,
        when: Option<&When>,
        missed: MissedRun,
        latest: i64,
        now: i64,
    ) -> Result<(), ScheduleError> {
        let once = when.is_none();
        let s = Settle {
            schedule_id: row.schedule_id.clone(),
            fire: None,
            missed: Some(missed),
            next_fire_at: advance(when, latest),
            state: if once { "completed" } else { "active" }.into(),
            reason: once.then(|| "missed".to_string()),
            enabled: !once,
            // A one-off that missed its window is over: its grant is given up now (Q5).
            revoke: once.then(|| revoke_of(row, "once missed", now, 0)),
            now,
            expected_version: row.version,
        };
        tracing::info!(schedule_id = %row.schedule_id, count = missed.count, "fire missed");
        self.settle_row(row, s).await
    }

    /// Turn an [`End`] into the transaction that records it and advances the row.
    async fn end_fire(
        &self,
        row: &ScheduleRow,
        when: Option<&When>,
        fire_at: i64,
        na: i64,
        now: i64,
        end: End,
    ) -> Result<(), ScheduleError> {
        let once = when.is_none();
        let next = advance(when, fire_at);
        let mut s = Settle {
            schedule_id: row.schedule_id.clone(),
            fire: None,
            missed: None,
            next_fire_at: next,
            state: "active".into(),
            reason: None,
            enabled: true,
            revoke: None,
            now,
            expected_version: row.version,
        };
        let fire = |outcome: &str, run_id: Option<String>| SettledFire {
            fire_at,
            outcome: outcome.to_string(),
            run_id,
        };
        match end {
            End::Delivered {
                outcome,
                run_id,
                keep_grant_until_not_after,
            } => {
                s.fire = Some(fire(outcome, Some(run_id)));
                if once {
                    // Q4: a QUEUED fire is discarded by a revoke, so its grant waits for `not_after`.
                    let not_before = if keep_grant_until_not_after { na } else { 0 };
                    complete_once(&mut s, row, None, "once complete", not_before, now);
                }
            }
            End::SkippedBusy => {
                s.fire = Some(fire("skipped_busy", None));
                if once {
                    complete_once(&mut s, row, Some("skipped_busy"), "once skipped", 0, now);
                }
            }
            End::GaveUp { outcome } => {
                s.fire = Some(fire(outcome, None));
                if once {
                    complete_once(&mut s, row, Some(outcome), "once gave up", 0, now);
                    if outcome == "failed_unavailable" {
                        s.state = "needs_attention".into();
                    }
                }
            }
            End::Denied(reason) => {
                s.fire = Some(fire("denied", None));
                s.enabled = false;
                s.reason = Some(deny_word(reason));
                match reason {
                    DenyReason::Revoked => {
                        s.state = "revoked".into();
                        s.next_fire_at = None;
                    }
                    DenyReason::Expired => {
                        s.state = "expired".into();
                        s.next_fire_at = None;
                    }
                    // The grant is kept: the user can re-enable after fixing the cause, it expires
                    // on its own date and `schedule_delete` revokes it. Nothing is queued.
                    _ => {
                        s.state = "needs_attention".into();
                        if once {
                            s.next_fire_at = None;
                        }
                    }
                }
            }
        }
        tracing::info!(schedule_id = %row.schedule_id, state = %s.state, "fire settled");
        self.settle_row(row, s).await
    }

    /// Write the settle transaction, then try a revoke it queued that is already due.
    async fn settle_row(&self, _row: &ScheduleRow, s: Settle) -> Result<(), ScheduleError> {
        let applied = self.store.settle(&s)?;
        if !applied {
            tracing::info!(schedule_id = %s.schedule_id, "fire recorded; the schedule was changed meanwhile, so its row is left as the user set it");
        }
        if let (true, Some(r)) = (applied, &s.revoke) {
            if r.not_before <= s.now {
                self.try_revoke(r).await;
            }
        }
        Ok(())
    }
}

fn revoke_of(row: &ScheduleRow, reason: &str, now: i64, not_before: i64) -> PendingRevoke {
    PendingRevoke {
        grant_id: row.grant_id.clone(),
        token: row.token.clone(),
        reason: reason.into(),
        created_at: now,
        not_before,
    }
}

/// A one-off is over: completed, disabled, its grant queued for revoke.
fn complete_once(
    s: &mut Settle,
    row: &ScheduleRow,
    reason: Option<&str>,
    revoke_reason: &str,
    not_before: i64,
    now: i64,
) {
    s.state = "completed".into();
    s.reason = reason.map(str::to_string);
    s.enabled = false;
    s.next_fire_at = None;
    s.revoke = Some(revoke_of(row, revoke_reason, now, not_before));
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::clock::TestClock;
    use crate::store::NewSchedule;

    const TOKEN: &str = "vam_rg_rg_T.topsecret";

    fn ts(s: &str) -> i64 {
        s.parse::<DateTime<Utc>>().unwrap().timestamp()
    }

    #[derive(Default)]
    struct FakeGw {
        script: Mutex<VecDeque<Result<FireOutcome, GatewayError>>>,
        fires: Mutex<Vec<FireRequest>>,
        revokes: Mutex<Vec<String>>,
        revoke_fails: Mutex<bool>,
        delay_ms: u64,
        active: AtomicUsize,
        max_active: AtomicUsize,
        /// Runs while a fire is in flight (a user's write landing meanwhile).
        on_fire: Mutex<Option<Box<dyn Fn() + Send>>>,
    }

    impl FakeGw {
        fn script(&self, items: Vec<Result<FireOutcome, GatewayError>>) {
            self.script.lock().unwrap().extend(items);
        }
        fn fires(&self) -> Vec<FireRequest> {
            self.fires.lock().unwrap().clone()
        }
    }

    #[async_trait]
    impl Gateway for FakeGw {
        async fn fire(&self, req: &FireRequest) -> Result<FireOutcome, GatewayError> {
            let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(n, Ordering::SeqCst);
            self.fires.lock().unwrap().push(req.clone());
            if let Some(f) = self.on_fire.lock().unwrap().as_ref() {
                f();
            }
            if self.delay_ms > 0 {
                tokio::time::sleep(Duration::from_millis(self.delay_ms)).await;
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            self.script
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(GatewayError::Timeout(Duration::from_secs(1))))
        }
        async fn revoke(&self, token: &GrantToken) -> Result<(), GatewayError> {
            if *self.revoke_fails.lock().unwrap() {
                return Err(GatewayError::Timeout(Duration::from_secs(1)));
            }
            self.revokes
                .lock()
                .unwrap()
                .push(token.expose().to_string());
            Ok(())
        }
    }

    fn accepted() -> Result<FireOutcome, GatewayError> {
        Ok(FireOutcome::Accepted {
            run_id: "run1".into(),
        })
    }
    fn unavailable(ms: u32) -> Result<FireOutcome, GatewayError> {
        Ok(FireOutcome::Unavailable { retry_after_ms: ms })
    }

    struct Rig {
        store: Arc<Store>,
        gw: Arc<FakeGw>,
        clock: TestClock,
        sched: Scheduler,
    }

    fn rig_with(gw: FakeGw, store: Arc<Store>, now: &str) -> Rig {
        let gw = Arc::new(gw);
        let clock = TestClock::new(now.parse().unwrap());
        let sched =
            Scheduler::new(store.clone(), gw.clone(), Arc::new(clock.clone())).with_jitter(|| 0.0);
        Rig {
            store,
            gw,
            clock,
            sched,
        }
    }

    fn rig(now: &str) -> Rig {
        rig_with(
            FakeGw::default(),
            Arc::new(Store::open_in_memory().unwrap()),
            now,
        )
    }

    fn new_schedule(id: &str, kind: &str, next: &str) -> NewSchedule {
        NewSchedule {
            schedule_id: id.into(),
            grant_id: format!("rg_{id}"),
            token: GrantToken::new(TOKEN),
            grant_expires_at: ts("2030-01-01T00:00:00Z"),
            target_json: "{}".into(),
            message_preview: String::new(),
            owner_user_id: "u".into(),
            creator_agent_urn: "a".into(),
            title: "t".into(),
            kind: kind.into(),
            expression: if kind == "cron" { "0 * * * *" } else { "x" }.into(),
            timezone: "UTC".into(),
            catch_up: "latest".into(),
            next_fire_at: Some(ts(next)),
            now: 0,
        }
    }

    fn add(r: &Rig, id: &str, kind: &str, next: &str) {
        r.store
            .insert_schedule(&new_schedule(id, kind, next))
            .unwrap();
    }

    fn sched_row(r: &Rig, id: &str) -> ScheduleRow {
        r.store.get_schedule(id).unwrap().unwrap()
    }

    fn pending(r: &Rig) -> Vec<PendingRevoke> {
        r.store.due_revokes(i64::MAX).unwrap()
    }

    #[test]
    fn backoff_is_full_jitter_capped_and_floored() {
        assert_eq!(backoff_delay_ms(1, 0, 0.5), 1_000);
        assert_eq!(backoff_delay_ms(2, 0, 0.5), 2_000);
        assert_eq!(backoff_delay_ms(3, 0, 0.999), 7_992);
        assert_eq!(backoff_delay_ms(30, 0, 1.0), BACKOFF_CAP_MS);
        assert_eq!(backoff_delay_ms(1, 30_000, 0.0), 30_000);
        assert_eq!(backoff_delay_ms(0, 0, 2.0), BACKOFF_BASE_MS);
    }

    #[tokio::test]
    async fn accepted_cron_fire_records_the_run_and_advances() {
        let r = rig("2026-01-01T10:00:05Z");
        add(&r, "sch_a", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![accepted()]);
        r.sched.run_pass().await;

        let reqs = r.gw.fires();
        assert_eq!(reqs.len(), 1);
        let t = ts("2026-01-01T10:00:00Z");
        assert_eq!(reqs[0].fire_at, t);
        assert_eq!(reqs[0].trigger_ref, "sch_a");
        assert_eq!(reqs[0].kind, FireKind::Scheduled);
        assert_eq!(reqs[0].not_after, Some(ts("2026-01-01T11:00:00Z")));
        assert_eq!(
            reqs[0].idempotency_key,
            idempotency_key("sch_a", t, KeyKind::Scheduled)
        );
        let fires = r.store.fires_for("sch_a").unwrap();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].outcome.as_deref(), Some("accepted"));
        assert_eq!(fires[0].run_id.as_deref(), Some("run1"));
        let s = sched_row(&r, "sch_a");
        assert_eq!(s.next_fire_at, Some(ts("2026-01-01T11:00:00Z")));
        assert_eq!((s.state.as_str(), s.enabled), ("active", true));
        assert!(pending(&r).is_empty());
        // Not due again until 11:00.
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 1);
    }

    /// A user's write (an `Edit`) that lands while the gateway call is in flight.
    fn edit_during_fire(
        r: &Rig,
        id: &'static str,
        f: impl Fn(&mut crate::store::Edit) + Send + 'static,
    ) {
        let store = r.store.clone();
        *r.gw.on_fire.lock().unwrap() = Some(Box::new(move || {
            let d = store.get_for_write(id, "u", "a").unwrap().unwrap();
            let mut e = crate::store::Edit {
                schedule_id: d.schedule_id.clone(),
                expected_version: d.version,
                title: d.title.clone(),
                kind: d.kind.clone(),
                expression: d.expression.clone(),
                timezone: d.timezone.clone(),
                catch_up: d.catch_up.clone(),
                enabled: d.enabled,
                state: d.state.clone(),
                state_reason: d.state_reason.clone(),
                next_fire_at: d.next_fire_at,
                rebind: None,
                abandon_open_fire: false,
                now: 1,
            };
            f(&mut e);
            assert!(store.apply_edit(&e).unwrap());
        }));
    }

    #[tokio::test]
    async fn a_pause_during_a_fire_stays_paused_after_the_fire_settles() {
        let r = rig("2026-01-01T10:00:05Z");
        add(&r, "sch_p", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![accepted()]);
        edit_during_fire(&r, "sch_p", |e| e.enabled = false);
        r.sched.run_pass().await;
        let s = sched_row(&r, "sch_p");
        assert!(!s.enabled, "the fire must not undo the pause");
        assert_eq!(
            s.next_fire_at,
            Some(ts("2026-01-01T10:00:00Z")),
            "row not advanced"
        );
        assert_eq!(s.version, 1);
        // The fire itself is still recorded.
        let fires = r.store.fires_for("sch_p").unwrap();
        assert_eq!(fires[0].outcome.as_deref(), Some("accepted"));
        assert_eq!(fires[0].run_id.as_deref(), Some("run1"));
    }

    #[tokio::test]
    async fn a_when_change_during_a_fire_keeps_the_new_next_fire() {
        let r = rig("2026-01-01T10:00:05Z");
        add(&r, "sch_w", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![accepted()]);
        let later = ts("2026-01-05T09:00:00Z");
        edit_during_fire(&r, "sch_w", move |e| e.next_fire_at = Some(later));
        r.sched.run_pass().await;
        let s = sched_row(&r, "sch_w");
        assert_eq!(s.next_fire_at, Some(later));
        assert_eq!(
            r.store.fires_for("sch_w").unwrap()[0].outcome.as_deref(),
            Some("accepted")
        );
    }

    #[tokio::test]
    async fn a_once_that_completes_while_the_user_changed_it_keeps_the_users_row() {
        let r = rig("2026-01-01T10:00:02Z");
        add(&r, "sch_o", "once", "2026-01-01T10:00:00Z");
        r.gw.script(vec![accepted()]);
        let later = ts("2026-01-02T09:00:00Z");
        edit_during_fire(&r, "sch_o", move |e| e.next_fire_at = Some(later));
        r.sched.run_pass().await;
        let s = sched_row(&r, "sch_o");
        assert_eq!(
            (s.state.as_str(), s.enabled),
            ("active", true),
            "not completed"
        );
        assert_eq!(s.next_fire_at, Some(later));
        assert!(pending(&r).is_empty(), "its grant is not given up");
        assert!(r.gw.revokes.lock().unwrap().is_empty());
        assert_eq!(
            r.store.fires_for("sch_o").unwrap()[0].outcome.as_deref(),
            Some("accepted")
        );
    }

    #[tokio::test]
    async fn a_delete_marking_during_a_fire_stays_deleting_with_its_revoke_queued() {
        for kind in ["once", "cron"] {
            let r = rig("2026-01-01T10:00:02Z");
            add(&r, "sch_d", kind, "2026-01-01T10:00:00Z");
            r.gw.script(vec![accepted()]);
            let store = r.store.clone();
            *r.gw.on_fire.lock().unwrap() = Some(Box::new(move || {
                store.mark_deleting("sch_d", 1).unwrap();
            }));
            r.sched.run_pass().await;
            let s = sched_row(&r, "sch_d");
            assert_eq!((s.state.as_str(), s.enabled), ("deleting", false), "{kind}");
            assert_eq!(
                s.next_fire_at, None,
                "{kind}: the fire did not advance the row"
            );
            assert_eq!(s.version, 1, "{kind}");
            let queued = pending(&r);
            assert_eq!(
                queued.len(),
                1,
                "{kind}: the delete's revoke is still queued"
            );
            assert_eq!(queued[0].grant_id, "rg_sch_d");
            assert_eq!(queued[0].reason, "delete");
            // The fire itself is on the ledger.
            assert_eq!(
                r.store.fires_for("sch_d").unwrap()[0].outcome.as_deref(),
                Some("accepted"),
                "{kind}"
            );
            // And the row never fires again.
            r.clock.set("2026-01-02T10:00:00Z".parse().unwrap());
            r.sched.run_pass().await;
            assert_eq!(r.gw.fires().len(), 1, "{kind}");
        }
    }

    #[tokio::test]
    async fn a_backoff_is_not_written_over_a_users_change() {
        let r = rig("2026-01-01T10:00:05Z");
        add(&r, "sch_r", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![unavailable(5000)]);
        edit_during_fire(&r, "sch_r", |e| e.enabled = false);
        r.sched.run_pass().await;
        let s = sched_row(&r, "sch_r");
        assert_eq!(s.backoff_until, None);
        assert_eq!(s.attempts, 0);
    }

    #[tokio::test]
    async fn an_unchanged_row_settles_as_before() {
        let r = rig("2026-01-01T10:00:05Z");
        add(&r, "sch_n", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![accepted()]);
        r.sched.run_pass().await;
        let s = sched_row(&r, "sch_n");
        assert_eq!(s.next_fire_at, Some(ts("2026-01-01T11:00:00Z")));
        assert_eq!(s.version, 0, "a fire is not a user write");
    }

    #[tokio::test]
    async fn once_completion_follows_q4() {
        // ACCEPTED: revoked at once. QUEUED and DUPLICATE: queued until not_after.
        for (outcome, at_once) in [("accepted", true), ("queued", false), ("duplicate", false)] {
            let r = rig("2026-01-01T10:00:02Z");
            add(&r, "sch_o", "once", "2026-01-01T10:00:00Z");
            let run_id = "run1".to_string();
            r.gw.script(vec![Ok(match outcome {
                "accepted" => FireOutcome::Accepted { run_id },
                "queued" => FireOutcome::Queued { run_id },
                _ => FireOutcome::Duplicate { run_id },
            })]);
            r.sched.run_pass().await;
            let s = sched_row(&r, "sch_o");
            assert_eq!(
                (s.state.as_str(), s.enabled, s.next_fire_at),
                ("completed", false, None),
                "{outcome}"
            );
            assert_eq!(
                r.store.fires_for("sch_o").unwrap()[0].outcome.as_deref(),
                Some(outcome)
            );
            if at_once {
                assert_eq!(r.gw.revokes.lock().unwrap().len(), 1);
                assert!(pending(&r).is_empty());
            } else {
                assert!(r.gw.revokes.lock().unwrap().is_empty(), "{outcome}");
                let p = pending(&r);
                let na = ts("2026-01-01T10:00:00Z") + 86_400;
                assert_eq!(p.len(), 1);
                assert_eq!(p[0].not_before, na);
                // Not due before not_after; due after it.
                r.clock.set(dt(na - 1));
                r.sched.revoke_pass().await;
                assert!(r.gw.revokes.lock().unwrap().is_empty());
                r.clock.set(dt(na));
                r.sched.revoke_pass().await;
                assert_eq!(r.gw.revokes.lock().unwrap().len(), 1);
                assert!(pending(&r).is_empty());
            }
        }
    }

    #[tokio::test]
    async fn busy_skipped_and_already_queued_end_the_fire_without_retry() {
        for reason in [BusyReason::Skipped, BusyReason::AlreadyQueued] {
            let r = rig("2026-01-01T10:00:02Z");
            add(&r, "sch_b", "cron", "2026-01-01T10:00:00Z");
            r.gw.script(vec![Ok(FireOutcome::Busy {
                reason,
                retry_after_ms: 0,
            })]);
            r.sched.run_pass().await;
            assert_eq!(r.gw.fires().len(), 1);
            let f = &r.store.fires_for("sch_b").unwrap()[0];
            assert_eq!(f.outcome.as_deref(), Some("skipped_busy"));
            assert_eq!(
                sched_row(&r, "sch_b").next_fire_at,
                Some(ts("2026-01-01T11:00:00Z"))
            );
        }
        // A one-off that is skipped is over, and its grant is given up.
        let r = rig("2026-01-01T10:00:02Z");
        add(&r, "sch_b", "once", "2026-01-01T10:00:00Z");
        r.gw.script(vec![Ok(FireOutcome::Busy {
            reason: BusyReason::Skipped,
            retry_after_ms: 0,
        })]);
        r.sched.run_pass().await;
        let s = sched_row(&r, "sch_b");
        assert_eq!(s.state, "completed");
        assert_eq!(s.state_reason.as_deref(), Some("skipped_busy"));
        assert_eq!(r.gw.revokes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn busy_active_run_retries_with_one_key_then_ends_skipped_busy() {
        let r = rig("2026-01-01T10:00:02Z");
        add(&r, "sch_b", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![
            Ok(FireOutcome::Busy {
                reason: BusyReason::ActiveRun,
                retry_after_ms: 2000,
            }),
            Ok(FireOutcome::Busy {
                reason: BusyReason::ActiveRun,
                retry_after_ms: 2000,
            }),
        ]);
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 1);
        let s = sched_row(&r, "sch_b");
        assert_eq!(s.backoff_until, Some(ts("2026-01-01T10:00:04Z")));
        assert_eq!(s.next_fire_at, Some(ts("2026-01-01T10:00:00Z")));
        r.sched.run_pass().await; // still backing off
        assert_eq!(r.gw.fires().len(), 1);
        r.clock.set("2026-01-01T10:00:04Z".parse().unwrap());
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 2);
        // not_after is 11:00: at that time the 10:00 fire is given up as skipped_busy, and the
        // 11:00 fire (due now) makes its own first attempt.
        r.clock.set("2026-01-01T11:00:00Z".parse().unwrap());
        r.sched.run_pass().await;
        let f = &r.store.fires_for("sch_b").unwrap()[0];
        assert_eq!(f.outcome.as_deref(), Some("skipped_busy"));
        let reqs = r.gw.fires();
        assert_eq!(reqs.len(), 3);
        assert_eq!(reqs[0].idempotency_key, reqs[1].idempotency_key);
        assert_ne!(reqs[1].idempotency_key, reqs[2].idempotency_key);
    }

    #[tokio::test]
    async fn unavailable_backs_off_reuses_the_key_and_gives_up_at_not_after() {
        let mut r = rig("2026-01-01T10:00:00Z");
        r.sched = r.sched.clone().with_jitter(|| 0.5);
        add(&r, "sch_u", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![
            unavailable(30_000),
            Ok(FireOutcome::RateLimited { retry_after_ms: 0 }),
            Err(GatewayError::Timeout(Duration::from_secs(1))),
        ]);
        r.sched.run_pass().await;
        let s = sched_row(&r, "sch_u");
        // Attempt 1: jitter gives 1 s, so the floor retry_after_ms wins.
        assert_eq!(s.backoff_until, Some(ts("2026-01-01T10:00:30Z")));
        assert_eq!(s.attempts, 1);
        r.clock.set("2026-01-01T10:00:29Z".parse().unwrap());
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 1, "not before retry_after");
        r.clock.set("2026-01-01T10:00:30Z".parse().unwrap());
        r.sched.run_pass().await;
        // Attempt 2: ceiling 4 s, jitter 0.5 -> 2 s.
        assert_eq!(
            sched_row(&r, "sch_u").backoff_until,
            Some(ts("2026-01-01T10:00:32Z"))
        );
        r.clock.set("2026-01-01T10:00:32Z".parse().unwrap());
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 3);
        let keys: HashSet<_> =
            r.gw.fires()
                .iter()
                .map(|q| q.idempotency_key.clone())
                .collect();
        assert_eq!(keys.len(), 1, "every retry reuses the key");
        assert_eq!(r.store.fires_for("sch_u").unwrap()[0].attempts, 3);

        r.clock.set("2026-01-01T11:00:00Z".parse().unwrap());
        r.sched.run_pass().await;
        // It gave up the 10:00 fire; the 11:00 fire is due now and starts its own retry.
        let f = &r.store.fires_for("sch_u").unwrap()[0];
        assert_eq!(f.outcome.as_deref(), Some("failed_unavailable"));
        let s = sched_row(&r, "sch_u");
        assert_eq!((s.state.as_str(), s.enabled), ("active", true));
    }

    #[tokio::test]
    async fn a_one_off_that_cannot_reach_the_gateway_needs_attention_and_is_revoked() {
        let r = rig("2026-01-01T10:00:00Z");
        add(&r, "sch_o", "once", "2026-01-01T10:00:00Z");
        r.gw.script(vec![Err(GatewayError::NotConfigured)]);
        r.sched.run_pass().await;
        r.clock.set(dt(ts("2026-01-01T10:00:00Z") + 86_400));
        r.sched.run_pass().await;
        let s = sched_row(&r, "sch_o");
        assert_eq!((s.state.as_str(), s.enabled), ("needs_attention", false));
        assert_eq!(
            r.store.fires_for("sch_o").unwrap()[0].outcome.as_deref(),
            Some("failed_unavailable")
        );
        assert_eq!(r.gw.revokes.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn denied_revoked_and_expired_end_the_schedule() {
        for (reason, state) in [
            (DenyReason::Revoked, "revoked"),
            (DenyReason::Expired, "expired"),
        ] {
            let r = rig("2026-01-01T10:00:02Z");
            add(&r, "sch_d", "cron", "2026-01-01T10:00:00Z");
            r.gw.script(vec![Ok(FireOutcome::Denied { reason })]);
            r.sched.run_pass().await;
            let s = sched_row(&r, "sch_d");
            assert_eq!(
                (s.state.as_str(), s.enabled, s.next_fire_at),
                (state, false, None)
            );
            assert_eq!(
                r.store.fires_for("sch_d").unwrap()[0].outcome.as_deref(),
                Some("denied")
            );
            r.sched.run_pass().await;
            assert_eq!(r.gw.fires().len(), 1, "never fires again");
        }
    }

    #[tokio::test]
    async fn needs_attention_keeps_its_token_and_queues_no_revoke() {
        for reason in [
            DenyReason::AgentUnavailable,
            DenyReason::TargetGone,
            DenyReason::OwnerDisabled,
            DenyReason::ToolNotApproved,
        ] {
            let r = rig("2026-01-01T10:00:02Z");
            add(&r, "sch_n", "cron", "2026-01-01T10:00:00Z");
            r.gw.script(vec![Ok(FireOutcome::Denied { reason })]);
            r.sched.run_pass().await;
            let s = sched_row(&r, "sch_n");
            assert_eq!((s.state.as_str(), s.enabled), ("needs_attention", false));
            assert_eq!(s.state_reason, Some(deny_word(reason)));
            assert_eq!(s.token.expose(), TOKEN, "the token stays for re-enable");
            assert!(pending(&r).is_empty(), "no pending_revokes row");
            assert!(r.gw.revokes.lock().unwrap().is_empty(), "no revoke call");
            r.sched.run_pass().await;
            assert_eq!(r.gw.fires().len(), 1, "disabled: never fires");
        }
        // A one-off refused for another reason also keeps its token.
        let r = rig("2026-01-01T10:00:02Z");
        add(&r, "sch_n", "once", "2026-01-01T10:00:00Z");
        r.gw.script(vec![Ok(FireOutcome::Denied {
            reason: DenyReason::TargetGone,
        })]);
        r.sched.run_pass().await;
        assert_eq!(sched_row(&r, "sch_n").state, "needs_attention");
        assert!(pending(&r).is_empty());
    }

    #[tokio::test]
    async fn catch_up_latest_fires_the_newest_missed_time_and_folds_the_rest() {
        let r = rig("2026-01-01T13:20:00Z");
        add(&r, "sch_c", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![accepted()]);
        r.sched.run_pass().await;
        let reqs = r.gw.fires();
        assert_eq!(reqs.len(), 1, "never more than one catch-up fire");
        assert_eq!(reqs[0].fire_at, ts("2026-01-01T13:00:00Z"));
        assert_eq!(reqs[0].not_after, Some(ts("2026-01-01T14:00:00Z")));
        let fires = r.store.fires_for("sch_c").unwrap();
        assert_eq!(fires.len(), 2);
        assert_eq!(fires[0].fire_at, ts("2026-01-01T10:00:00Z"));
        assert_eq!(fires[0].outcome.as_deref(), Some("missed"));
        assert_eq!(fires[0].missed_count, 3);
        assert_eq!(fires[1].outcome.as_deref(), Some("accepted"));
        assert_eq!(
            sched_row(&r, "sch_c").next_fire_at,
            Some(ts("2026-01-01T14:00:00Z"))
        );
    }

    #[tokio::test]
    async fn catch_up_outside_the_grace_window_fires_nothing() {
        let r = rig("2026-01-01T13:45:00Z"); // grace for an hourly cron is 30 min
        add(&r, "sch_c", "cron", "2026-01-01T10:00:00Z");
        r.sched.run_pass().await;
        assert!(r.gw.fires().is_empty());
        let fires = r.store.fires_for("sch_c").unwrap();
        assert_eq!(fires.len(), 1);
        assert_eq!(
            (fires[0].outcome.as_deref(), fires[0].missed_count),
            (Some("missed"), 4)
        );
        assert_eq!(
            sched_row(&r, "sch_c").next_fire_at,
            Some(ts("2026-01-01T14:00:00Z"))
        );
    }

    #[tokio::test]
    async fn catch_up_skip_records_missed_and_advances_but_an_on_time_fire_still_runs() {
        let r = rig("2026-01-01T13:20:00Z");
        let mut n = new_schedule("sch_s", "cron", "2026-01-01T10:00:00Z");
        n.catch_up = "skip".into();
        r.store.insert_schedule(&n).unwrap();
        r.sched.run_pass().await;
        assert!(r.gw.fires().is_empty());
        let f = &r.store.fires_for("sch_s").unwrap()[0];
        assert_eq!((f.outcome.as_deref(), f.missed_count), (Some("missed"), 4));
        assert_eq!(
            sched_row(&r, "sch_s").next_fire_at,
            Some(ts("2026-01-01T14:00:00Z"))
        );
        // At 14:00:30 the single due time is on time, so `skip` fires it.
        r.gw.script(vec![accepted()]);
        r.clock.set("2026-01-01T14:00:30Z".parse().unwrap());
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 1);
    }

    #[tokio::test]
    async fn a_missed_one_off_is_completed_and_revoked() {
        let r = rig("2026-01-03T10:00:00Z"); // two days late: past the 24 h grace
        add(&r, "sch_o", "once", "2026-01-01T10:00:00Z");
        r.sched.run_pass().await;
        assert!(r.gw.fires().is_empty());
        let s = sched_row(&r, "sch_o");
        assert_eq!(
            (s.state.as_str(), s.state_reason.as_deref()),
            ("completed", Some("missed"))
        );
        assert_eq!(r.gw.revokes.lock().unwrap().len(), 1);
        // Inside the grace window it fires.
        let r = rig("2026-01-01T20:00:00Z");
        add(&r, "sch_o", "once", "2026-01-01T10:00:00Z");
        r.gw.script(vec![accepted()]);
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 1);
    }

    #[tokio::test]
    async fn a_fire_begun_before_a_crash_is_resumed_with_the_same_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        let t = ts("2026-01-01T10:00:00Z");
        let key = idempotency_key("sch_x", t, KeyKind::Scheduled);
        {
            // The first process wrote its intent and (maybe) reached vamd, then died.
            let store = Store::open(&path).unwrap();
            store
                .insert_schedule(&new_schedule("sch_x", "cron", "2026-01-01T10:00:00Z"))
                .unwrap();
            store
                .begin_fire("sch_x", t, KIND_SCHEDULED, &key, None, t)
                .unwrap();
        }
        let r = rig_with(
            FakeGw::default(),
            Arc::new(Store::open(&path).unwrap()),
            "2026-01-01T10:03:00Z",
        );
        r.gw.script(vec![Ok(FireOutcome::Duplicate {
            run_id: "run1".into(),
        })]);
        r.sched.run_pass().await;
        let reqs = r.gw.fires();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].idempotency_key, key);
        assert_eq!(reqs[0].fire_at, t);
        let f = &r.store.fires_for("sch_x").unwrap()[0];
        assert_eq!((f.outcome.as_deref(), f.attempts), (Some("duplicate"), 2));
        assert_eq!(
            sched_row(&r, "sch_x").next_fire_at,
            Some(ts("2026-01-01T11:00:00Z"))
        );
    }

    #[tokio::test]
    async fn backoff_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("db");
        {
            let r = rig_with(
                FakeGw::default(),
                Arc::new(Store::open(&path).unwrap()),
                "2026-01-01T10:00:00Z",
            );
            add(&r, "sch_k", "cron", "2026-01-01T10:00:00Z");
            r.gw.script(vec![unavailable(60_000), accepted()]);
            r.sched.run_pass().await;
        }
        let r = rig_with(
            FakeGw::default(),
            Arc::new(Store::open(&path).unwrap()),
            "2026-01-01T10:00:59Z",
        );
        let s = sched_row(&r, "sch_k");
        assert_eq!(
            (s.backoff_until, s.attempts),
            (Some(ts("2026-01-01T10:01:00Z")), 1)
        );
        r.gw.script(vec![accepted()]);
        r.sched.run_pass().await;
        assert!(
            r.gw.fires().is_empty(),
            "still backing off after the restart"
        );
        r.clock.set("2026-01-01T10:01:00Z".parse().unwrap());
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 1);
        assert_eq!(
            r.store.fires_for("sch_k").unwrap()[0].attempts,
            2,
            "the attempt count carried over"
        );
    }

    #[tokio::test]
    async fn at_most_four_fires_are_in_flight() {
        let gw = FakeGw {
            delay_ms: 40,
            ..FakeGw::default()
        };
        let r = rig_with(
            gw,
            Arc::new(Store::open_in_memory().unwrap()),
            "2026-01-01T10:00:02Z",
        );
        for i in 0..10 {
            add(&r, &format!("sch_{i}"), "cron", "2026-01-01T10:00:00Z");
        }
        r.gw.script((0..10).map(|_| accepted()).collect());
        r.sched.run_pass().await;
        assert_eq!(r.gw.fires().len(), 10);
        assert_eq!(r.gw.max_active.load(Ordering::SeqCst), MAX_IN_FLIGHT);
    }

    #[tokio::test]
    async fn queued_revokes_wait_for_not_before_and_a_failed_revoke_is_retried() {
        let r = rig("2026-01-01T00:00:50Z");
        let q = |id: &str, nb: i64| PendingRevoke {
            grant_id: id.into(),
            token: GrantToken::new(format!("tok_{id}")),
            reason: "t".into(),
            created_at: 0,
            not_before: nb,
        };
        r.store.queue_revoke(&q("rg_a", 0)).unwrap();
        r.store
            .queue_revoke(&q("rg_b", ts("2026-01-01T00:01:00Z")))
            .unwrap();
        *r.gw.revoke_fails.lock().unwrap() = true;
        r.sched.revoke_pass().await;
        assert_eq!(pending(&r).len(), 2, "a failed revoke stays queued");
        *r.gw.revoke_fails.lock().unwrap() = false;
        r.sched.revoke_pass().await;
        assert_eq!(pending(&r).len(), 1, "rg_b is not due yet");
        r.clock.set("2026-01-01T00:01:00Z".parse().unwrap());
        r.sched.revoke_pass().await;
        assert!(pending(&r).is_empty());
        assert_eq!(r.gw.revokes.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_deleting_row_never_fires_and_is_purged_when_its_revoke_succeeds() {
        let r = rig("2026-01-01T10:00:02Z");
        add(&r, "sch_d", "cron", "2026-01-01T10:00:00Z");
        r.store.mark_deleting("sch_d", 1).unwrap();
        r.sched.run_pass().await;
        assert!(r.gw.fires().is_empty());
        assert_eq!(sched_row(&r, "sch_d").state, "deleting");
        r.sched.revoke_pass().await;
        assert!(r.store.get_schedule("sch_d").unwrap().is_none());
        assert!(pending(&r).is_empty());
    }

    #[tokio::test]
    async fn the_loop_wakes_on_notify_and_stops_on_shutdown() {
        let r = rig("2026-01-01T10:00:02Z");
        let wake = r.sched.wake_handle();
        let (tx, rx) = watch::channel(false);
        let h = tokio::spawn(r.sched.clone().run(rx));
        tokio::time::sleep(Duration::from_millis(100)).await; // the loop is now idle (30 s sleep)
        add(&r, "sch_w", "cron", "2026-01-01T10:00:00Z");
        r.gw.script(vec![accepted()]);
        wake.notify_one();
        let mut seen = false;
        for _ in 0..40 {
            if !r.gw.fires().is_empty() {
                seen = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(seen, "notify woke the loop well before the 30 s cap");
        tx.send(true).unwrap();
        tokio::time::timeout(Duration::from_secs(5), h)
            .await
            .unwrap()
            .unwrap();
    }
}
