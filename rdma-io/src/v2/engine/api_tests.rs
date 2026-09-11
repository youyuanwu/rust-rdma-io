use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll, RawWaker, RawWakerVTable, Waker};

use super::*;
use crate::v2::engine::session::connection::{TestConnectionProvider, install_connection};
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

#[test]
fn diagnostics_reads_one_coherent_published_snapshot() {
    let (engine, driver) = test_engine_pair(CompletionMode::Polling);
    let shared = Arc::clone(&engine.shared);
    let running = diagnostics::PublishedDiagnostics {
        engine: RdmaEngineDiagnostics {
            lifecycle: RdmaEngineLifecycle::Running,
            terminal_error: None,
            live_connections: 1,
            registered_operations: 1,
            accepted_operations: 1,
            pending_reclamations: 1,
            available_cq_credits: 1,
            retained_cq_credits: 1,
            quarantined_operations: 1,
            quarantined_mrs: 1,
            quarantined_bytes: 1,
            quarantined_connections: 1,
        },
        cm_pending_routes: 1,
        cm_retained_owners: 1,
    };
    let failed = diagnostics::PublishedDiagnostics {
        engine: RdmaEngineDiagnostics {
            lifecycle: RdmaEngineLifecycle::Failed,
            terminal_error: Some(RdmaEngineTerminalError {
                class: "snapshot".into(),
                message: "snapshot".into(),
            }),
            live_connections: 2,
            registered_operations: 2,
            accepted_operations: 2,
            pending_reclamations: 2,
            available_cq_credits: 2,
            retained_cq_credits: 2,
            quarantined_operations: 2,
            quarantined_mrs: 2,
            quarantined_bytes: 2,
            quarantined_connections: 2,
        },
        cm_pending_routes: 2,
        cm_retained_owners: 2,
    };
    shared.publish_diagnostics(running.clone());

    let writer = std::thread::spawn(move || {
        for index in 0..10_000 {
            shared.publish_diagnostics(if index % 2 == 0 {
                running.clone()
            } else {
                failed.clone()
            });
        }
    });
    for _ in 0..10_000 {
        let snapshot = engine.diagnostics();
        let marker = snapshot.live_connections;
        assert!(marker == 1 || marker == 2);
        assert_eq!(snapshot.registered_operations, marker);
        assert_eq!(snapshot.accepted_operations, marker);
        assert_eq!(snapshot.pending_reclamations, marker);
        assert_eq!(snapshot.available_cq_credits, marker);
        assert_eq!(snapshot.retained_cq_credits, marker);
        assert_eq!(snapshot.quarantined_operations, marker);
        assert_eq!(snapshot.quarantined_mrs, marker);
        assert_eq!(snapshot.quarantined_bytes, marker);
        assert_eq!(snapshot.quarantined_connections, marker);
        assert_eq!(
            snapshot.lifecycle,
            if marker == 1 {
                RdmaEngineLifecycle::Running
            } else {
                RdmaEngineLifecycle::Failed
            }
        );
        assert_eq!(snapshot.terminal_error.is_some(), marker == 2);
    }
    writer.join().unwrap();
    drop(driver);
}

#[tokio::test]
async fn frontend_reservations_do_not_partially_mutate_the_published_snapshot() {
    let (engine, driver) = test_engine_pair_with_capacity(CompletionMode::Polling, 1);
    let shared = Arc::clone(&engine.shared);
    let marker = diagnostics::PublishedDiagnostics {
        engine: RdmaEngineDiagnostics {
            lifecycle: RdmaEngineLifecycle::Running,
            terminal_error: None,
            live_connections: 0,
            registered_operations: 7,
            accepted_operations: 7,
            pending_reclamations: 7,
            available_cq_credits: 7,
            retained_cq_credits: 7,
            quarantined_operations: 7,
            quarantined_mrs: 7,
            quarantined_bytes: 7,
            quarantined_connections: 7,
        },
        cm_pending_routes: 7,
        cm_retained_owners: 7,
    };
    shared.publish_diagnostics(marker.clone());

    let lane = shared.commands.acquire_connect().await.unwrap();
    let reservation = shared.commands.reserve_connect(lane).unwrap();
    driver.reactor.publish_current_diagnostics(&shared);
    assert_eq!(
        lock_unpoison(&shared.diagnostics).engine.live_connections,
        0,
        "reactor publication must exclude a frontend-only reservation"
    );
    assert_eq!(engine.diagnostics().live_connections, 1);
    drop(reservation);
    assert_eq!(
        engine.diagnostics().live_connections,
        0,
        "releasing a frontend reservation must not require another reactor turn"
    );
    shared.publish_diagnostics(marker.clone());

    let commands = Arc::clone(&shared.commands);
    let writer = tokio::spawn(async move {
        for _ in 0..1_000 {
            let lane = commands.acquire_connect().await.unwrap();
            let reservation = commands.reserve_connect(lane).unwrap();
            tokio::task::yield_now().await;
            drop(reservation);
        }
    });

    while !writer.is_finished() {
        let snapshot = engine.diagnostics();
        assert!(snapshot.live_connections <= 1);
        assert_eq!(snapshot.registered_operations, 7);
        assert_eq!(snapshot.accepted_operations, 7);
        assert_eq!(snapshot.pending_reclamations, 7);
        assert_eq!(snapshot.available_cq_credits, 7);
        assert_eq!(snapshot.retained_cq_credits, 7);
        assert_eq!(snapshot.quarantined_operations, 7);
        assert_eq!(snapshot.quarantined_mrs, 7);
        assert_eq!(snapshot.quarantined_bytes, 7);
        assert_eq!(snapshot.quarantined_connections, 7);
        tokio::task::yield_now().await;
    }
    writer.await.unwrap();

    assert_eq!(*lock_unpoison(&shared.diagnostics), marker);
    assert_eq!(engine.diagnostics().live_connections, 0);
    drop(driver);
}

