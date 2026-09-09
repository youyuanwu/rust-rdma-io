//! Test-only raw operation fixtures used by sibling owner tests.

use std::sync::Arc;

use crate::v2::engine::registry::{Lookup, OperationToken};
use crate::wc::{WcOpcode, WorkCompletion};

use super::super::{Direction, EstablishedIoConnection, IoState};
use super::future::RdmaOperation;
use super::state::OperationState;

pub(in crate::v2::engine) fn install_accepted_operation_for_driver_test(
    io_core: &mut IoState,
    connections: &mut crate::v2::engine::session::registry::ConnectionRegistry,
    connection: crate::v2::engine::registry::ConnectionToken,
    opcode: WcOpcode,
) -> OperationToken {
    let direction = if opcode == WcOpcode::Recv {
        Direction::Recv
    } else {
        Direction::Send
    };
    let token = connections
        .with_connection_io_mut(connection, |connection, connection_io, _poster| {
            io_core
                .reserve_local(connection, connection_io, direction)
                .unwrap();
            assert!(io_core.cq_credits.reserve());
            let token = io_core
                .operations
                .allocate(|token| {
                    OperationState::new(token, Arc::clone(connection), direction, opcode, None, 1)
                })
                .unwrap();
            io_core.add_accepted(connection, connection_io, token);
            token
        })
        .expect("test connection remains registered");
    let Lookup::Occupied(operation) = io_core.operations.lookup_mut(token) else {
        unreachable!("test operation remains registered")
    };
    operation.commit_accepted();
    io_core.accepted_operations += 1;
    token
}

pub(in crate::v2::engine) fn register_operation_waker_for_test(
    io_core: &IoState,
    token: OperationToken,
    waker: &std::task::Waker,
) {
    let Lookup::Occupied(operation) = io_core.operations.lookup(token) else {
        panic!("test operation must remain registered")
    };
    operation.register_waker(waker);
}

pub(in crate::v2::engine) fn operation_future_for_io_lifetime_test(
    io_core: &mut IoState,
    connection: &Arc<EstablishedIoConnection>,
    connection_io: &mut super::super::ConnectionIoState,
) -> RdmaOperation {
    io_core
        .reserve_local(connection, connection_io, Direction::Send)
        .unwrap();
    assert!(io_core.cq_credits.reserve());
    let token = io_core
        .operations
        .allocate(|token| {
            OperationState::new(
                token,
                Arc::clone(connection),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
            )
        })
        .unwrap();
    io_core.add_accepted(connection, connection_io, token);
    let Lookup::Occupied(operation) = io_core.operations.lookup_mut(token) else {
        unreachable!("test operation remains registered")
    };
    let observer = operation.observer_for_test();
    operation.commit_accepted();
    io_core.accepted_operations += 1;
    RdmaOperation::from_in_flight(token, observer)
}

pub(in crate::v2::engine) fn completion_for_driver_test(
    token: OperationToken,
    qp_num: u32,
    opcode: u32,
    status: u32,
) -> WorkCompletion {
    let mut completion = WorkCompletion::default();
    completion.inner.wr_id = token.encode();
    completion.inner.qp_num = qp_num;
    completion.inner.opcode = opcode;
    completion.inner.status = status;
    completion
}
