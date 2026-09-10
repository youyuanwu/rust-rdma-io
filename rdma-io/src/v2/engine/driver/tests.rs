#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

use super::*;
use crate::v2::engine::io_core::{
    completion_for_driver_test, install_accepted_operation_for_driver_test,
};
use crate::v2::engine::session::connection::{TestConnectionProvider, install_connection};
use crate::v2::engine::{
    RdmaConnectionConfig, RdmaEngineLifecycle, RdmaEngineTerminalError, RdmaListener,
    lock_unpoison, test_engine_pair,
};
use crate::v2::qp::{BatchPostOutcome, QpCapabilities};
use crate::wr::{PreparedRecvBatch, PreparedSendBatch};

struct CountingWaker(AtomicUsize);

impl CountingWaker {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicUsize::new(0)))
    }

    fn count(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    fn waker(self: &Arc<Self>) -> Waker {
        unsafe fn clone(ptr: *const ()) -> RawWaker {
            let value = unsafe { Arc::from_raw(ptr.cast::<CountingWaker>()) };
            let cloned = Arc::clone(&value);
            std::mem::forget(value);
            RawWaker::new(Arc::into_raw(cloned).cast(), &VTABLE)
        }
        unsafe fn wake(ptr: *const ()) {
            let value = unsafe { Arc::from_raw(ptr.cast::<CountingWaker>()) };
            value.0.fetch_add(1, Ordering::AcqRel);
        }
        unsafe fn wake_by_ref(ptr: *const ()) {
            let value = unsafe { Arc::from_raw(ptr.cast::<CountingWaker>()) };
            value.0.fetch_add(1, Ordering::AcqRel);
            std::mem::forget(value);
        }
        unsafe fn drop_waker(ptr: *const ()) {
            unsafe { drop(Arc::from_raw(ptr.cast::<CountingWaker>())) };
        }
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, wake, wake_by_ref, drop_waker);
        let raw = RawWaker::new(Arc::into_raw(Arc::clone(self)).cast(), &VTABLE);
        unsafe { Waker::from_raw(raw) }
    }
}

#[tokio::test]
async fn actions_produced_before_a_later_source_error_are_published() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let mut connect = Box::pin(engine.connect("127.0.0.1:9".parse().unwrap()));
    let waker = futures_util::task::noop_waker();
    let mut cx = TaskContext::from_waker(&waker);
    assert!(connect.as_mut().poll(&mut cx).is_pending());

    engine.shared.commands.close_admission();
    driver.reactor.session.exhaust_deadline_sequence_for_test();
    driver.reactor.session.connections.schedule_deadline(
        super::super::session::DeadlineKind::ConnectionDrain,
        7,
        Duration::ZERO,
    );

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert!(matches!(
        connect.as_mut().poll(&mut cx),
        Poll::Ready(Err(Error::DriverShutdown))
    ));
}

#[test]
fn earliest_owner_deadline_handles_equal_and_missing_values() {
    let now = tokio::time::Instant::now();
    let later = now + Duration::from_secs(1);

    assert_eq!(earliest_deadline(Some(later), Some(now)), Some(now));
    assert_eq!(earliest_deadline(Some(now), Some(now)), Some(now));
    assert_eq!(earliest_deadline(Some(now), None), Some(now));
    assert_eq!(earliest_deadline(None, Some(later)), Some(later));
    assert_eq!(earliest_deadline(None, None), None);
}

#[test]
fn shutdown_and_failure_notify_the_reactor() {
    let (engine, driver) = test_engine_pair(CompletionMode::Polling);
    engine.shared.work_signal.take();

    engine.shared.request_shutdown();
    assert_eq!(
        engine.shared.work_signal.take(),
        REACTOR_WORK,
        "idle shutdown must notify the reactor"
    );

    engine
        .shared
        .begin_driver_failure(Error::InvalidConfig("publication test".into()));
    assert_eq!(
        engine.shared.work_signal.take(),
        REACTOR_WORK,
        "driver failure must notify the reactor"
    );
    drop(driver);
}

