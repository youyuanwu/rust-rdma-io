//! Owner-neutral deadline scheduling.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use tokio::time::Instant;

struct DeadlineEntry<P> {
    at: Instant,
    sequence: u64,
    payload: P,
}

impl<P> PartialEq for DeadlineEntry<P> {
    fn eq(&self, other: &Self) -> bool {
        self.at == other.at && self.sequence == other.sequence
    }
}

impl<P> Eq for DeadlineEntry<P> {}

impl<P> Ord for DeadlineEntry<P> {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.at
            .cmp(&other.at)
            .then_with(|| self.sequence.cmp(&other.sequence))
    }
}

impl<P> PartialOrd for DeadlineEntry<P> {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DeadlineSequenceExhausted;

pub(super) struct DeadlineQueue<P> {
    entries: BinaryHeap<Reverse<DeadlineEntry<P>>>,
    next_sequence: u64,
}

impl<P> Default for DeadlineQueue<P> {
    fn default() -> Self {
        Self {
            entries: BinaryHeap::new(),
            next_sequence: 0,
        }
    }
}

impl<P> DeadlineQueue<P> {
    pub(super) fn push(
        &mut self,
        at: Instant,
        payload: P,
    ) -> Result<(), DeadlineSequenceExhausted> {
        let sequence = self.next_sequence;
        self.next_sequence = sequence.checked_add(1).ok_or(DeadlineSequenceExhausted)?;
        self.entries.push(Reverse(DeadlineEntry {
            at,
            sequence,
            payload,
        }));
        Ok(())
    }

    pub(super) fn pop_one_due(&mut self, now: Instant) -> Option<P> {
        if self.entries.peek()?.0.at > now {
            return None;
        }
        self.entries.pop().map(|entry| entry.0.payload)
    }

    pub(super) fn next(&self) -> Option<Instant> {
        self.entries.peek().map(|entry| entry.0.at)
    }

    pub(super) fn has_due(&self, now: Instant) -> bool {
        self.entries.peek().is_some_and(|entry| entry.0.at <= now)
    }

    pub(super) fn drain_due(&mut self, now: Instant, limit: usize) -> VecDeque<P> {
        let mut due = VecDeque::new();
        while due.len() < limit
            && let Some(payload) = self.pop_one_due(now)
        {
            due.push_back(payload);
        }
        due
    }

    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(super) fn exhaust_sequence_for_test(&mut self) {
        self.next_sequence = u64::MAX;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn deadlines_are_stable_and_popped_one_at_a_time() {
        let now = Instant::now();
        let mut deadlines = DeadlineQueue::default();
        deadlines
            .push(now + Duration::from_secs(2), "later")
            .unwrap();
        deadlines.push(now, "first").unwrap();
        deadlines.push(now, "second").unwrap();

        assert_eq!(deadlines.pop_one_due(now), Some("first"));
        assert_eq!(deadlines.pop_one_due(now), Some("second"));
        assert_eq!(deadlines.next(), Some(now + Duration::from_secs(2)));
        assert_eq!(deadlines.pop_one_due(now), None);
    }

    #[test]
    fn deadline_sequence_exhaustion_is_checked_without_inserting() {
        let now = Instant::now();
        let mut deadlines = DeadlineQueue {
            next_sequence: u64::MAX,
            ..DeadlineQueue::default()
        };

        assert_eq!(
            deadlines.push(now, "overflow"),
            Err(DeadlineSequenceExhausted)
        );
        assert_eq!(deadlines.next(), None);
    }
}
