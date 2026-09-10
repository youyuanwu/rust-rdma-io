//! Coupled per-operation state and its owned lifecycle transitions.
//!
//! The backend record is stored by value in the reactor-owned registry. Every
//! transition therefore requires exclusive access to the record; only the
//! resource-free frontend observer remains shared.

use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_util::task::AtomicWaker;

use crate::v2::engine::io::{IoEventDestination, IoOperationIdentity, PendingIoEvent};
use crate::v2::engine::lifecycle::MemoizedTerminalResult;
use crate::v2::engine::registry::{ConnectionToken, OperationToken, lock_unpoison};
use crate::v2::error::{Error, Result};
use crate::v2::mr::Mr;
use crate::v2::op::Completion;
use crate::wc::{WcOpcode, WorkCompletion};

use super::super::{Direction, EstablishedIoConnection};

/// Resource-free scalar-operation observer shared with the frontend.
///
/// Provider-facing state and MR ownership stay in the reactor-owned operation
/// record. The observer carries only cancellation, a take-once result, and a
/// waker, so dropping or polling a frontend cannot mutate the backend.
pub(in crate::v2::engine) struct OperationObserver {
    result: Mutex<ObserverResult>,
    waker: AtomicWaker,
    cancelled: std::sync::atomic::AtomicBool,
}

enum ObserverResult {
    Pending,
    Ready(Option<(Result<Completion>, Option<Mr>)>),
    Taken,
}

impl OperationObserver {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            result: Mutex::new(ObserverResult::Pending),
            waker: AtomicWaker::new(),
            cancelled: std::sync::atomic::AtomicBool::new(false),
        })
    }

    pub(super) fn register(&self, waker: &Waker) {
        self.waker.register(waker);
    }

    pub(super) fn poll(&self, cx: &mut Context<'_>) -> Poll<(Result<Completion>, Option<Mr>)> {
        let mut result = lock_unpoison(&self.result);
        match &mut *result {
            ObserverResult::Pending => {
                self.waker.register(cx.waker());
                Poll::Pending
            }
            ObserverResult::Ready(output) => {
                let output = output
                    .take()
                    .unwrap_or_else(|| (Err(Error::DriverShutdown), None));
                *result = ObserverResult::Taken;
                Poll::Ready(output)
            }
            ObserverResult::Taken => Poll::Ready((Err(Error::DriverShutdown), None)),
        }
    }

    pub(super) fn complete(&self, output: (Result<Completion>, Option<Mr>)) {
        let mut result = lock_unpoison(&self.result);
        if matches!(*result, ObserverResult::Pending) {
            *result = ObserverResult::Ready(Some(output));
        }
    }

    pub(super) fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        let mut result = lock_unpoison(&self.result);
        if let ObserverResult::Ready(output) = &mut *result {
            drop(output.take());
            *result = ObserverResult::Taken;
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(std::sync::atomic::Ordering::Acquire)
    }

    pub(in crate::v2::engine) fn wake(&self) {
        self.waker.wake();
    }
}

pub(in crate::v2::engine) struct OperationState {
    token: OperationToken,
    identity: super::super::EstablishedIoIdentity,
    drain_notify: Arc<tokio::sync::Notify>,
    direction: Direction,
    expected_opcode: WcOpcode,
    mr_len: usize,
    lifecycle: OperationLifecycle,
    mr: Option<Mr>,
    completion: CompletionOwnership,
    observer: Option<Arc<OperationObserver>>,
    reclamation_pending: bool,
    event_destination: Option<IoEventDestination>,
    quarantined: bool,
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

impl OperationState {
    pub(super) fn token(&self) -> OperationToken {
        self.token
    }

    pub(in crate::v2::engine) fn connection_token(&self) -> ConnectionToken {
        self.identity.connection
    }

    pub(super) fn connection_identity(&self) -> super::super::EstablishedIoIdentity {
        self.identity
    }

