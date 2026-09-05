//! Bounded engine work-class and session-deadline scheduling.
//!
//! Each work class can occupy its queue at most once. A class that remains
//! ready is appended at the tail.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use tokio::time::Instant;

use super::progress::OwnerClass;

const OWNER_CLASS_COUNT: usize = 3;

/// Deduplicated fair rotation over progress owners.
pub(super) struct OwnerScheduler {
    classes: VecDeque<OwnerClass>,
    queued: [bool; OWNER_CLASS_COUNT],
}

impl OwnerScheduler {
    pub(super) fn new() -> Self {
        Self {
            classes: VecDeque::with_capacity(OWNER_CLASS_COUNT),
            queued: [false; OWNER_CLASS_COUNT],
        }
    }

    pub(super) fn mark_ready(&mut self, class: OwnerClass) {
        let queued = &mut self.queued[class.index()];
        if !*queued {
            *queued = true;
            self.classes.push_back(class);
        }
    }

    pub(super) fn next(&mut self) -> Option<OwnerClass> {
        let class = self.classes.pop_front()?;
        self.queued[class.index()] = false;
        Some(class)
    }

    pub(super) fn ready_count(&self) -> usize {
        self.classes.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum DeadlineKind {
    ConnectionDrain,
    EngineShutdown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Deadline {
    pub(super) at: Instant,
    pub(super) kind: DeadlineKind,
    pub(super) token: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DeadlineRequest {
    pub(super) at: Instant,
    pub(super) kind: DeadlineKind,
    pub(super) token: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DeadlineEntry {
    at: Instant,
    sequence: u64,
    kind: DeadlineKind,
    token: u64,
}

impl Ord for DeadlineEntry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

impl PartialOrd for DeadlineEntry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Default)]
pub(super) struct DeadlineQueue {
    entries: BinaryHeap<Reverse<DeadlineEntry>>,
    next_sequence: u64,
}

impl DeadlineQueue {
    pub(super) fn push(&mut self, at: Instant, kind: DeadlineKind, token: u64) -> bool {
        let sequence = self.next_sequence;
        let Some(next_sequence) = self.next_sequence.checked_add(1) else {
            return false;
        };
        self.next_sequence = next_sequence;
        self.entries.push(Reverse(DeadlineEntry {
            at,
            sequence,
            kind,
            token,
        }));
        true
    }

    pub(super) fn pop_due(&mut self, now: Instant, budget: usize) -> Vec<Deadline> {
        let mut due = Vec::with_capacity(budget.min(self.entries.len()));
        while due.len() < budget {
            let Some(Reverse(entry)) = self.entries.peek().copied() else {
                break;
            };
            if entry.at > now {
                break;
            }
            self.entries.pop();
            due.push(Deadline {
                at: entry.at,
                kind: entry.kind,
                token: entry.token,
            });
        }
        due
    }

    pub(super) fn next(&self) -> Option<Instant> {
        self.entries.peek().map(|entry| entry.0.at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn owner_classes_deduplicate_and_rotate() {
        let mut scheduler = OwnerScheduler::new();
        scheduler.mark_ready(OwnerClass::Io);
        scheduler.mark_ready(OwnerClass::Session);
        scheduler.mark_ready(OwnerClass::Io);
        scheduler.mark_ready(OwnerClass::Terminal);

        assert_eq!(scheduler.ready_count(), OWNER_CLASS_COUNT);
        assert_eq!(scheduler.next(), Some(OwnerClass::Io));
        scheduler.mark_ready(OwnerClass::Io);
        assert_eq!(scheduler.next(), Some(OwnerClass::Session));
        assert_eq!(scheduler.next(), Some(OwnerClass::Terminal));
        assert_eq!(scheduler.next(), Some(OwnerClass::Io));
        assert_eq!(scheduler.next(), None);
    }

    #[test]
    fn ready_at_entry_bounds_one_turn_per_owner() {
        let mut scheduler = OwnerScheduler::new();
        for class in [OwnerClass::Io, OwnerClass::Session, OwnerClass::Terminal] {
            scheduler.mark_ready(class);
        }
        let pass_budget = scheduler.ready_count();
        let mut serviced = Vec::new();
        for _ in 0..pass_budget {
            let class = scheduler.next().unwrap();
            serviced.push(class);
            scheduler.mark_ready(class);
        }

        assert_eq!(
            serviced,
            [OwnerClass::Io, OwnerClass::Session, OwnerClass::Terminal]
        );
        assert_eq!(scheduler.ready_count(), OWNER_CLASS_COUNT);
        assert_eq!(scheduler.next(), Some(OwnerClass::Io));
    }

    #[test]
    fn deadlines_are_ordered_and_budgeted() {
        let now = Instant::now();
        let mut deadlines = DeadlineQueue::default();
        assert!(deadlines.push(
            now + Duration::from_secs(2),
            DeadlineKind::EngineShutdown,
            2,
        ));
        assert!(deadlines.push(now, DeadlineKind::ConnectionDrain, 0));
        assert!(deadlines.push(
            now + Duration::from_secs(1),
            DeadlineKind::ConnectionDrain,
            1,
        ));

        let due = deadlines.pop_due(now + Duration::from_secs(2), 2);
        assert_eq!(due.len(), 2);
        assert_eq!(due[0].token, 0);
        assert_eq!(due[1].token, 1);
        assert_eq!(deadlines.next(), Some(now + Duration::from_secs(2)));
    }
}
