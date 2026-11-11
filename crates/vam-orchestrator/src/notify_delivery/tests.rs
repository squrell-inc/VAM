use std::sync::{Arc, Mutex};

use serde_json::json;
use vam_spec::v1::hosted_notification_channel_server::{
    HostedNotificationChannel, HostedNotificationChannelServer,
};
use vam_spec::v1::{NotificationChannel, TrustTier};

use super::*;
use crate::notification_prefs::QuietHours;
use crate::notifications::{record, resolve, resolve_for_run, Acted, NewNotification};
use crate::secrets::SecretKind;

/// 2026-11-04 00:00:00 UTC.
const DAY: i64 = 1_793_750_400;
const BOT: &str = "xoxb-TOP-SECRET-bot-value";
const APP: &str = "xapp-TOP-SECRET-app-value";
const PREVIEW: &str = "AGENT-WRITTEN-PREVIEW-9f3a";

struct Fx {
    store: Arc<SqliteWorkflowStore>,
    vault: Arc<SecretsVault>,
}

fn fx() -> Fx {
    let store = Arc::new(SqliteWorkflowStore::in_memory().expect("store"));
    let vault = SecretsVault::for_tests(store.clone());
    Fx { store, vault }
}

fn channel_manifest(broker: &str) -> CapabilityManifest {
    CapabilityManifest {
        broker_id: broker.into(),
        trust_tier: TrustTier::FirstParty as i32,
        notification_channel: Some(NotificationChannel {
            channel: "slack".into(),
            surface: "slack".into(),
            max_actions_per_minute: 0,
        }),
        ..Default::default()
    }
}

impl Fx {
    /// Slack enabled with two vault secrets and an approved channel broker.
    fn slack(&self) {
        self.vault
            .put_secret("notify/bot", BOT, SecretKind::Egress, &[], None)
            .unwrap();
        self.vault
            .put_secret("notify/app", APP, SecretKind::Egress, &[], None)
            .unwrap();
        self.settings(true);
        self.store
            .upsert_hsp_broker_approval(&channel_manifest("vam.notify"))
            .unwrap();
    }

    fn settings(&self, enabled: bool) {
        let mut s = NodeNotifySettings::default();
        s.slack.enabled = enabled;
        s.slack.bot_token_ref = Some("notify/bot".into());
        s.slack.app_token_ref = Some("notify/app".into());
        s.app_url = Some("https://vam.example.test".into());
        self.store.put_node_notify_settings(&s).unwrap();
    }

    /// A verified Slack contact for `user`.
    fn contact(&self, user: &str, account: &str) -> String {
        let (c, code) = self
            .store
            .start_contact_link(user, "slack", Some(account))
            .unwrap();
        assert!(self
            .store
            .verify_contact_with_code(&code, account)
            .unwrap()
            .is_verified());
        c.id
    }

    fn prefs(&self, user: &str, p: &Prefs) {
        self.store.put_notification_prefs(user, p).unwrap();
    }

    fn rows(&self, sql_where: &str) -> Vec<(String, String, String, Option<String>)> {
        let c = self.store.connection.lock().unwrap();
        let mut stmt = c
            .prepare(&format!(
                "SELECT id, kind, state, hold FROM notification_deliveries {sql_where}
                 ORDER BY created_at, id"
            ))
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        rows
    }

    fn one<T: rusqlite::types::FromSql>(&self, sql: &str) -> T {
        let c = self.store.connection.lock().unwrap();
        c.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    /// Inserts a record the way `notifications::record` would, at a chosen time, then makes the
    /// delivery rows for that time.
    fn raw(&self, id: &str, user: &str, kind: Kind, approval: Option<&str>, at: i64) -> usize {
        let c = self.store.connection.lock().unwrap();
        c.execute(
            "INSERT INTO notifications (id, user_id, kind, severity, subject_key, run_id,
                approval_id, agent_label, title, body, action, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?1, 'run-1', ?5, 'researcher', ?6, 'body', '{}', ?7, ?7)",
            params![
                id,
                user,
                kind.as_str(),
                kind.severity().as_str(),
                approval,
                format!("Title of {id}"),
                at
            ],
        )
        .unwrap();
        on_recorded_at(&c, id, at).unwrap()
    }

    fn approval(&self, id: &str, kind: &str, tool: &str, args: &Value) {
        self.store
            .record_published_approval_of_kind(id, "run-1", "local", &args.to_string(), kind, tool)
            .unwrap();
    }
}

fn note(user: &str, subject: &str, approval: &str) -> NewNotification {
    let (title, body) =
        crate::notifications::render(Kind::ApprovalPending, "researcher", "write_file", None);
    NewNotification {
        user_id: user.into(),
        kind: Kind::ApprovalPending,
        severity: Severity::Action,
        subject_key: subject.into(),
        run_id: Some("run-1".into()),
        space_id: None,
        thread_id: None,
        approval_id: Some(approval.into()),
        grant_id: None,
        receipt_digest: None,
        agent_urn: String::new(),
        agent_label: "researcher".into(),
        via: None,
        title,
        body,
        action: json!({"verb": "decide"}),
    }
}

fn put(f: &Fx, n: &NewNotification) -> String {
    let c = f.store.connection.lock().unwrap();
    match record(&c, n).unwrap() {
        crate::notifications::RecordOutcome::Inserted(id)
        | crate::notifications::RecordOutcome::Updated(id) => id,
    }
}

fn quiet() -> Prefs {
    Prefs {
        quiet_hours: Some(QuietHours {
            start: "22:00".into(),
            end: "07:00".into(),
            tz: "UTC".into(),
        }),
        ..Prefs::default()
    }
}

fn sent(code: DeliverOutcome) -> DeliverNotificationResponse {
    DeliverNotificationResponse {
        outcome: code as i32,
        ..Default::default()
    }
}