#[test]
fn engine_failure_preserves_explicit_cq_debt() {
    let failure = lifecycle::MemoizedTerminalResult::from_error(Error::EngineWedged {
        retained_bundles: 2,
        outstanding_operations: 3,
        cq_debt: 5,
    });
    assert!(matches!(
        failure.into_result(),
        Err(Error::EngineWedged {
            retained_bundles: 2,
            outstanding_operations: 3,
            cq_debt: 5,
        })
    ));
}

#[test]
fn readiness_build_outside_tokio_is_contextual() {
    let error = RdmaEngineBuilder::new("unreachable-device")
        .build()
        .err()
        .expect("readiness build outside Tokio must fail");
    assert!(matches!(error, Error::InvalidConfig(_)));
    assert!(error.to_string().contains("Tokio"));
}

#[test]
fn reclamation_budgets_are_owner_local() {
    let builder = RdmaEngineBuilder::new("rxe0")
        .io_reclamation_budget(7)
        .session_reclamation_budget(9);

    assert_eq!(builder.config.io_reclamation_budget, 7);
    assert_eq!(builder.config.session_reclamation_budget, 9);
}

#[tokio::test]
async fn driver_is_directly_spawnable_and_shutdown_is_idempotent() {
    let (engine, driver) = test_engine_pair(CompletionMode::Readiness);
    let (driver_result, shutdown_result) = tokio::join!(driver, engine.shutdown());
    driver_result.unwrap();
    shutdown_result.unwrap();
    engine.shutdown().await.unwrap();

    let diagnostics = engine.diagnostics();
    assert_eq!(diagnostics.lifecycle, RdmaEngineLifecycle::Terminated);
    assert!(diagnostics.terminal_error.is_none());
}

#[test]
fn pending_shutdown_waiter_is_woken_when_driver_drops() {
    let (engine, driver) = test_engine_pair(CompletionMode::Readiness);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    let mut shutdown = Box::pin(engine.shutdown());

    assert!(Pin::new(&mut shutdown).poll(&mut cx).is_pending());
    drop(driver);
    assert!(matches!(
        Pin::new(&mut shutdown).poll(&mut cx),
        Poll::Ready(Err(Error::DriverShutdown))
    ));
    assert_eq!(counter.count(), 1);
    assert_eq!(
        engine
            .diagnostics()
            .terminal_error
            .expect("terminal summary")
            .class,
        "DriverShutdown"
    );
}

#[test]
fn only_the_last_engine_frontend_requests_shutdown() {
    let (engine, driver) = test_engine_pair(CompletionMode::Polling);
    let clone = engine.clone();
    drop(engine);
    assert!(!clone.shared.commands.shutdown_requested());
    drop(clone);
    assert!(driver.shared.commands.shutdown_requested());
}

#[test]
fn pending_connect_command_owns_and_releases_its_transferable_reservation() {
    let (engine, driver) = test_engine_pair(CompletionMode::Readiness);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    let mut connect = Box::pin(engine.connect("127.0.0.1:9".parse().unwrap()));

    assert!(Pin::new(&mut connect).poll(&mut cx).is_pending());
    assert_eq!(engine.shared.commands.pending_connects(), 1);
    assert_eq!(engine.diagnostics().live_connections, 1);
    drop(driver);
    assert!(matches!(
        Pin::new(&mut connect).poll(&mut cx),
        Poll::Ready(Err(Error::DriverShutdown))
    ));
    assert_eq!(engine.shared.commands.pending_connects(), 0);
    assert_eq!(engine.diagnostics().live_connections, 0);
    assert!(counter.count() >= 1);
}

