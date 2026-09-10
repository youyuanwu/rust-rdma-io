//! Bounded typed command admission into the driver-owned reactor.

use std::collections::{HashSet, VecDeque};
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::super::driver::WorkSignal;
use super::super::io::ProtocolCommand;
use super::super::io_core::OperationCommand;
use super::super::registry::{
    ConnectionToken, ListenerToken, OperationToken, lock_unpoison, read_unpoison, write_unpoison,
};
use super::super::session::SessionFrontend;
use super::super::session::SessionReactorSources;
use super::super::session::cm::OutboundRequest;
use super::super::session::connection::ConnectionReservation;
use super::super::session::listener::{AcceptRequest, ListenRequest, ListenerAdmission};
use super::super::{EngineFrontendRoot, Error};

enum SessionCommand {
    Connect {
        request: Arc<OutboundRequest>,
        reservation: ConnectionReservation,
        _permit: OwnedSemaphorePermit,
    },
    Listen {
        request: Arc<ListenRequest>,
        _permit: OwnedSemaphorePermit,
    },
    #[cfg(any(test, feature = "test-hooks"))]
    TestInstall {
        request: Arc<super::super::driver::test_api::TestConnectionInstallRequest>,
        reservation: ConnectionReservation,
        _permit: OwnedSemaphorePermit,
    },
}

#[derive(Default)]
struct CommandQueues {
    connect: VecDeque<SessionCommand>,
    listen: VecDeque<SessionCommand>,
    accept: VecDeque<(ListenerToken, Arc<AcceptRequest>)>,
    operation: VecDeque<(Arc<OperationCommand>, OwnedSemaphorePermit)>,
    protocol: VecDeque<ProtocolQueueEntry>,
    next_class: usize,
}

enum ProtocolQueueEntry {
    Command(ProtocolCommand, OwnedSemaphorePermit),
    Publication(super::DeferredProtocolActions),
}

#[derive(Default)]
struct ControlQueue {
    connection_close: VecDeque<ConnectionToken>,
    connection_close_set: HashSet<ConnectionToken>,
    connect_cancel: VecDeque<Arc<OutboundRequest>>,
    #[cfg(any(test, feature = "test-hooks"))]
    connection_error: VecDeque<ConnectionToken>,
    #[cfg(any(test, feature = "test-hooks"))]
    connection_disconnect: VecDeque<ConnectionToken>,
    #[cfg(any(test, feature = "test-hooks"))]
    connection_fail_qp_destroy: VecDeque<ConnectionToken>,
    operation_cancel: VecDeque<OperationToken>,
    operation_cancel_set: HashSet<OperationToken>,
    listener_close: VecDeque<ListenerToken>,
    listener_close_set: HashSet<ListenerToken>,
    listener_work: VecDeque<ListenerToken>,
    listener_work_set: HashSet<ListenerToken>,
}

pub(in crate::v2::engine) struct CommandTurn {
    pub(in crate::v2::engine) session_work: bool,
    pub(in crate::v2::engine) has_more: bool,
    pub(in crate::v2::engine) shutdown_requested: bool,
}

#[derive(Debug)]
pub(in crate::v2::engine) struct ConnectAdmission {
    lane: OwnedSemaphorePermit,
    reservation: ConnectionReservation,
}

/// Frontend-owned protocol payload waiting to acquire operation-lane permits.
///
/// All message connections share this counter, so commands that have not yet
/// acquired semaphore permits cannot retain more than one operation lane's
/// configured capacity in aggregate.
pub(in crate::v2::engine) struct ProtocolPayloadReservation {
    pending: Arc<AtomicUsize>,
    operations: usize,
}

impl Drop for ProtocolPayloadReservation {
    fn drop(&mut self) {
        self.pending.fetch_sub(self.operations, Ordering::AcqRel);
    }
}

/// Cloneable, resource-free frontend admission endpoint.
///
/// Connect and listen permits bound only commands waiting for the driver.
/// Dequeued commands transfer into reactor-owned generational connection and
/// listener lifecycles.
pub(in crate::v2::engine) struct CommandIngress {
    connect_permits: Arc<Semaphore>,
    connection_permits: Arc<Semaphore>,
    listen_permits: Arc<Semaphore>,
    operation_permits: Arc<Semaphore>,
    operation_capacity: usize,
    pending_protocol_operations: Arc<AtomicUsize>,
    queues: Mutex<CommandQueues>,
    controls: Mutex<ControlQueue>,
    shutdown: AtomicBool,
    shutdown_command_pending: AtomicBool,
    closed: AtomicBool,
    closed_error: Mutex<Option<Error>>,
    max_connection_controls: usize,
    max_listener_controls: usize,
    signal: Arc<WorkSignal>,
    diagnostics: Arc<Mutex<super::super::diagnostics::PublishedDiagnostics>>,
}

impl CommandIngress {
    #[cfg(test)]
    pub(in crate::v2::engine) fn new(
        connection_capacity: usize,
        operation_capacity: usize,
        signal: Arc<WorkSignal>,
    ) -> Arc<Self> {
        Self::new_with_diagnostics(
            connection_capacity,
            operation_capacity,
            signal,
            Arc::new(Mutex::new(
                super::super::diagnostics::PublishedDiagnostics::initial(operation_capacity),
            )),
        )
    }