fn retry(code: &str, after_ms: i32) -> DeliverNotificationResponse {
    DeliverNotificationResponse {
        outcome: DeliverOutcome::Retry as i32,
        error_code: code.into(),
        retry_after_ms: after_ms,
        ..Default::default()
    }
}

fn due_of(f: &Fx, id: &str) -> Due {
    let c = f.store.connection.lock().unwrap();
    c.query_row(
        "SELECT id, notification_id, user_id, contact_id, channel, kind, update_of, attempts,
                created_at FROM notification_deliveries WHERE id = ?1",
        [id],
        |r| {
            Ok(Due {
                id: r.get(0)?,
                notification_id: r.get(1)?,
                user_id: r.get(2)?,
                contact_id: r.get(3)?,
                channel: r.get(4)?,
                kind: r.get(5)?,
                update_of: r.get(6)?,
                attempts: r.get(7)?,
                created_at: r.get(8)?,
            })
        },
    )
    .unwrap()
}

// ---- who gets a row ---------------------------------------------------------------------------

#[test]
fn an_unverified_or_revoked_contact_gets_no_row() {
    let f = fx();
    f.slack();
    // Pending: the user started linking and never confirmed.
    let (pending, _code) = f
        .store
        .start_contact_link("u1", "slack", Some("T1/U1"))
        .unwrap();
    assert_eq!(
        f.raw("n1", "u1", Kind::ApprovalPending, Some("ap1"), DAY),
        0
    );
    assert!(f.rows("").is_empty());
    // Verified: one row. Another user without a contact: none.
    let (_, code) = f
        .store
        .start_contact_link("u1", "slack", Some("T1/U1"))
        .unwrap();
    f.store.verify_contact_with_code(&code, "T1/U1").unwrap();
    assert_eq!(
        f.raw("n2", "u1", Kind::ApprovalPending, Some("ap2"), DAY),
        1
    );
    assert_eq!(
        f.raw("n3", "u2", Kind::ApprovalPending, Some("ap3"), DAY),
        0
    );
    // Revoked after: none.
    let id = f.rows("")[0].0.clone();
    let contact = due_of(&f, &id).contact_id;
    f.store.unlink_contact("u1", &contact).unwrap();
    assert_ne!(pending.id, contact);
    assert_eq!(
        f.raw("n4", "u1", Kind::ApprovalPending, Some("ap4"), DAY),
        0
    );
    assert_eq!(f.rows("").len(), 1);
}

#[test]
fn a_disabled_adapter_or_a_missing_broker_gets_no_row() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    assert_eq!(f.raw("n1", "u1", Kind::ApprovalPending, Some("a1"), DAY), 1);
    // Switched off: no row.
    f.settings(false);
    assert_eq!(f.raw("n2", "u1", Kind::ApprovalPending, Some("a2"), DAY), 0);
    // Back on, but the broker is disabled.
    f.settings(true);
    f.store.disable_hsp_broker_approval("vam.notify").unwrap();
    assert_eq!(f.raw("n3", "u1", Kind::ApprovalPending, Some("a3"), DAY), 0);
    // A broker without the channel block is no broker for slack.
    let mut other = channel_manifest("other.notify");
    other.notification_channel = None;
    f.store.upsert_hsp_broker_approval(&other).unwrap();
    assert_eq!(f.raw("n4", "u1", Kind::ApprovalPending, Some("a4"), DAY), 0);
    assert_eq!(f.rows("").len(), 1);
}

#[test]
fn preferences_and_node_defaults_decide_which_kinds_are_sent() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    // trigger.fired is off by default; budget.hit is on.
    assert_eq!(f.raw("n1", "u1", Kind::TriggerFired, None, DAY), 0);
    assert_eq!(f.raw("n2", "u1", Kind::BudgetHit, None, DAY), 1);
    f.prefs(
        "u1",
        &Prefs {
            kinds: [("budget.hit".to_string(), "off".to_string())].into(),
            ..Prefs::default()
        },
    );
    assert_eq!(f.raw("n3", "u1", Kind::BudgetHit, None, DAY), 0);
}

#[test]
fn quiet_hours_hold_everything_but_always_kinds() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    let night = DAY + 23 * 3600;
    f.prefs("u1", &quiet());
    assert_eq!(
        f.raw("n1", "u1", Kind::ApprovalPending, Some("a1"), night),
        1
    );
    let held = f.rows("WHERE notification_id = 'n1'");
    assert_eq!(
        (held[0].2.as_str(), held[0].3.as_deref()),
        ("held", Some("quiet"))
    );
    let nb: i64 =
        f.one("SELECT not_before FROM notification_deliveries WHERE notification_id = 'n1'");
    assert_eq!(nb, DAY + 31 * 3600, "held until 07:00 the next morning");
    // `always` goes out at once; so does anything outside the window.
    let mut p = quiet();
    p.always = vec!["approval.pending".into()];
    f.prefs("u1", &p);
    f.raw("n2", "u1", Kind::ApprovalPending, Some("a2"), night);
    assert_eq!(f.rows("WHERE notification_id = 'n2'")[0].2, "pending");
    f.raw("n3", "u1", Kind::BudgetHit, None, night);
    assert_eq!(f.rows("WHERE notification_id = 'n3'")[0].2, "held");
    f.raw("n4", "u1", Kind::BudgetHit, None, DAY + 12 * 3600);
    assert_eq!(f.rows("WHERE notification_id = 'n4'")[0].2, "pending");
}

