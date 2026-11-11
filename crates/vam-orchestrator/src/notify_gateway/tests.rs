use std::sync::atomic::AtomicBool;

use vam_spec::encoding::proto_to_json;
use vam_spec::v1::hosted_notification_gateway_client::HostedNotificationGatewayClient;
use vam_spec::v1::{CapabilityManifest, ExternalAccount, RunRequestPermission, ToolExport};

use super::*;
use crate::api::auth::ControlPlanePdp;
use crate::manager::SessionManager;
use crate::secrets::{SecretKind, SecretsVault};
use crate::workflow::SqliteWorkflowStore;

const APP: &str = "xapp-TOP-SECRET-app-value";
const BOT: &str = "xoxb-TOP-SECRET-bot-value";

fn app(store: Arc<SqliteWorkflowStore>) -> AppState {
    let (manager, _rx) = SessionManager::new(
        store.clone(),
        Arc::new(vam_core::CidAllocator::default()),
        Arc::new(|_vm_id, _cid| -> Box<dyn vam_core::Vmm> {
            unreachable!("the notification gateway tests boot no VM")
        }),
    );
    AppState {
        store: store.clone(),
        vault: SecretsVault::for_tests(store),
        manager,
        pdp: Arc::new(ControlPlanePdp::new().expect("pdp")),
        listen_addr: "127.0.0.1:0".to_string(),
        draining: Arc::new(AtomicBool::new(false)),
        ready: Arc::new(AtomicBool::new(true)),
        e2e_mock_scripts: false,
    }
}

fn channel(broker: &str, tier: TrustTier) -> CapabilityManifest {
    CapabilityManifest {
        broker_id: broker.into(),
        trust_tier: tier as i32,
        notification_channel: Some(NotificationChannel {
            channel: "slack".into(),
            surface: "slack".into(),
            max_actions_per_minute: 0,
        }),
        ..Default::default()
    }
}

