use crate::error::VamTelemetryError;
use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// Lossy bounded ring buffer for lossy telemetry lanes (traces and metrics).
/// Drops the oldest entry on overflow to guarantee zero backpressure on agent critical paths.
#[derive(Debug, Clone)]
pub struct LossyRingBuffer<T> {
    capacity: usize,
    inner: Arc<Mutex<RingBufferInner<T>>>,
}

#[derive(Debug)]
struct RingBufferInner<T> {
    queue: VecDeque<T>,
    dropped_count: u64,
}

impl<T> LossyRingBuffer<T> {
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "Capacity must be positive");
        Self {
            capacity,
            inner: Arc::new(Mutex::new(RingBufferInner {
                queue: VecDeque::with_capacity(capacity),
                dropped_count: 0,
            })),
        }
    }

    /// Push an item. If full, drops the oldest item and records the drop count.
    pub fn push(&self, item: T) {
        if let Ok(mut guard) = self.inner.lock() {
            if guard.queue.len() >= self.capacity {
                guard.queue.pop_front();
                guard.dropped_count += 1;
            }
            guard.queue.push_back(item);
        }
    }

    /// Pop the next item in FIFO order.
    pub fn pop(&self) -> Option<T> {
        self.inner.lock().ok()?.queue.pop_front()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|g| g.queue.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn dropped_count(&self) -> u64 {
        self.inner.lock().map(|g| g.dropped_count).unwrap_or(0)
    }
}

/// Disk-spool queue for durable event lanes.
/// Buffers events in-memory up to `memory_capacity`, then persists overflow to a
/// length-prefixed local disk spool. `dequeue` drains memory first, then disk;
/// `ack` advances the disk read cursor after a successful delivery.
#[derive(Debug)]
pub struct DiskSpoolQueue {
    spool_path: PathBuf,
    max_spool_bytes: u64,
    memory_buffer: Mutex<VecDeque<Vec<u8>>>,
    memory_capacity: usize,
    disk_read_offset: Mutex<u64>,
    /// Next file offset and payload of the unacked disk record, if any.
    inflight_disk: Mutex<Option<(u64, Vec<u8>)>>,
}

impl DiskSpoolQueue {
    pub fn new<P: AsRef<Path>>(
        spool_path: P,
        memory_capacity: usize,
        max_spool_bytes: u64,
    ) -> Self {
        Self {
            spool_path: spool_path.as_ref().to_path_buf(),
            max_spool_bytes,
            memory_buffer: Mutex::new(VecDeque::with_capacity(memory_capacity)),
            memory_capacity,
            disk_read_offset: Mutex::new(0),
            inflight_disk: Mutex::new(None),
        }
    }

    pub fn spool_path(&self) -> &Path {
        &self.spool_path
    }

