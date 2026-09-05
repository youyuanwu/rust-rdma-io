//! Bounded engine work-class and session-deadline scheduling.
//!
//! Each work class can occupy its queue at most once. A class that remains
//! ready is appended at the tail.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use tokio::time::Instant;

use super::progress::OwnerClass;

const WORK_CLASS_COUNT: usize = 4;
const OWNER_CLASS_COUNT: usize = 3;

/// Deduplicated fair rotation over progress owners.
#[allow(
    dead_code,
    reason = "introduced before driver migration in later phases"
)]
pub(super) struct OwnerScheduler {
    classes: VecDeque<OwnerClass>,
    queued: [bool; OWNER_CLASS_COUNT],
}

#[allow(
    dead_code,
    reason = "introduced before driver migration in later phases"
)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkClass {
    Terminal,
    Cm,
    Io,
    SessionReclamation,
}

impl WorkClass {
    pub(super) const fn index(self) -> usize {
        match self {
            Self::Terminal => 0,
            Self::Cm => 1,
            Self::Io => 2,
            Self::SessionReclamation => 3,
        }
    }
}

pub(super) struct WorkScheduler {
    classes: VecDeque<WorkClass>,
    class_queued: [bool; WORK_CLASS_COUNT],
    deadlines: DeadlineQueue,
}

impl WorkScheduler {
    pub(super) fn new() -> Self {
        Self {
            classes: VecDeque::with_capacity(WORK_CLASS_COUNT),
            class_queued: [false; WORK_CLASS_COUNT],
            deadlines: DeadlineQueue::default(),
        }
    }

    pub(super) fn mark_class_ready(&mut self, class: WorkClass) {
        let queued = &mut self.class_queued[class.index()];
        if !*queued {
            *queued = true;
            self.classes.push_back(class);
        }
    }

    pub(super) fn next_class(&mut self) -> Option<WorkClass> {
        let class = self.classes.pop_front()?;
        self.class_queued[class.index()] = false;
        Some(class)
    }

    pub(super) fn ready_class_count(&self) -> usize {
        self.classes.len()
    }

    pub(super) fn deadlines(&mut self) -> &mut DeadlineQueue {
        &mut self.deadlines
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.next()
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
    fn work_classes_rotate_without_starvation() {
        let mut scheduler = WorkScheduler::new();
        for class in [
            WorkClass::Terminal,
            WorkClass::Cm,
            WorkClass::Io,
            WorkClass::SessionReclamation,
        ] {
            scheduler.mark_class_ready(class);
        }

        let first_round: Vec<_> = (0..WORK_CLASS_COUNT)
            .map(|_| {
                let class = scheduler.next_class().unwrap();
                scheduler.mark_class_ready(class);
                class
            })
            .collect();
        assert_eq!(
            first_round,
            [
                WorkClass::Terminal,
                WorkClass::Cm,
                WorkClass::Io,
                WorkClass::SessionReclamation,
            ]
        );
        assert_eq!(
            scheduler.next_class(),
            Some(WorkClass::Terminal),
            "the first class rotates to the tail"
        );
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
