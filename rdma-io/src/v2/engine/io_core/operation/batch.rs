//! Protocol-owned SEND/RECV preparation and provider-acceptance reconciliation.
//!
//! `post_io_batch` is one transaction: it validates and reserves every entry,
//! prepares stable work requests, posts once, and then reconciles what the
//! provider actually accepted. The accepted, exact-prefix, early-CQE,
//! proven-unaccepted, and ambiguous outcomes stay in that single flow because
//! each one assigns a different owner to the same reservations; separating them
//! would let MR, registry, local-direction, and CQ-credit accounting drift.
//!
//! The module observes three ownership rules:
//!
//! - It never sees `OperationInner` or a state guard. Every transition is a
//!   method on `OperationState` that returns an owned record.
//! - It never reads or builds effect payload fields. Post-lock work accumulates
//!   in `AfterEngineUnlock` and publishes only after the posting and admission
//!   guards are dropped.
//! - It releases provider-visible ownership only on positive proof of
//!   non-acceptance; anything ambiguous — including a suffix that already
//!   observed a completion — is retained as accepted and left to reclamation.
//!
//! Submission validation lives in the sibling `validation` module so the scalar
//! future shares it without either submission path depending on the other.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use crate::v2::engine::io::{
    IoEventDestination, IoEventSender, IoOperationContext, IoRecvRequest, IoSendRequest,
    IoSubmissionDisposition,
};
use crate::v2::engine::registry::OperationToken;
use crate::v2::error::{Error, Result};
use crate::v2::mr::Mr;
use crate::v2::qp::BatchPostOutcome;
use crate::wc::WcOpcode;
use crate::wr::{PreparedRecvBatch, PreparedSendBatch, RecvWr, SendFlags, SendWr, Sge, WrOpcode};

use super::super::{Direction, EstablishedIoConnection, IoCore, OperationKind};
use super::effects::AfterEngineUnlock;
use super::state::OperationState;
use super::validation::ValidatedOperation;

struct InternalBatchEntry {
    token: OperationToken,
    state: Arc<OperationState>,
    sge: Sge,
}

type InternalPostInput = (Mr, Option<(usize, usize)>, IoOperationContext);

pub(in crate::v2::engine) fn post_io_recv_batch(
    shared: &IoCore,
    connection: &Arc<EstablishedIoConnection>,
    events: &IoEventSender,
    requests: Vec<IoRecvRequest>,
) -> IoSubmissionDisposition {
    post_io_batch(
        shared,
        connection,
        events,
        OperationKind::Recv,
        requests
            .into_iter()
            .map(|request| {
                let (mr, context) = request.into_parts();
                (mr, None, context)
            })
            .collect(),
    )
}

pub(in crate::v2::engine) fn post_io_send(
    shared: &IoCore,
    connection: &Arc<EstablishedIoConnection>,
    events: &IoEventSender,
    request: IoSendRequest,
) -> IoSubmissionDisposition {
    let (mr, len, context) = request.into_parts();
    post_io_batch(
        shared,
        connection,
        events,
        OperationKind::Send,
        vec![(mr, Some((0, len)), context)],
    )
}

