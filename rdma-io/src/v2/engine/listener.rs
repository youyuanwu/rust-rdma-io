//! Engine-owned listeners and ordered inbound accept arbitration.

use std::collections::VecDeque;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use futures_util::task::AtomicWaker;
use tokio::sync::Notify;

use super::connection::{ConnectionReservation, SharedCmId};
use super::registry::{lock_unpoison, read_unpoison};
use super::{EngineShared, PreEstablishSetup, RdmaConnection, RdmaConnectionConfig, SetupSummary};
use crate::v2::error::{Error, Result};

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

/// Engine-owned inbound listener with no private progress future or resources.
pub struct RdmaListener {
    pub(super) shared: Arc<EngineShared>,
    pub(super) state: Arc<ListenerState>,
}

impl Clone for RdmaListener {
    fn clone(&self) -> Self {
        self.state.frontend_count.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: Arc::clone(&self.shared),
            state: Arc::clone(&self.state),
        }
    }
}

impl RdmaListener {
    pub fn local_addr(&self) -> Result<SocketAddr> {
        Ok(self.state.local_addr)
    }

    pub async fn accept(&self) -> Result<RdmaConnection> {
        accept_with_setup(
            Arc::clone(&self.shared),
            Arc::clone(&self.state),
            RdmaConnectionConfig::default(),
            Box::new(EmptyPreEstablishSetup),
        )
        .await
    }

    pub async fn accept_with_config(&self, config: RdmaConnectionConfig) -> Result<RdmaConnection> {
        accept_with_setup(
            Arc::clone(&self.shared),
            Arc::clone(&self.state),
            config,
            Box::new(EmptyPreEstablishSetup),
        )
        .await
    }

    pub async fn close(&self) -> Result<()> {
        self.state.request_close(&self.shared);
        loop {
            let notified = self.state.close_notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(result) = self.state.close_result() {
                return result;
            }
            if let Some(outcome) = self.shared.outcome() {
                return outcome.into_result();
            }
            notified.await;
        }
    }

    #[cfg(any(test, feature = "test-hooks"))]
    #[doc(hidden)]
    pub async fn accept_with_test_setup_failure(
        &self,
        message: impl Into<String>,
    ) -> Result<RdmaConnection> {
        accept_with_setup(
            Arc::clone(&self.shared),
            Arc::clone(&self.state),
            RdmaConnectionConfig::default(),
            Box::new(FailingPreEstablishSetup(message.into())),
        )
        .await
    }
}

impl Drop for RdmaListener {
    fn drop(&mut self) {
        let previous = self.state.frontend_count.fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "listener frontend count must be positive");
        if previous == 1 {
            self.state.request_close(&self.shared);
        }
    }
}

pub(super) async fn listen(
    shared: Arc<EngineShared>,
    address: SocketAddr,
    config: RdmaListenerConfig,
) -> Result<RdmaListener> {
    config.validate()?;
    let admission = read_unpoison(&shared.admission);
    if let Some(error) = shared.admission_error() {
        return Err(error);
    }
    let request = Arc::new(ListenRequest::new(address, config));
    shared.cm.enqueue_listen(Arc::clone(&request));
    drop(admission);
    shared.work_signal.publish(super::cm::CM_WORK);
    ListenWaiter {
        shared,
        request,
        finished: false,
    }
    .await
}

pub(super) async fn accept_with_setup(
    shared: Arc<EngineShared>,
    listener: Arc<ListenerState>,
    config: RdmaConnectionConfig,
    setup: Box<dyn PreEstablishSetup>,
) -> Result<RdmaConnection> {
    config.validate(&shared.config, shared.provider.as_ref())?;
    let admission = read_unpoison(&shared.admission);
    if let Some(error) = shared.admission_error() {
        return Err(error);
    }
    let request = Arc::new(AcceptRequest::new(AcceptIntent::new(config, setup)));
    listener.register_waiter(Arc::clone(&request))?;
    drop(admission);
    shared.cm.enqueue_listener_work(&listener);
    shared.work_signal.publish(super::cm::CM_WORK);
    AcceptWaiter {
        shared,
        listener,
        request,
        finished: false,
    }
    .await
}

pub(super) struct EmptyPreEstablishSetup;

