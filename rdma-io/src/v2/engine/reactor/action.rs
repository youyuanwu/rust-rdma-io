//! Bounded, consumed post-turn publication.

use std::sync::Arc;

use crate::v2::engine::io::PendingIoEvent;
use crate::v2::engine::io_core::OperationState;

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
    events: Vec<Publication>,
    operations: Vec<Publication>,
    closes_and_listeners: Vec<Publication>,
    terminal: Vec<Publication>,
}

impl Default for ReactorActions {
    fn default() -> Self {
        Self {
            capacity: REACTOR_ACTION_BUDGET,
            events: Vec::new(),
            operations: Vec::new(),
            closes_and_listeners: Vec::new(),
            terminal: Vec::new(),
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
        self.events.push(Box::new(move || event.deliver()));
    }

    pub(in crate::v2::engine) fn push_operation_wake(&mut self, operation: Arc<OperationState>) {
        assert!(
            self.can_accept(1),
            "reactor operation wake exceeded turn budget"
        );
        self.operations.push(Box::new(move || operation.wake()));
    }

    pub(in crate::v2::engine) fn push_operation(&mut self, action: impl FnOnce() + Send + 'static) {
        assert!(
            self.can_accept(1),
            "reactor operation action exceeded turn budget"
        );
        self.operations.push(Box::new(action));
    }

    pub(in crate::v2::engine) fn push_close_or_listener(
        &mut self,
        action: impl FnOnce() + Send + 'static,
    ) {
        assert!(
            self.can_accept(1),
            "reactor close/listener action exceeded turn budget"
        );
        self.closes_and_listeners.push(Box::new(action));
    }

    pub(in crate::v2::engine) fn push_terminal(&mut self, action: impl FnOnce() + Send + 'static) {
        assert!(
            self.can_accept(1),
            "reactor terminal action exceeded turn budget"
        );
        self.terminal.push(Box::new(action));
    }

    pub(in crate::v2::engine) fn len(&self) -> usize {
        self.events.len()
            + self.operations.len()
            + self.closes_and_listeners.len()
            + self.terminal.len()
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

        actions.events.push(Box::new({
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
}