#[test]
fn held_rows_release_at_the_end_as_one_message() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    let night = DAY + 23 * 3600;
    f.prefs("u1", &quiet());
    for i in 0..3 {
        f.raw(&format!("n{i}"), "u1", Kind::BudgetHit, None, night + i);
    }
    // Before the window ends: nothing moves.
    assert_eq!(release_held(&f.store, DAY + 30 * 3600).unwrap(), 0);
    assert!(due_rows(&f.store, DAY + 30 * 3600, 50).unwrap().is_empty());
    // At the end: one summary, the three folded into it.
    let at = DAY + 31 * 3600;
    assert_eq!(release_held(&f.store, at).unwrap(), 3);
    let due = due_rows(&f.store, at, 50).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].kind, "summary");
    let folded = f.rows("WHERE state = 'suppressed'");
    assert_eq!(folded.len(), 3);
    let into: i64 = f.one(&format!(
        "SELECT COUNT(*) FROM notification_deliveries WHERE folded_into = '{}'",
        due[0].id
    ));
    assert_eq!(into, 3);
    // A lone held row goes out as itself.
    let next_night = DAY + 24 * 3600 + 23 * 3600;
    f.raw("solo", "u1", Kind::BudgetHit, None, next_night);
    let at = DAY + 2 * 24 * 3600 + 7 * 3600;
    assert_eq!(release_held(&f.store, at).unwrap(), 1);
    let solo = f.rows("WHERE notification_id = 'solo'");
    assert_eq!(solo[0].1, "post");
    assert_eq!(solo[0].2, "pending");
}

#[test]
fn a_held_decision_released_after_quiet_hours_goes_out_with_a_fresh_handle() {
    let f = fx();
    f.slack();
    f.store.create_user("u1", "u1").unwrap();
    let contact = f.contact("u1", "T1/U1");
    f.store
        .set_hsp_broker_endpoint("vam.notify", &HspEndpoint::Unix("/nonexistent.sock".into()))
        .unwrap();
    f.approval("ap1", "tool_call", "write_file", &json!({}));
    f.prefs("u1", &quiet());
    let night = DAY + 23 * 3600;
    assert_eq!(
        f.raw("n1", "u1", Kind::ApprovalPending, Some("ap1"), night),
        1
    );
    assert_eq!(f.rows("WHERE notification_id = 'n1'")[0].2, "held");
    // Held: no handle exists yet.
    let handles = |f: &Fx| -> i64 {
        f.one("SELECT COUNT(*) FROM api_keys WHERE issued_via = 'notification'")
    };
    assert_eq!(handles(&f), 0);
    let at = DAY + 31 * 3600;
    assert_eq!(release_held(&f.store, at).unwrap(), 1);
    let due = due_rows(&f.store, at, 50).unwrap().remove(0);
    assert_eq!(
        (due.kind.as_str(), due.notification_id.as_str()),
        ("post", "n1")
    );
    let Prepared::Send { request, .. } = prepare(&f.store, &f.vault, &due, at) else {
        panic!("the released post should be sendable");
    };
    assert!(request.action_handle.starts_with("vam_na_"));
    let r = request.rendering.as_ref().unwrap();
    assert_eq!(r.actions.len(), 2, "Approve and Deny");
    assert_eq!(handles(&f), 1);
    // The handle is bound to the contact and the record, and is live.
    let c = f.store.connection.lock().unwrap();
    let found =
        crate::credentials::find_action_handle(&c, &request.action_handle, "vam.notify").unwrap();
    assert_eq!(
        (found.notification_id.as_str(), found.contact_id.as_str()),
        ("n1", contact.as_str())
    );
    assert!(found.revoked_at.is_none());
}

#[test]
fn a_held_decision_that_resolved_meanwhile_is_not_sent() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    f.prefs("u1", &quiet());
    let night = DAY + 23 * 3600;
    f.raw("n1", "u1", Kind::ApprovalPending, Some("a1"), night);
    {
        let c = f.store.connection.lock().unwrap();
        c.execute(
            "UPDATE notifications SET resolved_at = ?1, resolved_reason = 'acted' WHERE id = 'n1'",
            [night + 60],
        )
        .unwrap();
        on_resolved_at(&c, Resolved::Notification("n1"), night + 60).unwrap();
    }
    let row = &f.rows("WHERE notification_id = 'n1'")[0];
    assert_eq!(row.2, "suppressed");
    assert_eq!(
        f.rows("WHERE kind = 'update'").len(),
        0,
        "nothing was sent to update"
    );
}

// ---- the rate fold ---------------------------------------------------------------------------

#[test]
fn the_thirty_first_post_in_an_hour_is_held_and_folds_with_the_rest() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    for i in 0..30 {
        assert_eq!(
            f.raw(&format!("n{i:02}"), "u1", Kind::BudgetHit, None, DAY + i),
            1
        );
    }
    assert_eq!(f.rows("WHERE state = 'pending'").len(), 30);
    f.raw("n30", "u1", Kind::BudgetHit, None, DAY + 100);
    let r = &f.rows("WHERE notification_id = 'n30'")[0];
    assert_eq!((r.2.as_str(), r.3.as_deref()), ("held", Some("rate")));
    let nb: i64 =
        f.one("SELECT not_before FROM notification_deliveries WHERE notification_id = 'n30'");
    assert_eq!(
        nb,
        DAY + 3600,
        "free when the oldest of the thirty leaves the window"
    );
    // Another user is not affected.
    f.contact("u2", "T1/U2");
    assert_eq!(f.raw("m0", "u2", Kind::BudgetHit, None, DAY + 100), 1);
    assert_eq!(f.rows("WHERE notification_id = 'm0'")[0].2, "pending");
    f.raw("n31", "u1", Kind::BudgetHit, None, DAY + 101);
    f.raw("n32", "u1", Kind::BudgetHit, None, DAY + 102);
    // Released together as one "and N more" message.
    let at = DAY + 3600;
    assert_eq!(release_held(&f.store, at).unwrap(), 3);
    let summaries = f.rows("WHERE kind = 'summary'");
    assert_eq!(summaries.len(), 1);
    let members: Vec<Facts> = {
        let c = f.store.connection.lock().unwrap();
        summary_members(&c, &summaries[0].0).unwrap()
    };
    assert_eq!(members.len(), 3);
    let r = build_summary(&members, Some("https://vam.example.test"));
    assert_eq!(r.title, "3 notifications");
    assert!(r.actions.is_empty() && r.jig_ir.is_none() && r.agent_text.is_none());
}

