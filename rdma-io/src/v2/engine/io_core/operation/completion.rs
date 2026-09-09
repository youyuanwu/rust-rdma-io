//! Exact CQE validation, queueing, bounded dispatch, and final release.
//!
//! A copied CQE only ever reaches an operation through this module, and it
//! does so in two separately validated steps. [`IoCore::prepare_completion`]
//! resolves the encoded `wr_id` against the generational registry, and
//! [`IoCore::enqueue_prepared_completion`] then requires the exact QP number,
//! a live-connection proof for that same connection *and* QP generation, the
//! caller-supplied connection to be the identical established identity, and —
//! for a successful status — the opcode the operation was validated to expect.
//! Every other outcome is a counted [`CqeReject`] and no ownership moves. A
//! completion is therefore never matched by `wr_id` alone: a stale, retired,
//! duplicate, unknown, wrong-connection, wrong-QP, or wrong-opcode CQE is
//! rejected before any state, credit, or MR accounting can observe it.
//!
//! `mark_completion_queued` is the single-shot admission marker: it makes the
//! queue hand-off idempotent, so a second CQE for the same operation is
//! rejected as `Duplicate` at enqueue time rather than being dispatched twice.
//!
//! Dispatch is always bounded and always runs under the shared admission
//! barrier, one completion at a time. The guard is deliberately scoped to a
//! single completion so it drops before the caller applies session effects or
//! publishes events and operation wakes; it is not held across a whole
//! quantum. `dispatch_connection_completions` serves scheduled per-connection
//! quanta, while `reject_queued_completions_after_qp_destroy` drains the
//! already-queued backlog under the configured dispatch budget after a QP is
//! destroyed. Both stop early on an empty queue and report whether completion
//! work remains, so neither can spin on an idle connection.
//!
//! [`IoCore::finish_operation`] is the one place that releases a completed
//! operation, and it releases in a fixed order: the registry slot first (a
//! failed release means another path already finished this operation, so the
//! CQE is rejected as `Duplicate` and nothing else is touched), then the
//! connection's accepted entry and local direction reservation, then the CQ
//! credit and the global accepted count. Retained debt is unwound only for
//! state the operation actually held: a reclaiming operation decrements the
//! pending-reclamation count, and a quarantined one releases its retained CQ
//! credit and quarantine MR/byte accounting and emits
//! [`OperationQuarantineEffect::Cleared`]. The user-visible event and the
//! operation wake are not published here — they are accumulated into
//! [`IoCoreEffects`] and published by the caller after the engine lock and any
//! posting guards are dropped. `finish_early_completion` is the same release
//! expressed as post-unlock work for the submission paths, which observe an
//! early CQE while still holding their posting guards.
//!
//! Dependency direction is one-way: this module uses `state`, `effects`, and
//! the parent `IoCore` accounting fields only. It does not depend on `batch`,
//! `future`, or reclamation; those callers reach completion through the
//! `pub(super)` finishing methods, which are the narrowest visibility that can
//! span the operation subtree.

use std::sync::Arc;
use std::sync::atomic::Ordering;

#[cfg(any(test, feature = "test-hooks"))]
use crate::v2::engine::registry::lock_unpoison;
use crate::v2::engine::registry::{ConnectionToken, LiveIoConnectionProof, Lookup, OperationToken};
use crate::wc::WorkCompletion;

use super::super::{EstablishedIoConnection, EstablishedIoIdentity, IoCore};
use super::effects::{AfterEngineUnlock, IoCoreEffects, OperationQuarantineEffect};
use super::state::{CompletionDisposition, OperationState};

/// Counted reason an exact CQE was refused before it could move ownership.
///
/// Every variant is a *rejection*, never a deferral: the completion is dropped
/// and the operation keeps whatever it already owned. `StaleConnection` is
/// raised by the session owner when the connection lookup fails; the remaining
/// variants are raised here.
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

/// A CQE matched to a live operation but not yet proven to belong to it.
///
/// The value only carries the copied completion and the resolved operation, and
/// both fields stay private, so the identity checks in
/// [`IoCore::enqueue_prepared_completion`] cannot be bypassed by constructing or
/// editing one. The type is intentionally absent from the parent facade: the
/// session owner holds it as an inferred temporary between the two steps.
pub(in crate::v2::engine) struct PendingCompletion {
    completion: WorkCompletion,
    operation: Arc<OperationState>,
}

