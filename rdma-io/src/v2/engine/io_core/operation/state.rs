use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Waker;

use futures_util::task::AtomicWaker;

use crate::v2::engine::io::{IoEventDestination, IoOperationIdentity, PendingIoEvent};
use crate::v2::engine::lifecycle::MemoizedTerminalResult;
use crate::v2::engine::registry::{ConnectionToken, OperationToken, lock_unpoison};
use crate::v2::error::{Error, Result};
use crate::v2::mr::Mr;
use crate::v2::op::Completion;
use crate::wc::{WcOpcode, WorkCompletion};

use super::super::{Direction, EstablishedIoConnection, IoCore};

pub(super) struct OperationState {
    token: OperationToken,
    connection: Arc<EstablishedIoConnection>,
    direction: Direction,
    expected_opcode: WcOpcode,
    mr_len: usize,
    inner: Mutex<OperationInner>,
    waker: AtomicWaker,
    cancelled: AtomicBool,
    quarantined: AtomicBool,
}

struct OperationInner {
    lifecycle: OperationLifecycle,
    mr: Option<Mr>,
    completion: CompletionOwnership,
    output: Option<(Result<Completion>, Option<Mr>)>,
    detached: bool,
    reclamation_pending: bool,
    event_destination: Option<IoEventDestination>,
}

enum CompletionOwnership {
    None,
    // A validated CQE is owned by the connection dispatch queue.
    Queued,
    // Dispatch consumed that CQE before post reconciliation committed the WR.
    Early(WorkCompletion),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum OperationLifecycle {
    Posting,
    InFlight,
    Completing,
    Cancelled,
    Reclaiming,
    Quarantined,
    Released,
}

pub(super) trait IntoEstablishedIoConnection {
    fn into_established_io(self) -> Arc<EstablishedIoConnection>;
}

impl IntoEstablishedIoConnection for Arc<EstablishedIoConnection> {
    fn into_established_io(self) -> Arc<EstablishedIoConnection> {
        self
    }
}

#[cfg(test)]
impl IntoEstablishedIoConnection for Arc<crate::v2::engine::session::connection::ConnectionState> {
    fn into_established_io(self) -> Arc<EstablishedIoConnection> {
        Arc::clone(&self.io)
    }
}

impl OperationState {
    pub(super) fn token(&self) -> OperationToken {
        self.token
    }

    pub(super) fn connection_token(&self) -> ConnectionToken {
        self.connection.identity().connection
    }

    pub(super) fn connection(&self) -> &EstablishedIoConnection {
        &self.connection
    }

    pub(super) fn direction(&self) -> Direction {
        self.direction
    }

    pub(super) fn expected_opcode(&self) -> WcOpcode {
        self.expected_opcode
    }

    pub(super) fn mr_len(&self) -> usize {
        self.mr_len
    }