#[tokio::test]
async fn shutdown_accounts_for_ingress_and_backend_connect_listen_commands() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let waker = futures_util::task::noop_waker();
    let mut cx = TaskContext::from_waker(&waker);
    let mut connect_backend = Box::pin(engine.connect("127.0.0.1:9".parse().unwrap()));
    let mut connect_ingress = Box::pin(engine.connect("127.0.0.1:10".parse().unwrap()));
    let mut listen_backend = Box::pin(engine.listen(
        "127.0.0.1:0".parse().unwrap(),
        RdmaListenerConfig::default(),
    ));
    let mut listen_ingress = Box::pin(engine.listen(
        "127.0.0.2:0".parse().unwrap(),
        RdmaListenerConfig::default(),
    ));
    for future in [&mut connect_backend, &mut connect_ingress] {
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    for future in [&mut listen_backend, &mut listen_ingress] {
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    engine.shared.commands.service_turn(
        &engine.shared,
        driver.reactor.io.core_mut(),
        &mut driver.reactor.session,
    );
    engine.shared.commands.service_turn(
        &engine.shared,
        driver.reactor.io.core_mut(),
        &mut driver.reactor.session,
    );
    assert_eq!(engine.shared.commands.pending_connects(), 1);
    assert_eq!(engine.shared.commands.pending_listens(), 1);

    let mut shutdown = Box::pin(engine.shutdown());
    assert!(shutdown.as_mut().poll(&mut cx).is_pending());
    engine.shared.commands.service_turn(
        &engine.shared,
        driver.reactor.io.core_mut(),
        &mut driver.reactor.session,
    );
    engine.shared.commands.service_turn(
        &engine.shared,
        driver.reactor.io.core_mut(),
        &mut driver.reactor.session,
    );
    assert_eq!(engine.shared.commands.pending_connects(), 0);
    assert_eq!(engine.shared.commands.pending_listens(), 0);
    // The unit fixture intentionally has no provider resources. Terminalize
    // the two commands already transferred to the authoritative CM backend
    // before polling the full driver; provider-backed tests exercise the same
    // path through normal bounded shutdown progress.
    driver.reactor.session.cm.begin_shutdown(
        &mut driver.reactor.session.connections,
        &driver.reactor.session.manager,
        &MemoizedTerminalResult::from_error(Error::DriverShutdown),
    );

    let mut driver_task = tokio::spawn(driver);
    let (
        connect_backend_result,
        connect_ingress_result,
        listen_backend_result,
        listen_ingress_result,
        shutdown_result,
        driver_result,
    ) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        tokio::join!(
            connect_backend.as_mut(),
            connect_ingress.as_mut(),
            listen_backend.as_mut(),
            listen_ingress.as_mut(),
            shutdown.as_mut(),
            &mut driver_task,
        )
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "shutdown command accounting matrix did not terminate: diagnostics={:?}, pending_routes={}, pending_connects={}, pending_listens={}, driver_finished={}",
            engine.diagnostics(),
            {
                let diagnostics = lock_unpoison(&engine.shared.diagnostics);
                diagnostics.engine.live_connections + diagnostics.cm_pending_routes
            },
            engine.shared.commands.pending_connects(),
            engine.shared.commands.pending_listens(),
            driver_task.is_finished(),
        )
    });
    for result in [connect_backend_result, connect_ingress_result] {
        assert!(matches!(result, Err(Error::DriverShutdown)));
    }
    for result in [listen_backend_result, listen_ingress_result] {
        assert!(matches!(result, Err(Error::DriverShutdown)));
    }
    shutdown_result.unwrap();
    driver_result.unwrap().unwrap();
    assert_eq!(engine.diagnostics().live_connections, 0);
}

