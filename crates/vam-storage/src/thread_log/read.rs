//! Read path of the thread log: chain walking, the reader trait, context assembly.

use super::*;
use std::collections::HashMap;

/// The complete (newline-terminated) lines of a segment's bytes. A torn tail
/// (no newline) is not included.
pub(super) fn complete_lines(bytes: &[u8]) -> Vec<&[u8]> {
    let end = complete_len(bytes);
    if end == 0 {
        return Vec::new();
    }
    bytes[..end - 1].split(|b| *b == b'\n').collect()
}

/// Length of the newline-terminated prefix of `bytes`.
pub(super) fn complete_len(bytes: &[u8]) -> usize {
    bytes
        .iter()
        .rposition(|b| *b == b'\n')
        .map(|i| i + 1)
        .unwrap_or(0)
}

#[derive(Default)]
struct Walker {
    expect_seq: Option<u64>,
    prev: Option<String>,
    /// Records whose body was removed and that no `redact` has named yet.
    pending_redact: BTreeMap<String, u64>,
}

fn broken(seq: u64, reason: impl Into<String>) -> Integrity {
    Integrity::BrokenAt {
        seq,
        reason: reason.into(),
    }
}

/// Walks one segment. `Ok(Some(_))` is an integrity break; `Err` is a refusal.
fn walk_segment(
    bytes: &[u8],
    first_seq: u64,
    walker: &mut Walker,
    after: u64,
    limit: usize,
    out: &mut Vec<Record>,
    limited: &mut bool,
) -> Result<Option<Integrity>, ThreadLogError> {
    for (index, line) in complete_lines(bytes).into_iter().enumerate() {
        let expected = walker
            .expect_seq
            .unwrap_or(if index == 0 { first_seq } else { 0 });
        if line.len() > MAX_RECORD_BYTES {
            return Ok(Some(broken(expected, "record_too_large")));
        }
        let record = match parse_line(line) {
            Ok(record) => record,
            Err(LineFail::Version(version)) => {
                return Err(ThreadLogError::UnsupportedVersion {
                    seq: expected,
                    version,
                })
            }
            Err(LineFail::Broken(reason)) => return Ok(Some(broken(expected, reason))),
        };
        if index == 0 && record.seq != first_seq {
            return Ok(Some(broken(
                record.seq,
                format!("segment named {first_seq} starts at seq {}", record.seq),
            )));
        }
        if let Some(want) = walker.expect_seq {
            if record.seq != want {
                return Ok(Some(broken(
                    want,
                    format!("expected seq {want}, found {}", record.seq),
                )));
            }
        }
        if let Some(prev) = &walker.prev {
            if record.prev != *prev {
                return Ok(Some(broken(record.seq, "prev does not match")));
            }
        }
        match &record.body {
            Some(body) => {
                if sha256_hex(canonical_json(body).as_bytes()) != record.body_sha256 {
                    return Ok(Some(broken(record.seq, "body_sha256 mismatch")));
                }
            }
            None => {
                walker.pending_redact.insert(record.id.clone(), record.seq);
            }
        }
        if record.kind == kind::REDACT {
            if let Some(target) = record
                .body
                .as_ref()
                .and_then(|b| b.get("target"))
                .and_then(Value::as_str)
            {
                walker.pending_redact.remove(target);
            }
        }
        walker.expect_seq = Some(record.seq + 1);
        walker.prev = Some(record.digest.clone());
        if record.seq > after {
            out.push(record);
            if out.len() >= limit {
                *limited = true;
                return Ok(None);
            }
        }
    }
    Ok(None)
}

pub(super) fn read_page<S: SegmentSource + ?Sized>(
    source: &S,
    thread: &str,
    after: u64,
    limit: usize,
) -> Result<ReadPage, ThreadLogError> {
    let thread = validate_thread(thread)?;
    let segments = source.segments(&thread)?;
    let mut out = Vec::new();
    let mut walker = Walker::default();
    let mut limited = false;
    let mut integrity = Integrity::Ok;
    if limit > 0 && !segments.is_empty() {
        let start = segments
            .iter()
            .rposition(|s| s.first_seq <= after.saturating_add(1))
            .unwrap_or(0);
        for segment in &segments[start..] {
            let bytes = source.read_segment(&thread, segment.first_seq)?;
            if let Some(brk) = walk_segment(
                &bytes,
                segment.first_seq,
                &mut walker,
                after,
                limit,
                &mut out,
                &mut limited,
            )? {
                integrity = brk;
                break;
            }
            if limited {
                break;
            }
        }
        if integrity == Integrity::Ok && !limited {
            if let Some((_, seq)) = walker.pending_redact.iter().min_by_key(|(_, s)| **s) {
                integrity = broken(*seq, "body removed without a redact record");
            }
        }
    }
    let next_after = out.last().map(|r| r.seq).unwrap_or(after);
    Ok(ReadPage {
        records: out,
        next_after,
        integrity,
    })
}