    pub(super) fn drain_notify(&self) -> Arc<tokio::sync::Notify> {
        Arc::clone(&self.drain_notify)
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

    /// Registers the waker woken by post-unlock operation wakes.
    #[cfg(test)]
    pub(super) fn register_waker(&self, waker: &Waker) {
        if let Some(observer) = self.observer.as_ref() {
            observer.register(waker);
        }
    }

    #[cfg(test)]
    pub(super) fn new(
        token: OperationToken,
        connection: Arc<EstablishedIoConnection>,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
    ) -> Self {
        Self::new_with_observer(
            token,
            connection,
            direction,
            expected_opcode,
            mr,
            mr_len,
            None,
            Some(OperationObserver::new()),
        )
    }

    pub(super) fn new_with_event(
        token: OperationToken,
        connection: Arc<EstablishedIoConnection>,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
        event_destination: Option<IoEventDestination>,
    ) -> Self {
        Self::new_with_observer(
            token,
            connection,
            direction,
            expected_opcode,
            mr,
            mr_len,
            event_destination,
            None,
        )
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "backend ownership is constructed atomically"
    )]
    fn new_with_observer(
        token: OperationToken,
        connection: Arc<EstablishedIoConnection>,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
        event_destination: Option<IoEventDestination>,
        observer: Option<Arc<OperationObserver>>,
    ) -> Self {
        Self {
            token,
            identity: connection.identity(),
            drain_notify: connection.drain_notify(),
            direction,
            expected_opcode,
            mr_len,
            lifecycle: OperationLifecycle::Posting,
            mr,
            completion: CompletionOwnership::None,
            observer,
            reclamation_pending: false,
            event_destination,
            quarantined: false,
        }
    }

    pub(super) fn new_scalar(
        token: OperationToken,
        connection: Arc<EstablishedIoConnection>,
        direction: Direction,
        expected_opcode: WcOpcode,
        mr: Option<Mr>,
        mr_len: usize,
        observer: Arc<OperationObserver>,
    ) -> Self {
        Self::new_with_observer(
            token,
            connection,
            direction,
            expected_opcode,
            mr,
            mr_len,
            None,
            Some(observer),
        )
    }

    pub(super) fn commit_accepted(&mut self) -> CommitAccepted {
        let accepted_lifecycle = if self
            .observer
            .as_ref()
            .is_some_and(|observer| observer.is_cancelled())
        {
            self.reclamation_pending = true;
            OperationLifecycle::Cancelled
        } else {
            OperationLifecycle::InFlight
        };
        let early = match std::mem::replace(&mut self.completion, CompletionOwnership::None) {
            CompletionOwnership::None => {
                self.lifecycle = accepted_lifecycle;
                None
            }
            CompletionOwnership::Queued => {
                self.completion = CompletionOwnership::Queued;
                self.lifecycle = accepted_lifecycle;
                None
            }
            CompletionOwnership::Early(completion) => {
                self.lifecycle = OperationLifecycle::Completing;
                Some(completion)
            }
        };
        CommitAccepted {
            early,
            cancellation_needs_reclamation: accepted_lifecycle == OperationLifecycle::Cancelled
                && self.reclamation_pending,
        }
    }

    pub(super) fn mark_completion_queued(&mut self) -> bool {
        if matches!(
            self.lifecycle,
            OperationLifecycle::Completing | OperationLifecycle::Released
        ) || !matches!(self.completion, CompletionOwnership::None)
        {
            return false;
        }
        self.completion = CompletionOwnership::Queued;
        true
    }

    pub(super) fn record_completion(
        &mut self,
        completion: WorkCompletion,
    ) -> CompletionDisposition {
        if !matches!(self.completion, CompletionOwnership::Queued) {
            return CompletionDisposition::Duplicate;
        }
        self.completion = CompletionOwnership::None;
        match self.lifecycle {
            OperationLifecycle::Posting => {
                self.completion = CompletionOwnership::Early(completion);
                CompletionDisposition::Deferred
            }
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming
            | OperationLifecycle::Quarantined => {
                self.lifecycle = OperationLifecycle::Completing;
                CompletionDisposition::Complete
            }
            OperationLifecycle::Completing | OperationLifecycle::Released => {
                CompletionDisposition::Duplicate
            }
        }
    }

