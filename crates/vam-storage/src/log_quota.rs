//! Size limits of the thread log and the prompt store (AR-2, Wave 3b).
//!
//! The host writes a thread's conversation to disk from what the guest sends
//! (a request may carry up to 10 MiB), and stores each distinct system prompt
//! and tool list by digest. Without a bound a guest could fill the host's disk.
//! [`LogQuota`] bounds one thread's log and the whole workspace (every thread's
//! log plus the prompt store), and keeps a small reserve for control records so
//! a full thread can still record that it is full and that its task ended.
//!
//! Enforced where the bytes are written: `TreeThreadStore::append` and
//! `PromptStore::put`. Over the limit is a typed error, never silent loss.

use std::sync::{Arc, Mutex};

pub const MIB: u64 = 1024 * 1024;
/// One thread's log.
pub const DEFAULT_THREAD_LOG_BYTES: u64 = 256 * MIB;
/// Every thread's log plus the prompt store of one workspace.
pub const DEFAULT_WORKSPACE_LOG_BYTES: u64 = 1024 * MIB;
/// Extra room, above both limits, for control records only.
pub const DEFAULT_LOG_RESERVE_BYTES: u64 = MIB;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogQuota {
    pub thread_bytes: u64,
    pub workspace_bytes: u64,
    pub reserve_bytes: u64,
}

impl Default for LogQuota {
    fn default() -> Self {
        Self {
            thread_bytes: DEFAULT_THREAD_LOG_BYTES,
            workspace_bytes: DEFAULT_WORKSPACE_LOG_BYTES,
            reserve_bytes: DEFAULT_LOG_RESERVE_BYTES,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuotaScope {
    Thread,
    Workspace,
}

impl QuotaScope {
    pub fn as_str(self) -> &'static str {
        match self {
            QuotaScope::Thread => "thread",
            QuotaScope::Workspace => "workspace",
        }
    }
}

struct UsageInner {
    bytes: Mutex<Option<u64>>,
    scan: Box<dyn Fn() -> u64 + Send + Sync>,
}

/// Bytes a workspace's thread logs and prompt store hold, shared by the stores
/// that write them. Counted from disk on first use, then kept up to date by the
/// writers (one session holds a workspace at a time).
#[derive(Clone)]
pub struct LogUsage {
    inner: Arc<UsageInner>,
}

impl std::fmt::Debug for LogUsage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LogUsage").finish_non_exhaustive()
    }
}

impl LogUsage {
    /// `scan` measures the workspace's bytes on disk.
    pub fn new(scan: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(UsageInner {
                bytes: Mutex::new(None),
                scan: Box::new(scan),
            }),
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut u64) -> R) -> R {
        let mut guard = self
            .inner
            .bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let slot = guard.get_or_insert_with(|| (self.inner.scan)());
        f(slot)
    }

    pub fn used(&self) -> u64 {
        self.with(|used| *used)
    }

    /// Adds `bytes` if the total stays at or under `limit`; otherwise returns
    /// the bytes in use and adds nothing.
    pub fn try_add(&self, bytes: u64, limit: u64) -> Result<(), u64> {
        self.with(|used| {
            if used.saturating_add(bytes) > limit {
                Err(*used)
            } else {
                *used += bytes;
                Ok(())
            }
        })
    }

    pub fn sub(&self, bytes: u64) {
        self.with(|used| *used = used.saturating_sub(bytes));
    }

    /// Forget the count: the next use measures the disk again (after a write
    /// failed part way).
    pub fn invalidate(&self) {
        *self
            .inner
            .bytes
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_the_approved_numbers() {
        let q = LogQuota::default();
        assert_eq!(q.thread_bytes, 256 * MIB);
        assert_eq!(q.workspace_bytes, 1024 * MIB);
        assert_eq!(q.reserve_bytes, MIB);
    }

    #[test]
    fn usage_is_measured_once_then_counted() {
        let scans = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counter = scans.clone();
        let usage = LogUsage::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            100
        });
        assert_eq!(usage.used(), 100);
        assert_eq!(usage.try_add(50, 200), Ok(()));
        assert_eq!(usage.try_add(51, 200), Err(150));
        usage.sub(20);
        assert_eq!(usage.used(), 130);
        assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 1);
        usage.invalidate();
        assert_eq!(usage.used(), 100);
        assert_eq!(scans.load(std::sync::atomic::Ordering::SeqCst), 2);
    }
}