#[tokio::test]
async fn reactor_notifications_still_poll_both_owners_once() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    let initial_io = driver.reactor.io.turn_count();
    let initial_session = driver.reactor.session.turn_count();

    engine.shared.work_signal.notify_reactor();
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(driver.reactor.io.turn_count(), initial_io + 1);
    assert_eq!(driver.reactor.session.turn_count(), initial_session + 1);

    engine.shared.work_signal.notify_reactor();
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(driver.reactor.io.turn_count(), initial_io + 2);
    assert_eq!(driver.reactor.session.turn_count(), initial_session + 2);
    drop(driver);
}

#[tokio::test(start_paused = true)]
async fn integrated_all_source_contention_has_bounded_non_starvation() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let poster = Arc::new(DrainInterleavingPoster {
        qp_num: 211,
        destroys: AtomicUsize::new(0),
    });
    let connection = install_connection(
        &driver.reactor.session.manager,
        &mut driver.reactor.session.connections,
        Arc::clone(&poster) as Arc<dyn TestConnectionProvider>,
        RdmaConnectionConfig::default(),
        None,
        None,
    )
    .unwrap();
    let operation = install_accepted_operation_for_driver_test(
        driver.reactor.io.core_mut(),
        &mut driver.reactor.session.connections,
        connection.session_token(),
        crate::wc::WcOpcode::Send,
    );
    driver.reactor.io.core_mut().cancel_operation(operation);
    driver.reactor.io.schedule_deadline_for_test(
        tokio::time::Instant::now(),
        super::super::registry::OperationToken::decode(u64::MAX),
    );
    engine
        .shared
        .test_driver
        .queue_released_connection_cqe(completion_for_driver_test(
            operation,
            poster.qp_num,
            rdma_io_sys::ibverbs::IBV_WC_SEND,
            rdma_io_sys::ibverbs::IBV_WC_SUCCESS,
        ));

    let (listener, listener_token) = driver
        .reactor
        .session
        .cm
        .test_listener(&driver.reactor.session.manager, 2);
    driver
        .reactor
        .session
        .cm
        .enqueue_listener_work(listener_token);
    let route_destroys = Arc::new(AtomicUsize::new(0));
    driver
        .reactor
        .session
        .cm
        .defer_test_route_destruction(Arc::clone(&route_destroys));
    for token in [connection.session_token().encode(), u64::MAX - 1] {
        driver.reactor.session.connections.schedule_deadline(
            super::super::session::DeadlineKind::ConnectionDrain,
            token,
            Duration::ZERO,
        );
    }
    assert_eq!(
        driver.reactor.session.service_deadline_requests(1).unwrap(),
        1
    );
    engine.shared.request_shutdown();

    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);
    driver
        .reactor
        .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
        .unwrap();
    assert!(driver.reactor.last_action_count_for_test() <= 32);
    let first = driver.reactor.take_served_sources_for_test();
    for required in [
        "Commands",
        "Cq",
        "IoReclamation",
        "IoDeadline",
        "CmListenerWork",
        "CmEvent",
        "CmDestruction",
        "SessionDeadlineIngress",
        "SessionDeadline",
        "ShutdownListeners",
        "ShutdownConnections",
    ] {
        assert!(
            first.iter().any(|source| source == required),
            "{required} received no bounded opportunity under contention: {first:?}"
        );
        assert_eq!(
            first
                .iter()
                .filter(|source| source.as_str() == required)
                .count(),
            1,
            "{required} received more than one ready-at-entry quantum"
        );
    }
    assert!(
        !first.iter().any(|source| source == "CompletionDispatch"),
        "CQ-produced completion feedback must be deferred to the next turn"
    );

    driver
        .reactor
        .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
        .unwrap();
    assert!(driver.reactor.last_action_count_for_test() <= 32);
    let second = driver.reactor.take_served_sources_for_test();
    assert!(
        second.iter().any(|source| source == "CompletionDispatch"),
        "the next rotating turn must service deferred completion feedback"
    );

    engine
        .shared
        .begin_driver_failure(Error::InvalidConfig("terminal contention test".into()));
    driver
        .reactor
        .turn_for_test(&engine.shared, CompletionMode::Polling, &mut cx)
        .unwrap_or(true);
    assert!(driver.reactor.last_action_count_for_test() <= 32);
    let terminal = driver.reactor.take_served_sources_for_test();
    assert!(
        terminal.iter().any(|source| source == "IoTerminal"),
        "terminal service must retain an opportunity after ordinary-source contention"
    );
    assert!(
        first.len() <= 21 && second.len() <= 21 && terminal.len() <= 21,
        "one turn cannot exceed the finite ready-at-entry source set"
    );

    drop(listener);
    drop(connection);
}

