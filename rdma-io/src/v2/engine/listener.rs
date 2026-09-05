//! Engine-owned listeners and ordered inbound accept arbitration.

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use super::connection::{ConnectionReservation, SharedCmId};
use super::io::{IoConnection, IoEventReceiver};
use super::lifecycle::{MemoizedTerminalResult, TakeOnceResult};
use super::registry::{lock_unpoison, read_unpoison};
use super::session::{SessionListener, SessionListenerCloseState};
use super::{ConnectionSetup, EngineShared, RdmaConnection, RdmaConnectionConfig, SetupSummary};
use crate::v2::error::{Error, Result};
use futures_util::task::AtomicWaker;

pub(crate) const DEFAULT_LISTENER_BACKLOG: usize = 128;
const MAX_LISTENER_BACKLOG: usize = 4_096;
pub(super) const KERNEL_LISTEN_BACKLOG_REQUEST: i32 = i32::MAX;

/// Configuration for one engine-owned listener.
///
/// The configurable backlog is a userspace queue limit, not the kernel
/// `rdma_listen` backlog. It must be in `1..=4096` when
/// [`RdmaEngine::listen`](super::RdmaEngine::listen) is called. The engine
/// independently requests `i32::MAX` from `rdma_listen`; a provider or kernel
/// may clamp that request, or may refuse it before any child reaches the
/// userspace queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RdmaListenerConfig {
    backlog: usize,
}

impl Default for RdmaListenerConfig {
    fn default() -> Self {
        Self {
            backlog: DEFAULT_LISTENER_BACKLOG,
        }
    }
}

impl RdmaListenerConfig {
    /// Store the userspace pending-child queue limit for validation by `listen`.
    ///
    /// Valid values are `1..=4096`. This setter deliberately does not validate,
    /// panic, or clamp. The independent kernel `rdma_listen` request is always
    /// `i32::MAX`; provider/kernel clamping does not change this configured
    /// userspace limit, while refusal is reported by `listen` as a contextual
    /// listener-creation error rather than as a userspace backlog rejection.
    pub fn backlog(mut self, value: usize) -> Self {
        self.backlog = value;
        self
    }

    /// Return the configured userspace pending-child queue limit.
    pub fn backlog_capacity(&self) -> usize {
        self.backlog
    }

    pub(super) fn validate(&self) -> Result<()> {
        if !(1..=MAX_LISTENER_BACKLOG).contains(&self.backlog) {
            return Err(Error::InvalidConfig(format!(
                "listener backlog {} is outside 1..={MAX_LISTENER_BACKLOG}",
                self.backlog
            )));
        }
        Ok(())
    }
}

/// Engine-owned inbound listener whose progress and resources belong to its engine driver.
///
/// Clones share one listener endpoint. Dropping the final clone requests an
/// asynchronous close; call [`Self::close`] when the result must be observed.
///
/// Waiters are ordered by registration and admitted children by CM arrival.
/// The oldest live waiter is paired with the oldest eligible child, with only
/// one selected/setup pair at a time. Cancellation after selection owns that
/// child through rejection or close, so a later waiter cannot overtake it.
pub struct RdmaListener {
    session: SessionListener,
}

impl Clone for RdmaListener {
    fn clone(&self) -> Self {
        self.session.retain_frontend();
        Self {
            session: self.session.clone(),
        }
    }
}

impl RdmaListener {
    /// Return the address assigned to this listener.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.session.local_addr())
    }

    /// Accept the next inbound connection with the default connection configuration.
    ///
    /// Pending accepts are cancelled safely if their futures are dropped. Once
    /// closing starts, new accepts fail with the listener's contextual close
    /// error, or with the engine-wide terminal error if the driver has failed.
    /// Low-level setup posts zero initial receives.
    pub async fn accept(&self) -> Result<RdmaConnection> {
        let (shared, state) = self.session.owners()?;
        accept_with_setup(
            shared,
            state,
            RdmaConnectionConfig::default(),
            empty_connection_setup(),
        )
        .await
    }

    /// Accept the next inbound connection with an explicit connection configuration.
    ///
    /// The configuration is validated before the accept waiter is registered.
    /// Cancellation and listener-close behavior are the same as [`Self::accept`].
    /// No value is silently clamped, and low-level setup posts zero receives.
    pub async fn accept_with_config(&self, config: RdmaConnectionConfig) -> Result<RdmaConnection> {
        let (shared, state) = self.session.owners()?;
        accept_with_setup(shared, state, config, empty_connection_setup()).await
    }

    pub(crate) async fn accept_with_io_setup<F>(
        &self,
        config: RdmaConnectionConfig,
        setup: F,
    ) -> Result<RdmaConnection>
    where
        F: FnOnce(IoConnection, IoEventReceiver) -> Result<usize> + Send + 'static,
    {
        let (shared, state) = self.session.owners()?;
        accept_with_setup(shared, state, config, Box::new(setup)).await
    }

    pub(crate) fn validate_message_connection_config(
        &self,
        config: &RdmaConnectionConfig,
    ) -> Result<()> {
        let (shared, _) = self.session.owners()?;
        config.validate(&shared.config, shared.provider.as_ref())
    }

    /// Close the listener and wait for CM destruction or engine termination.
    ///
    /// Close is idempotent across clones. If the engine driver fails while the
    /// CM ID is awaiting destruction, every close waiter is woken with the same
    /// engine-wide terminal error and the ID remains quarantined with the failed
    /// engine rather than being destroyed without CM progress.
    pub async fn close(&self) -> Result<()> {
        self.session.close().await
    }

    pub(super) fn from_state(shared: &Arc<EngineShared>, state: Arc<ListenerState>) -> Self {
        Self {
            session: shared.session.listener_capability(&state),
        }
    }
}

