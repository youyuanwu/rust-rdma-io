//! Engine-owned listeners and ordered inbound accept arbitration.

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::task::{Context, Poll};

use super::super::io::{BorrowedSetupIo, IoEventReceiver};
use super::super::lifecycle::{MemoizedTerminalResult, TakeOnceResult};
use super::super::reactor::CommandIngress;
use super::super::reactor::completion::CommandCompletion;
use super::super::registry::{ListenerToken, Lookup, PagedRegistry, lock_unpoison, read_unpoison};
use super::super::{ConnectionSetup, RdmaConnection, RdmaConnectionConfig, SetupSummary};
use super::connection::SharedCmId;
use super::{SessionFrontend, SessionListener, SessionListenerCloseState, SessionManager};
use crate::v2::error::{Error, Result};
use futures_util::task::AtomicWaker;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

pub(crate) const DEFAULT_LISTENER_BACKLOG: usize = 128;
const MAX_LISTENER_BACKLOG: usize = 4_096;

/// Configuration for one engine-owned listener.
///
/// The configured value is the single listener backlog contract. It must be in
/// `1..=4096`, is passed unchanged to `rdma_listen`, and bounds both admitted
/// accept requests and pending inbound children.
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
    /// Store the listener backlog for validation by `listen`.
    ///
    /// Valid values are `1..=4096`. This setter deliberately does not validate,
    /// panic, or clamp.
    pub fn backlog(mut self, value: usize) -> Self {
        self.backlog = value;
        self
    }

    /// Return the validated provider and userspace backlog bound.
    pub fn backlog_capacity(&self) -> usize {
        self.backlog
    }

    pub(in crate::v2::engine) fn validate(&self) -> Result<()> {
        if !(1..=MAX_LISTENER_BACKLOG).contains(&self.backlog) {
            return Err(Error::InvalidConfig(format!(
                "listener backlog {} is outside 1..={MAX_LISTENER_BACKLOG}",
                self.backlog
            )));
        }
        Ok(())
    }
}