#[test]
fn io_failure_cleanup_is_bounded_across_driver_polls() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let mut connections = Vec::new();
    for qp_num in 1..=100 {
        let poster = Arc::new(DrainInterleavingPoster {
            qp_num,
            destroys: AtomicUsize::new(0),
        });
        let connection = install_connection(
            &driver.reactor.session.manager,
            &mut driver.reactor.session.connections,
            poster as Arc<dyn TestConnectionProvider>,
            RdmaConnectionConfig::default(),
            None,
            None,
        )
        .unwrap();
        install_accepted_operation_for_driver_test(
            driver.reactor.io.core_mut(),
            &mut driver.reactor.session.connections,
            connection.session_token(),
            crate::wc::WcOpcode::Send,
        );
        connections.push(connection);
    }
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);

    assert!(
        driver
            .fail(
                Error::InvalidConfig("bounded I/O terminalization".into()),
                &mut cx,
            )
            .is_pending()
    );
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    let first = engine.diagnostics().quarantined_operations;
    assert!(first > 0 && first < 100);

    let mut result = Poll::Pending;
    for _ in 0..32 {
        result = Pin::new(&mut driver).poll(&mut cx);
        if result.is_ready() {
            break;
        }
    }
    assert!(matches!(result, Poll::Ready(Err(Error::InvalidConfig(_)))));
    assert_eq!(engine.diagnostics().quarantined_operations, 100);
    assert!(
        connections
            .iter()
            .all(|connection| connection.state.close_state().raw_outcome().is_some())
    );
    drop(connections);
}

#[test]
fn wake_before_register_is_seen_by_recheck() {
    let signal = WorkSignal::new();
    signal.notify_reactor();
    let observed = signal.epoch();
    let counter = CountingWaker::new();
    let pending = signal.register_and_recheck(&counter.waker(), observed - 1);
    assert_eq!(pending, REACTOR_WORK);
    assert_eq!(counter.count(), 1);
}

#[test]
fn wake_during_register_is_not_lost() {
    let signal = Arc::new(WorkSignal::new());
    let observed = signal.epoch();
    std::thread::scope(|scope| {
        let signal = Arc::clone(&signal);
        scope.spawn(move || signal.notify_reactor()).join().unwrap();
    });
    let counter = CountingWaker::new();
    let pending = signal.register_and_recheck(&counter.waker(), observed);
    assert_eq!(pending, REACTOR_WORK);
    assert_eq!(counter.count(), 1);
}

#[test]
fn enqueue_after_drain_is_seen_by_register_recheck() {
    let signal = WorkSignal::new();
    assert_eq!(signal.take(), 0);
    let observed = signal.epoch();
    signal.notify_reactor();
    let counter = CountingWaker::new();
    assert_eq!(
        signal.register_and_recheck(&counter.waker(), observed),
        REACTOR_WORK
    );
    assert_eq!(counter.count(), 1);
}

#[test]
fn concurrent_producers_coalesce_into_one_reactor_notification() {
    let signal = Arc::new(WorkSignal::new());
    std::thread::scope(|scope| {
        let mut producers = Vec::new();
        for _ in 0..2 {
            let signal = Arc::clone(&signal);
            producers.push(scope.spawn(move || {
                for _ in 0..32 {
                    signal.notify_reactor();
                }
            }));
        }
        for producer in producers {
            producer.join().unwrap();
        }
    });
    assert_eq!(signal.take(), REACTOR_WORK);
}

