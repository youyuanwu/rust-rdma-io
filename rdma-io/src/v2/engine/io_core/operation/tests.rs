#![cfg(test)]

use std::sync::Arc;
use std::time::Duration;

use super::accounting::{CqCreditPool, OperationRegistry};
use super::batch::{
    BatchOwnershipTransfer, InternalBatchEntry, PreparedBatchOwnership, commit_internal_entries,
};
use super::state::{CompletionDisposition, OperationLifecycle, OperationState};
use crate::v2::engine::io_core::{
    CqeReject, Direction, EstablishedIoConnection, EstablishedIoIdentity, IoDriverSignal, IoState,
};
use crate::v2::engine::registry::{ConnectionToken, LiveIoConnectionProof, Lookup, OperationToken};
use crate::v2::error::{Error, Result};
use crate::v2::qp::BatchPostOutcome;
use crate::wc::{WcOpcode, WorkCompletion};
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};
use rdma_io_sys::ibverbs::{IBV_WC_RECV, IBV_WC_SEND, IBV_WC_SUCCESS};

struct TestSignal;

impl IoDriverSignal for TestSignal {
    fn publish_cq_recheck(&self) {}
    fn publish_completion_dispatch(&self) {}
    fn publish_reclamation(&self) {}
    fn pause_operation_before_register(&self) {}
}

struct TestPoster {
    qp_num: u32,
}

impl super::super::IoPostAuthority for TestPoster {
    fn qp_num(&self) -> u32 {
        self.qp_num
    }

    fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        Ok(BatchPostOutcome::AllAccepted)
    }

    fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        Ok(BatchPostOutcome::AllAccepted)
    }
}

fn core(capacity: usize) -> IoState {
    IoState::new_owned(
        capacity,
        capacity,
        Duration::ZERO,
        capacity,
        Arc::new(std::sync::RwLock::new(())),
        Arc::new(TestSignal),
    )
    .unwrap()
}

fn connection(slot: u32, generation: u32, qp_num: u32) -> Arc<EstablishedIoConnection> {
    EstablishedIoConnection::new(
        EstablishedIoIdentity {
            connection: ConnectionToken { slot, generation },
            qp_num,
        },
        Arc::new(TestPoster { qp_num }),
        8,
        8,
        Arc::new(tokio::sync::Notify::new()),
    )
}

fn install_posting(
    core: &mut IoState,
    connection: &Arc<EstablishedIoConnection>,
    direction: Direction,
    opcode: WcOpcode,
) -> OperationToken {
    core.reserve_local(connection, direction).unwrap();
    assert!(core.cq_credits.reserve());
    core.operations
        .allocate(|token| {
            OperationState::new(token, Arc::clone(connection), direction, opcode, None, 64)
        })
        .unwrap()
}

fn commit_accepted(
    core: &mut IoState,
    connection: &Arc<EstablishedIoConnection>,
    token: OperationToken,
) {
    core.add_accepted(connection, token);
    core.accepted_operations += 1;
    let Lookup::Occupied(operation) = core.operations.lookup_mut(token) else {
        panic!("test operation remains registered")
    };
    operation.commit_accepted();
}

fn install_accepted(
    core: &mut IoState,
    connection: &Arc<EstablishedIoConnection>,
    direction: Direction,
    opcode: WcOpcode,
) -> OperationToken {
    let token = install_posting(core, connection, direction, opcode);
    commit_accepted(core, connection, token);
    token
}

fn wc(token: OperationToken, qp_num: u32, opcode: u32) -> WorkCompletion {
    let mut completion = WorkCompletion::default();
    completion.inner.wr_id = token.encode();
    completion.inner.qp_num = qp_num;
    completion.inner.opcode = opcode;
    completion.inner.status = IBV_WC_SUCCESS;
    completion
}

#[test]
fn credit_pool_never_oversubscribes_or_reuses_retained_debt() {
    let mut credits = CqCreditPool::new(2);
    assert!(credits.reserve());
    assert!(credits.reserve());
    assert!(!credits.reserve());
    credits.retain();
    assert_eq!(credits.free(), 0);
    assert_eq!(credits.retained(), 1);
    credits.release();
    assert_eq!(credits.free(), 1);
    assert_eq!(credits.retained(), 1);
    credits.release_retained();
    credits.release();
    assert_eq!(credits.free(), 2);
}

