use crate::error::VamTelemetryError;
use std::sync::Arc;
use tokio::sync::broadcast::{self, error::RecvError};
use vam_spec::v1::VamCloudEvent;

/// Default capacity for the in-process event broadcast bus.
pub const DEFAULT_BUS_CAPACITY: usize = 10_000;

/// Slow consumer byte cap: 16 MB limit before eviction (Decision T13).
pub const MAX_SLOW_CONSUMER_BYTES: usize = 16 * 1024 * 1024;

/// In-process real-time broadcast event bus for VAM CloudEvents.
/// Buffers up to 10,000 events and dispatches to live subscribers.
#[derive(Clone, Debug)]
pub struct BroadcastEventBus {
    sender: broadcast::Sender<Arc<VamCloudEvent>>,
    capacity: usize,
}

impl Default for BroadcastEventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl BroadcastEventBus {
    /// Create a new broadcast bus with the default 10,000 capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_BUS_CAPACITY)
    }

    /// Create a new broadcast bus with a custom capacity.
    pub fn with_capacity(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self { sender, capacity }
    }

    /// Publish an event to all live subscribers.
    /// Returns the number of active receivers.
    pub fn publish(&self, event: VamCloudEvent) -> usize {
        let arc_event = Arc::new(event);
        // send() errors only when there are zero active receivers, which is a normal state.
        self.sender.send(arc_event).unwrap_or(0)
    }

    /// Subscribe to real-time events on this bus.
    pub fn subscribe(&self) -> EventSubscriber {
        EventSubscriber {
            receiver: self.sender.subscribe(),
            accumulated_bytes: 0,
            evicted: false,
        }
    }

    /// Number of active subscribers currently listening.
    pub fn subscriber_count(&self) -> usize {
        self.sender.receiver_count()
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }
}

/// A subscriber handle wrapping a broadcast receiver with slow-consumer byte eviction tracking (T13).
pub struct EventSubscriber {
    receiver: broadcast::Receiver<Arc<VamCloudEvent>>,
    accumulated_bytes: usize,
    evicted: bool,
}

impl EventSubscriber {
    /// Asynchronously receive the next event from the broadcast bus.
    /// If the subscriber lags behind or exceeds the 16MB threshold, it is evicted.
    pub async fn recv(&mut self) -> Result<Arc<VamCloudEvent>, VamTelemetryError> {
        if self.evicted {
            return Err(VamTelemetryError::SlowConsumerEvicted {
                reason: "Subscriber was previously evicted due to lag or memory limits".to_string(),
            });
        }

        match self.receiver.recv().await {
            Ok(event) => {
                let estimated_size = estimate_event_bytes(&event);
                self.accumulated_bytes = self.accumulated_bytes.saturating_add(estimated_size);

                if self.accumulated_bytes > MAX_SLOW_CONSUMER_BYTES {
                    self.evicted = true;
                    return Err(VamTelemetryError::SlowConsumerEvicted {
                        reason: format!(
                            "Subscriber exceeded maximum memory allowance of 16MB (accumulated {} bytes)",
                            self.accumulated_bytes
                        ),
                    });
                }

                Ok(event)
            }
            Err(RecvError::Lagged(missed)) => {
                self.evicted = true;
                Err(VamTelemetryError::SlowConsumerEvicted {
                    reason: format!(
                        "Subscriber lagged behind by {missed} events; buffer overflowed"
                    ),
                })
            }
            Err(RecvError::Closed) => Err(VamTelemetryError::SlowConsumerEvicted {
                reason: "Event bus channel closed".to_string(),
            }),
        }
    }

    /// Synchronously try to receive the next event if available without blocking.
    pub fn try_recv(&mut self) -> Result<Arc<VamCloudEvent>, VamTelemetryError> {
        if self.evicted {
            return Err(VamTelemetryError::SlowConsumerEvicted {
                reason: "Subscriber was previously evicted due to lag or memory limits".to_string(),
            });
        }

        match self.receiver.try_recv() {
            Ok(event) => {
                let estimated_size = estimate_event_bytes(&event);
                self.accumulated_bytes = self.accumulated_bytes.saturating_add(estimated_size);

                if self.accumulated_bytes > MAX_SLOW_CONSUMER_BYTES {
                    self.evicted = true;
                    return Err(VamTelemetryError::SlowConsumerEvicted {
                        reason: format!(
                            "Subscriber exceeded maximum memory allowance of 16MB (accumulated {} bytes)",
                            self.accumulated_bytes
                        ),
                    });
                }

                Ok(event)
            }
            Err(broadcast::error::TryRecvError::Lagged(missed)) => {
                self.evicted = true;
                Err(VamTelemetryError::SlowConsumerEvicted {
                    reason: format!(
                        "Subscriber lagged behind by {missed} events; buffer overflowed"
                    ),
                })
            }
            Err(broadcast::error::TryRecvError::Empty) => Err(VamTelemetryError::QueueOverflow {
                lane: "try_recv_empty".to_string(),
                size: 0,
            }),
            Err(broadcast::error::TryRecvError::Closed) => {
                Err(VamTelemetryError::SlowConsumerEvicted {
                    reason: "Event bus channel closed".to_string(),
                })
            }
        }
    }

    pub fn is_evicted(&self) -> bool {
        self.evicted
    }

    pub fn accumulated_bytes(&self) -> usize {
        self.accumulated_bytes
    }
}

fn estimate_event_bytes(event: &VamCloudEvent) -> usize {
    let id_len = event.id.len();
    let type_len = event.r#type.len();
    let data_len = event.data.as_ref().map(|d| d.value.len()).unwrap_or(0);
    id_len + type_len + data_len + 64 // 64 bytes overhead for metadata fields
}

#[cfg(test)]
mod tests {
    use super::*;
    use vam_spec::v1::ResourceId;

    fn make_test_event(id: &str, seq: i64) -> VamCloudEvent {
        VamCloudEvent {
            id: id.to_string(),
            source: Some(ResourceId {
                urn: "urn:vam:node:test".to_string(),
            }),
            spec_version: "1.0".to_string(),
            r#type: "vam.v1.test.event".to_string(),
            sequence_number: seq,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_broadcast_publish_and_receive() {
        let bus = BroadcastEventBus::with_capacity(16);
        let mut sub = bus.subscribe();

        let count = bus.publish(make_test_event("evt-1", 1));
        assert_eq!(count, 1);

        let received = sub.recv().await.unwrap();
        assert_eq!(received.id, "evt-1");
        assert_eq!(received.sequence_number, 1);
    }

    #[tokio::test]
    async fn test_slow_consumer_lag_eviction() {
        let bus = BroadcastEventBus::with_capacity(4);
        let mut sub = bus.subscribe();

        // Publish more than capacity without draining
        for i in 0..10 {
            bus.publish(make_test_event(&format!("evt-{i}"), i));
        }

        let res = sub.recv().await;
        assert!(res.is_err());
        match res.unwrap_err() {
            VamTelemetryError::SlowConsumerEvicted { reason } => {
                assert!(reason.contains("lagged behind"));
            }
            other => panic!("Expected SlowConsumerEvicted, got {:?}", other),
        }
        assert!(sub.is_evicted());
    }
}