    pub(super) fn register_waker(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    pub(super) fn new(
        token: OperationToken,
        connection: impl IntoEstablishedIoConnection,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
    ) -> Self {
        Self::new_with_event(
            token,
            connection,
            direction,
            expected_opcode,
            mr,
            mr_len,
            None,
        )
    }

    pub(super) fn new_with_event(
        token: OperationToken,
        connection: impl IntoEstablishedIoConnection,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
        event_destination: Option<IoEventDestination>,
    ) -> Self {
        let detached = event_destination.is_some();
        let connection = connection.into_established_io();
        Self {
            token,
            connection,
            direction,
            expected_opcode,
            mr_len,
            inner: Mutex::new(OperationInner {
                lifecycle: OperationLifecycle::Posting,
                mr,
                completion: CompletionOwnership::None,
                output: None,
                detached,
                reclamation_pending: false,
                event_destination,
            }),
            waker: AtomicWaker::new(),
            cancelled: AtomicBool::new(false),
            quarantined: AtomicBool::new(false),
        }
    }

    pub(super) fn commit_accepted(&self) -> Option<WorkCompletion> {
        let mut inner = lock_unpoison(&self.inner);
        self.connection.add_accepted(self.token);
        let accepted_lifecycle = if inner.detached {
            OperationLifecycle::Cancelled
        } else {
            OperationLifecycle::InFlight
        };
        match std::mem::replace(&mut inner.completion, CompletionOwnership::None) {
            CompletionOwnership::None => {
                inner.lifecycle = accepted_lifecycle;
                None
            }
            CompletionOwnership::Queued => {
                inner.completion = CompletionOwnership::Queued;
                inner.lifecycle = accepted_lifecycle;
                None
            }
            CompletionOwnership::Early(completion) => {
                inner.lifecycle = OperationLifecycle::Completing;
                Some(completion)
            }
        }
    }

    pub(super) fn mark_completion_queued(&self) -> bool {
        let mut inner = lock_unpoison(&self.inner);
        if matches!(
            inner.lifecycle,
            OperationLifecycle::Completing | OperationLifecycle::Released
        ) || !matches!(inner.completion, CompletionOwnership::None)
        {
            return false;
        }
        inner.completion = CompletionOwnership::Queued;
        true
    }

    pub(super) fn record_completion(&self, completion: WorkCompletion) -> CompletionDisposition {
        let mut inner = lock_unpoison(&self.inner);
        if !matches!(inner.completion, CompletionOwnership::Queued) {
            return CompletionDisposition::Duplicate;
        }
        inner.completion = CompletionOwnership::None;
        match inner.lifecycle {
            OperationLifecycle::Posting => {
                inner.completion = CompletionOwnership::Early(completion);
                CompletionDisposition::Deferred
            }
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming
            | OperationLifecycle::Quarantined => {
                inner.lifecycle = OperationLifecycle::Completing;
                CompletionDisposition::Complete
            }
            OperationLifecycle::Completing | OperationLifecycle::Released => {
                CompletionDisposition::Duplicate
            }
        }
    }

    pub(super) fn cancel(&self, shared: &IoCore) -> bool {
        if self.cancelled.swap(true, Ordering::AcqRel) {
            return false;
        }
        let mut inner = lock_unpoison(&self.inner);
        let mut completed_output = None;
        let cancelled = match inner.lifecycle {
            OperationLifecycle::InFlight => {
                inner.lifecycle = OperationLifecycle::Cancelled;
                inner.detached = true;
                shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
                inner.reclamation_pending = true;
                true
            }
            OperationLifecycle::Released => {
                inner.detached = true;
                completed_output = inner.output.take();
                false
            }
            OperationLifecycle::Posting => {
                inner.detached = true;
                shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
                inner.reclamation_pending = true;
                true
            }
            OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming
            | OperationLifecycle::Quarantined
            | OperationLifecycle::Completing => false,
        };
        drop(inner);
        drop(completed_output);
        cancelled
    }

    pub(super) fn mark_reclaiming(&self) {
        let mut inner = lock_unpoison(&self.inner);
        if inner.lifecycle == OperationLifecycle::Cancelled {
            inner.lifecycle = OperationLifecycle::Reclaiming;
        }
    }

    pub(super) fn mark_quarantined(&self) -> QuarantineTransition {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        match inner.lifecycle {
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming => {
                inner.lifecycle = OperationLifecycle::Quarantined;
                inner.reclamation_pending = false;
                QuarantineTransition {
                    newly_quarantined: !self.quarantined.swap(true, Ordering::AcqRel),
                    was_reclaiming,
                }
            }
            _ => QuarantineTransition {
                newly_quarantined: false,
                was_reclaiming: false,
            },
        }
    }

    pub(super) fn fail_observer_for_close(&self, error: Error) -> bool {
        let mut inner = lock_unpoison(&self.inner);
        if !inner.detached
            && inner.output.is_none()
            && matches!(
                inner.lifecycle,
                OperationLifecycle::InFlight
                    | OperationLifecycle::Cancelled
                    | OperationLifecycle::Reclaiming
                    | OperationLifecycle::Quarantined
            )
        {
            inner.detached = true;
            inner.output = Some((Err(error), None));
            return true;
        }
        false
    }

    pub(super) fn finish_completion(&self, completion: WorkCompletion) -> FinishState {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        inner.reclamation_pending = false;
        let was_quarantined = self.quarantined.swap(false, Ordering::AcqRel);
        let mut mr = inner.mr.take();
        let typed = Completion::from_raw(completion);
        let result = typed.result().map(|()| typed);
        let event = inner.event_destination.take().map(|destination| {
            let event_mr = mr.take();
            destination.complete(
                IoOperationIdentity::from_token(self.token),
                result.clone(),
                event_mr,
            )
        });
        let detached_mr = if event.is_some() {
            None
        } else if inner.detached || inner.output.is_some() {
            mr
        } else {
            inner.output = Some((result, mr));
            None
        };
        inner.lifecycle = OperationLifecycle::Released;
        drop(inner);
        drop(detached_mr);
        FinishState {
            was_reclaiming,
            was_quarantined,
            event,
        }
    }

    pub(super) fn finish_after_qp_destroy(&self, error: Error) -> FinishState {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        inner.reclamation_pending = false;
        let was_quarantined = self.quarantined.swap(false, Ordering::AcqRel);
        let mut mr = inner.mr.take();
        let event = inner.event_destination.take().map(|destination| {
            let event_mr = mr.take();
            destination.complete(
                IoOperationIdentity::from_token(self.token),
                Err(error.clone()),
                event_mr,
            )
        });
        if event.is_none() && !inner.detached && inner.output.is_none() {
            inner.output = Some((Err(error), None));
        }
        inner.lifecycle = OperationLifecycle::Released;
        drop(inner);
        drop(mr);
        FinishState {
            was_reclaiming,
            was_quarantined,
            event,
        }
    }

    pub(super) fn take_mr(&self) -> Option<Mr> {
        lock_unpoison(&self.inner).mr.take()
    }

    pub(super) fn take_unaccepted(&self, error: Error) -> Option<UnacceptedRelease> {
        let mut inner = lock_unpoison(&self.inner);
        if !Self::can_release_unaccepted(&inner) {
            return None;
        }
        Some(self.take_unaccepted_locked(&mut inner, error))
    }

    pub(super) fn take_proven_unaccepted_batch(
        states: &[Arc<Self>],
        error: Error,
    ) -> Option<Vec<UnacceptedRelease>> {
        let mut inners = states
            .iter()
            .map(|state| lock_unpoison(&state.inner))
            .collect::<Vec<_>>();
        if inners
            .iter()
            .any(|inner| !Self::can_release_unaccepted(inner))
        {
            return None;
        }
        Some(
            states
                .iter()
                .zip(inners.iter_mut())
                .map(|(state, inner)| state.take_unaccepted_locked(inner, error.clone()))
                .collect(),
        )
    }

    fn can_release_unaccepted(inner: &OperationInner) -> bool {
        inner.lifecycle == OperationLifecycle::Posting
            && matches!(inner.completion, CompletionOwnership::None)
    }

    fn take_unaccepted_locked(
        &self,
        inner: &mut OperationInner,
        error: Error,
    ) -> UnacceptedRelease {
        debug_assert!(Self::can_release_unaccepted(inner));
        inner.lifecycle = OperationLifecycle::Released;
        let mut mr = inner.mr.take();
        let event = inner.event_destination.take().map(|destination| {
            destination.unaccepted(
                Some(IoOperationIdentity::from_token(self.token)),
                error,
                mr.take().expect("unaccepted I/O operation retains its MR"),
            )
        });
        UnacceptedRelease { event, mr }
    }

    #[cfg(test)]
    pub(super) fn can_release_unaccepted_for_test(&self) -> bool {
        let inner = lock_unpoison(&self.inner);
        Self::can_release_unaccepted(&inner)
    }

    #[cfg(test)]
    pub(super) fn completion_ownership_for_test(&self) -> &'static str {
        match lock_unpoison(&self.inner).completion {
            CompletionOwnership::None => "none",
            CompletionOwnership::Queued => "queued",
            CompletionOwnership::Early(_) => "early",
        }
    }

