//! Both services answer on one Unix socket.

use std::sync::Arc;

use tonic::transport::{Channel, Endpoint, Uri};
use vam_notify::adapter::Registry;
use vam_notify::gateway::GatewayClient;
use vam_notify::hub::Hub;
use vam_notify::log::DeliveryLog;
use vam_notify::serve::{bind_unix_listener, serve_unix};
use vam_notify::slack::SlackAdapter;
use vam_notify::transport::LiveTransport;
use vam_notify::NotifyBroker;
use vam_spec::v1::hosted_broker_client::HostedBrokerClient;
use vam_spec::v1::hosted_notification_channel_client::HostedNotificationChannelClient;
use vam_spec::v1::{DeliverNotificationRequest, DeliverOutcome, ListToolsRequest, RegisterRequest};

async fn dial(path: std::path::PathBuf) -> Channel {
    Endpoint::try_from("http://[::]:50051")
        .unwrap()
        .connect_with_connector(tower::service_fn(move |_: Uri| {
            let p = path.clone();
            async move { tokio::net::UnixStream::connect(p).await }
        }))
        .await
        .unwrap()
}

#[tokio::test]
async fn register_lists_no_tools_and_deliver_is_routed() {
    let tmp = tempfile::tempdir().unwrap();
    let sock = tmp.path().join("n.sock");
    let listener = bind_unix_listener(&sock).unwrap();
    let registry = Registry::default().with(Arc::new(SlackAdapter::new(Arc::new(
        LiveTransport::for_test("http://127.0.0.1:1/api"),
    ))));
    let log = DeliveryLog::open(&tmp.path().join("d.db")).unwrap();
    let hub = Hub::new(registry, log, GatewayClient::new(None));
    tokio::spawn(serve_unix(listener, NotifyBroker::new(hub)));

    let channel = dial(sock).await;
    let mut hsp = HostedBrokerClient::new(channel.clone());
    hsp.register(RegisterRequest::default()).await.unwrap();
    assert!(hsp
        .list_tools(ListToolsRequest::default())
        .await
        .unwrap()
        .into_inner()
        .tools
        .is_empty());

    let mut notify = HostedNotificationChannelClient::new(channel);
    // The live transport points at a closed local port (never Slack): the call is told to retry.
    let slack = notify
        .deliver_notification(DeliverNotificationRequest {
            delivery_id: "d1".into(),
            channel: "slack".into(),
            channel_address: "T1/U1".into(),
            resolved_secrets: [("bot_token".to_string(), "xoxb-x".to_string())].into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(slack.outcome, DeliverOutcome::Retry as i32);
    assert_eq!(slack.error_code, "unavailable");
    let other = notify
        .deliver_notification(DeliverNotificationRequest {
            delivery_id: "d2".into(),
            channel: "email".into(),
            ..Default::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(other.outcome, DeliverOutcome::Unsupported as i32);
}
