//! Backends of the thread log: the session tree (read-write) and the HEAD
//! manifest (read-only).

use super::read::{complete_len, complete_lines};
use super::*;
use crate::log_quota::{LogQuota, LogUsage, QuotaScope};

#[cfg(unix)]
fn nofollow(opts: &mut OpenOptions) -> &mut OpenOptions {
    use std::os::unix::fs::OpenOptionsExt;
    opts.custom_flags(libc::O_NOFOLLOW)
}

#[cfg(not(unix))]
fn nofollow(opts: &mut OpenOptions) -> &mut OpenOptions {
    opts
}

fn open_read(path: &Path) -> std::io::Result<File> {
    nofollow(OpenOptions::new().read(true)).open(path)
}

fn open_append(path: &Path, create_new: bool) -> std::io::Result<File> {
    let mut opts = OpenOptions::new();
    if create_new {
        opts.write(true).create_new(true);
    } else {
        opts.append(true);
    }
    nofollow(&mut opts).open(path)
}

fn sync_dir(path: &Path) {
    if let Ok(dir) = File::open(path) {
        let _ = dir.sync_all();
    }
}

/// `Ok(true)` if `path` is a real directory, `Ok(false)` if it is missing and
/// not created, an error if it is anything else (a symlink included).
fn ensure_real_dir(path: &Path, create: bool) -> Result<bool, ThreadLogError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_dir() => Ok(true),
        Ok(_) => Err(ThreadLogError::UnsafePath(format!(
            "{} is not a plain directory",
            path.display()
        ))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if create {
                match fs::create_dir(path) {
                    Ok(()) => Ok(true),
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                        ensure_real_dir(path, false)
                    }
                    Err(e) => Err(e.into()),
                }
            } else {
                Ok(false)
            }
        }
        Err(e) => Err(e.into()),
    }
}