struct DrainInterleavingPoster {
    qp_num: u32,
    destroys: AtomicUsize,
}

impl TestConnectionProvider for DrainInterleavingPoster {
    fn qp_num(&self) -> u32 {
        self.qp_num
    }

    fn capabilities(&self) -> Option<QpCapabilities> {
        None
    }

    fn post_send(&self, _: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
        unreachable!("interleaving test installs an accepted operation directly")
    }

    fn post_recv(&self, _: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
        unreachable!("interleaving test installs an accepted operation directly")
    }

    fn to_error(&self) -> Result<()> {
        Ok(())
    }

    fn destroy_qp(&self) -> Result<bool> {
        self.destroys.fetch_add(1, Ordering::AcqRel);
        Ok(true)
    }

    fn disconnect(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::test(start_paused = true)]
async fn cq_reclamation_ready_interleaving_dispatches_queued_success_and_flush_exactly() {
    for mode in [CompletionMode::Readiness, CompletionMode::Polling] {
        for (opcode, status) in [
            (
                rdma_io_sys::ibverbs::IBV_WC_SEND,
                rdma_io_sys::ibverbs::IBV_WC_SUCCESS,
            ),
            (
                rdma_io_sys::ibverbs::IBV_WC_RECV,
                rdma_io_sys::ibverbs::IBV_WC_WR_FLUSH_ERR,
            ),
        ] {
            let (engine, mut driver) = test_engine_pair(mode);
            let poster = Arc::new(DrainInterleavingPoster {
                qp_num: 71,
                destroys: AtomicUsize::new(0),
            });
            let connection = install_connection(
                &driver.reactor.session.manager,
                &mut driver.reactor.session.connections,
                Arc::clone(&poster) as Arc<dyn TestConnectionProvider>,
                RdmaConnectionConfig::default(),
                None,
                None,
            )
            .unwrap();
            let expected = if opcode == rdma_io_sys::ibverbs::IBV_WC_RECV {
                crate::wc::WcOpcode::Recv
            } else {
                crate::wc::WcOpcode::Send
            };
            let operation = install_accepted_operation_for_driver_test(
                driver.reactor.io.core_mut(),
                &mut driver.reactor.session.connections,
                connection.session_token(),
                expected,
            );
            let connection_token = connection.session_token();
            driver
                .reactor
                .session
                .connections
                .begin_close(connection_token);
            driver
                .reactor
                .session
                .manager
                .transition_connection_to_error(
                    &mut driver.reactor.session.connections,
                    connection_token,
                )
                .unwrap();
            engine
                .shared
                .test_driver
                .queue_released_connection_cqe(completion_for_driver_test(
                    operation,
                    poster.qp_num,
                    opcode,
                    status,
                ));
            driver.reactor.session.connections.schedule_deadline(
                super::super::session::DeadlineKind::ConnectionDrain,
                connection_token.encode(),
                Duration::ZERO,
            );
            let waker = Waker::noop();
            let mut cx = TaskContext::from_waker(waker);

            for _ in 0..4 {
                assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
                if engine.diagnostics().accepted_operations == 0
                    && poster.destroys.load(Ordering::Acquire) == 1
                {
                    break;
                }
            }

            let diagnostics = engine.diagnostics();
            assert_eq!(diagnostics.accepted_operations, 0);
            assert_eq!(diagnostics.registered_operations, 0);
            assert_eq!(
                poster.destroys.load(Ordering::Acquire),
                1,
                "exact completion permits normal session-owned retirement without fallback"
            );

            driver
                .reactor
                .finish_for_test(&engine.shared, MemoizedTerminalResult::success());
            drop(driver);
        }
    }
}

#[test]
fn driver_poll_outside_tokio_returns_contextual_error_without_panicking() {
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    let mut terminal = None;
    for _ in 0..4 {
        match Pin::new(&mut driver).poll(&mut cx) {
            Poll::Ready(result) => {
                terminal = Some(result);
                break;
            }
            Poll::Pending => {}
        }
    }
    assert!(matches!(terminal, Some(Err(Error::InvalidConfig(_)))));
    assert!(
        counter.count() > 0,
        "bounded terminal cleanup must schedule its next turn"
    );
}

#[cfg(panic = "unwind")]
#[test]
fn driver_poll_without_tokio_time_returns_contextual_error_without_panicking() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let result =
        runtime.block_on(async { std::future::poll_fn(|cx| Pin::new(&mut driver).poll(cx)).await });
    assert!(matches!(result, Err(Error::InvalidConfig(_))));
}