fn post_io_batch(
    shared: &IoCore,
    connection: &Arc<EstablishedIoConnection>,
    events: &IoEventSender,
    kind: OperationKind,
    entries: Vec<InternalPostInput>,
) -> IoSubmissionDisposition {
    if entries.is_empty() {
        return IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: 0,
            error: Error::InvalidConfig("I/O operation batch must not be empty".into()),
        };
    }
    let count = entries.len();
    let admission = shared.admission();
    if let Some(error) = shared.admission_error() {
        let after_unlock = detach_unreserved_entries(events, entries, error.clone());
        drop(admission);
        after_unlock.publish();
        return IoSubmissionDisposition::FullyUnaccepted {
            proven_unaccepted: count,
            error,
        };
    }
    let posting = match connection.begin_posting() {
        Ok(posting) => posting,
        Err(error) => {
            let after_unlock = detach_unreserved_entries(events, entries, error.clone());
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
    };
    let direction = kind.direction();
    let expected_opcode = match kind {
        OperationKind::Recv => WcOpcode::Recv,
        OperationKind::Send => WcOpcode::Send,
        OperationKind::Write | OperationKind::Read => {
            let error = Error::InvalidConfig("I/O batches support only SEND and RECV".into());
            let after_unlock = detach_unreserved_entries(events, entries, error.clone());
            drop(posting);
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
    };
    let mut reserved = Vec::with_capacity(count);
    let mut entries = entries.into_iter();
    while let Some((mr, range, context)) = entries.next() {
        let validated = match ValidatedOperation::new(kind, &mr, None, range) {
            Ok(validated) => validated,
            Err(error) => {
                let mut after_unlock = rollback_internal_entries(
                    shared,
                    connection,
                    direction,
                    reserved,
                    error.clone(),
                );
                after_unlock.push_event(
                    IoEventDestination::new(events.clone(), context).unaccepted(
                        None,
                        error.clone(),
                        mr,
                    ),
                );
                after_unlock.extend(detach_unreserved_entries(events, entries, error.clone()));
                drop(posting);
                drop(admission);
                after_unlock.publish();
                return IoSubmissionDisposition::FullyUnaccepted {
                    proven_unaccepted: count,
                    error,
                };
            }
        };
        if let Err(error) = connection.reserve_local(direction) {
            let mut after_unlock =
                rollback_internal_entries(shared, connection, direction, reserved, error.clone());
            after_unlock.push_event(IoEventDestination::new(events.clone(), context).unaccepted(
                None,
                error.clone(),
                mr,
            ));
            after_unlock.extend(detach_unreserved_entries(events, entries, error.clone()));
            drop(posting);
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
        let mr_len = mr.len();
        let mut mr = Some(mr);
        let mut destination = Some(IoEventDestination::new(events.clone(), context));
        let (token, state) = match shared.operations.allocate(|token| {
            Arc::new(OperationState::new_with_event(
                token,
                Arc::clone(connection),
                direction,
                expected_opcode,
                mr.take(),
                mr_len,
                destination.take(),
            ))
        }) {
            Ok(allocated) => allocated,
            Err(error) => {
                connection.release_local(direction);
                let mr = mr
                    .take()
                    .expect("operation allocation failure retains I/O MR");
                let destination = destination
                    .take()
                    .expect("operation allocation failure retains I/O destination");
                let mut after_unlock = rollback_internal_entries(
                    shared,
                    connection,
                    direction,
                    reserved,
                    error.clone(),
                );
                after_unlock.push_event(destination.unaccepted(None, error.clone(), mr));
                after_unlock.extend(detach_unreserved_entries(events, entries, error.clone()));
                drop(posting);
                drop(admission);
                after_unlock.publish();
                return IoSubmissionDisposition::FullyUnaccepted {
                    proven_unaccepted: count,
                    error,
                };
            }
        };
        if !shared.cq_credits.reserve() {
            let error = Error::CapacityExhausted;
            let release = state
                .take_unaccepted(error.clone())
                .expect("an operation rejected before posting has no completion");
            let registered = shared
                .operations
                .release(token, false)
                .expect("unposted operation remains registered");
            debug_assert!(Arc::ptr_eq(&registered, &state));
            connection.release_local(direction);
            let mut after_unlock =
                rollback_internal_entries(shared, connection, direction, reserved, error.clone());
            if let Some(event) = release.event {
                after_unlock.push_event(event);
            }
            drop(release.mr);
            after_unlock.extend(detach_unreserved_entries(events, entries, error.clone()));
            drop(posting);
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
        reserved.push(InternalBatchEntry {
            token,
            state,
            sge: validated.sge(),
        });
    }

    let requests = match kind {
        OperationKind::Recv => {
            let requests = reserved
                .iter()
                .map(|entry| RecvWr::new(entry.token.encode()).sg(entry.sge))
                .collect();
            match PreparedRecvBatch::new(requests) {
                Ok(batch) => InternalPreparedBatch::Recv(batch),
                Err(error) => {
                    let error = Error::from_v1(error);
                    let after_unlock = rollback_internal_entries(
                        shared,
                        connection,
                        direction,
                        reserved,
                        error.clone(),
                    );
                    drop(posting);
                    drop(admission);
                    after_unlock.publish();
                    return IoSubmissionDisposition::FullyUnaccepted {
                        proven_unaccepted: count,
                        error,
                    };
                }
            }
        }
        OperationKind::Send => {
            let requests = reserved
                .iter()
                .map(|entry| {
                    SendWr::new(entry.token.encode(), WrOpcode::Send)
                        .sg(entry.sge)
                        .flags(SendFlags::SIGNALED)
                })
                .collect();
            match PreparedSendBatch::new(requests) {
                Ok(batch) => InternalPreparedBatch::Send(batch),
                Err(error) => {
                    let error = Error::from_v1(error);
                    let after_unlock = rollback_internal_entries(
                        shared,
                        connection,
                        direction,
                        reserved,
                        error.clone(),
                    );
                    drop(posting);
                    drop(admission);
                    after_unlock.publish();
                    return IoSubmissionDisposition::FullyUnaccepted {
                        proven_unaccepted: count,
                        error,
                    };
                }
            }
        }
        OperationKind::Write | OperationKind::Read => unreachable!(),
    };
    let ownership =
        PreparedBatchOwnership::new(reserved).expect("non-empty detached batch ownership");
    let mut requests = requests;
    let outcome = match match &mut requests {
        InternalPreparedBatch::Recv(batch) => connection.post_recv(batch),
        InternalPreparedBatch::Send(batch) => connection.post_send(batch),
    } {
        Ok(outcome) => outcome,
        Err(error) => {
            let entries = ownership.into_entries();
            let after_unlock =
                rollback_internal_entries(shared, connection, direction, entries, error.clone());
            drop(posting);
            drop(admission);
            after_unlock.publish();
            return IoSubmissionDisposition::FullyUnaccepted {
                proven_unaccepted: count,
                error,
            };
        }
    };
    let transfer = ownership.consume(outcome);
    match transfer {
        BatchOwnershipTransfer::Accepted(accepted) => {
            let after_unlock = commit_internal_entries(shared, accepted);
            drop(posting);
            drop(admission);
            after_unlock.publish();
            IoSubmissionDisposition::AllAccepted { accepted: count }
        }
        BatchOwnershipTransfer::Partial {
            mut accepted,
            unaccepted,
            source,
        } => {
            let error = Error::PostFailed(clone_io_error(&source));
            let accepted_count = accepted.len();
            let unaccepted_count = unaccepted.len();
            match release_proven_unaccepted_entries(
                shared, connection, direction, unaccepted, error,
            ) {
                InternalRelease::Released(mut after_unlock) => {
                    after_unlock.extend(commit_internal_entries(shared, accepted));
                    drop(posting);
                    drop(admission);
                    after_unlock.publish();
                    let error = Error::PostFailed(source);
                    if accepted_count == 0 {
                        IoSubmissionDisposition::FullyUnaccepted {
                            proven_unaccepted: unaccepted_count,
                            error,
                        }
                    } else {
                        IoSubmissionDisposition::ExactPrefix {
                            accepted: accepted_count,
                            proven_unaccepted: unaccepted_count,
                            error,
                        }
                    }
                }
                InternalRelease::Retained(mut unaccepted) => {
                    accepted.append(&mut unaccepted);
                    let after_unlock = commit_internal_entries(shared, accepted);
                    drop(posting);
                    drop(admission);
                    after_unlock.publish();
                    IoSubmissionDisposition::RetainedAfterEarlyCompletion {
                        retained: count,
                        error: Error::PostFailed(source),
                    }
                }
            }
        }
        BatchOwnershipTransfer::Ambiguous { retained, source } => {
            let retained_count = retained.len();
            let after_unlock = commit_internal_entries(shared, retained);
            drop(posting);
            drop(admission);
            after_unlock.publish();
            IoSubmissionDisposition::RetainedAmbiguous {
                retained: retained_count,
                error: Error::PostFailed(source),
            }
        }
    }
}

enum InternalPreparedBatch {
    Recv(PreparedRecvBatch),
    Send(PreparedSendBatch),
}

fn commit_internal_entries(shared: &IoCore, entries: Vec<InternalBatchEntry>) -> AfterEngineUnlock {
    shared
        .accepted_operations
        .fetch_add(entries.len(), Ordering::AcqRel);
    let mut early = Vec::new();
    for entry in entries {
        if let Some(completion) = entry.state.commit_accepted() {
            early.push((entry.state, completion));
        }
    }
    shared.publish_cq_recheck();
    let mut after_unlock = AfterEngineUnlock::default();
    for (state, completion) in early {
        let effects = shared.finish_operation(state, completion);
        after_unlock.extend(effects.into_after_unlock());
    }
    after_unlock
}

fn rollback_internal_entries(
    shared: &IoCore,
    connection: &impl EstablishedIoRef,
    direction: Direction,
    entries: Vec<InternalBatchEntry>,
    error: Error,
) -> AfterEngineUnlock {
    match release_proven_unaccepted_entries(shared, connection, direction, entries, error) {
        InternalRelease::Released(after_unlock) => after_unlock,
        InternalRelease::Retained(entries) => {
            debug_assert!(
                entries.is_empty(),
                "an operation known not to have reached the provider acquired a completion"
            );
            commit_internal_entries(shared, entries)
        }
    }
}

enum InternalRelease {
    Released(AfterEngineUnlock),
    Retained(Vec<InternalBatchEntry>),
}

/// Ownership abstraction over the established I/O connection being posted to.
///
/// Production callers always pass the real `EstablishedIoConnection`; the
/// `cfg(test)` impl lets owner-local fixtures drive rollback and release with a
/// concrete `ConnectionState` without adding a session dependency to production
/// operation code.
trait EstablishedIoRef {
    fn established_io(&self) -> &EstablishedIoConnection;
}

impl EstablishedIoRef for EstablishedIoConnection {
    fn established_io(&self) -> &EstablishedIoConnection {
        self
    }
}

impl EstablishedIoRef for Arc<EstablishedIoConnection> {
    fn established_io(&self) -> &EstablishedIoConnection {
        self
    }
}

#[cfg(test)]
impl EstablishedIoRef for Arc<crate::v2::engine::session::connection::ConnectionState> {
    fn established_io(&self) -> &EstablishedIoConnection {
        &self.io
    }
}

fn release_proven_unaccepted_entries(
    shared: &IoCore,
    connection: &impl EstablishedIoRef,
    direction: Direction,
    entries: Vec<InternalBatchEntry>,
    error: Error,
) -> InternalRelease {
    let states = entries
        .iter()
        .map(|entry| Arc::clone(&entry.state))
        .collect::<Vec<_>>();
    let releases = OperationState::take_proven_unaccepted_batch(&states, error);
    let Some(releases) = releases else {
        return InternalRelease::Retained(entries);
    };

    let mut after_unlock = AfterEngineUnlock::default();
    for (entry, release) in entries.into_iter().zip(releases) {
        let registered = shared
            .operations
            .release(entry.token, false)
            .expect("proven-unaccepted operation remains registered");
        debug_assert!(Arc::ptr_eq(&registered, &entry.state));
        shared.cq_credits.release();
        connection.established_io().release_local(direction);
        if let Some(event) = release.event {
            after_unlock.push_event(event);
        }
        drop(release.mr);
    }
    InternalRelease::Released(after_unlock)
}

fn detach_unreserved_entries(
    events: &IoEventSender,
    entries: impl IntoIterator<Item = InternalPostInput>,
    error: Error,
) -> AfterEngineUnlock {
    let events = entries
        .into_iter()
        .map(|(mr, _, context)| {
            IoEventDestination::new(events.clone(), context).unaccepted(None, error.clone(), mr)
        })
        .collect();
    AfterEngineUnlock::from_events(events)
}

fn clone_io_error(error: &std::io::Error) -> std::io::Error {
    match error.raw_os_error() {
        Some(code) => std::io::Error::from_raw_os_error(code),
        None => std::io::Error::new(error.kind(), error.to_string()),
    }
}

/// `cfg(test)` mirrors of this module's private reservation vocabulary.
///
/// `operation::tests` reaches submission internals through `use super::*`, but
/// the production entry and release types keep private fields so that no
/// sibling can assemble or inspect reservations directly. These mirrors carry
/// the same fields with operation-subtree visibility and delegate straight to
/// the production functions, so tests exercise the real reconciliation without
/// widening production visibility.
/// Take-once ownership ledger paired with stable raw batch storage.
pub(super) struct PreparedBatchOwnership<T> {
    entries: Vec<T>,
}

pub(super) enum BatchOwnershipTransfer<T> {
    Accepted(Vec<T>),
    Partial {
        accepted: Vec<T>,
        unaccepted: Vec<T>,
        source: std::io::Error,
    },
    Ambiguous {
        retained: Vec<T>,
        source: std::io::Error,
    },
}

impl<T> PreparedBatchOwnership<T> {
    pub(super) fn new(entries: Vec<T>) -> Result<Self> {
        if entries.is_empty() {
            return Err(Error::InvalidConfig(
                "batch ownership ledger must not be empty".into(),
            ));
        }
        Ok(Self { entries })
    }

    pub(super) fn consume(mut self, outcome: BatchPostOutcome) -> BatchOwnershipTransfer<T> {
        match outcome {
            BatchPostOutcome::AllAccepted => BatchOwnershipTransfer::Accepted(self.entries),
            BatchPostOutcome::PrefixAccepted {
                accepted,
                first_unaccepted,
                source,
            } if accepted == first_unaccepted && accepted <= self.entries.len() => {
                let unaccepted = self.entries.split_off(accepted);
                BatchOwnershipTransfer::Partial {
                    accepted: self.entries,
                    unaccepted,
                    source,
                }
            }
            BatchPostOutcome::PrefixAccepted { source, .. }
            | BatchPostOutcome::Ambiguous { source } => BatchOwnershipTransfer::Ambiguous {
                retained: self.entries,
                source,
            },
        }
    }

    fn into_entries(self) -> Vec<T> {
        self.entries
    }
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;

    pub(in crate::v2::engine::io_core::operation) struct InternalBatchEntry {
        pub(in crate::v2::engine::io_core::operation) token: OperationToken,
        pub(in crate::v2::engine::io_core::operation) state: Arc<OperationState>,
        pub(in crate::v2::engine::io_core::operation) sge: Sge,
    }

    pub(in crate::v2::engine::io_core::operation) enum InternalRelease {
        Released(AfterEngineUnlock),
        Retained(Vec<InternalBatchEntry>),
    }

    pub(in crate::v2::engine::io_core::operation) fn release_proven_unaccepted_entries(
        shared: &IoCore,
        connection: &Arc<crate::v2::engine::session::connection::ConnectionState>,
        direction: Direction,
        entries: Vec<InternalBatchEntry>,
        error: Error,
    ) -> InternalRelease {
        let entries = entries
            .into_iter()
            .map(|entry| super::InternalBatchEntry {
                token: entry.token,
                state: entry.state,
                sge: entry.sge,
            })
            .collect();
        match super::release_proven_unaccepted_entries(
            shared, connection, direction, entries, error,
        ) {
            super::InternalRelease::Released(after_unlock) => {
                InternalRelease::Released(after_unlock)
            }
            super::InternalRelease::Retained(entries) => InternalRelease::Retained(
                entries
                    .into_iter()
                    .map(|entry| InternalBatchEntry {
                        token: entry.token,
                        state: entry.state,
                        sge: entry.sge,
                    })
                    .collect(),
            ),
        }
    }

    pub(in crate::v2::engine::io_core::operation) fn commit_internal_entries(
        shared: &IoCore,
        entries: Vec<InternalBatchEntry>,
    ) -> AfterEngineUnlock {
        super::commit_internal_entries(
            shared,
            entries
                .into_iter()
                .map(|entry| super::InternalBatchEntry {
                    token: entry.token,
                    state: entry.state,
                    sge: entry.sge,
                })
                .collect(),
        )
    }
}