pub(super) fn head_of<S: SegmentSource + ?Sized>(
    source: &S,
    thread: &str,
) -> Result<Option<LogHead>, ThreadLogError> {
    let thread = validate_thread(thread)?;
    for segment in source.segments(&thread)?.iter().rev() {
        let bytes = source.read_segment(&thread, segment.first_seq)?;
        if let Some(line) = complete_lines(&bytes).last() {
            return match parse_line(line) {
                Ok(record) => Ok(Some(record.head())),
                Err(LineFail::Version(version)) => Err(ThreadLogError::UnsupportedVersion {
                    seq: segment.first_seq,
                    version,
                }),
                Err(LineFail::Broken(reason)) => Err(ThreadLogError::Corrupt(reason)),
            };
        }
    }
    Ok(None)
}

/// Read side of a thread log.
pub trait ThreadLogReader: Send + Sync {
    fn head(&self, thread: &str) -> Result<Option<LogHead>, ThreadLogError>;
    /// Records with `seq > after`, verified from the first record read.
    /// `after = 0` verifies the chain from genesis.
    fn read(&self, thread: &str, after: u64, limit: usize) -> Result<ReadPage, ThreadLogError>;
    fn list(&self) -> Result<Vec<ThreadSummary>, ThreadLogError>;
}

impl<S: SegmentSource + Send + Sync> ThreadLogReader for S {
    fn head(&self, thread: &str) -> Result<Option<LogHead>, ThreadLogError> {
        head_of(self, thread)
    }

    fn read(&self, thread: &str, after: u64, limit: usize) -> Result<ReadPage, ThreadLogError> {
        read_page(self, thread, after, limit)
    }

    fn list(&self) -> Result<Vec<ThreadSummary>, ThreadLogError> {
        let mut out = Vec::new();
        for id in self.thread_ids()? {
            let Ok(thread) = validate_thread(&id) else {
                continue;
            };
            let segments = self.segments(&thread).unwrap_or_default();
            out.push(ThreadSummary {
                meta: self.thread_meta(&thread).ok().flatten(),
                head: head_of(self, &thread).ok().flatten(),
                log_bytes: segments.iter().map(|s| s.len).sum(),
                thread_id: thread,
            });
        }
        out.sort_by(|a, b| a.thread_id.cmp(&b.thread_id));
        Ok(out)
    }
}

/// Write side: the tree backend is the first; others may follow.
pub trait ThreadStore: ThreadLogReader {
    /// Appends a batch in order and fsyncs once at its end.
    fn append(&self, thread: &str, batch: Vec<NewRecord>) -> Result<AppendReceipt, ThreadLogError>;
    /// A new thread holding `parent`'s records up to `at_seq`, then a `fork` record.
    fn fork(&self, parent: &str, at_seq: u64, child: &str) -> Result<LogHead, ThreadLogError>;
}

/// Verifies a full chain from genesis.
pub fn verify_thread<R: ThreadLogReader + ?Sized>(
    reader: &R,
    thread: &str,
) -> Result<Integrity, ThreadLogError> {
    Ok(reader.read(thread, 0, usize::MAX)?.integrity)
}

/// Verifies seq continuity, `prev` links and body hashes of records in hand.
pub fn verify_records(records: &[Record]) -> Integrity {
    let mut prev: Option<&str> = None;
    let mut next: Option<u64> = None;
    for record in records {
        if let Some(n) = next {
            if record.seq != n {
                return broken(n, "seq is not contiguous");
            }
        }
        if let Some(p) = prev {
            if record.prev != p {
                return broken(record.seq, "prev does not match");
            }
        }
        if let Some(body) = &record.body {
            if sha256_hex(canonical_json(body).as_bytes()) != record.body_sha256 {
                return broken(record.seq, "body_sha256 mismatch");
            }
        }
        prev = Some(&record.digest);
        next = Some(record.seq + 1);
    }
    Integrity::Ok
}

#[derive(Debug, Clone, PartialEq)]
pub struct AssembledContext {
    pub messages: Vec<LogMessage>,
    /// Seq of the record each message came from (a compaction summary: the
    /// compaction record), parallel to `messages`.
    pub seqs: Vec<u64>,
    /// Last record of the records given, whatever its lane.
    pub head: Option<LogHead>,
    /// Serialized size of the messages.
    pub bytes: usize,
    pub system_sha256: Option<String>,
    pub tools_sha256: Option<String>,
    pub gaps: usize,
    /// Distinct runs whose records are in the assembled messages, in order of
    /// first appearance.
    pub runs: Vec<String>,
    /// Seq of the latest `compaction` record the messages start from, if any.
    /// The first message is its summary and its entry carries this seq; the
    /// kept messages after it have lower seqs.
    pub compaction_seq: Option<u64>,
}

