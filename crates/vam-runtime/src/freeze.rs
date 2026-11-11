//! Holding the workspace filesystem still while the host copies its image.
//!
//! A turn commit of a microVM is a copy of the guest's ext4 image, taken by the
//! host while the guest waits on `CommitTurn`. Written blocks still sitting in
//! the guest's page cache would be missing from that copy, and a copy taken
//! mid-write would not be a filesystem. So the guest does what the host cannot:
//!
//! 1. `syncfs` on `/mnt/drive9`, so the dirty data and metadata are on the disk;
//! 2. `FIFREEZE`, so nothing writes until the thaw;
//! 3. sends `CommitTurn` and waits for the reply, which comes once the host has
//!    its copy;
//! 4. `FITHAW`, on the reply, an error, a timeout or a dropped future (an RAII
//!    guard), and, if the async runtime itself is wedged, from a plain
//!    `std::thread` watchdog after [`FREEZE_WATCHDOG`].
//!
//! While frozen nothing here touches `/mnt/drive9`: the descriptor used to thaw
//! is opened before the freeze. Without an ext4 mount at `/mnt/drive9` (process
//! guests, tests) none of this applies and the request is made as it always was.
//! If the freeze itself fails the round's commit is skipped, and the reason is
//! logged: a copy of a filesystem nobody froze is not a commit.

use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where the workspace image is mounted in the guest.
pub const DRIVE9_MOUNT: &str = "/mnt/drive9";

/// `_IOWR('X', 119, int)` from `<linux/fs.h>`.
pub const FIFREEZE: u32 = 0xC004_5877;
/// `_IOWR('X', 120, int)` from `<linux/fs.h>`.
pub const FITHAW: u32 = 0xC004_5878;

/// The host gives a `CommitTurn` [`crate::pep::COMMIT_TURN_TIMEOUT`] (120 s); the
/// watchdog thaws five seconds after that, whatever the runtime is doing.
pub const FREEZE_WATCHDOG: Duration = Duration::from_secs(125);

/// The filesystem operations a freeze needs. The real one talks to the kernel;
/// tests use a fake that records the order of the calls.
pub trait FsFreezer: Send + Sync {
    /// Whether there is a filesystem to freeze (an ext4 mount at the workspace
    /// mount point).
    fn applies(&self) -> bool;
    /// Flushes the filesystem's dirty data and metadata to the disk.
    fn sync(&self) -> io::Result<()>;
    /// Blocks all writes until [`thaw`](Self::thaw).
    fn freeze(&self) -> io::Result<()>;
    /// Lifts the freeze. Must not touch the frozen filesystem by path.
    fn thaw(&self) -> io::Result<()>;
}

/// What came of running a request around a freeze.
#[derive(Debug, PartialEq, Eq)]
pub enum Guarded<T> {
    /// There is no ext4 mount: the request ran unfrozen.
    NotApplicable(T),
    /// The request ran with the filesystem frozen, and it has been thawed.
    Held(T),
    /// The freeze (or the sync before it) failed: the request was not made.
    Skipped(String),
}

/// Thaws exactly once: on drop, or from the watchdog, whichever comes first.
struct ThawGuard {
    state: Arc<ThawState>,
    stop: mpsc::Sender<()>,
}

struct ThawState {
    freezer: Arc<dyn FsFreezer>,
    thawed: AtomicBool,
}

impl ThawState {
    fn thaw(&self, why: &str) {
        if self.thawed.swap(true, Ordering::SeqCst) {
            return;
        }
        match self.freezer.thaw() {
            Ok(()) => {}
            Err(e) => eprintln!("[guest-agent] FITHAW failed ({why}): {e}"),
        }
    }
}

impl ThawGuard {
    fn new(freezer: Arc<dyn FsFreezer>, watchdog: Duration) -> Self {
        let state = Arc::new(ThawState {
            freezer,
            thawed: AtomicBool::new(false),
        });
        let (stop, wait) = mpsc::channel::<()>();
        let watching = state.clone();
        // A plain thread: it must fire even if the tokio runtime is wedged.
        let spawned = std::thread::Builder::new()
            .name("freeze-watchdog".into())
            .spawn(move || {
                if let Err(mpsc::RecvTimeoutError::Timeout) = wait.recv_timeout(watchdog) {
                    eprintln!(
                        "[guest-agent] freeze watchdog: thawing after {} s",
                        watchdog.as_secs()
                    );
                    watching.thaw("watchdog");
                }
            });
        if let Err(e) = spawned {
            // Without a watchdog the RAII thaw is the only one; say so.
            eprintln!("[guest-agent] freeze watchdog thread not started: {e}");
        }
        Self { state, stop }
    }
}

