//! Terminalization, quarantine, and positive-proof operation reclamation.

use crate::v2::engine::lifecycle::MemoizedTerminalResult;
use crate::v2::engine::registry::{ConnectionToken, Lookup, OperationToken};
use crate::v2::error::Error;

use super::super::{ConnectionIoState, EstablishedIoConnection, IoState};
use super::effects::{
    AfterEngineUnlock, DetachedIoCoreEffects, IoCoreEffects, OperationQuarantineEffect,
};

impl IoState {
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
        &mut self,
        tokens: &[OperationToken],
        error: Error,
    ) -> DetachedIoCoreEffects {
        let mut after_unlock = AfterEngineUnlock::default();
        for token in tokens.iter().copied() {
            if let Lookup::Occupied(operation) = self.operations.lookup_mut(token)
                && let Some(observer) = operation.fail_observer_for_close(error.clone())
            {
                after_unlock.push_operation_wake(observer);
            }
        }
        DetachedIoCoreEffects::new(after_unlock)
    }

    pub(in crate::v2::engine) fn scan_connection_observers_for_close(
        &mut self,
        connection: ConnectionToken,
        start: usize,
        error: Error,
        scan_budget: usize,
    ) -> (DetachedIoCoreEffects, usize, bool) {
        let (operations, next, complete, _) =
            self.operations.scan_occupied_tokens(start, scan_budget);
        let mut after_unlock = AfterEngineUnlock::default();
        for token in operations {
            if let Lookup::Occupied(operation) = self.operations.lookup_mut(token)
                && operation.connection_token() == connection
                && let Some(observer) = operation.fail_observer_for_close(error.clone())
            {
                after_unlock.push_operation_wake(observer);
            }
        }
        (DetachedIoCoreEffects::new(after_unlock), next, complete)
    }

    pub(in crate::v2::engine) fn scan_connection_quarantine(
        &mut self,
        connection: ConnectionToken,
        connection_io: &mut ConnectionIoState,
        start: usize,
        scan_budget: usize,
    ) -> (IoCoreEffects, usize, bool) {
        let (operations, next, complete, _) =
            self.operations.scan_occupied_tokens(start, scan_budget);
        let mut effects = IoCoreEffects::default();
        for token in operations {
            if matches!(
                self.operations.lookup(token),
                Lookup::Occupied(operation) if operation.connection_token() == connection
            ) {
                effects.extend(self.quarantine_operation(token, connection_io));
            }
        }
        (effects, next, complete)
    }

    pub(in crate::v2::engine) fn terminalize_operations(
        &mut self,
        outcome: &MemoizedTerminalResult,
    ) -> IoCoreEffects {
        let mut effects = IoCoreEffects::default();
        if !outcome.is_error() {
            return effects;
        }
        for token in self.operations.occupied_tokens() {
            let Lookup::Occupied(operation) = self.operations.lookup_mut(token) else {
                continue;
            };
            let terminalized = operation.finalize_terminal(outcome);
            let mr_len = operation.mr_len();
            let connection = operation.connection_token();
            debug_assert!(
                !terminalized.was_reclaiming || terminalized.newly_quarantined,
                "terminal reclamation must transfer its retained MR and CQ debt to quarantine"
            );
            if terminalized.was_reclaiming {
                self.pending_reclamations = self.pending_reclamations.saturating_sub(1);
            }
            if terminalized.newly_quarantined {
                self.quarantined_operations += 1;
                self.quarantined_mrs += 1;
                self.quarantined_bytes += mr_len;
                self.cq_credits.retain();
                effects.push_quarantine(OperationQuarantineEffect::Added {
                    connection,
                    operation: token,
                });
            }
            if let Some(observer) = terminalized.observer {
                effects.push_operation_wake(observer);
            }
        }
        effects
    }

    pub(in crate::v2::engine) fn terminalize_operations_bounded(
        &mut self,
        outcome: &MemoizedTerminalResult,
        cursor: usize,
        budget: usize,
    ) -> (IoCoreEffects, usize, bool, usize) {
        let mut effects = IoCoreEffects::default();
        if !outcome.is_error() {
            return (effects, cursor, true, 0);
        }
        let (operations, next, complete, scanned) =
            self.operations.scan_occupied_tokens(cursor, budget);
        for token in operations {
            let Lookup::Occupied(operation) = self.operations.lookup_mut(token) else {
                continue;
            };
            let terminalized = operation.finalize_terminal(outcome);
            let mr_len = operation.mr_len();
            let connection = operation.connection_token();
            debug_assert!(
                !terminalized.was_reclaiming || terminalized.newly_quarantined,
                "terminal reclamation must transfer its retained MR and CQ debt to quarantine"
            );
            if terminalized.was_reclaiming {
                self.pending_reclamations = self.pending_reclamations.saturating_sub(1);
            }
            if terminalized.newly_quarantined {
                self.quarantined_operations += 1;
                self.quarantined_mrs += 1;
                self.quarantined_bytes += mr_len;
                self.cq_credits.retain();
                effects.push_quarantine(OperationQuarantineEffect::Added {
                    connection,
                    operation: token,
                });
            }
            if let Some(observer) = terminalized.observer {
                effects.push_operation_wake(observer);
            }
        }
        (effects, next, complete, scanned)
    }

    pub(in crate::v2::engine) fn reclaim_after_qp_destroy(
        &mut self,
        destroyed_connection: ConnectionToken,
        destroyed_qp_num: u32,
        connection: &EstablishedIoConnection,
        connection_io: &mut ConnectionIoState,
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
        if !self.remove_accepted(connection, connection_io, token) {
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "QP destruction reclaim lost accepted-set membership"
            );
            return (false, IoCoreEffects::default());
        }
        let Some(mut operation) = self.operations.release(token, false) else {
            self.add_accepted(connection, connection_io, token);
            tracing::warn!(
                connection = identity.connection.encode(),
                operation = token.encode(),
                "QP destruction reclaim could not retire the operation registration"
            );
            return (false, IoCoreEffects::default());
        };
        self.release_local(connection, connection_io, operation.direction());
        self.cq_credits.release();
        let previous = self.accepted_operations;
        debug_assert!(previous > 0, "accepted operation count must be positive");
        self.accepted_operations = self.accepted_operations.saturating_sub(1);
        self.publish_io_if_drained(previous);
        let finished = operation.finish_after_qp_destroy(close_error);
        if finished.was_reclaiming {
            self.pending_reclamations = self.pending_reclamations.saturating_sub(1);
        }
        let mut effects = IoCoreEffects::default();
        effects.push_close_wake(connection.drain_notify());
        if let Some(event) = finished.event {
            effects.push_event(event);
        }
        if let Some(observer) = finished.observer {
            effects.push_operation_wake(observer);
        }
        if finished.was_quarantined {
            self.cq_credits.release_retained();
            self.quarantined_operations = self.quarantined_operations.saturating_sub(1);
            self.quarantined_mrs = self.quarantined_mrs.saturating_sub(1);
            self.quarantined_bytes = self.quarantined_bytes.saturating_sub(operation.mr_len());
            if self.clear_operation_quarantine(
                connection_io,
                operation.token(),
                operation.connection_token(),
            ) {
                effects.push_quarantine(OperationQuarantineEffect::Cleared {
                    connection: operation.connection_token(),
                    operation: operation.token(),
                });
            }
        }
        (true, effects)
    }

    pub(in crate::v2::engine) fn begin_reclamation(&mut self, token: OperationToken) {
        if let Lookup::Occupied(operation) = self.operations.lookup_mut(token) {
            operation.mark_reclaiming();
        }
    }

    pub(in crate::v2::engine) fn handle_reclamation_deadline(
        &mut self,
        token: OperationToken,
        connection_io: &mut ConnectionIoState,
    ) -> IoCoreEffects {
        self.quarantine_operation(token, connection_io)
    }

    pub(in crate::v2::engine) fn quarantine_operation(
        &mut self,
        token: OperationToken,
        connection_io: &mut ConnectionIoState,
    ) -> IoCoreEffects {
        let Lookup::Occupied(operation) = self.operations.lookup_mut(token) else {
            return IoCoreEffects::default();
        };
        let transition = operation.mark_quarantined();
        let mr_len = operation.mr_len();
        let connection = operation.connection_token();
        if !transition.newly_quarantined {
            return IoCoreEffects::default();
        }
        if transition.was_reclaiming {
            self.pending_reclamations = self.pending_reclamations.saturating_sub(1);
        }
        self.quarantined_operations += 1;
        self.quarantined_mrs += 1;
        self.quarantined_bytes += mr_len;
        self.cq_credits.retain();
        let mut effects = IoCoreEffects::default();
        debug_assert_eq!(connection_io.identity().connection, connection);
        effects.push_quarantine(OperationQuarantineEffect::Added {
            connection,
            operation: token,
        });
        effects
    }
}
