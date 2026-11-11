use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tokio_stream::Stream;
use tonic::{Request, Response, Status};
use tracing::{info, warn};

use crate::workflow::SqliteWorkflowStore;
use vam_spec::v1::telemetry_stream_service_server::TelemetryStreamService;
use vam_spec::v1::{StreamEventsRequest, VamCloudEvent};
use vam_telemetry::BroadcastEventBus;

/// Server implementing the TelemetryStreamService gRPC API.
/// Provides real-time event streaming with WAL cursor replay (Decision T13).
pub struct TelemetryStreamServer {
    store: Arc<SqliteWorkflowStore>,
    bus: Arc<BroadcastEventBus>,
}

impl TelemetryStreamServer {
    pub fn new(store: Arc<SqliteWorkflowStore>, bus: Arc<BroadcastEventBus>) -> Self {
        Self { store, bus }
    }
}

/// Serve `TelemetryStreamService` on `addr` (Phase 0: localhost bind, no PDP).
///
/// Example: `grpcurl -plaintext 127.0.0.1:9090 vam.v1.TelemetryStreamService/StreamEvents`
pub async fn serve_telemetry_grpc(
    addr: std::net::SocketAddr,
    store: Arc<SqliteWorkflowStore>,
    bus: Arc<BroadcastEventBus>,
) -> Result<(), tonic::transport::Error> {
    use vam_spec::v1::telemetry_stream_service_server::TelemetryStreamServiceServer;

    tonic::transport::Server::builder()
        .add_service(TelemetryStreamServiceServer::new(
            TelemetryStreamServer::new(store, bus),
        ))
        .serve(addr)
        .await
}

#[tonic::async_trait]
impl TelemetryStreamService for TelemetryStreamServer {
    type StreamEventsStream =
        Pin<Box<dyn Stream<Item = Result<VamCloudEvent, Status>> + Send + 'static>>;

    async fn stream_events(
        &self,
        request: Request<StreamEventsRequest>,
    ) -> Result<Response<Self::StreamEventsStream>, Status> {
        let req = request.into_inner();
        let filter_session = if req.filter_session_id.is_empty() {
            None
        } else {
            Some(req.filter_session_id)
        };
        let filter_types = if req.filter_event_types.is_empty() {
            None
        } else {
            Some(req.filter_event_types)
        };
        let resume_from = req.resume_from_sequence;

        info!(
            resume_from_sequence = resume_from,
            filter_session = ?filter_session,
            filter_types = ?filter_types,
            "Client subscribed to TelemetryStreamService.StreamEvents"
        );

        let (tx, rx) = mpsc::channel(256);
        let store = self.store.clone();
        let mut subscriber = self.bus.subscribe();

        tokio::spawn(async move {
            let mut last_streamed_seq = resume_from;

            // 1. Replay historical events from store WAL / workflow_outbox
            match store.read_outbox(resume_from) {
                Ok(historical_events) => {
                    for outbox_event in historical_events {
                        // Filter by session_id / run_id
                        if let Some(ref session) = filter_session {
                            if &outbox_event.run_id != session {
                                continue;
                            }
                        }
                        // Filter by event_types
                        if let Some(ref types) = filter_types {
                            if !types.iter().any(|t| t == &outbox_event.event_type) {
                                continue;
                            }
                        }

                        let cloud_event = crate::events::cloud_event_from_raw(
                            &outbox_event.run_id,
                            outbox_event.sequence,
                            &outbox_event.event_type,
                            &outbox_event.payload,
                        );
                        last_streamed_seq = outbox_event.sequence;
                        if tx.send(Ok(cloud_event)).await.is_err() {
                            return; // Client disconnected
                        }
                    }
                }
                Err(e) => {
                    let _ = tx
                        .send(Err(Status::internal(format!("Failed to read outbox: {e}"))))
                        .await;
                    return;
                }
            }

            // 2. Stream live events from broadcast bus
            loop {
                match subscriber.recv().await {
                    Ok(event) => {
                        // Avoid duplicates if event was already replayed from historical WAL
                        if event.sequence_number <= last_streamed_seq {
                            continue;
                        }

                        // Filter by session_id / run_id
                        if let Some(ref session) = filter_session {
                            if let Some(ref r_id) = event.run_id {
                                if &r_id.urn != session {
                                    continue;
                                }
                            }
                        }
                        // Filter by event_types
                        if let Some(ref types) = filter_types {
                            if !types.iter().any(|t| t == &event.r#type) {
                                continue;
                            }
                        }

                        last_streamed_seq = event.sequence_number;
                        if tx.send(Ok((*event).clone())).await.is_err() {
                            break; // Client disconnected
                        }
                    }
                    Err(vam_telemetry::VamTelemetryError::SlowConsumerEvicted { reason }) => {
                        warn!("Streaming subscriber evicted: {reason}");
                        let _ = tx.send(Err(Status::resource_exhausted(reason))).await;
                        break;
                    }
                    Err(e) => {
                        let _ = tx
                            .send(Err(Status::internal(format!("Event bus error: {e}"))))
                            .await;
                        break;
                    }
                }
            }
        });

        Ok(Response::new(Box::pin(ReceiverStream::new(rx))))
    }
}