#[cfg(not(panic = "unwind"))]
#[test]
fn abort_build_polling_driver_progresses_without_a_time_probe() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .build()
        .unwrap();
    let _entered = runtime.enter();
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(counter.count(), 1);
}

#[tokio::test(start_paused = true)]
async fn deadline_timer_wakes_driver_and_processes_due_work() {
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    driver.reactor.io.schedule_deadline_for_test(
        tokio::time::Instant::now() + Duration::from_secs(5),
        super::super::registry::OperationToken::decode(7),
    );
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(counter.count(), 0, "an unexpired deadline stays idle");

    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(counter.count() > 0, "the Tokio timer must wake the driver");
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
}

#[tokio::test(start_paused = true)]
async fn deadline_timer_rearms_for_newly_earlier_owner_deadline() {
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    let now = tokio::time::Instant::now();
    let later = now + Duration::from_secs(10);
    let earlier = now + Duration::from_secs(5);
    driver
        .reactor
        .io
        .schedule_deadline_for_test(later, super::super::registry::OperationToken::decode(1));
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(driver.deadline_at, Some(later));

    driver
        .reactor
        .io
        .schedule_deadline_for_test(earlier, super::super::registry::OperationToken::decode(2));
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(driver.deadline_at, Some(earlier));
}

#[tokio::test(start_paused = true)]
async fn deadline_timer_clears_removed_owner_deadline() {
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    driver
        .reactor
        .io
        .schedule_deadline_for_test(deadline, super::super::registry::OperationToken::decode(1));
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(driver.deadline_at, Some(deadline));

    driver.reactor.io.clear_deadlines_for_test();
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(driver.deadline_at, None);
}

#[tokio::test(start_paused = true)]
async fn deadline_timer_processes_already_expired_owner_deadline() {
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    driver.reactor.io.schedule_deadline_for_test(
        tokio::time::Instant::now(),
        super::super::registry::OperationToken::decode(1),
    );
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());

    assert_eq!(driver.reactor.io.next_deadline(), None);
    assert_eq!(driver.deadline_at, None);
}

#[tokio::test]
async fn readiness_idle_poll_does_not_self_wake_or_scan() {
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(counter.count(), 0);
    assert_eq!(driver.reactor.io.completion_connection_count(), 0);
}

#[tokio::test]
async fn idle_connections_publish_no_completion_dispatch_work() {
    for count in [1, 1_024] {
        let mut config = super::super::config::EngineConfig::new("test0".into());
        config.completion_mode = CompletionMode::Readiness;
        config.max_live_connections = count;
        let (shared, session) = super::super::EngineFrontendRoot::new(
            config,
            None,
            super::super::io::MemoryRegistrar::from_pd(None),
        )
        .unwrap();
        let shared = shared.into_shared();
        let mut driver = super::super::RdmaEngineDriver::new(Arc::clone(&shared), session, None);
        let connections = shared
            .test_driver
            .install_idle_connections(&mut driver.reactor.session, count)
            .unwrap();
        let waker = futures_util::task::noop_waker();
        let mut cx = TaskContext::from_waker(&waker);

        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
        assert_eq!(driver.reactor.io.completion_connection_count(), 0);
        assert!(!driver.reactor.io.core().has_published_connections());

        drop(connections);
        drop(driver);
    }
}

