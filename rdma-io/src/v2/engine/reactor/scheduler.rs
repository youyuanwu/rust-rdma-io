//! Ready-at-entry scheduling for one bounded reactor turn.

use std::collections::VecDeque;

/// Independently budgeted reactor sources.
///
/// The variants describe scheduling responsibility only. Provider and
/// lifecycle state remains in the existing authoritative owners until its
/// planned consolidation phase.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum ReactorSource {
    Commands,
    Cq,
    CompletionDispatch,
    IoReclamation,
    IoDeadline,
    CmCancellation,
    CmRetirement,
    CmOutboundStart,
    CmListenStart,
    CmListenerWork,
    CmEvent,
    CmDestruction,
    SessionDeadlineIngress,
    SessionDeadline,
    ShutdownPendingOutbound,
    ShutdownRoutes,
    ShutdownPendingListen,
    ShutdownListeners,
    ShutdownRetainedListeners,
    ShutdownConnections,
    IoTerminal,
}

impl ReactorSource {
    pub(super) const ALL: [Self; 21] = [
        Self::Commands,
        Self::Cq,
        Self::CompletionDispatch,
        Self::IoReclamation,
        Self::IoDeadline,
        Self::CmCancellation,
        Self::CmRetirement,
        Self::CmOutboundStart,
        Self::CmListenStart,
        Self::CmListenerWork,
        Self::CmEvent,
        Self::CmDestruction,
        Self::SessionDeadlineIngress,
        Self::SessionDeadline,
        Self::ShutdownPendingOutbound,
        Self::ShutdownRoutes,
        Self::ShutdownPendingListen,
        Self::ShutdownListeners,
        Self::ShutdownRetainedListeners,
        Self::ShutdownConnections,
        Self::IoTerminal,
    ];
}

pub(super) struct ReactorScheduler {
    next_start: usize,
}

impl ReactorScheduler {
    pub(super) fn new() -> Self {
        Self { next_start: 0 }
    }

    /// Snapshot ready sources in rotating order.
    ///
    /// The caller computes readiness before any source runs. Work produced by
    /// a source cannot join this queue and is deferred to a later external
    /// driver poll.
    pub(super) fn begin_turn(
        &mut self,
        mut ready: impl FnMut(ReactorSource) -> bool,
    ) -> VecDeque<ReactorSource> {
        let mut scheduled = VecDeque::with_capacity(ReactorSource::ALL.len());
        for offset in 0..ReactorSource::ALL.len() {
            let index = (self.next_start + offset) % ReactorSource::ALL.len();
            let source = ReactorSource::ALL[index];
            if ready(source) {
                scheduled.push_back(source);
            }
        }
        self.next_start = (self.next_start + 1) % ReactorSource::ALL.len();
        scheduled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_sources_ready_receive_one_quantum_and_global_start_rotates() {
        let mut scheduler = ReactorScheduler::new();
        let first = scheduler.begin_turn(|_| true);
        let second = scheduler.begin_turn(|_| true);

        assert_eq!(first.len(), ReactorSource::ALL.len());
        assert_eq!(second.len(), ReactorSource::ALL.len());
        for source in ReactorSource::ALL {
            assert_eq!(first.iter().filter(|entry| **entry == source).count(), 1);
            assert_eq!(second.iter().filter(|entry| **entry == source).count(), 1);
        }
        assert_eq!(first.front(), Some(&ReactorSource::Commands));
        assert_eq!(second.front(), Some(&ReactorSource::Cq));
        assert_eq!(second.back(), Some(&ReactorSource::Commands));
    }

    #[test]
    fn work_not_ready_at_entry_is_not_added_later() {
        let mut scheduler = ReactorScheduler::new();
        let mut command_ready = false;
        let scheduled = scheduler.begin_turn(|source| {
            if source == ReactorSource::Commands {
                let observed = command_ready;
                command_ready = true;
                observed
            } else {
                source == ReactorSource::Cq
            }
        });
        assert_eq!(
            scheduled.into_iter().collect::<Vec<_>>(),
            [ReactorSource::Cq]
        );
    }

    #[test]
    fn adversarial_all_source_feedback_is_deferred_to_the_next_turn() {
        let mut scheduler = ReactorScheduler::new();
        let first = scheduler.begin_turn(|_| true);
        let mut visits = [0usize; ReactorSource::ALL.len()];
        let mut feedback_ready = [false; ReactorSource::ALL.len()];

        for source in first {
            let index = ReactorSource::ALL
                .iter()
                .position(|candidate| *candidate == source)
                .unwrap();
            visits[index] += 1;
            feedback_ready[(index + 1) % feedback_ready.len()] = true;
        }

        assert!(visits.into_iter().all(|count| count == 1));
        assert!(feedback_ready.into_iter().all(|ready| ready));
        let second = scheduler.begin_turn(|source| {
            let index = ReactorSource::ALL
                .iter()
                .position(|candidate| *candidate == source)
                .unwrap();
            feedback_ready[index]
        });
        assert_eq!(second.len(), ReactorSource::ALL.len());
    }
}
