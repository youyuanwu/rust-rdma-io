//! Bounded typed command admission into the driver-owned reactor.

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

mod admission;
mod coalesced;
mod controls;
mod queues;
mod service;
mod shutdown;

use controls::ControlQueue;
use queues::{CommandQueues, ProtocolQueueEntry, SessionCommand};

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
    connection_capacity: usize,
    operation_capacity: usize,
    pending_protocol_operations: Arc<AtomicUsize>,
    queues: Mutex<CommandQueues>,
    controls: Mutex<ControlQueue>,
    shutdown: AtomicBool,
    shutdown_command_pending: AtomicBool,
    closed: AtomicBool,
    closed_error: Mutex<Option<Error>>,
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
            connection_capacity,
            operation_capacity,
            pending_protocol_operations: Arc::new(AtomicUsize::new(0)),
            queues: Mutex::new(CommandQueues::default()),
            controls: Mutex::new(ControlQueue::new(connection_capacity, operation_capacity)),
            shutdown: AtomicBool::new(false),
            shutdown_command_pending: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            closed_error: Mutex::new(None),
            signal,
            diagnostics,
        })
    }
}

impl CommandIngress {
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
        self.connection_capacity
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

    use super::super::super::driver::REACTOR_WORK;
    use super::super::super::driver::WorkSignal;
    use super::super::super::io::{
        ProtocolCommand, ProtocolTestAdmission, ProtocolTestProbe, event_port,
    };
    use super::super::super::registry::lock_unpoison;
    use super::super::super::registry::{ConnectionToken, ListenerToken, OperationToken};
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

    #[test]
    fn first_control_insert_notifies_and_duplicate_does_not() {
        let signal = Arc::new(WorkSignal::new());
        let ingress = CommandIngress::new(1, 1, Arc::clone(&signal));
        let operation = OperationToken::decode(1);
        ingress.request_operation_cancel(operation);
        assert_eq!(signal.take(), REACTOR_WORK);
        ingress.request_operation_cancel(operation);
        assert_eq!(signal.take(), 0);

        let listener = ListenerToken::decode(1);
        ingress.request_listener_work(listener);
        assert_eq!(signal.take(), REACTOR_WORK);
        ingress.request_listener_work(listener);
        assert_eq!(signal.take(), 0);
    }

    #[test]
    fn close_control_requests_notify_only_on_first_insertion() {
        let (engine, mut driver) = test_engine_pair_with_capacity(CompletionMode::Polling, 1);
        engine.shared.work_signal.take();
        let connection = engine
            .shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, 1)
            .unwrap()
            .pop()
            .unwrap();
        let connection_token = connection.session_token();
        engine
            .shared
            .commands
            .request_connection_close(&engine.shared.session, connection_token);
        assert_eq!(engine.shared.work_signal.take(), REACTOR_WORK);
        engine
            .shared
            .commands
            .request_connection_close(&engine.shared.session, connection_token);
        assert_eq!(engine.shared.work_signal.take(), 0);

        let (_listener, listener) = driver
            .reactor
            .session
            .cm
            .test_listener(&driver.reactor.session.manager, 1);
        let admission = driver
            .reactor
            .session
            .cm
            .listener_admission_for_test(listener);
        engine
            .shared
            .commands
            .request_listener_close(&engine.shared.session, listener, &admission);
        assert_eq!(engine.shared.work_signal.take(), REACTOR_WORK);
        engine
            .shared
            .commands
            .request_listener_close(&engine.shared.session, listener, &admission);
        assert_eq!(engine.shared.work_signal.take(), 0);
        drop(connection);
        drop(driver);
    }

    #[test]
    fn connection_drop_propagates_the_distinct_overflow_panic() {
        let (engine, mut driver) = test_engine_pair_with_capacity(CompletionMode::Polling, 1);
        let connection = engine
            .shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, 1)
            .unwrap()
            .pop()
            .unwrap();
        lock_unpoison(&engine.shared.commands.controls)
            .connection_close
            .push(ConnectionToken::decode(u64::MAX));

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| drop(connection)));
        assert!(panic.is_err());
        drop(driver);
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
