//! Terminalization, quarantine, and positive-proof operation reclamation.

use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

use crate::v2::engine::lifecycle::MemoizedTerminalResult;
use crate::v2::engine::registry::{ConnectionToken, Lookup, OperationToken};
use crate::v2::error::Error;

use super::super::{EstablishedIoConnection, IoCore};
use super::effects::{
    AfterEngineUnlock, DetachedIoCoreEffects, IoCoreEffects, OperationQuarantineEffect,
};

/// Non-forgeable port for proof-gated reclamation.
///
/// `IoCore::new` creates exactly one value and transfers it to the session
/// owner. The core has no dependency on the session proof or resource types.
pub(in crate::v2::engine) struct QpReclaimCapability {
    core: Weak<IoCore>,
}

impl QpReclaimCapability {
    pub(in crate::v2::engine::io_core) fn new(core: &Arc<IoCore>) -> Self {
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
    pub(in crate::v2::engine) fn qp_destroy_publication_prefix(
        &self,
        tokens: &[OperationToken],
        budget: usize,
    ) -> usize {
        let mut leaves = 0usize;
        let mut count = 0usize;
        for token in tokens {
            let next = match self.operations.lookup(*token) {
                Lookup::Occupied(operation) => operation.qp_destroy_publication_leaves(),
                _ => 0,
            };
            if leaves.saturating_add(next) > budget {
                break;
            }
            leaves += next;
            count += 1;
        }
        count
    }

    #[cfg(test)]
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

    pub(in crate::v2::engine) fn scan_connection_observers_for_close(
        &self,
        connection: ConnectionToken,
        start: usize,
        error: Error,
        scan_budget: usize,
    ) -> (DetachedIoCoreEffects, usize, bool) {
        let (operations, next, complete, _) = self.operations.scan_occupied(start, scan_budget);
        let mut after_unlock = AfterEngineUnlock::default();
        for operation in operations {
            if operation.connection_token() == connection
                && operation.fail_observer_for_close(error.clone())
            {
                after_unlock.push_operation_wake(operation);
            }
        }
        (DetachedIoCoreEffects::new(after_unlock), next, complete)
    }

    pub(in crate::v2::engine) fn scan_connection_quarantine(
        &self,
        connection: ConnectionToken,
        start: usize,
        scan_budget: usize,
    ) -> (IoCoreEffects, usize, bool) {
        let (operations, next, complete, _) = self.operations.scan_occupied(start, scan_budget);
        let mut effects = IoCoreEffects::default();
        for operation in operations {
            if operation.connection_token() == connection {
                effects.extend(self.quarantine_operation(operation.token()));
            }
        }
        (effects, next, complete)
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
        effects.push_close_wake(connection.drain_notify());
        if let Some(event) = finished.event {
            effects.push_event(event);
        }
        if finished.should_wake {
            effects.push_operation_wake(Arc::clone(&operation));
        }
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
