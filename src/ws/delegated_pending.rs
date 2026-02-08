use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::time::Instant;

use super::delegated::WsConfirmMode;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingInsertOutcome {
    /// Entry was created; caller should initiate the outbound send.
    Inserted,
    /// Existing in-flight entry matched and the waiter was attached (if provided).
    Joined,
    /// Existing in-flight entry matched, and the joiner requested `Sent` mode after the send was
    /// already observed as successful. The caller should complete the waiter immediately.
    JoinedAlreadySent,
    /// Existing in-flight entry had the same request id but a different fingerprint.
    PayloadMismatch,
    /// The table is at capacity and this request id was not already present.
    TooManyPending,
}

#[derive(Debug)]
pub struct PendingExpired<W> {
    pub request_id: u64,
    pub waiters: Vec<W>,
    pub sent_ok: bool,
}

#[derive(Debug)]
struct PendingEntry<W> {
    fingerprint: u64,
    deadline: Instant,
    sent_ok: bool,
    sent_waiters: Waiters<W>,
    confirmed_waiters: Waiters<W>,
}

#[derive(Debug)]
enum Waiters<W> {
    None,
    One(W),
    Many(Vec<W>),
}

impl<W> Waiters<W> {
    fn push(&mut self, waiter: W) {
        match self {
            Waiters::None => *self = Waiters::One(waiter),
            Waiters::One(_) => {
                let old = std::mem::replace(self, Waiters::None);
                match old {
                    Waiters::One(first) => *self = Waiters::Many(vec![first, waiter]),
                    _ => unreachable!("Waiters::One replaced with non-One"),
                }
            }
            Waiters::Many(v) => v.push(waiter),
        }
    }

    fn take(&mut self) -> Vec<W> {
        match std::mem::replace(self, Waiters::None) {
            Waiters::None => Vec::new(),
            Waiters::One(w) => vec![w],
            Waiters::Many(v) => v,
        }
    }

