//! The broker: `HostedBroker` (Register, no tools) and `HostedNotificationChannel`
//! (`DeliverNotification`). Delivery is the hub's: the adapter registry, the delivery log and the
//! handles held in memory.

use vam_spec::v1::hosted_broker_server::HostedBroker;
use vam_spec::v1::hosted_notification_channel_server::HostedNotificationChannel;
use vam_spec::v1::{
    DeliverNotificationRequest, DeliverNotificationResponse, InvokeRequest, InvokeResponse,
    ListToolsRequest, ListToolsResponse, OnCompleteRequest, OnCompleteResponse, OnPausedRequest,
    OnPausedResponse, OnRequestRequest, OnRequestResponse, OnResponseRequest, OnResponseResponse,
    RegisterRequest, RegisterResponse,
};

use std::sync::Arc;

use crate::hub::Hub;

#[derive(Clone)]
pub struct NotifyBroker {
    hub: Arc<Hub>,
}

impl NotifyBroker {
    pub fn new(hub: Arc<Hub>) -> Self {
        Self { hub }
    }

    pub fn hub(&self) -> &Arc<Hub> {
        &self.hub
    }
}

#[tonic::async_trait]
impl HostedBroker for NotifyBroker {
    async fn register(
        &self,
        _request: tonic::Request<RegisterRequest>,
    ) -> Result<tonic::Response<RegisterResponse>, tonic::Status> {
        Ok(tonic::Response::new(RegisterResponse {
            error: None,
            session_policy_version: String::new(),
        }))
    }

    async fn list_tools(
        &self,
        _request: tonic::Request<ListToolsRequest>,
    ) -> Result<tonic::Response<ListToolsResponse>, tonic::Status> {
        Ok(tonic::Response::new(ListToolsResponse { tools: vec![] }))
    }

    async fn invoke(
        &self,
        _request: tonic::Request<InvokeRequest>,
    ) -> Result<tonic::Response<InvokeResponse>, tonic::Status> {
        Err(tonic::Status::not_found("vam.notify exports no tools"))
    }

    async fn on_request(
        &self,
        _request: tonic::Request<OnRequestRequest>,
    ) -> Result<tonic::Response<OnRequestResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("vam.notify has no hooks"))
    }

    async fn on_complete(
        &self,
        _request: tonic::Request<OnCompleteRequest>,
    ) -> Result<tonic::Response<OnCompleteResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("vam.notify has no hooks"))
    }

    async fn on_response(
        &self,
        _request: tonic::Request<OnResponseRequest>,
    ) -> Result<tonic::Response<OnResponseResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("vam.notify has no hooks"))
    }

    async fn on_paused(
        &self,
        _request: tonic::Request<OnPausedRequest>,
    ) -> Result<tonic::Response<OnPausedResponse>, tonic::Status> {
        Err(tonic::Status::unimplemented("vam.notify has no hooks"))
    }
}

#[tonic::async_trait]
impl HostedNotificationChannel for NotifyBroker {
    async fn deliver_notification(
        &self,
        request: tonic::Request<DeliverNotificationRequest>,
    ) -> Result<tonic::Response<DeliverNotificationResponse>, tonic::Status> {
        Ok(tonic::Response::new(
            self.hub.deliver(request.into_inner()).await,
        ))
    }
}