impl Drop for RdmaListener {
    fn drop(&mut self) {
        if self.session.release_frontend() {
            self.session.request_close();
        }
    }
}

pub(super) async fn listen(
    shared: Arc<EngineShared>,
    address: SocketAddr,
    config: RdmaListenerConfig,
) -> Result<RdmaListener> {
    config.validate()?;
    let admission = read_unpoison(&shared.session.admission);
    if let Some(error) = shared.admission_error() {
        return Err(error);
    }
    let request = Arc::new(ListenRequest::new(address, config));
    shared.session.cm.enqueue_listen(Arc::clone(&request));
    drop(admission);
    shared.work_signal.publish(super::cm::CM_WORK);
    ListenWaiter {
        manager: Arc::downgrade(&shared.session),
        request: Arc::downgrade(&request),
        observer: Arc::clone(&request.observer),
        finished: false,
    }
    .await
}

pub(super) async fn accept_with_setup(
    shared: Arc<EngineShared>,
    listener: Arc<ListenerState>,
    config: RdmaConnectionConfig,
    setup: ConnectionSetup,
) -> Result<RdmaConnection> {
    config.validate(&shared.config, shared.provider.as_ref())?;
    let admission = read_unpoison(&shared.session.admission);
    if let Some(error) = shared.admission_error() {
        return Err(error);
    }
    let request = Arc::new(AcceptRequest::new(AcceptIntent::new(config, setup)));
    listener.register_waiter(Arc::clone(&request))?;
    drop(admission);
    shared.session.cm.enqueue_listener_work(&listener);
    shared.work_signal.publish(super::cm::CM_WORK);
    AcceptWaiter {
        manager: Arc::downgrade(&shared.session),
        listener: Arc::downgrade(&listener),
        request: Arc::downgrade(&request),
        observer: Arc::clone(&request.observer),
        finished: false,
    }
    .await
}

pub(super) fn empty_connection_setup() -> ConnectionSetup {
    Box::new(|_connection, _events| Ok(0))
}

pub(super) fn run_setup_before_establish(
    setup: ConnectionSetup,
    connection: &RdmaConnection,
    before_establish: impl FnOnce() -> Result<()>,
    establish: impl FnOnce() -> Result<()>,
) -> Result<SetupSummary> {
    let connection_state = connection.require_session_state()?;
    let accepted_before = connection_state.accepted_count();
    let (io, events) = IoConnection::from_connection(connection)?;
    let summary = SetupSummary {
        posted_wrs: setup(io, events)?,
    };
    let accepted_after = connection_state.accepted_count();
    let posted_wrs = accepted_after.checked_sub(accepted_before).ok_or_else(|| {
        Error::InvalidConfig("pre-establishment setup reduced the accepted WR set".into())
    })?;
    if posted_wrs != summary.posted_wrs {
        return Err(Error::InvalidConfig(format!(
            "pre-establishment setup reported {} posted WRs but registered {posted_wrs}",
            summary.posted_wrs
        )));
    }
    before_establish()?;
    establish()?;
    Ok(summary)
}

pub(super) struct AcceptIntent {
    config: RdmaConnectionConfig,
    setup: Option<ConnectionSetup>,
}

impl AcceptIntent {
    fn new(config: RdmaConnectionConfig, setup: ConnectionSetup) -> Self {
        Self {
            config,
            setup: Some(setup),
        }
    }

    pub(super) fn into_parts(mut self) -> Result<(RdmaConnectionConfig, ConnectionSetup)> {
        let setup = self.setup.take().ok_or_else(|| {
            Error::InvalidConfig("accept setup was consumed more than once".into())
        })?;
        Ok((self.config, setup))
    }
}

pub(super) struct IncomingChild {
    pub(super) cm_id: Option<SharedCmId>,
    pub(super) reservation: Option<ConnectionReservation>,
}

impl IncomingChild {
    pub(super) fn new(cm_id: SharedCmId, reservation: ConnectionReservation) -> Self {
        Self {
            cm_id: Some(cm_id),
            reservation: Some(reservation),
        }
    }

    pub(super) fn into_resources(mut self) -> Result<(SharedCmId, ConnectionReservation)> {
        let cm_id = self
            .cm_id
            .take()
            .ok_or_else(|| Error::InvalidConfig("inbound child lost its CM ID".into()))?;
        let reservation = self.reservation.take().ok_or_else(|| {
            Error::InvalidConfig("inbound child lost its connection reservation".into())
        })?;
        Ok((cm_id, reservation))
    }