#[test]
fn driver_drop_accounts_for_ingress_and_backend_connect_listen_commands() {
    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    let mut connect_backend = Box::pin(engine.connect("127.0.0.1:9".parse().unwrap()));
    let mut connect_ingress = Box::pin(engine.connect("127.0.0.1:10".parse().unwrap()));
    let mut listen_backend = Box::pin(engine.listen(
        "127.0.0.1:0".parse().unwrap(),
        RdmaListenerConfig::default(),
    ));
    let mut listen_ingress = Box::pin(engine.listen(
        "127.0.0.2:0".parse().unwrap(),
        RdmaListenerConfig::default(),
    ));
    for future in [&mut connect_backend, &mut connect_ingress] {
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    for future in [&mut listen_backend, &mut listen_ingress] {
        assert!(future.as_mut().poll(&mut cx).is_pending());
    }
    engine.shared.commands.service_turn(
        &engine.shared,
        driver.reactor.io.core_mut(),
        &mut driver.reactor.session,
    );
    engine.shared.commands.service_turn(
        &engine.shared,
        driver.reactor.io.core_mut(),
        &mut driver.reactor.session,
    );
    assert_eq!(engine.shared.commands.pending_connects(), 1);
    assert_eq!(engine.shared.commands.pending_listens(), 1);

    drop(driver);
    for future in [&mut connect_backend, &mut connect_ingress] {
        assert!(matches!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Err(Error::DriverShutdown))
        ));
    }
    for future in [&mut listen_backend, &mut listen_ingress] {
        assert!(matches!(
            future.as_mut().poll(&mut cx),
            Poll::Ready(Err(Error::DriverShutdown))
        ));
    }
    assert_eq!(engine.shared.commands.pending_connects(), 0);
    assert_eq!(engine.shared.commands.pending_listens(), 0);
    assert_eq!(engine.diagnostics().live_connections, 0);
    assert!(counter.count() >= 4);
}

#[test]
fn dropped_shutdown_future_unregisters_its_waiter() {
    let (engine, driver) = test_engine_pair(CompletionMode::Readiness);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    let mut shutdown = Box::pin(engine.shutdown());

    assert!(Pin::new(&mut shutdown).poll(&mut cx).is_pending());
    drop(shutdown);
    drop(driver);
    assert_eq!(
        counter.count(),
        0,
        "a cancelled shutdown must not retain its waker"
    );
}

#[test]
fn shutdown_initiates_each_preexisting_connection_close_once() {
    struct ShutdownPoster {
        qp_num: u32,
        error_transitions: AtomicUsize,
    }

    impl TestConnectionProvider for ShutdownPoster {
        fn qp_num(&self) -> u32 {
            self.qp_num
        }

        fn capabilities(&self) -> Option<QpCapabilities> {
            None
        }

        fn post_send(&self, _batch: &mut PreparedSendBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn post_recv(&self, _batch: &mut PreparedRecvBatch) -> Result<BatchPostOutcome> {
            Ok(BatchPostOutcome::AllAccepted)
        }

        fn to_error(&self) -> Result<()> {
            self.error_transitions.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }

        fn destroy_qp(&self) -> Result<bool> {
            Ok(true)
        }

        #[cfg(any(test, feature = "test-hooks"))]
        fn disconnect(&self) -> Result<()> {
            Ok(())
        }
    }

    let (engine, mut driver) = test_engine_pair(CompletionMode::Polling);
    let mut connections = Vec::new();
    let mut posters = Vec::new();
    for qp_num in 1..=3 {
        let poster = Arc::new(ShutdownPoster {
            qp_num,
            error_transitions: AtomicUsize::new(0),
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
        posters.push(poster);
        connections.push(connection);
    }

    engine.shared.request_shutdown();
    driver.reactor.session.manager.begin_all_connection_close(
        &mut driver.reactor.session.connections,
        driver.reactor.io.core_mut(),
    );
    driver.reactor.session.manager.begin_all_connection_close(
        &mut driver.reactor.session.connections,
        driver.reactor.io.core_mut(),
    );

    assert!(
        driver
            .reactor
            .session
            .manager
            .shutdown_connection_close_started
            .load(Ordering::Acquire)
    );
    assert!(connections.iter().all(|connection| {
        driver
            .reactor
            .session
            .connections
            .close_started(connection.session_token())
    }));
    assert!(
        posters
            .iter()
            .all(|poster| poster.error_transitions.load(Ordering::Acquire) == 1)
    );

    drop(connections);
    drop(driver);
}

#[test]
fn pending_listen_waiter_is_woken_when_driver_drops() {
    let (engine, driver) = test_engine_pair(CompletionMode::Polling);
    let counter = CountingWaker::new();
    let waker = counter.waker();
    let mut cx = TaskContext::from_waker(&waker);
    let mut listen = Box::pin(engine.listen(
        "127.0.0.1:0".parse().unwrap(),
        RdmaListenerConfig::default(),
    ));

    assert!(Pin::new(&mut listen).poll(&mut cx).is_pending());
    drop(driver);
    assert!(matches!(
        Pin::new(&mut listen).poll(&mut cx),
        Poll::Ready(Err(Error::DriverShutdown))
    ));
    assert_eq!(counter.count(), 1);
}
