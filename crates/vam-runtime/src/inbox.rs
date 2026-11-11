//! Steers at turn boundaries (AR-3.6, design V.3).
//!
//! The guest pulls the host's inbox at the top of every round and before it
//! accepts a final answer. Messages that came back are applied as ONE `user`
//! message, in seq order, and the loop goes on. The host replies in bounded
//! batches (a frame is small), so one pull cycle asks again after the last seq
//! it holds and stops on an empty batch. The last applied seq rides in the
//! suspend tail, so a cold resume never applies a message twice.
//!
//! A pull FETCHES after a cursor and ACKNOWLEDGES through `ack_seq`, the last seq
//! already applied to the conversation. Inside one cycle the acknowledgement does
//! not move, so a crash between batches loses nothing: the host still holds every
//! message the model has not seen. The acknowledgement moves only after the user
//! message is appended; the next boundary's pull, the peek before a final answer,
//! the suspend tail, or [`InboxCursor::finish`] carries it.

use std::future::Future;

use vam_spec::v1::ExecutionLease as Lease;

use crate::pep::PullInboxOutcome;
use crate::RuntimeError;

/// Most pulls in one cycle. The host holds at most 20 pending messages and
/// answers at least one per pull, so a well-behaved host needs 21 at most; the
/// cap only bounds a host that misbehaves.
pub const MAX_PULLS_PER_CYCLE: usize = 64;

/// Consecutive failed cycles after which the guest stops asking.
const MAX_FAILED_CYCLES: u8 = 3;

/// What one pull cycle collected.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Cycle {
    /// Messages not seen before, strictly increasing by seq.
    pub messages: Vec<(i64, String)>,
    /// The host has no inbox: stop asking for this task.
    pub unavailable: bool,
    /// A pull failed mid-sequence; `messages` holds what arrived before it.
    pub failed: bool,
    /// The host answered at least one pull, so it has seen the acknowledgement.
    pub reached: bool,
}

/// Pull until the host answers an empty batch. `pull(after_seq, ack_seq)` asks once;
/// `ack_seq` stays what the caller passes for the whole cycle.
///
/// Terminates on an empty batch, a batch with nothing new (a host that repeats
/// itself), an error, "unavailable", or [`MAX_PULLS_PER_CYCLE`] pulls. Whatever
/// arrived before an error is returned: the host acknowledges a batch when the
/// next pull names its last seq, so dropping it would lose the message.
pub async fn pull_cycle<F, Fut>(after_seq: i64, ack_seq: i64, mut pull: F) -> Cycle
where
    F: FnMut(i64, i64) -> Fut,
    Fut: Future<Output = Result<PullInboxOutcome, RuntimeError>>,
{
    let mut cycle = Cycle::default();
    let mut cursor = after_seq;
    for _ in 0..MAX_PULLS_PER_CYCLE {
        match pull(cursor, ack_seq).await {
            Ok(PullInboxOutcome::Unavailable) => {
                cycle.unavailable = true;
                break;
            }
            Ok(PullInboxOutcome::Messages(batch)) => {
                cycle.reached = true;
                let before = cycle.messages.len();
                for (seq, text) in batch {
                    if seq > cursor {
                        cursor = seq;
                        cycle.messages.push((seq, text));
                    }
                }
                if cycle.messages.len() == before {
                    break;
                }
            }
            Err(err) => {
                tracing::warn!("inbox pull failed: {err}");
                cycle.failed = true;
                break;
            }
        }
    }
    cycle
}

