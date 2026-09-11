use std::collections::VecDeque;
use std::sync::Arc;

use tokio::sync::OwnedSemaphorePermit;

use crate::v2::engine::io::ProtocolCommand;
use crate::v2::engine::io_core::OperationCommand;
use crate::v2::engine::registry::ListenerToken;
use crate::v2::engine::session::cm::OutboundRequest;
use crate::v2::engine::session::connection::ConnectionReservation;
use crate::v2::engine::session::listener::{AcceptRequest, ListenRequest};

pub(super) enum SessionCommand {
    Connect {
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
        _permit: OwnedSemaphorePermit,
    },
    Listen {
        request: Arc<ListenRequest>,
        _permit: OwnedSemaphorePermit,
    },
    #[cfg(any(test, feature = "test-hooks"))]
    TestInstall {
        request: Arc<crate::v2::engine::driver::test_api::TestConnectionInstallRequest>,
        reservation: ConnectionReservation,
        _permit: OwnedSemaphorePermit,
    },
}

#[derive(Default)]
pub(super) struct CommandQueues {
    pub(super) connect: VecDeque<SessionCommand>,
    pub(super) listen: VecDeque<SessionCommand>,
    pub(super) accept: VecDeque<(ListenerToken, Arc<AcceptRequest>)>,
    pub(super) operation: VecDeque<(Arc<OperationCommand>, OwnedSemaphorePermit)>,
    pub(super) protocol: VecDeque<ProtocolQueueEntry>,
    pub(super) next_class: usize,
}

impl CommandQueues {
    pub(super) fn select_ready_class(&mut self, ready: [bool; 5]) -> Option<usize> {
        for offset in 0..ready.len() {
            let class = (self.next_class + offset) % ready.len();
            if ready[class] {
                self.next_class = (class + 1) % ready.len();
                return Some(class);
            }
        }
        None
    }
}

pub(super) enum ProtocolQueueEntry {
    Command(ProtocolCommand, OwnedSemaphorePermit),
    Publication(crate::v2::engine::reactor::DeferredProtocolActions),
}

#[cfg(test)]
mod tests {
    use super::CommandQueues;

    #[test]
    fn five_class_cursor_rotates_and_skips_blocked_classes() {
        let mut queues = CommandQueues::default();
        assert_eq!(queues.select_ready_class([true; 5]), Some(0));
        assert_eq!(queues.select_ready_class([true; 5]), Some(1));
        assert_eq!(queues.select_ready_class([true; 5]), Some(2));
        assert_eq!(queues.select_ready_class([true; 5]), Some(3));
        assert_eq!(queues.select_ready_class([true; 5]), Some(4));
        assert_eq!(queues.select_ready_class([true; 5]), Some(0));

        let mut queues = CommandQueues::default();
        assert_eq!(queues.select_ready_class([true; 5]), Some(0));
        assert_eq!(
            queues.select_ready_class([true, false, true, true, true]),
            Some(2)
        );
        assert_eq!(
            queues.select_ready_class([true, false, false, false, false]),
            Some(0)
        );
        assert_eq!(queues.next_class, 1);
    }
}