fn trigger_broker(broker: &str) -> CapabilityManifest {
    CapabilityManifest {
        broker_id: broker.into(),
        trust_tier: TrustTier::FirstParty as i32,
        tools: vec![ToolExport {
            tool_name: "echo.ping".into(),
            side_effect: 0,
            ..Default::default()
        }],
        run_request_permission: Some(RunRequestPermission {
            grant_tools: vec!["echo.ping".into()],
            loop_guarded_tools: vec!["echo.ping".into()],
            surface: "echo".into(),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// A node with Slack enabled, the two secrets, a channel broker `vam.notify` and a trigger
/// broker `trig`.
fn node() -> AppState {
    let store = Arc::new(SqliteWorkflowStore::in_memory().unwrap());
    let state = app(store.clone());
    state
        .vault
        .put_secret("notify/app", APP, SecretKind::Egress, &[], None)
        .unwrap();
    state
        .vault
        .put_secret("notify/bot", BOT, SecretKind::Egress, &[], None)
        .unwrap();
    enable(&state, true);
    store
        .upsert_hsp_broker_approval(&channel("vam.notify", TrustTier::FirstParty))
        .unwrap();
    store
        .upsert_hsp_broker_approval(&trigger_broker("trig"))
        .unwrap();
    state
}

fn enable(state: &AppState, on: bool) {
    let mut s = crate::node_notify::NodeNotifySettings::default();
    s.slack.enabled = on;
    s.slack.bot_token_ref = Some("notify/bot".into());
    s.slack.app_token_ref = Some("notify/app".into());
    s.app_url = Some("https://vam.example.test".into());
    state.store.put_node_notify_settings(&s).unwrap();
}

fn cfg(channel: &str) -> GetChannelConfigRequest {
    GetChannelConfigRequest {
        channel: channel.into(),
        purpose: String::new(),
    }
}

fn cfg_for(channel: &str, purpose: &str) -> GetChannelConfigRequest {
    GetChannelConfigRequest {
        channel: channel.into(),
        purpose: purpose.into(),
    }
}

fn account(scope: &str, id: &str) -> Option<ExternalAccount> {
    Some(ExternalAccount {
        scope: scope.into(),
        id: id.into(),
    })
}

fn link_req(code: &str, scope: &str, id: &str) -> LinkChannelAccountRequest {
    LinkChannelAccountRequest {
        code: code.into(),
        external_account: account(scope, id),
        display: String::new(),
    }
}

fn denied<T: std::fmt::Debug>(r: Result<T, tonic::Status>) {
    match r {
        Err(s) => assert_eq!(s.code(), tonic::Code::PermissionDenied, "{s:?}"),
        Ok(v) => panic!("expected PERMISSION_DENIED, got {v:?}"),
    }
}

// ---- the proof every RPC starts with ------------------------------------------------------------

#[test]
fn the_proof_is_the_socket_the_tier_the_manifest_and_the_peer() {
    let st = node();
    let ok = prove_channel_broker(&st, "vam.notify", None).unwrap();
    assert_eq!(
        (ok.channel.as_str(), ok.surface.as_str()),
        ("slack", "slack")
    );
    // A trigger broker has no notification channel: refused on its own socket.
    assert!(matches!(
        prove_channel_broker(&st, "trig", None),
        Err(Refused::NotPermitted(_))
    ));
    // An unknown broker and a disabled one.
    assert!(matches!(
        prove_channel_broker(&st, "nobody", None),
        Err(Refused::NotPermitted(_))
    ));
    st.store.disable_hsp_broker_approval("vam.notify").unwrap();
    assert!(matches!(
        prove_channel_broker(&st, "vam.notify", None),
        Err(Refused::NotPermitted(_))
    ));
}

#[test]
fn a_tier_that_may_not_deliver_is_refused_even_with_the_block_stored() {
    let st = node();
    // Registration refuses this manifest; a row written some other way is refused here too.
    let m = channel("rogue", TrustTier::ThirdParty);
    let json = proto_to_json("vam.v1.CapabilityManifest", &m).unwrap();
    st.store
        .connection
        .lock()
        .unwrap()
        .execute(
            "INSERT INTO hsp_broker_approvals (broker_id, trust_tier, manifest_json, enabled, updated_at)
             VALUES ('rogue', ?1, ?2, 1, 0)",
            rusqlite::params![TrustTier::ThirdParty as i32, json],
        )
        .unwrap();
    assert!(matches!(
        prove_channel_broker(&st, "rogue", None),
        Err(Refused::NotPermitted(_))
    ));
    denied(channel_config(&st, "rogue", None, &cfg("slack")));
    assert!(!tier_may_deliver(TrustTier::ThirdParty as i32));
    assert!(!tier_may_deliver(TrustTier::Operator as i32));
    assert!(!tier_may_deliver(TrustTier::Unspecified as i32));
    assert!(tier_may_deliver(TrustTier::Core as i32));
    assert!(tier_may_deliver(TrustTier::FirstParty as i32));
    // The channel broker is never found by the delivery worker either.
    let c = st.store.connection.lock().unwrap();
    assert_eq!(
        crate::notify_delivery::find_channel_broker(&c, "slack")
            .unwrap()
            .as_deref(),
        Some("vam.notify")
    );
}

#[test]
fn the_peer_uid_is_checked_when_the_approval_records_one() {
    let st = node();
    st.store
        .set_hsp_broker_gateway("vam.notify", None, Some(4242))
        .unwrap();
    assert!(prove_channel_broker(&st, "vam.notify", Some(4242)).is_ok());
    for wrong in [Some(0), Some(1), None] {
        assert!(
            matches!(
                prove_channel_broker(&st, "vam.notify", wrong),
                Err(Refused::NotPermitted(_))
            ),
            "{wrong:?}"
        );
    }
    denied(channel_config(&st, "vam.notify", Some(1), &cfg("slack")));
    denied(link(&st, "vam.notify", Some(1), &link_req("X", "T1", "U1")));
}

// ---- GetChannelConfig --------------------------------------------------------------------------

#[test]
fn channel_config_is_refused_for_a_broker_without_the_channel_and_for_the_trigger_broker() {
    let st = node();
    denied(channel_config(&st, "trig", None, &cfg("slack")));
    denied(channel_config(&st, "nobody", None, &cfg("slack")));
    // A first-party broker with neither block.
    let plain = CapabilityManifest {
        broker_id: "plain".into(),
        trust_tier: TrustTier::FirstParty as i32,
        ..Default::default()
    };
    st.store.upsert_hsp_broker_approval(&plain).unwrap();
    denied(channel_config(&st, "plain", None, &cfg("slack")));
    // The channel broker may ask only for its own channel.
    denied(channel_config(&st, "vam.notify", None, &cfg("email")));
    denied(channel_config(&st, "vam.notify", None, &cfg("")));
}

#[test]
fn channel_config_serves_the_tokens_per_call_and_only_when_enabled() {
    let st = node();
    let r = channel_config(&st, "vam.notify", None, &cfg("slack")).unwrap();
    assert!(r.enabled);
    assert_eq!(r.resolved_secrets.len(), 2);
    assert_eq!(r.resolved_secrets[APP_TOKEN_KEY], APP);
    assert_eq!(r.resolved_secrets[BOT_TOKEN_KEY], BOT);
    // A reply asks for the bot token alone.
    let reply = channel_config(&st, "vam.notify", None, &cfg_for("slack", "reply")).unwrap();
    assert!(reply.enabled);
    assert_eq!(reply.resolved_secrets.len(), 1);
    assert_eq!(reply.resolved_secrets[BOT_TOKEN_KEY], BOT);
    assert_eq!(
        channel_config(&st, "vam.notify", None, &cfg_for("slack", "nonsense"))
            .unwrap_err()
            .code(),
        tonic::Code::InvalidArgument
    );
    assert_eq!(r.settings["app_url"], "https://vam.example.test");
    assert_eq!(r.settings["surface"], "slack");
    assert!(
        !r.settings.values().any(|v| v.contains("TOP-SECRET")),
        "settings hold nothing secret"
    );
    // Disabled by the operator: no settings and no secret.
    enable(&st, false);
    let off = channel_config(&st, "vam.notify", None, &cfg("slack")).unwrap();
    assert!(!off.enabled && off.resolved_secrets.is_empty() && off.settings.is_empty());
}

#[test]
fn a_secret_that_cannot_be_read_is_unavailable_and_says_nothing_of_it() {
    let st = node();
    let mut s = crate::node_notify::NodeNotifySettings::default();
    s.slack.enabled = true;
    s.slack.bot_token_ref = Some("notify/bot".into());
    s.slack.app_token_ref = Some("notify/missing".into());
    st.store.put_node_notify_settings(&s).unwrap();
    let e = channel_config(&st, "vam.notify", None, &cfg("slack")).unwrap_err();
    assert_eq!(e.code(), tonic::Code::Unavailable);
    assert!(!e.message().contains("missing") && !e.message().contains("TOP-SECRET"));
}

// ---- LinkChannelAccount ------------------------------------------------------------------------

fn start(st: &AppState, user: &str, address: Option<&str>) -> String {
    st.store
        .start_contact_link(user, "slack", address)
        .unwrap()
        .1
}

#[test]
fn linking_maps_every_outcome() {
    let st = node();
    // Linked.
    let code = start(&st, "u1", Some("T1/U1"));
    let r = link(&st, "vam.notify", None, &link_req(&code, "T1", "U1")).unwrap();
    assert_eq!(r.outcome, LinkOutcome::Linked as i32);
    assert!(r.contact_id.starts_with("ct_"));
    assert_eq!(
        st.store
            .verified_contact("u1", "slack")
            .unwrap()
            .unwrap()
            .id,
        r.contact_id
    );
    // The same code again: spent.
    let again = link(&st, "vam.notify", None, &link_req(&code, "T1", "U1")).unwrap();
    assert_eq!(again.outcome, LinkOutcome::InvalidCode as i32);
    // A wrong code.
    let wrong = link(
        &st,
        "vam.notify",
        None,
        &link_req("ZZZZZ-ZZZZZ", "T1", "U2"),
    )
    .unwrap();
    assert_eq!(wrong.outcome, LinkOutcome::InvalidCode as i32);
    // An account another user holds.
    let code2 = start(&st, "u2", None);
    let taken = link(&st, "vam.notify", None, &link_req(&code2, "T1", "U1")).unwrap();
    assert_eq!(taken.outcome, LinkOutcome::AlreadyLinked as i32);
    // A code for a named account presented by another one.
    let code3 = start(&st, "u2", Some("T1/U3"));
    let other = link(&st, "vam.notify", None, &link_req(&code3, "T1", "U4")).unwrap();
    assert_eq!(other.outcome, LinkOutcome::InvalidCode as i32);
    // Malformed accounts and codes.
    for (code, scope, id) in [
        ("", "T1", "U5"),
        (&code3[..], "", "U5"),
        (&code3[..], "T1", ""),
        (&code3[..], "T1", "a/b"),
    ] {
        let r = link(&st, "vam.notify", None, &link_req(code, scope, id)).unwrap();
        assert_eq!(
            r.outcome,
            LinkOutcome::InvalidCode as i32,
            "{code} {scope} {id}"
        );
    }
    let none = LinkChannelAccountRequest {
        code: code3.clone(),
        external_account: None,
        display: String::new(),
    };
    assert_eq!(
        link(&st, "vam.notify", None, &none).unwrap().outcome,
        LinkOutcome::InvalidCode as i32
    );
}

#[test]
fn another_workspace_is_refused_and_does_not_burn_the_code() {
    let st = node();
    let code = start(&st, "u1", None);
    assert_eq!(
        link(&st, "vam.notify", None, &link_req(&code, "T1", "U1"))
            .unwrap()
            .outcome,
        LinkOutcome::Linked as i32
    );
    let code2 = start(&st, "u2", None);
    let r = link(&st, "vam.notify", None, &link_req(&code2, "T9", "U9")).unwrap();
    assert_eq!(r.outcome, LinkOutcome::WrongWorkspace as i32);
    // The code still works from the right workspace.
    let r = link(&st, "vam.notify", None, &link_req(&code2, "T1", "U2")).unwrap();
    assert_eq!(r.outcome, LinkOutcome::Linked as i32);
}

#[test]
fn too_many_wrong_codes_from_one_account_are_refused() {
    let st = node();
    let good = start(&st, "u1", None);
    for _ in 0..crate::contacts::MAX_ATTEMPTS {
        let r = link(
            &st,
            "vam.notify",
            None,
            &link_req("AAAAA-AAAAA", "T1", "U1"),
        )
        .unwrap();
        assert_eq!(r.outcome, LinkOutcome::InvalidCode as i32);
    }
    let r = link(&st, "vam.notify", None, &link_req(&good, "T1", "U1")).unwrap();
    assert_eq!(r.outcome, LinkOutcome::TooManyAttempts as i32);
}

#[test]
fn linking_needs_the_channel_enabled_and_the_right_broker() {
    let st = node();
    let code = start(&st, "u1", None);
    denied(link(&st, "trig", None, &link_req(&code, "T1", "U1")));
    denied(link(&st, "nobody", None, &link_req(&code, "T1", "U1")));
    enable(&st, false);
    let r = link(&st, "vam.notify", None, &link_req(&code, "T1", "U1")).unwrap();
    assert_eq!(r.outcome, LinkOutcome::Unavailable as i32);
    assert!(st.store.verified_contact("u1", "slack").unwrap().is_none());
}

// ---- ActOnNotification (stub) ------------------------------------------------------------------

// ---- the real socket ---------------------------------------------------------------------------

async fn client(
    path: &std::path::Path,
) -> HostedNotificationGatewayClient<tonic::transport::Channel> {
    let path = path.to_path_buf();
    let channel = tonic::transport::Endpoint::try_from("http://127.0.0.1")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_uri: http::Uri| {
            let path = path.clone();
            async move { tokio::net::UnixStream::connect(path).await }
        }))
        .await
        .unwrap();
    HostedNotificationGatewayClient::new(channel)
}

#[tokio::test]
async fn the_socket_serves_a_channel_broker_and_refuses_the_trigger_broker_on_its_own_socket() {
    let st = node();
    let dir = tempfile::tempdir().unwrap();
    st.store.gateways.set_dir(dir.path().join("gw"));
    let notify_path = crate::gateway::sync_broker(&st, "vam.notify")
        .await
        .unwrap()
        .expect("a channel broker gets a gateway socket");
    let trig_path = crate::gateway::sync_broker(&st, "trig")
        .await
        .unwrap()
        .expect("a trigger broker keeps its gateway socket");
    assert_ne!(notify_path, trig_path);
    assert_eq!(
        st.store.get_hsp_broker_gateway("vam.notify").unwrap().0,
        Some(notify_path.to_string_lossy().into_owned())
    );
    // The channel broker's own socket serves the three RPCs.
    let mut c = client(&notify_path).await;
    let cfg_answer = c
        .get_channel_config(cfg("slack"))
        .await
        .unwrap()
        .into_inner();
    assert!(cfg_answer.enabled);
    assert_eq!(cfg_answer.resolved_secrets[APP_TOKEN_KEY], APP);
    let act = c
        .act_on_notification(ActOnNotificationRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(act.outcome, ActOnOutcome::Invalid as i32, "no handle");
    // The trigger broker's socket answers the same service with NOT_PERMITTED.
    let mut t = client(&trig_path).await;
    let e = t.get_channel_config(cfg("slack")).await.unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
    let e = t
        .link_channel_account(link_req("X", "T1", "U1"))
        .await
        .unwrap_err();
    assert_eq!(e.code(), tonic::Code::PermissionDenied);
    let act = t
        .act_on_notification(ActOnNotificationRequest::default())
        .await
        .unwrap()
        .into_inner();
    assert_eq!(act.outcome, ActOnOutcome::NotPermitted as i32);
    // A broker that loses its approval loses its socket.
    st.store.disable_hsp_broker_approval("vam.notify").unwrap();
    assert!(crate::gateway::sync_broker(&st, "vam.notify")
        .await
        .unwrap()
        .is_none());
    assert!(st
        .store
        .get_hsp_broker_gateway("vam.notify")
        .unwrap()
        .0
        .is_none());
}

#[test]
fn only_a_broker_with_a_permission_and_a_tier_that_fits_gets_a_socket() {
    let m = |tier: TrustTier, channel_block: bool, run: bool| CapabilityManifest {
        broker_id: "b".into(),
        trust_tier: tier as i32,
        notification_channel: channel_block.then(|| NotificationChannel {
            channel: "slack".into(),
            surface: "slack".into(),
            max_actions_per_minute: 0,
        }),
        run_request_permission: run.then(RunRequestPermission::default),
        ..Default::default()
    };
    assert!(crate::gateway::wants_gateway(&m(
        TrustTier::Core,
        true,
        false
    )));
    assert!(crate::gateway::wants_gateway(&m(
        TrustTier::FirstParty,
        true,
        false
    )));
    assert!(!crate::gateway::wants_gateway(&m(
        TrustTier::ThirdParty,
        true,
        false
    )));
    assert!(!crate::gateway::wants_gateway(&m(
        TrustTier::Operator,
        true,
        false
    )));
    assert!(crate::gateway::wants_gateway(&m(
        TrustTier::ThirdParty,
        false,
        true
    )));
    assert!(!crate::gateway::wants_gateway(&m(
        TrustTier::FirstParty,
        false,
        false
    )));
}

// ---- deciding from a channel (NT-2.4) ----------------------------------------------------------

use crate::hsp::HspEndpoint;
use crate::notifications::{Kind, NewNotification, RecordOutcome, Severity};
use crate::notify_delivery::Prepared;
use vam_spec::v1::{DeliverNotificationResponse, DeliverOutcome};

/// A node with Slack on, two users with verified Slack accounts, and the channel broker's endpoint.
struct World {
    st: AppState,
    alice: String,
    bob: String,
    alice_contact: String,
    bob_contact: String,
}

fn linked(st: &AppState, user: &str, account: &str) -> String {
    let (contact, code) = st
        .store
        .start_contact_link(user, "slack", Some(account))
        .unwrap();
    assert!(st
        .store
        .verify_contact_with_code(&code, account)
        .unwrap()
        .is_verified());
    contact.id
}

fn world() -> World {
    let st = node();
    st.store
        .set_hsp_broker_endpoint(
            "vam.notify",
            &HspEndpoint::Unix(std::path::PathBuf::from("/nonexistent/vam-notify.sock")),
        )
        .unwrap();
    let alice = st.store.create_user("alice", "alice").unwrap().user_id;
    let bob = st.store.create_user("bob", "bob").unwrap().user_id;
    let alice_contact = linked(&st, &alice, "T1/U1");
    let bob_contact = linked(&st, &bob, "T1/U2");
    World {
        st,
        alice,
        bob,
        alice_contact,
        bob_contact,
    }
}

struct Delivered {
    notification: String,
    approval: String,
    post: String,
    handle: String,
}

/// An approval for `user`, its record, and the delivery of it as the worker does it: the row is
/// due, `prepare` builds the request (and mints the handle), the broker said sent.
fn deliver(st: &AppState, user: &str, tag: &str) -> Delivered {
    let run = format!("urn:vam:run:local.vam.dev/acme/{tag}");
    st.store
        .create_run(&vam_spec::v1::ResourceId { urn: run.clone() })
        .unwrap();
    let approval = format!("ap-{tag}");
    st.store
        .record_published_approval_of_kind(
            &approval,
            &run,
            user,
            r#"{"path":"a.txt"}"#,
            "tool_call",
            "write_file",
        )
        .unwrap();
    let (title, body) =
        crate::notifications::render(Kind::ApprovalPending, "researcher", "write_file", None);
    let note = NewNotification {
        user_id: user.into(),
        kind: Kind::ApprovalPending,
        severity: Severity::Action,
        subject_key: tag.into(),
        run_id: Some(run),
        space_id: None,
        thread_id: None,
        approval_id: Some(approval.clone()),
        grant_id: None,
        receipt_digest: None,
        agent_urn: String::new(),
        agent_label: "researcher".into(),
        via: None,
        title,
        body,
        action: serde_json::json!({"verb": "decide"}),
    };
    let notification = {
        let c = st.store.connection.lock().unwrap();
        match crate::notifications::record(&c, &note).unwrap() {
            RecordOutcome::Inserted(id) | RecordOutcome::Updated(id) => id,
        }
    };
    let now = crate::catalog::unix_timestamp();
    let due = crate::notify_delivery::due_rows(&st.store, now, 50)
        .unwrap()
        .into_iter()
        .find(|d| d.notification_id == notification && d.kind == "post")
        .expect("a post row");
    let Prepared::Send { request, .. } =
        crate::notify_delivery::prepare(&st.store, &st.vault, &due, now)
    else {
        panic!("the post should be sendable");
    };
    let handle = request.action_handle.clone();
    crate::notify_delivery::finish(
        &st.store,
        &due,
        Ok(DeliverNotificationResponse {
            outcome: DeliverOutcome::Sent as i32,
            ..Default::default()
        }),
        now,
    )
    .unwrap();
    Delivered {
        notification,
        approval,
        post: due.id,
        handle,
    }
}

fn click(handle: &str, scope: &str, id: &str, decision: &str) -> ActOnNotificationRequest {
    ActOnNotificationRequest {
        handle: handle.into(),
        external_account: account(scope, id),
        decision: decision.into(),
        response: None,
    }
}

fn decision_of(st: &AppState, approval: &str) -> Option<String> {
    st.store.get_approval(approval).unwrap().decision
}

fn secret_part(handle: &str) -> &str {
    handle.split_once('.').unwrap().1
}

#[tokio::test]
async fn a_handle_is_bound_to_its_record_and_stored_as_a_hash_only() {
    let w = world();
    let d = deliver(&w.st, &w.alice, "bound");
    assert!(d.handle.starts_with("vam_na_"), "{}", d.handle);
    let found = {
        let c = w.st.store.connection.lock().unwrap();
        crate::credentials::find_action_handle(&c, &d.handle, "vam.notify").unwrap()
    };
    assert_eq!(found.broker_id, "vam.notify");
    assert_eq!(found.notification_id, d.notification);
    assert_eq!(found.approval_id, d.approval);
    assert_eq!(found.user_id, w.alice);
    assert_eq!(found.contact_id, w.alice_contact);
    assert_eq!(found.channel, "slack");
    assert_eq!(found.surface, "slack", "from the approved manifest");
    assert_eq!(found.decisions, vec!["APPROVED", "REJECTED"]);
    assert!(found.revoked_at.is_none());
    assert!(found.expires_at.unwrap() > crate::catalog::unix_timestamp());
    // The handle is on another broker's socket: not found there.
    {
        let c = w.st.store.connection.lock().unwrap();
        assert!(matches!(
            crate::credentials::find_action_handle(&c, &d.handle, "trig"),
            Err(crate::credentials::HandleError::Invalid)
        ));
        // Every column of every api_keys row, as text: the secret is nowhere.
        let mut stmt = c
            .prepare(
                "SELECT key_id || '|' || name || '|' || sha256_secret || '|' || principal || '|' ||
                        scopes || '|' || IFNULL(holder,'') || '|' || IFNULL(target_id,'') || '|' ||
                        IFNULL(constraints_json,'') || '|' || IFNULL(external_ref,'')
                 FROM api_keys",
            )
            .unwrap();
        let rows: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!rows.is_empty());
        for row in rows {
            assert!(!row.contains(secret_part(&d.handle)), "{row}");
            assert!(!row.contains(&d.handle));
        }
        let (hash, holder, via): (String, String, String) = c
            .query_row(
                "SELECT sha256_secret, holder, issued_via FROM api_keys WHERE key_id = ?1",
                [&found.key_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            hash,
            crate::keys::hash_api_key_secret(secret_part(&d.handle))
        );
        assert_eq!(holder, "broker:vam.notify");
        assert_eq!(via, "notification");
    }
    // The delivery tables hold no handle either.
    let c = w.st.store.connection.lock().unwrap();
    let in_deliveries: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM notification_deliveries WHERE
               id LIKE '%vam_na_%' OR IFNULL(last_error,'') LIKE '%vam_na_%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(in_deliveries, 0);
}

#[tokio::test]
async fn every_refusal_leaves_the_approval_pending_and_the_handle_usable() {
    let w = world();
    let mine = deliver(&w.st, &w.alice, "refuse-a");
    let bobs = deliver(&w.st, &w.bob, "refuse-b");
    let go = |req: ActOnNotificationRequest| {
        let st = &w.st;
        async move { act_on(st, "vam.notify", None, &req).await.outcome }
    };
    let (invalid, not_linked, not_permitted) = (
        ActOnOutcome::Invalid as i32,
        ActOnOutcome::NotLinked as i32,
        ActOnOutcome::NotPermitted as i32,
    );
    // A forged handle: well formed, never issued.
    assert_eq!(
        go(click(
            "vam_na_na_forged00000.forgedforgedforgedforgedforgedforged",
            "T1",
            "U1",
            "APPROVED"
        ))
        .await,
        invalid
    );
    // Not a handle at all.
    for junk in ["", "nope", "vam_na_", "vam_na_na_x.", "vam_rg_rg_x.y"] {
        assert_eq!(
            go(click(junk, "T1", "U1", "APPROVED")).await,
            invalid,
            "{junk}"
        );
    }
    // The right secret under another id.
    let swapped = format!("vam_na_na_otherotherother.{}", secret_part(&mine.handle));
    assert_eq!(go(click(&swapped, "T1", "U1", "APPROVED")).await, invalid);
    // Bob's real handle, presented from Alice's linked account: not hers.
    assert_eq!(
        go(click(&bobs.handle, "T1", "U1", "APPROVED")).await,
        invalid
    );
    // Alice's handle from an account nobody linked.
    assert_eq!(
        go(click(&mine.handle, "T9", "U9", "APPROVED")).await,
        not_linked
    );
    // ... from Bob's linked account: another user's account presenting her handle.
    assert_eq!(
        go(click(&mine.handle, "T1", "U2", "APPROVED")).await,
        invalid
    );
    // A request with no account, and a malformed one.
    let mut no_account = click(&mine.handle, "T1", "U1", "APPROVED");
    no_account.external_account = None;
    assert_eq!(go(no_account).await, not_linked);
    assert_eq!(
        go(click(&mine.handle, "T1/x", "U1", "APPROVED")).await,
        not_linked
    );
    // The trigger broker's socket: refused before the handle is looked at.
    assert_eq!(
        act_on(
            &w.st,
            "trig",
            None,
            &click(&mine.handle, "T1", "U1", "APPROVED")
        )
        .await
        .outcome,
        not_permitted
    );
    // Decisions the message never offered, and the words a channel can never use.
    for bad in [
        "SUBMITTED",
        "AMENDED",
        "amended",
        "approved",
        "",
        "ALWAYS_ALLOW",
        "always allow",
        "REMEMBER_ALWAYS_ALLOW",
        "remember",
        "go",
    ] {
        assert_eq!(
            go(click(&mine.handle, "T1", "U1", bad)).await,
            invalid,
            "{bad:?}"
        );
    }
    // None of it decided anything or spent the handle.
    assert_eq!(decision_of(&w.st, &mine.approval), None);
    assert_eq!(decision_of(&w.st, &bobs.approval), None);
    let c = w.st.store.connection.lock().unwrap();
    let live: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM api_keys WHERE issued_via = 'notification' AND revoked_at IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(live, 2);
    let rules: i64 = c
        .query_row("SELECT COUNT(*) FROM agent_rule_revisions", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    assert_eq!(rules, 0, "a channel decision writes no permission rule");
}

#[tokio::test]
async fn a_good_click_decides_as_the_handles_user_from_the_handles_surface_once() {
    let w = world();
    let d = deliver(&w.st, &w.alice, "good");
    let other = deliver(&w.st, &w.bob, "good-bob");
    let r = act_on(
        &w.st,
        "vam.notify",
        None,
        &click(&d.handle, "T1", "U1", "APPROVED"),
    )
    .await;
    assert_eq!(r.outcome, ActOnOutcome::Accepted as i32, "{}", r.message);
    let row = w.st.store.get_approval(&d.approval).unwrap();
    assert_eq!(row.decision.as_deref(), Some("APPROVED"));
    let (surface, by): (String, String) = {
        let c = w.st.store.connection.lock().unwrap();
        c.query_row(
            "SELECT decided_surface, decided_by_user_id FROM approvals WHERE request_id = ?1",
            [&d.approval],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!((surface.as_str(), by.as_str()), ("slack", w.alice.as_str()));
    // The record closed as acted from Slack by her.
    let (reason, acted_surface, acted_by): (String, String, String) = {
        let c = w.st.store.connection.lock().unwrap();
        c.query_row(
            "SELECT resolved_reason, acted_surface, acted_by_user_id FROM notifications WHERE id = ?1",
            [&d.notification],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    };
    assert_eq!(
        (reason.as_str(), acted_surface.as_str(), acted_by.as_str()),
        ("acted", "slack", w.alice.as_str())
    );
    // The handle was used up with the decision; a replay finds it already decided, and says by whom.
    let again = act_on(
        &w.st,
        "vam.notify",
        None,
        &click(&d.handle, "T1", "U1", "APPROVED"),
    )
    .await;
    assert_eq!(again.outcome, ActOnOutcome::AlreadyResolved as i32);
    assert_eq!(again.decided_by, "alice");
    assert_eq!(again.decided_surface, "slack");
    assert!(again.decided_at.is_some());
    // Bob's approval was not touched.
    assert_eq!(decision_of(&w.st, &other.approval), None);
    // A message update was queued for the sent post (the broker edits it).
    let updates: i64 = {
        let c = w.st.store.connection.lock().unwrap();
        c.query_row(
            "SELECT COUNT(*) FROM notification_deliveries
             WHERE update_of = ?1 AND kind = 'update' AND refresh = 0",
            [&d.post],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert_eq!(updates, 1);
}

#[tokio::test]
async fn a_record_decided_elsewhere_answers_already_resolved_with_who_and_where() {
    let w = world();
    let d = deliver(&w.st, &w.alice, "elsewhere");
    // Decided in the app first.
    w.st.store
        .decide_approval_with_surface(
            &vam_spec::v1::ResourceId {
                urn: d.approval.clone(),
            },
            &vam_spec::v1::ResourceId {
                urn: "urn:vam:token:local.vam.dev/acme/t".into(),
            },
            "REJECTED",
            "User::\"alice\"",
            &w.alice,
            Some("no"),
            None,
            None,
            None,
            "app",
        )
        .unwrap();
    let r = act_on(
        &w.st,
        "vam.notify",
        None,
        &click(&d.handle, "T1", "U1", "APPROVED"),
    )
    .await;
    assert_eq!(r.outcome, ActOnOutcome::AlreadyResolved as i32);
    assert_eq!(r.decided_surface, "app");
    assert_eq!(r.decided_by, "alice");
    // Still rejected: the click changed nothing.
    assert_eq!(decision_of(&w.st, &d.approval).as_deref(), Some("REJECTED"));
}

#[tokio::test]
async fn an_expired_a_revoked_and_an_unlinked_handle_decide_nothing() {
    let w = world();
    // Expired with the approval.
    let a = deliver(&w.st, &w.alice, "expired");
    {
        let c = w.st.store.connection.lock().unwrap();
        c.execute(
            "UPDATE api_keys SET expires_at = ?1 WHERE issued_via = 'notification'",
            [crate::catalog::unix_timestamp() - 1],
        )
        .unwrap();
    }
    assert_eq!(
        act_on(
            &w.st,
            "vam.notify",
            None,
            &click(&a.handle, "T1", "U1", "APPROVED")
        )
        .await
        .outcome,
        ActOnOutcome::Expired as i32
    );
    // Unlinking revokes the contact's open handles: the handle is no longer valid, though the
    // record is still waiting.
    let b = deliver(&w.st, &w.bob, "unlinked");
    w.st.store.unlink_contact(&w.bob, &w.bob_contact).unwrap();
    let r = act_on(
        &w.st,
        "vam.notify",
        None,
        &click(&b.handle, "T1", "U2", "APPROVED"),
    )
    .await;
    assert_eq!(r.outcome, ActOnOutcome::Invalid as i32);
    assert_eq!(decision_of(&w.st, &b.approval), None);
    // A deactivated user: the account check passes but the user is not active.
    let c = deliver(&w.st, &w.alice, "deactivated");
    {
        let conn = w.st.store.connection.lock().unwrap();
        conn.execute(
            "UPDATE users SET deactivated_at = 1 WHERE user_id = ?1",
            [&w.alice],
        )
        .unwrap();
    }
    assert_eq!(
        act_on(
            &w.st,
            "vam.notify",
            None,
            &click(&c.handle, "T1", "U1", "APPROVED")
        )
        .await
        .outcome,
        ActOnOutcome::NotLinked as i32
    );
    assert_eq!(decision_of(&w.st, &c.approval), None);
}

#[test]
fn the_rate_limit_is_per_broker_per_minute_and_zero_means_sixty() {
    let t0 = Instant::now();
    for _ in 0..2 {
        assert!(rate_take("test.rate.two", 2, t0).is_ok());
    }
    let wait = rate_take("test.rate.two", 2, t0 + Duration::from_secs(10)).unwrap_err();
    assert!(wait > 0 && wait <= 50_000, "{wait}");
    // Another broker has its own book.
    assert!(rate_take("test.rate.other", 2, t0).is_ok());
    // After the minute the window is free again.
    assert!(rate_take("test.rate.two", 2, t0 + Duration::from_secs(61)).is_ok());
    for _ in 0..60 {
        assert!(rate_take("test.rate.zero", 0, t0).is_ok());
    }
    assert!(rate_take("test.rate.zero", 0, t0).is_err());
}

#[tokio::test]
async fn a_handle_never_authenticates_http() {
    let w = world();
    let d = deliver(&w.st, &w.alice, "http");
    let secret = secret_part(&d.handle).to_string();
    for presented in [d.handle.as_str(), secret.as_str()] {
        let err =
            w.st.store
                .authenticate_bearer(presented)
                .expect_err("refused");
        assert!(matches!(err, OrchestratorError::Unauthenticated), "{err:?}");
        assert!(matches!(
            w.st.store
                .check_credential(presented, &crate::credentials::Transport::Http),
            Err(crate::credentials::CredentialError::WrongTransport)
                | Err(crate::credentials::CredentialError::Unknown)
        ));
    }
    // Through the router: every route that needs a login answers 401 to it.
    let router = crate::api::router(w.st.clone());
    for presented in [d.handle.as_str(), secret.as_str()] {
        for (method, uri) in [
            ("GET", "/v1/me".to_string()),
            ("GET", "/v1/notifications".to_string()),
            ("POST", format!("/v1/approvals/{}/decide", d.approval)),
        ] {
            let response = tower::ServiceExt::oneshot(
                router.clone(),
                http::Request::builder()
                    .method(method)
                    .uri(&uri)
                    .header("authorization", format!("Bearer {presented}"))
                    .header("Idempotency-Key", "k-http")
                    .header("content-type", "application/json")
                    .body(axum::body::Body::from(r#"{"decision":1}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
            assert_eq!(response.status(), http::StatusCode::UNAUTHORIZED, "{uri}");
        }
    }
    assert_eq!(decision_of(&w.st, &d.approval), None);
}

#[tokio::test]
async fn the_route_and_the_gateway_write_the_same_approval_row_except_the_surface() {
    let w = world();
    // The same user, two approvals; one decided by the HTTP route, one from Slack.
    let via_http = deliver(&w.st, &w.alice, "same-http");
    let via_slack = deliver(&w.st, &w.alice, "same-slack");
    let key = crate::keys::mint_api_key_secret();
    let scopes: Vec<String> = crate::keys::MEMBER_SCOPES
        .iter()
        .map(|s| s.to_string())
        .collect();
    w.st.store
        .create_api_key_owned(
            "key-alice",
            "alice",
            &key,
            "User::\"alice\"",
            &scopes,
            &w.alice,
        )
        .unwrap();
    let response = tower::ServiceExt::oneshot(
        crate::api::router(w.st.clone()),
        http::Request::builder()
            .method("POST")
            .uri(format!("/v1/approvals/{}/decide", via_http.approval))
            .header("authorization", format!("Bearer {key}"))
            .header("Idempotency-Key", "k-same")
            .header("content-type", "application/json")
            .body(axum::body::Body::from(r#"{"decision":1}"#))
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), http::StatusCode::OK);
    let r = act_on(
        &w.st,
        "vam.notify",
        None,
        &click(&via_slack.handle, "T1", "U1", "APPROVED"),
    )
    .await;
    assert_eq!(r.outcome, ActOnOutcome::Accepted as i32, "{}", r.message);

    type Values = Vec<rusqlite::types::Value>;
    let row = |approval: &str| -> (Values, String, Vec<String>, (String, String)) {
        let c = w.st.store.connection.lock().unwrap();
        let values: Values = c
            .query_row(
                "SELECT decision, decision_revision, rejection_reason, amended_args_json,
                        amended_args_digest, decided_by_user_id, owner_user_id, approver_user_id,
                        kind, tool_name, continuation_generation, published
                 FROM approvals WHERE request_id = ?1",
                [approval],
                |r| (0..12).map(|i| r.get(i)).collect(),
            )
            .unwrap();
        let (surface, run): (String, String) = c
            .query_row(
                "SELECT decided_surface, run_id FROM approvals WHERE request_id = ?1",
                [approval],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // Only the decision's own event: a resume racing in the background adds others.
        let events: Vec<String> = c
            .prepare(
                "SELECT event_type FROM workflow_outbox
                 WHERE run_id = ?1 AND event_type LIKE '%hitl.decided' ORDER BY sequence",
            )
            .unwrap()
            .query_map([&run], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        let note: (String, String) = c
            .query_row(
                "SELECT resolved_reason, acted_by_user_id FROM notifications WHERE approval_id = ?1",
                [approval],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        (values, surface, events, note)
    };
    let (http_row, http_surface, http_events, http_note) = row(&via_http.approval);
    let (slack_row, slack_surface, slack_events, slack_note) = row(&via_slack.approval);
    assert_eq!(http_surface, "api");
    assert_eq!(slack_surface, "slack");
    assert_eq!(http_row, slack_row, "identical except the surface");
    assert_eq!(http_events.len(), 1);
    assert_eq!(http_events, slack_events);
    assert_eq!(http_note, slack_note);
}

#[tokio::test]
async fn a_broker_start_revokes_its_handles_and_queues_a_fresh_one_per_waiting_decision() {
    let w = world();
    let waiting = deliver(&w.st, &w.alice, "restart-wait");
    let decided = deliver(&w.st, &w.bob, "restart-done");
    // Bob's is decided in the app before the restart.
    w.st.store
        .decide_approval_with_surface(
            &vam_spec::v1::ResourceId {
                urn: decided.approval.clone(),
            },
            &vam_spec::v1::ResourceId {
                urn: "urn:vam:token:local.vam.dev/acme/t2".into(),
            },
            "APPROVED",
            "User::\"bob\"",
            &w.bob,
            None,
            None,
            None,
            None,
            "app",
        )
        .unwrap();
    // A reply to a person asks for the bot token only: no handle is touched, nothing is queued.
    channel_config(&w.st, "vam.notify", None, &cfg_for("slack", "reply")).unwrap();
    assert_eq!(refreshes(&w.st), 0);
    // The start: handles gone, one refresh for the waiting decision only.
    let r = channel_config(&w.st, "vam.notify", None, &cfg("slack")).unwrap();
    assert!(r.enabled);
    assert_eq!(refreshes(&w.st), 1);
    let stale = act_on(
        &w.st,
        "vam.notify",
        None,
        &click(&waiting.handle, "T1", "U1", "APPROVED"),
    )
    .await;
    assert_eq!(stale.outcome, ActOnOutcome::Invalid as i32, "try again");
    assert_eq!(decision_of(&w.st, &waiting.approval), None);
    // A second start before the worker ran queues no second refresh.
    channel_config(&w.st, "vam.notify", None, &cfg("slack")).unwrap();
    assert_eq!(refreshes(&w.st), 1);
    // The worker's side: the refresh is the full message again, an edit of the sent post, with
    // a new handle.
    let now = crate::catalog::unix_timestamp();
    let due = crate::notify_delivery::due_rows(&w.st.store, now, 50)
        .unwrap()
        .into_iter()
        .find(|d| d.kind == "update" && d.notification_id == waiting.notification)
        .expect("the refresh is due");
    assert_eq!(due.update_of.as_deref(), Some(waiting.post.as_str()));
    let Prepared::Send { request, .. } =
        crate::notify_delivery::prepare(&w.st.store, &w.st.vault, &due, now)
    else {
        panic!("sendable");
    };
    assert_eq!(request.update_of, waiting.post);
    assert_eq!(request.rendering.as_ref().unwrap().actions.len(), 2);
    assert!(request.action_handle.starts_with("vam_na_"));
    assert_ne!(request.action_handle, waiting.handle);
    // The new handle works; the old one still does not.
    let fresh = request.action_handle.clone();
    assert_eq!(
        act_on(
            &w.st,
            "vam.notify",
            None,
            &click(&waiting.handle, "T1", "U1", "APPROVED")
        )
        .await
        .outcome,
        ActOnOutcome::Invalid as i32
    );
    let ok = act_on(
        &w.st,
        "vam.notify",
        None,
        &click(&fresh, "T1", "U1", "APPROVED"),
    )
    .await;
    assert_eq!(ok.outcome, ActOnOutcome::Accepted as i32, "{}", ok.message);
    assert_eq!(
        w.st.store
            .get_approval(&waiting.approval)
            .unwrap()
            .decision
            .as_deref(),
        Some("APPROVED")
    );
    // Resolved: a refresh that had not gone out is suppressed, and the decided edit is queued.
    let c = w.st.store.connection.lock().unwrap();
    let pending_refresh: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM notification_deliveries
             WHERE refresh = 1 AND state IN ('pending','held')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(pending_refresh, 0);
    let decided_edits: i64 = c
        .query_row(
            "SELECT COUNT(*) FROM notification_deliveries
             WHERE refresh = 0 AND kind = 'update' AND notification_id = ?1",
            [&waiting.notification],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(decided_edits, 1);
}

fn refreshes(st: &AppState) -> i64 {
    let c = st.store.connection.lock().unwrap();
    c.query_row(
        "SELECT COUNT(*) FROM notification_deliveries WHERE refresh = 1 AND state = 'pending'",
        [],
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn the_offered_set_of_an_option_gate_is_its_option_ids() {
    use vam_spec::v1::{NotificationRendering, RenderedAction};
    let action = |id: &str, decision: &str, option: &str| RenderedAction {
        id: id.into(),
        label: id.into(),
        decision: decision.into(),
        option: option.into(),
        style: 0,
    };
    let r = NotificationRendering {
        actions: vec![
            action("opt_0", "SUBMITTED", "go"),
            action("opt_1", "SUBMITTED", "stop"),
            action("opt_2", "SUBMITTED", "go"),
        ],
        ..Default::default()
    };
    assert_eq!(
        crate::notify_delivery::offered_decisions(&r),
        vec!["go".to_string(), "stop".to_string()]
    );
    let tool = NotificationRendering {
        actions: vec![
            action("approve", "APPROVED", ""),
            action("deny", "REJECTED", ""),
        ],
        ..Default::default()
    };
    assert_eq!(
        crate::notify_delivery::offered_decisions(&tool),
        vec!["APPROVED".to_string(), "REJECTED".to_string()]
    );
}

#[test]
fn an_mcp_credential_named_as_a_channel_token_is_refused_and_never_returned() {
    for named_as in ["app", "bot"] {
        let st = node();
        st.vault
            .put_secret(
                "e2e-mcp/oauth/access-token",
                "MCP-TOKEN-VALUE",
                SecretKind::Connection,
                &["mcp.example.com".to_string()],
                None,
            )
            .unwrap();
        let mut s = crate::node_notify::NodeNotifySettings::default();
        s.slack.enabled = true;
        s.slack.bot_token_ref = Some("notify/bot".into());
        s.slack.app_token_ref = Some("notify/app".into());
        match named_as {
            "app" => s.slack.app_token_ref = Some("e2e-mcp/oauth/access-token".into()),
            _ => s.slack.bot_token_ref = Some("e2e-mcp/oauth/access-token".into()),
        }
        st.store.put_node_notify_settings(&s).unwrap();
        let e = channel_config(&st, "vam.notify", None, &cfg("slack")).unwrap_err();
        assert_eq!(e.code(), tonic::Code::PermissionDenied, "{named_as}");
        assert!(!e.message().contains("MCP-TOKEN-VALUE") && !e.message().contains("e2e-mcp"));
    }
}
