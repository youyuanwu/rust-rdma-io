//! Bounded, consumed post-turn publication.

use std::collections::VecDeque;
use std::sync::Arc;

use crate::v2::engine::io::PendingIoEvent;
use crate::v2::engine::io_core::OperationObserver;

/// The exact maximum number of user-visible publication leaves produced by
/// one external engine-driver poll.
pub(in crate::v2::engine) const REACTOR_ACTION_BUDGET: usize = 32;

type Publication = Box<dyn FnOnce() + Send + 'static>;

/// Detached actions published only after the reactor turn releases its
/// exclusive borrow.
///
/// This is a turn-local value, not a publication backlog. A source must check
/// the remaining capacity before it removes or mutates authoritative work.
/// Work that does not fit therefore remains on its operation, listener,
/// close, terminal, or other existing owner record for a later reactor turn.
pub(in crate::v2::engine) struct ReactorActions {
    capacity: usize,
    events: VecDeque<Publication>,
    operations: VecDeque<Publication>,
    closes_and_listeners: VecDeque<Publication>,
    terminal: VecDeque<Publication>,
}

/// Bounded publication remainder owned by a dequeued protocol command.
///
/// This is not a second action queue: it is the single consumed result of one
/// provider submission. It remains on the command ingress until each action
/// has moved into a turn-local [`ReactorActions`] batch.
pub(in crate::v2::engine) struct DeferredProtocolActions {
    actions: ReactorActions,
}

impl Default for ReactorActions {
    fn default() -> Self {
        Self {
            capacity: REACTOR_ACTION_BUDGET,
            events: VecDeque::new(),
            operations: VecDeque::new(),
            closes_and_listeners: VecDeque::new(),
            terminal: VecDeque::new(),
        }
    }
}

impl ReactorActions {
    pub(in crate::v2::engine) fn for_synchronous_driver_drop() -> Self {
        Self {
            capacity: usize::MAX,
            ..Self::default()
        }
    }

    pub(in crate::v2::engine) fn remaining(&self) -> usize {
        self.capacity.saturating_sub(self.len())
    }

    pub(in crate::v2::engine) fn can_accept(&self, leaves: usize) -> bool {
        leaves <= self.remaining()
    }

    pub(in crate::v2::engine) fn push_event(&mut self, event: PendingIoEvent) {
        assert!(
            self.can_accept(1),
            "reactor event action exceeded turn budget"
        );
        self.events.push_back(Box::new(move || event.deliver()));
    }

    pub(in crate::v2::engine) fn push_operation_wake(&mut self, observer: Arc<OperationObserver>) {
        assert!(
            self.can_accept(1),
            "reactor operation wake exceeded turn budget"
        );
        self.operations.push_back(Box::new(move || observer.wake()));
    }

    pub(in crate::v2::engine) fn push_operation(&mut self, action: impl FnOnce() + Send + 'static) {
        assert!(
            self.can_accept(1),
            "reactor operation action exceeded turn budget"
        );
        self.operations.push_back(Box::new(action));
    }

    pub(in crate::v2::engine) fn push_close_or_listener(
        &mut self,
        action: impl FnOnce() + Send + 'static,
    ) {
        assert!(
            self.can_accept(1),
            "reactor close/listener action exceeded turn budget"
        );
        self.closes_and_listeners.push_back(Box::new(action));
    }

    pub(in crate::v2::engine) fn push_terminal(&mut self, action: impl FnOnce() + Send + 'static) {
        assert!(
            self.can_accept(1),
            "reactor terminal action exceeded turn budget"
        );
        self.terminal.push_back(Box::new(action));
    }

    pub(in crate::v2::engine) fn len(&self) -> usize {
        self.events.len()
            + self.operations.len()
            + self.closes_and_listeners.len()
            + self.terminal.len()
    }

    fn append_bounded_to(&mut self, target: &mut Self) -> usize {
        let mut appended = 0;
        while target.can_accept(1) {
            if let Some(action) = self.events.pop_front() {
                target.events.push_back(action);
            } else if let Some(action) = self.operations.pop_front() {
                target.operations.push_back(action);
            } else if let Some(action) = self.closes_and_listeners.pop_front() {
                target.closes_and_listeners.push_back(action);
            } else if let Some(action) = self.terminal.pop_front() {
                target.terminal.push_back(action);
            } else {
                break;
            }
            appended += 1;
        }
        appended
    }

    /// Consume the batch in deterministic observer order.
    pub(in crate::v2::engine) fn publish(self) {
        debug_assert!(
            self.capacity == usize::MAX || self.len() <= REACTOR_ACTION_BUDGET,
            "one reactor turn cannot publish more than its exact leaf budget"
        );
        for action in self
            .events
            .into_iter()
            .chain(self.operations)
            .chain(self.closes_and_listeners)
            .chain(self.terminal)
        {
            action();
        }
    }

