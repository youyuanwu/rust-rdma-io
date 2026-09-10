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

pub(super) enum ProtocolQueueEntry {
    Command(ProtocolCommand, OwnedSemaphorePermit),
    Publication(crate::v2::engine::reactor::DeferredProtocolActions),
}