impl PreEstablishSetup for EmptyPreEstablishSetup {
    fn run(self: Box<Self>, _connection: &RdmaConnection) -> Result<SetupSummary> {
        Ok(SetupSummary { posted_wrs: 0 })
    }
}

#[cfg(any(test, feature = "test-hooks"))]
struct FailingPreEstablishSetup(String);

#[cfg(any(test, feature = "test-hooks"))]
impl PreEstablishSetup for FailingPreEstablishSetup {
    fn run(self: Box<Self>, _connection: &RdmaConnection) -> Result<SetupSummary> {
        Err(Error::InvalidConfig(self.0))
    }
}

pub(super) fn run_setup_before_establish(
    setup: Box<dyn PreEstablishSetup>,
    connection: &RdmaConnection,
    before_establish: impl FnOnce() -> Result<()>,
    establish: impl FnOnce() -> Result<()>,
) -> Result<SetupSummary> {
    let accepted_before = connection.state.accepted_count();
    let summary = setup.run(connection)?;
    let accepted_after = connection.state.accepted_count();
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
    setup: Option<Box<dyn PreEstablishSetup>>,
}

impl AcceptIntent {
    fn new(config: RdmaConnectionConfig, setup: Box<dyn PreEstablishSetup>) -> Self {
        Self {
            config,
            setup: Some(setup),
        }
    }