    #[cfg(test)]
    pub(super) fn test_only() -> Self {
        Self {
            cm_id: None,
            reservation: None,
        }
    }
}

pub(super) struct ListenRequest {
    pub(super) address: SocketAddr,
    pub(super) config: RdmaListenerConfig,
    observer: Arc<ListenRequestObserver>,
}

struct ListenRequestObserver {
    result: Mutex<TakeOnceResult<RdmaListener>>,
    cancelled: AtomicBool,
    waker: AtomicWaker,
}

impl ListenRequest {
    fn new(address: SocketAddr, config: RdmaListenerConfig) -> Self {
        Self {
            address,
            config,
            observer: Arc::new(ListenRequestObserver {
                result: Mutex::new(TakeOnceResult::Pending),
                cancelled: AtomicBool::new(false),
                waker: AtomicWaker::new(),
            }),
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.observer.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn complete(&self, result: Result<RdmaListener>) {
        let mut current = lock_unpoison(&self.observer.result);
        if matches!(&*current, TakeOnceResult::Pending) {
            *current = TakeOnceResult::Ready(result);
            drop(current);
            self.observer.waker.wake();
        }
    }
}

impl ListenRequestObserver {
    fn take_result(&self) -> Option<Result<RdmaListener>> {
        let mut current = lock_unpoison(&self.result);
        match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
            TakeOnceResult::Ready(result) => Some(result),
            TakeOnceResult::Pending => {
                *current = TakeOnceResult::Pending;
                None
            }
            TakeOnceResult::Taken => None,
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut current = lock_unpoison(&self.result);
        let replacement = match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
            TakeOnceResult::Pending => TakeOnceResult::Ready(Err(Error::DriverShutdown)),
            TakeOnceResult::Ready(Ok(listener)) => {
                drop(listener);
                TakeOnceResult::Ready(Err(Error::DriverShutdown))
            }
            TakeOnceResult::Ready(Err(error)) => TakeOnceResult::Ready(Err(error)),
            TakeOnceResult::Taken => TakeOnceResult::Taken,
        };
        *current = replacement;
        drop(current);
        self.waker.wake();
    }
}

struct ListenWaiter {
    manager: Weak<super::SessionManager>,
    request: Weak<ListenRequest>,
    observer: Arc<ListenRequestObserver>,
    finished: bool,
}

impl Future for ListenWaiter {
    type Output = Result<RdmaListener>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.observer.take_result() {
            self.finished = true;
            return Poll::Ready(result);
        }
        self.observer.waker.register(cx.waker());
        if let Some(result) = self.observer.take_result() {
            self.finished = true;
            return Poll::Ready(result);
        }
        Poll::Pending
    }
}

impl Drop for ListenWaiter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.observer.cancel();
        if self.request.upgrade().is_none() {
            return;
        }
        if let Some(engine) = self.manager.upgrade().and_then(|manager| manager.engine()) {
            engine.work_signal.publish(super::cm::CM_WORK);
        }
    }
}

pub(super) struct AcceptRequest {
    intent: Mutex<Option<AcceptIntent>>,
    observer: Arc<AcceptRequestObserver>,
    route_token: AtomicU64,
}

struct AcceptRequestObserver {
    result: Mutex<TakeOnceResult<RdmaConnection>>,
    cancelled: AtomicBool,
    delivered: AtomicBool,
    waker: AtomicWaker,
}

impl AcceptRequest {
    fn new(intent: AcceptIntent) -> Self {
        Self {
            intent: Mutex::new(Some(intent)),
            observer: Arc::new(AcceptRequestObserver {
                result: Mutex::new(TakeOnceResult::Pending),
                cancelled: AtomicBool::new(false),
                delivered: AtomicBool::new(false),
                waker: AtomicWaker::new(),
            }),
            route_token: AtomicU64::new(0),
        }
    }

    #[cfg(test)]
    pub(super) fn test_only() -> Arc<Self> {
        Arc::new(Self::new(AcceptIntent::new(
            RdmaConnectionConfig::default(),
            empty_connection_setup(),
        )))
    }