    pub(super) fn take_output(&self) -> Option<(Result<Completion>, Option<Mr>)> {
        lock_unpoison(&self.inner).output.take()
    }

    pub(super) fn detach_with_post_error(&self, shared: &IoCore) {
        let mut inner = lock_unpoison(&self.inner);
        inner.detached = true;
        inner.lifecycle = OperationLifecycle::Cancelled;
        shared.pending_reclamations.fetch_add(1, Ordering::AcqRel);
        inner.reclamation_pending = true;
        self.cancelled.store(true, Ordering::Release);
    }

    pub(super) fn finalize_terminal(&self, outcome: &MemoizedTerminalResult) -> TerminalizeState {
        let mut inner = lock_unpoison(&self.inner);
        let was_reclaiming = inner.reclamation_pending;
        let newly_quarantined = match inner.lifecycle {
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming => {
                inner.lifecycle = OperationLifecycle::Quarantined;
                !self.quarantined.swap(true, Ordering::AcqRel)
            }
            OperationLifecycle::Quarantined => false,
            OperationLifecycle::Posting
            | OperationLifecycle::Completing
            | OperationLifecycle::Released => {
                return TerminalizeState {
                    was_reclaiming: false,
                    newly_quarantined: false,
                    should_wake: false,
                };
            }
        };
        inner.reclamation_pending = false;
        if !inner.detached && inner.output.is_none() {
            let error = outcome.error().unwrap_or(Error::DriverShutdown);
            inner.output = Some((Err(error), None));
        }
        drop(inner);
        TerminalizeState {
            was_reclaiming,
            newly_quarantined,
            should_wake: true,
        }
    }

    pub(super) fn wake(&self) {
        self.waker.wake();
    }

    #[cfg(test)]
    pub(super) fn lifecycle(&self) -> OperationLifecycle {
        lock_unpoison(&self.inner).lifecycle
    }
}

pub(super) enum CompletionDisposition {
    Deferred,
    Complete,
    Duplicate,
}

pub(super) struct UnacceptedRelease {
    pub(super) event: Option<PendingIoEvent>,
    pub(super) mr: Option<Mr>,
}

pub(super) struct FinishState {
    pub(super) was_reclaiming: bool,
    pub(super) was_quarantined: bool,
    pub(super) event: Option<PendingIoEvent>,
}

pub(super) struct QuarantineTransition {
    pub(super) newly_quarantined: bool,
    pub(super) was_reclaiming: bool,
}

pub(super) struct TerminalizeState {
    pub(super) was_reclaiming: bool,
    pub(super) newly_quarantined: bool,
    pub(super) should_wake: bool,
}