#[test]
fn a_summary_lists_ten_then_says_and_n_more() {
    let members: Vec<Facts> = (0..13)
        .map(|i| Facts {
            id: format!("n{i}"),
            kind: Kind::BudgetHit,
            approval_id: None,
            agent_label: String::new(),
            title: format!("Budget {i}"),
            body: String::new(),
            resolved_at: None,
            resolved_reason: None,
            acted_by: None,
            acted_surface: None,
        })
        .collect();
    let r = build_summary(&members, None);
    assert_eq!(r.title, "13 notifications");
    assert_eq!(r.body.lines().count(), 11);
    assert!(r.body.ends_with("and 3 more"));
    assert!(r.open_url.is_empty());
}

// ---- updates on resolve ----------------------------------------------------------------------

#[test]
fn a_resolve_queues_one_update_per_sent_post_and_suppresses_the_rest() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    let n1 = put(&f, &note("u1", "s1", "ap1"));
    let n2 = put(&f, &note("u1", "s2", "ap2"));
    let post1 = f.rows(&format!("WHERE notification_id = '{n1}'"))[0]
        .0
        .clone();
    let post2 = f.rows(&format!("WHERE notification_id = '{n2}'"))[0]
        .0
        .clone();
    // The first was sent; the second was not.
    let due1 = due_of(&f, &post1);
    assert_eq!(
        finish(&f.store, &due1, Ok(sent(DeliverOutcome::Sent)), DAY).unwrap(),
        "sent"
    );
    {
        let c = f.store.connection.lock().unwrap();
        let by = || {
            Some(Acted {
                surface: "app",
                by_user_id: "u1",
            })
        };
        resolve(&c, "ap1", "acted", by()).unwrap();
        resolve(&c, "ap2", "acted", by()).unwrap();
        // A second resolve of the same thing changes nothing.
        resolve(&c, "ap1", "acted", by()).unwrap();
    }
    let updates = f.rows("WHERE kind = 'update'");
    assert_eq!(updates.len(), 1);
    assert_eq!(updates[0].2, "pending");
    let original: String =
        f.one("SELECT update_of FROM notification_deliveries WHERE kind = 'update'");
    assert_eq!(original, post1);
    assert_eq!(f.rows(&format!("WHERE id = '{post2}'"))[0].2, "suppressed");
    // resolve_for_run reaches the rest of a run's records too.
    let n3 = put(&f, &note("u1", "s3", "ap3"));
    let post3 = f.rows(&format!("WHERE notification_id = '{n3}'"))[0]
        .0
        .clone();
    finish(
        &f.store,
        &due_of(&f, &post3),
        Ok(sent(DeliverOutcome::Sent)),
        DAY,
    )
    .unwrap();
    {
        let c = f.store.connection.lock().unwrap();
        resolve_for_run(&c, "run-1", "run_ended").unwrap();
    }
    assert_eq!(f.rows("WHERE kind = 'update'").len(), 2);
}

#[test]
fn an_update_says_who_decided_where_and_offers_nothing() {
    let facts = Facts {
        id: "n1".into(),
        kind: Kind::ApprovalPending,
        approval_id: Some("ap1".into()),
        agent_label: "researcher".into(),
        title: "Approve write_file".into(),
        body: "body".into(),
        resolved_at: Some(DAY + 9 * 3600 + 12 * 60),
        resolved_reason: Some("acted".into()),
        acted_by: Some("u1".into()),
        acted_surface: Some("app".into()),
    };
    let r = build_update(&facts, "alice", Some("https://vam.example.test"));
    assert_eq!(r.title, "Decided by alice in app at 2026-11-04 09:12 UTC");
    assert!(r.actions.is_empty() && r.jig_ir.is_none() && r.agent_text.is_none());
    assert_eq!(r.body, "Approve write_file");
    let mut expired = facts.clone();
    expired.acted_surface = None;
    expired.resolved_reason = Some("expired".into());
    assert!(build_update(&expired, "", None)
        .title
        .starts_with("No longer waiting (expired)"));
}

#[test]
fn a_post_that_resolved_while_it_was_in_flight_still_gets_its_update() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    let n = put(&f, &note("u1", "s1", "ap1"));
    let post = f.rows(&format!("WHERE notification_id = '{n}'"))[0]
        .0
        .clone();
    let due = due_of(&f, &post);
    // The record resolved (by a path that has not reached the hook yet) while the post was
    // leaving; the send then returns SENT: the update is queued by the send's own completion.
    {
        let c = f.store.connection.lock().unwrap();
        c.execute(
            "UPDATE notifications SET resolved_at = ?1, resolved_reason = 'acted',
                acted_surface = 'cli', acted_by_user_id = 'u1' WHERE id = ?2",
            params![DAY, n],
        )
        .unwrap();
    }
    finish(&f.store, &due, Ok(sent(DeliverOutcome::Sent)), DAY + 1).unwrap();
    assert_eq!(f.rows("WHERE kind = 'update'").len(), 1);
}

// ---- retries ---------------------------------------------------------------------------------