impl PendingCompletion {
    /// Reports the established identity the resolved operation belongs to.
    ///
    /// This is the connection and QP the caller must look up and prove live
    /// before the completion may be queued.
    pub(in crate::v2::engine) fn identity(&self) -> EstablishedIoIdentity {
        self.operation.connection().identity()
    }
}

impl IoCore {
    /// Counts one refused CQE under test or `test-hooks` builds.
    ///
    /// Production builds keep no rejection history; the reason is consumed so
    /// the diagnostic path adds no production accounting.
    pub(in crate::v2::engine) fn reject_cqe(&self, reason: CqeReject) {
        #[cfg(any(test, feature = "test-hooks"))]
        {
            self.rejected_cqes.fetch_add(1, Ordering::Relaxed);
            lock_unpoison(&self.rejected_cqe_reasons).push(reason);
        }
        #[cfg(not(any(test, feature = "test-hooks")))]
        let _ = reason;
    }

    /// Resolves a copied CQE's `wr_id` to a currently registered operation.
    ///
    /// This is only the generational identity step. A duplicate, stale,
    /// retired, or unknown token is rejected here; the QP, connection, and
    /// opcode proofs still belong to [`Self::enqueue_prepared_completion`].
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

    /// Proves a pending CQE belongs to this exact connection and queues it.
    ///
    /// All four proofs must hold: the CQE's QP number matches the operation's
    /// identity, `live` proves that same connection *and* QP generation, the
    /// supplied connection is that identical identity, and a successful status
    /// carries the opcode the operation expects. The final
    /// `mark_completion_queued` check makes queueing single-shot, so a second
    /// CQE for the operation is refused instead of being dispatched twice.
    /// Returns the connection to make ready, or `None` if the CQE was rejected.
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

    /// Dispatches at most `quantum` queued completions for one connection.
    ///
    /// Stops early when the queue drains, and reports the completions actually
    /// processed plus whether work remains so the scheduler can requeue the
    /// connection instead of rescanning it.
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

    /// Re-resolves one dequeued completion and routes it to its operation.
    ///
    /// The token is looked up again because the operation may have been
    /// released between queueing and dispatch. `Deferred` means the operation
    /// is not finished yet (a batch still owes completions), `Complete` hands
    /// off to the single release path, and `Duplicate` is refused.
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

    /// Releases one completed operation and records its post-unlock effects.
    ///
    /// This is the only completion-side release path. The registry slot is
    /// released first and doubles as the duplicate guard; only afterwards are
    /// the accepted entry, local direction, CQ credit, and accepted count
    /// unwound, followed by any reclaiming, quarantine, and drained accounting
    /// the operation actually held. The returned effects carry the user event
    /// and operation wake for the caller to publish after unlocking.
    ///
    /// Visible to the whole operation subtree because the batch, scalar, and
    /// reclamation paths all resolve exact CQEs through it; that is the
    /// narrowest visibility Rust can express for sibling access.
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
        if removed {
            effects.push_close_wake(operation.connection().drain_notify());
        }
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

    /// Finishes an operation whose CQE arrived while posting guards are held.
    ///
    /// Identical to [`Self::finish_operation`], but converts the effects into
    /// post-unlock work so the submission paths publish only after their
    /// admission and posting guards are dropped.
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

    /// Drains up to `budget` queued completions after QP destruction.
    ///
    /// Unlike a scheduled quantum this is a bounded backlog drain, so an empty
    /// queue reports no remaining work directly instead of re-reading the
    /// connection's readiness.
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

    /// Routes the completions already queued when the QP was destroyed.
    ///
    /// The backlog is still exact: each entry was proven against the live
    /// connection before queueing, so it is dispatched normally under the
    /// configured dispatch budget rather than discarded.
    pub(in crate::v2::engine) fn reject_queued_completions_after_qp_destroy(
        &self,
        connection: &EstablishedIoConnection,
        action_limited_budget: usize,
    ) -> (bool, IoCoreEffects) {
        self.dispatch_queued_completions(
            connection,
            self.completion_dispatch_budget.min(action_limited_budget),
        )
    }
}