impl Drop for ThawGuard {
    fn drop(&mut self) {
        self.state.thaw("guard");
        let _ = self.stop.send(());
    }
}

/// Runs `request` with the freezer's filesystem synced and frozen, and thaws it
/// afterwards however the request ends (a value, an error inside the value, a
/// timeout inside it, or the future being dropped). Logs
/// `[guest-agent] turn commit: sync_ms=… freeze_held_ms=…`.
pub async fn around_frozen<T, Fut>(
    freezer: &Arc<dyn FsFreezer>,
    watchdog: Duration,
    request: impl FnOnce() -> Fut,
) -> Guarded<T>
where
    Fut: Future<Output = T>,
{
    if !freezer.applies() {
        return Guarded::NotApplicable(request().await);
    }
    let started = Instant::now();
    if let Err(e) = freezer.sync() {
        return Guarded::Skipped(format!("syncfs failed: {e}"));
    }
    let sync_ms = started.elapsed().as_millis();
    if let Err(e) = freezer.freeze() {
        return Guarded::Skipped(format!("FIFREEZE failed: {e}"));
    }
    let frozen_at = Instant::now();
    let guard = ThawGuard::new(freezer.clone(), watchdog);
    let value = request().await;
    drop(guard);
    eprintln!(
        "[guest-agent] turn commit: sync_ms={sync_ms} freeze_held_ms={}",
        frozen_at.elapsed().as_millis()
    );
    Guarded::Held(value)
}

/// Whether `mounts` (the text of `/proc/self/mounts`) has an ext4 filesystem
/// mounted exactly at `mount_point`. The last matching line wins (a later mount
/// hides an earlier one).
pub fn mount_is_ext4(mounts: &str, mount_point: &str) -> bool {
    mounts
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _device = fields.next()?;
            let point = fields.next()?;
            let fstype = fields.next()?;
            (point == mount_point).then_some(fstype)
        })
        .next_back()
        == Some("ext4")
}

/// The real thing: `syncfs`, `FIFREEZE` and `FITHAW` on a descriptor of the
/// workspace mount.
pub struct Drive9Freezer {
    mount: String,
    /// Opened before the freeze and kept for the thaw: nothing opens the frozen
    /// filesystem by path.
    held: Mutex<Option<std::fs::File>>,
}

impl Drive9Freezer {
    pub fn new(mount: impl Into<String>) -> Self {
        Self {
            mount: mount.into(),
            held: Mutex::new(None),
        }
    }

    fn open(&self) -> io::Result<std::fs::File> {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_CLOEXEC)
            .open(&self.mount)
    }
}

impl Default for Drive9Freezer {
    fn default() -> Self {
        Self::new(DRIVE9_MOUNT)
    }
}

#[cfg(target_os = "linux")]
impl FsFreezer for Drive9Freezer {
    fn applies(&self) -> bool {
        std::fs::read_to_string("/proc/self/mounts")
            .map(|mounts| mount_is_ext4(&mounts, &self.mount))
            .unwrap_or(false)
    }