#[test]
fn retries_follow_the_schedule_and_end_failed_after_a_day() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    f.raw("n1", "u1", Kind::BudgetHit, None, DAY);
    let id = f.rows("")[0].0.clone();
    let mut now = DAY;
    let mut waits = Vec::new();
    loop {
        let due = due_of(&f, &id);
        let state = finish(&f.store, &due, Ok(retry("ratelimited", 0)), now).unwrap();
        if state == "failed" {
            break;
        }
        let nb: i64 = f.one("SELECT not_before FROM notification_deliveries");
        waits.push(nb - now);
        now = nb;
        assert!(waits.len() < 100, "must end");
    }
    assert_eq!(&waits[..6], &[30, 120, 600, 1800, 3600, 3600]);
    assert!(waits[6..].iter().all(|w| *w == 3600));
    assert!(now - DAY <= GIVE_UP_SECS && now - DAY > GIVE_UP_SECS - 3600);
    let (state, err): (String, String) = {
        let c = f.store.connection.lock().unwrap();
        c.query_row(
            "SELECT state, last_error FROM notification_deliveries",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!((state.as_str(), err.as_str()), ("failed", "gave_up"));
    // Never sent, so the record was never shown.
    let shown: Option<i64> = f.one("SELECT first_shown_at FROM notifications WHERE id = 'n1'");
    assert!(shown.is_none());
}

#[test]
fn permanent_errors_fail_at_once_and_retry_after_is_honoured() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    for (i, code) in ["user_not_found", "account_inactive", "invalid_auth"]
        .iter()
        .enumerate()
    {
        let id = format!("n{i}");
        f.raw(&id, "u1", Kind::BudgetHit, None, DAY + i as i64);
        let post = f.rows(&format!("WHERE notification_id = '{id}'"))[0]
            .0
            .clone();
        // Even when the broker calls it a retry.
        assert_eq!(
            finish(&f.store, &due_of(&f, &post), Ok(retry(code, 0)), DAY).unwrap(),
            "failed"
        );
        let err: String = f.one(&format!(
            "SELECT last_error FROM notification_deliveries WHERE id = '{post}'"
        ));
        assert_eq!(err.as_str(), *code);
    }
    f.raw("slow", "u1", Kind::BudgetHit, None, DAY + 10);
    let post = f.rows("WHERE notification_id = 'slow'")[0].0.clone();
    finish(
        &f.store,
        &due_of(&f, &post),
        Ok(retry("ratelimited", 600_000)),
        DAY,
    )
    .unwrap();
    let nb: i64 = f.one(&format!(
        "SELECT not_before FROM notification_deliveries WHERE id = '{post}'"
    ));
    assert_eq!(nb, DAY + 600, "Retry-After beyond the schedule wins");
    // An unsupported adapter ends the row.
    let due = due_of(&f, &post);
    let unsupported = DeliverNotificationResponse {
        outcome: DeliverOutcome::Unsupported as i32,
        ..Default::default()
    };
    assert_eq!(
        finish(&f.store, &due, Ok(unsupported), DAY + 700).unwrap(),
        "failed"
    );
    // A transport error is a retry with a code, never the error text.
    f.raw("net", "u1", Kind::BudgetHit, None, DAY + 11);
    let post = f.rows("WHERE notification_id = 'net'")[0].0.clone();
    let err = Err(OrchestratorError::Hsp(
        "uds connect failed: secret-looking text".into(),
    ));
    assert_eq!(
        finish(&f.store, &due_of(&f, &post), err, DAY).unwrap(),
        "pending"
    );
    let code: String = f.one(&format!(
        "SELECT last_error FROM notification_deliveries WHERE id = '{post}'"
    ));
    assert_eq!(code, "transport");
}

#[test]
fn a_sent_post_marks_the_record_shown_once() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    f.raw("n1", "u1", Kind::BudgetHit, None, DAY);
    let post = f.rows("")[0].0.clone();
    finish(
        &f.store,
        &due_of(&f, &post),
        Ok(sent(DeliverOutcome::Duplicate)),
        DAY + 5,
    )
    .unwrap();
    let shown: Option<i64> = f.one("SELECT first_shown_at FROM notifications WHERE id = 'n1'");
    assert_eq!(shown, Some(DAY + 5));
    let due = due_of(&f, &post);
    {
        let c = f.store.connection.lock().unwrap();
        mark_shown(&c, &due, DAY + 99).unwrap();
    }
    let shown: Option<i64> = f.one("SELECT first_shown_at FROM notifications WHERE id = 'n1'");
    assert_eq!(shown, Some(DAY + 5), "the first time stays");
}

#[test]
fn a_failed_record_write_rolls_the_delivery_rows_back() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    {
        let c = f.store.connection.lock().unwrap();
        let tx = c.unchecked_transaction().unwrap();
        record(&tx, &note("u1", "s1", "ap1")).unwrap();
        let n: i64 = tx
            .query_row("SELECT COUNT(*) FROM notification_deliveries", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
        // The caller fails: the transaction is dropped.
    }
    assert!(f.rows("").is_empty());
    let c: i64 = f.one("SELECT COUNT(*) FROM notifications");
    assert_eq!(c, 0);
}

// ---- no secret at rest ------------------------------------------------------------------------