    fn is_empty(&self) -> bool {
        matches!(self, Waiters::None)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct DeadlineItem {
    deadline: Instant,
    request_id: u64,
}

#[derive(Debug)]
pub struct PendingTable<W> {
    max_pending: usize,
    entries: HashMap<u64, PendingEntry<W>>,
    deadlines: BinaryHeap<Reverse<DeadlineItem>>,
}

impl<W> PendingTable<W> {
    pub fn new(max_pending: usize) -> Self {
        Self {
            max_pending,
            entries: HashMap::new(),
            deadlines: BinaryHeap::new(),
        }
    }

    #[inline]
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn insert_or_join(
        &mut self,
        request_id: u64,
        fingerprint: u64,
        deadline: Instant,
        confirm_mode: WsConfirmMode,
        waiter: Option<W>,
    ) -> (PendingInsertOutcome, Option<W>) {
        if let Some(entry) = self.entries.get_mut(&request_id) {
            if entry.fingerprint != fingerprint {
                return (PendingInsertOutcome::PayloadMismatch, waiter);
            }

            if let Some(waiter) = waiter {
                match confirm_mode {
                    WsConfirmMode::Sent => {
                        if entry.sent_ok {
                            // Do not store; complete immediately.
                            // Note: we still honor tighter deadlines below to help GC.
                            // (deadline tightening affects confirmed waiters, if any).
                            if deadline < entry.deadline {
                                entry.deadline = deadline;
                                self.deadlines.push(Reverse(DeadlineItem {
                                    deadline,
                                    request_id,
                                }));
                            }
                            return (PendingInsertOutcome::JoinedAlreadySent, Some(waiter));
                        }
                        entry.sent_waiters.push(waiter);
                    }
                    WsConfirmMode::Confirmed => entry.confirmed_waiters.push(waiter),
                }
            }

            // If a joiner supplies an earlier deadline, honor it.
            if deadline < entry.deadline {
                entry.deadline = deadline;
                self.deadlines.push(Reverse(DeadlineItem {
                    deadline,
                    request_id,
                }));
            }

            return (PendingInsertOutcome::Joined, None);
        }

        if self.entries.len() >= self.max_pending {
            return (PendingInsertOutcome::TooManyPending, waiter);
        }

        let mut sent_waiters = Waiters::None;
        let mut confirmed_waiters = Waiters::None;
        if let Some(waiter) = waiter {
            match confirm_mode {
                WsConfirmMode::Sent => sent_waiters.push(waiter),
                WsConfirmMode::Confirmed => confirmed_waiters.push(waiter),
            }
        }

        self.entries.insert(
            request_id,
            PendingEntry {
                fingerprint,
                deadline,
                sent_ok: false,
                sent_waiters,
                confirmed_waiters,
            },
        );
        self.deadlines.push(Reverse(DeadlineItem {
            deadline,
            request_id,
        }));
        (PendingInsertOutcome::Inserted, None)
    }

    pub fn complete_not_delivered(&mut self, request_id: u64) -> Option<Vec<W>> {
        self.entries.remove(&request_id).map(|mut entry| {
            let mut out = entry.sent_waiters.take();
            out.extend(entry.confirmed_waiters.take());
            out
        })
    }

    /// Mark a delegated request as successfully sent and return any `Sent` waiters to complete now.
    ///
    /// If there are no `Confirmed` waiters, the entry is removed.
    pub fn mark_sent_ok(&mut self, request_id: u64) -> Option<Vec<W>> {
        let mut entry = self.entries.remove(&request_id)?;
        entry.sent_ok = true;
        let sent_waiters = entry.sent_waiters.take();

        if entry.confirmed_waiters.is_empty() {
            // No one is waiting for an inbound confirmation.
            return Some(sent_waiters);
        }

        // Keep the entry for confirmation/timeout.
        self.entries.insert(request_id, entry);
        Some(sent_waiters)
    }

    pub fn complete_confirmed(&mut self, request_id: u64) -> Option<Vec<W>> {
        let entry = self.entries.get(&request_id)?;
        if entry.confirmed_waiters.is_empty() {
            // No confirmed waiters: do not complete on match (Sent-mode requests should not
            // "re-confirm").
            return None;
        }

        let mut entry = self.entries.remove(&request_id)?;
        let mut out = entry.sent_waiters.take();
        out.extend(entry.confirmed_waiters.take());
        Some(out)
    }

    pub fn complete_rejected(&mut self, request_id: u64) -> Option<Vec<W>> {
        self.complete_confirmed(request_id)
    }

    pub fn next_deadline(&mut self) -> Option<Instant> {
        while let Some(Reverse(item)) = self.deadlines.peek().copied() {
            match self.entries.get(&item.request_id) {
                Some(entry) if entry.deadline == item.deadline => return Some(item.deadline),
                _ => {
                    // Stale heap item (entry removed or deadline updated).
                    let _ = self.deadlines.pop();
                }
            }
        }
        None
    }

    pub fn expire_due(&mut self, now: Instant) -> Vec<PendingExpired<W>> {
        let mut out = Vec::new();
        while let Some(Reverse(item)) = self.deadlines.peek().copied() {
            if item.deadline > now {
                break;
            }
            let _ = self.deadlines.pop();

            let Some(entry) = self.entries.get(&item.request_id) else {
                continue;
            };
            if entry.deadline != item.deadline {
                continue;
            }

            let mut entry = self
                .entries
                .remove(&item.request_id)
                .expect("entry exists (checked above)");
            out.push(PendingExpired {
                request_id: item.request_id,
                waiters: {
                    let mut w = entry.sent_waiters.take();
                    w.extend(entry.confirmed_waiters.take());
                    w
                },
                sent_ok: entry.sent_ok,
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn dedup_joins_waiters_and_does_not_require_second_send() {
        let mut table = PendingTable::<u32>::new(10);
        let d = Instant::now() + Duration::from_secs(5);

        assert_eq!(
            table.insert_or_join(1, 111, d, WsConfirmMode::Sent, Some(10)),
            (PendingInsertOutcome::Inserted, None)
        );
        assert_eq!(
            table.insert_or_join(1, 111, d, WsConfirmMode::Sent, Some(11)),
            (PendingInsertOutcome::Joined, None)
        );

        let waiters = table.mark_sent_ok(1).expect("entry exists");
        assert_eq!(waiters, vec![10, 11]);
        assert!(table.is_empty());
    }

    #[test]
    fn mismatch_is_rejected_without_mutating_existing_entry() {
        let mut table = PendingTable::<u32>::new(10);
        let d = Instant::now() + Duration::from_secs(5);

        assert_eq!(
            table.insert_or_join(7, 1, d, WsConfirmMode::Confirmed, Some(1)),
            (PendingInsertOutcome::Inserted, None)
        );
        assert_eq!(
            table.insert_or_join(7, 2, d, WsConfirmMode::Confirmed, Some(2)),
            (PendingInsertOutcome::PayloadMismatch, Some(2))
        );

        // Ensure original entry is still present and can be expired.
        assert_eq!(table.len(), 1);
        assert!(table.mark_sent_ok(7).is_some());
        let expired = table.expire_due(d + Duration::from_secs(1));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].request_id, 7);
        assert_eq!(expired[0].waiters, vec![1]);
        assert!(table.is_empty());
    }

    #[test]
    fn expiry_completes_all_waiters_and_cleans_table() {
        let mut table = PendingTable::<u32>::new(10);
        let d = Instant::now() + Duration::from_millis(5);

        assert_eq!(
            table.insert_or_join(42, 9, d, WsConfirmMode::Confirmed, Some(1)),
            (PendingInsertOutcome::Inserted, None)
        );
        assert_eq!(
            table.insert_or_join(42, 9, d, WsConfirmMode::Confirmed, Some(2)),
            (PendingInsertOutcome::Joined, None)
        );
        let waiters = table.mark_sent_ok(42).expect("entry exists");
        assert!(waiters.is_empty());

        let expired = table.expire_due(d + Duration::from_secs(1));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].request_id, 42);
        assert_eq!(expired[0].waiters, vec![1, 2]);
        assert!(expired[0].sent_ok);
        assert!(table.is_empty());
        assert!(table.next_deadline().is_none());
    }

    #[test]
    fn cap_rejects_new_ids_but_allows_joining_existing_ones() {
        let mut table = PendingTable::<u32>::new(1);
        let d = Instant::now() + Duration::from_secs(5);

        assert_eq!(
            table.insert_or_join(1, 1, d, WsConfirmMode::Confirmed, Some(10)),
            (PendingInsertOutcome::Inserted, None)
        );
        assert_eq!(
            table.insert_or_join(2, 1, d, WsConfirmMode::Confirmed, Some(20)),
            (PendingInsertOutcome::TooManyPending, Some(20))
        );
        assert_eq!(
            table.insert_or_join(1, 1, d, WsConfirmMode::Confirmed, Some(11)),
            (PendingInsertOutcome::Joined, None)
        );

        let expired = table.expire_due(d + Duration::from_secs(1));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].waiters, vec![10, 11]);
    }