    fn sync(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let dir = self.open()?;
        // SAFETY: plain syscall on an open descriptor.
        if unsafe { libc::syncfs(dir.as_raw_fd()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn freeze(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let dir = self.open()?;
        // SAFETY: FIFREEZE ignores its argument; the descriptor is open.
        if unsafe { libc::ioctl(dir.as_raw_fd(), FIFREEZE as _, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        *self.held.lock().unwrap_or_else(|p| p.into_inner()) = Some(dir);
        Ok(())
    }

    fn thaw(&self) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        let dir = self.held.lock().unwrap_or_else(|p| p.into_inner()).take();
        let Some(dir) = dir else {
            return Ok(());
        };
        // SAFETY: FITHAW ignores its argument; the descriptor is open.
        if unsafe { libc::ioctl(dir.as_raw_fd(), FITHAW as _, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(not(target_os = "linux"))]
impl FsFreezer for Drive9Freezer {
    fn applies(&self) -> bool {
        false
    }
    fn sync(&self) -> io::Result<()> {
        Ok(())
    }
    fn freeze(&self) -> io::Result<()> {
        Ok(())
    }
    fn thaw(&self) -> io::Result<()> {
        Ok(())
    }
}

/// The freezer of this guest: the workspace mount.
pub fn drive9_freezer() -> Arc<dyn FsFreezer> {
    Arc::new(Drive9Freezer::default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records each call, in order, with the time it happened.
    #[derive(Default)]
    struct Fake {
        applies: bool,
        fail_sync: bool,
        fail_freeze: bool,
        events: Mutex<Vec<&'static str>>,
        thaws: Mutex<Vec<Instant>>,
    }

    impl Fake {
        fn mounted() -> Arc<Self> {
            Arc::new(Self {
                applies: true,
                ..Self::default()
            })
        }
        fn log(&self, what: &'static str) {
            self.events.lock().unwrap().push(what);
        }
        fn events(&self) -> Vec<&'static str> {
            self.events.lock().unwrap().clone()
        }
    }

    impl FsFreezer for Fake {
        fn applies(&self) -> bool {
            self.applies
        }
        fn sync(&self) -> io::Result<()> {
            self.log("sync");
            if self.fail_sync {
                return Err(io::Error::other("sync refused"));
            }
            Ok(())
        }
        fn freeze(&self) -> io::Result<()> {
            self.log("freeze");
            if self.fail_freeze {
                return Err(io::Error::from_raw_os_error(libc::EBUSY));
            }
            Ok(())
        }
        fn thaw(&self) -> io::Result<()> {
            self.log("thaw");
            self.thaws.lock().unwrap().push(Instant::now());
            Ok(())
        }
    }

    fn dynamic(fake: &Arc<Fake>) -> Arc<dyn FsFreezer> {
        fake.clone()
    }

    #[test]
    fn the_ioctl_numbers_are_iowr_x_119_and_120_of_int() {
        // _IOWR(type, nr, size): (3 << 30) | (size << 16) | (type << 8) | nr
        let iowr = |nr: u32| (3u32 << 30) | (4 << 16) | ((b'X' as u32) << 8) | nr;
        assert_eq!(FIFREEZE, iowr(119));
        assert_eq!(FITHAW, iowr(120));
        assert_eq!(FIFREEZE, 0xC004_5877);
        assert_eq!(FITHAW, 0xC004_5878);
    }

    #[tokio::test]
    async fn the_order_is_sync_freeze_request_reply_thaw() {
        let fake = Fake::mounted();
        let f = fake.clone();
        let out = around_frozen(&dynamic(&fake), Duration::from_secs(30), || async move {
            f.log("request");
            "reply"
        })
        .await;
        assert_eq!(out, Guarded::Held("reply"));
        assert_eq!(fake.events(), vec!["sync", "freeze", "request", "thaw"]);
    }

    #[tokio::test]
    async fn a_refusal_or_an_error_inside_the_request_still_thaws() {
        for result in [Err::<(), _>("refused: code 13"), Err("transport error")] {
            let fake = Fake::mounted();
            let out = around_frozen(&dynamic(&fake), Duration::from_secs(30), || async move {
                result
            })
            .await;
            assert_eq!(out, Guarded::Held(result));
            assert_eq!(fake.events(), vec!["sync", "freeze", "thaw"]);
        }
    }

    #[tokio::test]
    async fn a_timeout_inside_the_request_thaws() {
        let fake = Fake::mounted();
        let out = around_frozen(&dynamic(&fake), Duration::from_secs(30), || async {
            tokio::time::timeout(Duration::from_millis(20), std::future::pending::<()>()).await
        })
        .await;
        assert!(matches!(out, Guarded::Held(Err(_))));
        assert_eq!(fake.events(), vec!["sync", "freeze", "thaw"]);
    }

    #[tokio::test]
    async fn dropping_the_future_while_frozen_thaws() {
        let fake = Fake::mounted();
        let freezer = dynamic(&fake);
        let cancelled = tokio::time::timeout(
            Duration::from_millis(50),
            around_frozen(&freezer, Duration::from_secs(30), || {
                std::future::pending::<()>()
            }),
        )
        .await;
        assert!(cancelled.is_err(), "the request never finishes");
        assert_eq!(fake.events(), vec!["sync", "freeze", "thaw"]);
    }

    #[tokio::test]
    async fn the_watchdog_thaws_even_when_the_runtime_is_wedged() {
        let fake = Fake::mounted();
        let wedged = fake.clone();
        let started = Instant::now();
        // The request blocks the runtime thread itself: only a plain thread can
        // thaw. The watchdog is 100 ms; the block lasts 600 ms.
        let out = around_frozen(&dynamic(&fake), Duration::from_millis(100), || async move {
            wedged.log("request");
            std::thread::sleep(Duration::from_millis(600));
            wedged.log("request done");
        })
        .await;
        assert!(matches!(out, Guarded::Held(())));
        // The thaw happened once, by the watchdog, before the request returned.
        assert_eq!(
            fake.events(),
            vec!["sync", "freeze", "request", "thaw", "request done"]
        );
        let thawed_at = fake.thaws.lock().unwrap()[0];
        assert!(thawed_at.duration_since(started) < Duration::from_millis(500));
    }

    #[tokio::test]
    async fn without_an_ext4_mount_the_request_runs_unfrozen() {
        let fake = Arc::new(Fake::default());
        let out = around_frozen(&dynamic(&fake), Duration::from_secs(30), || async { 7 }).await;
        assert_eq!(out, Guarded::NotApplicable(7));
        assert!(fake.events().is_empty());
    }

    #[tokio::test]
    async fn a_failed_freeze_or_sync_skips_the_request_and_says_why() {
        let fake = Arc::new(Fake {
            applies: true,
            fail_freeze: true,
            ..Fake::default()
        });
        let out = around_frozen(&dynamic(&fake), Duration::from_secs(30), || async {
            panic!("the request must not be made")
        })
        .await;
        match out {
            Guarded::Skipped(why) => assert!(why.contains("FIFREEZE failed"), "{why}"),
            _ => panic!("expected Skipped"),
        }
        // Never frozen, so nothing to thaw.
        assert_eq!(fake.events(), vec!["sync", "freeze"]);

        let fake = Arc::new(Fake {
            applies: true,
            fail_sync: true,
            ..Fake::default()
        });
        let out = around_frozen(&dynamic(&fake), Duration::from_secs(30), || async {}).await;
        assert!(matches!(out, Guarded::Skipped(why) if why.contains("syncfs")));
        assert_eq!(fake.events(), vec!["sync"]);
    }

    #[test]
    fn only_an_ext4_mount_at_the_exact_mount_point_applies() {
        let mounts = "\
rootfs / ext4 ro,relatime 0 0\n\
proc /proc proc rw 0 0\n\
/dev/vdb /mnt/drive9 ext4 rw,relatime 0 0\n\
/dev/vdc /mnt/drive9/skills ext4 ro,relatime 0 0\n";
        assert!(mount_is_ext4(mounts, "/mnt/drive9"));
        assert!(mount_is_ext4(mounts, "/mnt/drive9/skills"));
        assert!(!mount_is_ext4(mounts, "/mnt"));
        assert!(!mount_is_ext4(mounts, "/mnt/drive"));
        let tmpfs = "tmpfs /mnt/drive9 tmpfs rw 0 0\n";
        assert!(!mount_is_ext4(tmpfs, "/mnt/drive9"));
        // A later mount on the same point hides the earlier one.
        let stacked = "/dev/vdb /mnt/drive9 ext4 rw 0 0\ntmpfs /mnt/drive9 tmpfs rw 0 0\n";
        assert!(!mount_is_ext4(stacked, "/mnt/drive9"));
        assert!(!mount_is_ext4("", "/mnt/drive9"));
    }

    #[test]
    fn the_real_freezer_does_not_apply_where_there_is_no_workspace_mount() {
        let freezer = Drive9Freezer::new("/nonexistent/drive9");
        assert!(!freezer.applies());
        assert!(freezer.freeze().is_err());
        assert!(freezer.sync().is_err());
        // Thawing what was never frozen is a no-op.
        assert!(freezer.thaw().is_ok());
    }
}