/// Reads every record of `thread` (verified from genesis). On an integrity
/// break the records before it are returned with the break.
pub fn read_all_records<R: ThreadLogReader + ?Sized>(
    reader: &R,
    thread: &str,
) -> Result<(Vec<Record>, Integrity), ThreadLogError> {
    let mut records = Vec::new();
    let mut after = 0;
    loop {
        let page = reader.read(thread, after, 500)?;
        let done = page.records.is_empty();
        after = page.next_after;
        records.extend(page.records);
        if page.integrity != Integrity::Ok {
            return Ok((records, page.integrity));
        }
        if done {
            return Ok((records, Integrity::Ok));
        }
    }
}

/// The records of `lane` that survive rollbacks: those strictly between a
/// `rollback.to` and that rollback are excluded. In log order.
pub fn records_without_rollbacks<'a>(records: &'a [Record], lane: &str) -> Vec<&'a Record> {
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let mut included: Vec<&Record> = Vec::new();
    for record in records.iter().rev().filter(|r| r.lane == lane) {
        if ranges
            .iter()
            .any(|(to, rb)| record.seq > *to && record.seq < *rb)
        {
            continue;
        }
        if record.kind == kind::ROLLBACK {
            if let Some(to) = record
                .body
                .as_ref()
                .and_then(|b| b.get("to"))
                .and_then(Value::as_u64)
            {
                ranges.push((to, record.seq));
            }
        }
        included.push(record);
    }
    included.reverse();
    included
}

/// The messages the model sees next on `lane`: the latest `compaction` gives
/// `[summary] + messages from keep_from`, messages after it follow, and records
/// strictly between a `rollback.to` and that rollback are excluded. A pure
/// function of the records.
pub fn assemble_context(
    records: &[Record],
    lane: &str,
) -> Result<AssembledContext, ThreadLogError> {
    let included = records_without_rollbacks(records, lane);

    let mut entries: Vec<(u64, LogMessage)> = Vec::new();
    let mut run_of: HashMap<u64, String> = HashMap::new();
    let mut system_sha256 = None;
    let mut tools_sha256 = None;
    let mut gaps = 0usize;
    let mut compaction_seq: Option<u64> = None;
    for record in included {
        match record.kind.as_str() {
            kind::MESSAGE => {
                if let Some(message) = record.message() {
                    run_of.insert(record.seq, record.run.clone());
                    entries.push((record.seq, message));
                }
            }
            kind::COMPACTION => {
                let body = record.body.as_ref();
                let summary = body
                    .and_then(|b| b.get("summary"))
                    .and_then(|m| serde_json::from_value::<LogMessage>(m.clone()).ok());
                let keep_from = body
                    .and_then(|b| b.get("keep_from"))
                    .and_then(Value::as_u64)
                    .unwrap_or(record.seq);
                if let Some(summary) = summary {
                    run_of.insert(record.seq, record.run.clone());
                    // An earlier summary is never kept: the new one replaces it
                    // (its seq is the earlier compaction's, above `keep_from`).
                    let kept: Vec<(u64, LogMessage)> = entries
                        .drain(..)
                        .filter(|(seq, _)| *seq >= keep_from && Some(*seq) != compaction_seq)
                        .collect();
                    compaction_seq = Some(record.seq);
                    entries.push((record.seq, summary));
                    entries.extend(kept);
                }
            }
            kind::TASK_START | kind::PROMPT_CHANGE => {
                if let Some(body) = &record.body {
                    if let Some(s) = body.get("system_sha256").and_then(Value::as_str) {
                        system_sha256 = Some(s.to_string());
                    }
                    if let Some(s) = body.get("tools_sha256").and_then(Value::as_str) {
                        tools_sha256 = Some(s.to_string());
                    }
                }
            }
            kind::GAP => gaps += 1,
            _ => {}
        }
    }
    let seqs: Vec<u64> = entries.iter().map(|(seq, _)| *seq).collect();
    let mut runs: Vec<String> = Vec::new();
    for seq in &seqs {
        if let Some(run) = run_of.get(seq) {
            if !run.is_empty() && !runs.contains(run) {
                runs.push(run.clone());
            }
        }
    }
    let messages: Vec<LogMessage> = entries.into_iter().map(|(_, m)| m).collect();
    let bytes: usize = messages
        .iter()
        .map(|m| canonical_json(&serde_json::to_value(m).unwrap_or(Value::Null)).len())
        .sum();
    if bytes > MAX_CONTEXT_BYTES {
        return Err(ThreadLogError::ContextTooLarge { bytes });
    }
    Ok(AssembledContext {
        messages,
        seqs,
        head: records.last().map(Record::head),
        bytes,
        system_sha256,
        tools_sha256,
        gaps,
        runs,
        compaction_seq,
    })
}