#[test]
fn operation_registry_is_value_owned_generational_and_duplicate_aware() {
    let mut registry = OperationRegistry::new(1).unwrap();
    let connection = connection(1, 3, 7);
    let token = registry
        .allocate(|token| {
            OperationState::new(
                token,
                Arc::clone(&connection),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
            )
        })
        .unwrap();
    assert!(matches!(registry.lookup(token), Lookup::Occupied(_)));
    registry.release(token, true).unwrap();
    assert!(matches!(registry.lookup(token), Lookup::Duplicate));

    let reused = registry
        .allocate(|token| {
            OperationState::new(
                token,
                Arc::clone(&connection),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
            )
        })
        .unwrap();
    assert_eq!(reused.slot, token.slot);
    assert_ne!(reused.generation, token.generation);
    registry.release(reused, false).unwrap();
    assert!(matches!(registry.lookup(reused), Lookup::Stale));
}

#[test]
fn early_cqe_is_retained_until_post_acceptance_reconciliation() {
    let mut core = core(4);
    let connection = connection(1, 1, 11);
    let token = install_posting(&mut core, &connection, Direction::Send, WcOpcode::Send);
    let completion = wc(token, 11, IBV_WC_SEND);
    let Lookup::Occupied(operation) = core.operations.lookup_mut(token) else {
        panic!("posting operation")
    };
    assert!(operation.mark_completion_queued());
    assert!(matches!(
        operation.record_completion(completion),
        CompletionDisposition::Deferred
    ));
    let early = operation
        .commit_accepted()
        .early
        .expect("early CQE retained");
    core.add_accepted(&connection, token);
    core.accepted_operations += 1;

    let effects = core.finish_early_completion(token, early);
    assert_eq!(core.operations.live(), 0);
    assert_eq!(core.accepted_operations, 0);
    assert_eq!(core.connection_accepted_count(&connection), 0);
    assert_eq!(core.cq_credits.free(), 4);
    effects.publish();
}

#[test]
fn batch_early_cqe_uses_post_guard_publication_without_drained_effect() {
    let mut core = core(1);
    let connection = connection(1, 1, 15);
    let token = install_posting(&mut core, &connection, Direction::Recv, WcOpcode::Recv);
    let completion = wc(token, 15, IBV_WC_RECV);
    let Lookup::Occupied(operation) = core.operations.lookup_mut(token) else {
        panic!("posting batch operation")
    };
    assert!(operation.mark_completion_queued());
    assert!(matches!(
        operation.record_completion(completion),
        CompletionDisposition::Deferred
    ));

    let after_unlock = commit_internal_entries(
        &mut core,
        vec![InternalBatchEntry {
            token,
            sge: crate::wr::Sge::new(0, 0, 0),
        }],
    );
    after_unlock.publish();

    assert_eq!(core.operations.live(), 0);
    assert_eq!(core.accepted_operations, 0);
    assert_eq!(core.connection_accepted_count(&connection), 0);
    assert_eq!(core.cq_credits.free(), 1);
}

#[test]
fn operation_cancellation_is_deduplicated_and_schedules_one_deadline() {
    let mut core = core(2);
    let connection = connection(1, 1, 12);
    let token = install_accepted(&mut core, &connection, Direction::Send, WcOpcode::Send);

    core.cancel_operation(token);
    core.cancel_operation(token);

    assert_eq!(core.pending_reclamations, 1);
    assert_eq!(core.reclamation_request_count(), 1);
    let Lookup::Occupied(operation) = core.operations.lookup(token) else {
        panic!("cancelled operation remains registered")
    };
    assert_eq!(operation.lifecycle(), OperationLifecycle::Reclaiming);
}

#[test]
fn cancellation_observed_during_provider_post_commits_then_reclaims_once() {
    let mut core = core(1);
    let connection = connection(1, 1, 13);
    core.reserve_local(&connection, Direction::Send).unwrap();
    assert!(core.cq_credits.reserve());
    let observer = super::state::OperationObserver::new();
    let token = core
        .operations
        .allocate(|token| {
            OperationState::new_scalar(
                token,
                Arc::clone(&connection),
                Direction::Send,
                WcOpcode::Send,
                None,
                1,
                Arc::clone(&observer),
            )
        })
        .unwrap();

    // This is the state reached when the frontend is dropped while the sole
    // reactor thread is inside the provider post call.
    observer.cancel();
    core.add_accepted(&connection, token);
    core.accepted_operations += 1;
    let Lookup::Occupied(operation) = core.operations.lookup_mut(token) else {
        panic!("posting operation remains registered")
    };
    let committed = operation.commit_accepted();
    assert!(committed.cancellation_needs_reclamation);
    core.pending_reclamations += 1;
    core.schedule_reclamation(token);

    assert_eq!(core.pending_reclamations, 1);
    assert_eq!(core.reclamation_request_count(), 1);
    let Lookup::Occupied(operation) = core.operations.lookup(token) else {
        panic!("cancelled accepted operation remains registered")
    };
    assert_eq!(operation.lifecycle(), OperationLifecycle::Reclaiming);
    assert_eq!(core.accepted_operations, 1);
    assert_eq!(core.cq_credits.free(), 0);
}

