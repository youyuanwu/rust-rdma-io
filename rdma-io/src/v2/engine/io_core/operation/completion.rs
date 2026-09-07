//! Exact CQE validation, queueing, bounded dispatch, and final release.

use std::sync::Arc;
use std::sync::atomic::Ordering;

#[cfg(any(test, feature = "test-hooks"))]
use crate::v2::engine::registry::lock_unpoison;
use crate::v2::engine::registry::{ConnectionToken, LiveIoConnectionProof, Lookup, OperationToken};
use crate::wc::WorkCompletion;

use super::super::{EstablishedIoConnection, IoCore};
use super::effects::{AfterEngineUnlock, IoCoreEffects, OperationQuarantineEffect};
use super::state::{CompletionDisposition, OperationState};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum CqeReject {
    StaleConnection,
    StaleOperation,
    RetiredOperation,
    Unknown,
    Duplicate,
    WrongConnection,
    WrongQpNum,
    UnexpectedOpcode,
}

pub(in crate::v2::engine) struct PendingCompletion {
    completion: WorkCompletion,
    operation: Arc<OperationState>,
}

impl PendingCompletion {
    pub(in crate::v2::engine) fn identity(&self) -> super::super::EstablishedIoIdentity {
        self.operation.connection().identity()
    }
}

impl IoCore {
    pub(in crate::v2::engine) fn reject_cqe(&self, reason: CqeReject) {
        #[cfg(any(test, feature = "test-hooks"))]
        {
            self.rejected_cqes.fetch_add(1, Ordering::Relaxed);
            lock_unpoison(&self.rejected_cqe_reasons).push(reason);
        }
        #[cfg(not(any(test, feature = "test-hooks")))]
        let _ = reason;
    }

    pub(in crate::v2::engine) fn prepare_completion(
        &self,
        completion: WorkCompletion,
    ) -> Option<PendingCompletion> {
        let token = OperationToken::decode(completion.wr_id());
        let operation = match self.operations.lookup(token) {
            Lookup::Occupied(operation) => operation,
            Lookup::Duplicate => {
                self.reject_cqe(CqeReject::Duplicate);
                return None;
            }
            Lookup::Stale => {
                self.reject_cqe(CqeReject::StaleOperation);
                return None;
            }
            Lookup::Retired => {
                self.reject_cqe(CqeReject::RetiredOperation);
                return None;
            }
            Lookup::Unknown => {
                self.reject_cqe(CqeReject::Unknown);
                return None;
            }
        };
        Some(PendingCompletion {
            completion,
            operation,
        })
    }

    pub(in crate::v2::engine) fn enqueue_prepared_completion(
        &self,
        pending: PendingCompletion,
        live: Option<LiveIoConnectionProof>,
        connection: &Arc<EstablishedIoConnection>,
    ) -> Option<ConnectionToken> {
        let identity = pending.operation.connection().identity();
        if pending.completion.qp_num() != identity.qp_num {
            self.reject_cqe(CqeReject::WrongQpNum);
            return None;
        }
        if !live.is_some_and(|proof| proof.proves(identity.connection, identity.qp_num))
            || connection.identity() != identity
        {
            self.reject_cqe(CqeReject::WrongConnection);
            return None;
        }
        if pending.completion.is_success()
            && pending.completion.opcode() != pending.operation.expected_opcode()
        {
            self.reject_cqe(CqeReject::UnexpectedOpcode);
            return None;
        }
        if !pending.operation.mark_completion_queued() {
            self.reject_cqe(CqeReject::Duplicate);
            return None;
        }
        connection.enqueue_completion(pending.completion);
        Some(identity.connection)
    }

    pub(in crate::v2::engine) fn dispatch_connection_completions(
        &self,
        connection: &EstablishedIoConnection,
        quantum: usize,
    ) -> (usize, bool, IoCoreEffects) {
        let mut processed = 0;
        let mut effects = IoCoreEffects::default();
        while processed < quantum {
            let Some(completion) = connection.pop_completion() else {
                break;
            };
            effects.extend(self.dispatch_connection_completion(completion));
            processed += 1;
        }
        (processed, connection.has_completion_work(), effects)
    }

    fn dispatch_queued_completion(&self, completion: WorkCompletion) -> IoCoreEffects {
        let token = OperationToken::decode(completion.wr_id());
        let operation = match self.operations.lookup(token) {
            Lookup::Occupied(operation) => operation,
            Lookup::Duplicate => {
                self.reject_cqe(CqeReject::Duplicate);
                return IoCoreEffects::default();
            }
            Lookup::Stale => {
                self.reject_cqe(CqeReject::StaleOperation);
                return IoCoreEffects::default();
            }
            Lookup::Retired => {
                self.reject_cqe(CqeReject::RetiredOperation);
                return IoCoreEffects::default();
            }
            Lookup::Unknown => {
                self.reject_cqe(CqeReject::Unknown);
                return IoCoreEffects::default();
            }
        };
        match operation.record_completion(completion) {
            CompletionDisposition::Deferred => IoCoreEffects::default(),
            CompletionDisposition::Complete => self.finish_operation(operation, completion),
            CompletionDisposition::Duplicate => {
                self.reject_cqe(CqeReject::Duplicate);
                IoCoreEffects::default()
            }
        }
    }

    pub(super) fn finish_operation(
        &self,
        operation: Arc<OperationState>,
        completion: WorkCompletion,
    ) -> IoCoreEffects {
        if self.operations.release(operation.token(), true).is_none() {
            self.reject_cqe(CqeReject::Duplicate);
            return IoCoreEffects::default();
        }
        let removed = operation.connection().remove_accepted(operation.token());
        operation.connection().release_local(operation.direction());
        self.cq_credits.release();
        let previous = self.accepted_operations.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "accepted operation count must be positive");
        self.publish_io_if_drained(previous);
        let finished = operation.finish_completion(completion);
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
        if removed
            && !operation.connection().is_posting_open()
            && operation.connection().accepted_count() == 0
        {
            effects.push_drained(operation.connection_token());
        }
        effects
    }

    pub(super) fn finish_early_completion(
        &self,
        operation: Arc<OperationState>,
        completion: WorkCompletion,
    ) -> AfterEngineUnlock {
        self.finish_operation(operation, completion)
            .into_after_unlock()
    }

    fn dispatch_connection_completion(&self, completion: WorkCompletion) -> IoCoreEffects {
        // CQ routing and terminal publication share the admission barrier.
        // The guard covers one completion and drops before the caller applies
        // session effects or publishes events and operation wakes.
        let _admission = self.admission();
        self.dispatch_queued_completion(completion)
    }

    fn dispatch_queued_completions(
        &self,
        connection: &EstablishedIoConnection,
        budget: usize,
    ) -> (bool, IoCoreEffects) {
        let mut effects = IoCoreEffects::default();
        for _ in 0..budget {
            let Some(completion) = connection.pop_completion() else {
                return (false, effects);
            };
            effects.extend(self.dispatch_connection_completion(completion));
        }
        (connection.has_completion_work(), effects)
    }

    pub(in crate::v2::engine) fn reject_queued_completions_after_qp_destroy(
        &self,
        connection: &EstablishedIoConnection,
    ) -> (bool, IoCoreEffects) {
        self.dispatch_queued_completions(connection, self.completion_dispatch_budget)
    }
}