    pub(super) fn into_parts(
        mut self,
    ) -> Result<(RdmaConnectionConfig, Box<dyn PreEstablishSetup>)> {
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
    fn test_only() -> Self {
        Self {
            cm_id: None,
            reservation: None,
        }
    }
}

pub(super) struct ListenRequest {
    pub(super) address: SocketAddr,
    pub(super) config: RdmaListenerConfig,
    result: Mutex<ListenResult>,
    cancelled: AtomicBool,
    waker: AtomicWaker,
}

impl ListenRequest {
    fn new(address: SocketAddr, config: RdmaListenerConfig) -> Self {
        Self {
            address,
            config,
            result: Mutex::new(ListenResult::Pending),
            cancelled: AtomicBool::new(false),
            waker: AtomicWaker::new(),
        }
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn complete(&self, result: Result<RdmaListener>) {
        let mut current = lock_unpoison(&self.result);
        if matches!(&*current, ListenResult::Pending) {
            *current = ListenResult::Ready(result);
            drop(current);
            self.waker.wake();
        }
    }

    fn take_result(&self) -> Option<Result<RdmaListener>> {
        let mut current = lock_unpoison(&self.result);
        match std::mem::replace(&mut *current, ListenResult::Taken) {
            ListenResult::Ready(result) => Some(result),
            ListenResult::Pending => {
                *current = ListenResult::Pending;
                None
            }
            ListenResult::Taken => None,
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut current = lock_unpoison(&self.result);
        let replacement = match std::mem::replace(&mut *current, ListenResult::Taken) {
            ListenResult::Pending => ListenResult::Ready(Err(Error::DriverShutdown)),
            ListenResult::Ready(Ok(listener)) => {
                drop(listener);
                ListenResult::Ready(Err(Error::DriverShutdown))
            }
            ListenResult::Ready(Err(error)) => ListenResult::Ready(Err(error)),
            ListenResult::Taken => ListenResult::Taken,
        };
        *current = replacement;
        drop(current);
        self.waker.wake();
    }
}

enum ListenResult {
    Pending,
    Ready(Result<RdmaListener>),
    Taken,
}

struct ListenWaiter {
    shared: Arc<EngineShared>,
    request: Arc<ListenRequest>,
    finished: bool,
}

impl Future for ListenWaiter {
    type Output = Result<RdmaListener>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.request.take_result() {
            self.finished = true;
            return Poll::Ready(result);
        }
        self.request.waker.register(cx.waker());
        if let Some(result) = self.request.take_result() {
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
        self.request.cancel();
        self.shared.work_signal.publish(super::cm::CM_WORK);
    }
}

pub(super) struct AcceptRequest {
    intent: Mutex<Option<AcceptIntent>>,
    result: Mutex<AcceptResult>,
    cancelled: AtomicBool,
    cancellation_counted: AtomicBool,
    delivered: AtomicBool,
    route_token: AtomicU64,
    waker: AtomicWaker,
}

impl AcceptRequest {
    fn new(intent: AcceptIntent) -> Self {
        Self {
            intent: Mutex::new(Some(intent)),
            result: Mutex::new(AcceptResult::Pending),
            cancelled: AtomicBool::new(false),
            cancellation_counted: AtomicBool::new(false),
            delivered: AtomicBool::new(false),
            route_token: AtomicU64::new(0),
            waker: AtomicWaker::new(),
        }
    }

    pub(super) fn take_intent(&self) -> Option<AcceptIntent> {
        lock_unpoison(&self.intent).take()
    }

    pub(super) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn mark_cancellation_counted(&self) -> bool {
        !self.cancellation_counted.swap(true, Ordering::AcqRel)
    }

    pub(super) fn set_route_token(&self, token: u64) {
        self.route_token.store(token, Ordering::Release);
    }

    pub(super) fn route_token(&self) -> u64 {
        self.route_token.load(Ordering::Acquire)
    }

    pub(super) fn is_delivered(&self) -> bool {
        self.delivered.load(Ordering::Acquire)
    }

    pub(super) fn complete(&self, result: Result<RdmaConnection>) {
        let mut current = lock_unpoison(&self.result);
        if matches!(&*current, AcceptResult::Pending) {
            *current = AcceptResult::Ready(result);
            drop(current);
            self.waker.wake();
        }
    }

    pub(super) fn complete_success(&self, connection: RdmaConnection) {
        let mut current = lock_unpoison(&self.result);
        if self.cancelled.load(Ordering::Acquire) || !matches!(&*current, AcceptResult::Pending) {
            drop(current);
            drop(connection);
            return;
        }
        *current = AcceptResult::Ready(Ok(connection));
        drop(current);
        self.waker.wake();
    }

    pub(super) fn fail_undelivered(&self, error: Error) -> bool {
        let mut current = lock_unpoison(&self.result);
        let replacement = match std::mem::replace(&mut *current, AcceptResult::Taken) {
            AcceptResult::Pending | AcceptResult::Ready(Ok(_)) => AcceptResult::Ready(Err(error)),
            AcceptResult::Ready(Err(existing)) => AcceptResult::Ready(Err(existing)),
            AcceptResult::Taken => AcceptResult::Taken,
        };
        *current = replacement;
        drop(current);
        self.waker.wake();
        self.is_delivered()
    }

    fn take_result(&self) -> Option<Result<RdmaConnection>> {
        let mut current = lock_unpoison(&self.result);
        match std::mem::replace(&mut *current, AcceptResult::Taken) {
            AcceptResult::Ready(result) => {
                if result.is_ok() {
                    self.delivered.store(true, Ordering::Release);
                }
                Some(result)
            }
            AcceptResult::Pending => {
                *current = AcceptResult::Pending;
                None
            }
            AcceptResult::Taken => None,
        }
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        let mut current = lock_unpoison(&self.result);
        let replacement = match std::mem::replace(&mut *current, AcceptResult::Taken) {
            AcceptResult::Pending => AcceptResult::Pending,
            AcceptResult::Ready(Ok(connection)) => {
                drop(connection);
                AcceptResult::Taken
            }
            AcceptResult::Ready(Err(error)) => AcceptResult::Ready(Err(error)),
            AcceptResult::Taken => AcceptResult::Taken,
        };
        *current = replacement;
        drop(current);
        self.waker.wake();
    }
}

enum AcceptResult {
    Pending,
    Ready(Result<RdmaConnection>),
    Taken,
}

struct AcceptWaiter {
    shared: Arc<EngineShared>,
    listener: Arc<ListenerState>,
    request: Arc<AcceptRequest>,
    finished: bool,
}

impl Future for AcceptWaiter {
    type Output = Result<RdmaConnection>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(result) = self.request.take_result() {
            if result.is_ok() {
                self.shared.cm.mark_accept_delivered(&self.request);
                self.shared.work_signal.publish(super::cm::CM_WORK);
            }
            self.finished = true;
            return Poll::Ready(result);
        }
        self.request.waker.register(cx.waker());
        if let Some(result) = self.request.take_result() {
            if result.is_ok() {
                self.shared.cm.mark_accept_delivered(&self.request);
                self.shared.work_signal.publish(super::cm::CM_WORK);
            }
            self.finished = true;
            return Poll::Ready(result);
        }
        Poll::Pending
    }
}

impl Drop for AcceptWaiter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.request.cancel();
        self.shared.cm.enqueue_listener_work(&self.listener);
        self.shared.work_signal.publish(super::cm::CM_WORK);
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
    failure: Mutex<Option<Arc<str>>>,
    close_outcome: Mutex<Option<ListenerCloseOutcome>>,
    pub(super) close_notify: Notify,
    pub(super) frontend_count: AtomicUsize,
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
            close_outcome: Mutex::new(None),
            close_notify: Notify::new(),
            frontend_count: AtomicUsize::new(1),
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
            close_outcome: Mutex::new(None),
            close_notify: Notify::new(),
            frontend_count: AtomicUsize::new(1),
            work_enqueued: AtomicBool::new(false),
        })
    }

    pub(super) fn register_waiter(&self, request: Arc<AcceptRequest>) -> Result<()> {
        if self.closing.load(Ordering::Acquire) {
            return Err(Error::TransportClosed);
        }
        let mut queues = lock_unpoison(&self.queues);
        if self.closing.load(Ordering::Acquire) {
            return Err(Error::TransportClosed);
        }
        queues.waiters.push_back(request);
        select_pair(&mut queues);
        Ok(())
    }

    pub(super) fn admit_child(&self, child: IncomingChild) -> ChildAdmission {
        let mut queues = lock_unpoison(&self.queues);
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
        let mut queues = lock_unpoison(&self.queues);
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
        let mut queues = lock_unpoison(&self.queues);
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
        let mut queues = lock_unpoison(&self.queues);
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
        let mut queues = lock_unpoison(&self.queues);
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
            shared.cm.enqueue_listener_work(self);
            shared.work_signal.publish(super::cm::CM_WORK);
        }
    }

    pub(super) fn fail(self: &Arc<Self>, shared: &Arc<EngineShared>, message: String) {
        let mut failure = lock_unpoison(&self.failure);
        if failure.is_none() {
            *failure = Some(Arc::from(message));
        }
        drop(failure);
        self.request_close(shared);
    }

    pub(super) fn close_error(&self) -> Error {
        match lock_unpoison(&self.failure).clone() {
            Some(message) => Error::Verbs(std::io::Error::other(message.to_string())),
            None => Error::TransportClosed,
        }
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
        let queues = lock_unpoison(&self.queues);
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

    pub(super) fn finish_close(&self, error: Option<String>) {
        let mut outcome = lock_unpoison(&self.close_outcome);
        if outcome.is_none() {
            let failure = error
                .map(Arc::from)
                .or_else(|| lock_unpoison(&self.failure).clone());
            *outcome = Some(match failure {
                Some(message) => ListenerCloseOutcome::Failed(message),
                None => ListenerCloseOutcome::Success,
            });
        }
        drop(outcome);
        self.close_notify.notify_waiters();
    }

    pub(super) fn terminalize(&self, outcome: &super::EngineOutcome) {
        self.closing.store(true, Ordering::Release);
        let mut queues = lock_unpoison(&self.queues);
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
        self.close_notify.notify_waiters();
    }

    fn close_result(&self) -> Option<Result<()>> {
        lock_unpoison(&self.close_outcome)
            .clone()
            .map(ListenerCloseOutcome::into_result)
    }

    pub(super) fn queue_counts(&self) -> (usize, usize, usize) {
        let queues = lock_unpoison(&self.queues);
        (
            queues.children.len(),
            queues.waiters.len(),
            usize::from(queues.selected.is_some()),
        )
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

#[derive(Clone)]
enum ListenerCloseOutcome {
    Success,
    Failed(Arc<str>),
}

impl ListenerCloseOutcome {
    fn into_result(self) -> Result<()> {
        match self {
            Self::Success => Ok(()),
            Self::Failed(message) => Err(Error::Verbs(std::io::Error::other(message.to_string()))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> Arc<AcceptRequest> {
        Arc::new(AcceptRequest::new(AcceptIntent::new(
            RdmaConnectionConfig::default(),
            Box::new(EmptyPreEstablishSetup),
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
        assert_eq!(first.queue_counts(), (2, 0, 0));
        assert_eq!(second.queue_counts(), (2, 0, 0));

        let first_waiter = request();
        first.register_waiter(Arc::clone(&first_waiter)).unwrap();
        assert!(matches!(
            first.next_action(),
            ListenerAction::ProcessSelected { .. }
        ));
        assert_eq!(second.queue_counts(), (2, 0, 0));
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
