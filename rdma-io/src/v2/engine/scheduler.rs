//! Bounded work-class, ready-connection, and deadline scheduling.
//!
//! Each work class and connection can occupy its queue at most once. A class
//! that remains ready is appended at the tail, and a continuously ready
//! connection is likewise requeued only after one configured quantum.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashSet, VecDeque};

use tokio::time::Instant;

const WORK_CLASS_COUNT: usize = 5;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum WorkClass {
    Terminal,
    Cm,
    Cq,
    Reclamation,
    ReadyConnection,
}

impl WorkClass {
    const fn index(self) -> usize {
        match self {
            Self::Terminal => 0,
            Self::Cm => 1,
            Self::Cq => 2,
            Self::Reclamation => 3,
            Self::ReadyConnection => 4,
        }
    }
}

pub(super) struct WorkScheduler {
    classes: VecDeque<WorkClass>,
    class_queued: [bool; WORK_CLASS_COUNT],
    ready_connections: ReadyConnections,
    deadlines: DeadlineQueue,
}

impl WorkScheduler {
    pub(super) fn new() -> Self {
        Self {
            classes: VecDeque::with_capacity(WORK_CLASS_COUNT),
            class_queued: [false; WORK_CLASS_COUNT],
            ready_connections: ReadyConnections::default(),
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

    pub(super) fn enqueue_connection(&mut self, connection: ReadyConnection) {
        if self.ready_connections.enqueue(connection) {
            self.mark_class_ready(WorkClass::ReadyConnection);
        }
    }

    pub(super) fn pop_connection(&mut self) -> Option<ReadyConnection> {
        self.ready_connections.pop()
    }

    pub(super) fn requeue_connection(&mut self, connection: ReadyConnection) {
        self.ready_connections.enqueue(connection);
        self.mark_class_ready(WorkClass::ReadyConnection);
    }

    pub(super) fn ready_connection_count(&self) -> usize {
        self.ready_connections.len()
    }

    pub(super) fn deadlines(&mut self) -> &mut DeadlineQueue {
        &mut self.deadlines
    }

    pub(super) fn next_deadline(&self) -> Option<Instant> {
        self.deadlines.next()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct ReadyConnection {
    pub(super) slot: u32,
    pub(super) generation: u32,
}

#[derive(Default)]
struct ReadyConnections {
    queue: VecDeque<ReadyConnection>,
    queued: HashSet<ReadyConnection>,
}

impl ReadyConnections {
    fn enqueue(&mut self, connection: ReadyConnection) -> bool {
        if !self.queued.insert(connection) {
            return false;
        }
        self.queue.push_back(connection);
        true
    }

    fn pop(&mut self) -> Option<ReadyConnection> {
        let connection = self.queue.pop_front()?;
        self.queued.remove(&connection);
        Some(connection)
    }

    fn len(&self) -> usize {
        self.queue.len()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum DeadlineKind {
    Reclamation,
    ConnectionDrain,
    MessageHello,
    #[allow(
        dead_code,
        reason = "graceful shutdown scheduling is completed in Phase 6"
    )]
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

    fn connection(slot: u32) -> ReadyConnection {
        ReadyConnection {
            slot,
            generation: 1,
        }
    }

    #[test]
    fn work_classes_rotate_without_starvation() {
        let mut scheduler = WorkScheduler::new();
        for class in [
            WorkClass::Terminal,
            WorkClass::Cm,
            WorkClass::Cq,
            WorkClass::Reclamation,
            WorkClass::ReadyConnection,
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
                WorkClass::Cq,
                WorkClass::Reclamation,
                WorkClass::ReadyConnection,
            ]
        );
        assert_eq!(
            scheduler.next_class(),
            Some(WorkClass::Terminal),
            "the first class rotates to the tail"
        );
    }

    #[test]
    fn duplicate_ready_connections_are_suppressed() {
        let mut scheduler = WorkScheduler::new();
        scheduler.enqueue_connection(connection(7));
        scheduler.enqueue_connection(connection(7));
        assert_eq!(scheduler.ready_connection_count(), 1);
        assert_eq!(scheduler.pop_connection(), Some(connection(7)));
        assert_eq!(scheduler.pop_connection(), None);
    }

    #[test]
    fn continuously_ready_connections_receive_one_quantum_per_round() {
        let mut scheduler = WorkScheduler::new();
        for slot in 0..8 {
            scheduler.enqueue_connection(connection(slot));
        }

        let mut first_round = Vec::new();
        for _ in 0..8 {
            let selected = scheduler.pop_connection().unwrap();
            first_round.push(selected.slot);
            scheduler.requeue_connection(selected);
        }
        assert_eq!(first_round, (0..8).collect::<Vec<_>>());
        assert_eq!(scheduler.pop_connection(), Some(connection(0)));
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
        assert!(deadlines.push(now, DeadlineKind::Reclamation, 0));
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
