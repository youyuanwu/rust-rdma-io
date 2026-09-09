//! Bounded typed command admission ahead of the current session backend.

use std::collections::{HashSet, VecDeque};
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::super::driver::{COMMAND_WORK, SESSION_WORK, WorkSignal};
use super::super::io::ProtocolCommand;
use super::super::io_core::OperationCommand;
use super::super::registry::{
    ConnectionToken, Lookup, OperationToken, lock_unpoison, read_unpoison,
};
use super::super::session::SessionManager;
use super::super::session::cm::OutboundRequest;
use super::super::session::listener::ListenRequest;
use super::super::{EngineShared, Error};

enum SessionCommand {
    Connect {
        request: Arc<OutboundRequest>,
        _permit: OwnedSemaphorePermit,
    },
    Listen {
        request: Arc<ListenRequest>,
        _permit: OwnedSemaphorePermit,
    },
}

#[derive(Default)]
struct CommandQueues {
    connect: VecDeque<SessionCommand>,
    listen: VecDeque<SessionCommand>,
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
    operation_cancel: VecDeque<OperationToken>,
    operation_cancel_set: HashSet<OperationToken>,
}

pub(in crate::v2::engine) struct CommandTurn {
    pub(in crate::v2::engine) session_work: bool,
    pub(in crate::v2::engine) has_more: bool,
}

/// Cloneable, resource-free frontend admission endpoint.
///
/// Connect and listen permits bound only commands waiting for the driver. The
/// existing session backend remains the sole owner of provider interaction and
/// lifecycle state after dequeue.
pub(in crate::v2::engine) struct CommandIngress {
    connect_permits: Arc<Semaphore>,
    listen_permits: Arc<Semaphore>,
    operation_permits: Arc<Semaphore>,
    operation_capacity: usize,
    queues: Mutex<CommandQueues>,
    controls: Mutex<ControlQueue>,
    shutdown: AtomicBool,
    closed: AtomicBool,
    max_connection_controls: usize,
    signal: Arc<WorkSignal>,
}