#[test]
fn no_table_holds_a_token_an_address_or_agent_text() {
    let f = fx();
    f.slack();
    // A handle is owned by a real user (api_keys.owner_user_id is a foreign key).
    f.store.create_user("u1", "u1").unwrap();
    f.contact("u1", "T1/U1");
    f.approval(
        "ap1",
        "tool_call",
        "write_file",
        &json!({ "path": PREVIEW }),
    );
    let n = put(&f, &note("u1", "s1", "ap1"));
    let post = f.rows(&format!("WHERE notification_id = '{n}'"))[0]
        .0
        .clone();
    // Build the request the worker would send: the token is in it ...
    f.store
        .set_hsp_broker_endpoint("vam.notify", &HspEndpoint::Unix("/nonexistent.sock".into()))
        .unwrap();
    let Prepared::Send { request, .. } = prepare(&f.store, &f.vault, &due_of(&f, &post), DAY)
    else {
        panic!("expected a send");
    };
    assert_eq!(request.resolved_secrets[BOT_TOKEN_KEY], BOT);
    assert_eq!(request.channel_address, "T1/U1");
    assert!(request
        .rendering
        .as_ref()
        .unwrap()
        .agent_text
        .as_ref()
        .unwrap()
        .text
        .contains(PREVIEW));
    // ... and nothing else holds it, after the send is recorded too.
    finish(
        &f.store,
        &due_of(&f, &post),
        Ok(sent(DeliverOutcome::Sent)),
        DAY,
    )
    .unwrap();
    let schema: String =
        f.one("SELECT sql FROM sqlite_master WHERE name = 'notification_deliveries'");
    for banned in [
        "token",
        "secret",
        "handle",
        "address",
        "external",
        "message_id",
    ] {
        assert!(
            !schema.to_ascii_lowercase().contains(banned),
            "schema has `{banned}`: {schema}"
        );
    }
    let c = f.store.connection.lock().unwrap();
    let tables: Vec<String> = c
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for t in tables {
        let mut stmt = c.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
        let cols = stmt.column_count();
        let mut rows = stmt.query([]).unwrap();
        while let Some(r) = rows.next().unwrap() {
            for i in 0..cols {
                let v: rusqlite::types::Value = r.get(i).unwrap();
                let text = match v {
                    rusqlite::types::Value::Text(s) => s,
                    rusqlite::types::Value::Blob(b) => String::from_utf8_lossy(&b).to_string(),
                    _ => continue,
                };
                assert!(
                    !text.contains(BOT) && !text.contains(APP),
                    "{t} holds a token"
                );
                if t == "notification_deliveries" || t == "notifications" {
                    assert!(!text.contains(PREVIEW), "{t} holds agent text");
                }
                if t == "notification_deliveries" {
                    assert!(!text.contains("T1/U1"), "{t} holds an address");
                }
            }
        }
    }
}

// ---- rendering --------------------------------------------------------------------------------

fn facts(kind: Kind, approval: &str) -> Facts {
    Facts {
        id: "nt_1".into(),
        kind,
        approval_id: Some(approval.into()),
        agent_label: "researcher".into(),
        title: "Host title".into(),
        body: "Host body".into(),
        resolved_at: None,
        resolved_reason: None,
        acted_by: None,
        acted_surface: None,
    }
}

fn approval(kind: &str, tool: &str, args: Value) -> ApprovalFacts {
    ApprovalFacts {
        kind: kind.into(),
        tool_name: tool.into(),
        requested_args_json: args.to_string(),
    }
}

fn ir_json(r: &NotificationRendering) -> String {
    r.jig_ir
        .as_ref()
        .map(|s| vam_spec::harness::struct_to_json(s).to_string())
        .unwrap_or_default()
}

#[test]
fn a_tool_approval_offers_approve_and_deny_and_never_always() {
    let a = approval("tool_call", "write_file", json!({"path": PREVIEW}));
    let r = build_rendering(
        &facts(Kind::ApprovalPending, "ap1"),
        Some(&a),
        Some("https://vam.example.test/"),
    );
    let decisions: Vec<_> = r
        .actions
        .iter()
        .map(|a| (a.label.as_str(), a.decision.as_str()))
        .collect();
    assert_eq!(
        decisions,
        vec![("Approve", "APPROVED"), ("Deny", "REJECTED")]
    );
    assert_eq!(r.title, "Host title");
    assert_eq!(r.body, "Host body");
    assert_eq!(r.open_url, "https://vam.example.test/notifications/nt_1");
    assert_eq!(r.cli_line, "vam approve decide ap1");
    let ir = ir_json(&r);
    assert!(!ir.is_empty(), "the approve jig renders natively");
    for banned in ["Always", "always", "never", "Always deny", PREVIEW] {
        assert!(!ir.contains(banned), "the IR has `{banned}`: {ir}");
    }
    assert!(ir.contains("Approve") && ir.contains("Deny"));
    let text = r.agent_text.expect("agent text");
    assert_eq!(
        text.label,
        "written by researcher, not verified by the host"
    );
    assert!(text.text.contains(PREVIEW));
}

#[test]
fn agent_text_is_capped_labelled_and_cannot_close_the_fence() {
    let long = "é".repeat(2000);
    let t = agent_text("written by x, not verified by the host", &long).unwrap();
    assert!(t.text.len() <= AGENT_TEXT_MAX_BYTES);
    assert!(
        t.text.chars().all(|c| c == 'é'),
        "cut on a character boundary"
    );
    let t = agent_text("l", "before ``` rm -rf ``` after\u{7}").unwrap();
    assert!(!t.text.contains("```") && !t.text.contains('\u{7}'));
    assert!(agent_text("l", "   ").is_none());
}

#[test]
fn a_resolved_record_or_a_plain_kind_offers_no_actions() {
    let a = approval("tool_call", "write_file", json!({}));
    let mut resolved = facts(Kind::ApprovalPending, "ap1");
    resolved.resolved_at = Some(DAY);
    assert!(build_rendering(&resolved, Some(&a), None)
        .actions
        .is_empty());
    let plain = build_rendering(&facts(Kind::BudgetHit, "x"), None, None);
    assert!(plain.actions.is_empty() && plain.jig_ir.is_none() && plain.open_url.is_empty());
    assert_eq!(plain.cli_line, "vam notifications show nt_1");
}