#[tokio::test]
async fn polling_empty_iteration_cooperatively_yields_once() {
    let (_engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    {
        let mut cx = TaskContext::from_waker(&waker);
        assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    }
    tokio::task::yield_now().await;
    assert_eq!(counter.count(), 1);
}

#[tokio::test]
async fn terminal_request_wakes_driver_and_state_is_monotonic() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    engine.shared.request_shutdown();
    assert_eq!(counter.count(), 1);
    assert!(matches!(
        Pin::new(&mut driver).poll(&mut cx),
        Poll::Ready(Ok(()))
    ));
    driver.reactor.transition_running(&engine.shared);
    assert_eq!(
        engine.diagnostics().lifecycle,
        RdmaEngineLifecycle::Terminated
    );
}

#[tokio::test(start_paused = true)]
async fn final_accepted_operation_drain_wakes_and_reconsiders_terminal() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    let poster = Arc::new(DrainInterleavingPoster {
        qp_num: 73,
        destroys: AtomicUsize::new(0),
    });
    let connection = install_connection(
        &driver.reactor.session.manager,
        &mut driver.reactor.session.connections,
        Arc::clone(&poster) as Arc<dyn TestConnectionProvider>,
        RdmaConnectionConfig::default(),
        None,
        None,
    )
    .unwrap();
    let operation = install_accepted_operation_for_driver_test(
        driver.reactor.io.core_mut(),
        &mut driver.reactor.session.connections,
        connection.session_token(),
        crate::wc::WcOpcode::Send,
    );
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    engine.shared.request_shutdown();
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(engine.diagnostics().accepted_operations, 1);

    let wakes_before_drain = counter.count();
    engine
        .shared
        .test_driver
        .queue_released_connection_cqe(completion_for_driver_test(
            operation,
            poster.qp_num,
            rdma_io_sys::ibverbs::IBV_WC_SEND,
            rdma_io_sys::ibverbs::IBV_WC_SUCCESS,
        ));

    let mut result = Pin::new(&mut driver).poll(&mut cx);
    assert!(
        counter.count() > wakes_before_drain,
        "the final accepted-operation drain must wake the registered driver"
    );
    for _ in 0..8 {
        if result.is_ready() || engine.diagnostics().accepted_operations == 0 {
            break;
        }
        result = Pin::new(&mut driver).poll(&mut cx);
    }
    assert_eq!(engine.diagnostics().accepted_operations, 0);
    for _ in 0..8 {
        if result.is_ready() {
            break;
        }
        result = Pin::new(&mut driver).poll(&mut cx);
    }

    assert!(matches!(result, Poll::Ready(Ok(()))));
    assert_eq!(engine.diagnostics().live_connections, 0);
    assert_eq!(
        poster.destroys.load(Ordering::Acquire),
        1,
        "session retirement must retain QP destruction evidence"
    );
}

#[tokio::test]
async fn final_session_cleanup_is_composed_after_the_owner_pass() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let connections = engine
        .shared
        .test_driver
        .install_idle_connections(&mut driver.reactor.session, 1)
        .unwrap();
    engine.shared.request_shutdown();
    let waker = Waker::noop();
    let mut cx = TaskContext::from_waker(waker);

    let mut result = Poll::Pending;
    for _ in 0..16 {
        result = Pin::new(&mut driver).poll(&mut cx);
        if result.is_ready() {
            break;
        }
    }

    assert!(matches!(result, Poll::Ready(Ok(()))));
    assert_eq!(engine.diagnostics().live_connections, 0);
    drop(connections);
}

fn pending_destruction_listener(
    _engine: &super::super::RdmaEngine,
    driver: &mut super::super::RdmaEngineDriver,
) -> (RdmaListener, Arc<AtomicUsize>) {
    let (listener, token) = driver
        .reactor
        .session
        .cm
        .test_listener(&driver.reactor.session.manager, 1);
    let destroy_count = Arc::new(AtomicUsize::new(0));
    driver
        .reactor
        .session
        .cm
        .defer_test_listener_destruction(token, Arc::clone(&destroy_count));
    (listener, destroy_count)
}

