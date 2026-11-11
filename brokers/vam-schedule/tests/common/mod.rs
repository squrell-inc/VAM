//! Shared by the integration tests: a scripted fake `HostedTriggerGateway` on a UDS.
#![allow(dead_code, clippy::result_large_err)]

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_stream::wrappers::UnixListenerStream;
use tonic::transport::Server;
use vam_schedule::gateway::{FireKind, FireRequest};
use vam_schedule::grant::GrantToken;
use vam_schedule::serve::bind_unix_listener;
use vam_spec::v1::hosted_trigger_gateway_server::{
    HostedTriggerGateway, HostedTriggerGatewayServer,
};
use vam_spec::v1::{
    ErrorStatus, RequestRunTriggerRequest, RequestRunTriggerResponse, RevokeRunGrantRequest,
    RevokeRunGrantResponse, RunTriggerOutcome, VamErrorCode,
};

pub const TOKEN: &str = "vam_rg_rg_ABC.topsecret";

pub type Scripted = Result<RequestRunTriggerResponse, tonic::Status>;

#[derive(Clone, Default)]
pub struct FakeGateway {
    pub script: Arc<Mutex<VecDeque<Scripted>>>,
    pub fires: Arc<Mutex<Vec<RequestRunTriggerRequest>>>,
    pub revokes: Arc<Mutex<Vec<String>>>,
    pub revoke_error: Arc<Mutex<bool>>,
    pub hang: Arc<Mutex<bool>>,
}

#[tonic::async_trait]
impl HostedTriggerGateway for FakeGateway {
    async fn request_run_trigger(
        &self,
        request: tonic::Request<RequestRunTriggerRequest>,
    ) -> Result<tonic::Response<RequestRunTriggerResponse>, tonic::Status> {
        self.fires.lock().unwrap().push(request.into_inner());
        if *self.hang.lock().unwrap() {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
        let next = self.script.lock().unwrap().pop_front();
        next.unwrap_or_else(|| Err(tonic::Status::internal("script exhausted")))
            .map(tonic::Response::new)
    }

    async fn revoke_run_grant(
        &self,
        request: tonic::Request<RevokeRunGrantRequest>,
    ) -> Result<tonic::Response<RevokeRunGrantResponse>, tonic::Status> {
        self.revokes
            .lock()
            .unwrap()
            .push(request.into_inner().run_grant);
        let error = if *self.revoke_error.lock().unwrap() {
            Some(ErrorStatus {
                code: VamErrorCode::Internal as i32,
                message: "boom".into(),
                retryable: true,
                details: vec![],
            })
        } else {
            None
        };
        Ok(tonic::Response::new(RevokeRunGrantResponse { error }))
    }
}

pub fn serve_gateway(gw: FakeGateway, sock: &Path) -> tokio::task::JoinHandle<()> {
    let listener = bind_unix_listener(sock).expect("bind");
    tokio::spawn(async move {
        let _ = Server::builder()
            .add_service(HostedTriggerGatewayServer::new(gw))
            .serve_with_incoming(UnixListenerStream::new(listener))
            .await;
    })
}

pub fn resp(outcome: RunTriggerOutcome) -> RequestRunTriggerResponse {
    RequestRunTriggerResponse {
        outcome: outcome as i32,
        ..Default::default()
    }
}

pub fn request(key: &str) -> FireRequest {
    FireRequest {
        token: GrantToken::new(TOKEN),
        idempotency_key: key.into(),
        fire_at: 1_000,
        trigger_ref: "sch_ABC".into(),
        kind: FireKind::Scheduled,
        not_after: Some(1_000 + 86_400),
    }
}