#[test]
fn an_option_gate_is_one_button_per_option_and_no_form() {
    let a = approval(
        "gate",
        "",
        json!({"question": "Go on?", "options": ["go", "stop"], "response_schema": {"type": "object"}}),
    );
    let r = build_rendering(&facts(Kind::GateOpened, "g1"), Some(&a), None);
    let labels: Vec<_> = r
        .actions
        .iter()
        .map(|a| (a.label.as_str(), a.decision.as_str(), a.option.as_str()))
        .collect();
    assert_eq!(
        labels,
        vec![("go", "SUBMITTED", "go"), ("stop", "SUBMITTED", "stop")]
    );
    assert!(r.jig_ir.is_none());
    assert_eq!(r.agent_text.unwrap().label, "from the playbook");
}

#[test]
fn a_simple_schema_gate_renders_natively_and_a_file_gate_does_not() {
    let simple = approval(
        "gate",
        "",
        json!({"question": "Name?", "response_schema": {
            "type": "object", "required": ["name"],
            "properties": {"name": {"type": "string"}, "ok": {"type": "boolean"}}}}),
    );
    let r = build_rendering(&facts(Kind::GateOpened, "g1"), Some(&simple), None);
    let ir = ir_json(&r);
    assert!(ir.contains("Input") || ir.contains("TextArea"), "{ir}");
    assert!(!ir.contains("Name?"), "the question is agent text, not IR");
    assert_eq!(r.actions.len(), 1);
    assert_eq!(r.actions[0].decision, "SUBMITTED");

    let file = approval(
        "gate",
        "",
        json!({"question": "Which file?", "response_schema": {
            "type": "object", "required": ["file"],
            "properties": {"file": {"type": "string", "format": "vam-workspace-file"}}}}),
    );
    let r = build_rendering(&facts(Kind::GateOpened, "g2"), Some(&file), None);
    assert!(r.jig_ir.is_none(), "a file field is not native");
    assert!(
        r.actions.is_empty(),
        "no buttons: title, body and Open in VAM only"
    );
    assert!(!r.title.is_empty());
}

#[test]
fn a_question_renders_as_a_select_with_choices_and_a_text_area_without() {
    let with = approval(
        "question",
        "ask_user",
        json!({"jig": "vam.question", "data": {"question": "Which?", "choices": ["a", "b"]}}),
    );
    let r = build_rendering(&facts(Kind::QuestionAsked, "q1"), Some(&with), None);
    let ir = ir_json(&r);
    assert!(ir.contains("Select") && !ir.contains("TextArea"), "{ir}");
    assert_eq!(r.agent_text.as_ref().unwrap().text, "Which?");
    let without = approval(
        "question",
        "ask_user",
        json!({"jig": "vam.question", "data": {"question": "Why?"}}),
    );
    let ir = ir_json(&build_rendering(
        &facts(Kind::QuestionAsked, "q2"),
        Some(&without),
        None,
    ));
    assert!(ir.contains("TextArea") && !ir.contains("Select"), "{ir}");
}

#[test]
fn native_means_every_primitive_in_the_set_and_only_submit_actions() {
    let ok = json!({"nodes": {"a": {"type": "Stack"}, "b": {"type": "Button", "props": {"action": {"event": "submit"}}}}});
    assert!(renders_natively(&ok));
    for bad in [
        json!({"nodes": {"a": {"type": "FileInput"}}}),
        json!({"nodes": {"a": {"type": "NumberInput"}}}),
        json!({"nodes": {"a": {"type": "ProgressBar"}}}),
        json!({"nodes": {"a": {"type": "Button", "props": {"action": {"event": "set"}}}}}),
        json!({"nodes": {}}),
        json!({}),
    ] {
        assert!(!renders_natively(&bad), "{bad}");
    }
    let many: Vec<_> = (0..101).map(|i| json!(i.to_string())).collect();
    let big = json!({"nodes": {"s": {"type": "Select", "props": {"options": many}}}});
    assert!(!renders_natively(&big));
    for p in SLACK_PRIMITIVES {
        assert!(
            renders_natively(&json!({"nodes": {"a": {"type": p}}})),
            "{p}"
        );
    }
}

// ---- the worker against a broker -------------------------------------------------------------

#[derive(Clone, Default)]
struct FakeChannel {
    seen: Arc<Mutex<Vec<DeliverNotificationRequest>>>,
}

#[tonic::async_trait]
impl HostedNotificationChannel for FakeChannel {
    async fn deliver_notification(
        &self,
        request: tonic::Request<DeliverNotificationRequest>,
    ) -> Result<tonic::Response<DeliverNotificationResponse>, tonic::Status> {
        self.seen.lock().unwrap().push(request.into_inner());
        Ok(tonic::Response::new(sent(DeliverOutcome::Sent)))
    }
}

