//! Owned low-level operation futures, admission, and exact CQE routing.

mod accounting;
mod batch;
mod completion;
mod effects;
mod future;
mod state;
mod validation;

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};
#[cfg(test)]
use std::{
    future::Future,
    pin::Pin,
    sync::Mutex,
    sync::atomic::{AtomicBool, AtomicUsize},
    task::{Context, Poll},
};

#[cfg(test)]
use super::super::io::{
    IoEventDestination, IoOperationContext, IoRecvRequest, IoSendRequest, IoSubmissionDisposition,
};
use super::super::lifecycle::MemoizedTerminalResult;
#[cfg(test)]
use super::super::registry::lock_unpoison;
use super::super::registry::{ConnectionToken, Lookup, OperationToken};
#[cfg(test)]
use super::Direction;
#[cfg(test)]
use super::OperationKind;
use super::{EstablishedIoConnection, IoCore};
use crate::v2::error::Error;
#[cfg(test)]
use crate::v2::error::Result;
#[cfg(test)]
use crate::v2::mr::Mr;
#[cfg(test)]
use crate::v2::op::Completion;
#[cfg(test)]
use crate::v2::qp::BatchPostOutcome;
#[cfg(test)]
use crate::wc::WcOpcode;
#[cfg(test)]
use crate::wc::WorkCompletion;
#[cfg(test)]
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, Sge, WrOpcode};

pub(super) use accounting::{CqCreditPool, OperationRegistry};
#[cfg(test)]
use batch::test_support::{
    InternalBatchEntry, InternalRelease, commit_internal_entries, release_proven_unaccepted_entries,
};
#[cfg(test)]
use batch::{BatchOwnershipTransfer, PreparedBatchOwnership};
pub(in crate::v2::engine) use batch::{post_io_recv_batch, post_io_send};
pub(in crate::v2::engine) use completion::CqeReject;
use effects::{AfterEngineUnlock, DetachedIoCoreEffects};
pub(in crate::v2::engine) use effects::{
    CommittedIoCoreEffects, IoCoreEffects, OperationQuarantineEffect,
};
pub use future::RdmaOperation;
#[cfg(test)]
use future::publish_after_post_guards;
#[cfg(test)]
use state::OperationState;
#[cfg(test)]
use state::{CompletionDisposition, OperationLifecycle};

/// Non-forgeable port for proof-gated reclamation.
///
/// `IoCore::new` creates exactly one value and transfers it to the session
/// owner. The core has no dependency on the session proof or resource types.
pub(in crate::v2::engine) struct QpReclaimCapability {
    core: Weak<IoCore>,
}

impl QpReclaimCapability {
    pub(super) fn new(core: &Arc<IoCore>) -> Self {
        Self {
            core: Arc::downgrade(core),
        }
    }

    pub(in crate::v2::engine) fn reclaim(
        &self,
        destroyed_connection: ConnectionToken,
        destroyed_qp_num: u32,
        connection: &EstablishedIoConnection,
        close_error: Error,
        token: OperationToken,
    ) -> (bool, IoCoreEffects) {
        let Some(core) = self.core.upgrade() else {
            return (false, IoCoreEffects::default());
        };
        core.reclaim_after_qp_destroy(
            destroyed_connection,
            destroyed_qp_num,
            connection,
            close_error,
            token,
        )
    }
}

impl IoCore {
    pub(in crate::v2::engine) fn fail_observers_for_close(
        &self,
        tokens: &[OperationToken],
        error: Error,
    ) -> DetachedIoCoreEffects {
        let mut after_unlock = AfterEngineUnlock::default();
        for token in tokens.iter().copied() {
            if let Lookup::Occupied(operation) = self.operations.lookup(token)
                && operation.fail_observer_for_close(error.clone())
            {
                after_unlock.push_operation_wake(operation);
            }
        }
        DetachedIoCoreEffects::new(after_unlock)
    }

    pub(in crate::v2::engine) fn terminalize_operations(
        &self,
        outcome: &MemoizedTerminalResult,
    ) -> IoCoreEffects {
        let mut effects = IoCoreEffects::default();
        if !outcome.is_error() {
            return effects;
        }
        for operation in self.operations.occupied() {
            let terminalized = operation.finalize_terminal(outcome);
            debug_assert!(
                !terminalized.was_reclaiming || terminalized.newly_quarantined,
                "terminal reclamation must transfer its retained MR and CQ debt to quarantine"
            );
            if terminalized.was_reclaiming {
                self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
            }
            if terminalized.newly_quarantined {
                self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
                self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
                self.quarantined_bytes
                    .fetch_add(operation.mr_len(), Ordering::AcqRel);
                self.cq_credits.retain();
                effects.push_quarantine(OperationQuarantineEffect::Added {
                    operation: operation.token(),
                    connection: operation.connection_token(),
                });
            }
            if terminalized.should_wake {
                effects.push_operation_wake(operation);
            }
        }
        effects
    }

