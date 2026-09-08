//! Bounded typed command admission ahead of the current session backend.

use std::collections::{HashSet, VecDeque};
use std::future::poll_fn;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use super::super::driver::{COMMAND_WORK, SESSION_WORK, WorkSignal};
use super::super::io_core::OperationCommand;
use super::super::registry::{ConnectionToken, Lookup, lock_unpoison, read_unpoison};
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
    next_class: usize,
}

#[derive(Default)]
struct ControlQueue {
    connection_close: VecDeque<ConnectionToken>,
    connection_close_set: HashSet<ConnectionToken>,
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
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn close_ordinary(&self, error: Error) {
        self.close_admission();
        self.drain_ordinary(error);
    }

    pub(in crate::v2::engine) fn service_turn(&self, shared: &Arc<EngineShared>) -> CommandTurn {
        let mut session_work = false;

        if self.shutdown.swap(false, Ordering::AcqRel) {
            shared.start_shutdown_progress();
            session_work = true;
        }

        let close = {
            let mut controls = lock_unpoison(&self.controls);
            let close = controls.connection_close.pop_front();
            if let Some(token) = close {
                controls.connection_close_set.remove(&token);
            }
            close
        };
        if let Some(token) = close {
            shared.session.request_connection_close(token);
            session_work = true;
        }

        enum ReadyCommand {
            Session(SessionCommand),
            Operation(Arc<OperationCommand>, OwnedSemaphorePermit),
        }
        let command = {
            let mut queues = lock_unpoison(&self.queues);
            let mut selected = None;
            for offset in 0..3 {
                let class = (queues.next_class + offset) % 3;
                selected = match class {
                    0 => queues.connect.pop_front().map(ReadyCommand::Session),
                    1 => queues.listen.pop_front().map(ReadyCommand::Session),
                    2 => queues
                        .operation
                        .pop_front()
                        .map(|(command, permit)| ReadyCommand::Operation(command, permit)),
                    _ => unreachable!(),
                };
                if selected.is_some() {
                    queues.next_class = (class + 1) % 3;
                    break;
                }
            }
            selected
        };
        if let Some(command) = command {
            match command {
                ReadyCommand::Session(SessionCommand::Connect { request, .. }) => {
                    shared.session.cm.enqueue(request);
                    session_work = true;
                }
                ReadyCommand::Session(SessionCommand::Listen { request, .. }) => {
                    shared.session.cm.enqueue_listen(request);
                    session_work = true;
                }
                ReadyCommand::Operation(command, _permit) => {
                    command.execute(&shared.session);
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

    fn has_pending(&self) -> bool {
        if self.shutdown.load(Ordering::Acquire) {
            return true;
        }
        let queues = lock_unpoison(&self.queues);
        if !queues.connect.is_empty() || !queues.listen.is_empty() || !queues.operation.is_empty() {
            return true;
        }
        drop(queues);
        !lock_unpoison(&self.controls).connection_close.is_empty()
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
    pub(in crate::v2::engine) fn pending_operations(&self) -> usize {
        lock_unpoison(&self.queues).operation.len()
    }

    #[cfg(test)]
    pub(in crate::v2::engine) fn available_operation_permits(&self) -> usize {
        self.operation_permits.available_permits()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::super::driver::WorkSignal;
    use super::CommandIngress;

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

    #[test]
    fn shutdown_requests_coalesce() {
        let ingress = CommandIngress::new(1, 1, Arc::new(WorkSignal::new()));
        ingress.request_shutdown();
        ingress.request_shutdown();
        assert!(ingress.shutdown.load(std::sync::atomic::Ordering::Acquire));
    }
}