#[test]
fn exact_prefix_and_ambiguous_batch_reconciliation_preserve_whole_ownership() {
    let exact = PreparedBatchOwnership::new(vec![1, 2, 3]).unwrap().consume(
        BatchPostOutcome::PrefixAccepted {
            accepted: 1,
            first_unaccepted: 1,
            source: std::io::Error::from_raw_os_error(libc::ENOMEM),
        },
    );
    let BatchOwnershipTransfer::Partial {
        accepted,
        unaccepted,
        ..
    } = exact
    else {
        panic!("exact provider prefix must split ownership")
    };
    assert_eq!(accepted, vec![1]);
    assert_eq!(unaccepted, vec![2, 3]);

    let ambiguous =
        PreparedBatchOwnership::new(vec![1, 2, 3])
            .unwrap()
            .consume(BatchPostOutcome::Ambiguous {
                source: std::io::Error::from_raw_os_error(libc::EIO),
            });
    let BatchOwnershipTransfer::Ambiguous { retained, .. } = ambiguous else {
        panic!("ambiguous provider result must retain the complete batch")
    };
    assert_eq!(retained, vec![1, 2, 3]);

    let mut registry = OperationRegistry::new(2).unwrap();
    let connection = connection(2, 1, 14);
    let first = registry
        .allocate(|token| {
            OperationState::new(
                token,
                Arc::clone(&connection),
                Direction::Recv,
                WcOpcode::Recv,
                None,
                1,
            )
        })
        .unwrap();
    let second = registry
        .allocate(|token| {
            OperationState::new(
                token,
                Arc::clone(&connection),
                Direction::Recv,
                WcOpcode::Recv,
                None,
                1,
            )
        })
        .unwrap();
    let completion = wc(second, 14, IBV_WC_RECV);
    let Lookup::Occupied(operation) = registry.lookup_mut(second) else {
        panic!("second suffix operation")
    };
    assert!(operation.mark_completion_queued());
    assert!(matches!(
        operation.record_completion(completion),
        CompletionDisposition::Deferred
    ));
    assert!(
        registry
            .take_proven_unaccepted_batch(
                &[first, second],
                Error::PostFailed(std::io::Error::from_raw_os_error(libc::ENOMEM)),
            )
            .is_none(),
        "one early CQE retains the entire ambiguous suffix transaction"
    );
    assert_eq!(registry.live(), 2);
}

#[test]
fn exact_cqe_validation_rejects_wrong_qp_opcode_and_duplicate() {
    let mut core = core(4);
    let connection = connection(2, 9, 17);
    let token = install_accepted(&mut core, &connection, Direction::Send, WcOpcode::Send);
    let live = LiveIoConnectionProof::for_test(connection.identity());

    let wrong_qp = core
        .prepare_completion(wc(token, 18, IBV_WC_SEND))
        .expect("live token resolves");
    assert!(
        core.enqueue_prepared_completion(wrong_qp, Some(live), &connection)
            .is_none()
    );
    let wrong_opcode = core
        .prepare_completion(wc(token, 17, IBV_WC_RECV))
        .expect("live token resolves");
    assert!(
        core.enqueue_prepared_completion(wrong_opcode, Some(live), &connection)
            .is_none()
    );

    let exact = core
        .prepare_completion(wc(token, 17, IBV_WC_SEND))
        .expect("exact token resolves");
    assert_eq!(
        core.enqueue_prepared_completion(exact, Some(live), &connection),
        Some(connection.identity().connection)
    );
    let duplicate = core
        .prepare_completion(wc(token, 17, IBV_WC_SEND))
        .expect("registered token still resolves before dispatch");
    assert!(
        core.enqueue_prepared_completion(duplicate, Some(live), &connection)
            .is_none()
    );

    let (processed, ready, _) = core.dispatch_connection_completions(&connection, 1);
    assert_eq!((processed, ready), (1, false));
    assert_eq!(core.operations.live(), 0);
    assert_eq!(
        core.rejected_cqe_reasons(),
        vec![
            CqeReject::WrongQpNum,
            CqeReject::UnexpectedOpcode,
            CqeReject::Duplicate,
        ]
    );
}