impl CommandIngress {
    pub(in crate::v2::engine) fn new(
        connection_capacity: usize,
        operation_capacity: usize,
        signal: Arc<WorkSignal>,
    ) -> Arc<Self> {
        Arc::new(Self {
            connect_permits: Arc::new(Semaphore::new(connection_capacity)),
            listen_permits: Arc::new(Semaphore::new(connection_capacity)),
            operation_permits: Arc::new(Semaphore::new(operation_capacity)),
            operation_capacity,
            queues: Mutex::new(CommandQueues::default()),
            controls: Mutex::new(ControlQueue::default()),
            shutdown: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            max_connection_controls: connection_capacity,
            signal,
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
        permit: OwnedSemaphorePermit,
    ) {
        lock_unpoison(&self.queues)
            .connect
            .push_back(SessionCommand::Connect {
                request,
                _permit: permit,
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

    pub(in crate::v2::engine) fn publish_command_work(&self) {
        self.signal.publish(COMMAND_WORK);
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

    pub(in crate::v2::engine) fn enqueue_operation(
        &self,
        manager: &SessionManager,
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
    pub(in crate::v2::engine) fn enqueue_protocol(
        &self,
        manager: &SessionManager,
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
            self.publish_command_work();
        }
    }

    pub(in crate::v2::engine) fn request_connection_close(
        &self,
        manager: &SessionManager,
        token: ConnectionToken,
    ) {
        let inserted = {
            let _admission = read_unpoison(&manager.admission);
            if self.closed.load(Ordering::Acquire)
                || manager.admission_error().is_some()
                || !matches!(manager.connections.lookup(token), Lookup::Occupied(_))
            {
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
            self.publish_command_work();
        }
    }

    pub(in crate::v2::engine) fn request_shutdown(&self) {
        if !self.shutdown.swap(true, Ordering::AcqRel) {
            self.signal.publish(COMMAND_WORK);
        }
    }

    pub(in crate::v2::engine) fn close_admission(&self) {
        if !self.closed.swap(true, Ordering::AcqRel) {
            self.connect_permits.close();
            self.listen_permits.close();
            self.operation_permits.close();
        }
    }

    pub(in crate::v2::engine) fn drain_ordinary(&self, error: Error) {
        let mut queues = lock_unpoison(&self.queues);
        let commands: Vec<_> = queues.connect.drain(..).collect();
        let listens: Vec<_> = queues.listen.drain(..).collect();
        let operations: Vec<_> = queues.operation.drain(..).collect();
        let protocols: Vec<_> = queues.protocol.drain(..).collect();
        drop(queues);
        for command in commands.into_iter().chain(listens) {
            match command {
                SessionCommand::Connect { request, .. } => {
                    drop(request.take_reservation());
                    request.complete_failure(error.clone());
                }
                SessionCommand::Listen { request, .. } => {
                    request.complete(Err(error.clone()));
                }
            }
        }
        for (command, _permit) in operations {
            command.cancel_before_execution(error.clone());
        }
        for entry in protocols {
            match entry {
                ProtocolQueueEntry::Command(command, _permit) => {
                    command.reject(error.clone());
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
        let operations: Vec<_> = queues.operation.drain(..).collect();
        let protocols: Vec<_> = queues.protocol.drain(..).collect();
        drop(queues);
        for command in commands.into_iter().chain(listens) {
            match command {
                SessionCommand::Connect { request, .. } => {
                    drop(request.take_reservation());
                    request.complete_failure_into(error.clone(), actions);
                }
                SessionCommand::Listen { request, .. } => {
                    request.complete_into(Err(error.clone()), actions);
                }
            }
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

    pub(in crate::v2::engine) fn service_turn_into(
        &self,
        shared: &Arc<EngineShared>,
        io: &mut super::super::io_core::IoState,
        actions: &mut super::ReactorActions,
    ) -> CommandTurn {
        let mut session_work = false;

        if self.shutdown.swap(false, Ordering::AcqRel) {
            shared.start_shutdown_progress();
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
            shared
                .session
                .request_connection_close_into(io, token, actions);
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
            Operation(Arc<OperationCommand>, OwnedSemaphorePermit),
            Protocol(ProtocolQueueEntry),
        }
        let terminal_error = self.closed.load(Ordering::Acquire).then(|| {
            shared
                .session
                .admission_error()
                .unwrap_or(Error::DriverShutdown)
        });
        let command = if actions.remaining() != 0 {
            let mut queues = lock_unpoison(&self.queues);
            let mut selected = None;
            for offset in 0..4 {
                let class = (queues.next_class + offset) % 4;
                let required_actions = if class == 2 && terminal_error.is_none() {
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
                    1 => queues.listen.pop_front().map(ReadyCommand::Session),
                    2 => queues
                        .operation
                        .pop_front()
                        .map(|(command, permit)| ReadyCommand::Operation(command, permit)),
                    3 => queues.protocol.pop_front().map(ReadyCommand::Protocol),
                    _ => unreachable!(),
                };
                if selected.is_some() {
                    queues.next_class = (class + 1) % 4;
                    break;
                }
            }
            selected
        } else {
            None
        };
        if let Some(command) = command {
            match command {
                ReadyCommand::Session(SessionCommand::Connect { request, .. }) => {
                    if let Some(error) = terminal_error {
                        drop(request.take_reservation());
                        request.complete_failure_into(error, actions);
                    } else {
                        shared.session.cm.enqueue(request);
                        session_work = true;
                    }
                }
                ReadyCommand::Session(SessionCommand::Listen { request, .. }) => {
                    if let Some(error) = terminal_error {
                        request.complete_into(Err(error), actions);
                    } else {
                        shared.session.cm.enqueue_listen(request);
                        session_work = true;
                    }
                }
                ReadyCommand::Operation(command, permit) => {
                    if let Some(error) = terminal_error {
                        command.cancel_before_execution_into(error, actions);
                    } else {
                        command.execute_into(&shared.session, io, actions);
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
                                command.execute_into(shared, io, publication.actions_mut());
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
            self.signal.publish(SESSION_WORK);
        }
        let has_more = self.has_pending();
        if has_more {
            self.signal.publish(COMMAND_WORK);
        }
        CommandTurn {
            session_work,
            has_more,
        }
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn service_turn(
        &self,
        shared: &Arc<EngineShared>,
        io: &mut super::super::io_core::IoState,
    ) -> CommandTurn {
        let mut actions = super::ReactorActions::default();
        let report = self.service_turn_into(shared, io, &mut actions);
        actions.publish();
        report
    }

    pub(in crate::v2::engine) fn has_pending(&self) -> bool {
        if self.shutdown.load(Ordering::Acquire) {
            return true;
        }
        let queues = lock_unpoison(&self.queues);
        if !queues.connect.is_empty()
            || !queues.listen.is_empty()
            || !queues.operation.is_empty()
            || !queues.protocol.is_empty()
        {
            return true;
        }
        drop(queues);
        let controls = lock_unpoison(&self.controls);
        !controls.connection_close.is_empty() || !controls.operation_cancel.is_empty()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn available_connect_permits(&self) -> usize {
        self.connect_permits.available_permits()
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
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::task::Context;

    use super::super::super::driver::WorkSignal;
    use super::super::super::io::{
        ProtocolCommand, ProtocolTestAdmission, ProtocolTestProbe, event_port,
    };
    use super::super::super::{CompletionMode, test_engine_pair};
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

    fn protocol_adapter(engine: &super::super::super::RdmaEngine) -> Arc<ProtocolTestAdmission> {
        ProtocolTestAdmission::new(
            Arc::downgrade(&engine.shared.commands),
            Arc::downgrade(&engine.shared.session),
        )
    }

    fn test_protocol_command(
        adapter: &ProtocolTestAdmission,
        operations: usize,
        actions: usize,
        probe: ProtocolTestProbe,
    ) -> (ProtocolCommand, super::super::super::io::IoEventReceiver) {
        let (events, receiver) = event_port();
        (
            ProtocolCommand::for_test(operations, actions, events, adapter.open_token(), probe),
            receiver,
        )
    }

    #[tokio::test]
    async fn connect_admission_is_bounded_and_wakes_after_release() {
        let ingress = CommandIngress::new(1, 1, Arc::new(WorkSignal::new()));
        let permit = ingress.acquire_connect().await.unwrap();
        assert_eq!(ingress.available_connect_permits(), 0);

        let waiting = {
            let ingress = Arc::clone(&ingress);
            tokio::spawn(async move { ingress.acquire_connect().await })
        };
        tokio::task::yield_now().await;
        assert!(!waiting.is_finished());
        drop(permit);
        let permit = waiting.await.unwrap().unwrap();
        drop(permit);
        assert_eq!(ingress.available_connect_permits(), 1);
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
        let adapter = protocol_adapter(&engine);
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
        commands.service_turn(&engine.shared, driver.reactor.io.core_mut());
        assert_eq!(probe.executed.load(Ordering::Acquire), 1);
        assert_eq!(probe.dropped.load(Ordering::Acquire), 1);
        assert_eq!(commands.available_operation_permits(), capacity);
    }

    #[tokio::test]
    async fn cancellation_before_and_after_protocol_queueing_resolves_once() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let commands = Arc::clone(&engine.shared.commands);
        let capacity = commands.available_operation_permits();
        let blocker = commands.operation_batch_acquire(capacity).await.unwrap();
        let adapter = protocol_adapter(&engine);
        let before = protocol_probe(false);
        let (command, _events) = test_protocol_command(&adapter, 1, 0, before.clone());
        assert!(adapter.submit(command).is_ok());
        before.cancelled.store(true, Ordering::Release);
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert_eq!(adapter.poll(&mut cx, 1), 1);
        assert_eq!(before.resolved.load(Ordering::Acquire), 1);
        assert_eq!(before.executed.load(Ordering::Acquire), 0);
        assert_eq!(before.dropped.load(Ordering::Acquire), 1);

        drop(blocker);
        let after = protocol_probe(false);
        let (command, _events) = test_protocol_command(&adapter, 1, 0, after.clone());
        assert!(adapter.submit(command).is_ok());
        assert_eq!(adapter.poll(&mut cx, 1), 1);
        assert_eq!(commands.pending_protocol(), 1);
        after.cancelled.store(true, Ordering::Release);
        commands.service_turn(&engine.shared, driver.reactor.io.core_mut());
        assert_eq!(after.resolved.load(Ordering::Acquire), 1);
        assert_eq!(after.executed.load(Ordering::Acquire), 0);
        assert_eq!(after.dropped.load(Ordering::Acquire), 1);
        assert_eq!(commands.available_operation_permits(), capacity);
    }

    #[tokio::test]
    async fn queued_protocol_receiver_loss_prevents_provider_execution_and_restores_permit() {
        let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
        let commands = Arc::clone(&engine.shared.commands);
        let capacity = commands.available_operation_permits();
        let adapter = protocol_adapter(&engine);
        let probe = protocol_probe(false);
        let (command, events) = test_protocol_command(&adapter, 2, 0, probe.clone());
        assert!(adapter.submit(command).is_ok());
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert_eq!(adapter.poll(&mut cx, 1), 1);
        assert_eq!(commands.available_operation_permits(), capacity - 2);
        drop(events);

        commands.service_turn(&engine.shared, driver.reactor.io.core_mut());
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
        let adapter = protocol_adapter(&engine);
        let probe = protocol_probe(false);
        let (command, _events) = test_protocol_command(&adapter, 12, 12, probe.clone());
        assert!(adapter.submit(command).is_ok());
        let mut cx = Context::from_waker(futures_util::task::noop_waker_ref());
        assert_eq!(adapter.poll(&mut cx, 1), 1);

        let mut actions = super::super::ReactorActions::default();
        for _ in 0..28 {
            actions.push_operation(|| {});
        }
        let first =
            commands.service_turn_into(&engine.shared, driver.reactor.io.core_mut(), &mut actions);
        assert!(first.has_more);
        actions.publish();
        assert_eq!(probe.executed.load(Ordering::Acquire), 1);
        assert_eq!(probe.published.load(Ordering::Acquire), 4);
        assert_eq!(commands.available_operation_permits(), capacity);

        let second = commands.service_turn(&engine.shared, driver.reactor.io.core_mut());
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
}