    pub(in crate::v2::engine::io_core) fn cancel_backend(&mut self) -> bool {
        match self.lifecycle {
            OperationLifecycle::InFlight => {
                self.lifecycle = OperationLifecycle::Cancelled;
                self.observer.take();
                self.reclamation_pending = true;
                true
            }
            OperationLifecycle::Released => {
                self.observer.take();
                false
            }
            OperationLifecycle::Posting => {
                self.observer.take();
                self.reclamation_pending = true;
                true
            }
            OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming
            | OperationLifecycle::Quarantined
            | OperationLifecycle::Completing => false,
        }
    }

    pub(super) fn mark_reclaiming(&mut self) {
        if self.lifecycle == OperationLifecycle::Cancelled {
            self.lifecycle = OperationLifecycle::Reclaiming;
        }
    }

    pub(super) fn mark_quarantined(&mut self) -> QuarantineTransition {
        let was_reclaiming = self.reclamation_pending;
        match self.lifecycle {
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming => {
                self.lifecycle = OperationLifecycle::Quarantined;
                self.reclamation_pending = false;
                QuarantineTransition {
                    newly_quarantined: !std::mem::replace(&mut self.quarantined, true),
                    was_reclaiming,
                }
            }
            _ => QuarantineTransition {
                newly_quarantined: false,
                was_reclaiming: false,
            },
        }
    }

    pub(super) fn fail_observer_for_close(
        &mut self,
        error: Error,
    ) -> Option<Arc<OperationObserver>> {
        if self.observer.is_some()
            && matches!(
                self.lifecycle,
                OperationLifecycle::InFlight
                    | OperationLifecycle::Cancelled
                    | OperationLifecycle::Reclaiming
                    | OperationLifecycle::Quarantined
            )
        {
            let observer = self.observer.take().expect("checked scalar observer");
            observer.complete((Err(error), None));
            return Some(observer);
        }
        None
    }

    pub(super) fn finish_completion(&mut self, completion: WorkCompletion) -> FinishState {
        let was_reclaiming = self.reclamation_pending;
        self.reclamation_pending = false;
        let was_quarantined = std::mem::replace(&mut self.quarantined, false);
        let mut mr = self.mr.take();
        let typed = Completion::from_raw(completion);
        let result = typed.result().map(|()| typed);
        let event = self.event_destination.take().map(|destination| {
            let event_mr = mr.take();
            destination.complete(
                IoOperationIdentity::from_token(self.token),
                result.clone(),
                event_mr,
            )
        });
        let observer = self.observer.take();
        let detached_mr = if event.is_some() {
            None
        } else if let Some(observer) = observer.as_ref() {
            if observer.is_cancelled() {
                mr
            } else {
                observer.complete((result, mr));
                None
            }
        } else {
            mr
        };
        self.lifecycle = OperationLifecycle::Released;
        drop(detached_mr);
        FinishState {
            was_reclaiming,
            was_quarantined,
            event,
            observer,
        }
    }

    pub(super) fn finish_after_qp_destroy(&mut self, error: Error) -> FinishState {
        let was_reclaiming = self.reclamation_pending;
        self.reclamation_pending = false;
        let was_quarantined = std::mem::replace(&mut self.quarantined, false);
        let mut mr = self.mr.take();
        let observer = self.observer.take();
        let event = self.event_destination.take().map(|destination| {
            let event_mr = mr.take();
            destination.complete(
                IoOperationIdentity::from_token(self.token),
                Err(error.clone()),
                event_mr,
            )
        });
        if event.is_none()
            && let Some(observer) = observer.as_ref()
            && !observer.is_cancelled()
        {
            observer.complete((Err(error), None));
        }
        self.lifecycle = OperationLifecycle::Released;
        drop(mr);
        FinishState {
            was_reclaiming,
            was_quarantined,
            event,
            observer,
        }
    }

