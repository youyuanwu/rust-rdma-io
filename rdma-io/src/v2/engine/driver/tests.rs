#![cfg(test)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};
use std::time::Duration;

use super::*;
use crate::v2::engine::io_core::{
    completion_for_driver_test, install_accepted_operation_for_driver_test,
};
use crate::v2::engine::session::connection::{WorkRequestPoster, install_connection};
use crate::v2::engine::session::listener::ListenerState;
use crate::v2::engine::{
    RdmaConnectionConfig, RdmaEngineLifecycle, RdmaEngineTerminalError, RdmaListener,
    test_engine_pair,
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
fn shutdown_and_failure_publish_both_cleanup_owners() {
    let (engine, driver) = test_engine_pair(CompletionMode::Polling);
    engine.shared.work_signal.take();

    engine.shared.request_shutdown();
    assert_eq!(
        engine.shared.work_signal.take(),
        IO_WORK | SESSION_WORK,
        "idle shutdown must explicitly schedule both cleanup owners"
    );

    engine
        .shared
        .begin_driver_failure(Error::InvalidConfig("publication test".into()));
    assert_eq!(
        engine.shared.work_signal.take(),
        IO_WORK | SESSION_WORK,
        "driver failure must explicitly reschedule both bounded cleanup owners"
    );
    drop(driver);
}

#[tokio::test]
async fn software_wakes_coalesced_with_either_owner_still_poll_both_once() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);

    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    let initial_io = driver.reactor.io.turn_count();
    let initial_session = driver.reactor.session.turn_count();

    engine.shared.work_signal.publish(IO_WORK);
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(driver.reactor.io.turn_count(), initial_io + 1);
    assert_eq!(driver.reactor.session.turn_count(), initial_session + 1);

    engine.shared.work_signal.publish(SESSION_WORK);
    assert!(Pin::new(&mut driver).poll(&mut cx).is_pending());
    assert_eq!(driver.reactor.io.turn_count(), initial_io + 2);
    assert_eq!(driver.reactor.session.turn_count(), initial_session + 2);
    drop(driver);
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
            &engine.shared.session,
            &mut driver.reactor.session.connections,
            poster as Arc<dyn WorkRequestPoster>,
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
    signal.publish(SESSION_WORK);
    let observed = signal.epoch();
    let counter = CountingWaker::new();
    let pending = signal.register_and_recheck(&counter.waker(), observed - 1);
    assert_eq!(pending, SESSION_WORK);
    assert_eq!(counter.count(), 1);
}

#[test]
fn wake_during_register_is_not_lost() {
    let signal = Arc::new(WorkSignal::new());
    let observed = signal.epoch();
    std::thread::scope(|scope| {
        let signal = Arc::clone(&signal);
        scope.spawn(move || signal.publish(IO_WORK)).join().unwrap();
    });
    let counter = CountingWaker::new();
    let pending = signal.register_and_recheck(&counter.waker(), observed);
    assert_eq!(pending, IO_WORK);
    assert_eq!(counter.count(), 1);
}

#[test]
fn enqueue_after_drain_is_seen_by_register_recheck() {
    let signal = WorkSignal::new();
    assert_eq!(signal.take(), 0);
    let observed = signal.epoch();
    signal.publish(IO_WORK);
    let counter = CountingWaker::new();
    assert_eq!(
        signal.register_and_recheck(&counter.waker(), observed),
        IO_WORK
    );
    assert_eq!(counter.count(), 1);
}

#[test]
fn concurrent_producers_coalesce_without_losing_work_classes() {
    let signal = Arc::new(WorkSignal::new());
    std::thread::scope(|scope| {
        let mut producers = Vec::new();
        for bit in [IO_WORK, SESSION_WORK] {
            let signal = Arc::clone(&signal);
            producers.push(scope.spawn(move || {
                for _ in 0..32 {
                    signal.publish(bit);
                }
            }));
        }
        for producer in producers {
            producer.join().unwrap();
        }
    });
    assert_eq!(signal.take(), IO_WORK | SESSION_WORK);
}