#[tokio::test]
async fn the_worker_sends_with_the_token_then_the_update_naming_the_original() {
    let f = fx();
    f.slack();
    f.store.create_user("u1", "u1").unwrap();
    f.contact("u1", "T1/U1");
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("b.sock");
    let listener = tokio::net::UnixListener::bind(&sock).unwrap();
    let fake = FakeChannel::default();
    let seen = fake.seen.clone();
    tokio::spawn(
        tonic::transport::Server::builder()
            .add_service(HostedNotificationChannelServer::new(fake))
            .serve_with_incoming(tokio_stream::wrappers::UnixListenerStream::new(listener)),
    );
    f.store
        .set_hsp_broker_endpoint("vam.notify", &HspEndpoint::Unix(sock))
        .unwrap();
    f.approval("ap1", "tool_call", "write_file", &json!({"path": PREVIEW}));
    let n = put(&f, &note("u1", "s1", "ap1"));
    let now = crate::catalog::unix_timestamp();
    assert_eq!(run_once_at(&f.store, &f.vault, now).await, 1);
    let first = {
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        let r = &seen[0];
        assert_eq!(r.channel, "slack");
        assert_eq!(r.channel_address, "T1/U1");
        assert_eq!(r.notification_id, n);
        assert_eq!(
            r.resolved_secrets.get(BOT_TOKEN_KEY).map(String::as_str),
            Some(BOT)
        );
        assert_eq!(
            r.resolved_secrets.len(),
            1,
            "the app token is not sent with a delivery"
        );
        assert_eq!(r.rendering.as_ref().unwrap().actions.len(), 2);
        assert!(r.update_of.is_empty());
        r.delivery_id.clone()
    };
    assert_eq!(f.rows("WHERE state = 'sent'").len(), 1);
    let shown: Option<i64> = f.one(&format!(
        "SELECT first_shown_at FROM notifications WHERE id = '{n}'"
    ));
    assert!(shown.is_some());
    // Nothing due: nothing sent twice.
    assert_eq!(run_once_at(&f.store, &f.vault, now + 1).await, 0);
    // Decided in the app: the update names the original delivery.
    {
        let c = f.store.connection.lock().unwrap();
        resolve(
            &c,
            "ap1",
            "acted",
            Some(Acted {
                surface: "app",
                by_user_id: "u1",
            }),
        )
        .unwrap();
    }
    assert_eq!(run_once_at(&f.store, &f.vault, now + 5).await, 1);
    let seen = seen.lock().unwrap();
    assert_eq!(seen.len(), 2);
    assert_eq!(seen[1].update_of, first);
    let r = seen[1].rendering.as_ref().unwrap();
    assert!(
        r.title.starts_with("Decided by u1 in app at "),
        "{}",
        r.title
    );
    assert!(r.actions.is_empty() && r.jig_ir.is_none());
}

#[tokio::test]
async fn the_worker_retries_without_a_broker_and_suppresses_after_an_unlink() {
    let f = fx();
    f.slack();
    let contact = f.contact("u1", "T1/U1");
    f.approval("ap1", "tool_call", "write_file", &json!({}));
    let n = put(&f, &note("u1", "s1", "ap1"));
    let now = crate::catalog::unix_timestamp();
    // No endpoint recorded for the broker: a retry, with a code.
    assert_eq!(run_once_at(&f.store, &f.vault, now).await, 1);
    let code: String = f.one("SELECT last_error FROM notification_deliveries");
    assert_eq!(code, "broker_unavailable");
    assert_eq!(f.rows("WHERE state = 'pending'").len(), 1);
    // The user unlinks: the next try suppresses it.
    f.store.unlink_contact("u1", &contact).unwrap();
    assert_eq!(run_once_at(&f.store, &f.vault, now + 31).await, 1);
    assert_eq!(f.rows("WHERE state = 'suppressed'").len(), 1);
    let code: String = f.one("SELECT last_error FROM notification_deliveries");
    assert_eq!(code, "contact_gone");
    // A pruned record takes its rows with it.
    {
        let c = f.store.connection.lock().unwrap();
        c.execute("DELETE FROM notifications WHERE id = ?1", [n])
            .unwrap();
    }
    run_once_at(&f.store, &f.vault, now + 40).await;
    assert!(f.rows("").is_empty());
}

#[test]
fn an_unreadable_secret_is_a_retry_with_a_code_not_a_leak() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    f.store
        .set_hsp_broker_endpoint("vam.notify", &HspEndpoint::Unix("/nonexistent.sock".into()))
        .unwrap();
    let mut s = NodeNotifySettings::default();
    s.slack.enabled = true;
    s.slack.bot_token_ref = Some("notify/missing".into());
    s.slack.app_token_ref = Some("notify/app".into());
    f.store.put_node_notify_settings(&s).unwrap();
    f.raw("n1", "u1", Kind::BudgetHit, None, DAY);
    let post = f.rows("")[0].0.clone();
    let p = prepare(&f.store, &f.vault, &due_of(&f, &post), DAY);
    assert!(matches!(p, Prepared::Retry("secret_missing")), "{p:?}");
}

#[test]
fn an_mcp_credential_named_as_the_bot_token_is_refused_and_never_read() {
    let f = fx();
    f.slack();
    f.contact("u1", "T1/U1");
    f.store
        .set_hsp_broker_endpoint("vam.notify", &HspEndpoint::Unix("/nonexistent.sock".into()))
        .unwrap();
    f.vault
        .put_secret(
            "e2e-mcp/oauth/access-token",
            "MCP-TOKEN-VALUE",
            SecretKind::Connection,
            &["mcp.example.com".to_string()],
            None,
        )
        .unwrap();
    let mut s = NodeNotifySettings::default();
    s.slack.enabled = true;
    s.slack.bot_token_ref = Some("e2e-mcp/oauth/access-token".into());
    s.slack.app_token_ref = Some("notify/app".into());
    f.store.put_node_notify_settings(&s).unwrap();
    f.raw("n1", "u1", Kind::BudgetHit, None, DAY);
    let post = f.rows("")[0].0.clone();
    let p = prepare(&f.store, &f.vault, &due_of(&f, &post), DAY);
    // A permanent failure, not a retry: waiting cannot make the credential a channel token.
    assert!(matches!(p, Prepared::Fail("secret_not_permitted")), "{p:?}");
    assert!(!format!("{p:?}").contains("MCP-TOKEN-VALUE"));
}

#[test]
fn a_list_card_is_not_native_in_slack() {
    let list = json!({"root": "l", "nodes": {
        "l": {"type": "List", "props": {"items": {"$data": "/messages"}}, "children": []}
    }});
    assert!(!renders_natively(&list));
    // An approval is rendered from the approve jig, whatever card the skill ships.
    assert!(approve_ir("mail_send").is_some_and(|ir| renders_natively(&ir)));
}