fn read_file_bounded(path: &Path) -> Result<Vec<u8>, ThreadLogError> {
    let file = open_read(path)?;
    if !file.metadata()?.is_file() {
        return Err(ThreadLogError::UnsafePath(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    file.take(MAX_SEGMENT_READ_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_SEGMENT_READ_BYTES {
        return Err(ThreadLogError::Corrupt(format!(
            "{} is larger than {} bytes",
            path.display(),
            MAX_SEGMENT_READ_BYTES
        )));
    }
    Ok(bytes)
}

/// Where the open segment of a thread ends.
#[derive(Debug, Clone)]
struct WriteState {
    /// `0` when the thread has no segment yet.
    first_seq: u64,
    len: u64,
    head: Option<LogHead>,
}

type Slot = Arc<Mutex<Option<WriteState>>>;

/// The thread log in a session's materialized tree: `<root>/.vam/threads/<thread>/log/`.
pub struct TreeThreadStore {
    root: PathBuf,
    slots: Mutex<HashMap<String, Slot>>,
    quota: Option<(LogQuota, LogUsage)>,
}

/// Records the host writes about the task itself, not the conversation. They
/// may use the quota's reserve, so a full thread can still say so and end.
fn is_control_record(rec: &NewRecord) -> bool {
    rec.src == "host"
        || matches!(
            rec.kind.as_str(),
            kind::GAP
                | kind::ROLLBACK
                | kind::TASK_START
                | kind::TASK_END
                | kind::SUSPEND
                | kind::COMMIT
                | kind::FORK
        )
}

impl std::fmt::Debug for TreeThreadStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeThreadStore")
            .field("root", &self.root)
            .finish_non_exhaustive()
    }
}

impl TreeThreadStore {
    /// `root` is the session's `materialized` directory.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            slots: Mutex::new(HashMap::new()),
            quota: None,
        }
    }

    /// Bounds what this store writes: one thread's log by `quota.thread_bytes`
    /// and the workspace (this store's logs plus whatever `usage` also counts,
    /// the prompt store) by `quota.workspace_bytes`. Control records may use
    /// the reserve above both.
    pub fn with_quota(mut self, quota: LogQuota, usage: LogUsage) -> Self {
        self.quota = Some((quota, usage));
        self
    }

    /// Bytes of every thread's log segments in this tree.
    pub fn log_bytes(&self) -> u64 {
        let mut total = 0;
        for thread in self.thread_ids().unwrap_or_default() {
            total += self
                .segments(&thread)
                .map(|segments| segments.iter().map(|s| s.len).sum::<u64>())
                .unwrap_or(0);
        }
        total
    }

    /// Checks a built batch against the limits and counts it. Content records
    /// (the conversation) must fit under the limits; control records may use
    /// the reserve above them.
    fn charge(
        &self,
        thread_used: u64,
        built: &[(String, String, bool)],
    ) -> Result<(), ThreadLogError> {
        let Some((quota, usage)) = &self.quota else {
            return Ok(());
        };
        let total: u64 = built.iter().map(|(line, _, _)| line.len() as u64 + 1).sum();
        let has_content = built.iter().any(|(_, _, control)| !control);
        // A batch that mixes content and control is held to the content limit
        // for its content and may spill its control records into the reserve.
        let content_bytes: u64 = built
            .iter()
            .filter(|(_, _, control)| !control)
            .map(|(line, _, _)| line.len() as u64 + 1)
            .sum();
        let thread_cap = quota.thread_bytes.saturating_add(quota.reserve_bytes);
        if thread_used.saturating_add(total) > thread_cap
            || (has_content && thread_used.saturating_add(content_bytes) > quota.thread_bytes)
        {
            return Err(ThreadLogError::QuotaExceeded {
                scope: QuotaScope::Thread,
                limit: quota.thread_bytes,
                used: thread_used,
            });
        }
        let workspace_cap = quota.workspace_bytes.saturating_add(quota.reserve_bytes);
        let workspace_used = usage.used();
        if has_content && workspace_used.saturating_add(content_bytes) > quota.workspace_bytes {
            return Err(ThreadLogError::QuotaExceeded {
                scope: QuotaScope::Workspace,
                limit: quota.workspace_bytes,
                used: workspace_used,
            });
        }
        usage
            .try_add(total, workspace_cap)
            .map_err(|used| ThreadLogError::QuotaExceeded {
                scope: QuotaScope::Workspace,
                limit: quota.workspace_bytes,
                used,
            })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The legacy `timeline.jsonl` of a thread in this tree (empty when absent).
    pub fn timeline(
        &self,
        thread: &str,
    ) -> Result<Vec<crate::vam_dir::TurnRecord>, ThreadLogError> {
        let thread = validate_thread(thread)?;
        crate::vam_dir::read_timeline(&self.root, &thread)
            .map_err(|e| ThreadLogError::Corrupt(format!("invalid timeline: {e}")))
    }

    fn threads_dir(&self, create: bool) -> Result<Option<PathBuf>, ThreadLogError> {
        let vam = self.root.join(VAM_DIR);
        if !ensure_real_dir(&vam, create)? {
            return Ok(None);
        }
        let threads = vam.join(THREADS_DIR);
        if !ensure_real_dir(&threads, create)? {
            return Ok(None);
        }
        Ok(Some(threads))
    }

    /// `<root>/.vam/threads/<thread>`, every component checked.
    fn thread_dir(&self, thread: &str, create: bool) -> Result<Option<PathBuf>, ThreadLogError> {
        let Some(threads) = self.threads_dir(create)? else {
            return Ok(None);
        };
        let dir = threads.join(thread);
        Ok(ensure_real_dir(&dir, create)?.then_some(dir))
    }

    fn log_dir(&self, thread: &str, create: bool) -> Result<Option<PathBuf>, ThreadLogError> {
        let Some(dir) = self.thread_dir(thread, create)? else {
            return Ok(None);
        };
        let log = dir.join(LOG_DIR);
        Ok(ensure_real_dir(&log, create)?.then_some(log))
    }

    fn slot(&self, thread: &str) -> Result<Slot, ThreadLogError> {
        let mut slots = self
            .slots
            .lock()
            .map_err(|_| ThreadLogError::Corrupt("thread store lock poisoned".into()))?;
        Ok(slots.entry(thread.to_string()).or_default().clone())
    }

    /// Reads the tail from disk: repairs a torn last line, drops trailing empty
    /// segments, and finds the head.
    fn load_state(&self, thread: &str, log_dir: &Path) -> Result<WriteState, ThreadLogError> {
        let mut segments = self.segments(thread)?;
        while let Some(segment) = segments.pop() {
            let path = log_dir.join(segment_file_name(segment.first_seq));
            let bytes = read_file_bounded(&path)?;
            let good = complete_len(&bytes);
            if good != bytes.len() {
                let file = nofollow(OpenOptions::new().write(true)).open(&path)?;
                file.set_len(good as u64)?;
                file.sync_all()?;
            }
            let lines = complete_lines(&bytes);
            let Some(last) = lines.last() else {
                fs::remove_file(&path)?;
                sync_dir(log_dir);
                continue;
            };
            return match parse_line(last) {
                Ok(record) => Ok(WriteState {
                    first_seq: segment.first_seq,
                    len: good as u64,
                    head: Some(record.head()),
                }),
                Err(LineFail::Version(version)) => Err(ThreadLogError::UnsupportedVersion {
                    seq: segment.first_seq,
                    version,
                }),
                Err(LineFail::Broken(reason)) => Err(ThreadLogError::Corrupt(format!(
                    "last record of {} is unreadable: {reason}",
                    path.display()
                ))),
            };
        }
        Ok(WriteState {
            first_seq: 0,
            len: 0,
            head: None,
        })
    }

    fn append_locked(
        &self,
        thread: &str,
        state: &mut Option<WriteState>,
        batch: Vec<NewRecord>,
    ) -> Result<AppendReceipt, ThreadLogError> {
        let log_dir = self
            .log_dir(thread, true)?
            .ok_or_else(|| ThreadLogError::Corrupt("log directory unavailable".into()))?;
        let segments = self.segments(thread)?;
        let cached_ok = match (state.as_ref(), segments.last()) {
            (Some(st), Some(last)) => st.first_seq == last.first_seq && st.len == last.len,
            (Some(st), None) => st.first_seq == 0,
            (None, _) => false,
        };
        if !cached_ok {
            *state = Some(self.load_state(thread, &log_dir)?);
        }
        let st = state
            .as_mut()
            .ok_or_else(|| ThreadLogError::Corrupt("no write state".into()))?;

        let ts_ms = now_ms();
        let mut next_seq = st.head.as_ref().map(|h| h.seq + 1).unwrap_or(1);
        let prev = st
            .head
            .as_ref()
            .map(|h| h.digest.clone())
            .unwrap_or_else(genesis_digest);
        let first_seq = next_seq;

        // Build every line first so the size limits see the exact bytes.
        let mut built: Vec<(String, String, bool)> = Vec::with_capacity(batch.len());
        {
            let mut seq = next_seq;
            let mut chain = prev.clone();
            // `seq` is also read after a gap rewrite inside the body; keep the explicit counter.
            #[allow(clippy::explicit_counter_loop)]
            for rec in batch {
                let mut control = is_control_record(&rec);
                let (mut line, mut digest, _) = build_line(&rec, seq, &chain, ts_ms);
                if line.len() + 1 > MAX_RECORD_BYTES {
                    let gap = NewRecord {
                        body: serde_json::json!({
                            "reason": "record_too_large",
                            "bytes": line.len(),
                            "original_kind": rec.kind,
                            "original_body_sha256":
                                sha256_hex(canonical_json(&rec.body).as_bytes()),
                        }),
                        kind: kind::GAP.to_string(),
                        ..rec
                    };
                    control = true;
                    (line, digest, _) = build_line(&gap, seq, &chain, ts_ms);
                }
                chain = digest.clone();
                built.push((line, digest, control));
                seq += 1;
            }
        }
        let thread_used: u64 = segments.iter().map(|s| s.len).sum();
        self.charge(thread_used, &built)?;

        let mut file: Option<File> = None;
        let mut created_segment = false;
        for (line, digest, _) in built {
            let need = line.len() as u64 + 1;
            let rotate = st.first_seq == 0 || (st.len > 0 && st.len + need > SEGMENT_MAX_BYTES);
            if rotate {
                if let Some(old) = file.take() {
                    old.sync_data()?;
                }
                st.first_seq = next_seq;
                st.len = 0;
            }
            if file.is_none() {
                let path = log_dir.join(segment_file_name(st.first_seq));
                let new = st.len == 0;
                file = Some(open_append(&path, new)?);
                created_segment |= new;
            }
            if let Some(f) = file.as_mut() {
                let mut out = line.into_bytes();
                out.push(b'\n');
                f.write_all(&out)?;
            }
            st.len += need;
            st.head = Some(LogHead {
                seq: next_seq,
                digest: digest.clone(),
            });
            next_seq += 1;
        }
        if let Some(f) = file {
            f.sync_data()?;
        }
        if created_segment {
            sync_dir(&log_dir);
        }
        let head = st
            .head
            .clone()
            .ok_or_else(|| ThreadLogError::Corrupt("empty batch".into()))?;
        Ok(AppendReceipt { first_seq, head })
    }
}

impl SegmentSource for TreeThreadStore {
    fn thread_ids(&self) -> Result<Vec<String>, ThreadLogError> {
        let Some(threads) = self.threads_dir(false)? else {
            return Ok(Vec::new());
        };
        let mut ids = Vec::new();
        for entry in fs::read_dir(threads)? {
            let entry = entry?;
            if entry.file_type()?.is_dir() {
                if let Some(name) = entry.file_name().to_str() {
                    ids.push(name.to_string());
                }
            }
        }
        Ok(ids)
    }

    fn segments(&self, thread: &str) -> Result<Vec<SegmentInfo>, ThreadLogError> {
        let thread = validate_thread(thread)?;
        let Some(log_dir) = self.log_dir(&thread, false)? else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        for entry in fs::read_dir(log_dir)? {
            let entry = entry?;
            let meta = fs::symlink_metadata(entry.path())?;
            if !meta.file_type().is_file() {
                continue;
            }
            if let Some(first_seq) = entry.file_name().to_str().and_then(parse_segment_name) {
                out.push(SegmentInfo {
                    first_seq,
                    len: meta.len(),
                });
            }
        }
        out.sort_by_key(|s| s.first_seq);
        Ok(out)
    }

    fn read_segment(&self, thread: &str, first_seq: u64) -> Result<Vec<u8>, ThreadLogError> {
        let thread = validate_thread(thread)?;
        let log_dir = self
            .log_dir(&thread, false)?
            .ok_or_else(|| ThreadLogError::NotFound(format!("thread {thread} has no log")))?;
        read_file_bounded(&log_dir.join(segment_file_name(first_seq)))
    }

    fn thread_meta(&self, thread: &str) -> Result<Option<ThreadMeta>, ThreadLogError> {
        let thread = validate_thread(thread)?;
        let Some(dir) = self.thread_dir(&thread, false)? else {
            return Ok(None);
        };
        let path = dir.join(THREAD_JSON);
        match read_file_bounded(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map(Some)
                .map_err(|e| ThreadLogError::Corrupt(format!("invalid thread.json: {e}"))),
            Err(ThreadLogError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl ThreadStore for TreeThreadStore {
    fn append(&self, thread: &str, batch: Vec<NewRecord>) -> Result<AppendReceipt, ThreadLogError> {
        if batch.is_empty() {
            return Err(ThreadLogError::Corrupt("empty batch".into()));
        }
        let thread = validate_thread(thread)?;
        let slot = self.slot(&thread)?;
        let mut guard = slot
            .lock()
            .map_err(|_| ThreadLogError::Corrupt("thread lock poisoned".into()))?;
        let result = self.append_locked(&thread, &mut guard, batch);
        if let Err(err) = &result {
            // Whatever happened to the file, trust disk, not memory, next time.
            *guard = None;
            if !matches!(err, ThreadLogError::QuotaExceeded { .. }) {
                if let Some((_, usage)) = &self.quota {
                    usage.invalidate();
                }
            }
        }
        result
    }

    fn fork(&self, parent: &str, at_seq: u64, child: &str) -> Result<LogHead, ThreadLogError> {
        let parent = validate_thread(parent)?;
        let child = validate_thread(child)?;
        if parent == child {
            return Err(ThreadLogError::InvalidThread(
                "a thread cannot fork itself".into(),
            ));
        }
        let page = self.read(&parent, 0, usize::MAX)?;
        if let Integrity::BrokenAt { seq, reason } = &page.integrity {
            return Err(ThreadLogError::Corrupt(format!(
                "parent log is broken at {seq}: {reason}"
            )));
        }
        let Some(at) = page.records.iter().find(|r| r.seq == at_seq) else {
            return Err(ThreadLogError::NotFound(format!(
                "parent thread has no record {at_seq}"
            )));
        };
        if !self.segments(&child)?.is_empty() {
            return Err(ThreadLogError::Corrupt(
                "child thread already has a log".into(),
            ));
        }
        let slot = self.slot(&child)?;
        let mut guard = slot
            .lock()
            .map_err(|_| ThreadLogError::Corrupt("thread lock poisoned".into()))?;
        *guard = None;
        let log_dir = self
            .log_dir(&child, true)?
            .ok_or_else(|| ThreadLogError::Corrupt("log directory unavailable".into()))?;
        for segment in self.segments(&parent)? {
            if segment.first_seq > at_seq {
                break;
            }
            let bytes = self.read_segment(&parent, segment.first_seq)?;
            let mut kept = Vec::new();
            for line in complete_lines(&bytes) {
                match parse_line(line) {
                    Ok(record) if record.seq <= at_seq => {
                        kept.extend_from_slice(line);
                        kept.push(b'\n');
                    }
                    _ => break,
                }
            }
            if kept.is_empty() {
                continue;
            }
            let tmp = log_dir.join(format!(".tmp-{}", segment_file_name(segment.first_seq)));
            let mut file = open_append(&tmp, true)?;
            file.write_all(&kept)?;
            file.sync_all()?;
            fs::rename(&tmp, log_dir.join(segment_file_name(segment.first_seq)))?;
        }
        sync_dir(&log_dir);
        if let Some(mut meta) = self.thread_meta(&parent)? {
            meta.thread_id = child.clone();
            meta.parent_thread = Some(parent.clone());
            meta.log = Some(crate::vam_dir::ThreadLogInfo::v1());
            if let Some(dir) = self.thread_dir(&child, false)? {
                let json = serde_json::to_string_pretty(&meta)
                    .map_err(|e| ThreadLogError::Serialization(e.to_string()))?;
                let mut file = open_append(&dir.join(THREAD_JSON), true)?;
                file.write_all(json.as_bytes())?;
                file.sync_all()?;
            }
        }
        let fork = NewRecord {
            run: at.run.clone(),
            lane: MAIN_LANE.to_string(),
            agent: at.agent.clone(),
            parent_lane: None,
            round: 0,
            src: "host".to_string(),
            kind: kind::FORK.to_string(),
            body: serde_json::json!({
                "parent_thread": parent,
                "parent_seq": at_seq,
                "parent_digest": at.digest,
            }),
        };
        let result = self.append_locked(&child, &mut guard, vec![fork]);
        if result.is_err() {
            *guard = None;
        }
        Ok(result?.head)
    }
}

/// Read-only access to the thread logs of a workspace's HEAD manifest.
pub struct HeadThreadReader<'a> {
    manifests: &'a ManifestManager,
    chunks: &'a ChunkStore,
    workspace_id: String,
}

impl<'a> HeadThreadReader<'a> {
    pub fn new(
        manifests: &'a ManifestManager,
        chunks: &'a ChunkStore,
        workspace_id: impl Into<String>,
    ) -> Self {
        Self {
            manifests,
            chunks,
            workspace_id: workspace_id.into(),
        }
    }

    fn files(&self) -> Result<Vec<crate::manifest::ManifestFile>, ThreadLogError> {
        let to_err = |e: crate::error::Drive9Error| ThreadLogError::Corrupt(e.to_string());
        let Some(head) = self
            .manifests
            .read_head(&self.workspace_id)
            .map_err(to_err)?
        else {
            return Ok(Vec::new());
        };
        let manifest = self
            .manifests
            .get_manifest(&self.workspace_id, &head)
            .map_err(to_err)?;
        let prefix = format!("{VAM_DIR}/{THREADS_DIR}/");
        Ok(manifest
            .files
            .into_iter()
            .filter(|f| f.path.starts_with(&prefix))
            .collect())
    }

    /// The legacy `timeline.jsonl` of a thread at HEAD (empty when absent).
    pub fn timeline(
        &self,
        thread: &str,
    ) -> Result<Vec<crate::vam_dir::TurnRecord>, ThreadLogError> {
        let thread = validate_thread(thread)?;
        let path = format!(
            "{VAM_DIR}/{THREADS_DIR}/{thread}/{}",
            crate::vam_dir::TIMELINE_JSONL
        );
        let files = self.files()?;
        let Some(file) = files.iter().find(|f| f.path == path) else {
            return Ok(Vec::new());
        };
        let bytes = self.file_bytes(file)?;
        crate::vam_dir::parse_timeline(&String::from_utf8_lossy(&bytes))
            .map_err(|e| ThreadLogError::Corrupt(format!("invalid timeline: {e}")))
    }

    fn file_bytes(&self, file: &crate::manifest::ManifestFile) -> Result<Vec<u8>, ThreadLogError> {
        if file.size_bytes > MAX_SEGMENT_READ_BYTES {
            return Err(ThreadLogError::Corrupt(format!(
                "{} is larger than {} bytes",
                file.path, MAX_SEGMENT_READ_BYTES
            )));
        }
        let mut data = Vec::new();
        for hash in &file.chunks {
            let chunk = self
                .chunks
                .get_chunk(hash)
                .map_err(|e| ThreadLogError::Corrupt(e.to_string()))?;
            data.extend_from_slice(&chunk);
        }
        Ok(data)
    }
}

impl SegmentSource for HeadThreadReader<'_> {
    fn thread_ids(&self) -> Result<Vec<String>, ThreadLogError> {
        let prefix = format!("{VAM_DIR}/{THREADS_DIR}/");
        let mut ids: Vec<String> = self
            .files()?
            .iter()
            .filter_map(|f| f.path.strip_prefix(&prefix))
            .filter_map(|rest| rest.split('/').next().map(str::to_string))
            .collect();
        ids.sort();
        ids.dedup();
        Ok(ids)
    }

    fn segments(&self, thread: &str) -> Result<Vec<SegmentInfo>, ThreadLogError> {
        let thread = validate_thread(thread)?;
        let prefix = format!("{VAM_DIR}/{THREADS_DIR}/{thread}/{LOG_DIR}/");
        let mut out: Vec<SegmentInfo> = self
            .files()?
            .iter()
            .filter_map(|f| {
                let name = f.path.strip_prefix(&prefix)?;
                let first_seq = parse_segment_name(name)?;
                Some(SegmentInfo {
                    first_seq,
                    len: f.size_bytes,
                })
            })
            .collect();
        out.sort_by_key(|s| s.first_seq);
        Ok(out)
    }

    fn read_segment(&self, thread: &str, first_seq: u64) -> Result<Vec<u8>, ThreadLogError> {
        let thread = validate_thread(thread)?;
        let path = format!(
            "{VAM_DIR}/{THREADS_DIR}/{thread}/{LOG_DIR}/{}",
            segment_file_name(first_seq)
        );
        let files = self.files()?;
        let file = files
            .iter()
            .find(|f| f.path == path)
            .ok_or_else(|| ThreadLogError::NotFound(path.clone()))?;
        self.file_bytes(file)
    }

    fn thread_meta(&self, thread: &str) -> Result<Option<ThreadMeta>, ThreadLogError> {
        let thread = validate_thread(thread)?;
        let path = format!("{VAM_DIR}/{THREADS_DIR}/{thread}/{THREAD_JSON}");
        let files = self.files()?;
        let Some(file) = files.iter().find(|f| f.path == path) else {
            return Ok(None);
        };
        let bytes = self.file_bytes(file)?;
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| ThreadLogError::Corrupt(format!("invalid thread.json: {e}")))
    }
}