    pub(super) fn take_intent(&self) -> Option<AcceptIntent> {
        lock_unpoison(&self.intent).take()
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.observer.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn set_route_token(&self, token: u64) {
        self.route_token.store(token, Ordering::Release);
    }

    pub(super) fn route_token(&self) -> u64 {
        self.route_token.load(Ordering::Acquire)
    }

    pub(super) fn is_delivered(&self) -> bool {
        self.observer.delivered.load(Ordering::Acquire)
    }

    pub(super) fn complete(&self, result: Result<RdmaConnection>) {
        let mut current = lock_unpoison(&self.observer.result);
        if matches!(&*current, TakeOnceResult::Pending) {
            *current = TakeOnceResult::Ready(result);
            drop(current);
            self.observer.waker.wake();
        }
    }

    pub(super) fn complete_success(&self, connection: RdmaConnection) {
        let mut current = lock_unpoison(&self.observer.result);
        if self.observer.cancelled.load(Ordering::Acquire)
            || !matches!(&*current, TakeOnceResult::Pending)
        {
            drop(current);
            drop(connection);
            return;
        }
        *current = TakeOnceResult::Ready(Ok(connection));
        drop(current);
        self.observer.waker.wake();
    }

    pub(super) fn fail_undelivered(&self, error: Error) -> bool {
        let mut current = lock_unpoison(&self.observer.result);
        let replacement = match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
            TakeOnceResult::Pending | TakeOnceResult::Ready(Ok(_)) => {
                TakeOnceResult::Ready(Err(error))
            }
            TakeOnceResult::Ready(Err(existing)) => TakeOnceResult::Ready(Err(existing)),
            TakeOnceResult::Taken => TakeOnceResult::Taken,
        };
        *current = replacement;
        drop(current);
        self.observer.waker.wake();
        self.is_delivered()
    }

    #[cfg(test)]
    fn cancel(&self) {
        self.observer.cancel();
    }

    #[cfg(test)]
    pub(super) fn take_result_for_test(&self) -> Option<Result<RdmaConnection>> {
        self.observer.take_result()
    }
}

impl AcceptRequestObserver {
    fn take_result(&self) -> Option<Result<RdmaConnection>> {
        let mut current = lock_unpoison(&self.result);
        match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
            TakeOnceResult::Ready(result) => {
                if result.is_ok() {
                    self.delivered.store(true, Ordering::Release);
                }
                Some(result)
            }
            TakeOnceResult::Pending => {
                *current = TakeOnceResult::Pending;
                None
            }
            TakeOnceResult::Taken => None,
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut current = lock_unpoison(&self.result);
        let replacement = match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
            TakeOnceResult::Pending => TakeOnceResult::Pending,
            TakeOnceResult::Ready(Ok(connection)) => {
                drop(connection);
                TakeOnceResult::Taken
            }
            TakeOnceResult::Ready(Err(error)) => TakeOnceResult::Ready(Err(error)),
            TakeOnceResult::Taken => TakeOnceResult::Taken,
        };
        *current = replacement;
        drop(current);
        self.waker.wake();
    }
}

struct AcceptWaiter {
    manager: Weak<super::SessionManager>,
    listener: Weak<ListenerState>,
    request: Weak<AcceptRequest>,
    observer: Arc<AcceptRequestObserver>,
    finished: bool,
}

impl Future for AcceptWaiter {
    type Output = Result<RdmaConnection>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.observer.take_result() {
            if result.is_ok() {
                self.mark_delivered();
            }
            self.finished = true;
            return Poll::Ready(result);
        }
        self.observer.waker.register(cx.waker());
        if let Some(result) = self.observer.take_result() {
            if result.is_ok() {
                self.mark_delivered();
            }

            self.finished = true;
            return Poll::Ready(result);
        }
        Poll::Pending
    }
}

impl AcceptWaiter {
    fn mark_delivered(&self) {
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let Some(listener) = self.listener.upgrade() else {
            return;
        };
        let Some(request) = self.request.upgrade() else {
            return;
        };
        manager.cm.mark_accept_delivered(&listener, &request);
        if let Some(engine) = manager.engine() {
            engine.work_signal.publish(super::cm::CM_WORK);
        }
    }
}

impl Drop for AcceptWaiter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.observer.cancel();
        let Some(manager) = self.manager.upgrade() else {
            return;
        };
        let Some(listener) = self.listener.upgrade() else {
            return;
        };
        if self.request.upgrade().is_none() {
            return;
        }
        manager.cm.enqueue_listener_work(&listener);
        if let Some(engine) = manager.engine() {
            engine.work_signal.publish(super::cm::CM_WORK);
        }
    }
}

pub(super) struct ListenerState {
    pub(super) token: u64,
    pub(super) local_addr: SocketAddr,
    pub(super) backlog: usize,
    pub(super) cm_id: Mutex<Option<SharedCmId>>,
    queues: Mutex<ListenerQueues>,
    closing: AtomicBool,
    finalization_started: AtomicBool,
    failure: Mutex<Option<Error>>,
    close: Arc<SessionListenerCloseState>,
    work_enqueued: AtomicBool,
}

impl ListenerState {
    pub(super) fn new(
        token: u64,
        local_addr: SocketAddr,
        config: RdmaListenerConfig,
        cm_id: SharedCmId,
    ) -> Self {
        Self {
            token,
            local_addr,
            backlog: config.backlog,
            cm_id: Mutex::new(Some(cm_id)),
            queues: Mutex::new(ListenerQueues::default()),
            closing: AtomicBool::new(false),
            finalization_started: AtomicBool::new(false),
            failure: Mutex::new(None),
            close: SessionListenerCloseState::new(),
            work_enqueued: AtomicBool::new(false),
        }
    }