    pub(in crate::v2::engine) fn terminalize_operations_bounded(
        &self,
        outcome: &MemoizedTerminalResult,
        cursor: usize,
        budget: usize,
    ) -> (IoCoreEffects, usize, bool, usize) {
        let mut effects = IoCoreEffects::default();
        if !outcome.is_error() {
            return (effects, cursor, true, 0);
        }
        let (operations, next, complete, scanned) = self.operations.scan_occupied(cursor, budget);
        for operation in operations {
            let terminalized = operation.finalize_terminal(outcome);
            debug_assert!(
                !terminalized.was_reclaiming || terminalized.newly_quarantined,
                "terminal reclamation must transfer its retained MR and CQ debt to quarantine"
            );
            if terminalized.was_reclaiming {
                self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
            }
            if terminalized.newly_quarantined {
                self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
                self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
                self.quarantined_bytes
                    .fetch_add(operation.mr_len(), Ordering::AcqRel);
                self.cq_credits.retain();
                effects.push_quarantine(OperationQuarantineEffect::Added {
                    operation: operation.token(),
                    connection: operation.connection_token(),
                });
            }
            if terminalized.should_wake {
                effects.push_operation_wake(operation);
            }
        }
        (effects, next, complete, scanned)
    }

    fn reclaim_after_qp_destroy(
        &self,
        destroyed_connection: ConnectionToken,
        destroyed_qp_num: u32,
        connection: &EstablishedIoConnection,
        close_error: Error,
        token: OperationToken,
    ) -> (bool, IoCoreEffects) {
        let identity = connection.identity();
        if destroyed_connection != identity.connection || destroyed_qp_num != identity.qp_num {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "operation reclaim rejected a mismatched QP destruction proof"
            );
            return (false, IoCoreEffects::default());
        }
        let Lookup::Occupied(operation) = self.operations.lookup(token) else {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "QP destruction left an accepted token without an operation registration"
            );
            return (false, IoCoreEffects::default());
        };
        if operation.connection_token() != identity.connection {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                owner = operation.connection_token().encode(),
                "QP destruction found an accepted token owned by another connection"
            );
            return (false, IoCoreEffects::default());
        }
        if !connection.remove_accepted(token) {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "QP destruction reclaim lost accepted-set membership"
            );
            return (false, IoCoreEffects::default());
        }
        if self.operations.release(token, false).is_none() {
            connection.add_accepted(token);
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "QP destruction reclaim could not retire the operation registration"
            );
            return (false, IoCoreEffects::default());
        }
        connection.release_local(operation.direction());
        self.cq_credits.release();
        let previous = self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "accepted operation count must be positive");
        self.publish_io_if_drained(previous);
        let finished = operation.finish_after_qp_destroy(close_error);
        if finished.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        let mut effects = IoCoreEffects::default();
        if let Some(event) = finished.event {
            effects.push_event(event);
        }
        effects.push_operation_wake(Arc::clone(&operation));
        if finished.was_quarantined {
            self.cq_credits.release_retained();
            self.quarantined_operations.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_mrs.fetch_sub(1, Ordering::AcqRel);
            self.quarantined_bytes
                .fetch_sub(operation.mr_len(), Ordering::AcqRel);
            effects.push_quarantine(OperationQuarantineEffect::Cleared {
                operation: operation.token(),
                connection: operation.connection_token(),
            });
        }
        (true, effects)
    }

    pub(in crate::v2::engine) fn begin_reclamation(&self, token: OperationToken) {
        if let Lookup::Occupied(operation) = self.operations.lookup(token) {
            operation.mark_reclaiming();
        }
    }

    pub(in crate::v2::engine) fn handle_reclamation_deadline(
        &self,
        token: OperationToken,
    ) -> IoCoreEffects {
        self.quarantine_operation(token)
    }

    pub(in crate::v2::engine) fn quarantine_operation(
        &self,
        token: OperationToken,
    ) -> IoCoreEffects {
        let Lookup::Occupied(operation) = self.operations.lookup(token) else {
            return IoCoreEffects::default();
        };
        let transition = operation.mark_quarantined();
        if !transition.newly_quarantined {
            return IoCoreEffects::default();
        }
        if transition.was_reclaiming {
            self.pending_reclamations.fetch_sub(1, Ordering::AcqRel);
        }
        self.quarantined_operations.fetch_add(1, Ordering::AcqRel);
        self.quarantined_mrs.fetch_add(1, Ordering::AcqRel);
        self.quarantined_bytes
            .fetch_add(operation.mr_len(), Ordering::AcqRel);
        self.cq_credits.retain();
        let mut effects = IoCoreEffects::default();
        effects.push_quarantine(OperationQuarantineEffect::Added {
            operation: operation.token(),
            connection: operation.connection_token(),
        });
        effects
    }
}

#[cfg(test)]
pub(in crate::v2::engine) fn install_accepted_operation_for_driver_test(
    io_core: &IoCore,
    connection: &Arc<super::super::session::connection::ConnectionState>,
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
mod tests;