    pub fn enqueue(&self, payload: Vec<u8>) -> Result<(), VamTelemetryError> {
        let mut mem =
            self.memory_buffer
                .lock()
                .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: format!("mutex poisoned: {}", e),
                })?;

        if mem.len() < self.memory_capacity {
            mem.push_back(payload);
            return Ok(());
        }

        // Memory buffer full: spool to disk
        self.spool_to_disk(&payload)?;
        Ok(())
    }

    fn spool_to_disk(&self, data: &[u8]) -> Result<(), VamTelemetryError> {
        if let Some(parent) = self.spool_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        if let Ok(metadata) = std::fs::metadata(&self.spool_path) {
            if metadata.len() + (data.len() as u64) > self.max_spool_bytes {
                return Err(VamTelemetryError::QueueOverflow {
                    lane: "durable_events_disk_spool".to_string(),
                    size: metadata.len() as usize,
                });
            }
        }

        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.spool_path)
            .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: self.spool_path.display().to_string(),
                details: e.to_string(),
            })?;

        // Write length-prefixed record
        let len_prefix = (data.len() as u32).to_le_bytes();
        file.write_all(&len_prefix)
            .and_then(|_| file.write_all(data))
            .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: self.spool_path.display().to_string(),
                details: e.to_string(),
            })?;

        Ok(())
    }

    pub fn dequeue_memory(&self) -> Option<Vec<u8>> {
        self.memory_buffer.lock().ok()?.pop_front()
    }

    pub fn memory_len(&self) -> usize {
        self.memory_buffer.lock().map(|b| b.len()).unwrap_or(0)
    }

    /// Pop the next payload: in-memory first, then one length-prefixed disk record.
    /// Disk records stay on disk until [`ack`].
    pub fn dequeue(&self) -> Result<Option<Vec<u8>>, VamTelemetryError> {
        if let Some(payload) = self.dequeue_memory() {
            return Ok(Some(payload));
        }
        self.read_next_disk_record()
    }

    /// Acknowledge the in-flight disk record from the last [`dequeue`].
    /// No-op when the last record came from memory or nothing is in flight.
    pub fn ack(&self) -> Result<(), VamTelemetryError> {
        let mut inflight =
            self.inflight_disk
                .lock()
                .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: format!("mutex poisoned: {}", e),
                })?;
        if let Some((next_offset, _)) = inflight.take() {
            let mut offset =
                self.disk_read_offset
                    .lock()
                    .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                        path: self.spool_path.display().to_string(),
                        details: format!("mutex poisoned: {}", e),
                    })?;
            *offset = next_offset;
            drop(offset);
            drop(inflight);
            self.maybe_compact()?;
        }
        Ok(())
    }

    fn read_next_disk_record(&self) -> Result<Option<Vec<u8>>, VamTelemetryError> {
        let mut inflight =
            self.inflight_disk
                .lock()
                .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: format!("mutex poisoned: {}", e),
                })?;
        if let Some((_, ref payload)) = *inflight {
            return Ok(Some(payload.clone()));
        }

        let offset =
            self.disk_read_offset
                .lock()
                .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: format!("mutex poisoned: {}", e),
                })?;
        let offset = *offset;

        if !self.spool_path.exists() {
            return Ok(None);
        }

        let mut file =
            File::open(&self.spool_path).map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: self.spool_path.display().to_string(),
                details: e.to_string(),
            })?;
        let file_len = file
            .metadata()
            .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: self.spool_path.display().to_string(),
                details: e.to_string(),
            })?
            .len();
        if offset >= file_len {
            return Ok(None);
        }

        file.seek(SeekFrom::Start(offset))
            .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: self.spool_path.display().to_string(),
                details: e.to_string(),
            })?;

        let mut len_buf = [0u8; 4];
        match file.read_exact(&mut len_buf) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => {
                return Err(VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: e.to_string(),
                });
            }
        }
        let rec_len = u32::from_le_bytes(len_buf) as usize;
        if rec_len > 16 * 1024 * 1024 {
            return Err(VamTelemetryError::SpoolDiskFailure {
                path: self.spool_path.display().to_string(),
                details: format!("disk record length {rec_len} exceeds 16MiB cap"),
            });
        }

        let mut data = vec![0u8; rec_len];
        file.read_exact(&mut data)
            .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: self.spool_path.display().to_string(),
                details: e.to_string(),
            })?;

        let next_offset = offset + 4 + rec_len as u64;
        *inflight = Some((next_offset, data.clone()));
        Ok(Some(data))
    }

    fn maybe_compact(&self) -> Result<(), VamTelemetryError> {
        const COMPACT_AFTER_BYTES: u64 = 1024 * 1024;
        let offset =
            self.disk_read_offset
                .lock()
                .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: format!("mutex poisoned: {}", e),
                })?;
        let offset = *offset;
        if offset < COMPACT_AFTER_BYTES || !self.spool_path.exists() {
            return Ok(());
        }

        let bytes =
            std::fs::read(&self.spool_path).map_err(|e| VamTelemetryError::SpoolDiskFailure {
                path: self.spool_path.display().to_string(),
                details: e.to_string(),
            })?;
        if offset as usize >= bytes.len() {
            std::fs::write(&self.spool_path, []).map_err(|e| {
                VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: e.to_string(),
                }
            })?;
        } else {
            std::fs::write(&self.spool_path, &bytes[offset as usize..]).map_err(|e| {
                VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: e.to_string(),
                }
            })?;
        }

        let mut write_offset =
            self.disk_read_offset
                .lock()
                .map_err(|e| VamTelemetryError::SpoolDiskFailure {
                    path: self.spool_path.display().to_string(),
                    details: format!("mutex poisoned: {}", e),
                })?;
        *write_offset = 0;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lossy_ring_buffer_drop_oldest() {
        let ring = LossyRingBuffer::<u32>::new(3);
        ring.push(1);
        ring.push(2);
        ring.push(3);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.dropped_count(), 0);

        // 4th push drops 1
        ring.push(4);
        assert_eq!(ring.len(), 3);
        assert_eq!(ring.dropped_count(), 1);

        assert_eq!(ring.pop(), Some(2));
        assert_eq!(ring.pop(), Some(3));
        assert_eq!(ring.pop(), Some(4));
        assert_eq!(ring.pop(), None);
    }

    #[test]
    fn test_disk_spool_queue() {
        let temp_dir = tempfile::tempdir().unwrap();
        let spool_file = temp_dir.path().join("events.spool");
        let spool = DiskSpoolQueue::new(&spool_file, 2, 1024);

        // In-memory queue
        assert!(spool.enqueue(b"event-1".to_vec()).is_ok());
        assert!(spool.enqueue(b"event-2".to_vec()).is_ok());

        // Overflow to disk
        assert!(spool.enqueue(b"event-3".to_vec()).is_ok());
        assert!(spool_file.exists());

        assert_eq!(spool.dequeue_memory(), Some(b"event-1".to_vec()));
        assert_eq!(spool.dequeue_memory(), Some(b"event-2".to_vec()));
        assert_eq!(spool.dequeue_memory(), None);
    }

    #[test]
    fn test_disk_spool_drain_and_ack() {
        let temp_dir = tempfile::tempdir().unwrap();
        let spool_file = temp_dir.path().join("events.spool");
        let spool = DiskSpoolQueue::new(&spool_file, 1, 1024);

        assert!(spool.enqueue(b"mem".to_vec()).is_ok());
        assert!(spool.enqueue(b"disk-a".to_vec()).is_ok());
        assert!(spool.enqueue(b"disk-b".to_vec()).is_ok());

        assert_eq!(spool.dequeue().unwrap(), Some(b"mem".to_vec()));
        assert!(spool.ack().is_ok());

        assert_eq!(spool.dequeue().unwrap(), Some(b"disk-a".to_vec()));
        assert_eq!(
            spool.dequeue().unwrap(),
            Some(b"disk-a".to_vec()),
            "unacked disk record must be returned again"
        );
        assert!(spool.ack().is_ok());

        assert_eq!(spool.dequeue().unwrap(), Some(b"disk-b".to_vec()));
        assert!(spool.ack().is_ok());
        assert_eq!(spool.dequeue().unwrap(), None);
    }
}