    #[test]
    fn earlier_deadline_from_joiner_is_honored_and_stale_heap_items_are_ignored() {
        let mut table = PendingTable::<u32>::new(10);
        let d1 = Instant::now() + Duration::from_secs(10);
        let d0 = Instant::now() + Duration::from_secs(1);

        assert_eq!(
            table.insert_or_join(1, 1, d1, WsConfirmMode::Confirmed, Some(1)),
            (PendingInsertOutcome::Inserted, None)
        );
        assert_eq!(
            table.insert_or_join(1, 1, d0, WsConfirmMode::Confirmed, Some(2)),
            (PendingInsertOutcome::Joined, None)
        );

        assert_eq!(table.next_deadline().unwrap(), d0);
        let expired = table.expire_due(d0 + Duration::from_millis(1));
        assert_eq!(expired.len(), 1);
        assert_eq!(expired[0].waiters, vec![1, 2]);

        // The stale d1 heap item should not surface as a deadline after removal.
        assert!(table.next_deadline().is_none());
    }

    #[test]
    fn confirmation_completion_only_applies_to_confirmed_mode() {
        let mut table = PendingTable::<u32>::new(10);
        let d = Instant::now() + Duration::from_secs(5);

        assert_eq!(
            table.insert_or_join(1, 1, d, WsConfirmMode::Sent, Some(10)),
            (PendingInsertOutcome::Inserted, None)
        );
        assert!(table.complete_confirmed(1).is_none());
        let waiters = table.mark_sent_ok(1).expect("entry exists");
        assert_eq!(waiters, vec![10]);

        assert_eq!(
            table.insert_or_join(2, 1, d, WsConfirmMode::Confirmed, Some(20)),
            (PendingInsertOutcome::Inserted, None)
        );
        let waiters = table
            .complete_confirmed(2)
            .expect("confirmed should complete");
        assert_eq!(waiters, vec![20]);
        assert!(table.is_empty());
    }

    #[test]
    fn sent_joiner_after_send_ok_is_completed_immediately() {
        let mut table = PendingTable::<u32>::new(10);
        let d = Instant::now() + Duration::from_secs(5);

        assert_eq!(
            table.insert_or_join(9, 1, d, WsConfirmMode::Confirmed, Some(1)),
            (PendingInsertOutcome::Inserted, None)
        );
        assert!(table.mark_sent_ok(9).is_some());

        let (outcome, waiter) = table.insert_or_join(9, 1, d, WsConfirmMode::Sent, Some(2));
        assert_eq!(outcome, PendingInsertOutcome::JoinedAlreadySent);
        assert_eq!(waiter, Some(2));
    }
}