    pub(super) fn qp_destroy_publication_leaves(&self) -> usize {
        1 + usize::from(self.event_destination.is_some() || self.observer.is_some())
    }

    pub(super) fn take_mr(&mut self) -> Option<Mr> {
        self.mr.take()
    }

    pub(super) fn take_unaccepted(&mut self, error: Error) -> Option<UnacceptedRelease> {
        if !self.can_release_unaccepted() {
            return None;
        }
        Some(self.take_unaccepted_owned(error))
    }

    pub(super) fn can_release_unaccepted(&self) -> bool {
        self.lifecycle == OperationLifecycle::Posting
            && matches!(self.completion, CompletionOwnership::None)
    }

    fn take_unaccepted_owned(&mut self, error: Error) -> UnacceptedRelease {
        debug_assert!(self.can_release_unaccepted());
        self.lifecycle = OperationLifecycle::Released;
        let mut mr = self.mr.take();
        let event = self.event_destination.take().map(|destination| {
            destination.unaccepted(
                Some(IoOperationIdentity::from_token(self.token)),
                error,
                mr.take().expect("unaccepted I/O operation retains its MR"),
            )
        });
        UnacceptedRelease { event, mr }
    }

    pub(super) fn detach_with_post_error(&mut self) -> bool {
        self.observer.take();
        self.lifecycle = OperationLifecycle::Cancelled;
        if self.reclamation_pending {
            false
        } else {
            self.reclamation_pending = true;
            true
        }
    }

    pub(super) fn finalize_terminal(
        &mut self,
        outcome: &MemoizedTerminalResult,
    ) -> TerminalizeState {
        let was_reclaiming = self.reclamation_pending;
        let newly_quarantined = match self.lifecycle {
            OperationLifecycle::InFlight
            | OperationLifecycle::Cancelled
            | OperationLifecycle::Reclaiming => {
                self.lifecycle = OperationLifecycle::Quarantined;
                !std::mem::replace(&mut self.quarantined, true)
            }
            OperationLifecycle::Quarantined => false,
            OperationLifecycle::Posting
            | OperationLifecycle::Completing
            | OperationLifecycle::Released => {
                return TerminalizeState {
                    was_reclaiming: false,
                    newly_quarantined: false,
                    observer: None,
                };
            }
        };
        self.reclamation_pending = false;
        let observer = self.observer.take().filter(|observer| {
            if observer.is_cancelled() {
                false
            } else {
                let error = outcome.error().unwrap_or(Error::DriverShutdown);
                observer.complete((Err(error), None));
                true
            }
        });
        TerminalizeState {
            was_reclaiming,
            newly_quarantined,
            observer,
        }
    }

    #[cfg(test)]
    pub(super) fn lifecycle(&self) -> OperationLifecycle {
        self.lifecycle
    }

    #[cfg(test)]
    pub(super) fn observer_for_test(&self) -> Arc<OperationObserver> {
        self.observer
            .as_ref()
            .cloned()
            .expect("test scalar operation retains its observer")
    }
}

pub(super) enum CompletionDisposition {
    Deferred,
    Complete,
    Duplicate,
}

pub(super) struct CommitAccepted {
    pub(super) early: Option<WorkCompletion>,
    pub(super) cancellation_needs_reclamation: bool,
}

/// Post-unlock ownership taken from an operation that never reached the provider.
pub(super) struct UnacceptedRelease {
    pub(super) event: Option<PendingIoEvent>,
    pub(super) mr: Option<Mr>,
}

pub(super) struct FinishState {
    pub(super) was_reclaiming: bool,
    pub(super) was_quarantined: bool,
    pub(super) event: Option<PendingIoEvent>,
    pub(super) observer: Option<Arc<OperationObserver>>,
}

pub(super) struct QuarantineTransition {
    pub(super) newly_quarantined: bool,
    pub(super) was_reclaiming: bool,
}

pub(super) struct TerminalizeState {
    pub(super) was_reclaiming: bool,
    pub(super) newly_quarantined: bool,
    pub(super) observer: Option<Arc<OperationObserver>>,
}