fn assert_terminal_close(
    close: &mut Pin<Box<impl Future<Output = Result<()>>>>,
    cx: &mut TaskContext<'_>,
    expected: &RdmaEngineTerminalError,
) {
    let Poll::Ready(Err(error)) = close.as_mut().poll(cx) else {
        panic!("pending listener close was not terminalized");
    };
    assert_eq!(error.to_string(), expected.message);
}

#[test]
fn driver_drop_wakes_listener_close_pending_cm_destruction() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let (listener, destroy_count) = pending_destruction_listener(&engine, &mut driver);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    let mut close = Box::pin(listener.close());

    assert!(close.as_mut().poll(&mut cx).is_pending());
    drop(driver);

    let terminal = engine
        .diagnostics()
        .terminal_error
        .expect("driver drop must publish a terminal error");
    assert_eq!(terminal.class, "EngineWedged");
    assert_terminal_close(&mut close, &mut cx, &terminal);
    assert_eq!(counter.count(), 1);
    assert_eq!(destroy_count.load(Ordering::Acquire), 0);
    assert_eq!(
        lock_unpoison(&engine.shared.diagnostics).cm_retained_owners,
        1
    );
    assert!(super::super::reactor::failed_reactor_contains(
        &engine.shared
    ));
}

#[test]
fn driver_error_wakes_listener_close_once_and_preserves_pending_destruction() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let (listener, destroy_count) = pending_destruction_listener(&engine, &mut driver);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    let driver_waker = Waker::noop();
    let mut driver_cx = TaskContext::from_waker(driver_waker);
    let mut close = Box::pin(listener.close());

    assert!(close.as_mut().poll(&mut cx).is_pending());
    assert!(
        driver
            .fail(
                Error::InvalidConfig("injected driver progress failure".into()),
                &mut driver_cx,
            )
            .is_pending()
    );
    let mut driver_error = None;
    for _ in 0..64 {
        match Pin::new(&mut driver).poll(&mut driver_cx) {
            Poll::Ready(Err(error)) => {
                driver_error = Some(error);
                break;
            }
            Poll::Ready(Ok(())) => panic!("injected failure completed successfully"),
            Poll::Pending => {}
        }
    }
    let driver_error = driver_error.unwrap_or_else(|| {
        panic!(
            "driver failure did not make bounded terminal progress: lifecycle={:?}, commands={}, session_finish={}, listener_work={}, cm_owners={}",
            driver.reactor.lifecycle.lifecycle(),
            engine.shared.commands.has_pending(),
            driver.reactor.session.can_finish(),
            driver.reactor.session.cm.listener_work_count(),
            driver
                .reactor
                .session
                .cm
                .retained_owner_count(&driver.reactor.session.connections),
        )
    });
    match Pin::new(&mut driver).poll(&mut driver_cx) {
        Poll::Ready(Err(repolled)) => {
            assert_eq!(repolled.to_string(), driver_error.to_string());
        }
        Poll::Ready(Ok(())) => panic!("re-polled failed driver completed successfully"),
        Poll::Pending => panic!("re-polled terminal driver lost its memoized result"),
    }
    let terminal = engine
        .diagnostics()
        .terminal_error
        .expect("driver error must publish a terminal error");
    assert_eq!(driver_error.to_string(), terminal.message);
    assert_terminal_close(&mut close, &mut cx, &terminal);
    assert_eq!(counter.count(), 1);
    assert_eq!(destroy_count.load(Ordering::Acquire), 0);
    assert_eq!(
        driver
            .reactor
            .session
            .cm
            .retained_owner_count(&driver.reactor.session.connections),
        1
    );
    assert!(
        driver.reactor.requires_complete_quarantine(),
        "a Ready failure must retain the complete reactor resource root until Drop"
    );

    drop(driver);
    assert!(super::super::reactor::failed_reactor_contains(
        &engine.shared
    ));
    assert_eq!(counter.count(), 1, "driver drop must not finish twice");
    assert_eq!(
        engine
            .diagnostics()
            .terminal_error
            .expect("terminal error remains available"),
        terminal
    );
}