    #[cfg(test)]
    pub(super) fn test_only(backlog: usize) -> Arc<Self> {
        Arc::new(Self {
            token: 1,
            local_addr: "127.0.0.1:1".parse().unwrap(),
            backlog,
            cm_id: Mutex::new(None),
            queues: Mutex::new(ListenerQueues::default()),
            closing: AtomicBool::new(false),
            finalization_started: AtomicBool::new(false),
            failure: Mutex::new(None),
            close: SessionListenerCloseState::new(),
            work_enqueued: AtomicBool::new(false),
        })
    }

    fn lock_queues(&self) -> std::sync::MutexGuard<'_, ListenerQueues> {
        lock_unpoison(&self.queues)
    }

    pub(super) fn register_waiter(&self, request: Arc<AcceptRequest>) -> Result<()> {
        if self.closing.load(Ordering::Acquire) {
            return Err(self.close_error());
        }
        let mut queues = self.lock_queues();
        if self.closing.load(Ordering::Acquire) {
            return Err(self.close_error());
        }
        queues.waiters.push_back(request);
        select_pair(&mut queues);
        Ok(())
    }

    pub(super) fn admit_child(&self, child: IncomingChild) -> ChildAdmission {
        let mut queues = self.lock_queues();
        let mut cancelled = Vec::new();
        while queues
            .waiters
            .front()
            .is_some_and(|request| request.is_cancelled())
        {
            cancelled.push(queues.waiters.pop_front().expect("front waiter exists"));
        }
        if self.closing.load(Ordering::Acquire) {
            return ChildAdmission {
                cancelled,
                rejected: Some((child, InboundRejectReason::ListenerClosed)),
            };
        }
        select_pair(&mut queues);
        if queues.selected.is_none()
            && let Some(request) = queues.waiters.pop_front()
        {
            queues.selected = Some(SelectedAccept::Ready { request, child });
            return ChildAdmission {
                cancelled,
                rejected: None,
            };
        }
        if queues.children.len() >= self.backlog {
            return ChildAdmission {
                cancelled,
                rejected: Some((child, InboundRejectReason::BacklogFull)),
            };
        }
        queues.children.push_back(child);
        ChildAdmission {
            cancelled,
            rejected: None,
        }
    }

    pub(super) fn next_action(&self) -> ListenerAction {
        let mut queues = self.lock_queues();
        if let Some(position) = queues
            .waiters
            .iter()
            .position(|request| request.is_cancelled())
        {
            return ListenerAction::CancelledBeforeSelection(
                queues
                    .waiters
                    .remove(position)
                    .expect("cancelled waiter position is valid"),
            );
        }

        if self.closing.load(Ordering::Acquire) {
            if let Some(request) = queues.waiters.pop_front() {
                return ListenerAction::FailUnselected(request);
            }
            if let Some(child) = queues.children.pop_front() {
                return ListenerAction::RejectChild(child, InboundRejectReason::ListenerClosed);
            }
            match queues.selected.take() {
                Some(SelectedAccept::Ready { request, child }) => {
                    queues.selected = Some(SelectedAccept::Processing {
                        request: Arc::clone(&request),
                    });
                    return ListenerAction::RejectSelected {
                        request,
                        child,
                        reason: self.close_error(),
                    };
                }
                Some(SelectedAccept::Routed {
                    request,
                    route,
                    cancel_started,
                }) => {
                    queues.selected = Some(SelectedAccept::Routed {
                        request: Arc::clone(&request),
                        route,
                        cancel_started: true,
                    });
                    if !cancel_started {
                        return ListenerAction::CancelAfterAccept { request, route };
                    }
                }
                Some(selected @ SelectedAccept::Processing { .. }) => {
                    queues.selected = Some(selected);
                }
                None => {
                    if !self.finalization_started.swap(true, Ordering::AcqRel) {
                        return ListenerAction::FinalizeClose;
                    }
                }
            }
            return ListenerAction::None;
        }

        select_pair(&mut queues);
        match queues.selected.take() {
            Some(SelectedAccept::Ready { request, child }) => {
                queues.selected = Some(SelectedAccept::Processing {
                    request: Arc::clone(&request),
                });
                ListenerAction::ProcessSelected { request, child }
            }
            Some(SelectedAccept::Routed {
                request,
                route,
                cancel_started,
            }) if request.is_cancelled() && !cancel_started => {
                queues.selected = Some(SelectedAccept::Routed {
                    request: Arc::clone(&request),
                    route,
                    cancel_started: true,
                });
                ListenerAction::CancelAfterAccept { request, route }
            }
            Some(selected) => {
                queues.selected = Some(selected);
                ListenerAction::None
            }
            None => ListenerAction::None,
        }
    }

    pub(super) fn route_selected(&self, request: &Arc<AcceptRequest>, route: u64) -> Result<()> {
        let mut queues = self.lock_queues();
        match queues.selected.take() {
            Some(SelectedAccept::Processing { request: current })
                if Arc::ptr_eq(&current, request) =>
            {
                queues.selected = Some(SelectedAccept::Routed {
                    request: Arc::clone(request),
                    route,
                    cancel_started: false,
                });
                Ok(())
            }
            Some(selected) => {
                queues.selected = Some(selected);
                Err(Error::InvalidConfig(
                    "listener selected pair changed during setup".into(),
                ))
            }
            None => Err(Error::InvalidConfig(
                "listener lost its selected pair during setup".into(),
            )),
        }
    }

    pub(super) fn finish_selected_request(&self, request: &Arc<AcceptRequest>) -> bool {
        let mut queues = self.lock_queues();
        let matches = match queues.selected.as_ref() {
            Some(SelectedAccept::Processing { request: current })
            | Some(SelectedAccept::Ready {
                request: current, ..
            })
            | Some(SelectedAccept::Routed {
                request: current, ..
            }) => Arc::ptr_eq(current, request),
            None => false,
        };
        if matches {
            queues.selected = None;
            select_pair(&mut queues);
        }
        matches
    }

    pub(super) fn finish_selected_route(&self, route: u64) -> bool {
        let mut queues = self.lock_queues();
        let matches = matches!(
            queues.selected.as_ref(),
            Some(SelectedAccept::Routed {
                route: current, ..
            }) if *current == route
        );
        if matches {
            queues.selected = None;
            select_pair(&mut queues);
        }
        matches
    }

    pub(super) fn request_close(self: &Arc<Self>, shared: &Arc<EngineShared>) {
        if !self.closing.swap(true, Ordering::AcqRel) {
            shared.session.cm.enqueue_listener_work(self);
            shared.work_signal.publish(super::cm::CM_WORK);
        }
    }

    pub(super) fn fail(self: &Arc<Self>, shared: &Arc<EngineShared>, error: Error) {
        let mut failure = lock_unpoison(&self.failure);
        if failure.is_none() {
            *failure = Some(error);
        }
        drop(failure);
        self.request_close(shared);
    }

    pub(super) fn close_error(&self) -> Error {
        lock_unpoison(&self.failure)
            .clone()
            .unwrap_or(Error::TransportClosed)
    }

    pub(super) fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    pub(super) fn begin_work(&self) {
        self.work_enqueued.store(false, Ordering::Release);
    }

    pub(super) fn try_enqueue_work(&self) -> bool {
        !self.work_enqueued.swap(true, Ordering::AcqRel)
    }

    pub(super) fn has_work(&self) -> bool {
        let queues = self.lock_queues();
        if self.closing.load(Ordering::Acquire) {
            return !queues.waiters.is_empty()
                || !queues.children.is_empty()
                || matches!(queues.selected, Some(SelectedAccept::Ready { .. }))
                || matches!(
                    queues.selected,
                    Some(SelectedAccept::Routed {
                        cancel_started: false,
                        ..
                    })
                )
                || (queues.selected.is_none()
                    && !self.finalization_started.load(Ordering::Acquire));
        }
        queues.waiters.iter().any(|request| request.is_cancelled())
            || matches!(queues.selected, Some(SelectedAccept::Ready { .. }))
            || matches!(
                queues.selected,
                Some(SelectedAccept::Routed {
                    ref request,
                    cancel_started: false,
                    ..
                }) if request.is_cancelled()
            )
            || (queues.selected.is_none()
                && !queues.waiters.is_empty()
                && !queues.children.is_empty())
    }

    pub(super) fn take_cm_id(&self) -> Option<SharedCmId> {
        lock_unpoison(&self.cm_id).take()
    }

    pub(super) fn finish_close(&self, error: Option<Error>) {
        let failure = error.or_else(|| lock_unpoison(&self.failure).clone());
        self.close.store_if_empty(match failure {
            Some(error) => MemoizedTerminalResult::from_error(error),
            None => MemoizedTerminalResult::success(),
        });
        self.close.notify_waiters();
    }

    pub(super) fn terminalize(&self, outcome: &MemoizedTerminalResult) {
        self.closing.store(true, Ordering::Release);
        {
            self.close.store_if_empty(outcome.clone());
        }
        let mut queues = self.lock_queues();
        let mut requests: Vec<_> = queues
            .waiters
            .drain(..)
            .map(|request| (request, false))
            .collect();
        if let Some(selected) = queues.selected.take() {
            let (request, routed) = match selected {
                SelectedAccept::Ready { request, .. } | SelectedAccept::Processing { request } => {
                    (request, false)
                }
                SelectedAccept::Routed { request, .. } => (request, true),
            };
            requests.push((request, routed));
        }
        queues.children.clear();
        drop(queues);
        for (request, routed) in requests {
            let error = outcome
                .clone()
                .into_result()
                .expect_err("terminal listener outcome must be an error");
            if routed {
                let _ = request.fail_undelivered(error);
            } else {
                request.complete(Err(error));
            }
        }
        self.close.notify_waiters();
    }

    pub(super) fn close_state(&self) -> Arc<SessionListenerCloseState> {
        Arc::clone(&self.close)
    }
}