#[test]
fn reclamation_deadline_quarantines_and_late_cqe_clears_exact_debt() {
    let mut core = core(2);
    let connection = connection(3, 1, 19);
    let token = install_accepted(&mut core, &connection, Direction::Recv, WcOpcode::Recv);
    core.cancel_operation(token);
    let added = core.handle_reclamation_deadline(token);
    assert!(core.operation_quarantined_for_test(token));
    assert_eq!(core.pending_reclamations, 0);
    assert_eq!(core.quarantined_operations, 1);
    assert_eq!(core.quarantined_mrs, 1);
    assert_eq!(core.quarantined_bytes, 64);
    assert_eq!(core.cq_credits.retained(), 1);
    drop(added);

    let live = LiveIoConnectionProof::for_test(connection.identity());
    let pending = core
        .prepare_completion(wc(token, 19, IBV_WC_RECV))
        .expect("quarantined operation still resolves");
    core.enqueue_prepared_completion(pending, Some(live), &connection)
        .expect("exact late CQE");
    let (_, _, cleared) = core.dispatch_connection_completions(&connection, 1);
    drop(cleared);

    assert_eq!(core.quarantined_operations, 0);
    assert_eq!(core.quarantined_mrs, 0);
    assert_eq!(core.quarantined_bytes, 0);
    assert_eq!(core.cq_credits.retained(), 0);
    assert_eq!(core.cq_credits.free(), 2);
    assert!(!core.operation_quarantined_for_test(token));
}

#[test]
fn qp_destruction_release_requires_exact_connection_and_qp() {
    let mut core = core(2);
    let connection = connection(4, 2, 23);
    let token = install_accepted(&mut core, &connection, Direction::Send, WcOpcode::Send);

    assert!(
        !core
            .reclaim_after_qp_destroy(
                ConnectionToken {
                    slot: 4,
                    generation: 2,
                },
                24,
                &connection,
                Error::TransportClosed,
                token,
            )
            .0
    );
    assert_eq!(core.operations.live(), 1);

    let (released, effects) = core.reclaim_after_qp_destroy(
        connection.identity().connection,
        connection.identity().qp_num,
        &connection,
        Error::TransportClosed,
        token,
    );
    assert!(released);
    drop(effects);
    assert_eq!(core.operations.live(), 0);
    assert_eq!(core.accepted_operations, 0);
    assert_eq!(core.connection_accepted_count(&connection), 0);
    assert_eq!(core.cq_credits.free(), 2);
}

#[test]
fn terminalization_is_bounded_and_preserves_provider_ownership_in_quarantine() {
    let mut core = core(3);
    let connection = connection(5, 1, 29);
    for _ in 0..3 {
        install_accepted(&mut core, &connection, Direction::Send, WcOpcode::Send);
    }
    let outcome =
        crate::v2::engine::lifecycle::MemoizedTerminalResult::from_error(Error::DriverShutdown);

    let (_, next, complete, scanned) = core.terminalize_operations_bounded(&outcome, 0, 2);
    assert_eq!(scanned, 2);
    assert!(!complete);
    assert_eq!(core.quarantined_operations, 2);
    let (_, _, complete, scanned) = core.terminalize_operations_bounded(&outcome, next, 2);
    assert_eq!(scanned, 1);
    assert!(complete);
    assert_eq!(core.quarantined_operations, 3);
    assert_eq!(core.operations.live(), 3);
    assert_eq!(core.accepted_operations, 3);
    assert_eq!(core.cq_credits.free(), 0);
    assert_eq!(core.cq_credits.retained(), 3);
    for token in core.operations.occupied_tokens() {
        assert!(core.operation_quarantined_for_test(token));
    }
}

#[test]
fn retiring_connection_io_requires_empty_ledgers_and_clears_publication_key() {
    let mut core = core(1);
    let connection = connection(6, 4, 31);
    core.reserve_local(&connection, Direction::Send).unwrap();
    core.release_local(&connection, Direction::Send);
    core.publish_connection(&connection);
    assert!(core.has_published_connections());

    core.retire_connection_io(connection.identity().connection);

    assert!(!core.has_published_connections());
    assert_eq!(core.connection_accepted_count(&connection), 0);
}