    pub(in crate::v2::engine) fn new_with_diagnostics(
        connection_capacity: usize,
        operation_capacity: usize,
        signal: Arc<WorkSignal>,
        diagnostics: Arc<Mutex<super::super::diagnostics::PublishedDiagnostics>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            connect_permits: Arc::new(Semaphore::new(connection_capacity)),
            connection_permits: Arc::new(Semaphore::new(connection_capacity)),
            listen_permits: Arc::new(Semaphore::new(connection_capacity)),
            operation_permits: Arc::new(Semaphore::new(operation_capacity)),
            operation_capacity,
            pending_protocol_operations: Arc::new(AtomicUsize::new(0)),
            queues: Mutex::new(CommandQueues::default()),
            controls: Mutex::new(ControlQueue::default()),
            shutdown: AtomicBool::new(false),
            shutdown_command_pending: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            closed_error: Mutex::new(None),
            max_connection_controls: connection_capacity,
            max_listener_controls: connection_capacity,
            signal,
            diagnostics,
        })
    }

    /// Force the admitting poll to return `Pending` before observing completion.
    ///
    /// A driver on another executor thread may consume and complete a command
    /// immediately after enqueue. This one-poll boundary keeps frontend semantics
    /// deterministic: first poll admits only; a later poll observes the result.
    pub(in crate::v2::engine) async fn yield_after_admission() {
        let mut admitted = false;
        poll_fn(|cx| {
            if admitted {
                Poll::Ready(())
            } else {
                admitted = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await;
    }

    pub(in crate::v2::engine) async fn acquire_connect(
        self: &Arc<Self>,
    ) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.connect_permits).acquire_owned().await.ok()
    }

    pub(in crate::v2::engine) fn reserve_connect(
        self: &Arc<Self>,
        lane: OwnedSemaphorePermit,
    ) -> Result<ConnectAdmission, Error> {
        let permit_pool = Arc::clone(&self.connection_permits);
        let reservation = Arc::clone(&permit_pool)
            .try_acquire_owned()
            .map(|permit| {
                ConnectionReservation::new_with_frontend_diagnostics(
                    permit,
                    Arc::clone(&permit_pool),
                    self.max_connection_controls,
                    Arc::clone(&self.diagnostics),
                )
            })
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::NoPermits => Error::CapacityExhausted,
                tokio::sync::TryAcquireError::Closed => Error::DriverShutdown,
            })?;
        Ok(ConnectAdmission { lane, reservation })
    }

    pub(in crate::v2::engine) async fn acquire_listen(
        self: &Arc<Self>,
    ) -> Option<OwnedSemaphorePermit> {
        Arc::clone(&self.listen_permits).acquire_owned().await.ok()
    }

    pub(in crate::v2::engine) fn operation_acquire(
        self: &Arc<Self>,
    ) -> impl std::future::Future<Output = Option<OwnedSemaphorePermit>> + Send + 'static {
        let permits = Arc::clone(&self.operation_permits);
        async move { permits.acquire_owned().await.ok() }
    }

    pub(in crate::v2::engine) fn enqueue_connect(
        &self,
        request: Arc<OutboundRequest>,
        admission: ConnectAdmission,
    ) {
        let ConnectAdmission { lane, reservation } = admission;
        lock_unpoison(&self.queues)
            .connect
            .push_back(SessionCommand::Connect {
                request,
                reservation,
                _permit: lane,
            });
    }

    pub(in crate::v2::engine) fn enqueue_listen(
        &self,
        request: Arc<ListenRequest>,
        permit: OwnedSemaphorePermit,
    ) {
        lock_unpoison(&self.queues)
            .listen
            .push_back(SessionCommand::Listen {
                request,
                _permit: permit,
            });
    }

    pub(in crate::v2::engine) fn enqueue_accept(
        &self,
        listener: ListenerToken,
        request: Arc<AcceptRequest>,
    ) {
        lock_unpoison(&self.queues)
            .accept
            .push_back((listener, request));
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn enqueue_test_connection_install(
        &self,
        request: Arc<super::super::driver::test_api::TestConnectionInstallRequest>,
        admission: ConnectAdmission,
    ) {
        let ConnectAdmission { lane, reservation } = admission;
        lock_unpoison(&self.queues)
            .connect
            .push_back(SessionCommand::TestInstall {
                request,
                reservation,
                _permit: lane,
            });
    }

    pub(in crate::v2::engine) fn notify_reactor(&self) {
        self.signal.notify_reactor();
    }

    pub(in crate::v2::engine) fn defer_setup_publication(
        &self,
        mut publication: super::DeferredProtocolActions,
        actions: &mut super::ReactorActions,
    ) {
        publication.append_bounded_to(actions);
        if publication.is_empty() {
            return;
        }
        lock_unpoison(&self.queues)
            .protocol
            .push_back(ProtocolQueueEntry::Publication(publication));
        self.notify_reactor();
    }

    pub(in crate::v2::engine) fn cancel_connect(&self, target: &Arc<OutboundRequest>) -> bool {
        let command = {
            let mut queues = lock_unpoison(&self.queues);
            let position = queues.connect.iter().position(|command| {
                matches!(
                    command,
                    SessionCommand::Connect { request, .. }
                        if Arc::ptr_eq(request, target)
                )
            });
            position.and_then(|position| queues.connect.remove(position))
        };
        command.is_some()
    }

    pub(in crate::v2::engine) fn cancel_listen(&self, target: &Arc<ListenRequest>) -> bool {
        let command = {
            let mut queues = lock_unpoison(&self.queues);
            let position = queues.listen.iter().position(|command| {
                matches!(
                    command,
                    SessionCommand::Listen { request, .. }
                        if Arc::ptr_eq(request, target)
                )
            });
            position.and_then(|position| queues.listen.remove(position))
        };
        command.is_some()
    }

    pub(in crate::v2::engine) fn cancel_accept(&self, target: &Arc<AcceptRequest>) -> bool {
        let request = {
            let mut queues = lock_unpoison(&self.queues);
            let position = queues
                .accept
                .iter()
                .position(|(_, request)| Arc::ptr_eq(request, target));
            position.and_then(|position| queues.accept.remove(position))
        };
        if let Some((_, request)) = request {
            request.release_permit();
            true
        } else {
            false
        }
    }

    pub(in crate::v2::engine) fn request_listener_work(&self, token: ListenerToken) {
        let inserted = {
            let mut controls = lock_unpoison(&self.controls);
            if controls.listener_work_set.insert(token) {
                controls.listener_work.push_back(token);
                true
            } else {
                false
            }
        };
        if inserted {
            self.notify_reactor();
        }
    }

    pub(in crate::v2::engine) fn request_listener_close(
        &self,
        frontend: &SessionFrontend,
        token: ListenerToken,
        admission: &ListenerAdmission,
    ) {
        if admission.is_terminal() {
            return;
        }
        let inserted = {
            let _admission = write_unpoison(&frontend.admission);
            if admission.is_terminal() {
                return;
            }
            admission.close();
            if self.closed.load(Ordering::Acquire) {
                return;
            }
            let mut controls = lock_unpoison(&self.controls);
            if !controls.listener_close_set.insert(token) {
                false
            } else if controls.listener_close.len() >= self.max_listener_controls {
                controls.listener_close_set.remove(&token);
                debug_assert!(
                    false,
                    "validated live listener controls cannot exceed configured capacity"
                );
                false
            } else {
                controls.listener_close.push_back(token);
                true
            }
        };
        if inserted {
            self.notify_reactor();
        }
    }

    pub(in crate::v2::engine) fn enqueue_operation(
        &self,
        manager: &SessionFrontend,
        command: Arc<OperationCommand>,
        permit: OwnedSemaphorePermit,
    ) -> Result<(), Error> {
        let _admission = read_unpoison(&manager.admission);
        if self.closed.load(Ordering::Acquire) {
            return Err(manager.admission_error().unwrap_or(Error::DriverShutdown));
        }
        lock_unpoison(&self.queues)
            .operation
            .push_back((command, permit));
        Ok(())
    }

    #[cfg_attr(test, allow(dead_code))]
    #[allow(
        clippy::result_large_err,
        reason = "failed bounded admission returns the command with all owned MRs intact"
    )]
    pub(in crate::v2::engine) fn enqueue_protocol(
        &self,
        manager: &SessionFrontend,
        command: ProtocolCommand,
        permit: OwnedSemaphorePermit,
    ) -> Result<(), (Error, ProtocolCommand)> {
        let _admission = read_unpoison(&manager.admission);
        if self.closed.load(Ordering::Acquire) {
            return Err((
                manager.admission_error().unwrap_or(Error::DriverShutdown),
                command,
            ));
        }
        lock_unpoison(&self.queues)
            .protocol
            .push_back(ProtocolQueueEntry::Command(command, permit));
        Ok(())
    }

    pub(in crate::v2::engine) fn validate_operation_batch(
        &self,
        count: usize,
    ) -> Result<u32, Error> {
        let count = u32::try_from(count).map_err(|_| {
            Error::InvalidConfig("protocol I/O batch length must be in 1..=u32::MAX".into())
        })?;
        if count == 0 || count as usize > self.operation_capacity {
            return Err(Error::InvalidConfig(format!(
                "protocol I/O batch length must be in 1..={}",
                self.operation_capacity
            )));
        }
        Ok(count)
    }

    pub(in crate::v2::engine) fn reserve_protocol_payload(
        &self,
        operations: usize,
    ) -> Result<ProtocolPayloadReservation, Error> {
        self.validate_operation_batch(operations)?;
        self.pending_protocol_operations
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                pending
                    .checked_add(operations)
                    .filter(|next| *next <= self.operation_capacity)
            })
            .map_err(|_| Error::CapacityExhausted)?;
        Ok(ProtocolPayloadReservation {
            pending: Arc::clone(&self.pending_protocol_operations),
            operations,
        })
    }

    pub(in crate::v2::engine) fn operation_batch_acquire(
        self: &Arc<Self>,
        count: usize,
    ) -> impl std::future::Future<Output = Option<OwnedSemaphorePermit>> + Send + 'static {
        let permits = Arc::clone(&self.operation_permits);
        let count = self
            .validate_operation_batch(count)
            .expect("protocol batch is validated before admission");
        async move { permits.acquire_many_owned(count).await.ok() }
    }

    pub(in crate::v2::engine) fn cancel_operation(&self, target: &Arc<OperationCommand>) -> bool {
        let command = {
            let mut queues = lock_unpoison(&self.queues);
            let position = queues
                .operation
                .iter()
                .position(|(command, _)| Arc::ptr_eq(command, target));
            position.and_then(|position| queues.operation.remove(position))
        };
        command.is_some()
    }

    pub(in crate::v2::engine) fn request_operation_cancel(&self, token: OperationToken) {
        let inserted = {
            let mut controls = lock_unpoison(&self.controls);
            if controls.operation_cancel_set.insert(token) {
                controls.operation_cancel.push_back(token);
                true
            } else {
                false
            }
        };
        if inserted {
            self.notify_reactor();
        }
    }

    pub(in crate::v2::engine) fn request_connection_close(
        &self,
        manager: &SessionFrontend,
        token: ConnectionToken,
    ) {
        let inserted = {
            let _admission = read_unpoison(&manager.admission);
            if self.closed.load(Ordering::Acquire) || manager.admission_error().is_some() {
                return;
            }

            let mut controls = lock_unpoison(&self.controls);
            if !controls.connection_close_set.insert(token) {
                false
            } else if controls.connection_close.len() >= self.max_connection_controls {
                controls.connection_close_set.remove(&token);
                debug_assert!(
                    false,
                    "validated live connection controls cannot exceed configured capacity"
                );
                false
            } else {
                controls.connection_close.push_back(token);
                true
            }
        };
        if inserted {
            self.notify_reactor();
        }
    }

    pub(in crate::v2::engine) fn request_connect_cancel(&self, request: Arc<OutboundRequest>) {
        lock_unpoison(&self.controls)
            .connect_cancel
            .push_back(request);
        self.notify_reactor();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn request_connection_error(&self, token: ConnectionToken) {
        lock_unpoison(&self.controls)
            .connection_error
            .push_back(token);
        self.notify_reactor();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn request_connection_disconnect(&self, token: ConnectionToken) {
        lock_unpoison(&self.controls)
            .connection_disconnect
            .push_back(token);
        self.notify_reactor();
    }

    #[cfg(any(test, feature = "test-hooks"))]
    pub(in crate::v2::engine) fn request_fail_next_qp_destroy(&self, token: ConnectionToken) {
        lock_unpoison(&self.controls)
            .connection_fail_qp_destroy
            .push_back(token);
        self.notify_reactor();
    }

    pub(in crate::v2::engine) fn request_shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::AcqRel) {
            self.shutdown_command_pending.store(true, Ordering::Release);
            self.signal.notify_reactor();
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn close_admission(&self) {
        self.close_admission_with(Error::DriverShutdown);
    }

    pub(in crate::v2::engine) fn close_admission_with(&self, error: Error) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            *lock_unpoison(&self.closed_error) = Some(error);
            self.connect_permits.close();
            self.connection_permits.close();
            self.listen_permits.close();
            self.operation_permits.close();
        }
    }

    pub(in crate::v2::engine) fn admission_error(&self) -> Option<Error> {
        lock_unpoison(&self.closed_error).clone()
    }

    pub(in crate::v2::engine) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn drain_ordinary(&self, error: Error) {
        let mut queues = lock_unpoison(&self.queues);
        let commands: Vec<_> = queues.connect.drain(..).collect();
        let listens: Vec<_> = queues.listen.drain(..).collect();
        let accepts: Vec<_> = queues.accept.drain(..).collect();
        let operations: Vec<_> = queues.operation.drain(..).collect();
        let protocols: Vec<_> = queues.protocol.drain(..).collect();
        drop(queues);
        for command in commands.into_iter().chain(listens) {
            match command {
                SessionCommand::Connect { request, .. } => {
                    request.complete_failure(error.clone());
                }
                SessionCommand::Listen { request, .. } => {
                    request.complete(Err(error.clone()));
                }
                #[cfg(any(test, feature = "test-hooks"))]
                SessionCommand::TestInstall { request, .. } => {
                    request.complete_failure(error.clone());
                }
            }
        }
        for (_, request) in accepts {
            request.release_permit();
            request.complete(Err(error.clone()));
        }
        for (command, _permit) in operations {
            command.cancel_before_execution(error.clone());
        }
        for entry in protocols {
            match entry {
                ProtocolQueueEntry::Command(command, _permit) => {
                    command.reject_after_pending(error.clone());
                }
                ProtocolQueueEntry::Publication(actions) => actions.publish_synchronously(),
            }
        }
    }

    pub(in crate::v2::engine) fn drain_ordinary_into(
        &self,
        error: Error,
        actions: &mut super::ReactorActions,
    ) {
        let mut queues = lock_unpoison(&self.queues);
        let commands: Vec<_> = queues.connect.drain(..).collect();
        let listens: Vec<_> = queues.listen.drain(..).collect();
        let accepts: Vec<_> = queues.accept.drain(..).collect();
        let operations: Vec<_> = queues.operation.drain(..).collect();
        let protocols: Vec<_> = queues.protocol.drain(..).collect();
        drop(queues);
        for command in commands.into_iter().chain(listens) {
            match command {
                SessionCommand::Connect { request, .. } => {
                    request.complete_failure_into(error.clone(), actions);
                }
                SessionCommand::Listen { request, .. } => {
                    request.complete_into(Err(error.clone()), actions);
                }
                #[cfg(any(test, feature = "test-hooks"))]
                SessionCommand::TestInstall { request, .. } => {
                    request.complete_failure_into(error.clone(), actions);
                }
            }
        }
        for (_, request) in accepts {
            request.release_permit();
            request.complete_into(Err(error.clone()), actions);
        }
        for (command, _permit) in operations {
            command.cancel_before_execution_into(error.clone(), actions);
        }
        for entry in protocols {
            match entry {
                ProtocolQueueEntry::Command(command, _permit) => {
                    command.reject_into(error.clone(), actions);
                }
                ProtocolQueueEntry::Publication(mut pending) => {
                    pending.append_bounded_to(actions);
                    assert!(
                        pending.is_empty(),
                        "synchronous driver-drop actions have unbounded capacity"
                    );
                }
            }
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn close_ordinary(&self, error: Error) {
        self.close_admission();
        self.drain_ordinary(error);
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn pending_listener_closes(&self) -> usize {
        lock_unpoison(&self.controls).listener_close.len()
    }

    pub(in crate::v2::engine) fn service_turn_into(
        &self,
        shared: &Arc<EngineFrontendRoot>,
        io: &mut super::super::io_core::IoState,
        session: &mut SessionReactorSources,
        actions: &mut super::ReactorActions,
    ) -> CommandTurn {
        let mut session_work = false;

        let shutdown_requested = self.shutdown_command_pending.swap(false, Ordering::AcqRel);
        if shutdown_requested {
            session_work = true;
        }

        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(token) = lock_unpoison(&self.controls)
            .connection_fail_qp_destroy
            .pop_front()
        {
            if let Some(Err(error)) = session.connections.with_connection(token, |connection| {
                connection.fail_next_qp_destroy_for_test()
            }) {
                session.manager.begin_driver_failure(error);
            }
            session_work = true;
        }

        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(token) = lock_unpoison(&self.controls)
            .connection_disconnect
            .pop_front()
        {
            if let Some(Err(error)) = session
                .connections
                .with_connection(token, |connection| connection.disconnect_for_test())
            {
                session.manager.begin_driver_failure(error);
            }
            session_work = true;
        }

        let close = if actions.remaining() >= 4 {
            let mut controls = lock_unpoison(&self.controls);
            let close = controls.connection_close.pop_front();
            if let Some(token) = close {
                controls.connection_close_set.remove(&token);
            }
            close
        } else {
            None
        };
        if let Some(token) = close {
            SessionReactorSources::begin_connection_close_into(
                &session.manager,
                &mut session.cm,
                &mut session.connections,
                token,
                io,
                actions,
            );
            session_work = true;
        }

        let listener_close = {
            let mut controls = lock_unpoison(&self.controls);
            let token = controls.listener_close.pop_front();
            if let Some(token) = token {
                controls.listener_close_set.remove(&token);
            }
            token
        };
        if let Some(token) = listener_close {
            session.cm.request_listener_close(token);
            session_work = true;
        }

        let listener_work = {
            let mut controls = lock_unpoison(&self.controls);
            let token = controls.listener_work.pop_front();
            if let Some(token) = token {
                controls.listener_work_set.remove(&token);
            }
            token
        };
        if let Some(token) = listener_work {
            session.cm.enqueue_listener_work(token);
            session_work = true;
        }

        let connect_cancel = lock_unpoison(&self.controls).connect_cancel.pop_front();
        if let Some(request) = connect_cancel {
            session.connections.enqueue_cancellation(request);
            session_work = true;
        }

        #[cfg(any(test, feature = "test-hooks"))]
        if let Some(token) = lock_unpoison(&self.controls).connection_error.pop_front() {
            if let Err(error) = session
                .manager
                .transition_connection_to_error_token(&mut session.connections, token)
            {
                session.manager.begin_driver_failure(error);
            }
            session_work = true;
        }

        let cancellation = {
            let mut controls = lock_unpoison(&self.controls);
            let token = controls.operation_cancel.pop_front();
            if let Some(token) = token {
                controls.operation_cancel_set.remove(&token);
            }
            token
        };
        if let Some(token) = cancellation {
            io.cancel_operation(token);
        }

        enum ReadyCommand {
            Session(SessionCommand),
            Accept(ListenerToken, Arc<AcceptRequest>),
            Operation(Arc<OperationCommand>, OwnedSemaphorePermit),
            Protocol(ProtocolQueueEntry),
        }
        let terminal_error = self.closed.load(Ordering::Acquire).then(|| {
            shared
                .session
                .admission_error()
                .unwrap_or(Error::DriverShutdown)
        });
        let listener_slot_available = session.cm.listener_slot_available();
        let command = if actions.remaining() != 0 {
            let mut queues = lock_unpoison(&self.queues);
            let mut selected = None;
            for offset in 0..5 {
                let class = (queues.next_class + offset) % 5;
                let required_actions = if class == 3 && terminal_error.is_none() {
                    // Starting an operation can synchronously consume an early
                    // CQE (event + operation wake + close wake) before the
                    // command-completion wake is detached.
                    4
                } else {
                    1
                };
                if !actions.can_accept(required_actions) {
                    continue;
                }
                selected = match class {
                    0 => queues.connect.pop_front().map(ReadyCommand::Session),
                    1 if listener_slot_available || terminal_error.is_some() => {
                        queues.listen.pop_front().map(ReadyCommand::Session)
                    }
                    1 => None,
                    2 => queues
                        .accept
                        .pop_front()
                        .map(|(listener, request)| ReadyCommand::Accept(listener, request)),
                    3 => queues
                        .operation
                        .pop_front()
                        .map(|(command, permit)| ReadyCommand::Operation(command, permit)),
                    4 => queues.protocol.pop_front().map(ReadyCommand::Protocol),
                    _ => unreachable!(),
                };
                if selected.is_some() {
                    queues.next_class = (class + 1) % 5;
                    break;
                }
            }
            selected
        } else {
            None
        };
        if let Some(command) = command {
            match command {
                ReadyCommand::Session(SessionCommand::Connect {
                    request,
                    reservation,
                    ..
                }) => {
                    if let Some(error) = terminal_error {
                        request.complete_failure_into(error, actions);
                    } else {
                        session.connections.enqueue_outbound(request, reservation);
                        session_work = true;
                    }
                }
                ReadyCommand::Session(SessionCommand::Listen {
                    request,
                    _permit: permit,
                }) => {
                    if let Some(error) = terminal_error {
                        request.complete_into(Err(error), actions);
                    } else {
                        match session.cm.reserve_listener(&request) {
                            Ok(token) => {
                                session.cm.enqueue_listen(token, request);
                                session_work = true;
                            }
                            Err(Error::CapacityExhausted) => {
                                lock_unpoison(&self.queues).listen.push_front(
                                    SessionCommand::Listen {
                                        request,
                                        _permit: permit,
                                    },
                                );
                            }
                            Err(error) => request.complete_into(Err(error), actions),
                        }
                    }
                }
                ReadyCommand::Accept(listener, request) => {
                    if let Some(error) = terminal_error {
                        request.release_permit();
                        request.complete_into(Err(error), actions);
                    } else {
                        let result = session.cm.admit_accept(listener, Arc::clone(&request));
                        match result {
                            Ok(()) => {
                                session.cm.enqueue_listener_work(listener);
                                session_work = true;
                            }
                            Err(error) => {
                                request.release_permit();
                                request.complete_into(Err(error), actions);
                            }
                        }
                    }
                }
                #[cfg(any(test, feature = "test-hooks"))]
                ReadyCommand::Session(SessionCommand::TestInstall {
                    request,
                    reservation,
                    ..
                }) => {
                    if let Some(error) = terminal_error {
                        request.complete_failure_into(error, actions);
                    } else {
                        request.execute_into(
                            &session.manager,
                            &mut session.connections,
                            reservation,
                            actions,
                        );
                    }
                }
                ReadyCommand::Operation(command, permit) => {
                    if let Some(error) = terminal_error {
                        command.cancel_before_execution_into(error, actions);
                    } else {
                        command.execute_into(&mut session.connections, io, actions);
                    }
                    drop(permit);
                }
                ReadyCommand::Protocol(entry) => {
                    let mut publication = match entry {
                        ProtocolQueueEntry::Command(command, permit) => {
                            let count = command.len();
                            let mut publication = super::DeferredProtocolActions::new(count);
                            if command.receiver_lost() {
                                drop(command);
                            } else if command.is_cancelled() {
                                command.cancel_into(publication.actions_mut());
                            } else if let Some(error) = terminal_error {
                                command.reject_into(error, publication.actions_mut());
                            } else {
                                command.execute_into(
                                    &mut session.connections,
                                    io,
                                    publication.actions_mut(),
                                );
                            }
                            drop(permit);
                            publication
                        }
                        ProtocolQueueEntry::Publication(publication) => publication,
                    };
                    publication.append_bounded_to(actions);
                    if !publication.is_empty() {
                        lock_unpoison(&self.queues)
                            .protocol
                            .push_front(ProtocolQueueEntry::Publication(publication));
                    }
                }
            }
        }

        if session_work {
            self.signal.notify_reactor();
        }
        let has_more = self.has_runnable(session.cm.listener_slot_available());
        if has_more {
            self.signal.notify_reactor();
        }
        CommandTurn {
            session_work,
            has_more,
            shutdown_requested,
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_turn(
        &self,
        shared: &Arc<EngineFrontendRoot>,
        io: &mut super::super::io_core::IoState,
        session: &mut SessionReactorSources,
    ) -> CommandTurn {
        let mut actions = super::ReactorActions::default();
        let report = self.service_turn_into(shared, io, session, &mut actions);
        actions.publish();
        report
    }

    pub(in crate::v2::engine) fn has_pending(&self) -> bool {
        if self.shutdown_command_pending.load(Ordering::Acquire) {
            return true;
        }
        let queues = lock_unpoison(&self.queues);
        if !queues.connect.is_empty()
            || !queues.listen.is_empty()
            || !queues.accept.is_empty()
            || !queues.operation.is_empty()
            || !queues.protocol.is_empty()
        {
            return true;
        }
        drop(queues);
        let controls = lock_unpoison(&self.controls);
        !controls.connection_close.is_empty()
            || !controls.connect_cancel.is_empty()
            || !controls.listener_close.is_empty()
            || !controls.listener_work.is_empty()
            || {
                #[cfg(any(test, feature = "test-hooks"))]
                {
                    !controls.connection_error.is_empty()
                        || !controls.connection_disconnect.is_empty()
                        || !controls.connection_fail_qp_destroy.is_empty()
                }
                #[cfg(not(any(test, feature = "test-hooks")))]
                {
                    false
                }
            }
            || !controls.operation_cancel.is_empty()
    }

    pub(in crate::v2::engine) fn has_runnable(&self, listener_slot_available: bool) -> bool {
        if self.shutdown_command_pending.load(Ordering::Acquire) {
            return true;
        }
        let queues = lock_unpoison(&self.queues);
        let listener_runnable = listener_slot_available || self.closed.load(Ordering::Acquire);
        if !queues.connect.is_empty()
            || (!queues.listen.is_empty() && listener_runnable)
            || !queues.accept.is_empty()
            || !queues.operation.is_empty()
            || !queues.protocol.is_empty()
        {
            return true;
        }
        drop(queues);
        let controls = lock_unpoison(&self.controls);
        !controls.connection_close.is_empty()
            || !controls.connect_cancel.is_empty()
            || !controls.listener_close.is_empty()
            || !controls.listener_work.is_empty()
            || {
                #[cfg(any(test, feature = "test-hooks"))]
                {
                    !controls.connection_error.is_empty()
                        || !controls.connection_disconnect.is_empty()
                        || !controls.connection_fail_qp_destroy.is_empty()
                }
                #[cfg(not(any(test, feature = "test-hooks")))]
                {
                    false
                }
            }
            || !controls.operation_cancel.is_empty()
    }

    pub(in crate::v2::engine) fn shutdown_requested(&self) -> bool {
        self.shutdown.load(Ordering::Acquire)
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn available_connect_permits(&self) -> usize {
        self.connect_permits.available_permits()
    }

    pub(in crate::v2::engine) fn connection_admission(&self) -> Arc<Semaphore> {
        Arc::clone(&self.connection_permits)
    }

    pub(in crate::v2::engine) fn connection_reservations(&self) -> usize {
        self.max_connection_controls
            .saturating_sub(self.connection_permits.available_permits())
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn pending_connects(&self) -> usize {
        lock_unpoison(&self.queues).connect.len()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn pending_listens(&self) -> usize {
        lock_unpoison(&self.queues).listen.len()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn pending_accepts(&self) -> usize {
        lock_unpoison(&self.queues).accept.len()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn available_listen_permits(&self) -> usize {
        self.listen_permits.available_permits()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn pending_protocol(&self) -> usize {
        lock_unpoison(&self.queues).protocol.len()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn available_operation_permits(&self) -> usize {
        self.operation_permits.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Context;

    use super::super::super::driver::WorkSignal;
    use super::super::super::io::{
        ProtocolCommand, ProtocolTestAdmission, ProtocolTestProbe, event_port,
    };
    use super::super::super::session::listener::{RdmaListenerConfig, listen};
    use super::super::super::{CompletionMode, test_engine_pair, test_engine_pair_with_capacity};
    use super::CommandIngress;

    fn protocol_probe(cancelled: bool) -> ProtocolTestProbe {
        ProtocolTestProbe {
            cancelled: Arc::new(AtomicBool::new(cancelled)),
            executed: Arc::new(AtomicUsize::new(0)),
            resolved: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(AtomicUsize::new(0)),
            published: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn protocol_admission(engine: &super::super::super::RdmaEngine) -> Arc<ProtocolTestAdmission> {
        ProtocolTestAdmission::new(
            Arc::downgrade(&engine.shared.commands),
            Arc::downgrade(&engine.shared.session),
        )
    }

    fn test_protocol_command(
        admission: &ProtocolTestAdmission,
        operations: usize,
        actions: usize,
        probe: ProtocolTestProbe,
    ) -> (ProtocolCommand, super::super::super::io::IoEventReceiver) {
        let (events, receiver) = event_port();
        (
            ProtocolCommand::for_test(operations, actions, events, admission.open_token(), probe),
            receiver,
        )
    }

    #[tokio::test]
    async fn connect_admission_is_bounded_and_wakes_after_release() {
        let ingress = CommandIngress::new(1, 1, Arc::new(WorkSignal::new()));
        let lane = ingress.acquire_connect().await.unwrap();
        let permit = ingress.reserve_connect(lane).unwrap();
        assert_eq!(ingress.available_connect_permits(), 0);
        assert_eq!(ingress.connection_reservations(), 1);

        let waiting = {
            let ingress = Arc::clone(&ingress);
            tokio::spawn(async move { ingress.acquire_connect().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(permit);
        assert_eq!(ingress.connection_reservations(), 0);
        let lane = waiting.await.unwrap().unwrap();
        let permit = ingress.reserve_connect(lane).unwrap();
        assert_eq!(ingress.connection_reservations(), 1);
        drop(permit);
        assert_eq!(ingress.available_connect_permits(), 1);
        assert_eq!(ingress.connection_reservations(), 0);
    }

    #[tokio::test]
    async fn closing_admission_wakes_waiters_with_driver_shutdown() {
        let ingress = CommandIngress::new(1, 1, Arc::new(WorkSignal::new()));
        let _permit = ingress.acquire_listen().await.unwrap();
        let waiting = {
            let ingress = Arc::clone(&ingress);
            tokio::spawn(async move { ingress.acquire_listen().await })
        };
        tokio::task::yield_now().await;
        ingress.close_ordinary(crate::v2::Error::DriverShutdown);
        assert!(waiting.await.unwrap().is_none());
    }

    #[tokio::test]
    async fn cancelling_the_head_waiter_allows_the_next_waiter_to_acquire() {
        let ingress = CommandIngress::new(1, 1, Arc::new(WorkSignal::new()));
        let permit = ingress.acquire_connect().await.unwrap();
        let first = {
            let ingress = Arc::clone(&ingress);
            tokio::spawn(async move { ingress.acquire_connect().await })
        };
        let second = {
            let ingress = Arc::clone(&ingress);
            tokio::spawn(async move { ingress.acquire_connect().await })
        };
        tokio::task::yield_now().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        drop(permit);
        assert!(second.await.unwrap().is_some());
    }

    #[tokio::test]
    async fn permit_waiters_are_fifo_and_do_not_barge() {
        let ingress = CommandIngress::new(1, 1, Arc::new(WorkSignal::new()));
        let permit = ingress.acquire_connect().await.unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let first = {
            let ingress = Arc::clone(&ingress);
            let tx = tx.clone();
            tokio::spawn(async move {
                let permit = ingress.acquire_connect().await.unwrap();
                tx.send(1).unwrap();
                permit
            })
        };
        tokio::task::yield_now().await;
        let second = {
            let ingress = Arc::clone(&ingress);
            tokio::spawn(async move {
                let permit = ingress.acquire_connect().await.unwrap();
                tx.send(2).unwrap();
                permit
            })
        };
        drop(permit);
        assert_eq!(rx.recv().await, Some(1));
        drop(first.await.unwrap());
        assert_eq!(rx.recv().await, Some(2));
        drop(second.await.unwrap());
    }

    #[tokio::test]
    async fn protocol_batch_permits_wait_atomically_and_restore_exact_capacity() {
        let ingress = CommandIngress::new(1, 4, Arc::new(WorkSignal::new()));
        let first = ingress.operation_batch_acquire(3).await.unwrap();
        assert_eq!(ingress.operation_permits.available_permits(), 1);
        let waiting = {
            let ingress = Arc::clone(&ingress);
            tokio::spawn(async move { ingress.operation_batch_acquire(2).await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        assert_eq!(ingress.operation_permits.available_permits(), 0);
        drop(first);
        let second = waiting.await.unwrap().unwrap();
        assert_eq!(ingress.operation_permits.available_permits(), 2);
        drop(second);
        assert_eq!(ingress.operation_permits.available_permits(), 4);
    }

    #[tokio::test]
    async fn saturated_protocol_admission_stays_frontend_owned_until_capacity_recovers() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let commands = Arc::clone(&engine.shared.commands);
        let capacity = commands.available_operation_permits();
        let blocker = commands.operation_batch_acquire(capacity).await.unwrap();
        let adapter = protocol_admission(&engine);
        let probe = protocol_probe(false);
        let (command, _events) = test_protocol_command(&adapter, 3, 0, probe.clone());
        assert!(adapter.submit(command).is_ok());

        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert_eq!(adapter.poll(&mut cx, 1), 0);
        assert_eq!(commands.pending_protocol(), 0);
        assert_eq!(probe.executed.load(Ordering::Acquire), 0);
        assert_eq!(probe.dropped.load(Ordering::Acquire), 0);

        drop(blocker);
        tokio::task::yield_now().await;
        assert_eq!(adapter.poll(&mut cx, 1), 1);
        assert_eq!(commands.pending_protocol(), 1);
        commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert_eq!(probe.executed.load(Ordering::Acquire), 1);
        assert_eq!(probe.dropped.load(Ordering::Acquire), 1);
        assert_eq!(commands.available_operation_permits(), capacity);
    }

    #[tokio::test]
    async fn pending_protocol_payload_is_bounded_by_operation_capacity() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        let commands = Arc::clone(&engine.shared.commands);
        let capacity = commands.available_operation_permits();
        assert!(capacity > 1);
        let blocker = commands.operation_batch_acquire(capacity).await.unwrap();
        let adapter = protocol_admission(&engine);

        let first = protocol_probe(false);
        let (command, _events) = test_protocol_command(&adapter, capacity - 1, 0, first.clone());
        assert!(adapter.submit(command).is_ok());

        let overflow = protocol_probe(false);
        let (command, _events) = test_protocol_command(&adapter, 2, 0, overflow.clone());
        assert!(matches!(
            adapter.submit(command),
            Err((crate::v2::Error::CapacityExhausted, _))
        ));
        assert_eq!(first.dropped.load(Ordering::Acquire), 0);
        drop(blocker);
    }

    #[tokio::test]
    async fn cancellation_before_and_after_protocol_queueing_resolves_once() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let commands = Arc::clone(&engine.shared.commands);
        let capacity = commands.available_operation_permits();
        let blocker = commands.operation_batch_acquire(capacity).await.unwrap();
        let adapter = protocol_admission(&engine);
        let before = protocol_probe(false);
        let (command, events) = test_protocol_command(&adapter, 1, 0, before.clone());
        assert!(adapter.submit(command).is_ok());
        before.cancelled.store(true, Ordering::Release);
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert_eq!(adapter.poll(&mut cx, 1), 1);
        assert_eq!(before.resolved.load(Ordering::Acquire), 1);
        assert_eq!(before.executed.load(Ordering::Acquire), 0);
        assert_eq!(before.dropped.load(Ordering::Acquire), 1);
        assert!(matches!(
            events.pop(),
            Some(super::super::super::io::IoEvent::Submission(
                super::super::super::io::IoSubmissionDisposition::FullyUnaccepted {
                    proven_unaccepted: 1,
                    ..
                }
            ))
        ));
        assert!(events.pop().is_none());

        drop(blocker);
        let after = protocol_probe(false);
        let (command, _events) = test_protocol_command(&adapter, 1, 0, after.clone());
        assert!(adapter.submit(command).is_ok());
        assert_eq!(adapter.poll(&mut cx, 1), 1);
        assert_eq!(commands.pending_protocol(), 1);
        after.cancelled.store(true, Ordering::Release);
        commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert_eq!(after.resolved.load(Ordering::Acquire), 1);
        assert_eq!(after.executed.load(Ordering::Acquire), 0);
        assert_eq!(after.dropped.load(Ordering::Acquire), 1);
        assert_eq!(commands.available_operation_permits(), capacity);
    }

    #[tokio::test]
    async fn pending_protocol_payload_is_bounded_across_connections() {
        let (engine, _driver) = test_engine_pair(CompletionMode::Polling);
        let commands = Arc::clone(&engine.shared.commands);
        let capacity = commands.available_operation_permits();
        let blocker = commands.operation_batch_acquire(capacity).await.unwrap();
        let first = protocol_admission(&engine);
        let second = protocol_admission(&engine);

        let first_probe = protocol_probe(false);
        let (command, _events) = test_protocol_command(&first, capacity, 0, first_probe);
        assert!(first.submit(command).is_ok());

        let second_probe = protocol_probe(false);
        let (command, _events) = test_protocol_command(&second, 1, 0, second_probe.clone());
        assert!(matches!(
            second.submit(command),
            Err((crate::v2::Error::CapacityExhausted, _))
        ));

        drop(first);
        let (command, _events) = test_protocol_command(&second, 1, 0, second_probe);
        assert!(
            second.submit(command).is_ok(),
            "dropping one connection's pending payload must release shared capacity"
        );
        drop(blocker);
    }

    #[tokio::test]
    async fn queued_protocol_receiver_loss_prevents_provider_execution_and_restores_permit() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let commands = Arc::clone(&engine.shared.commands);
        let capacity = commands.available_operation_permits();
        let adapter = protocol_admission(&engine);
        let probe = protocol_probe(false);
        let (command, events) = test_protocol_command(&adapter, 2, 0, probe.clone());
        assert!(adapter.submit(command).is_ok());
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert_eq!(adapter.poll(&mut cx, 1), 1);
        assert_eq!(commands.available_operation_permits(), capacity - 2);
        drop(events);

        commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert_eq!(probe.executed.load(Ordering::Acquire), 0);
        assert_eq!(probe.resolved.load(Ordering::Acquire), 0);
        assert_eq!(probe.dropped.load(Ordering::Acquire), 1);
        assert_eq!(commands.available_operation_permits(), capacity);
    }

    #[tokio::test]
    async fn large_protocol_batch_drains_publication_across_bounded_turns() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let commands = Arc::clone(&engine.shared.commands);
        let capacity = commands.available_operation_permits();
        let adapter = protocol_admission(&engine);
        let probe = protocol_probe(false);
        let (command, _events) = test_protocol_command(&adapter, 12, 12, probe.clone());
        assert!(adapter.submit(command).is_ok());
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert_eq!(adapter.poll(&mut cx, 1), 1);

        let mut actions = super::super::ReactorActions::default();
        for _ in 0..28 {
            actions.push_operation(|| {});
        }
        let first = commands.service_turn_into(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
            &mut actions,
        );
        assert!(first.has_more);
        actions.publish();
        assert_eq!(probe.executed.load(Ordering::Acquire), 1);
        assert_eq!(probe.published.load(Ordering::Acquire), 4);
        assert_eq!(commands.available_operation_permits(), capacity);

        let second = commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert!(!second.has_more);
        assert_eq!(probe.published.load(Ordering::Acquire), 12);
        assert_eq!(probe.dropped.load(Ordering::Acquire), 1);
    }

    #[tokio::test]
    async fn closed_protocol_admission_rejects_without_consuming_capacity() {
        let ingress = CommandIngress::new(1, 4, Arc::new(WorkSignal::new()));
        ingress.close_admission();
        assert!(ingress.operation_batch_acquire(2).await.is_none());
        assert_eq!(ingress.operation_permits.available_permits(), 4);
    }

    #[test]
    fn oversized_protocol_batch_is_rejected_before_waiting() {
        let ingress = CommandIngress::new(1, 4, Arc::new(WorkSignal::new()));
        assert!(matches!(
            ingress.validate_operation_batch(5),
            Err(crate::v2::Error::InvalidConfig(_))
        ));
        assert_eq!(ingress.operation_permits.available_permits(), 4);
    }

    #[test]
    fn shutdown_requests_coalesce() {
        let ingress = CommandIngress::new(1, 1, Arc::new(WorkSignal::new()));
        ingress.request_shutdown();
        ingress.request_shutdown();
        assert!(ingress.shutdown.load(std::sync::atomic::Ordering::Acquire));
    }

    #[test]
    fn blocked_listen_head_runs_after_exact_listener_slot_release() {
        let (engine, mut driver) = test_engine_pair_with_capacity(CompletionMode::Polling, 1);
        let occupied = driver.reactor.session.cm.reserve_test_listener_slot(1);
        let mut pending = Box::pin(listen(
            Arc::clone(&engine.shared.session),
            Arc::clone(&engine.shared.commands),
            "127.0.0.2:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(1),
        ));
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(pending.as_mut().poll(&mut cx).is_pending());
        let report = engine.shared.commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert!(!report.has_more);
        assert_eq!(engine.shared.commands.pending_listens(), 1);
        assert!(
            driver
                .reactor
                .session
                .cm
                .pending_listen_addresses()
                .is_empty()
        );

        driver.reactor.session.cm.release_test_listener(occupied);
        let report = engine.shared.commands.service_turn(
            &engine.shared,
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session,
        );
        assert!(report.session_work);
        assert_eq!(engine.shared.commands.pending_listens(), 0);
        assert_eq!(
            driver.reactor.session.cm.pending_listen_addresses(),
            vec!["127.0.0.2:0".parse().unwrap()]
        );
    }

    #[test]
    fn blocked_listen_cancellation_releases_lane_without_slot_transfer() {
        let (engine, mut driver) = test_engine_pair_with_capacity(CompletionMode::Polling, 1);
        let _occupied = driver.reactor.session.cm.reserve_test_listener_slot(1);
        let mut pending = Box::pin(listen(
            Arc::clone(&engine.shared.session),
            Arc::clone(&engine.shared.commands),
            "127.0.0.2:0".parse().unwrap(),
            RdmaListenerConfig::default().backlog(1),
        ));
        let waker = futures_util::task::noop_waker();
        let mut cx = Context::from_waker(&waker);
        assert!(pending.as_mut().poll(&mut cx).is_pending());
        assert_eq!(engine.shared.commands.available_listen_permits(), 0);
        drop(pending);
        assert_eq!(engine.shared.commands.pending_listens(), 0);
        assert_eq!(engine.shared.commands.available_listen_permits(), 1);
    }

    #[test]
    fn shutdown_and_driver_drop_dispose_a_slot_blocked_listen_once() {
        for drop_driver in [false, true] {
            let (engine, mut driver) = test_engine_pair_with_capacity(CompletionMode::Polling, 1);
            let _occupied = driver.reactor.session.cm.reserve_test_listener_slot(1);
            let mut pending = Box::pin(listen(
                Arc::clone(&engine.shared.session),
                Arc::clone(&engine.shared.commands),
                "127.0.0.2:0".parse().unwrap(),
                RdmaListenerConfig::default().backlog(1),
            ));
            let waker = futures_util::task::noop_waker();
            let mut cx = Context::from_waker(&waker);
            assert!(pending.as_mut().poll(&mut cx).is_pending());
            if drop_driver {
                drop(driver);
            } else {
                engine.shared.request_shutdown();
                engine.shared.commands.service_turn(
                    &engine.shared,
                    driver.reactor.io.core_mut(),
                    &mut driver.reactor.session,
                );
            }
            assert!(matches!(
                pending.as_mut().poll(&mut cx),
                std::task::Poll::Ready(Err(crate::v2::Error::DriverShutdown))
            ));
            assert_eq!(engine.shared.commands.pending_listens(), 0);
            assert_eq!(engine.shared.commands.available_listen_permits(), 1);
            if drop_driver {
                assert_eq!(
                    super::super::super::registry::lock_unpoison(&engine.shared.diagnostics)
                        .cm_retained_owners,
                    0,
                    "driver drop releases a resource-free occupied listener slot"
                );
            }
        }
    }
}
