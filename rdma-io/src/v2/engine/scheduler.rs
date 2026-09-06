//! Bounded owner rotation and owner-neutral deadline scheduling.
//!
//! Each work class can occupy its queue at most once. A class that remains
//! ready is appended at the tail.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, VecDeque};

use tokio::time::Instant;

use super::progress::OwnerClass;

const OWNER_CLASS_COUNT: usize = 2;

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

    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        self.entries.clear();
    }

    #[cfg(test)]
    pub(super) fn exhaust_sequence_for_test(&mut self) {
        self.next_sequence = u64::MAX;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Source {
    First,
    Second,
}

pub(super) struct AlternatingSources {
    first_starts_next_turn: bool,
}

impl Default for AlternatingSources {
    fn default() -> Self {
        Self {
            first_starts_next_turn: true,
        }
    }
}

impl AlternatingSources {
    pub(super) fn begin_turn(&mut self) -> AlternatingTurn {
        let first_preferred = self.first_starts_next_turn;
        self.first_starts_next_turn = !first_preferred;
        AlternatingTurn { first_preferred }
    }

    #[cfg(test)]
    pub(super) fn first_starts_next_turn(&self) -> bool {
        self.first_starts_next_turn
    }
}

pub(super) struct AlternatingTurn {
    first_preferred: bool,
}

impl AlternatingTurn {
    pub(super) fn order(&self) -> [Source; 2] {
        if self.first_preferred {
            [Source::First, Source::Second]
        } else {
            [Source::Second, Source::First]
        }
    }

    pub(super) fn consumed(&mut self) {
        self.first_preferred = !self.first_preferred;
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

        assert_eq!(scheduler.ready_count(), OWNER_CLASS_COUNT);
        assert_eq!(scheduler.next(), Some(OwnerClass::Io));
        scheduler.mark_ready(OwnerClass::Io);
        assert_eq!(scheduler.next(), Some(OwnerClass::Session));
        assert_eq!(scheduler.next(), Some(OwnerClass::Io));
        assert_eq!(scheduler.next(), None);
    }

    #[test]
    fn ready_at_entry_bounds_one_turn_per_owner() {
        let mut scheduler = OwnerScheduler::new();
        for class in [OwnerClass::Io, OwnerClass::Session] {
            scheduler.mark_ready(class);
        }
        let pass_budget = scheduler.ready_count();
        let mut serviced = Vec::new();
        for _ in 0..pass_budget {
            let class = scheduler.next().unwrap();
            serviced.push(class);
            scheduler.mark_ready(class);
        }

        assert_eq!(serviced, [OwnerClass::Io, OwnerClass::Session]);
        assert_eq!(scheduler.ready_count(), OWNER_CLASS_COUNT);
        assert_eq!(scheduler.next(), Some(OwnerClass::Io));
    }

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
        let mut deadlines = DeadlineQueue::default();
        deadlines.next_sequence = u64::MAX;

        assert_eq!(
            deadlines.push(now, "overflow"),
            Err(DeadlineSequenceExhausted)
        );
        assert_eq!(deadlines.next(), None);
    }

    #[test]
    fn alternating_sources_flip_by_turn_and_consumed_unit() {
        let mut sources = AlternatingSources::default();
        let mut first = sources.begin_turn();
        assert_eq!(first.order(), [Source::First, Source::Second]);
        first.consumed();
        assert_eq!(first.order(), [Source::Second, Source::First]);

        let second = sources.begin_turn();
        assert_eq!(second.order(), [Source::Second, Source::First]);
        assert!(sources.first_starts_next_turn());
    }

    fn consume_sources(
        sources: &mut AlternatingSources,
        budget: usize,
        first_available: &mut usize,
        second_available: &mut usize,
    ) -> Vec<Source> {
        let mut turn = sources.begin_turn();
        let mut consumed = Vec::new();
        while consumed.len() < budget {
            let mut selected = None;
            for source in turn.order() {
                let available = match source {
                    Source::First => &mut *first_available,
                    Source::Second => &mut *second_available,
                };
                if *available > 0 {
                    *available -= 1;
                    selected = Some(source);
                    break;
                }
            }
            let Some(source) = selected else {
                break;
            };
            consumed.push(source);
            turn.consumed();
        }
        consumed
    }

    #[test]
    fn alternating_sources_cover_odd_even_budgets_and_sustained_pressure() {
        let mut sources = AlternatingSources::default();
        let mut first_available = 8;
        let mut second_available = 8;

        assert_eq!(
            consume_sources(&mut sources, 3, &mut first_available, &mut second_available),
            [Source::First, Source::Second, Source::First]
        );
        assert_eq!(
            consume_sources(&mut sources, 4, &mut first_available, &mut second_available),
            [Source::Second, Source::First, Source::Second, Source::First]
        );
    }

    #[test]
    fn alternating_sources_transfer_unused_opportunities_to_nonempty_source() {
        let mut sources = AlternatingSources::default();
        let mut first_available = 0;
        let mut second_available = 3;

        assert_eq!(
            consume_sources(&mut sources, 3, &mut first_available, &mut second_available),
            [Source::Second, Source::Second, Source::Second]
        );
    }
}