    /// Collapse setup-only detached effects into one bounded publication
    /// leaf. Setup has already committed all reactor-owned state before this
    /// leaf can run, and a provider rejection of a batch may otherwise
    /// produce more than one ordinary turn's worth of returned-MR events.
    pub(in crate::v2::engine) fn append_setup_result_to(self, target: &mut Self) {
        if self.len() == 0 {
            return;
        }
        assert!(
            target.can_accept(1),
            "pre-establishment setup result exceeded turn budget"
        );
        target.push_operation(move || self.publish());
    }
}

impl DeferredProtocolActions {
    /// A batch can produce one completion event and one operation wake per WR,
    /// plus at most one connection-drain wake when its final accepted WR
    /// completes during submission.
    pub(in crate::v2::engine) fn new(operations: usize) -> Self {
        Self {
            actions: ReactorActions {
                // Every early-completed operation can detach a connection
                // drain wake, an I/O event, and an operation wake.
                // One additional resource-free submission receipt reports
                // the reconciled provider disposition to the protocol owner.
                capacity: operations.saturating_mul(3).saturating_add(1),
                ..ReactorActions::default()
            },
        }
    }

    pub(in crate::v2::engine) fn actions_mut(&mut self) -> &mut ReactorActions {
        &mut self.actions
    }

    pub(in crate::v2::engine) fn append_bounded_to(
        &mut self,
        target: &mut ReactorActions,
    ) -> usize {
        self.actions.append_bounded_to(target)
    }

    pub(in crate::v2::engine) fn is_empty(&self) -> bool {
        self.actions.len() == 0
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn publish_synchronously(mut self) {
        let mut actions = ReactorActions::for_synchronous_driver_drop();
        self.append_bounded_to(&mut actions);
        debug_assert!(self.is_empty());
        actions.publish();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;

    #[test]
    fn exact_budget_rejects_the_thirty_third_leaf() {
        let mut actions = ReactorActions::default();
        for _ in 0..REACTOR_ACTION_BUDGET {
            assert!(actions.can_accept(1));
            actions.push_operation(|| {});
        }
        assert!(!actions.can_accept(1));
        assert_eq!(actions.len(), REACTOR_ACTION_BUDGET);
    }

    #[test]
    fn consumed_batch_publishes_in_required_order() {
        let observed = Arc::new(Mutex::new(Vec::new()));
        let mut actions = ReactorActions::default();
        let target = Arc::clone(&observed);
        actions.push_terminal(move || target.lock().unwrap().push("terminal"));
        let target = Arc::clone(&observed);
        actions.push_close_or_listener(move || target.lock().unwrap().push("listener"));
        let target = Arc::clone(&observed);
        actions.push_operation(move || target.lock().unwrap().push("operation"));

        actions.events.push_back(Box::new({
            let target = Arc::clone(&observed);
            move || target.lock().unwrap().push("event")
        }));

        actions.publish();
        assert_eq!(
            *observed.lock().unwrap(),
            ["event", "operation", "listener", "terminal"]
        );
    }

    #[test]
    fn batch_has_no_overflow_storage() {
        let mut actions = ReactorActions::default();
        for _ in 0..REACTOR_ACTION_BUDGET {
            actions.push_operation(|| {});
        }
        assert_eq!(actions.remaining(), 0);
        assert!(!actions.can_accept(1));
    }

    #[test]
    fn setup_result_collapses_more_than_thirty_two_reentrant_effects() {
        let borrow_gate = Arc::new(Mutex::new(()));
        let observed = Arc::new(Mutex::new(Vec::new()));
        let reactor_borrow = borrow_gate.lock().unwrap();
        let mut setup = ReactorActions::for_synchronous_driver_drop();
        for index in 0..40 {
            let borrow_gate = Arc::clone(&borrow_gate);
            let observed = Arc::clone(&observed);
            setup.push_operation(move || {
                let _reentrant = borrow_gate
                    .try_lock()
                    .expect("setup publication runs after the reactor borrow ends");
                observed.lock().unwrap().push(index);
            });
        }
        let mut turn = ReactorActions::default();
        setup.append_setup_result_to(&mut turn);
        assert_eq!(turn.len(), 1);
        assert!(observed.lock().unwrap().is_empty());

        drop(reactor_borrow);
        turn.publish();
        assert_eq!(*observed.lock().unwrap(), (0..40).collect::<Vec<_>>());
    }
}
