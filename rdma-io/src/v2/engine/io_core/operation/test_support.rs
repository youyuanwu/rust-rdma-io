//! Test-only raw operation fixtures used by sibling owner tests.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::v2::engine::registry::{Lookup, OperationToken};
use crate::wc::{WcOpcode, WorkCompletion};

use super::super::{Direction, EstablishedIoConnection, IoCore};
use super::future::RdmaOperation;
use super::state::OperationState;

pub(in crate::v2::engine) fn install_accepted_operation_for_driver_test(
    io_core: &IoCore,
    connection: &Arc<crate::v2::engine::session::connection::ConnectionState>,
    opcode: WcOpcode,
) -> OperationToken {
    let direction = if opcode == WcOpcode::Recv {
        Direction::Recv
    } else {
        Direction::Send
    };
    connection.reserve_local(direction).unwrap();
    assert!(io_core.cq_credits.reserve());
    let (token, operation) = io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                Arc::clone(connection),
                direction,
                opcode,
                None,
                1,
            ))
        })
        .unwrap();
    operation.commit_accepted();
    io_core.accepted_operations.fetch_add(1, Ordering::AcqRel);
    token
}

pub(in crate::v2::engine) fn register_operation_waker_for_test(
    io_core: &IoCore,
    token: OperationToken,
    waker: &std::task::Waker,
) {
    let Lookup::Occupied(operation) = io_core.operations.lookup(token) else {
        panic!("test operation must remain registered")
    };
    operation.register_waker(waker);
}

pub(in crate::v2::engine) fn operation_future_for_io_lifetime_test(
    io_core: &Arc<IoCore>,
    connection: &Arc<EstablishedIoConnection>,
) -> RdmaOperation {
    connection.reserve_local(Direction::Send).unwrap();
    assert!(io_core.cq_credits.reserve());
    let (_, operation) = io_core
        .operations
        .allocate(|token| {
            Arc::new(OperationState::new(
                token,
                Arc::clone(connection),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
            ))
        })
        .unwrap();
    operation.commit_accepted();
    io_core.accepted_operations.fetch_add(1, Ordering::AcqRel);
    RdmaOperation::from_in_flight(Arc::clone(io_core), operation)
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
