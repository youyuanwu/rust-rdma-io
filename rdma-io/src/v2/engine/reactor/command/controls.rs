use std::collections::VecDeque;
use std::sync::Arc;

use super::coalesced::CoalescedQueue;
use crate::v2::engine::registry::{ConnectionToken, ListenerToken, OperationToken};
use crate::v2::engine::session::cm::OutboundRequest;

pub(super) struct ControlQueue {
    pub(super) connection_close: CoalescedQueue<ConnectionToken>,
    pub(super) connect_cancel: VecDeque<Arc<OutboundRequest>>,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) connection_error: VecDeque<ConnectionToken>,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) connection_disconnect: VecDeque<ConnectionToken>,
    #[cfg(any(test, feature = "test-hooks"))]
    pub(super) connection_fail_qp_destroy: VecDeque<ConnectionToken>,
    pub(super) operation_cancel: CoalescedQueue<OperationToken>,
    pub(super) listener_close: CoalescedQueue<ListenerToken>,
    pub(super) listener_work: CoalescedQueue<ListenerToken>,
}

impl ControlQueue {
    pub(super) fn new(connection_capacity: usize, operation_capacity: usize) -> Self {
        Self {
            connection_close: CoalescedQueue::new(connection_capacity),
            connect_cancel: VecDeque::new(),
            #[cfg(any(test, feature = "test-hooks"))]
            connection_error: VecDeque::new(),
            #[cfg(any(test, feature = "test-hooks"))]
            connection_disconnect: VecDeque::new(),
            #[cfg(any(test, feature = "test-hooks"))]
            connection_fail_qp_destroy: VecDeque::new(),
            operation_cancel: CoalescedQueue::new(operation_capacity),
            listener_close: CoalescedQueue::new(connection_capacity),
            listener_work: CoalescedQueue::new(connection_capacity),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::ControlQueue;
    use crate::v2::engine::registry::{ConnectionToken, ListenerToken, OperationToken};

    #[test]
    fn all_four_controls_share_the_same_bounded_coalescing_contract() {
        let mut controls = ControlQueue::new(1, 1);
        let connection = ConnectionToken::decode(1);
        let operation = OperationToken::decode(1);
        let listener = ListenerToken::decode(1);

        assert!(controls.connection_close.push(connection));
        assert!(!controls.connection_close.push(connection));
        assert!(controls.operation_cancel.push(operation));
        assert!(!controls.operation_cancel.push(operation));
        assert!(controls.listener_close.push(listener));
        assert!(!controls.listener_close.push(listener));
        assert!(controls.listener_work.push(listener));
        assert!(!controls.listener_work.push(listener));

        assert_eq!(controls.connection_close.pop(), Some(connection));
        assert_eq!(controls.operation_cancel.pop(), Some(operation));
        assert_eq!(controls.listener_close.pop(), Some(listener));
        assert_eq!(controls.listener_work.pop(), Some(listener));
    }

    #[test]
    fn all_four_controls_fail_fast_on_distinct_overflow() {
        let connection_overflow = std::panic::catch_unwind(|| {
            let mut controls = ControlQueue::new(1, 1);
            controls.connection_close.push(ConnectionToken::decode(1));
            controls.connection_close.push(ConnectionToken::decode(2));
        });
        let operation_overflow = std::panic::catch_unwind(|| {
            let mut controls = ControlQueue::new(1, 1);
            controls.operation_cancel.push(OperationToken::decode(1));
            controls.operation_cancel.push(OperationToken::decode(2));
        });
        let listener_close_overflow = std::panic::catch_unwind(|| {
            let mut controls = ControlQueue::new(1, 1);
            controls.listener_close.push(ListenerToken::decode(1));
            controls.listener_close.push(ListenerToken::decode(2));
        });
        let listener_work_overflow = std::panic::catch_unwind(|| {
            let mut controls = ControlQueue::new(1, 1);
            controls.listener_work.push(ListenerToken::decode(1));
            controls.listener_work.push(ListenerToken::decode(2));
        });

        assert!(connection_overflow.is_err());
        assert!(operation_overflow.is_err());
        assert!(listener_close_overflow.is_err());
        assert!(listener_work_overflow.is_err());
    }
}