/// The one user message for a set of steers: texts verbatim, a blank line between.
pub fn steer_text(messages: &[(i64, String)]) -> String {
    messages
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The guest's view of its inbox for one task.
#[derive(Debug, Default)]
pub struct InboxCursor {
    /// Last seq applied to the conversation.
    pub last_seq: i64,
    /// Highest seq the host has been told about.
    acked: i64,
    disabled: bool,
    failed_cycles: u8,
}

impl InboxCursor {
    /// Steers to apply at a round boundary, or `None` (nothing pending).
    /// Advances `last_seq` to the last message returned.
    pub async fn take(&mut self, lease: &Lease) -> Option<String> {
        if self.disabled {
            return None;
        }
        let ack = self.last_seq;
        let cycle = pull_cycle(self.last_seq, ack, |after, ack| {
            crate::pep::pull_inbox(lease, after, ack)
        })
        .await;
        self.note(&cycle);
        if cycle.reached {
            self.acked = self.acked.max(ack);
        }
        let last = cycle.messages.last().map(|(seq, _)| *seq)?;
        self.last_seq = last;
        Some(steer_text(&cycle.messages))
    }

    /// True when a steer is waiting. Does not advance: the next [`Self::take`]
    /// returns the same messages.
    pub async fn peek(&mut self, lease: &Lease) -> bool {
        if self.disabled {
            return false;
        }
        let ack = self.last_seq;
        let outcome = crate::pep::pull_inbox(lease, self.last_seq, ack).await;
        let cycle = pull_cycle_once(self.last_seq, outcome);
        self.note(&cycle);
        if cycle.reached {
            self.acked = self.acked.max(ack);
        }
        !cycle.messages.is_empty()
    }

    /// A resumed task starts after what the suspended one had applied (and had
    /// acknowledged with its suspend tail).
    pub fn resume_at(&mut self, seq: i64) {
        self.last_seq = seq;
        self.acked = seq;
    }

    /// True when the host has not been told about the last applied steer.
    pub fn unacknowledged(&self) -> bool {
        !self.disabled && self.last_seq > self.acked
    }

    /// Tell the host about every steer applied, on the way out of the task. Every end
    /// of the loop (answer, failure, the iteration cap, a suspend) goes through here,
    /// so a steer the conversation holds is never left pending to run a second time.
    pub async fn finish(&mut self, lease: &Lease) {
        if !self.unacknowledged() {
            return;
        }
        let ack = self.last_seq;
        match crate::pep::pull_inbox(lease, ack, ack).await {
            Ok(_) => self.acked = ack,
            Err(err) => tracing::warn!("inbox acknowledgement at the end failed: {err}"),
        }
    }

    fn note(&mut self, cycle: &Cycle) {
        if cycle.unavailable {
            self.disabled = true;
        } else if cycle.failed && cycle.messages.is_empty() {
            self.failed_cycles += 1;
            if self.failed_cycles >= MAX_FAILED_CYCLES {
                self.disabled = true;
            }
        } else {
            self.failed_cycles = 0;
        }
    }
}

fn pull_cycle_once(after_seq: i64, outcome: Result<PullInboxOutcome, RuntimeError>) -> Cycle {
    match outcome {
        Ok(PullInboxOutcome::Unavailable) => Cycle {
            unavailable: true,
            ..Cycle::default()
        },
        Ok(PullInboxOutcome::Messages(batch)) => Cycle {
            messages: batch.into_iter().filter(|(s, _)| *s > after_seq).collect(),
            reached: true,
            ..Cycle::default()
        },
        Err(_) => Cycle {
            failed: true,
            ..Cycle::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A host with `total` pending messages of `size` bytes, answering batches of at
    /// most `cap` bytes (at least one message), as the real host does.
    fn host<'a>(
        total: i64,
        size: usize,
        cap: usize,
        calls: &'a RefCell<Vec<i64>>,
        fail_at_call: Option<usize>,
    ) -> impl FnMut(i64, i64) -> std::future::Ready<Result<PullInboxOutcome, RuntimeError>> + 'a
    {
        move |after, _ack| {
            calls.borrow_mut().push(after);
            if fail_at_call == Some(calls.borrow().len()) {
                return std::future::ready(Err(RuntimeError::Handler("host went away".into())));
            }
            let mut batch = Vec::new();
            let mut bytes = 0usize;
            for seq in (after + 1)..=total {
                if !batch.is_empty() && bytes + size > cap {
                    break;
                }
                bytes += size;
                let head = format!("{seq}:");
                batch.push((seq, format!("{head}{}", "x".repeat(size - head.len()))));
            }
            std::future::ready(Ok(PullInboxOutcome::Messages(batch)))
        }
    }

    #[tokio::test]
    async fn twenty_full_messages_arrive_in_order_as_one_user_message() {
        let calls = RefCell::new(Vec::new());
        let size = 32 * 1024;
        let cycle = pull_cycle(0, 0, host(20, size, 48 * 1024, &calls, None)).await;
        assert!(!cycle.failed && !cycle.unavailable);
        let seqs: Vec<i64> = cycle.messages.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, (1..=20).collect::<Vec<_>>());
        // 48 KiB fits one 32 KiB message, not two: twenty batches and the empty one.
        assert_eq!(calls.borrow().len(), 21);
        assert_eq!(calls.borrow().last(), Some(&20));
        let text = steer_text(&cycle.messages);
        assert_eq!(text.len(), 20 * size + 19 * 2);
        // Every message starts at its own offset, in seq order.
        for (i, (seq, body)) in cycle.messages.iter().enumerate() {
            let at = i * (size + 2);
            assert!(text[at..].starts_with(&format!("{seq}:")));
            assert_eq!(&text[at..at + size], body.as_str());
        }
    }

    #[tokio::test]
    async fn small_messages_share_a_batch() {
        let calls = RefCell::new(Vec::new());
        let cycle = pull_cycle(0, 0, host(5, 1024, 48 * 1024, &calls, None)).await;
        assert_eq!(cycle.messages.len(), 5);
        assert_eq!(calls.borrow().as_slice(), &[0, 5]);
    }

    #[tokio::test]
    async fn an_error_mid_sequence_ends_the_cycle_and_keeps_what_arrived() {
        let calls = RefCell::new(Vec::new());
        let cycle = pull_cycle(0, 0, host(20, 32 * 1024, 48 * 1024, &calls, Some(4))).await;
        assert!(cycle.failed);
        let seqs: Vec<i64> = cycle.messages.iter().map(|(s, _)| *s).collect();
        assert_eq!(seqs, vec![1, 2, 3]);
        assert_eq!(calls.borrow().len(), 4, "no pull after the error");
    }

    #[tokio::test]
    async fn a_host_that_repeats_itself_cannot_loop_the_guest() {
        let calls = RefCell::new(0usize);
        let cycle = pull_cycle(0, 0, |_, _| {
            *calls.borrow_mut() += 1;
            std::future::ready(Ok(PullInboxOutcome::Messages(vec![(1, "again".into())])))
        })
        .await;
        assert_eq!(cycle.messages.len(), 1);
        assert_eq!(*calls.borrow(), 2);
    }

    #[tokio::test]
    async fn a_host_that_never_runs_dry_is_stopped_at_the_cap() {
        let calls = RefCell::new(0i64);
        let cycle = pull_cycle(0, 0, |_, _| {
            *calls.borrow_mut() += 1;
            let seq = *calls.borrow();
            std::future::ready(Ok(PullInboxOutcome::Messages(vec![(seq, "m".into())])))
        })
        .await;
        assert_eq!(cycle.messages.len(), MAX_PULLS_PER_CYCLE);
    }

    #[tokio::test]
    async fn seqs_at_or_below_the_cursor_are_never_applied() {
        let cycle = pull_cycle(5, 5, |after, _| {
            std::future::ready(Ok(PullInboxOutcome::Messages(if after == 5 {
                vec![(3, "old".into()), (5, "old".into()), (6, "new".into())]
            } else {
                vec![]
            })))
        })
        .await;
        assert_eq!(cycle.messages, vec![(6, "new".to_string())]);
    }

    #[tokio::test]
    async fn unavailable_stops_the_cycle_and_the_cursor_stops_asking() {
        let cycle = pull_cycle(0, 0, |_, _| {
            std::future::ready(Ok(PullInboxOutcome::Unavailable))
        })
        .await;
        assert!(cycle.unavailable && cycle.messages.is_empty());
        let mut cursor = InboxCursor::default();
        cursor.note(&cycle);
        assert!(cursor.disabled);
    }

    #[test]
    fn three_failed_cycles_switch_pulling_off() {
        let mut cursor = InboxCursor::default();
        let failed = Cycle {
            failed: true,
            ..Cycle::default()
        };
        cursor.note(&failed);
        cursor.note(&failed);
        assert!(!cursor.disabled);
        cursor.note(&failed);
        assert!(cursor.disabled);
    }

    #[test]
    fn steers_join_verbatim_with_a_blank_line() {
        let text = steer_text(&[(1, "one".into()), (2, " two\n".into())]);
        assert_eq!(text, "one\n\n two\n");
    }
}