pub(in crate::v2::engine) fn with_validated_listener_backlog<T>(
    config: &RdmaListenerConfig,
    provider_call: impl FnOnce(i32) -> T,
) -> Result<T> {
    config.validate()?;
    let backlog = i32::try_from(config.backlog_capacity())
        .map_err(|_| Error::InvalidConfig("listener backlog does not fit provider ABI".into()))?;
    Ok(provider_call(backlog))
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
        let (frontend, commands, token, admission) = self.session.owners()?;
        accept_with_setup(
            frontend,
            commands,
            token,
            admission,
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
        let (frontend, commands, token, admission) = self.session.owners()?;
        accept_with_setup(
            frontend,
            commands,
            token,
            admission,
            config,
            empty_connection_setup(),
        )
        .await
    }

    pub(crate) async fn accept_with_io_setup<F>(
        &self,
        config: RdmaConnectionConfig,
        setup: F,
    ) -> Result<RdmaConnection>
    where
        F: for<'a> FnOnce(BorrowedSetupIo<'a>, IoEventReceiver) -> Result<usize> + Send + 'static,
    {
        let (frontend, commands, token, admission) = self.session.owners()?;
        accept_with_setup(
            frontend,
            commands,
            token,
            admission,
            config,
            Box::new(setup),
        )
        .await
    }

    pub(crate) fn validate_message_connection_config(
        &self,
        config: &RdmaConnectionConfig,
    ) -> Result<()> {
        let (frontend, _, _, _) = self.session.owners()?;
        frontend.validate_connection_config(config)
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

    pub(in crate::v2::engine) fn from_state(
        manager: &SessionManager,
        state: &ListenerEntry,
    ) -> Self {
        Self {
            session: manager.listener_capability(state),
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

pub(in crate::v2::engine) async fn listen(
    frontend: Arc<SessionFrontend>,
    commands: Arc<CommandIngress>,
    address: SocketAddr,
    config: RdmaListenerConfig,
) -> Result<RdmaListener> {
    config.validate()?;
    let permit = commands
        .acquire_listen()
        .await
        .ok_or_else(|| frontend.admission_error().unwrap_or(Error::DriverShutdown))?;
    let admission = read_unpoison(&frontend.admission);
    if let Some(error) = frontend.admission_error() {
        return Err(error);
    }
    let request = Arc::new(ListenRequest::new(address, config));
    commands.enqueue_listen(Arc::clone(&request), permit);
    drop(admission);
    commands.publish_command_work();
    let waiter = ListenWaiter {
        frontend: Arc::downgrade(&frontend),
        commands: Arc::downgrade(&commands),
        request: Arc::downgrade(&request),
        observer: Arc::clone(&request.observer),
        finished: false,
    };
    drop(request);
    drop(frontend);
    CommandIngress::yield_after_admission().await;
    waiter.await
}

pub(in crate::v2::engine) async fn accept_with_setup(
    frontend: Arc<SessionFrontend>,
    commands: Arc<CommandIngress>,
    listener: ListenerToken,
    listener_admission: Arc<ListenerAdmission>,
    config: RdmaConnectionConfig,
    setup: ConnectionSetup,
) -> Result<RdmaConnection> {
    frontend.validate_connection_config(&config)?;
    let permit = listener_admission
        .acquire()
        .await
        .ok_or_else(|| listener_admission.close_error())?;
    let admission = read_unpoison(&frontend.admission);
    if let Some(error) = frontend.admission_error() {
        return Err(error);
    }
    if !listener_admission.is_open() {
        return Err(listener_admission.close_error());
    }
    let request = Arc::new(AcceptRequest::new(AcceptIntent::new(config, setup), permit));
    commands.enqueue_accept(listener, Arc::clone(&request));
    drop(admission);
    commands.publish_command_work();
    let waiter = AcceptWaiter {
        commands: Arc::downgrade(&commands),
        listener,
        request: Arc::downgrade(&request),
        observer: Arc::clone(&request.observer),
        finished: false,
    };
    drop(request);
    drop(listener_admission);
    drop(commands);
    drop(frontend);
    CommandIngress::yield_after_admission().await;
    waiter.await
}

pub(in crate::v2::engine) fn empty_connection_setup() -> ConnectionSetup {
    Box::new(|_connection, _events| Ok(0))
}

pub(in crate::v2::engine) fn run_setup_before_establish(
    setup: ConnectionSetup,
    connection: &RdmaConnection,
    connections: &mut super::registry::ConnectionRegistry,
    connection_token: super::super::registry::ConnectionToken,
    io_core: &mut super::super::io_core::IoState,
    actions: &mut crate::v2::engine::reactor::ReactorActions,
    before_establish: impl FnOnce() -> Result<()>,
    establish: impl FnOnce(&mut super::registry::ConnectionRegistry) -> Result<()>,
) -> Result<SetupSummary> {
    let accepted_before = connections.accepted_count(connection_token);
    let mut setup_actions =
        crate::v2::engine::reactor::ReactorActions::for_synchronous_driver_drop();
    let setup_result = connections.with_connection_mut(connection_token, |connection_state| {
        let (io, events) = super::super::io::BorrowedSetupIo::from_connection(
            connection,
            connection_state,
            io_core,
            &mut setup_actions,
        )?;
        Ok::<_, Error>(SetupSummary {
            posted_wrs: setup(io, events)?,
        })
    });
    setup_actions.append_setup_result_to(actions);
    let summary = setup_result.ok_or(Error::TransportClosed)??;
    let accepted_after = connections.accepted_count(connection_token);
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
    establish(connections)?;
    Ok(summary)
}

pub(in crate::v2::engine) struct AcceptIntent {
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

    pub(in crate::v2::engine) fn into_parts(
        mut self,
    ) -> Result<(RdmaConnectionConfig, ConnectionSetup)> {
        let setup = self.setup.take().ok_or_else(|| {
            Error::InvalidConfig("accept setup was consumed more than once".into())
        })?;
        Ok((self.config, setup))
    }
}

pub(in crate::v2::engine) struct IncomingChild {
    pub(in crate::v2::engine) token: super::super::registry::ConnectionToken,
}

impl IncomingChild {
    pub(in crate::v2::engine) fn new(token: super::super::registry::ConnectionToken) -> Self {
        Self { token }
    }

    pub(in crate::v2::engine) fn into_token(self) -> super::super::registry::ConnectionToken {
        self.token
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn test_only() -> Self {
        Self {
            token: super::super::registry::ConnectionToken {
                slot: u32::MAX,
                generation: 1,
            },
        }
    }
}

pub(in crate::v2::engine) struct ListenRequest {
    pub(in crate::v2::engine) address: SocketAddr,
    pub(in crate::v2::engine) config: RdmaListenerConfig,
    observer: Arc<ListenRequestObserver>,
}

struct ListenRequestObserver {
    completion: CommandCompletion<RdmaListener>,
}

impl ListenRequest {
    pub(in crate::v2::engine) fn new(address: SocketAddr, config: RdmaListenerConfig) -> Self {
        Self {
            address,
            config,
            observer: Arc::new(ListenRequestObserver {
                completion: CommandCompletion::new(),
            }),
        }
    }

    pub(in crate::v2::engine) fn is_cancelled(&self) -> bool {
        self.observer.completion.is_cancelled()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn complete(&self, result: Result<RdmaListener>) {
        self.observer.completion.complete_listener(result);
    }

    pub(in crate::v2::engine) fn complete_into(
        &self,
        result: Result<RdmaListener>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        self.observer
            .completion
            .complete_into(result, true, actions);
    }
}

impl ListenRequestObserver {
    fn take_result(&self) -> Option<Result<RdmaListener>> {
        self.completion.take_result()
    }

    fn cancel(&self) {
        drop(self.completion.cancel(Error::DriverShutdown));
    }
}

struct ListenWaiter {
    frontend: Weak<SessionFrontend>,
    commands: Weak<CommandIngress>,
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
        self.observer.completion.register(cx.waker());
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
        let Some(request) = self.request.upgrade() else {
            return;
        };
        if let Some(commands) = self.commands.upgrade()
            && commands.cancel_listen(&request)
        {
            return;
        }
        if let Some(frontend) = self.frontend.upgrade() {
            frontend.publish_session_work();
        }
    }
}

pub(in crate::v2::engine) struct AcceptRequest {
    intent: Mutex<Option<AcceptIntent>>,
    observer: Arc<AcceptRequestObserver>,
    route_token: AtomicU64,
    permit: Mutex<Option<OwnedSemaphorePermit>>,
}

struct AcceptRequestObserver {
    result: Mutex<TakeOnceResult<RdmaConnection>>,
    cancelled: AtomicBool,
    delivered: AtomicBool,
    waker: Arc<AtomicWaker>,
}

pub(in crate::v2::engine) struct UndeliveredAcceptFailure {
    delivered: bool,
    connection: Option<RdmaConnection>,
}

impl UndeliveredAcceptFailure {
    pub(in crate::v2::engine) fn delivered(&self) -> bool {
        self.delivered
    }

    pub(in crate::v2::engine) fn into_connection(self) -> Option<RdmaConnection> {
        self.connection
    }
}

impl AcceptRequest {
    fn new(intent: AcceptIntent, permit: OwnedSemaphorePermit) -> Self {
        Self {
            intent: Mutex::new(Some(intent)),
            observer: Arc::new(AcceptRequestObserver {
                result: Mutex::new(TakeOnceResult::Pending),
                cancelled: AtomicBool::new(false),
                delivered: AtomicBool::new(false),
                waker: Arc::new(AtomicWaker::new()),
            }),
            route_token: AtomicU64::new(0),
            permit: Mutex::new(Some(permit)),
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn test_only() -> Arc<Self> {
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits
            .try_acquire_owned()
            .expect("test accept permit is available");
        Arc::new(Self::new(
            AcceptIntent::new(RdmaConnectionConfig::default(), empty_connection_setup()),
            permit,
        ))
    }

    pub(in crate::v2::engine) fn take_intent(&self) -> Option<AcceptIntent> {
        lock_unpoison(&self.intent).take()
    }

    pub(in crate::v2::engine) fn is_cancelled(&self) -> bool {
        self.observer.cancelled.load(Ordering::Acquire)
    }

    pub(in crate::v2::engine) fn set_route_token(&self, token: u64) {
        self.route_token.store(token, Ordering::Release);
    }

    pub(in crate::v2::engine) fn is_delivered(&self) -> bool {
        self.observer.delivered.load(Ordering::Acquire)
    }

    pub(in crate::v2::engine) fn release_permit(&self) -> bool {
        lock_unpoison(&self.permit).take().is_some()
    }

    #[cfg(test)]
    fn owns_permit(&self) -> bool {
        lock_unpoison(&self.permit).is_some()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn complete(&self, result: Result<RdmaConnection>) {
        let mut current = lock_unpoison(&self.observer.result);
        if matches!(&*current, TakeOnceResult::Pending) {
            *current = TakeOnceResult::Ready(result);
            drop(current);
            self.observer.waker.wake();
        }
    }

    pub(in crate::v2::engine) fn complete_into(
        &self,
        result: Result<RdmaConnection>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let mut current = lock_unpoison(&self.observer.result);
        if matches!(&*current, TakeOnceResult::Pending) {
            *current = TakeOnceResult::Ready(result);
            drop(current);
            let waker = Arc::clone(&self.observer.waker);
            actions.push_close_or_listener(move || waker.wake());
        }
    }

    pub(in crate::v2::engine) fn complete_success_into(
        &self,
        connection: RdmaConnection,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let mut current = lock_unpoison(&self.observer.result);
        if !matches!(&*current, TakeOnceResult::Pending) {
            drop(current);
            drop(connection);
            return;
        }
        *current = TakeOnceResult::Ready(Ok(connection));
        if self.observer.cancelled.load(Ordering::Acquire) {
            return;
        }
        drop(current);
        let waker = Arc::clone(&self.observer.waker);
        actions.push_close_or_listener(move || waker.wake());
    }

    pub(in crate::v2::engine) fn claim_undelivered_failure_into(
        &self,
        error: Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> UndeliveredAcceptFailure {
        let mut current = lock_unpoison(&self.observer.result);
        let mut connection = None;
        let replacement = match std::mem::replace(&mut *current, TakeOnceResult::Taken) {
            TakeOnceResult::Pending => TakeOnceResult::Ready(Err(error)),
            TakeOnceResult::Ready(Ok(value)) => {
                connection = Some(value);
                TakeOnceResult::Ready(Err(error))
            }
            TakeOnceResult::Ready(Err(existing)) => TakeOnceResult::Ready(Err(existing)),
            TakeOnceResult::Taken => TakeOnceResult::Taken,
        };
        *current = replacement;
        let delivered = self.is_delivered();
        drop(current);
        let waker = Arc::clone(&self.observer.waker);
        actions.push_close_or_listener(move || waker.wake());
        UndeliveredAcceptFailure {
            delivered,
            connection,
        }
    }

    pub(in crate::v2::engine) fn fail_undelivered_into(
        &self,
        error: Error,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) -> bool {
        let failure = self.claim_undelivered_failure_into(error, actions);
        let delivered = failure.delivered();
        drop(failure);
        delivered
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn cancel(&self) {
        self.observer.cancel();
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn take_result_for_test(&self) -> Option<Result<RdmaConnection>> {
        self.observer.take_result()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn has_undelivered_success_for_test(&self) -> bool {
        matches!(
            &*lock_unpoison(&self.observer.result),
            TakeOnceResult::Ready(Ok(_))
        ) && !self.is_delivered()
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
        let replacement = std::mem::replace(&mut *current, TakeOnceResult::Taken);
        *current = replacement;
        drop(current);
        self.waker.wake();
    }
}

struct AcceptWaiter {
    commands: Weak<CommandIngress>,
    listener: ListenerToken,
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
        if self.request.upgrade().is_none() {
            return;
        }
        if let Some(commands) = self.commands.upgrade() {
            commands.request_listener_work(self.listener);
        }
    }
}

impl Drop for AcceptWaiter {
    fn drop(&mut self) {
        if self.finished {
            return;
        }
        self.observer.cancel();
        let Some(request) = self.request.upgrade() else {
            return;
        };
        if let Some(commands) = self.commands.upgrade() {
            if commands.cancel_accept(&request) {
                return;
            }
            commands.request_listener_work(self.listener);
        }
    }
}

pub(in crate::v2::engine) struct ListenerAdmission {
    token: ListenerToken,
    permits: Arc<Semaphore>,
    open: AtomicBool,
    close_reason: Mutex<Option<Error>>,
    close: Arc<SessionListenerCloseState>,
}

impl ListenerAdmission {
    fn new(
        token: ListenerToken,
        backlog: usize,
        close: Arc<SessionListenerCloseState>,
    ) -> Arc<Self> {
        Arc::new(Self {
            token,
            permits: Arc::new(Semaphore::new(backlog)),
            open: AtomicBool::new(true),
            close_reason: Mutex::new(None),
            close,
        })
    }

    pub(in crate::v2::engine) fn token(&self) -> ListenerToken {
        self.token
    }

    pub(in crate::v2::engine) async fn acquire(&self) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.permits).acquire_owned().await.ok()
    }

    pub(in crate::v2::engine) fn is_open(&self) -> bool {
        self.open.load(Ordering::Acquire)
    }

    pub(in crate::v2::engine) fn is_terminal(&self) -> bool {
        self.close.outcome().is_some()
    }

    pub(in crate::v2::engine) fn close(&self) {
        if self.open.swap(false, Ordering::AcqRel) {
            self.permits.close();
        }
    }

    pub(in crate::v2::engine) fn close_with_error(&self, error: Error) {
        let mut reason = lock_unpoison(&self.close_reason);
        if reason.is_none() {
            *reason = Some(error);
        }
        self.close();
    }

    pub(in crate::v2::engine) fn close_error(&self) -> Error {
        lock_unpoison(&self.close_reason)
            .clone()
            .or_else(|| {
                self.close
                    .outcome()
                    .and_then(|outcome| outcome.into_result().err())
            })
            .unwrap_or(Error::TransportClosed)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn available_permits(&self) -> usize {
        self.permits.available_permits()
    }
}

pub(in crate::v2::engine) struct ListenerEntry {
    pub(in crate::v2::engine) token: ListenerToken,
    requested_addr: SocketAddr,
    pub(in crate::v2::engine) local_addr: Option<SocketAddr>,
    pub(in crate::v2::engine) backlog: usize,
    pub(in crate::v2::engine) cm_id: Option<SharedCmId>,
    cm_destruction_pending: bool,
    raw_id: usize,
    context_key: usize,
    queues: ListenerQueues,
    child_slots_used: usize,
    closing: bool,
    finalization_started: bool,
    failure: Option<Error>,
    close: Arc<SessionListenerCloseState>,
    admission: Arc<ListenerAdmission>,
    work_enqueued: bool,
}

impl ListenerEntry {
    fn creating(token: ListenerToken, address: SocketAddr, config: RdmaListenerConfig) -> Self {
        let backlog = config.backlog;
        let close = SessionListenerCloseState::new();
        Self {
            token,
            requested_addr: address,
            local_addr: None,
            backlog,
            cm_id: None,
            cm_destruction_pending: false,
            raw_id: 0,
            context_key: 0,
            queues: ListenerQueues::default(),
            child_slots_used: 0,
            closing: false,
            finalization_started: false,
            failure: None,
            close: Arc::clone(&close),
            admission: ListenerAdmission::new(token, backlog, close),
            work_enqueued: false,
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn test_only(backlog: usize) -> Self {
        let token = ListenerToken {
            slot: 0,
            generation: 1,
        };
        let mut entry = Self::creating(
            token,
            "127.0.0.1:1".parse().unwrap(),
            RdmaListenerConfig::default().backlog(backlog),
        );
        entry.local_addr = Some("127.0.0.1:1".parse().unwrap());
        entry
    }

    fn activate(
        &mut self,
        local_addr: SocketAddr,
        cm_id: SharedCmId,
        raw_id: usize,
        context_key: usize,
    ) {
        self.local_addr = Some(local_addr);
        self.cm_id = Some(cm_id);
        self.raw_id = raw_id;
        self.context_key = context_key;
    }

    pub(in crate::v2::engine) fn display_addr(&self) -> SocketAddr {
        self.local_addr.unwrap_or(self.requested_addr)
    }

    pub(in crate::v2::engine) fn raw_id(&self) -> usize {
        self.raw_id
    }

    pub(in crate::v2::engine) fn context_key(&self) -> usize {
        self.context_key
    }

    pub(in crate::v2::engine) fn admission(&self) -> Arc<ListenerAdmission> {
        Arc::clone(&self.admission)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn test_accept_request(&self) -> Arc<AcceptRequest> {
        let permit = Arc::clone(&self.admission.permits)
            .try_acquire_owned()
            .expect("listener test has an accept permit");
        Arc::new(AcceptRequest::new(
            AcceptIntent::new(RdmaConnectionConfig::default(), empty_connection_setup()),
            permit,
        ))
    }

    pub(in crate::v2::engine) fn register_waiter(
        &mut self,
        request: Arc<AcceptRequest>,
    ) -> Result<()> {
        if self.is_closing() {
            return Err(self.close_error());
        }
        debug_assert!(
            self.queues.waiters.len() + usize::from(self.queues.selected.is_some()) < self.backlog,
            "an accept permit bounds every queued or selected request"
        );
        self.queues.waiters.push_back(request);
        select_pair(&mut self.queues);
        Ok(())
    }

    pub(in crate::v2::engine) fn admit_child(&mut self, child: IncomingChild) -> ChildAdmission {
        if self.is_closing() {
            return ChildAdmission {
                rejected: Some((child, InboundRejectReason::ListenerClosed)),
            };
        }
        if self.child_slots_used >= self.backlog {
            return ChildAdmission {
                rejected: Some((child, InboundRejectReason::BacklogFull)),
            };
        }
        self.child_slots_used += 1;
        self.queues.children.push_back(child);
        select_pair(&mut self.queues);
        ChildAdmission { rejected: None }
    }

    pub(in crate::v2::engine) fn next_action(&mut self) -> ListenerAction {
        if let Some(SelectedAccept::Routed {
            request,
            route,
            cancel_started,
        }) = self.queues.selected.as_ref()
        {
            let request = Arc::clone(request);
            let route = *route;
            let cancel_started = *cancel_started;
            // Taking the successful result transfers the connection to the
            // frontend. That transfer is linearized under the observer result
            // lock and must win over a concurrently observed listener close.
            if request.is_delivered() {
                return ListenerAction::AcknowledgeDelivery { request, route };
            }
            if self.is_closing() || request.is_cancelled() {
                if cancel_started {
                    return ListenerAction::None;
                }
                self.queues.selected = Some(SelectedAccept::Routed {
                    request: Arc::clone(&request),
                    route,
                    cancel_started: true,
                });
                return ListenerAction::CancelAfterAccept { request, route };
            }
        }

        if let Some(request) = self
            .queues
            .waiters
            .pop_front_if(|request| request.is_cancelled())
        {
            request.release_permit();
            return ListenerAction::CancelledBeforeSelection(request);
        }

        if self.is_closing() {
            if let Some(request) = self.queues.waiters.pop_front() {
                request.release_permit();
                return ListenerAction::FailUnselected(request);
            }
            if let Some(child) = self.queues.children.pop_front() {
                return ListenerAction::RejectChild(child, InboundRejectReason::ListenerClosed);
            }
            match self.queues.selected.take() {
                Some(SelectedAccept::Ready { request, child }) => {
                    self.queues.selected = Some(SelectedAccept::Processing {
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
                    self.queues.selected = Some(SelectedAccept::Routed {
                        request: Arc::clone(&request),
                        route,
                        cancel_started: true,
                    });
                    if !cancel_started {
                        return ListenerAction::CancelAfterAccept { request, route };
                    }
                }
                Some(selected @ SelectedAccept::Processing { .. }) => {
                    self.queues.selected = Some(selected);
                }
                None => {
                    if !self.finalization_started {
                        self.finalization_started = true;
                        return ListenerAction::FinalizeClose;
                    }
                }
            }
            return ListenerAction::None;
        }

        select_pair(&mut self.queues);
        match self.queues.selected.take() {
            Some(SelectedAccept::Ready { request, child }) => {
                self.queues.selected = Some(SelectedAccept::Processing {
                    request: Arc::clone(&request),
                });
                ListenerAction::ProcessSelected { request, child }
            }
            Some(SelectedAccept::Routed {
                request,
                route,
                cancel_started,
            }) if request.is_cancelled() && !cancel_started => {
                self.queues.selected = Some(SelectedAccept::Routed {
                    request: Arc::clone(&request),
                    route,
                    cancel_started: true,
                });
                ListenerAction::CancelAfterAccept { request, route }
            }
            Some(selected) => {
                self.queues.selected = Some(selected);
                ListenerAction::None
            }
            None => ListenerAction::None,
        }
    }

    pub(in crate::v2::engine) fn route_selected(
        &mut self,
        request: &Arc<AcceptRequest>,
        route: u64,
    ) -> Result<()> {
        match self.queues.selected.take() {
            Some(SelectedAccept::Processing { request: current })
                if Arc::ptr_eq(&current, request) =>
            {
                self.queues.selected = Some(SelectedAccept::Routed {
                    request: Arc::clone(request),
                    route,
                    cancel_started: false,
                });
                Ok(())
            }
            Some(selected) => {
                self.queues.selected = Some(selected);
                Err(Error::InvalidConfig(
                    "listener selected pair changed during setup".into(),
                ))
            }
            None => Err(Error::InvalidConfig(
                "listener lost its selected pair during setup".into(),
            )),
        }
    }

    pub(in crate::v2::engine) fn finish_selected_request(
        &mut self,
        request: &Arc<AcceptRequest>,
    ) -> bool {
        let matches = match self.queues.selected.as_ref() {
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
            let selected = self
                .queues
                .selected
                .take()
                .expect("matching selected request exists");
            selected.request().release_permit();
            self.release_child_slot();
            select_pair(&mut self.queues);
        }
        matches
    }

    pub(in crate::v2::engine) fn finish_selected_route(&mut self, route: u64) -> bool {
        let matches = matches!(
            self.queues.selected.as_ref(),
            Some(SelectedAccept::Routed {
                route: current, ..
            }) if *current == route
        );
        if matches {
            let selected = self
                .queues
                .selected
                .take()
                .expect("matching selected route exists");
            selected.request().release_permit();
            self.release_child_slot();
            select_pair(&mut self.queues);
        }
        matches
    }

    pub(in crate::v2::engine) fn mark_selected_route_closing(&mut self, route: u64) -> bool {
        let Some(SelectedAccept::Routed {
            route: current,
            cancel_started,
            ..
        }) = self.queues.selected.as_mut()
        else {
            return false;
        };
        if *current != route {
            return false;
        }
        let first = !*cancel_started;
        *cancel_started = true;
        first
    }

    pub(in crate::v2::engine) fn release_unpaired_child_slot(&mut self) {
        self.release_child_slot();
    }

    fn release_child_slot(&mut self) {
        debug_assert!(self.child_slots_used > 0);
        self.child_slots_used = self.child_slots_used.saturating_sub(1);
    }

    pub(in crate::v2::engine) fn request_close(&mut self) -> bool {
        if self.closing {
            return false;
        }
        self.closing = true;
        self.admission.close();
        true
    }

    pub(in crate::v2::engine) fn close_accept_admission(&self, error: Error) {
        self.admission.close_with_error(error);
    }

    pub(in crate::v2::engine) fn fail(&mut self, error: Error) -> bool {
        if self.failure.is_none() {
            self.failure = Some(error.clone());
        }
        self.admission.close_with_error(error);
        self.request_close()
    }

    pub(in crate::v2::engine) fn close_error(&self) -> Error {
        self.failure.clone().unwrap_or(Error::TransportClosed)
    }

    pub(in crate::v2::engine) fn is_closing(&self) -> bool {
        self.closing || !self.admission.is_open()
    }

    pub(in crate::v2::engine) fn begin_work(&mut self) {
        self.work_enqueued = false;
    }

    pub(in crate::v2::engine) fn try_enqueue_work(&mut self) -> bool {
        if self.work_enqueued {
            false
        } else {
            self.work_enqueued = true;
            true
        }
    }

    pub(in crate::v2::engine) fn has_work(&self) -> bool {
        if self.is_closing() {
            return !self.queues.waiters.is_empty()
                || !self.queues.children.is_empty()
                || matches!(self.queues.selected, Some(SelectedAccept::Ready { .. }))
                || matches!(
                    self.queues.selected,
                    Some(SelectedAccept::Routed {
                        cancel_started: false,
                        ..
                    })
                )
                || (self.queues.selected.is_none() && !self.finalization_started);
        }
        self.queues
            .waiters
            .front()
            .is_some_and(|request| request.is_cancelled())
            || matches!(self.queues.selected, Some(SelectedAccept::Ready { .. }))
            || matches!(
                self.queues.selected,
                Some(SelectedAccept::Routed {
                    ref request,
                    cancel_started: false,
                    ..
                }) if request.is_cancelled() || request.is_delivered()
            )
            || (self.queues.selected.is_none()
                && !self.queues.waiters.is_empty()
                && !self.queues.children.is_empty())
    }

    pub(in crate::v2::engine) fn take_cm_id(&mut self) -> Option<SharedCmId> {
        self.cm_id.take()
    }

    pub(in crate::v2::engine) fn mark_cm_destruction_pending(&mut self) {
        self.cm_destruction_pending = true;
    }

    pub(in crate::v2::engine) fn cm_destruction_pending(&self) -> bool {
        self.cm_destruction_pending
    }

    pub(in crate::v2::engine) fn finish_close_into(
        &self,
        error: Option<Error>,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
    ) {
        let failure = error.or_else(|| self.failure.clone());
        let _ = self.close.store_if_empty(match failure {
            Some(error) => MemoizedTerminalResult::from_error(error),
            None => MemoizedTerminalResult::success(),
        });
        self.close.notify_waiters_into(actions);
    }

    pub(in crate::v2::engine) fn terminalize_waiters_into(
        &mut self,
        outcome: &MemoizedTerminalResult,
        actions: &mut crate::v2::engine::reactor::ReactorActions,
        budget: usize,
    ) -> usize {
        let error = outcome
            .clone()
            .into_result()
            .expect_err("terminal listener outcome must be an error");
        self.admission.close_with_error(error.clone());
        self.request_close();
        if self.close.store_if_empty(outcome.clone()) {
            self.close.notify_waiters_into(actions);
        }
        let mut processed = 0;
        while processed < budget && actions.can_accept(1) {
            let Some(request) = self.queues.waiters.pop_front() else {
                break;
            };
            request.release_permit();
            request.complete_into(Err(error.clone()), actions);
            processed += 1;
        }
        if processed < budget
            && actions.can_accept(1)
            && let Some(selected) = self.queues.selected.as_ref()
        {
            let request = Arc::clone(selected.request());
            if request.release_permit() {
                if matches!(selected, SelectedAccept::Routed { .. }) {
                    let _ = request.fail_undelivered_into(error, actions);
                } else {
                    request.complete_into(Err(error), actions);
                }
                processed += 1;
            }
        }
        processed
    }

    pub(in crate::v2::engine) fn close_state(&self) -> Arc<SessionListenerCloseState> {
        Arc::clone(&self.close)
    }

    pub(in crate::v2::engine) fn has_waiters(&self) -> bool {
        !self.queues.waiters.is_empty()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn child_slots_used(&self) -> usize {
        self.child_slots_used
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn selected_is_some(&self) -> bool {
        self.queues.selected.is_some()
    }
}

pub(in crate::v2::engine) struct ListenerRegistry {
    entries: PagedRegistry<ListenerToken, ListenerEntry>,
    raw_ids: HashMap<usize, ListenerToken>,
}

impl ListenerRegistry {
    pub(in crate::v2::engine) fn new(capacity: usize) -> Result<Self> {
        Ok(Self {
            entries: PagedRegistry::new(capacity)?,
            raw_ids: HashMap::new(),
        })
    }

    pub(in crate::v2::engine) fn reserve(
        &mut self,
        address: SocketAddr,
        config: RdmaListenerConfig,
    ) -> Result<ListenerToken> {
        self.entries
            .allocate_owned(|token| ListenerEntry::creating(token, address, config))
    }

    pub(in crate::v2::engine) fn activate(
        &mut self,
        token: ListenerToken,
        local_addr: SocketAddr,
        cm_id: SharedCmId,
        raw_id: usize,
        context_key: usize,
    ) -> std::result::Result<(), SharedCmId> {
        if self.raw_ids.contains_key(&raw_id) {
            return Err(cm_id);
        }
        let Some(entry) = self.entries.get_mut(token) else {
            return Err(cm_id);
        };
        if entry.cm_id.is_some() || entry.raw_id != 0 || entry.context_key != 0 {
            return Err(cm_id);
        }
        entry.activate(local_addr, cm_id, raw_id, context_key);
        self.raw_ids.insert(raw_id, token);
        Ok(())
    }

    pub(in crate::v2::engine) fn lookup(&self, token: ListenerToken) -> Lookup<&ListenerEntry> {
        self.entries.lookup_ref(token)
    }

    pub(in crate::v2::engine) fn get(&self, token: ListenerToken) -> Option<&ListenerEntry> {
        match self.lookup(token) {
            Lookup::Occupied(entry) => Some(entry),
            Lookup::Duplicate | Lookup::Stale | Lookup::Unknown | Lookup::Retired => None,
        }
    }

    pub(in crate::v2::engine) fn get_mut(
        &mut self,
        token: ListenerToken,
    ) -> Option<&mut ListenerEntry> {
        self.entries.get_mut(token)
    }

    pub(in crate::v2::engine) fn token_for_raw(&self, raw_id: usize) -> Option<ListenerToken> {
        self.raw_ids.get(&raw_id).copied()
    }

    pub(in crate::v2::engine) fn remove_identity(
        &mut self,
        token: ListenerToken,
        raw_id: usize,
    ) -> bool {
        if self.raw_ids.get(&raw_id) != Some(&token) {
            return false;
        }
        self.raw_ids.remove(&raw_id);
        true
    }

    pub(in crate::v2::engine) fn release(
        &mut self,
        token: ListenerToken,
        completed: bool,
    ) -> Option<ListenerEntry> {
        let raw_id = self.entries.get_mut(token).map_or(0, |entry| entry.raw_id);
        if raw_id != 0 {
            self.remove_identity(token, raw_id);
        }
        let entry = self.entries.release(token, completed)?;
        entry.admission.close();
        Some(entry)
    }

    pub(in crate::v2::engine) fn live(&self) -> usize {
        self.entries.live()
    }

    pub(in crate::v2::engine) fn has_capacity(&self) -> bool {
        self.entries.has_capacity()
    }

    pub(in crate::v2::engine) fn occupied(&self) -> Vec<ListenerToken> {
        self.entries.occupied_tokens()
    }

    pub(in crate::v2::engine) fn provider_owner_count(&self) -> usize {
        self.occupied()
            .into_iter()
            .filter(|token| self.get(*token).is_some_and(|entry| entry.cm_id.is_some()))
            .count()
    }

    pub(in crate::v2::engine) fn scan_occupied(
        &self,
        start: usize,
        budget: usize,
    ) -> (Vec<ListenerToken>, usize, bool, usize) {
        self.entries.scan_occupied_tokens(start, budget)
    }

    #[cfg(test)]
    fn force_generation_for_test(
        &mut self,
        token: ListenerToken,
        generation: u32,
    ) -> ListenerToken {
        self.entries.force_generation_for_test(token, generation)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn activate_identity_for_test(
        &mut self,
        token: ListenerToken,
        local_addr: SocketAddr,
        raw_id: usize,
        context_key: usize,
    ) -> bool {
        if self.raw_ids.contains_key(&raw_id) {
            return false;
        }
        let Some(entry) = self.entries.get_mut(token) else {
            return false;
        };
        if entry.cm_id.is_some() || entry.raw_id != 0 || entry.context_key != 0 {
            return false;
        }
        entry.local_addr = Some(local_addr);
        entry.raw_id = raw_id;
        entry.context_key = context_key;
        self.raw_ids.insert(raw_id, token);
        true
    }

    #[cfg(test)]
    fn retired(&self) -> usize {
        self.entries.retired()
    }

    #[cfg(test)]
    fn free(&self) -> usize {
        self.entries.free()
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

impl SelectedAccept {
    fn request(&self) -> &Arc<AcceptRequest> {
        match self {
            Self::Ready { request, .. }
            | Self::Processing { request }
            | Self::Routed { request, .. } => request,
        }
    }
}

pub(in crate::v2::engine) struct ChildAdmission {
    pub(in crate::v2::engine) rejected: Option<(IncomingChild, InboundRejectReason)>,
}

pub(in crate::v2::engine) enum ListenerAction {
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
    AcknowledgeDelivery {
        request: Arc<AcceptRequest>,
        route: u64,
    },
    FinalizeClose,
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::v2::engine) enum InboundRejectReason {
    BacklogFull,
    ConnectionCapacity,
    AdmissionClosed,
    ListenerClosed,
    ContextMismatch,
    SetupFailure,
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;
    use std::task::{Wake, Waker};

    use super::*;

    struct CountWake(AtomicUsize);

    impl Wake for CountWake {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    #[test]
    fn terminal_listener_overflow_stays_on_authoritative_waiter_queue() {
        let mut listener = ListenerEntry::test_only(64);
        let wakes = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wakes));
        let requests = (0..40)
            .map(|_| {
                let request = request(&listener);
                request.observer.waker.register(&waker);
                listener.register_waiter(Arc::clone(&request)).unwrap();
                request
            })
            .collect::<Vec<_>>();
        let outcome = MemoizedTerminalResult::from_error(Error::DriverShutdown);

        let mut first = crate::v2::engine::reactor::ReactorActions::default();
        assert_eq!(
            listener.terminalize_waiters_into(&outcome, &mut first, 32),
            crate::v2::engine::reactor::REACTOR_ACTION_BUDGET - 1
        );
        assert_eq!(
            first.len(),
            crate::v2::engine::reactor::REACTOR_ACTION_BUDGET
        );
        assert_eq!(wakes.0.load(Ordering::Acquire), 0);
        assert_eq!(
            requests
                .iter()
                .filter(|request| matches!(
                    &*lock_unpoison(&request.observer.result),
                    TakeOnceResult::Ready(Err(Error::DriverShutdown))
                ))
                .count(),
            crate::v2::engine::reactor::REACTOR_ACTION_BUDGET - 1
        );
        first.publish();
        assert_eq!(
            wakes.0.load(Ordering::Acquire),
            crate::v2::engine::reactor::REACTOR_ACTION_BUDGET - 1
        );

        let mut second = crate::v2::engine::reactor::ReactorActions::default();
        assert_eq!(
            listener.terminalize_waiters_into(&outcome, &mut second, 32),
            9
        );
        assert_eq!(second.len(), 9);
        second.publish();
        assert_eq!(wakes.0.load(Ordering::Acquire), 40);
    }

    #[test]
    fn accept_failure_mutates_authoritative_queue_before_detached_wake() {
        let mut listener = ListenerEntry::test_only(4);
        let request = request(&listener);
        let wakes = Arc::new(CountWake(AtomicUsize::new(0)));
        request
            .observer
            .waker
            .register(&Waker::from(Arc::clone(&wakes)));
        listener.register_waiter(Arc::clone(&request)).unwrap();
        request.cancel();
        let cancellation_wakes = wakes.0.load(Ordering::Acquire);
        request
            .observer
            .waker
            .register(&Waker::from(Arc::clone(&wakes)));

        let ListenerAction::CancelledBeforeSelection(selected) = listener.next_action() else {
            panic!("authoritative listener queue did not select the cancelled request")
        };
        assert!(Arc::ptr_eq(&selected, &request));
        assert!(listener.queues.waiters.is_empty());

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        selected.complete_into(Err(Error::DriverShutdown), &mut actions);
        assert_eq!(wakes.0.load(Ordering::Acquire), cancellation_wakes);
        assert!(matches!(
            &*lock_unpoison(&request.observer.result),
            TakeOnceResult::Ready(Err(Error::DriverShutdown))
        ));
        actions.publish();
        assert_eq!(wakes.0.load(Ordering::Acquire), cancellation_wakes + 1);
    }

    #[test]
    fn accept_success_and_listener_close_publish_only_after_state_commit() {
        let (engine, mut driver) =
            super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
        let connection = engine
            .shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, 1)
            .unwrap()
            .pop()
            .unwrap();
        let request = AcceptRequest::test_only();
        let accept_wakes = Arc::new(CountWake(AtomicUsize::new(0)));
        request
            .observer
            .waker
            .register(&Waker::from(Arc::clone(&accept_wakes)));
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        request.complete_success_into(connection, &mut actions);
        assert!(matches!(
            &*lock_unpoison(&request.observer.result),
            TakeOnceResult::Ready(Ok(_))
        ));
        assert_eq!(accept_wakes.0.load(Ordering::Acquire), 0);

        let listener = ListenerEntry::test_only(1);
        let close = listener.close_state();
        let before_close = actions.len();
        listener.finish_close_into(None, &mut actions);
        assert!(close.outcome().unwrap().is_success());
        assert_eq!(actions.len(), before_close + 1);

        actions.publish();
        assert_eq!(accept_wakes.0.load(Ordering::Acquire), 1);
        drop(request.take_result_for_test());
        drop(driver);
    }

    #[test]
    fn cancellation_winning_success_publication_retains_the_selected_connection() {
        let (engine, mut driver) =
            super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
        let connection = engine
            .shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, 1)
            .unwrap()
            .pop()
            .unwrap();
        let request = AcceptRequest::test_only();
        request.cancel();

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        request.complete_success_into(connection, &mut actions);
        assert!(request.has_undelivered_success_for_test());
        assert_eq!(actions.len(), 0);

        let failure = request.claim_undelivered_failure_into(Error::DriverShutdown, &mut actions);
        assert!(!failure.delivered());
        assert!(failure.into_connection().is_some());
        assert!(matches!(
            request.take_result_for_test(),
            Some(Err(Error::DriverShutdown))
        ));
        actions.publish();
        drop(driver);
    }

    #[test]
    fn cancellation_after_success_wake_retains_the_selected_connection() {
        let (engine, mut driver) =
            super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
        let connection = engine
            .shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, 1)
            .unwrap()
            .pop()
            .unwrap();
        let request = AcceptRequest::test_only();
        let wakes = Arc::new(CountWake(AtomicUsize::new(0)));
        request
            .observer
            .waker
            .register(&Waker::from(Arc::clone(&wakes)));

        let mut published = crate::v2::engine::reactor::ReactorActions::default();
        request.complete_success_into(connection, &mut published);
        published.publish();
        assert_eq!(wakes.0.load(Ordering::Acquire), 1);

        request.cancel();
        assert!(request.has_undelivered_success_for_test());
        let mut cancelled = crate::v2::engine::reactor::ReactorActions::default();
        let failure = request.claim_undelivered_failure_into(Error::DriverShutdown, &mut cancelled);
        assert!(!failure.delivered());
        assert!(failure.into_connection().is_some());
        assert!(matches!(
            request.take_result_for_test(),
            Some(Err(Error::DriverShutdown))
        ));
        cancelled.publish();
        drop(driver);
    }

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

    #[test]
    fn pending_listen_and_accept_futures_release_strong_session_owners() {
        let (engine, _driver) =
            super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
        let baseline_engine_owners = Arc::strong_count(&engine.shared);
        let mut listen_future = Box::pin(listen(
            Arc::clone(&engine.shared.session),
            Arc::clone(&engine.shared.commands),
            "127.0.0.1:0".parse().unwrap(),
            RdmaListenerConfig::default(),
        ));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(listen_future.as_mut().poll(&mut context).is_pending());
        assert_eq!(
            Arc::strong_count(&engine.shared),
            baseline_engine_owners,
            "suspended listen future must retain only weak session routing"
        );
        assert_eq!(engine.shared.commands.pending_listens(), 1);
        drop(listen_future);
        assert_eq!(engine.shared.commands.pending_listens(), 0);
        assert_eq!(
            engine.shared.commands.available_listen_permits(),
            engine.shared.config.max_live_connections
        );

        let listener = ListenerEntry::test_only(4);
        let listener_admission = listener.admission();
        let mut accept_future = Box::pin(accept_with_setup(
            Arc::clone(&engine.shared.session),
            Arc::clone(&engine.shared.commands),
            listener.token,
            Arc::clone(&listener_admission),
            RdmaConnectionConfig::default(),
            empty_connection_setup(),
        ));
        assert!(accept_future.as_mut().poll(&mut context).is_pending());
        assert_eq!(Arc::strong_count(&engine.shared), baseline_engine_owners);
        assert_eq!(engine.shared.commands.pending_accepts(), 1);
        assert_eq!(listener_admission.available_permits(), 3);
        drop(accept_future);
        assert_eq!(engine.shared.commands.pending_accepts(), 0);
        assert_eq!(listener_admission.available_permits(), 4);
    }

    #[test]
    fn command_ingress_services_listens_in_fifo_order_one_per_turn() {
        let (engine, mut driver) =
            super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
        let first_address = "127.0.0.1:0".parse().unwrap();
        let second_address = "127.0.0.2:0".parse().unwrap();
        let mut first = Box::pin(listen(
            Arc::clone(&engine.shared.session),
            Arc::clone(&engine.shared.commands),
            first_address,
            RdmaListenerConfig::default(),
        ));
        let mut second = Box::pin(listen(
            Arc::clone(&engine.shared.session),
            Arc::clone(&engine.shared.commands),
            second_address,
            RdmaListenerConfig::default(),
        ));
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        assert!(first.as_mut().poll(&mut context).is_pending());
        assert!(second.as_mut().poll(&mut context).is_pending());
        assert_eq!(engine.shared.commands.pending_listens(), 2);

        engine.shared.commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert_eq!(engine.shared.commands.pending_listens(), 1);
        assert_eq!(
            driver.reactor.session.cm.pending_listen_addresses(),
            vec![first_address]
        );
        engine.shared.commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert_eq!(
            driver.reactor.session.cm.pending_listen_addresses(),
            vec![first_address, second_address]
        );
    }

    fn request(listener: &ListenerEntry) -> Arc<AcceptRequest> {
        listener.test_accept_request()
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
        let mut listener = ListenerEntry::test_only(1);
        let request = request(&listener);
        listener.fail(Error::InvalidConfig("listener CM close failed".into()));

        let error = listener.register_waiter(request).unwrap_err();
        assert!(matches!(error, Error::InvalidConfig(_)));
        assert!(error.to_string().contains("listener CM close failed"));
        assert!(
            listener
                .admission()
                .close_error()
                .to_string()
                .contains("listener CM close failed")
        );
    }

    #[test]
    fn terminalization_publishes_accept_admission_error_before_close() {
        let mut listener = ListenerEntry::test_only(1);
        let outcome = MemoizedTerminalResult::from_error(Error::DriverShutdown);
        let mut actions = crate::v2::engine::reactor::ReactorActions::default();

        assert_eq!(
            listener.terminalize_waiters_into(&outcome, &mut actions, 0),
            0
        );
        assert!(matches!(
            listener.admission().close_error(),
            Error::DriverShutdown
        ));
    }

    #[test]
    fn waiter_registration_and_child_arrival_order_are_exact() {
        let mut listener = ListenerEntry::test_only(2);
        let first = request(&listener);
        let second = request(&listener);
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
        let mut listener = ListenerEntry::test_only(1);
        let request = request(&listener);
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
        assert!(listener.queues.selected.is_some());
        request.observer.delivered.store(true, Ordering::Release);
        assert!(matches!(
            listener.next_action(),
            ListenerAction::AcknowledgeDelivery { route: 42, .. }
        ));
        assert!(listener.finish_selected_route(42));
        assert!(listener.queues.selected.is_none());
        assert!(!request.owns_permit());
    }

    #[test]
    fn delivered_accept_wins_over_late_waiter_cancellation() {
        let mut listener = ListenerEntry::test_only(1);
        let request = request(&listener);
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
        request.set_route_token(43);
        listener.route_selected(&request, 43).unwrap();
        request.observer.delivered.store(true, Ordering::Release);
        request.cancel();

        assert!(matches!(
            listener.next_action(),
            ListenerAction::AcknowledgeDelivery { route: 43, .. }
        ));
        assert!(listener.selected_is_some());
        assert_eq!(listener.child_slots_used(), 1);
        assert_eq!(listener.admission.available_permits(), 0);
        assert!(request.owns_permit());

        assert!(listener.finish_selected_route(43));
        assert!(!listener.selected_is_some());
        assert_eq!(listener.child_slots_used(), 0);
        assert_eq!(listener.admission.available_permits(), 1);
        assert!(!request.owns_permit());
    }

    #[test]
    fn delivered_accept_wins_over_late_listener_close() {
        let mut listener = ListenerEntry::test_only(1);
        let request = request(&listener);
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
        request.set_route_token(44);
        listener.route_selected(&request, 44).unwrap();
        request.observer.delivered.store(true, Ordering::Release);
        listener.admission.close();

        assert!(matches!(
            listener.next_action(),
            ListenerAction::AcknowledgeDelivery { route: 44, .. }
        ));
        assert!(listener.selected_is_some());
        assert_eq!(listener.child_slots_used(), 1);
        assert!(request.owns_permit());
        assert!(listener.finish_selected_route(44));
        assert!(!listener.selected_is_some());
        assert_eq!(listener.child_slots_used(), 0);
        assert!(!request.owns_permit());
    }

    #[test]
    fn armed_route_close_suppresses_delivered_listener_requeue() {
        let mut listener = ListenerEntry::test_only(1);
        let request = request(&listener);
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
        request.set_route_token(45);
        listener.route_selected(&request, 45).unwrap();
        request.observer.delivered.store(true, Ordering::Release);
        assert!(listener.has_work());

        assert!(listener.mark_selected_route_closing(45));
        assert!(!listener.has_work());
        assert!(listener.selected_is_some());
        assert_eq!(listener.child_slots_used(), 1);
        assert!(request.owns_permit());
        assert!(listener.finish_selected_route(45));
        assert!(!listener.selected_is_some());
        assert_eq!(listener.child_slots_used(), 0);
        assert!(!request.owns_permit());
    }

    #[test]
    fn cancellation_before_selection_removes_only_that_waiter() {
        let mut listener = ListenerEntry::test_only(2);
        let cancelled = request(&listener);
        let survivor = request(&listener);
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
        let mut first = ListenerEntry::test_only(2);
        let mut second = ListenerEntry::test_only(2);
        for listener in [&mut first, &mut second] {
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
        let first_waiter = request(&first);
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
        let mut listener = ListenerEntry::test_only(2);
        let first = request(&listener);
        let second = request(&listener);
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
        let mut listener = ListenerEntry::test_only(2);
        let selected = request(&listener);
        let pending = request(&listener);
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
        listener.request_close();

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
        listener.release_unpaired_child_slot();
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
        assert_eq!(listener.child_slots_used(), 0);
        assert_eq!(listener.admission.available_permits(), 2);
    }

    #[test]
    fn listener_registry_capacity_generation_and_retirement_are_exact() {
        let address = "127.0.0.1:1".parse().unwrap();
        let config = RdmaListenerConfig::default().backlog(1);
        let mut registry = ListenerRegistry::new(2).unwrap();
        let first = registry.reserve(address, config.clone()).unwrap();
        assert_eq!(ListenerToken::decode(first.encode()), first);
        let second = registry.reserve(address, config.clone()).unwrap();
        assert_eq!(registry.live(), 2);
        assert!(!registry.has_capacity());
        assert!(matches!(
            registry.reserve(address, config.clone()),
            Err(Error::CapacityExhausted)
        ));

        registry.release(first, false).unwrap();
        let reused = registry.reserve(address, config.clone()).unwrap();
        assert_eq!(reused.slot, first.slot);
        assert_eq!(reused.generation, first.generation + 1);
        assert!(matches!(registry.lookup(first), Lookup::Stale));

        let exhausted = registry.force_generation_for_test(reused, u32::MAX);
        registry.release(exhausted, true).unwrap();
        assert_eq!(registry.retired(), 1);
        assert!(matches!(registry.lookup(exhausted), Lookup::Duplicate));
        registry.release(second, true).unwrap();
        assert_eq!(registry.free(), 1);
        let remaining = registry.reserve(address, config).unwrap();
        assert_ne!(remaining.slot, exhausted.slot);
    }

    #[test]
    fn listener_registry_rejects_stale_token_and_raw_identity() {
        let address = "127.0.0.1:1".parse().unwrap();
        let config = RdmaListenerConfig::default().backlog(1);
        let mut registry = ListenerRegistry::new(1).unwrap();
        let first = registry.reserve(address, config.clone()).unwrap();
        assert!(registry.activate_identity_for_test(first, address, 41, 51));
        assert_eq!(registry.token_for_raw(41), Some(first));
        registry.release(first, false).unwrap();
        assert_eq!(registry.token_for_raw(41), None);

        let second = registry.reserve(address, config).unwrap();
        assert_ne!(first, second);
        assert!(matches!(registry.lookup(first), Lookup::Stale));
        assert!(matches!(registry.lookup(second), Lookup::Occupied(_)));
    }

    #[test]
    fn terminal_stale_listener_drop_cannot_consume_reused_slot_control_capacity() {
        let (engine, mut driver) = super::super::super::test_engine_pair_with_capacity(
            super::super::super::CompletionMode::Polling,
            1,
        );
        let (stale, stale_token) = driver
            .reactor
            .session
            .cm
            .test_listener(&driver.reactor.session.manager, 1);
        let stale_close = stale.session.close.clone();
        stale_close.store_if_empty(MemoizedTerminalResult::success());
        driver.reactor.session.cm.release_test_listener(stale_token);

        let (live, live_token) = driver
            .reactor
            .session
            .cm
            .test_listener(&driver.reactor.session.manager, 1);
        assert_ne!(stale_token, live_token);
        drop(stale);
        assert_eq!(engine.shared.commands.pending_listener_closes(), 0);

        drop(live);
        assert_eq!(engine.shared.commands.pending_listener_closes(), 1);
    }

    #[tokio::test]
    async fn accept_permits_are_fifo_and_do_not_barge() {
        let listener = ListenerEntry::test_only(1);
        let admission = listener.admission();
        let held = admission.acquire().await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let first = {
            let admission = Arc::clone(&admission);
            let tx = tx.clone();
            tokio::spawn(async move {
                let permit = admission.acquire().await.unwrap();
                tx.send(1).unwrap();
                permit
            })
        };
        tokio::task::yield_now().await;
        let second = {
            let admission = Arc::clone(&admission);
            tokio::spawn(async move {
                let permit = admission.acquire().await.unwrap();
                tx.send(2).unwrap();
                permit
            })
        };
        drop(held);
        assert_eq!(rx.recv().await, Some(1));
        drop(first.await.unwrap());
        assert_eq!(rx.recv().await, Some(2));
        drop(second.await.unwrap());
        assert_eq!(admission.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancelled_accept_head_is_removed_and_release_wakes_next() {
        let listener = ListenerEntry::test_only(1);
        let admission = listener.admission();
        let held = admission.acquire().await.unwrap();
        let first = {
            let admission = Arc::clone(&admission);
            tokio::spawn(async move { admission.acquire().await })
        };
        tokio::task::yield_now().await;
        let second = {
            let admission = Arc::clone(&admission);
            tokio::spawn(async move { admission.acquire().await })
        };
        tokio::task::yield_now().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        drop(held);
        let permit = second.await.unwrap().expect("next waiter is woken");
        assert_eq!(admission.available_permits(), 0);
        drop(permit);
        assert_eq!(admission.available_permits(), 1);
    }

    #[test]
    fn accept_wake_rechecks_listener_close_before_command_registration() {
        let (engine, _driver) =
            super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
        let listener = ListenerEntry::test_only(1);
        let admission = listener.admission();
        let held = Arc::clone(&admission.permits).try_acquire_owned().unwrap();
        let mut accept = Box::pin(accept_with_setup(
            Arc::clone(&engine.shared.session),
            Arc::clone(&engine.shared.commands),
            listener.token,
            Arc::clone(&admission),
            RdmaConnectionConfig::default(),
            empty_connection_setup(),
        ));
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(accept.as_mut().poll(&mut cx).is_pending());

        let barrier =
            super::super::super::registry::write_unpoison(&engine.shared.session.admission);
        admission.close_with_error(Error::InvalidConfig(
            "listener closed during accept wake".into(),
        ));
        drop(barrier);
        drop(held);

        assert!(matches!(
            accept.as_mut().poll(&mut cx),
            Poll::Ready(Err(Error::InvalidConfig(message)))
                if message.contains("listener closed during accept wake")
        ));
        assert_eq!(engine.shared.commands.pending_accepts(), 0);
    }

    #[test]
    fn admitted_accept_forces_the_first_poll_to_yield() {
        let (engine, _driver) =
            super::super::super::test_engine_pair(super::super::super::CompletionMode::Polling);
        let listener = ListenerEntry::test_only(1);
        let admission = listener.admission();
        let mut accept = Box::pin(accept_with_setup(
            Arc::clone(&engine.shared.session),
            Arc::clone(&engine.shared.commands),
            listener.token,
            admission,
            RdmaConnectionConfig::default(),
            empty_connection_setup(),
        ));
        let wakes = Arc::new(CountWake(AtomicUsize::new(0)));
        let waker = Waker::from(Arc::clone(&wakes));
        let mut cx = Context::from_waker(&waker);

        assert!(accept.as_mut().poll(&mut cx).is_pending());
        assert_eq!(engine.shared.commands.pending_accepts(), 1);
        assert_eq!(
            wakes.0.load(Ordering::Acquire),
            1,
            "admission must self-wake only after forcing the admitting poll to return"
        );
        drop(accept);
        assert_eq!(engine.shared.commands.pending_accepts(), 0);
    }

    #[test]
    fn accept_request_and_child_slot_transfer_together_through_selection() {
        let mut listener = ListenerEntry::test_only(1);
        let request = request(&listener);
        listener.register_waiter(Arc::clone(&request)).unwrap();
        assert!(
            listener
                .admit_child(IncomingChild::test_only())
                .rejected
                .is_none()
        );
        assert_eq!(listener.admission.available_permits(), 0);
        assert_eq!(listener.child_slots_used(), 1);
        assert!(listener.selected_is_some());
        assert!(matches!(
            listener.next_action(),
            ListenerAction::ProcessSelected { .. }
        ));
        listener.route_selected(&request, 9).unwrap();
        request.cancel();
        assert!(matches!(
            listener.next_action(),
            ListenerAction::CancelAfterAccept { route: 9, .. }
        ));
        assert!(listener.finish_selected_route(9));
        assert_eq!(listener.admission.available_permits(), 1);
        assert_eq!(listener.child_slots_used(), 0);
        assert!(!listener.selected_is_some());
        assert!(!request.owns_permit());
    }

    #[test]
    fn terminal_failure_releases_accept_permits_and_retains_child_owners_once() {
        let mut listener = ListenerEntry::test_only(2);
        let selected = request(&listener);
        let pending = request(&listener);
        listener.register_waiter(Arc::clone(&selected)).unwrap();
        listener.register_waiter(Arc::clone(&pending)).unwrap();
        listener.admit_child(IncomingChild::test_only());
        listener.admit_child(IncomingChild::test_only());
        assert!(matches!(
            listener.next_action(),
            ListenerAction::ProcessSelected { .. }
        ));
        listener.route_selected(&selected, 11).unwrap();

        let mut actions = crate::v2::engine::reactor::ReactorActions::default();
        let outcome = MemoizedTerminalResult::from_error(Error::DriverShutdown);
        assert_eq!(
            listener.terminalize_waiters_into(&outcome, &mut actions, 8),
            2
        );
        assert!(!selected.owns_permit());
        assert!(!pending.owns_permit());
        assert_eq!(listener.admission.available_permits(), 2);
        assert_eq!(listener.child_slots_used(), 2);
        assert!(listener.selected_is_some());
        assert_eq!(listener.queues.children.len(), 1);
        actions.publish();
    }

    #[test]
    fn provider_backlog_seam_receives_the_exact_public_value() {
        let config = RdmaListenerConfig::default().backlog(37);
        let mut observed = None;
        with_validated_listener_backlog(&config, |backlog| observed = Some(backlog)).unwrap();
        assert_eq!(observed, Some(37));
        assert_eq!(config.backlog_capacity(), 37);

        let mut called = false;
        assert!(
            with_validated_listener_backlog(&RdmaListenerConfig::default().backlog(0), |_| {
                called = true;
            })
            .is_err()
        );
        assert!(!called);
    }
}
