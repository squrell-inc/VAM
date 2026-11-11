//! `schedule_delete` and `schedule_run_now`.
//!
//! Delete withdraws the grant at the gateway first; only an answer lets the row go. With no
//! answer the row becomes `deleting` (never fires) and its token waits in `pending_revokes` for
//! the loop's per-minute drain. Run now fires once with the stored grant on its own ledger row
//! (kind `run_now`); it never changes the schedule.

use serde_json::{json, Map, Value};

use crate::fire::Gateway;
use crate::gateway::{FireKind, FireOutcome, FireRequest};
use crate::idempotency::run_now_key;
use crate::ops::{check_keys, rfc, schedule_id_arg, Caller, Ctx, ToolErr};
use crate::tools;

/// How long the gateway may still accept a run-now after its `fire_at`.
const RUN_NOW_WINDOW_SECS: i64 = 24 * 3600;

type Res = Result<Value, ToolErr>;

pub async fn delete(
    ctx: &Ctx<'_>,
    gateway: &dyn Gateway,
    caller: &Caller,
    args: &Map<String, Value>,
) -> Res {
    check_keys(tools::SCHEDULE_DELETE, args)?;
    let id = schedule_id_arg(args)?;
    let row = ctx
        .store
        .get_for_write(&id, &caller.user_id, &caller.agent_urn)?
        .ok_or_else(ToolErr::not_found)?;
    let now = ctx.clock.now().timestamp();
    match gateway.revoke(&row.token).await {
        Ok(()) => {
            ctx.store.delete_schedule(&id)?;
            tracing::info!(schedule_id = %id, "schedule deleted");
            Ok(json!({ "scheduleId": id, "deleted": true }))
        }
        Err(e) => {
            // Never fires again from here; the loop retries the revoke and removes the row.
            ctx.store.mark_deleting(&id, now)?;
            ctx.wake.notify_one();
            tracing::warn!(schedule_id = %id, error = %e, "delete: gateway did not answer; queued");
            Ok(json!({
                "scheduleId": id,
                "deleted": false,
                "state": "deleting",
                "note": "the schedule will not run again; its approval is being withdrawn and it will disappear when that is done"
            }))
        }
    }
}