fn select_pair(queues: &mut ListenerQueues) {
    if queues.selected.is_some() {
        return;
    }
    if queues
        .waiters
        .front()
        .is_some_and(|request| request.is_cancelled())
    {
        return;
    }
    if !queues.waiters.is_empty() && !queues.children.is_empty() {
        let request = queues.waiters.pop_front().expect("waiter exists");
        let child = queues.children.pop_front().expect("child exists");
        queues.selected = Some(SelectedAccept::Ready { request, child });
    }
}

#[derive(Default)]
struct ListenerQueues {
    waiters: VecDeque<Arc<AcceptRequest>>,
    children: VecDeque<IncomingChild>,
    selected: Option<SelectedAccept>,
}

enum SelectedAccept {
    Ready {
        request: Arc<AcceptRequest>,
        child: IncomingChild,
    },
    Processing {
        request: Arc<AcceptRequest>,
    },
    Routed {
        request: Arc<AcceptRequest>,
        route: u64,
        cancel_started: bool,
    },
}

pub(super) struct ChildAdmission {
    pub(super) cancelled: Vec<Arc<AcceptRequest>>,
    pub(super) rejected: Option<(IncomingChild, InboundRejectReason)>,
}

pub(super) enum ListenerAction {
    CancelledBeforeSelection(Arc<AcceptRequest>),
    FailUnselected(Arc<AcceptRequest>),
    RejectChild(IncomingChild, InboundRejectReason),
    ProcessSelected {
        request: Arc<AcceptRequest>,
        child: IncomingChild,
    },
    RejectSelected {
        request: Arc<AcceptRequest>,
        child: IncomingChild,
        reason: Error,
    },
    CancelAfterAccept {
        request: Arc<AcceptRequest>,
        route: u64,
    },
    FinalizeClose,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum InboundRejectReason {
    BacklogFull,
    ConnectionCapacity,
    AdmissionClosed,
    ListenerClosed,
    ContextMismatch,
    SetupFailure,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waiter_observers_do_not_retain_listen_or_accept_records() {
        let listen = Arc::new(ListenRequest::new(
            "127.0.0.1:1".parse().unwrap(),
            RdmaListenerConfig::default(),
        ));
        let listen_record = Arc::downgrade(&listen);
        let listen_observer = Arc::clone(&listen.observer);
        listen.complete(Err(Error::DriverShutdown));
        drop(listen);
        assert!(listen_record.upgrade().is_none());
        assert!(matches!(
            listen_observer.take_result(),
            Some(Err(Error::DriverShutdown))
        ));

        let accept = AcceptRequest::test_only();
        let accept_record = Arc::downgrade(&accept);
        let accept_observer = Arc::clone(&accept.observer);
        accept.complete(Err(Error::DriverShutdown));
        drop(accept);
        assert!(accept_record.upgrade().is_none());
        assert!(matches!(
            accept_observer.take_result(),
            Some(Err(Error::DriverShutdown))
        ));
    }

    fn request() -> Arc<AcceptRequest> {
        Arc::new(AcceptRequest::new(AcceptIntent::new(
            RdmaConnectionConfig::default(),
            empty_connection_setup(),
        )))
    }

    #[test]
    fn backlog_setter_is_deferred_and_exactly_bounded() {
        let zero = RdmaListenerConfig::default().backlog(0);
        assert_eq!(zero.backlog_capacity(), 0);
        assert!(zero.validate().is_err());

        let maximum = RdmaListenerConfig::default().backlog(4_096);
        assert_eq!(maximum.backlog_capacity(), 4_096);
        maximum.validate().unwrap();

        let too_large = RdmaListenerConfig::default().backlog(4_097);
        assert_eq!(too_large.backlog_capacity(), 4_097);
        assert!(too_large.validate().is_err());
    }

    #[test]
    fn default_backlog_matches_the_contract() {
        assert_eq!(
            RdmaListenerConfig::default().backlog_capacity(),
            DEFAULT_LISTENER_BACKLOG
        );
    }

    #[test]
    fn waiter_registration_reports_the_listener_failure_context() {
        let listener = ListenerState::test_only(1);
        *lock_unpoison(&listener.failure) =
            Some(Error::InvalidConfig("listener CM close failed".into()));
        listener.closing.store(true, Ordering::Release);

        let error = listener.register_waiter(request()).unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));
        assert!(error.to_string().contains("listener CM close failed"));
    }

    #[test]
    fn waiter_registration_and_child_arrival_order_are_exact() {
        let listener = ListenerState::test_only(2);
        let first = request();
        let second = request();
        listener.register_waiter(Arc::clone(&first)).unwrap();
        listener.register_waiter(Arc::clone(&second)).unwrap();

        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        match listener.next_action() {
            ListenerAction::ProcessSelected { request, .. } => {
                assert!(Arc::ptr_eq(&request, &first));
            }
            _ => panic!("oldest waiter must receive the oldest child"),
        }

        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        assert!(matches!(listener.next_action(), ListenerAction::None));
        assert!(listener.finish_selected_request(&first));
        match listener.next_action() {
            ListenerAction::ProcessSelected { request, .. } => {
                assert!(Arc::ptr_eq(&request, &second));
            }
            _ => panic!("later waiter overtook the selected pair"),
        }
    }

    #[test]
    fn delivered_accept_defensively_releases_selection_after_route_retirement() {
        let (engine, driver) =
            super::super::test_engine_pair(super::super::CompletionMode::Polling);
        let listener = ListenerState::test_only(1);
        let request = request();
        listener.register_waiter(Arc::clone(&request)).unwrap();
        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        assert!(matches!(
            listener.next_action(),
            ListenerAction::ProcessSelected { .. }
        ));
        request.set_route_token(42);
        listener.route_selected(&request, 42).unwrap();
        assert!(lock_unpoison(&listener.queues).selected.is_some());

        engine.shared.cm.mark_accept_delivered(&listener, &request);
        assert!(lock_unpoison(&listener.queues).selected.is_none());

        drop(engine);
        drop(driver);
    }

    #[test]
    fn cancellation_before_selection_removes_only_that_waiter() {
        let listener = ListenerState::test_only(2);
        let cancelled = request();
        let survivor = request();
        listener.register_waiter(Arc::clone(&cancelled)).unwrap();
        listener.register_waiter(Arc::clone(&survivor)).unwrap();
        cancelled.cancel();

        match listener.next_action() {
            ListenerAction::CancelledBeforeSelection(request) => {
                assert!(Arc::ptr_eq(&request, &cancelled));
            }
            _ => panic!("cancelled waiter was not removed before selection"),
        }
        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        match listener.next_action() {
            ListenerAction::ProcessSelected { request, .. } => {
                assert!(Arc::ptr_eq(&request, &survivor));
            }
            _ => panic!("surviving waiter lost its registration order"),
        }
    }

    #[test]
    fn userspace_backlog_and_listener_state_are_independent() {
        let first = ListenerState::test_only(2);
        let second = ListenerState::test_only(2);
        for listener in [&first, &second] {
            assert!(
                listener
                    .admit_child(IncomingChild::test_only())
                    .rejected
                    .is_none()
            );
            assert!(
                listener
                    .admit_child(IncomingChild::test_only())
                    .rejected
                    .is_none()
            );
            let overflow = listener.admit_child(IncomingChild::test_only());
            assert!(matches!(
                overflow.rejected,
                Some((_, InboundRejectReason::BacklogFull))
            ));
        }
        let first_waiter = request();
        first.register_waiter(Arc::clone(&first_waiter)).unwrap();
        assert!(matches!(
            first.next_action(),
            ListenerAction::ProcessSelected { .. }
        ));
        assert!(matches!(
            second.admit_child(IncomingChild::test_only()).rejected,
            Some((_, InboundRejectReason::BacklogFull))
        ));
    }

    #[test]
    fn cancellation_after_accept_blocks_later_selection_until_close_disposition() {
        let listener = ListenerState::test_only(2);
        let first = request();
        let second = request();
        listener.register_waiter(Arc::clone(&first)).unwrap();
        listener.register_waiter(Arc::clone(&second)).unwrap();
        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        match listener.next_action() {
            ListenerAction::ProcessSelected { request, .. } => {
                assert!(Arc::ptr_eq(&request, &first));
            }
            _ => panic!("first pair was not selected"),
        }
        listener.route_selected(&first, 7).unwrap();
        first.cancel();
        assert!(matches!(
            listener.next_action(),
            ListenerAction::CancelAfterAccept { route: 7, .. }
        ));
        assert!(matches!(listener.next_action(), ListenerAction::None));
        assert!(listener.finish_selected_route(7));
        match listener.next_action() {
            ListenerAction::ProcessSelected { request, .. } => {
                assert!(Arc::ptr_eq(&request, &second));
            }
            _ => panic!("later waiter did not remain blocked through selected close"),
        }
    }

    #[test]
    fn close_actions_dispose_each_queue_owner_once_before_finalization() {
        let listener = ListenerState::test_only(2);
        let selected = request();
        let pending = request();
        listener.register_waiter(Arc::clone(&selected)).unwrap();
        listener.register_waiter(Arc::clone(&pending)).unwrap();
        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        listener.closing.store(true, Ordering::Release);

        match listener.next_action() {
            ListenerAction::FailUnselected(request) => {
                assert!(Arc::ptr_eq(&request, &pending));
            }
            _ => panic!("close must fail the unselected waiter first"),
        }
        assert!(matches!(
            listener.next_action(),
            ListenerAction::RejectChild(_, InboundRejectReason::ListenerClosed)
        ));
        match listener.next_action() {
            ListenerAction::RejectSelected { request, .. } => {
                assert!(Arc::ptr_eq(&request, &selected));
                assert!(listener.finish_selected_request(&request));
            }
            _ => panic!("close must reject the selected pair exactly once"),
        }
        assert!(matches!(
            listener.next_action(),
            ListenerAction::FinalizeClose
        ));
        assert!(matches!(listener.next_action(), ListenerAction::None));
    }
}