struct DrainInterleavingPoster {
    qp_num: u32,
    destroys: AtomicUsize,
}

impl WorkRequestPoster for DrainInterleavingPoster {
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

    fn to_error(
        &self,
        _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
    ) -> Result<()> {
        Ok(())
    }

    fn destroy_qp(
        &self,
        _authority: &crate::v2::engine::session::SessionLifecycleAuthority,
    ) -> Result<bool> {
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
                &engine.shared.session,
                &mut driver.reactor.session.connections,
                Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
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
            engine
                .shared
                .session
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

            engine.shared.finish(
                &mut driver.reactor.session,
                driver.reactor.io.core_mut(),
                MemoizedTerminalResult::success(),
            );
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
        let shared = EngineShared::new(config, None, None).unwrap().into_shared();
        let mut driver = super::super::RdmaEngineDriver::new(Arc::clone(&shared), None);
        let connections = shared
            .test_driver
            .install_idle_connections(&shared, &mut driver.reactor.session.connections, count)
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
    engine.shared.transition_running();
    assert_eq!(engine.shared.lifecycle(), RdmaEngineLifecycle::Terminated);
}

#[tokio::test(start_paused = true)]
async fn final_accepted_operation_drain_wakes_and_reconsiders_terminal() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Readiness);
    let poster = Arc::new(DrainInterleavingPoster {
        qp_num: 73,
        destroys: AtomicUsize::new(0),
    });
    let connection = install_connection(
        &engine.shared.session,
        &mut driver.reactor.session.connections,
        Arc::clone(&poster) as Arc<dyn WorkRequestPoster>,
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
        "session retirement must retain QP destruction authority"
    );
}

#[tokio::test]
async fn final_session_cleanup_is_composed_after_the_owner_pass() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let connections = engine
        .shared
        .test_driver
        .install_idle_connections(&engine.shared, &mut driver.reactor.session.connections, 1)
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
    engine: &super::super::RdmaEngine,
) -> (RdmaListener, Arc<AtomicUsize>) {
    let state = ListenerState::test_only(1);
    let destroy_count = Arc::new(AtomicUsize::new(0));
    engine
        .shared
        .session
        .cm
        .defer_test_listener_destruction(Arc::clone(&state), Arc::clone(&destroy_count));
    (
        RdmaListener::from_state(&engine.shared.session, state),
        destroy_count,
    )
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
    let (engine, driver) = test_engine_pair(CompletionMode::Polling);
    let (listener, destroy_count) = pending_destruction_listener(&engine);
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
    assert_eq!(engine.shared.session.cm.retained_adapter_owner_count(), 1);
}

#[test]
fn driver_error_wakes_listener_close_once_and_preserves_pending_destruction() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let (listener, destroy_count) = pending_destruction_listener(&engine);
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
    let driver_error = loop {
        match Pin::new(&mut driver).poll(&mut driver_cx) {
            Poll::Ready(Err(error)) => break error,
            Poll::Ready(Ok(())) => panic!("injected failure completed successfully"),
            Poll::Pending => {}
        }
    };
    let terminal = engine
        .diagnostics()
        .terminal_error
        .expect("driver error must publish a terminal error");
    assert_eq!(driver_error.to_string(), terminal.message);
    assert_terminal_close(&mut close, &mut cx, &terminal);
    assert_eq!(counter.count(), 1);
    assert_eq!(destroy_count.load(Ordering::Acquire), 0);
    assert_eq!(
        engine
            .shared
            .session
            .cm
            .retained_owner_count(&driver.reactor.session.connections),
        1
    );

    drop(driver);
    assert_eq!(counter.count(), 1, "driver drop must not finish twice");
    assert_eq!(
        engine
            .diagnostics()
            .terminal_error
            .expect("terminal error remains available"),
        terminal
    );
}