pub async fn run_now(
    ctx: &Ctx<'_>,
    gateway: &dyn Gateway,
    caller: &Caller,
    args: &Map<String, Value>,
    calling_run_id: &str,
) -> Res {
    check_keys(tools::SCHEDULE_RUN_NOW, args)?;
    let id = schedule_id_arg(args)?;
    let row = ctx
        .store
        .get_for_write(&id, &caller.user_id, &caller.agent_urn)?
        .ok_or_else(ToolErr::not_found)?;
    let now = ctx.clock.now().timestamp();
    match row.state.as_str() {
        "deleting" | "revoked" | "expired" => {
            return Err(ToolErr::precondition(format!(
                "the schedule is {}; it cannot run",
                row.state
            )))
        }
        _ => {}
    }
    if row.grant_expires_at <= now {
        return Err(ToolErr::precondition(
            "the approval has ended; update the schedule with `run` to approve it again",
        ));
    }
    let fire_at = now;
    let key = run_now_key(&id, fire_at, calling_run_id);
    ctx.store
        .begin_fire(&id, fire_at, "run_now", &key, None, now)?;
    let req = FireRequest {
        token: row.token.clone(),
        idempotency_key: key,
        fire_at,
        trigger_ref: id.clone(),
        kind: FireKind::RunNow,
        not_after: Some(fire_at + RUN_NOW_WINDOW_SECS),
    };
    let answer = gateway.fire(&req).await;
    let (outcome, run_id, refusal) = match answer {
        Ok(FireOutcome::Accepted { run_id }) => ("accepted", Some(run_id), None),
        Ok(FireOutcome::Queued { run_id }) => ("queued", Some(run_id), None),
        Ok(FireOutcome::Duplicate { run_id }) => ("duplicate", Some(run_id), None),
        Ok(FireOutcome::Busy { .. }) => (
            "skipped_busy",
            None,
            Some("the Space is busy; nothing was started, try again shortly"),
        ),
        Ok(FireOutcome::RateLimited { .. }) | Ok(FireOutcome::Unavailable { .. }) | Err(_) => (
            "failed_unavailable",
            None,
            Some("the run could not be started right now; nothing was started, try again"),
        ),
        Ok(FireOutcome::Denied { reason }) => {
            tracing::info!(schedule_id = %id, reason = ?reason, "run now denied");
            (
                "denied",
                None,
                Some("the gateway refused the run (the approval is not usable); the schedule is unchanged"),
            )
        }
    };
    ctx.store
        .finish_run_now(&id, fire_at, outcome, run_id.as_deref(), now)?;
    tracing::info!(schedule_id = %id, outcome, "run now answered");
    if let Some(text) = refusal {
        return Err(ToolErr::precondition(format!("{outcome}: {text}")));
    }
    Ok(json!({
        "scheduleId": id,
        "outcome": outcome,
        "runId": run_id,
        "firedAt": rfc(fire_at),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::{Clock, TestClock};
    use crate::gateway::GatewayError;
    use crate::grant::GrantToken;
    use crate::store::{NewSchedule, Store};
    use async_trait::async_trait;
    use std::sync::Mutex;
    use std::time::Duration;
    use tokio::sync::Notify;

    #[derive(Default)]
    struct Gw {
        fire_answer: Mutex<Option<Result<FireOutcome, GatewayError>>>,
        revoke_down: Mutex<bool>,
        fires: Mutex<Vec<FireRequest>>,
        revokes: Mutex<usize>,
    }

    #[async_trait]
    impl Gateway for Gw {
        async fn fire(&self, req: &FireRequest) -> Result<FireOutcome, GatewayError> {
            self.fires.lock().unwrap().push(req.clone());
            self.fire_answer
                .lock()
                .unwrap()
                .take()
                .unwrap_or(Err(GatewayError::Timeout(Duration::from_secs(1))))
        }
        async fn revoke(&self, _t: &GrantToken) -> Result<(), GatewayError> {
            *self.revokes.lock().unwrap() += 1;
            if *self.revoke_down.lock().unwrap() {
                Err(GatewayError::Timeout(Duration::from_secs(1)))
            } else {
                Ok(())
            }
        }
    }

    struct Rig {
        store: Store,
        clock: TestClock,
        wake: Notify,
    }

    fn rig() -> Rig {
        let r = Rig {
            store: Store::open_in_memory().unwrap(),
            clock: TestClock::new("2026-01-01T10:00:00.700Z".parse().unwrap()),
            wake: Notify::new(),
        };
        r.store
            .insert_schedule(&NewSchedule {
                schedule_id: "sch_a".into(),
                grant_id: "rg_a".into(),
                token: GrantToken::new("tok"),
                grant_expires_at: 1_900_000_000,
                target_json: "{}".into(),
                message_preview: String::new(),
                owner_user_id: "u".into(),
                creator_agent_urn: "a".into(),
                title: "t".into(),
                kind: "cron".into(),
                expression: "0 * * * *".into(),
                timezone: "UTC".into(),
                catch_up: "latest".into(),
                next_fire_at: Some(1_800_000_000),
                now: 0,
            })
            .unwrap();
        r
    }

    impl Rig {
        fn ctx(&self) -> Ctx<'_> {
            Ctx {
                store: &self.store,
                clock: &self.clock,
                min_interval: Duration::from_secs(900),
                wake: &self.wake,
            }
        }
    }

    fn me() -> Caller {
        Caller {
            user_id: "u".into(),
            agent_urn: "a".into(),
        }
    }
    fn other() -> Caller {
        Caller {
            user_id: "u2".into(),
            agent_urn: "a".into(),
        }
    }
    fn args() -> Map<String, Value> {
        json!({"scheduleId": "sch_a"}).as_object().unwrap().clone()
    }

    #[tokio::test]
    async fn delete_with_the_gateway_up_removes_the_row() {
        let r = rig();
        let gw = Gw::default();
        let out = delete(&r.ctx(), &gw, &me(), &args()).await.unwrap();
        assert_eq!(out["deleted"], json!(true));
        assert!(r.store.get_schedule("sch_a").unwrap().is_none());
        assert!(r.store.due_revokes(i64::MAX).unwrap().is_empty());
        assert_eq!(*gw.revokes.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn delete_with_the_gateway_down_marks_deleting_bumps_the_version_and_drains_later() {
        let r = rig();
        let gw = Gw::default();
        *gw.revoke_down.lock().unwrap() = true;
        let v0 = r.store.get_schedule("sch_a").unwrap().unwrap().version;
        let out = delete(&r.ctx(), &gw, &me(), &args()).await.unwrap();
        assert_eq!(out["state"], json!("deleting"));
        let row = r.store.get_schedule("sch_a").unwrap().unwrap();
        assert_eq!((row.state.as_str(), row.enabled), ("deleting", false));
        assert_eq!(row.next_fire_at, None);
        assert_eq!(row.version, v0 + 1);
        assert_eq!(r.store.due_revokes(i64::MAX).unwrap().len(), 1);
        assert!(r.store.due_schedules(i64::MAX, 10).unwrap().is_empty());
        // A repeat delete is idempotent; the drain's success then removes the row.
        delete(&r.ctx(), &gw, &me(), &args()).await.unwrap();
        assert_eq!(r.store.due_revokes(i64::MAX).unwrap().len(), 1);
        *gw.revoke_down.lock().unwrap() = false;
        let out = delete(&r.ctx(), &gw, &me(), &args()).await.unwrap();
        assert_eq!(out["deleted"], json!(true));
        assert!(r.store.get_schedule("sch_a").unwrap().is_none());
        assert!(r.store.due_revokes(i64::MAX).unwrap().is_empty());
    }

    #[tokio::test]
    async fn only_the_owner_and_creator_may_delete_or_run() {
        let r = rig();
        let gw = Gw::default();
        let e = delete(&r.ctx(), &gw, &other(), &args()).await.unwrap_err();
        assert_eq!(e, ToolErr::not_found());
        let e = run_now(&r.ctx(), &gw, &other(), &args(), "run1")
            .await
            .unwrap_err();
        assert_eq!(e, ToolErr::not_found());
        assert_eq!(*gw.revokes.lock().unwrap(), 0);
        assert!(gw.fires.lock().unwrap().is_empty());
        assert!(r.store.get_schedule("sch_a").unwrap().is_some());
    }

    #[tokio::test]
    async fn run_now_fires_once_on_its_own_ledger_row_and_leaves_the_schedule_alone() {
        let r = rig();
        let gw = Gw::default();
        *gw.fire_answer.lock().unwrap() = Some(Ok(FireOutcome::Accepted {
            run_id: "urn:vam:run:x".into(),
        }));
        let before = r.store.get_schedule("sch_a").unwrap().unwrap();
        let out = run_now(&r.ctx(), &gw, &me(), &args(), "run1")
            .await
            .unwrap();
        assert_eq!(out["outcome"], json!("accepted"));
        assert_eq!(out["runId"], json!("urn:vam:run:x"));
        let f = gw.fires.lock().unwrap()[0].clone();
        assert_eq!(f.kind, FireKind::RunNow);
        assert_eq!(f.trigger_ref, "sch_a");
        assert_eq!(f.not_after, Some(f.fire_at + 86_400));
        assert_eq!(f.fire_at, r.clock.now().timestamp());
        assert_eq!(f.idempotency_key, run_now_key("sch_a", f.fire_at, "run1"));
        let fires = r.store.fires_for("sch_a").unwrap();
        assert_eq!(fires.len(), 1);
        assert_eq!(fires[0].kind, "run_now");
        assert_eq!(fires[0].outcome.as_deref(), Some("accepted"));
        assert_eq!(fires[0].run_id.as_deref(), Some("urn:vam:run:x"));
        let after = r.store.get_schedule("sch_a").unwrap().unwrap();
        assert_eq!(
            (after.next_fire_at, after.version, after.state),
            (before.next_fire_at, before.version, before.state)
        );
    }

    #[test]
    fn run_now_keys_differ_by_calling_run_and_repeat_for_a_retry() {
        assert_eq!(run_now_key("s", 5, "r1"), run_now_key("s", 5, "r1"));
        assert_ne!(run_now_key("s", 5, "r1"), run_now_key("s", 5, "r2"));
        assert_ne!(run_now_key("s", 5, "r1"), run_now_key("s", 6, "r1"));
    }

    #[tokio::test]
    async fn run_now_refusals_are_errors_and_a_deny_leaves_the_state() {
        let r = rig();
        let gw = Gw::default();
        for (answer, word) in [
            (
                Ok(FireOutcome::Denied {
                    reason: vam_spec::v1::DenyReason::Revoked,
                }),
                "denied",
            ),
            (
                Ok(FireOutcome::Busy {
                    reason: vam_spec::v1::BusyReason::Unspecified,
                    retry_after_ms: 0,
                }),
                "skipped_busy",
            ),
            (
                Err(GatewayError::Timeout(Duration::from_secs(1))),
                "failed_unavailable",
            ),
        ] {
            *gw.fire_answer.lock().unwrap() = Some(answer);
            let e = run_now(&r.ctx(), &gw, &me(), &args(), "run1")
                .await
                .unwrap_err();
            assert!(e.message.contains(word), "{}", e.message);
        }
        assert_eq!(
            r.store.get_schedule("sch_a").unwrap().unwrap().state,
            "active"
        );
    }

    #[tokio::test]
    async fn run_now_refuses_a_deleting_schedule() {
        let r = rig();
        let gw = Gw::default();
        r.store.mark_deleting("sch_a", 1).unwrap();
        let e = run_now(&r.ctx(), &gw, &me(), &args(), "r")
            .await
            .unwrap_err();
        assert_eq!(e.code, vam_spec::v1::VamErrorCode::FailedPrecondition);
        assert!(gw.fires.lock().unwrap().is_empty());
    }
}
